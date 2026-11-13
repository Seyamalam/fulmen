//! The C ABI v1 between Fulmen and a native plugin (DESIGN.md §13.4).
//!
//! Everything in this module is `#[repr(C)]` plain data: pointers, lengths
//! and function pointers. The layout is the contract — bump
//! [`FULMEN_PLUGIN_ABI`] on any change.
//!
//! # Ownership contract
//!
//! **Nothing crosses the boundary by ownership.** Every `FfiStr` /
//! `FfiHeaderPair` is a *borrowed* `(ptr, len)` slice valid only for the
//! duration of the call it arrives in: a host pointer borrows memory the
//! host owns (the request's `HeaderMap`/`Bytes`); a plugin pointer borrows
//! memory the plugin owns (a `'static` string, a stack/heap buffer the
//! host copies synchronously before returning). Neither side may retain a
//! borrowed slice past the call, and neither side may free memory the
//! other owns. The only owned hand-off is the plugin instance itself —
//! `create` returns a `*mut c_void` the host later gives back to `destroy`
//! exactly once.
//!
//! Return codes on hook functions: `0` continues the pipeline, `1` answers
//! with the response written into `out`, anything negative reports a hook
//! failure (a panic the SDK trampoline caught) with the message copied
//! into `err`.

use std::ffi::c_void;

/// The ABI version tag a descriptor must carry (§13.4, §14.1).
///
/// Unstable until Fulmen 1.0: any layout change to this module bumps it.
pub const FULMEN_PLUGIN_ABI: u32 = 1;

/// The `PyCapsule` name a descriptor travels under.
pub const FULMEN_PLUGIN_CAPSULE: &std::ffi::CStr = c"fulmen.plugin.v1";

/// The symbol a plugin shared library exports (`export_plugin!` emits it).
pub const FULMEN_PLUGIN_SYMBOL: &str = "fulmen_plugin_v1";

/// The number of hookable phases — `hooks` array length.
pub const PHASE_COUNT: usize = 8;

/// A borrowed `(ptr, len)` byte slice (UTF-8 where it carries text).
///
/// Never NUL-terminated — always use `len`. A null `ptr` with `len == 0`
/// is the empty/absent value; `ptr` must be non-null whenever `len > 0`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiStr {
    /// Start of the slice; borrowed — see the module contract.
    pub ptr: *const u8,
    /// Slice length in bytes.
    pub len: usize,
}

impl FfiStr {
    /// The empty slice (`ptr = null`, `len = 0`).
    pub const NULL: Self = Self {
        ptr: std::ptr::null(),
        len: 0,
    };

    /// Borrow `s` for the duration of an FFI call.
    ///
    /// Safe: constructing a borrowed slice is just pointer+len; the call
    /// contract (see module docs) is what keeps the pointee alive.
    pub fn from_bytes(s: &[u8]) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// Borrow `s` — shorthand for `from_bytes(s.as_bytes())`.
    pub fn from_utf8(s: &str) -> Self {
        Self::from_bytes(s.as_bytes())
    }

    /// `true` for the null/empty slice.
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    /// The borrowed bytes, with a caller-asserted lifetime.
    ///
    /// # Safety
    /// The caller chooses `'a` and must guarantee the slice's owner
    /// outlives it — i.e. the borrow does not escape the FFI call the
    /// slice was handed to (see the module contract).
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        // SAFETY: caller guarantees the pointee is live for 'a.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::fmt::Debug for FfiStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiStr")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .finish()
    }
}

/// A borrowed `(name, value)` header pair.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FfiHeaderPair {
    /// Header name (ASCII).
    pub name: FfiStr,
    /// Header value bytes (not necessarily UTF-8).
    pub value: FfiStr,
}

/// A header-map view handed across the boundary: an opaque host context
/// plus the function table that reads (and optionally mutates) it.
///
/// `set`/`remove` are `None` on the read-only request-headers view and
/// `Some` on the response-headers view.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FfiHeaders {
    /// Opaque host context — on the Fulmen host, `*mut http::HeaderMap`.
    pub ctx: *mut c_void,
    /// Number of header fields.
    pub count: unsafe extern "C" fn(ctx: *mut c_void) -> usize,
    /// Write the `index`-th `(name, value)` into `out`; `false` when out
    /// of range.
    pub at: unsafe extern "C" fn(ctx: *mut c_void, index: usize, out: *mut FfiHeaderPair) -> bool,
    /// Write the first value of `name` (case-insensitive) into `out`;
    /// `false` when absent.
    pub get: unsafe extern "C" fn(ctx: *mut c_void, name: FfiStr, out: *mut FfiStr) -> bool,
    /// Insert `name` → `value` (replacing existing values); `false` on an
    /// invalid name or value. `None` when the view is read-only.
    pub set: Option<unsafe extern "C" fn(ctx: *mut c_void, name: FfiStr, value: FfiStr) -> bool>,
    /// Remove every `name` value; `false` when absent. `None` when the
    /// view is read-only.
    pub remove: Option<unsafe extern "C" fn(ctx: *mut c_void, name: FfiStr) -> bool>,
}

impl std::fmt::Debug for FfiHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiHeaders")
            .field("ctx", &self.ctx)
            .field("writable", &self.set.is_some())
            .finish_non_exhaustive()
    }
}

/// The peer address, inline (no borrowed data).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FfiSocketAddr {
    /// `4` (IPv4) or `16` (IPv6) — the used prefix length of `ip`.
    pub ip_len: u8,
    /// IP octets in network order.
    pub ip: [u8; 16],
    /// Port in host byte order.
    pub port: u16,
}

/// The request context a hook receives.
///
/// `response` is non-null only in the post-handler phases
/// (`after_handle`, `on_response`, `after_response`): mutating it mutates
/// what the client receives for the first two; `after_response` observes
/// a clone, since the response is already produced.
#[repr(C)]
pub struct FfiHookCxV1 {
    /// The phase index (see [`crate::Phase::index`]); informational —
    /// the hook's slot already says which phase ran.
    pub phase: u32,
    /// Request method (`"GET"`, …).
    pub method: FfiStr,
    /// Request path (percent-encoded).
    pub path: FfiStr,
    /// Query string without `?`; `ptr` is null when the request has none.
    pub query: FfiStr,
    /// Collected request body.
    pub body: FfiStr,
    /// Peer address.
    pub remote: FfiSocketAddr,
    /// Request headers — read-only (`set`/`remove` are `None`).
    pub request_headers: FfiHeaders,
    /// The in-flight response — null for the decision phases.
    pub response: *mut FfiResponseView,
}

impl std::fmt::Debug for FfiHookCxV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiHookCxV1")
            .field("phase", &self.phase)
            .field("method", &self.method)
            .field("path", &self.path)
            .field("has_response", &!self.response.is_null())
            .finish_non_exhaustive()
    }
}

/// A mutable response view: an opaque context per mutable piece plus the
/// operations on it. The same shape serves the in-flight response
/// (`cx.response`) and the decision scratch (`out`): a short-circuit
/// response is built by writing through `out`'s operations.
#[repr(C)]
pub struct FfiResponseView {
    /// Opaque host context for the status ops (`*mut http::StatusCode`).
    pub status_ctx: *mut c_void,
    /// Read the status code.
    pub get_status: unsafe extern "C" fn(ctx: *mut c_void) -> u16,
    /// Set the status code; `false` when `status` is invalid.
    pub set_status: unsafe extern "C" fn(ctx: *mut c_void, status: u16) -> bool,
    /// Opaque host context for the body ops (`*mut bytes::Bytes`).
    pub body_ctx: *mut c_void,
    /// Borrow the body bytes (valid until the hook returns).
    pub get_body: unsafe extern "C" fn(ctx: *mut c_void) -> FfiStr,
    /// Replace the body — the host copies the slice before returning.
    pub set_body: unsafe extern "C" fn(ctx: *mut c_void, body: FfiStr) -> bool,
    /// Response headers — mutable (`set`/`remove` are `Some`).
    pub headers: FfiHeaders,
}

impl std::fmt::Debug for FfiResponseView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiResponseView")
            .field("status_ctx", &self.status_ctx)
            .field("body_ctx", &self.body_ctx)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// A host-provided write buffer for error reporting: the callee copies up
/// to `cap` bytes at `ptr` and stores the count in `len`.
#[repr(C)]
#[derive(Debug)]
pub struct FfiBuf {
    /// Buffer start.
    pub ptr: *mut u8,
    /// Buffer capacity.
    pub cap: usize,
    /// Bytes written.
    pub len: usize,
}

/// The per-phase hook signature.
///
/// `state` is the plugin instance `create` returned; `cx` the request
/// context; `out` the scratch response editor filled when returning `1`;
/// `err` a host buffer for a failure message when returning `< 0`.
pub type FfiHookFn = unsafe extern "C" fn(
    state: *mut c_void,
    cx: *const FfiHookCxV1,
    out: *mut FfiResponseView,
    err: *mut FfiBuf,
) -> i32;

/// The plugin-instance factory: `config` is the plugin's configuration as
/// JSON (borrowed for the call). Returns the plugin state pointer, or
/// null with an error message in `err`.
pub type FfiCreateFn = unsafe extern "C" fn(config: FfiStr, err: *mut FfiBuf) -> *mut c_void;

/// Tears down a plugin instance; called once, with the `create` value.
pub type FfiDestroyFn = unsafe extern "C" fn(state: *mut c_void);

/// The versioned descriptor a plugin exports: the `fulmen_plugin_v1`
/// entry point returns a pointer to a static instance of this.
#[repr(C)]
pub struct FulmenPluginDescriptorV1 {
    /// Must equal [`FULMEN_PLUGIN_ABI`]; the host refuses anything else.
    pub abi_version: u32,
    /// `size_of::<Self>()` on the plugin side — guards layout drift: the
    /// host reads only this many bytes' worth of the v1 prefix.
    pub struct_size: u32,
    /// Plugin name (borrowed `'static`, UTF-8).
    pub name: FfiStr,
    /// Plugin version (borrowed `'static`, UTF-8).
    pub version: FfiStr,
    /// Instance factory — see [`FfiCreateFn`].
    pub create: FfiCreateFn,
    /// Instance teardown — see [`FfiDestroyFn`].
    pub destroy: FfiDestroyFn,
    /// Per-phase hooks indexed by [`crate::Phase::index`]; `None` means
    /// the phase is not hooked.
    pub hooks: [Option<FfiHookFn>; PHASE_COUNT],
}

// SAFETY: a descriptor is immutable data — every field is either a value
// copy or a borrowed 'static slice/function pointer, and it is never
// mutated after `export_plugin!`'s `describe` builds it.
unsafe impl Send for FulmenPluginDescriptorV1 {}
// SAFETY: same reasoning — shared reads are the only access.
unsafe impl Sync for FulmenPluginDescriptorV1 {}

impl std::fmt::Debug for FulmenPluginDescriptorV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FulmenPluginDescriptorV1")
            .field("abi_version", &self.abi_version)
            .field("struct_size", &self.struct_size)
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
