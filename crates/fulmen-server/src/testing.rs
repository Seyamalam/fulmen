//! A pure-Rust [`Dispatcher`] with fixed routes.
//!
//! Drives the server tests today and the benchmarks later (`docs/ROADMAP.md`
//! M1.4, M1.27): it shows the transport's ceiling before any Python work is
//! counted.

use std::time::Duration;

use http::{Method, StatusCode};

use crate::dispatcher::DispatchFuture;
use crate::{Dispatcher, Request, Response};

/// What a route replies with.
#[derive(Clone, Debug)]
enum RouteBody {
    /// A fixed response, cloned per request (`Bytes` clones are cheap).
    Fixed(Response),
    /// Echo the request body back as `application/octet-stream`.
    Echo,
    /// A fixed response sent after a delay (drain and timeout tests).
    Delayed(Response, Duration),
}

#[derive(Debug)]
struct Route {
    method: Method,
    path: String,
    body: RouteBody,
}

/// A [`Dispatcher`] that answers exact-match routes with fixed responses.
///
/// ```
/// use fulmen_server::testing::TestDispatcher;
///
/// let dispatcher = TestDispatcher::hello_world();
/// ```
#[derive(Debug)]
pub struct TestDispatcher {
    routes: Vec<Route>,
}

impl TestDispatcher {
    /// An empty dispatcher: every request gets `404`.
    #[must_use]
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// The benchmark preset: `GET /plaintext` returns `hello world` as
    /// `text/plain` and `GET /json` returns a small JSON object.
    #[must_use]
    pub fn hello_world() -> Self {
        Self::new()
            .route(Method::GET, "/plaintext", Response::text("hello world"))
            .route(Method::GET, "/json", Response::json(r#"{"hello":"world"}"#))
    }

    /// Add an exact-match route answered with `response`.
    #[must_use]
    pub fn route(mut self, method: Method, path: impl Into<String>, response: Response) -> Self {
        self.routes.push(Route {
            method,
            path: path.into(),
            body: RouteBody::Fixed(response),
        });
        self
    }

    /// Add a route that echoes the request body back.
    #[must_use]
    pub fn route_echo(self, method: Method, path: impl Into<String>) -> Self {
        self.route_kind(method, path, RouteBody::Echo)
    }

    /// Add a route that waits `delay` before answering (graceful-shutdown
    /// and timeout tests).
    #[must_use]
    pub fn route_delayed(
        self,
        method: Method,
        path: impl Into<String>,
        response: Response,
        delay: Duration,
    ) -> Self {
        self.route_kind(method, path, RouteBody::Delayed(response, delay))
    }

    fn route_kind(mut self, method: Method, path: impl Into<String>, body: RouteBody) -> Self {
        self.routes.push(Route {
            method,
            path: path.into(),
            body,
        });
        self
    }
}

impl Default for TestDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher for TestDispatcher {
    fn dispatch(&self, request: Request) -> DispatchFuture {
        let path = request.uri.path();
        let Some(route) = self
            .routes
            .iter()
            .find(|route| route.method == request.method && route.path == path)
        else {
            return Box::pin(async { Response::status(StatusCode::NOT_FOUND) });
        };
        let response = match &route.body {
            RouteBody::Echo => Response::bytes(request.body.clone()),
            RouteBody::Fixed(response) | RouteBody::Delayed(response, _) => response.clone(),
        };
        let delay = match &route.body {
            RouteBody::Delayed(_, delay) => *delay,
            _ => Duration::ZERO,
        };
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn get(path: &str) -> Request {
        Request {
            method: Method::GET,
            uri: path.parse().unwrap(),
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
            remote_addr: SocketAddr::from(([127, 0, 0, 1], 1)),
        }
    }

    #[tokio::test]
    async fn hello_world_routes() {
        let d = TestDispatcher::hello_world();
        let res = d.dispatch(get("/plaintext")).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(&res.body[..], b"hello world");
        let res = d.dispatch(get("/json")).await;
        assert_eq!(&res.body[..], br#"{"hello":"world"}"#);
        assert_eq!(
            d.dispatch(get("/missing")).await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn echo_returns_body() {
        let d = TestDispatcher::new().route_echo(Method::POST, "/echo");
        let mut req = get("/echo");
        req.method = Method::POST;
        req.body = Bytes::from_static(b"ping");
        let res = d.dispatch(req).await;
        assert_eq!(&res.body[..], b"ping");
    }
}
