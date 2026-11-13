//! The example Fulmen native plugin (DESIGN.md §13.4, ROADMAP M3.16).
//!
//! `RequestIdPlugin` stamps an `x-request-id` response header on every
//! answer: the request's own `x-request-id` when the client sent one,
//! else a `{prefix}-{n}` id from an atomic counter. It demonstrates the
//! whole SDK surface — `setup` reading JSON config, a request-header
//! read, a response-header write, and interior-mutable state across
//! concurrent calls — in about sixty lines.
//!
//! Build (`just` does this for the tests): `cargo build -p
//! fulmen-example-plugin`, then load `target/debug/libfulmen_example_plugin
//! .{so,dylib,dll}` with `app.use(NativePlugin(path))`.

use std::sync::atomic::{AtomicU64, Ordering};

use fulmen_plugin_sdk::{Decision, FulmenPlugin, PluginError, RequestView, ResponseView};

/// The plugin — a counter is the only state (interior mutability keeps
/// `&self` hooks sound under concurrent dispatch).
#[derive(Debug)]
pub struct RequestIdPlugin {
    /// Prefix for generated ids — `config.prefix`, default `"req"`.
    prefix: Box<str>,
    /// Monotonic id source.
    next: AtomicU64,
}

impl FulmenPlugin for RequestIdPlugin {
    const NAME: &'static str = "request-id";
    const VERSION: &'static str = "0.1.0";

    /// `config` is JSON: `{"prefix": "myapp"}` overrides the id prefix.
    fn setup(config: &[u8]) -> Result<Self, PluginError> {
        let prefix = if config.is_empty() {
            "req".into()
        } else {
            let value: serde_json::Value = serde_json::from_slice(config)?;
            value
                .get("prefix")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("req")
                .into()
        };
        Ok(Self {
            prefix,
            next: AtomicU64::new(1),
        })
    }

    fn on_response(
        &self,
        cx: &RequestView<'_>,
        res: &mut ResponseView<'_>,
    ) -> Result<Decision, PluginError> {
        if res.header("x-request-id").is_none() {
            let id = cx.header("x-request-id").map_or_else(
                || {
                    format!(
                        "{}-{}",
                        self.prefix,
                        self.next.fetch_add(1, Ordering::Relaxed)
                    )
                    .into_bytes()
                },
                <[u8]>::to_vec,
            );
            res.set_header("x-request-id", &id);
        }
        Ok(Decision::Continue)
    }
}

fulmen_plugin_sdk::export_plugin!(RequestIdPlugin, on_response);

/// A test fixture plugin: `on_request` panics — the host must contain the
/// unwind and answer `500`, never abort or propagate UB (§13.4).
#[derive(Debug)]
pub struct PanicPlugin;

impl FulmenPlugin for PanicPlugin {
    const NAME: &'static str = "panic";
    const VERSION: &'static str = "0.1.0";

    fn setup(_config: &[u8]) -> Result<Self, PluginError> {
        Ok(Self)
    }

    #[expect(
        clippy::panic,
        reason = "PanicPlugin is a fixture: its hook panics deliberately so the containment test verifies the ABI boundary"
    )]
    fn on_request(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        panic!("PanicPlugin: deliberate hook panic for the containment test");
    }
}

fulmen_plugin_sdk::export_plugin!(PanicPlugin as fulmen_plugin_v1_panic, on_request);

/// A second entry point exposing a descriptor stamped with a deliberately
/// wrong ABI version — the M3.24 compat test loads it and expects the
/// host to refuse with an `expected …, found …` error.
#[doc(hidden)]
#[expect(
    unsafe_code,
    reason = "test-only ABI-mismatch descriptor needs the same exported-symbol mechanism export_plugin! uses"
)]
#[unsafe(no_mangle)]
pub extern "C" fn fulmen_plugin_v1_bad() -> *const fulmen_plugin_sdk::FulmenPluginDescriptorV1 {
    static BAD: std::sync::OnceLock<fulmen_plugin_sdk::FulmenPluginDescriptorV1> =
        std::sync::OnceLock::new();
    BAD.get_or_init(|| {
        // The real descriptor machinery, re-versioned — the host must
        // refuse this before ever calling `create`.
        let mut descriptor = fulmen_plugin_sdk::describe::<RequestIdPlugin>(&[]);
        descriptor.abi_version = 9999;
        descriptor
    })
}
