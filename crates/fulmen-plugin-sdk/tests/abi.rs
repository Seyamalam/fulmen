//! ABI v1 contract tests (ROADMAP M3.24): a plugin built against the
//! current SDK loads and reports `abi_version == host`, a
//! deliberately-wrong-version descriptor is refused with an
//! `expected …, found …` error, and every cross-ABI call is contained —
//! panics surface as hook failures, never unwinds.

use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use fulmen_plugin_sdk::host::{HookArgs, HookOutcome, LoadedPlugin, ResponseMut, describe};
use fulmen_plugin_sdk::{
    Decision, FULMEN_PLUGIN_ABI, FulmenPlugin, FulmenPluginDescriptorV1, PluginError,
    PluginResponse, RequestView, ResponseView,
};
use http::{HeaderMap, HeaderValue, StatusCode};

/// A plugin exercising the read/mutate surface: request header in,
/// response header out, and a `Decision::Respond` short-circuit.
struct EchoPlugin;

impl FulmenPlugin for EchoPlugin {
    const NAME: &'static str = "echo";
    const VERSION: &'static str = "1.2.3";

    fn setup(config: &[u8]) -> Result<Self, PluginError> {
        // The host hands the JSON config through verbatim.
        if config == b"fail" {
            return Err(PluginError::new("config said fail"));
        }
        Ok(Self)
    }

    fn on_request(&self, cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        assert_eq!(cx.method(), "GET");
        assert_eq!(cx.path(), "/hello");
        assert_eq!(cx.query(), Some("a=1"));
        assert_eq!(cx.header("x-seen"), Some(b"yes".as_slice()));
        let names: Vec<&str> = cx.headers().map(|(name, _)| name).collect();
        assert!(names.contains(&"x-seen"), "{names:?}");
        if cx.header("x-short").is_some() {
            return Ok(Decision::Respond(
                PluginResponse::new(403)
                    .header("x-blocked", "true".as_bytes().to_vec())
                    .body("denied"),
            ));
        }
        Ok(Decision::Continue)
    }

    fn on_response(
        &self,
        _cx: &RequestView<'_>,
        res: &mut ResponseView<'_>,
    ) -> Result<Decision, PluginError> {
        res.set_header("x-native", b"1");
        Ok(Decision::Continue)
    }
}

fulmen_plugin_sdk::export_plugin!(EchoPlugin, on_request, on_response);

/// A plugin whose `on_request` panics — the boundary must turn it into a
/// `Panicked` outcome, not an unwind.
struct PanicPlugin;

impl FulmenPlugin for PanicPlugin {
    const NAME: &'static str = "panic";
    const VERSION: &'static str = "0.0.1";

    fn setup(_config: &[u8]) -> Result<Self, PluginError> {
        Ok(Self)
    }

    #[expect(
        clippy::panic,
        reason = "fixture panics deliberately to verify host-side panic containment"
    )]
    fn on_request(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        panic!("plugin blew up on purpose");
    }
}

fulmen_plugin_sdk::export_plugin!(PanicPlugin as PANIC_V1, on_request);

/// A plugin whose `setup` panics.
struct SetupPanicPlugin;

impl FulmenPlugin for SetupPanicPlugin {
    const NAME: &'static str = "setup-panic";
    const VERSION: &'static str = "0.0.1";

    #[expect(
        clippy::panic,
        reason = "fixture panics deliberately to verify host-side panic containment"
    )]
    fn setup(_config: &[u8]) -> Result<Self, PluginError> {
        panic!("setup blew up on purpose");
    }
}

fulmen_plugin_sdk::export_plugin!(SetupPanicPlugin as SETUP_PANIC_V1);

/// A plugin counting `destroy` calls — verifies the host's
/// exactly-once teardown.
struct DropPlugin;

static DROPS: AtomicUsize = AtomicUsize::new(0);

impl Drop for DropPlugin {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

impl FulmenPlugin for DropPlugin {
    const NAME: &'static str = "drop";
    const VERSION: &'static str = "0.0.1";

    fn setup(_config: &[u8]) -> Result<Self, PluginError> {
        Ok(Self)
    }
}

fulmen_plugin_sdk::export_plugin!(DropPlugin as DROP_V1);

/// The descriptor `export_plugin!` published for `T`, as a `NonNull` —
/// what the host reads out of the capsule.
fn descriptor_of(
    entry: extern "C" fn() -> *const FulmenPluginDescriptorV1,
) -> NonNull<FulmenPluginDescriptorV1> {
    NonNull::new(entry().cast_mut()).unwrap_or_else(|| unreachable!("entry returns a static"))
}

/// A deliberately-wrong-version descriptor (the `fulmen_plugin_v1_bad`
/// fixture in `examples/native-plugin`, kept in-process here).
fn bad_descriptor() -> NonNull<FulmenPluginDescriptorV1> {
    static BAD: OnceLock<FulmenPluginDescriptorV1> = OnceLock::new();
    let descriptor = BAD.get_or_init(|| {
        let mut d = fulmen_plugin_sdk::describe::<EchoPlugin>(&[]);
        d.abi_version = 9999;
        d
    });
    NonNull::from(descriptor)
}

/// Minimal `HookArgs` for a GET `/hello?a=1` carrying `x-seen: yes` and
/// `x-short` when `short` is set.
fn args<'a>(
    headers: &'a HeaderMap,
    body: &'a [u8],
    response: Option<ResponseMut<'a>>,
) -> HookArgs<'a> {
    HookArgs {
        method: "GET",
        path: "/hello",
        query: Some("a=1"),
        headers,
        body,
        remote_addr: "127.0.0.1:8080".parse().unwrap_or_else(|_| unreachable!()),
        response,
    }
}

/// Request headers for [`args`].
fn request_headers(short: bool) -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert("x-seen", HeaderValue::from_static("yes"));
    if short {
        map.insert("x-short", HeaderValue::from_static("1"));
    }
    map
}

#[test]
fn current_sdk_descriptor_loads_and_reports_host_abi() {
    // M3.24: a plugin built against the current SDK reports
    // abi_version == host and its hook phases.
    let info = describe(descriptor_of(fulmen_plugin_v1)).unwrap();
    assert_eq!(info.abi_version, FULMEN_PLUGIN_ABI);
    assert_eq!(info.name, "echo");
    assert_eq!(info.version, "1.2.3");
    assert_eq!(
        info.phases,
        vec![
            fulmen_plugin_sdk::Phase::OnRequest,
            fulmen_plugin_sdk::Phase::OnResponse,
        ]
    );

    let plugin = LoadedPlugin::load(descriptor_of(fulmen_plugin_v1)).unwrap();
    assert_eq!(plugin.name(), "echo");
    assert!(plugin.has_hook(0));
    assert!(!plugin.has_hook(1));
    assert!(plugin.has_hook(6));
    plugin.create(br#"{"any":"json"}"#).unwrap();
}

#[test]
fn wrong_abi_version_is_refused_with_expected_found_error() {
    // M3.24: the deliberately-wrong-version descriptor must fail with the
    // clear expected-vs-found message.
    let err = LoadedPlugin::load(bad_descriptor()).unwrap_err();
    let message = err.to_string();
    assert_eq!(
        message,
        format!(
            "native plugin ABI mismatch: expected {FULMEN_PLUGIN_ABI}, found 9999 — \
             rebuild the plugin against the matching fulmen-plugin-sdk"
        )
    );
    assert!(message.contains("expected"), "{message}");
    assert!(message.contains("found"), "{message}");

    // `describe` refuses the same way.
    assert!(describe(bad_descriptor()).is_err());
}

#[test]
fn hook_runs_and_mutates_the_response() {
    let plugin = LoadedPlugin::load(descriptor_of(fulmen_plugin_v1)).unwrap();
    plugin.create(b"{}").unwrap();

    // Decision phase: request view sees method/path/query/headers.
    let headers = request_headers(false);
    let mut call = args(&headers, b"", None);
    assert!(matches!(
        plugin.run_hook(0, &mut call),
        HookOutcome::Continue
    ));

    // Post-handler phase: the response view mutates the real response.
    let mut status = StatusCode::OK;
    let mut res_headers = HeaderMap::new();
    let mut body = Bytes::from_static(b"hi");
    let mut call = args(
        &headers,
        b"",
        Some(ResponseMut {
            status: &mut status,
            headers: &mut res_headers,
            body: &mut body,
        }),
    );
    assert!(matches!(
        plugin.run_hook(6, &mut call),
        HookOutcome::Continue
    ));
    assert_eq!(
        res_headers.get("x-native"),
        Some(&HeaderValue::from_static("1"))
    );
}

#[test]
fn decision_respond_short_circuits_with_parts() {
    let plugin = LoadedPlugin::load(descriptor_of(fulmen_plugin_v1)).unwrap();
    plugin.create(b"{}").unwrap();

    let headers = request_headers(true);
    let mut call = args(&headers, b"", None);
    match plugin.run_hook(0, &mut call) {
        HookOutcome::Respond(parts) => {
            assert_eq!(parts.status, StatusCode::FORBIDDEN);
            assert_eq!(
                parts.headers.get("x-blocked"),
                Some(&HeaderValue::from_static("true"))
            );
            assert_eq!(parts.body, Bytes::from_static(b"denied"));
        }
        other => panic!("expected Respond, got {other:?}"),
    }
}

#[test]
fn plugin_panic_is_contained_as_a_hook_failure() {
    let plugin = LoadedPlugin::load(descriptor_of(PANIC_V1)).unwrap();
    plugin.create(b"{}").unwrap();

    let headers = request_headers(false);
    let mut call = args(&headers, b"", None);
    match plugin.run_hook(0, &mut call) {
        HookOutcome::Panicked(message) => {
            assert!(message.contains("panic"), "{message}");
        }
        other => panic!("expected Panicked, got {other:?}"),
    }
}

#[test]
fn setup_failures_and_panics_are_startup_errors() {
    let plugin = LoadedPlugin::load(descriptor_of(fulmen_plugin_v1)).unwrap();
    match plugin.create(b"fail") {
        Err(err) => assert_eq!(
            err.to_string(),
            "native plugin setup failed: config said fail"
        ),
        Ok(()) => panic!("expected create to fail"),
    }

    let panicking = LoadedPlugin::load(descriptor_of(SETUP_PANIC_V1)).unwrap();
    match panicking.create(b"{}") {
        // The SDK trampoline caught the panic and reported it through
        // `err` — `CreatePanic` is reserved for a panic escaping the
        // trampoline itself (a non-SDK plugin).
        Err(err) => assert_eq!(
            err.to_string(),
            "native plugin setup failed: plugin panicked during setup"
        ),
        Ok(()) => panic!("expected create to fail"),
    }
}

#[test]
fn destroy_runs_exactly_once_on_drop() {
    let before = DROPS.load(Ordering::SeqCst);
    {
        let plugin = LoadedPlugin::load(descriptor_of(DROP_V1)).unwrap();
        plugin.create(b"{}").unwrap();
    }
    assert_eq!(DROPS.load(Ordering::SeqCst), before + 1);

    // A plugin never `create`d is never `destroy`ed.
    {
        let _plugin = LoadedPlugin::load(descriptor_of(DROP_V1)).unwrap();
    }
    assert_eq!(DROPS.load(Ordering::SeqCst), before + 1);
}

#[test]
fn hook_before_create_is_a_failure_not_ub() {
    let plugin = LoadedPlugin::load(descriptor_of(fulmen_plugin_v1)).unwrap();
    let headers = request_headers(false);
    let mut call = args(&headers, b"", None);
    assert!(matches!(
        plugin.run_hook(0, &mut call),
        HookOutcome::Panicked(_)
    ));
}
