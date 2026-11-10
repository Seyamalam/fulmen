//! Per-connection read-side timeout enforcement.
//!
//! hyper drives one connection future; it has no knob to tell "waiting for
//! the first byte of the next request" (`keep_alive_idle_timeout`) from "a
//! request head is partially read" (`header_read_timeout`). [`TimeoutIo`]
//! wraps the accepted socket, tracks which phase the connection is in via
//! [`Phase`] (shared with the service), and applies the right deadline to
//! each stalled read. On expiry it writes a real `408` response when a
//! request was in progress and then errors the read, which tears the
//! connection down.
//!
//! Deadline model (`docs/DESIGN.md` §14.4):
//!
//! | Phase | Meaning | Deadline |
//! | --- | --- | --- |
//! | `Head` | first byte seen, head incomplete | `header_read_timeout`, total |
//! | `Body` | head parsed, body incomplete | `body_read_timeout`, total |
//! | `Busy` | body collected, dispatcher running | none |
//! | `Idle` | response written, awaiting next request | `keep_alive_idle_timeout`, per stall |

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, Sleep};

use crate::date::DateCache;

/// Request-read phase shared between [`TimeoutIo`] and the service.
///
/// The I/O wrapper moves `Idle`/`Busy` to `Head` when bytes arrive; the
/// service moves `Head` to `Body` when hyper hands it a parsed request,
/// `Body` to `Busy` when the body is collected, and back to `Idle` when
/// the response is ready.
#[derive(Debug)]
pub(crate) enum Phase {
    /// Reading a request head; must finish by `until`.
    Head { until: Instant },
    /// Reading a request body; must finish by `until`.
    Body { until: Instant },
    /// Dispatching; socket reads carry no deadline.
    Busy,
    /// Between requests on a keep-alive connection.
    Idle,
}

/// Shared cell the service and the I/O wrapper transition through.
pub(crate) type SharedPhase = Arc<Mutex<Phase>>;

fn lock(phase: &Mutex<Phase>) -> MutexGuard<'_, Phase> {
    phase.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Head parsed, body collection begins (`service` calls this).
pub(crate) fn body_started(phase: &SharedPhase, until: Instant) {
    *lock(phase) = Phase::Body { until };
}

/// Body collected, dispatch begins.
pub(crate) fn dispatch_started(phase: &SharedPhase) {
    let mut phase = lock(phase);
    if matches!(*phase, Phase::Body { .. }) {
        *phase = Phase::Busy;
    }
}

/// Response ready to write. A partially-read pipelined head keeps its own
/// deadline instead of reverting to idle.
pub(crate) fn response_ready(phase: &SharedPhase) {
    let mut phase = lock(phase);
    if matches!(*phase, Phase::Body { .. } | Phase::Busy) {
        *phase = Phase::Idle;
    }
}

/// Mark a connection idle when the future that produced the response
/// exits, on any path including early returns.
pub(crate) struct IdleOnDrop<'a>(&'a SharedPhase);

impl<'a> IdleOnDrop<'a> {
    pub(crate) fn new(phase: &'a SharedPhase) -> Self {
        Self(phase)
    }
}

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        response_ready(self.0);
    }
}

/// Timeouts applied by [`TimeoutIo`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct IoTimeouts {
    /// Total budget for one request head.
    pub(crate) header_read: Duration,
    /// Stall budget while idle between requests.
    pub(crate) keep_alive_idle: Duration,
}

/// A `TcpStream` wrapper that enforces the read-side timeouts.
///
/// Writes pass straight through. Read stalls are bounded by the current
/// [`Phase`]; when a deadline expires the wrapper writes a minimal `408`
/// (unless the connection was idle) and fails the read so hyper closes
/// the connection.
#[derive(Debug)]
pub(crate) struct TimeoutIo {
    inner: TcpStream,
    phase: SharedPhase,
    timeouts: IoTimeouts,
    date: Arc<DateCache>,
    sleep: Pin<Box<Sleep>>,
    /// Start of the current stalled read in `Idle`, i.e. when the keep
    /// alive clock began. Kept here (not in `Phase`) because only this
    /// task observes stalls.
    idle_since: Option<Instant>,
    /// Deadline currently armed in `sleep`, to avoid re-arming it on
    /// every poll.
    armed: Option<Instant>,
}

impl TimeoutIo {
    pub(crate) fn new(
        inner: TcpStream,
        phase: SharedPhase,
        timeouts: IoTimeouts,
        date: Arc<DateCache>,
    ) -> Self {
        Self {
            inner,
            phase,
            timeouts,
            date,
            sleep: Box::pin(tokio::time::sleep(Duration::from_secs(0))),
            idle_since: None,
            armed: None,
        }
    }

    /// Bytes arrived while waiting between or during requests: a new head
    /// just started.
    fn on_data(&mut self) {
        let mut phase = lock(&self.phase);
        if matches!(*phase, Phase::Idle | Phase::Busy) {
            *phase = Phase::Head {
                until: Instant::now() + self.timeouts.header_read,
            };
        }
    }

    /// The deadline for a read that just stalled, or `None` in `Busy`.
    fn deadline(&mut self) -> Option<Instant> {
        match *lock(&self.phase) {
            Phase::Head { until } | Phase::Body { until } => {
                self.idle_since = None;
                Some(until)
            }
            Phase::Idle => {
                let since = self.idle_since.get_or_insert_with(Instant::now);
                Some(*since + self.timeouts.keep_alive_idle)
            }
            Phase::Busy => {
                self.idle_since = None;
                None
            }
        }
    }

    /// A deadline expired: answer `408` if a request was in progress and
    /// fail the read.
    fn expire(&mut self) -> Poll<io::Result<()>> {
        let in_request = matches!(*lock(&self.phase), Phase::Head { .. } | Phase::Body { .. });
        if in_request {
            let mut response = [0u8; 128];
            let len = timeout_response(&self.date, &mut response);
            let _ = self.inner.try_write(&response[..len]);
        }
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "request read timed out",
        )))
    }
}

/// Serialize the minimal `408` sent by [`TimeoutIo`] on expiry, including
/// the cached `Date`. Returns the number of bytes written to `out`.
fn timeout_response(date: &DateCache, out: &mut [u8; 128]) -> usize {
    const HEAD: &str =
        "HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\ndate: ";
    out[..HEAD.len()].copy_from_slice(HEAD.as_bytes());
    out[HEAD.len()..HEAD.len() + 29].copy_from_slice(&date.bytes());
    let tail = HEAD.len() + 29;
    out[tail..tail + 4].copy_from_slice(b"\r\n\r\n");
    tail + 4
}

impl AsyncRead for TimeoutIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > filled {
                    this.on_data();
                }
                this.idle_since = None;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                let Some(deadline) = this.deadline() else {
                    return Poll::Pending;
                };
                if Instant::now() >= deadline {
                    return this.expire();
                }
                if this.armed != Some(deadline) {
                    this.armed = Some(deadline);
                    this.sleep.as_mut().reset(deadline);
                }
                if this.sleep.as_mut().poll(cx).is_ready() {
                    this.armed = None;
                    return this.expire();
                }
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for TimeoutIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_transitions() {
        let phase: SharedPhase = Arc::new(Mutex::new(Phase::Idle));
        response_ready(&phase);
        assert!(matches!(*lock(&phase), Phase::Idle));
        body_started(&phase, Instant::now());
        assert!(matches!(*lock(&phase), Phase::Body { .. }));
        dispatch_started(&phase);
        assert!(matches!(*lock(&phase), Phase::Busy));
        response_ready(&phase);
        assert!(matches!(*lock(&phase), Phase::Idle));
    }

    #[test]
    fn response_ready_keeps_pipelined_head_deadline() {
        let until = Instant::now() + Duration::from_secs(10);
        let phase: SharedPhase = Arc::new(Mutex::new(Phase::Head { until }));
        response_ready(&phase);
        assert!(matches!(*lock(&phase), Phase::Head { until: u } if u == until));
    }
}
