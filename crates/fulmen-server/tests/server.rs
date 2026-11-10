//! Integration tests for `fulmen-server`: a real server on an ephemeral
//! port, driven by a raw `std::net::TcpStream` client (no HTTP client
//! crate, so we see exactly what goes over the wire).
#![expect(
    clippy::expect_used,
    reason = "helper functions panic with context; failures are test failures"
)]
#![expect(
    clippy::panic,
    reason = "the panic dispatcher must panic on purpose to test 500 handling"
)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use fulmen_server::testing::TestDispatcher;
use fulmen_server::{DispatchFuture, Dispatcher, Request, Response, Server, ServerConfig, serve};
use http::Method;

const REQ_PLAINTEXT: &[u8] = b"GET /plaintext HTTP/1.1\r\nhost: test\r\n\r\n";

/// One parsed HTTP response.
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn config() -> ServerConfig {
    ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        worker_threads: 2,
        handle_signals: false,
        ..ServerConfig::default()
    }
}

fn start(dispatcher: impl Dispatcher, cfg: ServerConfig) -> Server {
    serve(cfg, dispatcher).expect("serve failed")
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set_read_timeout failed");
    stream
}

/// Read until `\r\n\r\n`, then parse `Content-Length` bytes of body (or
/// read to EOF when the server closes).
fn read_response(stream: &TcpStream) -> RawResponse {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    let mut raw = stream;
    while !head.ends_with(b"\r\n\r\n") {
        let n = raw.read(&mut byte).expect("read head failed");
        assert!(n > 0, "connection closed before response head");
        head.push(byte[0]);
        assert!(head.len() < 64 * 1024, "response head too large");
    }
    let text = String::from_utf8(head).expect("head not utf-8");
    let mut lines = text.split("\r\n");
    let status_line = lines.next().expect("empty response");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("no status code")
        .parse()
        .expect("bad status code");
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').expect("malformed header");
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let response = RawResponse {
        status,
        headers,
        body: Vec::new(),
    };
    let body = if let Some(len) = response
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        let mut body = vec![0u8; len];
        raw.read_exact(&mut body).expect("read body failed");
        body
    } else {
        let mut body = Vec::new();
        let _ = raw.read_to_end(&mut body);
        body
    };
    RawResponse { body, ..response }
}

fn roundtrip(stream: &mut TcpStream, request: &[u8]) -> RawResponse {
    stream.write_all(request).expect("write failed");
    read_response(stream)
}

#[test]
fn plaintext_response_has_date_and_no_server_header() {
    let server = start(TestDispatcher::hello_world(), config());
    let mut stream = connect(server.local_addr());
    let res = roundtrip(&mut stream, REQ_PLAINTEXT);
    assert_eq!(res.status, 200);
    assert_eq!(res.body, b"hello world");
    assert_eq!(res.header("content-type"), Some("text/plain"));
    assert!(res.header("date").is_some(), "missing Date header");
    assert!(res.header("server").is_none(), "Server header must be off");
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn keep_alive_and_pipelining() {
    let server = start(TestDispatcher::hello_world(), config());
    let mut stream = connect(server.local_addr());

    // Sequential requests on one connection.
    let first = roundtrip(&mut stream, REQ_PLAINTEXT);
    let second = roundtrip(&mut stream, REQ_PLAINTEXT);
    assert_eq!(first.status, 200);
    assert_eq!(second.status, 200);

    // Two requests in one write: pipelining must answer both, in order.
    stream
        .write_all(&[REQ_PLAINTEXT, REQ_PLAINTEXT].concat())
        .expect("write failed");
    let (a, b) = (read_response(&stream), read_response(&stream));
    assert_eq!(a.status, 200);
    assert_eq!(b.status, 200);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn rejects_too_many_headers_with_431() {
    let server = start(TestDispatcher::hello_world(), config());
    let mut stream = connect(server.local_addr());
    let mut request = String::from("GET /plaintext HTTP/1.1\r\nhost: test\r\n");
    for i in 0..100 {
        use std::fmt::Write as _;
        let _ = write!(request, "x-pad-{i}: v\r\n");
    }
    request.push_str("\r\n");
    let res = roundtrip(&mut stream, request.as_bytes());
    assert_eq!(res.status, 431);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn rejects_too_long_url_with_414() {
    let server = start(TestDispatcher::hello_world(), config());
    let mut stream = connect(server.local_addr());
    let request = format!(
        "GET /{} HTTP/1.1\r\nhost: test\r\n\r\n",
        "a".repeat(9 * 1024)
    );
    let res = roundtrip(&mut stream, request.as_bytes());
    assert_eq!(res.status, 414);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn rejects_oversized_declared_body_with_413() {
    let server = start(
        TestDispatcher::new().route_echo(Method::POST, "/echo"),
        config(),
    );
    let mut stream = connect(server.local_addr());
    // Content-Length over the limit: rejected before the body is read.
    let request = b"POST /echo HTTP/1.1\r\nhost: test\r\ncontent-length: 2097153\r\n\r\n";
    let res = roundtrip(&mut stream, request);
    assert_eq!(res.status, 413);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn incomplete_head_times_out_with_408() {
    let cfg = ServerConfig {
        header_read_timeout: Duration::from_millis(150),
        ..config()
    };
    let server = start(TestDispatcher::hello_world(), cfg);
    let mut stream = connect(server.local_addr());
    stream
        .write_all(b"GET /plaintext HTTP/1.1\r\nhost: test\r\n")
        .expect("write failed");
    let res = read_response(&stream);
    assert_eq!(res.status, 408);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn stalled_body_times_out_with_408() {
    let cfg = ServerConfig {
        body_read_timeout: Duration::from_millis(150),
        ..config()
    };
    let server = start(TestDispatcher::new().route_echo(Method::POST, "/echo"), cfg);
    let mut stream = connect(server.local_addr());
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nhost: test\r\ncontent-length: 1048576\r\n\r\npartial")
        .expect("write failed");
    let res = read_response(&stream);
    assert_eq!(res.status, 408);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn idle_keep_alive_connection_is_closed() {
    let cfg = ServerConfig {
        keep_alive_idle_timeout: Duration::from_millis(150),
        ..config()
    };
    let server = start(TestDispatcher::hello_world(), cfg);
    let mut stream = connect(server.local_addr());
    let res = roundtrip(&mut stream, REQ_PLAINTEXT);
    assert_eq!(res.status, 200);
    thread::sleep(Duration::from_millis(400));
    // The idle connection should be closed: read returns EOF (or the OS
    // reports a reset).
    let mut buf = [0u8; 8];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "idle connection still open");
    server.shutdown();
    server.wait().expect("wait failed");
}

/// A dispatcher that panics on `/panic` and answers elsewhere.
struct Fragile;

impl Dispatcher for Fragile {
    fn dispatch(&self, request: Request) -> DispatchFuture {
        if request.uri.path() == "/panic" {
            Box::pin(async { panic!("dispatcher blew up") })
        } else {
            Box::pin(async { Response::text("fine") })
        }
    }
}

#[test]
fn dispatcher_panic_is_500_and_connection_survives() {
    let server = start(Fragile, config());
    let mut stream = connect(server.local_addr());
    let crashed = roundtrip(&mut stream, b"GET /panic HTTP/1.1\r\nhost: test\r\n\r\n");
    assert_eq!(crashed.status, 500);
    // The connection task survived the poisoned dispatch.
    let alive = roundtrip(&mut stream, b"GET /other HTTP/1.1\r\nhost: test\r\n\r\n");
    assert_eq!(alive.status, 200);
    server.shutdown();
    server.wait().expect("wait failed");
}

#[test]
fn graceful_shutdown_drains_and_stops_accepting() {
    let server = start(
        TestDispatcher::new().route_delayed(
            Method::GET,
            "/slow",
            Response::text("done"),
            Duration::from_millis(200),
        ),
        config(),
    );
    let addr = server.local_addr();
    let mut stream = connect(addr);
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nhost: test\r\n\r\n")
        .expect("write failed");
    // Let the request reach the dispatcher before shutting down.
    thread::sleep(Duration::from_millis(50));
    server.shutdown();
    // The in-flight request still completes.
    let res = read_response(&stream);
    assert_eq!(res.status, 200);
    assert_eq!(res.body, b"done");
    server.wait().expect("wait failed");

    // The listener is gone and the old connection is closed.
    let mut buf = [0u8; 8];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while TcpStream::connect(addr).is_ok() {
        assert!(Instant::now() < deadline, "listener still accepting");
        thread::sleep(Duration::from_millis(20));
    }
}
