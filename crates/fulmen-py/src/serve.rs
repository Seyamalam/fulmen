//! The serving entry points: [`serve`] backs `Fulmen.run()` and
//! [`serve_for_test`] gives tests a stoppable server on an ephemeral port.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fulmen_server::{Dispatcher, ServerConfig};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use tokio::runtime::Runtime;

use crate::app::{BridgeConfig, LifecycleHook, NativeApp, compile_app, dispatcher};
use crate::convert::log_message;
use crate::dispatch::PyDispatcher;

/// Compile `app` and serve it until the process is signalled (SIGINT/SIGTERM
/// run the server's own graceful shutdown) or the server fails.
///
/// Blocks until the server stops; the GIL is released for the whole wait so
/// Python signal handling and other threads run normally.
///
/// `uvloop` selects the asyncio loop implementation (M1.16): `None` uses
/// uvloop when importable, `False` forces plain asyncio, `True` fails
/// startup when uvloop is missing. `gil_batch_size` tunes the sync pool's
/// GIL batching (§7.5). `reuse_port` binds with
/// `SO_REUSEPORT` so worker processes can share `host:port` (§7.2, M1.23).
/// `debug` turns on §11.6 output validation.
///
/// # Errors
/// `StartupError` for declaration problems, `ValueError` for an invalid
/// `host`, `OSError` when the bind or the server fails.
#[expect(
    clippy::too_many_arguments,
    reason = "the Python signature carries every serving knob; keyword-only after `port`"
)]
#[pyfunction]
#[pyo3(signature = (app, host, port, *, debug=false, uvloop=None, gil_batch_size=32, reuse_port=false))]
pub(crate) fn serve(
    py: Python<'_>,
    app: &Bound<'_, NativeApp>,
    host: &str,
    port: u16,
    debug: bool,
    uvloop: Option<bool>,
    gil_batch_size: usize,
    reuse_port: bool,
) -> PyResult<()> {
    let native = app.borrow();
    let config_bridge = BridgeConfig {
        uvloop,
        gil_batch_size,
        debug,
    };
    let compiled = compile_app(py, &native, &config_bridge)?;
    let logger = Arc::clone(&compiled.inner.logger);
    drop(native);

    // §5.7: `on_startup` fires after a successful compile, before bind.
    run_lifecycle(py, &compiled.startup_hooks, "startup", &logger)?;

    let bind = parse_bind(host, port)?;
    let config = ServerConfig {
        bind,
        handle_signals: true,
        ..ServerConfig::default()
    };
    let disp = dispatcher(&compiled);
    let server = if reuse_port {
        let listener = bind_reuse_port(bind)?;
        py.detach(move || fulmen_server::serve_listener(config, listener, disp))?
    } else {
        py.detach(move || fulmen_server::serve(config, disp))?
    };

    let addr = server.local_addr();
    log_message(py, &logger, format!("fulmen serving on http://{addr}"));
    py.detach(move || server.wait())?;
    run_lifecycle(py, &compiled.shutdown_hooks, "shutdown", &logger)
}

/// A running test server: bound on an ephemeral port, stoppable via
/// [`stop`](TestServer::stop).
#[pyclass(name = "TestServer", module = "fulmen._fulmen")]
pub(crate) struct TestServer {
    server: Option<fulmen_server::Server>,
    addr: String,
    /// `@app.on_shutdown` hooks, run once by `stop`.
    shutdown_hooks: Option<Vec<LifecycleHook>>,
}

#[pymethods]
impl TestServer {
    /// The bound address as `"host:port"`.
    fn addr(&self) -> &str {
        &self.addr
    }

    /// Shut the server down gracefully and wait for it to stop — then run
    /// the app's `on_shutdown` hooks. Idempotent.
    fn stop(&mut self, py: Python<'_>) {
        if let Some(server) = self.server.take() {
            server.shutdown();
            let _ = py.detach(move || server.wait());
        }
        if let Some(hooks) = self.shutdown_hooks.take() {
            let _ = run_lifecycle(py, &hooks, "shutdown", &logging(py));
        }
    }

    /// `with serve_for_test(...) as server:` — returns `self`.
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Context-manager exit: [`stop`](Self::stop).
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc: &Bound<'_, PyAny>,
        _tb: &Bound<'_, PyAny>,
    ) -> bool {
        self.stop(py);
        false
    }
}

impl Drop for TestServer {
    /// Make sure a leaked `TestServer` still stops its runtime.
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown();
            let _ = server.wait();
        }
    }
}

/// Compile `app` and start it on `host:0` (an ephemeral port). Returns a
/// [`TestServer`] whose [`addr`](TestServer::addr) reports the bound port.
///
/// Signal handling stays with the process so the server never steals a
/// test runner's `SIGINT`. `uvloop` selects the asyncio loop
/// implementation exactly like [`serve`].
///
/// `debug` turns on §11.6 output validation.
///
/// # Errors
/// `StartupError` for declaration problems, `ValueError` for an invalid
/// `host`, `OSError` when the bind or the server fails.
#[pyfunction]
#[pyo3(signature = (app, host="127.0.0.1", uvloop=None, debug=false))]
pub(crate) fn serve_for_test(
    py: Python<'_>,
    app: &Bound<'_, NativeApp>,
    host: &str,
    uvloop: Option<bool>,
    debug: bool,
) -> PyResult<TestServer> {
    let native = app.borrow();
    let compiled = compile_app(
        py,
        &native,
        &BridgeConfig {
            uvloop,
            debug,
            ..BridgeConfig::default()
        },
    )?;
    drop(native);

    let ip: IpAddr = host
        .parse()
        .map_err(|_| PyValueError::new_err(format!("invalid host `{host}`")))?;
    let config = ServerConfig {
        bind: SocketAddr::new(ip, 0),
        handle_signals: false,
        // Tests stop servers often; a short graceful window keeps teardown
        // fast without breaking in-flight requests.
        shutdown_timeout: Duration::from_secs(5),
        ..ServerConfig::default()
    };
    let logger = Arc::clone(&compiled.inner.logger);
    run_lifecycle(py, &compiled.startup_hooks, "startup", &logger)?;
    let disp = dispatcher(&compiled);
    let server = py.detach(move || fulmen_server::serve(config, disp))?;
    let addr = server.local_addr().to_string();
    Ok(TestServer {
        server: Some(server),
        addr,
        shutdown_hooks: Some(compiled.shutdown_hooks),
    })
}

/// The `(status, headers, body)` triple a dispatched request answers —
/// the same shape a socket client observes off the wire.
type WireResponse = (u16, Vec<(String, String)>, Vec<u8>);

/// An in-process, socket-free dispatch client for tests (DESIGN.md §22).
///
/// Compiles the app once exactly like [`serve_for_test`], then drives
/// `PyDispatcher::dispatch` on a small tokio runtime — every request
/// runs the whole §8 pipeline (routing, hooks, guards, extraction,
/// dependency resolution, handler, encoding, response hooks, error
/// routing) with no listener, socket or HTTP framing. The wire
/// `remote_addr` is fabricated as `127.0.0.1:0`; `date`/`content-length`
/// are added to the response when absent, matching what the transport
/// appends over a socket.
#[pyclass(name = "DispatchClient", module = "fulmen._fulmen")]
pub(crate) struct DispatchClient {
    /// The compiled pipeline — its `Arc<AppInner>` keeps the executors
    /// alive for the client's lifetime.
    dispatcher: PyDispatcher,
    /// `@app.on_shutdown` hooks, run once by `stop`.
    shutdown_hooks: Option<Vec<LifecycleHook>>,
    /// Drives each `DispatchFuture`; `None` after `stop`. One worker
    /// thread is enough — `block_on` runs the request's own phases on
    /// the caller; the worker only drains fire-and-forget settles.
    runtime: Option<Runtime>,
    /// `logging.getLogger("fulmen")` for the shutdown-hook log path.
    logger: Arc<Py<PyAny>>,
}

#[pymethods]
impl DispatchClient {
    /// `request(method, path, headers, body)` → `(status, headers, body)`.
    ///
    /// `path` is the origin-form request target (`/items?x=1`);
    /// `headers` is a list of `(name, value)` pairs so repeated headers
    /// survive; `body` is `bytes` or `None`.
    #[pyo3(signature = (method, path, headers, body=None))]
    fn request(
        &self,
        py: Python<'_>,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> PyResult<WireResponse> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("DispatchClient is stopped"))?;
        let method = http::Method::from_bytes(method.as_bytes())
            .map_err(|_| PyValueError::new_err(format!("invalid HTTP method {method:?}")))?;
        let uri: http::Uri = path
            .parse()
            .map_err(|_| PyValueError::new_err(format!("invalid request target {path:?}")))?;
        let mut map = http::HeaderMap::with_capacity(headers.len());
        for (name, value) in headers {
            let name = http::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| PyValueError::new_err(format!("invalid header name {name:?}")))?;
            let value = http::HeaderValue::from_str(&value)
                .map_err(|_| PyValueError::new_err(format!("invalid value for header `{name}`")))?;
            map.append(name, value);
        }
        let request = fulmen_server::Request {
            method,
            uri,
            headers: map,
            body: body.map_or_else(Bytes::new, Bytes::from),
            // No peer exists without a socket; port `0` marks the
            // fabricated loopback address.
            remote_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        };
        // The future is `Send + 'static`; `block_on` must not hold the GIL
        // while it waits on executor completions.
        let mut response = py.detach(|| runtime.block_on(self.dispatcher.dispatch(request)));
        // The transport sets these on the wire (service.rs); repeat them
        // so the tuple matches what a socket client observes.
        let body_len = response.body.len();
        if let Ok(value) = http::HeaderValue::from_str(&body_len.to_string()) {
            response
                .headers
                .entry(http::header::CONTENT_LENGTH)
                .or_insert(value);
        }
        response
            .headers
            .entry(http::header::DATE)
            .or_insert_with(fulmen_server::date_header);
        let headers: Vec<(String, String)> = response
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        Ok((response.status.as_u16(), headers, response.body.to_vec()))
    }

    /// Shut the runtime down and run `on_shutdown` hooks once. Idempotent.
    fn stop(&mut self, py: Python<'_>) {
        if let Some(runtime) = self.runtime.take() {
            // The transport-analogue goes down before the lifecycle, like
            // `TestServer.stop`: bound wait, then hooks.
            py.detach(move || runtime.shutdown_timeout(Duration::from_secs(1)));
        }
        if let Some(hooks) = self.shutdown_hooks.take() {
            let _ = run_lifecycle(py, &hooks, "shutdown", &self.logger);
        }
    }

    /// `with dispatch_for_test(...) as client:` — returns `self`.
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Context-manager exit: [`stop`](Self::stop).
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc: &Bound<'_, PyAny>,
        _tb: &Bound<'_, PyAny>,
    ) -> bool {
        self.stop(py);
        false
    }
}

/// Compile `app` for in-process dispatch: the same compile and startup
/// lifecycle as [`serve_for_test`], minus the listener (DESIGN.md §22).
///
/// The returned [`DispatchClient`] answers requests by running the
/// compiled pipeline directly, so tests exercise production routing,
/// hooks, extraction and error routing without opening a socket.
///
/// `uvloop` selects the asyncio loop implementation exactly like
/// [`serve`]; `debug` turns on §11.6 output validation.
///
/// # Errors
/// `StartupError` for declaration or executor problems.
#[pyfunction]
#[pyo3(signature = (app, uvloop=None, debug=false))]
pub(crate) fn dispatch_for_test(
    py: Python<'_>,
    app: &Bound<'_, NativeApp>,
    uvloop: Option<bool>,
    debug: bool,
) -> PyResult<DispatchClient> {
    let native = app.borrow();
    let compiled = compile_app(
        py,
        &native,
        &BridgeConfig {
            uvloop,
            debug,
            ..BridgeConfig::default()
        },
    )?;
    drop(native);
    let logger = Arc::clone(&compiled.inner.logger);
    run_lifecycle(py, &compiled.startup_hooks, "startup", &logger)?;
    // One worker thread: `block_on` runs each request's phases on the
    // caller; the worker only drains fire-and-forget settles (e.g.
    // `after_response`). Multi-thread so concurrent `request` calls
    // from Python threads may `block_on` at once.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|err| {
            crate::app::StartupError::new_err(format!(
                "failed to start the dispatch runtime: {err}"
            ))
        })?;
    Ok(DispatchClient {
        dispatcher: dispatcher(&compiled),
        shutdown_hooks: Some(compiled.shutdown_hooks),
        runtime: Some(runtime),
        logger,
    })
}

/// Run `on_startup`/`on_shutdown` hooks: `def` hooks call directly,
/// `async def` coroutines run on a throwaway `asyncio` loop (the app's
/// own loop thread is only warranted during serving). A startup failure
/// propagates — the app never starts half-initialized; a shutdown failure
/// logs and lets the rest run.
fn run_lifecycle(
    py: Python<'_>,
    hooks: &[LifecycleHook],
    kind: &str,
    logger: &Py<PyAny>,
) -> PyResult<()> {
    for hook in hooks {
        let outcome = hook.callable.call0(py).and_then(|ret| {
            if hook.is_async {
                py.import("asyncio")?.call_method1("run", (ret.bind(py),))?;
            }
            Ok(())
        });
        if let Err(err) = outcome {
            if kind == "startup" {
                return Err(err);
            }
            log_message(py, logger, format!("on_{kind} hook raised: {err}"));
        }
    }
    Ok(())
}

/// `logging.getLogger("fulmen")` for contexts without a compiled app.
fn logging(py: Python<'_>) -> Py<PyAny> {
    py.import("logging")
        .and_then(|m| m.getattr("getLogger"))
        .and_then(|f| f.call1(("fulmen",)))
        .map_or_else(|_| py.None(), Bound::unbind)
}

/// Build a `SocketAddr` from `host:port`; `host` may be a bare IP or a
/// bracketed IPv6 literal.
fn parse_bind(host: &str, port: u16) -> PyResult<SocketAddr> {
    let ip: IpAddr = host.parse().map_err(|_| {
        PyValueError::new_err(format!("invalid host `{host}` (expected an IP literal)"))
    })?;
    Ok(SocketAddr::new(ip, port))
}

/// Bind `addr` with `SO_REUSEPORT` so several worker processes can listen
/// on the same `host:port` (§7.2, M1.23). Used only by the worker-supervisor
/// path in `python/fulmen/workers.py`.
#[cfg(unix)]
fn bind_reuse_port(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// `SO_REUSEPORT` does not exist off POSIX; the Python layer refuses
/// `workers > 1` before reaching this, so reaching it is a misuse.
#[cfg(not(unix))]
fn bind_reuse_port(_addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SO_REUSEPORT is not supported on this platform",
    ))
}
