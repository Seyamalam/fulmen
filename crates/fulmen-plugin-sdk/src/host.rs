//! The host side of the ABI boundary — the safe API `fulmen-py` calls.
//!
//! `fulmen-py` extracts the descriptor pointer from the `fulmen.plugin.v1`
//! `PyCapsule` with `PyO3`'s safe `pointer_checked`, then hands the pointer
//! here: [`describe`] and [`LoadedPlugin::load`] read and validate the
//! descriptor, [`LoadedPlugin::create`] builds the instance, and
//! [`LoadedPlugin::run_hook`] executes a phase hook — every cross-ABI
//! call wrapped in `catch_unwind`, so a plugin panic is a hook failure
//! (a 500 upstream), never an unwind through an `extern "C"` frame.
//!
//! The view shims reach directly into the host's `http::HeaderMap`,
//! `http::StatusCode` and `bytes::Bytes`, so a hook call allocates
//! nothing on the read path.

use std::ffi::c_void;
use std::net::{IpAddr, SocketAddr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, Ordering};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::abi::{
    FULMEN_PLUGIN_ABI, FfiBuf, FfiHeaderPair, FfiHeaders, FfiHookCxV1, FfiHookFn, FfiResponseView,
    FfiSocketAddr, FfiStr, FulmenPluginDescriptorV1, PHASE_COUNT,
};
use crate::phase::Phase;

/// Error-buffer capacity handed to `create`/hook calls (§13.4).
const ERR_CAP: usize = 512;

/// A failure loading or initializing a native plugin.
#[derive(Debug)]
pub enum LoadError {
    /// The descriptor's `abi_version` did not match
    /// [`FULMEN_PLUGIN_ABI`].
    AbiMismatch {
        /// The version the plugin's descriptor reported.
        found: u32,
    },
    /// The descriptor's `struct_size` does not cover the v1 layout.
    StructSize {
        /// The size the plugin's descriptor reported.
        found: u32,
    },
    /// `name` or `version` was not UTF-8.
    BadUtf8,
    /// `create` reported a setup failure (plugin's own message).
    Create(String),
    /// `create` panicked across the boundary.
    CreatePanic,
    /// `create` was called twice on one `LoadedPlugin`.
    AlreadyCreated,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AbiMismatch { found } => write!(
                f,
                "native plugin ABI mismatch: expected {FULMEN_PLUGIN_ABI}, found {found} — \
                 rebuild the plugin against the matching fulmen-plugin-sdk"
            ),
            Self::StructSize { found } => write!(
                f,
                "native plugin descriptor is malformed: struct_size {found} < {}",
                size_of::<FulmenPluginDescriptorV1>()
            ),
            Self::BadUtf8 => f.write_str("native plugin descriptor carries non-UTF-8 name/version"),
            Self::Create(message) => write!(f, "native plugin setup failed: {message}"),
            Self::CreatePanic => f.write_str("native plugin panicked during setup"),
            Self::AlreadyCreated => f.write_str("native plugin instance already created"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Read-only metadata describing a loaded plugin — what
/// `describe_native_plugin` reports to Python.
#[derive(Debug)]
pub struct PluginInfo {
    /// Plugin name (`FulmenPlugin::NAME`).
    pub name: String,
    /// Plugin version (`FulmenPlugin::VERSION`).
    pub version: String,
    /// The `abi_version` the descriptor reported (`== FULMEN_PLUGIN_ABI`).
    pub abi_version: u32,
    /// The phases the plugin hooks.
    pub phases: Vec<Phase>,
}

/// A descriptor the host read and validated — the plugin's static
/// descriptor stays in its own memory; only copies are kept.
struct DescriptorCopy {
    name: Box<str>,
    version: Box<str>,
    hooks: [Option<FfiHookFn>; PHASE_COUNT],
    create: crate::abi::FfiCreateFn,
    destroy: crate::abi::FfiDestroyFn,
}

/// Read and validate a descriptor pointer from a `fulmen.plugin.v1`
/// capsule.
///
/// # Safety note
/// `desc` must point at a live `FulmenPluginV1` — guaranteed by the
/// capsule contract the caller verified (name-checked `PyCapsule` whose
/// owner keeps the plugin's shared library loaded).
fn read_descriptor(desc: NonNull<FulmenPluginDescriptorV1>) -> Result<DescriptorCopy, LoadError> {
    // SAFETY: per the function contract, `desc` points at a live
    // descriptor — reading `abi_version`/`struct_size` (the first 8
    // bytes) is safe under that contract; the version check happens
    // before any further field is read.
    let d = unsafe { desc.as_ref() };
    if d.abi_version != FULMEN_PLUGIN_ABI {
        return Err(LoadError::AbiMismatch {
            found: d.abi_version,
        });
    }
    if (d.struct_size as usize) < size_of::<FulmenPluginDescriptorV1>() {
        return Err(LoadError::StructSize {
            found: d.struct_size,
        });
    }
    // SAFETY: `abi_version == 1` means `desc` points at the full v1
    // layout, and `struct_size` covers it; `name`/`version` are the
    // plugin's own 'static borrows, live while its library is loaded
    // (the capsule's owner guarantees that).
    let name = unsafe { d.name.as_bytes() };
    // SAFETY: same contract as `name` above.
    let version = unsafe { d.version.as_bytes() };
    Ok(DescriptorCopy {
        name: ffi_str_owned(name)?,
        version: ffi_str_owned(version)?,
        hooks: d.hooks,
        create: d.create,
        destroy: d.destroy,
    })
}

/// `FfiStr` → owned `String`, rejecting non-UTF-8.
fn ffi_str_owned(bytes: &[u8]) -> Result<Box<str>, LoadError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map(String::into_boxed_str)
        .map_err(|_| LoadError::BadUtf8)
}

/// Read the descriptor's public metadata without building an instance.
///
/// `desc` is the pointer a `fulmen.plugin.v1` `PyCapsule` carries.
///
/// # Errors
/// [`LoadError`] when the descriptor fails validation.
pub fn describe(desc: NonNull<FulmenPluginDescriptorV1>) -> Result<PluginInfo, LoadError> {
    let copy = read_descriptor(desc)?;
    Ok(PluginInfo {
        name: copy.name.to_string(),
        version: copy.version.to_string(),
        abi_version: FULMEN_PLUGIN_ABI,
        phases: Phase::ALL
            .into_iter()
            .filter(|phase| copy.hooks[phase.index()].is_some())
            .collect(),
    })
}

/// A loaded native plugin: the validated descriptor copy plus the
/// instance `create` produced.
///
/// `Send + Sync`: the plugin's hooks may run on any dispatch thread.
pub struct LoadedPlugin {
    name: Box<str>,
    version: Box<str>,
    hooks: [Option<FfiHookFn>; PHASE_COUNT],
    create: crate::abi::FfiCreateFn,
    destroy: crate::abi::FfiDestroyFn,
    /// The plugin instance — `create`'s return, atomically so setup can
    /// never race and `drop` never double-frees.
    state: AtomicPtr<c_void>,
}

// SAFETY: `FulmenPlugin: Send + Sync` is part of the trait contract, so
// the instance behind `state` tolerates cross-thread calls; `state`
// itself is only handed to the plugin's own extern fns, and
// create/destroy are serialized by the atomic compare-exchange.
unsafe impl Send for LoadedPlugin {}
// SAFETY: same reasoning — shared `&self` access from many threads is
// the dispatch model.
unsafe impl Sync for LoadedPlugin {}

impl LoadedPlugin {
    /// Read and validate a descriptor — the plugin is *not* yet
    /// instantiated; call [`create`](Self::create) with the plugin's
    /// JSON config next.
    ///
    /// `desc` is the pointer a `fulmen.plugin.v1` `PyCapsule` carries.
    ///
    /// # Errors
    /// [`LoadError`] when the descriptor fails validation.
    pub fn load(desc: NonNull<FulmenPluginDescriptorV1>) -> Result<Self, LoadError> {
        let copy = read_descriptor(desc)?;
        Ok(Self {
            name: copy.name,
            version: copy.version,
            hooks: copy.hooks,
            create: copy.create,
            destroy: copy.destroy,
            state: AtomicPtr::new(std::ptr::null_mut()),
        })
    }

    /// Build the plugin instance — `config` is the plugin's
    /// configuration as JSON (empty for none). Once only.
    ///
    /// # Errors
    /// [`LoadError::Create`] with the plugin's own message,
    /// [`LoadError::CreatePanic`] if it panicked,
    /// [`LoadError::AlreadyCreated`] on a second call.
    pub fn create(&self, config: &[u8]) -> Result<(), LoadError> {
        if !self.state.load(Ordering::SeqCst).is_null() {
            return Err(LoadError::AlreadyCreated);
        }
        let mut errbytes = [0u8; ERR_CAP];
        let mut err = FfiBuf {
            ptr: errbytes.as_mut_ptr(),
            cap: ERR_CAP,
            len: 0,
        };
        let config = FfiStr::from_bytes(config);
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `config` borrows our slice for the call; `err` is a
            // live out-buffer.
            unsafe { (self.create)(config, &raw mut err) }
        }));
        match result {
            Err(_) => Err(LoadError::CreatePanic),
            Ok(state) if state.is_null() => Err(LoadError::Create(err_message(
                &errbytes[..err.len.min(ERR_CAP)],
            ))),
            Ok(state) => {
                if self
                    .state
                    .compare_exchange(
                        std::ptr::null_mut(),
                        state,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    Ok(())
                } else {
                    // A racing `create` won — destroy the orphan instance.
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        // SAFETY: `state` is the pointer the plugin's own
                        // `create` returned, never yet seen by `destroy`.
                        unsafe { (self.destroy)(state) }
                    }));
                    Err(LoadError::AlreadyCreated)
                }
            }
        }
    }

    /// The plugin's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The plugin's version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// `true` when the plugin hooks `phase`.
    pub fn has_hook(&self, phase: usize) -> bool {
        self.hooks.get(phase).is_some_and(Option::is_some)
    }

    /// Run the `phase` hook against `args`.
    ///
    /// Returns [`HookOutcome::Continue`] when the plugin does not hook
    /// `phase` or reports `0`, [`HookOutcome::Respond`] with the
    /// host-materialized response for `1`, and
    /// [`HookOutcome::Panicked`] for any failure (plugin panic, bad
    /// return code, hook before `create`). Never panics across the
    /// boundary.
    pub fn run_hook(&self, phase: usize, args: &mut HookArgs<'_>) -> HookOutcome {
        let Some(hook) = self.hooks.get(phase).copied().flatten() else {
            return HookOutcome::Continue;
        };
        let state = self.state.load(Ordering::SeqCst);
        if state.is_null() {
            return HookOutcome::Panicked(format!(
                "plugin `{}` hook ran before setup completed",
                self.name
            ));
        }
        let request_headers = request_headers_view(args.headers);
        let mut cx = FfiHookCxV1 {
            phase: u32::try_from(phase).unwrap_or(u32::MAX),
            method: FfiStr::from_utf8(args.method),
            path: FfiStr::from_utf8(args.path),
            query: args.query.map_or(FfiStr::NULL, FfiStr::from_utf8),
            body: FfiStr::from_bytes(args.body),
            remote: ffi_addr(args.remote_addr),
            request_headers,
            response: std::ptr::null_mut(),
        };
        let mut res_view = args.response.as_mut().map(response_view);
        if let Some(view) = res_view.as_mut() {
            cx.response = std::ptr::from_mut(view);
        }
        let mut parts = ResponseParts::default();
        let mut scratch = scratch_view(&mut parts);
        let mut errbytes = [0u8; ERR_CAP];
        let mut err = FfiBuf {
            ptr: errbytes.as_mut_ptr(),
            cap: ERR_CAP,
            len: 0,
        };
        // SAFETY: `state` is the pointer `create` returned; `cx`, `scratch`
        // and `err` are live for the call; the contract's borrowed slices
        // borrow `args`, which outlives the call.
        let code = catch_unwind(AssertUnwindSafe(|| unsafe {
            hook(state, &raw const cx, &raw mut scratch, &raw mut err)
        }));
        match code {
            Ok(0) => HookOutcome::Continue,
            Ok(1) => HookOutcome::Respond(parts),
            Ok(code) => {
                HookOutcome::Panicked(err_message_or(&errbytes[..err.len.min(ERR_CAP)], || {
                    format!("plugin `{}` hook returned invalid code {code}", self.name)
                }))
            }
            Err(_) => HookOutcome::Panicked(format!("plugin `{}` panicked in a hook", self.name)),
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        let state = self.state.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !state.is_null() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: `state` is `create`'s pointer; the atomic swap
                // makes this the one and only `destroy` call for it.
                unsafe { (self.destroy)(state) }
            }));
        }
    }
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("name", &self.name)
            .field("version", &self.version)
            .field(
                "phases",
                &Phase::ALL
                    .into_iter()
                    .filter(|phase| self.has_hook(phase.index()))
                    .collect::<Vec<_>>(),
            )
            .field("created", &!self.state.load(Ordering::Relaxed).is_null())
            .finish_non_exhaustive()
    }
}

/// A decoded error-buffer message.
fn err_message(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The buffer's message, or `fallback` when the plugin wrote nothing.
fn err_message_or(bytes: &[u8], fallback: impl FnOnce() -> String) -> String {
    let message = err_message(bytes);
    if message.is_empty() {
        fallback()
    } else {
        message
    }
}

/// The response a hook's `Respond` code built — host types, materialized
/// through the scratch view.
#[derive(Debug, Default)]
pub struct ResponseParts {
    /// Response status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

/// The host's per-call hook inputs — the `HookCx` fields fulmen-py
/// already holds, borrowed for the call.
#[derive(Debug)]
pub struct HookArgs<'a> {
    /// Request method.
    pub method: &'a str,
    /// Request path.
    pub path: &'a str,
    /// Query string without `?`.
    pub query: Option<&'a str>,
    /// Request headers (read-only for the plugin).
    pub headers: &'a HeaderMap,
    /// Collected request body.
    pub body: &'a [u8],
    /// Peer address.
    pub remote_addr: SocketAddr,
    /// The in-flight response — `Some` for the post-handler phases.
    pub response: Option<ResponseMut<'a>>,
}

/// The mutable pieces of the in-flight response a post-handler hook may
/// touch, borrowed from the server's `Response`.
#[derive(Debug)]
pub struct ResponseMut<'a> {
    /// Response status.
    pub status: &'a mut StatusCode,
    /// Response headers.
    pub headers: &'a mut HeaderMap,
    /// Response body.
    pub body: &'a mut Bytes,
}

/// What a `run_hook` call produced.
#[derive(Debug)]
pub enum HookOutcome {
    /// Continue the pipeline.
    Continue,
    /// Answer with `response`.
    Respond(ResponseParts),
    /// The hook failed (panic across the boundary or contract
    /// violation) — the host maps this to a 500 problem response.
    Panicked(String),
}

/// `SocketAddr` → its inline FFI form.
fn ffi_addr(addr: SocketAddr) -> FfiSocketAddr {
    let mut ip = [0u8; 16];
    let ip_len = match addr.ip() {
        IpAddr::V4(v4) => {
            ip[..4].copy_from_slice(&v4.octets());
            4
        }
        IpAddr::V6(v6) => {
            ip.copy_from_slice(&v6.octets());
            16
        }
    };
    FfiSocketAddr {
        ip_len,
        ip,
        port: addr.port(),
    }
}

/// The read-only header view over the request `HeaderMap`.
fn request_headers_view(map: &HeaderMap) -> FfiHeaders {
    FfiHeaders {
        // The `*const`→`*mut` round-trip is sound: `set`/`remove` are
        // `None`, so nothing writes through `ctx`.
        ctx: std::ptr::from_ref(map).cast_mut().cast::<c_void>(),
        count: hdr_count,
        at: hdr_at,
        get: hdr_get,
        set: None,
        remove: None,
    }
}

/// The writable header view over a response `HeaderMap`.
fn response_headers_view(map: &mut HeaderMap) -> FfiHeaders {
    FfiHeaders {
        ctx: std::ptr::from_mut(map).cast::<c_void>(),
        count: hdr_count,
        at: hdr_at,
        get: hdr_get,
        set: Some(hdr_set),
        remove: Some(hdr_remove),
    }
}

/// The `FfiResponseView` over the in-flight response.
fn response_view(res: &mut ResponseMut<'_>) -> FfiResponseView {
    FfiResponseView {
        status_ctx: std::ptr::from_mut(res.status).cast::<c_void>(),
        get_status: status_get,
        set_status: status_set,
        body_ctx: std::ptr::from_mut(res.body).cast::<c_void>(),
        get_body: body_get,
        set_body: body_set,
        headers: response_headers_view(res.headers),
    }
}

/// The scratch `FfiResponseView` a `Respond` decision writes into.
fn scratch_view(parts: &mut ResponseParts) -> FfiResponseView {
    FfiResponseView {
        status_ctx: std::ptr::from_mut(&mut parts.status).cast::<c_void>(),
        get_status: status_get,
        set_status: status_set,
        body_ctx: std::ptr::from_mut(&mut parts.body).cast::<c_void>(),
        get_body: body_get,
        set_body: body_set,
        headers: response_headers_view(&mut parts.headers),
    }
}

// --- The vtable shims -------------------------------------------------
// `ctx` is always the pointer the host stored when it built the view —
// a HeaderMap/StatusCode/Bytes reference whose borrow outlives the hook
// call (guaranteed by `HookArgs`'s lifetimes).

/// `FfiHeaders::count` over a `HeaderMap`.
unsafe extern "C" fn hdr_count(ctx: *mut c_void) -> usize {
    // SAFETY: `ctx` is the `HeaderMap` the host borrowed for the call.
    unsafe { &*ctx.cast_const().cast::<HeaderMap>() }.len()
}

/// `FfiHeaders::at` over a `HeaderMap` — O(n) lookup by index; header
/// maps are small (~tens of fields), and the iterator is not re-entrant.
unsafe extern "C" fn hdr_at(ctx: *mut c_void, index: usize, out: *mut FfiHeaderPair) -> bool {
    // SAFETY: `ctx` is the `HeaderMap` borrowed for the call.
    let map = unsafe { &*ctx.cast_const().cast::<HeaderMap>() };
    match map.iter().nth(index) {
        Some((name, value)) => {
            // SAFETY: `out` is a live caller-provided out-pointer.
            unsafe {
                *out = FfiHeaderPair {
                    name: FfiStr::from_utf8(name.as_str()),
                    value: FfiStr::from_bytes(value.as_bytes()),
                };
            }
            true
        }
        None => false,
    }
}

/// `FfiHeaders::get` over a `HeaderMap` — first value for `name`.
unsafe extern "C" fn hdr_get(ctx: *mut c_void, name: FfiStr, out: *mut FfiStr) -> bool {
    // SAFETY: `ctx` is the `HeaderMap` borrowed for the call.
    let map = unsafe { &*ctx.cast_const().cast::<HeaderMap>() };
    // SAFETY: `name` is a borrowed slice valid for the call.
    let name = unsafe { name.as_bytes() };
    let Ok(name) = HeaderName::from_bytes(name) else {
        return false;
    };
    match map.get(&name) {
        Some(value) => {
            // SAFETY: `out` is a live caller-provided out-pointer.
            unsafe {
                *out = FfiStr::from_bytes(value.as_bytes());
            }
            true
        }
        None => false,
    }
}

/// `FfiHeaders::set` over a `HeaderMap` — insert `name` → `value`.
unsafe extern "C" fn hdr_set(ctx: *mut c_void, name: FfiStr, value: FfiStr) -> bool {
    // SAFETY: `ctx` is the `HeaderMap` mutably borrowed for the call.
    let map = unsafe { &mut *ctx.cast::<HeaderMap>() };
    // SAFETY: `name` is a borrowed slice valid for the call.
    let name = unsafe { name.as_bytes() };
    // SAFETY: `value` is a borrowed slice valid for the call.
    let value = unsafe { value.as_bytes() };
    let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name), HeaderValue::from_bytes(value))
    else {
        return false;
    };
    map.insert(name, value);
    true
}

/// `FfiHeaders::remove` over a `HeaderMap` — drop every `name` value.
unsafe extern "C" fn hdr_remove(ctx: *mut c_void, name: FfiStr) -> bool {
    // SAFETY: `ctx` is the `HeaderMap` mutably borrowed for the call.
    let map = unsafe { &mut *ctx.cast::<HeaderMap>() };
    // SAFETY: `name` is a borrowed slice valid for the call.
    let name = unsafe { name.as_bytes() };
    let Ok(name) = HeaderName::from_bytes(name) else {
        return false;
    };
    map.remove(&name).is_some()
}

/// `FfiResponseView::get_status` over a `StatusCode`.
unsafe extern "C" fn status_get(ctx: *mut c_void) -> u16 {
    // SAFETY: `ctx` is the `StatusCode` borrowed for the call.
    unsafe { *ctx.cast_const().cast::<StatusCode>() }.as_u16()
}

/// `FfiResponseView::set_status` over a `StatusCode`.
unsafe extern "C" fn status_set(ctx: *mut c_void, status: u16) -> bool {
    // SAFETY: `ctx` is the `StatusCode` mutably borrowed for the call.
    unsafe {
        match StatusCode::from_u16(status) {
            Ok(code) => {
                *ctx.cast::<StatusCode>() = code;
                true
            }
            Err(_) => false,
        }
    }
}

/// `FfiResponseView::get_body` over a `Bytes` — returns a borrowed slice
/// of the buffer.
unsafe extern "C" fn body_get(ctx: *mut c_void) -> FfiStr {
    // SAFETY: `ctx` is the `Bytes` borrowed for the call.
    FfiStr::from_bytes(unsafe { &*ctx.cast_const().cast::<Bytes>() })
}

/// `FfiResponseView::set_body` over a `Bytes` — copies the slice.
unsafe extern "C" fn body_set(ctx: *mut c_void, body: FfiStr) -> bool {
    // SAFETY: `body` is a borrowed slice valid for the call; it is copied
    // before returning.
    let body = unsafe { body.as_bytes() };
    // SAFETY: `ctx` is the `Bytes` mutably borrowed for the call.
    unsafe {
        *ctx.cast::<Bytes>() = Bytes::copy_from_slice(body);
    }
    true
}
