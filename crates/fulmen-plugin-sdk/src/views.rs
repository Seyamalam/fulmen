//! The safe view types a [`crate::FulmenPlugin`] hook receives — thin
//! wrappers over the `#[repr(C)]` call context. No Python objects, no
//! allocations on the read path: every accessor either copies a scalar or
//! calls through the host's vtable.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::abi::{FfiHeaderPair, FfiHeaders, FfiHookCxV1, FfiResponseView, FfiSocketAddr, FfiStr};
use crate::phase::Phase;

/// The read-only request a hook sees.
///
/// All borrows live only for the hook call — the host owns the memory
/// (see the crate's ABI contract).
pub struct RequestView<'a> {
    cx: &'a FfiHookCxV1,
}

impl<'a> RequestView<'a> {
    /// Wrap the raw context. `cx` must be live for `'a`.
    ///
    /// # Safety
    /// Callers (the SDK's generated trampolines) guarantee `cx` points at
    /// the host's call context, valid for the hook's duration.
    pub(crate) unsafe fn new(cx: &'a FfiHookCxV1) -> Self {
        Self { cx }
    }

    /// The phase this hook is running in.
    pub fn phase(&self) -> Phase {
        // The host always writes a valid index; `OnRequest` is a fallback
        // that can only trigger on a host bug.
        Phase::from_index(self.cx.phase).unwrap_or(Phase::OnRequest)
    }

    /// The request method (`"GET"`, …).
    pub fn method(&self) -> &'a str {
        ffi_str(&self.cx.method)
    }

    /// The request path (percent-encoded).
    pub fn path(&self) -> &'a str {
        ffi_str(&self.cx.path)
    }

    /// The query string without `?`, when present.
    pub fn query(&self) -> Option<&'a str> {
        (!self.cx.query.is_null()).then(|| ffi_str(&self.cx.query))
    }

    /// The collected request body.
    pub fn body(&self) -> &'a [u8] {
        // SAFETY: the request body outlives the hook call by contract.
        unsafe { self.cx.body.as_bytes() }
    }

    /// The peer address.
    pub fn remote_addr(&self) -> SocketAddr {
        socket_addr(&self.cx.remote)
    }

    /// The first value of request header `name` (case-insensitive), if
    /// present.
    pub fn header(&self, name: &str) -> Option<&'a [u8]> {
        header_get(&self.cx.request_headers, name)
    }

    /// Iterate every request `(name, value)` header pair.
    pub fn headers(&self) -> HeaderIter<'a> {
        HeaderIter::new(&self.cx.request_headers)
    }
}

impl std::fmt::Debug for RequestView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestView")
            .field("phase", &self.phase())
            .field("method", &self.method())
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

/// The mutable response a post-handler hook sees — and the scratch
/// editor the host hands a hook for a short-circuit answer.
pub struct ResponseView<'a> {
    raw: &'a mut FfiResponseView,
}

impl<'a> ResponseView<'a> {
    /// Wrap the raw view. `raw` must be live for `'a`.
    ///
    /// # Safety
    /// Callers guarantee `raw` points at a valid host response view.
    pub(crate) unsafe fn new(raw: &'a mut FfiResponseView) -> Self {
        Self { raw }
    }

    /// The current status code.
    pub fn status(&self) -> u16 {
        // SAFETY: the view's ctx pointers are host-owned and live for 'a.
        unsafe { (self.raw.get_status)(self.raw.status_ctx) }
    }

    /// Set the status code; `false` when `status` is invalid.
    pub fn set_status(&mut self, status: u16) -> bool {
        // SAFETY: same liveness contract as `status`.
        unsafe { (self.raw.set_status)(self.raw.status_ctx, status) }
    }

    /// The response body bytes.
    pub fn body(&self) -> &[u8] {
        // SAFETY: the returned slice borrows host memory valid for the
        // call; the borrow is constrained to this `&self` borrow so
        // `set_body` cannot invalidate it while held.
        let slice = unsafe { (self.raw.get_body)(self.raw.body_ctx) };
        // SAFETY: same liveness contract.
        unsafe { slice.as_bytes() }
    }

    /// Replace the response body (the host copies `body`).
    pub fn set_body(&mut self, body: &[u8]) -> bool {
        // SAFETY: `body` is copied before the shim returns.
        unsafe { (self.raw.set_body)(self.raw.body_ctx, FfiStr::from_bytes(body)) }
    }

    /// The first value of response header `name`, if present.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        header_get(&self.raw.headers, name)
    }

    /// Insert response header `name` → `value` (replacing existing
    /// values); `false` on an invalid name or value.
    pub fn set_header(&mut self, name: &str, value: &[u8]) -> bool {
        let Some(set) = self.raw.headers.set else {
            return false;
        };
        // SAFETY: name/value are read synchronously by the shim.
        unsafe {
            set(
                self.raw.headers.ctx,
                FfiStr::from_utf8(name),
                FfiStr::from_bytes(value),
            )
        }
    }

    /// Remove every `name` value from the response; `false` when absent.
    pub fn remove_header(&mut self, name: &str) -> bool {
        let Some(remove) = self.raw.headers.remove else {
            return false;
        };
        // SAFETY: same liveness contract.
        unsafe { remove(self.raw.headers.ctx, FfiStr::from_utf8(name)) }
    }

    /// Iterate every response `(name, value)` header pair.
    pub fn headers(&self) -> HeaderIter<'_> {
        HeaderIter::new(&self.raw.headers)
    }
}

impl std::fmt::Debug for ResponseView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseView")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// An iterator over a header view's `(name, value)` pairs.
pub struct HeaderIter<'a> {
    headers: &'a FfiHeaders,
    count: usize,
    next: usize,
}

impl<'a> HeaderIter<'a> {
    fn new(headers: &'a FfiHeaders) -> Self {
        // SAFETY: the headers context is host-owned and live for 'a.
        let count = unsafe { (headers.count)(headers.ctx) };
        Self {
            headers,
            count,
            next: 0,
        }
    }
}

impl<'a> Iterator for HeaderIter<'a> {
    /// `(name, value)` — `name` is `""` for a non-UTF-8 name (never in
    /// practice: header names are ASCII).
    type Item = (&'a str, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.count {
            return None;
        }
        let mut pair = FfiHeaderPair {
            name: FfiStr::NULL,
            value: FfiStr::NULL,
        };
        // SAFETY: `pair` is a live out-pointer for the shim's duration.
        let ok = unsafe { (self.headers.at)(self.headers.ctx, self.next, &raw mut pair) };
        self.next += 1;
        if !ok {
            self.next = self.count;
            return None;
        }
        // SAFETY: the returned slice borrows host memory valid for 'a.
        let name = unsafe { pair.name.as_bytes() };
        // SAFETY: same liveness contract.
        let value = unsafe { pair.value.as_bytes() };
        Some((core::str::from_utf8(name).unwrap_or(""), value))
    }
}

impl std::fmt::Debug for HeaderIter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeaderIter")
            .field("remaining", &(self.count - self.next))
            .finish()
    }
}

/// `header_get` shared by both views: first `name` value, if present.
fn header_get<'a>(headers: &'a FfiHeaders, name: &str) -> Option<&'a [u8]> {
    let mut value = FfiStr::NULL;
    // SAFETY: `value` is a live out-pointer for the shim's duration; the
    // result borrows host memory valid for 'a.
    let ok = unsafe { (headers.get)(headers.ctx, FfiStr::from_utf8(name), &raw mut value) };
    // SAFETY: same liveness contract.
    ok.then(|| unsafe { value.as_bytes() })
}

/// `FfiStr` → `&str` with the usual UTF-8 fallback.
fn ffi_str(s: &FfiStr) -> &str {
    // SAFETY: call-context slices are live for the borrow's scope.
    let bytes = unsafe { s.as_bytes() };
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Rebuild a `SocketAddr` from the inline FFI form.
fn socket_addr(addr: &FfiSocketAddr) -> SocketAddr {
    let ip = if addr.ip_len == 4 {
        IpAddr::V4(Ipv4Addr::new(
            addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3],
        ))
    } else {
        IpAddr::V6(Ipv6Addr::from(addr.ip))
    };
    SocketAddr::new(ip, addr.port)
}
