//! The plugin side of the ABI boundary: `export_plugin!`'s expansion
//! support. The `__entry`/`__hook_fn` items are `#[doc(hidden)]` machinery
//! called only from macro-generated code — not a public API.
//!
//! Every trampoline catches panics with `catch_unwind` before touching
//! Rust-side plugin code: an unwind through an `extern "C"` frame is an
//! abort, and a plugin panic must surface as a hook error / 500, never UB.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::abi::{
    FULMEN_PLUGIN_ABI, FfiBuf, FfiHookCxV1, FfiHookFn, FfiResponseView, FfiStr,
    FulmenPluginDescriptorV1, PHASE_COUNT,
};
use crate::phase::Phase;
use crate::views::{RequestView, ResponseView};
use crate::{Decision, FulmenPlugin, PluginError, PluginResponse};

/// Build `P`'s v1 descriptor: `name`/`version` borrow the type's
/// `&'static` consts; `hooks` holds trampolines for the named phases.
#[doc(hidden)]
pub fn describe<P: FulmenPlugin>(phases: &[Phase]) -> FulmenPluginDescriptorV1 {
    let mut hooks = [None; PHASE_COUNT];
    for &phase in phases {
        hooks[phase.index()] = Some(__hook_fn::<P>(phase));
    }
    FulmenPluginDescriptorV1 {
        abi_version: FULMEN_PLUGIN_ABI,
        struct_size: u32::try_from(size_of::<FulmenPluginDescriptorV1>()).unwrap_or(u32::MAX),
        name: FfiStr::from_utf8(P::NAME),
        version: FfiStr::from_utf8(P::VERSION),
        create: create::<P>,
        destroy: destroy::<P>,
        hooks,
    }
}

/// The trampoline for `phase` — the `extern "C"` fn a descriptor slot
/// points at.
#[doc(hidden)]
pub fn __hook_fn<P: FulmenPlugin>(phase: Phase) -> FfiHookFn {
    match phase {
        Phase::OnRequest => on_request::<P>,
        Phase::BeforeRoute => before_route::<P>,
        Phase::Guard => guard::<P>,
        Phase::Resolve => resolve::<P>,
        Phase::BeforeHandle => before_handle::<P>,
        Phase::AfterHandle => after_handle::<P>,
        Phase::OnResponse => on_response::<P>,
        Phase::AfterResponse => after_response::<P>,
    }
}

/// The `create` trampoline: reads the JSON config, runs `P::setup`,
/// returns the instance as an opaque state pointer. A `PluginError` (or a
/// panic, caught here) writes `err` and returns null.
unsafe extern "C" fn create<P: FulmenPlugin>(
    config: FfiStr,
    err: *mut FfiBuf,
) -> *mut std::ffi::c_void {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the host borrows `config` bytes it keeps live for this call.
        let config = unsafe { config.as_bytes() };
        P::setup(config)
    }));
    match result {
        Ok(Ok(plugin)) => Box::into_raw(Box::new(plugin)).cast(),
        Ok(Err(setup_err)) => {
            write_err(err, setup_err.message());
            std::ptr::null_mut()
        }
        Err(_) => {
            write_err(err, "plugin panicked during setup");
            std::ptr::null_mut()
        }
    }
}

/// The `destroy` trampoline: reconstructs the boxed plugin and drops it.
/// Panics during `Drop` are caught — teardown must never abort the host.
unsafe extern "C" fn destroy<P: FulmenPlugin>(state: *mut std::ffi::c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !state.is_null() {
            // SAFETY: `state` is the pointer `create` returned and the host
            // calls `destroy` exactly once for it.
            drop(unsafe { Box::from_raw(state.cast::<P>()) });
        }
    }));
}

/// Trampoline generator for the request-only phases.
macro_rules! request_trampoline {
    ($name:ident, $method:ident) => {
        #[doc(hidden)]
        unsafe extern "C" fn $name<P: FulmenPlugin>(
            state: *mut std::ffi::c_void,
            cx: *const FfiHookCxV1,
            out: *mut FfiResponseView,
            err: *mut FfiBuf,
        ) -> i32 {
            let decision = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: `state` is the pointer `create` returned; hooks take
                // `&self`, so a shared borrow is all the call needs.
                let plugin = unsafe { &*(state.cast::<P>()) };
                // SAFETY: `cx` is the host's call context, live for this call.
                let cx = unsafe { &*cx };
                // SAFETY: `cx` outlives the view's borrow.
                let request = unsafe { RequestView::new(cx) };
                plugin.$method(&request)
            }));
            decision_code(decision, out, err)
        }
    };
}

/// Trampoline generator for the post-handler phases (request + mutable
/// response view).
macro_rules! response_trampoline {
    ($name:ident, $method:ident) => {
        #[doc(hidden)]
        unsafe extern "C" fn $name<P: FulmenPlugin>(
            state: *mut std::ffi::c_void,
            cx: *const FfiHookCxV1,
            out: *mut FfiResponseView,
            err: *mut FfiBuf,
        ) -> i32 {
            let decision = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: `state` is the pointer `create` returned; hooks take
                // `&self`, so a shared borrow is all the call needs.
                let plugin = unsafe { &*(state.cast::<P>()) };
                // SAFETY: `cx` is the host's call context, live for this call.
                let cx = unsafe { &*cx };
                // SAFETY: `cx` outlives the view's borrow.
                let request = unsafe { RequestView::new(cx) };
                // SAFETY: `cx.response` is the host's live response view for
                // a post-handler phase.
                let mut response = unsafe { ResponseView::new(&mut *cx.response) };
                plugin.$method(&request, &mut response)
            }));
            decision_code(decision, out, err)
        }
    };
}

request_trampoline!(on_request, on_request);
request_trampoline!(before_route, before_route);
request_trampoline!(guard, guard);
request_trampoline!(resolve, resolve);
request_trampoline!(before_handle, before_handle);
response_trampoline!(after_handle, after_handle);
response_trampoline!(on_response, on_response);
response_trampoline!(after_response, after_response);

/// Map a caught decision onto the ABI return code: `0` continue, `1`
/// respond (`out` filled), `-1` failure (`err` filled).
fn decision_code(
    decision: std::thread::Result<Result<Decision, PluginError>>,
    out: *mut FfiResponseView,
    err: *mut FfiBuf,
) -> i32 {
    match decision {
        Ok(Ok(Decision::Continue)) => 0,
        Ok(Ok(Decision::Respond(response))) => {
            // SAFETY: `out` is the host's scratch response view, live for
            // this call — `decision_code` runs inside it.
            if write_response(unsafe { &mut *out }, &response) {
                1
            } else {
                write_err(err, "plugin produced an invalid response");
                -1
            }
        }
        Ok(Err(setup_err)) => {
            write_err(err, setup_err.message());
            -1
        }
        Err(_) => {
            write_err(err, "plugin hook panicked");
            -1
        }
    }
}

/// Copy `response` through the scratch view's shims.
fn write_response(out: &mut FfiResponseView, response: &PluginResponse) -> bool {
    // SAFETY: `out`'s ctx pointers are host-owned and live for the call
    // (the same contract the plugin side sees).
    let mut ok = unsafe { (out.set_status)(out.status_ctx, response.status_code()) };
    for (name, value) in response.header_pairs() {
        let Some(set) = out.headers.set else {
            return false;
        };
        // SAFETY: name/value are read synchronously by the shim.
        ok &= unsafe {
            set(
                out.headers.ctx,
                FfiStr::from_bytes(name.as_bytes()),
                FfiStr::from_bytes(value),
            )
        };
    }
    // SAFETY: `body` is copied before the shim returns.
    ok & unsafe { (out.set_body)(out.body_ctx, FfiStr::from_bytes(response.body_bytes())) }
}

/// Copy `message` into the caller-provided error buffer (truncated to
/// `cap`).
fn write_err(err: *mut FfiBuf, message: &str) {
    if err.is_null() {
        return;
    }
    // SAFETY: `err` is the host's live error buffer for this call.
    let err = unsafe { &mut *err };
    let bytes = message.as_bytes();
    let len = bytes.len().min(err.cap);
    if len > 0 && !err.ptr.is_null() {
        // SAFETY: `ptr` is host-owned with `cap` bytes of room.
        unsafe { err.ptr.copy_from_nonoverlapping(bytes.as_ptr(), len) };
    }
    err.len = len;
}
