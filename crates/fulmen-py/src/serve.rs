//! The serving entry points: [`serve`] backs `Fulmen.run()` and
//! [`serve_for_test`] gives tests a stoppable server on an ephemeral port.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use fulmen_server::ServerConfig;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::app::{NativeApp, compile_app, dispatcher};
use crate::convert::log_message;

/// Compile `app` and serve it until the process is signalled (SIGINT/SIGTERM
/// run the server's own graceful shutdown) or the server fails.
///
/// Blocks until the server stops; the GIL is released for the whole wait so
/// Python signal handling and other threads run normally.
///
/// # Errors
/// `StartupError` for declaration problems, `ValueError` for an invalid
/// `host`, `OSError` when the bind or the server fails.
#[pyfunction]
#[pyo3(signature = (app, host, port, workers=1, debug=false))]
pub(crate) fn serve(
    py: Python<'_>,
    app: &Bound<'_, NativeApp>,
    host: &str,
    port: u16,
    workers: u32,
    debug: bool,
) -> PyResult<()> {
    let _ = debug; // accepted for API stability; debug behaviour is M2+.
    let native = app.borrow();
    let compiled = compile_app(py, &native)?;
    let logger = std::sync::Arc::clone(&compiled.inner.logger);
    if workers > 1 {
        log_message(
            py,
            &logger,
            format!(
                "workers={workers} requested; multi-process serving is not implemented yet, running a single process"
            ),
        );
    }
    drop(native);

    let bind = parse_bind(host, port)?;
    let config = ServerConfig {
        bind,
        handle_signals: true,
        ..ServerConfig::default()
    };
    let disp = dispatcher(&compiled);
    let server = py.detach(move || fulmen_server::serve(config, disp))?;

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
/// test runner's `SIGINT`.
///
/// # Errors
/// `StartupError` for declaration problems, `ValueError` for an invalid
/// `host`, `OSError` when the bind or the server fails.
#[pyfunction]
#[pyo3(signature = (app, host="127.0.0.1"))]
pub(crate) fn serve_for_test(
    py: Python<'_>,
    app: &Bound<'_, NativeApp>,
    host: &str,
) -> PyResult<TestServer> {
    let native = app.borrow();
    let compiled = compile_app(py, &native)?;
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
