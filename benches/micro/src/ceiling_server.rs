//! The pure-Rust ceiling server for the end-to-end benchmarks
//! (`docs/ROADMAP.md` M1.25, M1.27).
//!
//! Serves `fulmen_server::testing::TestDispatcher` over a real socket so an
//! `oha` run measures the transport bound — accept, parse, dispatch, write —
//! before any interpreter work is counted. The routes mirror the M1
//! scenarios byte for byte (`benches/e2e/README.md`):
//!
//! - `GET /` -> `200`, `hello world`, `text/plain`
//! - `GET /json` -> `200`, `{"message":"hello world"}`, `application/json`
//! - `GET /items/42` -> `200`, `{"item_id":42}`, `application/json`
//!
//! `/items/42` is an exact-match route: the ceiling is the cost of serving
//! the request, not of parameter extraction, so the literal path the
//! benchmark requests is the honest bound.
//!
//! Usage: `ceiling-server [--host IP] [--port N] [--threads N]` — defaults
//! `127.0.0.1:8106`, tokio worker threads = CPU cores. `SIGINT`/`SIGTERM`
//! shut down gracefully.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::thread::available_parallelism;

use fulmen_server::testing::TestDispatcher;
use fulmen_server::{Response, ServerConfig, serve, serve_listener};
use http::Method;

/// Parsed command line: where to bind and how many tokio threads to run.
struct Args {
    bind: SocketAddr,
    threads: usize,
    reuse_port: bool,
}

fn main() -> io::Result<()> {
    let args = parse_args()?;
    let config = ServerConfig {
        bind: args.bind,
        worker_threads: args.threads,
        ..ServerConfig::default()
    };
    let dispatcher = TestDispatcher::new()
        .route(Method::GET, "/", Response::text("hello world"))
        .route(
            Method::GET,
            "/json",
            Response::json(r#"{"message":"hello world"}"#),
        )
        .route(
            Method::GET,
            "/items/42",
            Response::json(r#"{"item_id":42}"#),
        );
    let server = if args.reuse_port {
        serve_listener(config, reuse_port_listener(args.bind)?, dispatcher)?
    } else {
        serve(config, dispatcher)?
    };
    server.wait()
}

/// Read `--host`, `--port`, `--threads` and `--reuse-port` from argv.
/// Every value-less or unknown argument, and every unparsable value, is a
/// usage error: a benchmark binary should fail loudly, never guess.
fn parse_args() -> io::Result<Args> {
    let mut host: IpAddr = IpAddr::from([127, 0, 0, 1]);
    let mut port = 8106u16;
    let mut threads = available_parallelism().map_or(1, std::num::NonZero::get);
    let mut reuse_port = false;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let usage = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown or malformed argument `{arg}`; usage: ceiling-server \
                     [--host IP] [--port N] [--threads N] [--reuse-port]"
                ),
            )
        };
        match arg.as_str() {
            "--host" => {
                host = argv
                    .next()
                    .ok_or_else(usage)?
                    .parse()
                    .map_err(|_| usage())?;
            }
            "--port" => {
                port = argv
                    .next()
                    .ok_or_else(usage)?
                    .parse()
                    .map_err(|_| usage())?;
            }
            "--threads" => {
                threads = argv
                    .next()
                    .ok_or_else(usage)?
                    .parse()
                    .map_err(|_| usage())?;
            }
            "--reuse-port" => reuse_port = true,
            _ => return Err(usage()),
        }
    }
    Ok(Args {
        bind: SocketAddr::new(host, port),
        threads,
        reuse_port,
    })
}

/// Bind `addr` with `SO_REUSEPORT` so several ceiling-server processes can
/// share one port, matching how the Python workers serve (`§7.2`, D15).
#[cfg(unix)]
fn reuse_port_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};

    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// `SO_REUSEPORT` does not exist off POSIX.
#[cfg(not(unix))]
fn reuse_port_listener(_addr: SocketAddr) -> io::Result<TcpListener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_REUSEPORT is not supported on this platform",
    ))
}
