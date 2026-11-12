//! The serving entry points: [`serve`] backs `Fulmen.run()` and
//! [`serve_for_test`] gives tests a stoppable server on an ephemeral port.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use fulmen_server::ServerConfig;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::app::{BridgeConfig, NativeApp, compile_app, dispatcher};
use crate::convert::log_message;

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
/// `debug` turns on §14.2 output validation.
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
    let logger = std::sync::Arc::clone(&compiled.inner.logger);
    drop(native);

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
    Ok(())
}

/// A running test server: bound on an ephemeral port, stoppable via
/// [`stop`](TestServer::stop).
#[pyclass(name = "TestServer", module = "fulmen._fulmen")]
pub(crate) struct TestServer {
    server: Option<fulmen_server::Server>,
    addr: String,
}

#[pymethods]
impl TestServer {
    /// The bound address as `"host:port"`.
    fn addr(&self) -> &str {
        &self.addr
    }

    /// Shut the server down gracefully and wait for it to stop. Idempotent.
    fn stop(&mut self, py: Python<'_>) {
        if let Some(server) = self.server.take() {
            server.shutdown();
            let _ = py.detach(move || server.wait());
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
/// `debug` turns on §14.2 output validation.
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
    let disp = dispatcher(&compiled);
    let server = py.detach(move || fulmen_server::serve(config, disp))?;
    let addr = server.local_addr().to_string();
    Ok(TestServer {
        server: Some(server),
        addr,
    })
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
