//! The [`Dispatcher`] trait: the server's only interface to handlers.

use std::future::Future;
use std::pin::Pin;

use crate::{Request, Response};

/// The future returned by [`Dispatcher::dispatch`].
///
/// Boxed so the trait stays object-safe (`Arc<dyn Dispatcher>`) and
/// `'static` so it can be spawned as its own task; see the crate-level
/// docs for why this shape was chosen.
pub type DispatchFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// The server's only interface to request handling (`docs/DESIGN.md` §6.2).
///
/// Implemented by `fulmen-py` for real apps and by
/// [`testing::TestDispatcher`](crate::testing::TestDispatcher) for tests
/// and benchmarks. Implementations must be `Send + Sync + 'static`: the
/// same dispatcher is shared across every worker thread and connection.
///
/// Each call (including building the future) is spawned as its own tokio
/// task, so a panic becomes a `500` response instead of killing the
/// connection task (`docs/DESIGN.md` §14.6).
pub trait Dispatcher: Send + Sync + 'static {
    /// Handle one request. The body is already collected and within the
    /// configured limits.
    fn dispatch(&self, request: Request) -> DispatchFuture;
}

impl<F, Fut> Dispatcher for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn dispatch(&self, request: Request) -> DispatchFuture {
        Box::pin((self)(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closure_dispatchers_work() {
        let d = |req: Request| async move {
            Response::status(if req.uri.path() == "/ok" {
                http::StatusCode::OK
            } else {
                http::StatusCode::NOT_FOUND
            })
        };
        let req = |path: &str| Request {
            method: http::Method::GET,
            uri: path.parse().unwrap(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
            remote_addr: "127.0.0.1:1".parse().unwrap(),
        };
        assert_eq!(d.dispatch(req("/ok")).await.status, http::StatusCode::OK);
        assert_eq!(
            d.dispatch(req("/no")).await.status,
            http::StatusCode::NOT_FOUND
        );
        let boxed: std::sync::Arc<dyn Dispatcher> = std::sync::Arc::new(d);
        assert_eq!(
            boxed.dispatch(req("/ok")).await.status,
            http::StatusCode::OK
        );
    }
}
