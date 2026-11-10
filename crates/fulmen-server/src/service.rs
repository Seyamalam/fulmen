//! The per-request path: limit checks, body collection, dispatch, and
//! turning the wire [`Response`] back into a hyper response.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use http::header::{CONNECTION, CONTENT_LENGTH, DATE};
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use tokio::time::{Instant, timeout};

use crate::conn::{IdleOnDrop, SharedPhase, body_started, dispatch_started};
use crate::date::DateCache;
use crate::{Dispatcher, ServerConfig};
use crate::{Request, Response};

/// Per-connection context cloned into each service call.
pub(crate) struct ConnCtx {
    pub(crate) dispatcher: Arc<dyn Dispatcher>,
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) date: Arc<DateCache>,
    pub(crate) phase: SharedPhase,
    pub(crate) remote_addr: SocketAddr,
}

/// Handle one request on a keep-alive connection.
///
/// Never fails at the hyper layer: every rejection is a real HTTP
/// response (`docs/DESIGN.md` §8), and dispatcher panics become `500`
/// because the dispatch runs in its own task.
pub(crate) async fn handle(
    ctx: ConnCtx,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    body_started(&ctx.phase, Instant::now() + ctx.config.body_read_timeout);
    // On every exit path the phase returns to Idle (or keeps a pipelined
    // head's deadline).
    let _idle = IdleOnDrop::new(&ctx.phase);

    if let Some(rejection) = check_head(&ctx.config, &req) {
        return Ok(rejection);
    }

    let (parts, mut body) = req.into_parts();
    let body = match collect(&ctx, &mut body).await {
        Ok(body) => body,
        Err(rejection) => return Ok(rejection),
    };
    dispatch_started(&ctx.phase);

    let wire = Request {
        method: parts.method,
        uri: parts.uri,
        headers: parts.headers,
        body,
        remote_addr: ctx.remote_addr,
    };
    let response = dispatch(&ctx, wire).await;
    Ok(build_response(&ctx, response))
}

/// Run the dispatcher in its own task so a panic becomes `500` instead of
/// poisoning the connection task (`docs/DESIGN.md` §14.6). Calling
/// `dispatch` inside the task catches panics both when the future is
/// built and when it is polled.
async fn dispatch(ctx: &ConnCtx, request: Request) -> Response {
    let dispatcher = Arc::clone(&ctx.dispatcher);
    match tokio::task::spawn(async move { dispatcher.dispatch(request).await }).await {
        Ok(response) => response,
        Err(_) => Response::status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Check the parsed head against the configured limits, before the body
/// is read.
fn check_head(
    config: &ServerConfig,
    req: &hyper::Request<Incoming>,
) -> Option<hyper::Response<Full<Bytes>>> {
    if uri_len(req.uri()) > config.max_url_length {
        return Some(rejection(StatusCode::URI_TOO_LONG));
    }
    let headers = req.headers();
    if headers.len() > config.max_header_count {
        return Some(rejection(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE));
    }
    // Approximate the header block size from the parsed fields: per header
    // `name: value\r\n` is name + value + 4 bytes, plus the final CRLF.
    // The raw head is also bounded by hyper's read buffer, so the true
    // size can only be a few bytes off.
    let block = headers.iter().fold(2usize, |acc, (name, value)| {
        acc + name.as_str().len() + value.len() + 4
    });
    if block > config.max_header_block_size {
        return Some(rejection(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE));
    }
    if declared_too_large(headers, config.max_body_size) {
        return Some(rejection(StatusCode::PAYLOAD_TOO_LARGE));
    }
    None
}

/// Serialized length of the request target, rebuilt from its parsed
/// parts. Origin-form is `path?query`; absolute-form adds scheme and
/// authority, which is a small underestimate (no `://` counted for the
/// authority-only CONNECT form) and safe for a byte limit.
fn uri_len(uri: &http::Uri) -> usize {
    uri.path().len()
        + uri.query().map_or(0, |q| q.len() + 1)
        + uri.scheme_str().map_or(0, |s| s.len() + 3)
        + uri.authority().map_or(0, |a| a.as_str().len())
}

/// `Content-Length` above the body limit: reject without reading a byte.
fn declared_too_large(headers: &HeaderMap, max: usize) -> bool {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|length| length > u64::try_from(max).unwrap_or(u64::MAX))
}

/// Collect the request body, enforcing the size cap and read timeout.
///
/// Rejections close the connection because the unread body leaves the
/// stream in an unknown position for keep-alive.
async fn collect(
    ctx: &ConnCtx,
    body: &mut Incoming,
) -> Result<Bytes, hyper::Response<Full<Bytes>>> {
    let max = ctx.config.max_body_size;
    let read = async {
        let mut buf = BytesMut::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| Rejection::BadBody)?;
            if let Ok(data) = frame.into_data() {
                if buf.len() + data.len() > max {
                    return Err(Rejection::TooLarge);
                }
                buf.extend_from_slice(&data);
            }
        }
        Ok(buf.freeze())
    };
    match timeout(ctx.config.body_read_timeout, read).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(rejection)) => Err(rejection.into_response()),
        Err(_) => Err(Rejection::TimedOut.into_response()),
    }
}

/// Body-collection failures that map to a response status.
enum Rejection {
    TooLarge,
    TimedOut,
    BadBody,
}

impl Rejection {
    fn into_response(self) -> hyper::Response<Full<Bytes>> {
        let status = match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TimedOut => StatusCode::REQUEST_TIMEOUT,
            Self::BadBody => StatusCode::BAD_REQUEST,
        };
        rejection(status)
    }
}

/// A rejection response with `connection: close` when the request stream
/// is in an unknown position (early rejections and body errors).
fn rejection(status: StatusCode) -> hyper::Response<Full<Bytes>> {
    let reason = status.canonical_reason().unwrap_or("error");
    let mut response = hyper::Response::new(Full::new(Bytes::from(reason.to_owned() + "\n")));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONNECTION, http::HeaderValue::from_static("close"));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain"),
    );
    response
}

/// Turn the wire [`Response`] into a hyper response, adding `Date`.
fn build_response(ctx: &ConnCtx, wire: Response) -> hyper::Response<Full<Bytes>> {
    let mut response = hyper::Response::new(Full::new(wire.body));
    *response.status_mut() = wire.status;
    *response.headers_mut() = wire.headers;
    response
        .headers_mut()
        .entry(DATE)
        .or_insert_with(|| ctx.date.get());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_check() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "2048".parse().unwrap());
        assert!(declared_too_large(&headers, 1024));
        assert!(!declared_too_large(&headers, 4096));
        assert!(!declared_too_large(&HeaderMap::new(), 1));
        headers.insert(CONTENT_LENGTH, "not-a-number".parse().unwrap());
        let mut only = HeaderMap::new();
        only.insert(CONTENT_LENGTH, "bogus".parse().unwrap());
        assert!(!declared_too_large(&only, 1));
    }
}
