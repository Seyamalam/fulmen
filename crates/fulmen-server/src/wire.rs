//! Wire-level request and response types exchanged with a [`Dispatcher`].

use std::net::SocketAddr;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri, header};

/// A single HTTP request as the server sees it on the wire.
///
/// The body is already fully collected and within the configured limits,
/// so a [`Dispatcher`](crate::Dispatcher) never touches the socket.
#[derive(Debug)]
pub struct Request {
    /// HTTP method.
    pub method: Method,
    /// Request target: origin-form path plus query.
    pub uri: Uri,
    /// Request headers.
    pub headers: HeaderMap,
    /// Collected request body.
    pub body: Bytes,
    /// Peer address of the connection this request arrived on.
    pub remote_addr: SocketAddr,
}

/// A single HTTP response produced by a [`Dispatcher`](crate::Dispatcher).
#[derive(Clone, Debug)]
pub struct Response {
    /// Status code.
    pub status: StatusCode,
    /// Response headers. `Content-Length` is set by the server from
    /// [`body`](Self::body); `Date` is added when absent, so a dispatcher
    /// may supply its own.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

impl Response {
    /// A response with an empty body and the given status.
    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    /// `200 OK` with a `text/plain` body.
    pub fn text(body: impl Into<Bytes>) -> Self {
        Self::typed(StatusCode::OK, header::CONTENT_TYPE, "text/plain", body)
    }

    /// `200 OK` with an `application/json` body.
    pub fn json(body: impl Into<Bytes>) -> Self {
        Self::typed(
            StatusCode::OK,
            header::CONTENT_TYPE,
            "application/json",
            body,
        )
    }

    /// `200 OK` with `application/octet-stream`.
    pub fn bytes(body: impl Into<Bytes>) -> Self {
        Self::typed(
            StatusCode::OK,
            header::CONTENT_TYPE,
            "application/octet-stream",
            body,
        )
    }

    fn typed(
        status: StatusCode,
        name: header::HeaderName,
        value: &'static str,
        body: impl Into<Bytes>,
    ) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(name, header::HeaderValue::from_static(value));
        Self {
            status,
            headers,
            body: body.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_set_content_type() {
        let res = Response::text("hi");
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.headers[header::CONTENT_TYPE], "text/plain");
        assert_eq!(&res.body[..], b"hi");
        let res = Response::json("{}");
        assert_eq!(res.headers[header::CONTENT_TYPE], "application/json");
    }
}
