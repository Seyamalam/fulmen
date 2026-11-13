//! The Fulmen native-plugin SDK (DESIGN.md §13.4): write request-pipeline
//! hooks in Rust, compile them into a shared library, and load the result
//! through `app.use(NativePlugin("path/to/lib.so"))`.
//!
//! A plugin implements [`FulmenPlugin`] — `setup` plus one method per
//! hookable [`Phase`] — and publishes it with [`export_plugin!`], which
//! generates the `fulmen_plugin_v1` entry symbol and the versioned
//! descriptor the host's `PyCapsule` loader consumes. Everything the host
//! hands a hook arrives as borrowed `(ptr, len)` data valid for the call;
//! hooks return [`Decision`] to continue or short-circuit.
//!
//! The crate is the one place `unsafe` lives for the native-plugin
//! feature: the ABI types in [`abi`], the generated call shims in
//! [`export`], and the host-side loader in [`host`]. Plugin crates
//! themselves stay `unsafe_code`-free — `export_plugin!` emits the one
//! `unsafe` attribute (the exported symbol) under an `#[expect]` that
//! documents why.
#![expect(
    unsafe_code,
    reason = "the native-plugin ABI boundary lives here: FFI types, call trampolines and descriptor dereference"
)]

pub mod abi;
mod export;
pub mod host;
mod phase;
mod views;

/// This crate's component name, as `native_components()` reports it.
pub const NAME: &str = "plugin-sdk";

pub use abi::{
    FULMEN_PLUGIN_ABI, FULMEN_PLUGIN_CAPSULE, FULMEN_PLUGIN_SYMBOL, FfiBuf, FfiHeaderPair,
    FfiHeaders, FfiHookCxV1, FfiResponseView, FfiSocketAddr, FfiStr, FulmenPluginDescriptorV1,
};
#[doc(hidden)]
pub use export::{__hook_fn, describe};
pub use phase::Phase;
pub use views::{HeaderIter, RequestView, ResponseView};

/// A hook's verdict — mirrors `fulmen_core::pipeline::Decision`.
#[derive(Debug)]
pub enum Decision {
    /// Continue the pipeline.
    Continue,
    /// Answer immediately with `response` (a short-circuit — auth
    /// rejection, CORS preflight, cached answer).
    Respond(PluginResponse),
}

/// A response a plugin builds — for a `Decision::Respond` short-circuit.
///
/// Plain Rust data (`u16` status, `Box<str>`/`Box<[u8]>` fields); the
/// host's view shims copy it into its own `StatusCode`/`HeaderMap`/`Bytes`
/// before the hook returns, so nothing here is borrowed across the
/// boundary.
#[derive(Debug, Default)]
pub struct PluginResponse {
    status: u16,
    headers: Vec<(Box<str>, Box<[u8]>)>,
    body: Vec<u8>,
}

impl PluginResponse {
    /// A response with the given status code and an empty body.
    pub fn new(status: u16) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// Set the response body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Append a `(name, value)` header field.
    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>, value: impl Into<Vec<u8>>) -> Self {
        self.headers
            .push((name.as_ref().into(), value.into().into_boxed_slice()));
        self
    }

    /// A `200` `text/plain; charset=utf-8` response carrying `text`.
    pub fn text(text: impl Into<Vec<u8>>) -> Self {
        Self::new(200)
            .header(
                "content-type",
                "text/plain; charset=utf-8".as_bytes().to_vec(),
            )
            .body(text)
    }

    /// A `200` `application/json` response carrying `body`.
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200)
            .header("content-type", "application/json".as_bytes().to_vec())
            .body(body)
    }

    /// Read access for the SDK's own writers (not part of the plugin API
    /// surface a typical author needs).
    #[doc(hidden)]
    pub fn status_code(&self) -> u16 {
        self.status
    }

    /// Read access — see [`status_code`](Self::status_code).
    #[doc(hidden)]
    pub fn header_pairs(&self) -> &[(Box<str>, Box<[u8]>)] {
        &self.headers
    }

    /// Read access — see [`status_code`](Self::status_code).
    #[doc(hidden)]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }
}

/// A plugin failure — a `setup` error becomes the app's startup error
/// (a plugin that cannot initialize never reaches the pipeline); a hook
/// error becomes a 500 problem response (§13.4).
#[derive(Debug)]
pub struct PluginError {
    message: String,
}

impl PluginError {
    /// Wrap `message` (`format!` output is fine).
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The failure text the host reports.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Any `Display` value converts — `err` values from `std` and libraries
/// just `?` through `setup`.
impl<E: std::fmt::Display> From<E> for PluginError {
    fn from(err: E) -> Self {
        Self::new(err.to_string())
    }
}

/// A native plugin: `setup` plus one method per hookable phase.
///
/// Implement only the phases you need and name them to
/// [`export_plugin!`]; unimplemented phases cost nothing (their
/// descriptor slot stays `None` and the host never calls).
///
/// `Send + Sync` is required and hook methods take `&self`: the host may
/// call a hook concurrently across dispatch threads (one instance serves
/// the whole app, including on a free-threaded build). Keep mutable
/// state in interior-mutability cells; per-request data belongs in the
/// request itself (e.g. response headers a later phase can read).
pub trait FulmenPlugin: Send + Sync + Sized + 'static {
    /// Stable plugin name — used in `use_plugin(name)` matching,
    /// diagnostics, and hook names (`"{name}:{phase}"`).
    const NAME: &'static str;
    /// Plugin version (metadata only).
    const VERSION: &'static str;

    /// Build the instance. `config` is the plugin's configuration as JSON
    /// (the `config` argument of `use()`), empty when none was given.
    ///
    /// Called once at app compile; failure is a startup error.
    ///
    /// # Errors
    /// Any `PluginError` — the message reaches the app's startup report.
    fn setup(config: &[u8]) -> Result<Self, PluginError>;

    /// `on_request` — before routing: request-id, trusted-proxy work.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn on_request(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `before_route` — route known; CORS preflight.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn before_route(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `guard` — allow/deny checks; `Decision::Respond` rejects.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn guard(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `resolve` — the native dependency-resolution seam.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn resolve(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `before_handle` — immediately before the handler.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn before_handle(&self, _cx: &RequestView<'_>) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `after_handle` — the response exists; a `Respond` decision
    /// replaces it.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn after_handle(
        &self,
        _cx: &RequestView<'_>,
        _res: &mut ResponseView<'_>,
    ) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `on_response` — after the response is encoded; mutate headers on
    /// any answer.
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn on_response(
        &self,
        _cx: &RequestView<'_>,
        _res: &mut ResponseView<'_>,
    ) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }

    /// `after_response` — the response is produced; fire-and-forget
    /// (access log, metrics, background work).
    ///
    /// # Errors
    /// `PluginError` — the host turns it into a 500 problem response.
    fn after_response(
        &self,
        _cx: &RequestView<'_>,
        _res: &mut ResponseView<'_>,
    ) -> Result<Decision, PluginError> {
        Ok(Decision::Continue)
    }
}

/// Publish a `FulmenPlugin` to the host.
///
/// ```rust,ignore
/// pub struct MyPlugin;
/// impl fulmen_plugin_sdk::FulmenPlugin for MyPlugin {
///     const NAME: &'static str = "my-plugin";
///     const VERSION: &'static str = "0.1.0";
///     fn setup(_config: &[u8]) -> Result<Self, fulmen_plugin_sdk::PluginError> {
///         Ok(Self)
///     }
///     fn on_response(&self, _: &RequestView<'_>, res: &mut ResponseView<'_>)
///         -> Result<Decision, PluginError>
///     {
///         res.set_header("x-powered-by", b"my-plugin");
///         Ok(Decision::Continue)
///     }
/// }
/// fulmen_plugin_sdk::export_plugin!(MyPlugin, on_response);
/// ```
///
/// Emits `pub extern "C" fn fulmen_plugin_v1() -> *const
/// FulmenPluginDescriptorV1` — the symbol `NativePlugin` looks up in the
/// shared library. The `#[expect(unsafe_code)]` on the generated item is
/// deliberate: the exported-symbol attribute is the single unsafe
/// declaration the plugin crate needs, and it is generated, never written
/// by the author (the plugin crate otherwise stays `unsafe_code`-denied).
#[macro_export]
macro_rules! export_plugin {
    // `export_plugin!(P as entry, phases…)` — an alternate symbol name,
    // for test fixtures and multi-plugin builds (the default symbol is
    // `fulmen_plugin_v1`).
    ($ty:ty as $entry:ident $(,)?) => {
        $crate::export_plugin! { @emit $entry, $ty, }
    };
    ($ty:ty as $entry:ident, $($phase:ident),+ $(,)?) => {
        $crate::export_plugin! { @emit $entry, $ty, $($phase),+ }
    };
    ($ty:ty $(,)?) => {
        $crate::export_plugin! { @emit fulmen_plugin_v1, $ty, }
    };
    ($ty:ty, $($phase:ident),+ $(,)?) => {
        $crate::export_plugin! { @emit fulmen_plugin_v1, $ty, $($phase),+ }
    };
    (@emit $entry:ident, $ty:ty, $($phase:ident),* $(,)?) => {
        /// The v1 entry point `NativePlugin` looks up via `ctypes`
        /// (DESIGN.md §13.4): returns the plugin's static descriptor.
        ///
        /// The descriptor lives in a fn-local `static` here — deliberately
        /// not inside `__entry`'s generic body, where a static may be
        /// shared across monomorphizations by linker folding.
        #[expect(
            unsafe_code,
            reason = "export_plugin! generates the ABI entry point — the host finds the descriptor by symbol name; plugin authors write no unsafe themselves"
        )]
        #[unsafe(no_mangle)]
        pub extern "C" fn $entry() -> *const $crate::FulmenPluginDescriptorV1 {
            static DESCRIPTOR: ::std::sync::OnceLock<$crate::FulmenPluginDescriptorV1> =
                ::std::sync::OnceLock::new();
            ::std::ptr::from_ref(DESCRIPTOR.get_or_init(|| {
                $crate::describe::<$ty>(&[
                    $($crate::__fulmen_phase!($phase)),*
                ])
            }))
        }
    };
}

/// Maps a phase name (`export_plugin!`'s trailing arguments) to its
/// [`Phase`] variant — the names are exactly the trait's method names.
#[doc(hidden)]
#[macro_export]
macro_rules! __fulmen_phase {
    (on_request) => {
        $crate::Phase::OnRequest
    };
    (before_route) => {
        $crate::Phase::BeforeRoute
    };
    (guard) => {
        $crate::Phase::Guard
    };
    (resolve) => {
        $crate::Phase::Resolve
    };
    (before_handle) => {
        $crate::Phase::BeforeHandle
    };
    (after_handle) => {
        $crate::Phase::AfterHandle
    };
    (on_response) => {
        $crate::Phase::OnResponse
    };
    (after_response) => {
        $crate::Phase::AfterResponse
    };
}
