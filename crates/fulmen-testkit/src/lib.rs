//! `fulmen-testkit` (DESIGN.md §15.3): a runnable conformance suite per
//! adapter interface.
//!
//! A third-party adapter proves it behaves like the built-ins by calling
//! the suite for the interface it implements — every check is a `pub`
//! function that panics on a contract violation, so it composes into
//! `#[test]`s directly:
//!
//! ```no_run
//! use fulmen_core::adapters::MemorySessionStore;
//! let store = MemorySessionStore::new();
//! fulmen_testkit::session::suite(&store);
//! ```
//!
//! The bundled `Memory*` implementations run through the same suites in
//! this crate's `tests/` — the suites never special-case them.
//!
//! Timing: the suites exercise real TTL/lease/deadline behaviour, so a
//! few checks sleep tens of milliseconds. All waits are short and
//! bounded — no suite exceeds a second in total.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

pub mod cache;
pub mod jobs;
pub mod ratelimit;
pub mod session;
pub mod validator;

/// Drive `future` to completion on the current thread — a minimal
/// executor for the adapter traits' [`StoreFuture`]s.
///
/// [`StoreFuture`]: fulmen_core::adapters::StoreFuture
///
/// The waker parks/unparks the polling thread, which is all the
/// in-memory stores need — their futures resolve on first poll, and
/// `MemoryJobQueue::dequeue` wakes its waiter from the timer thread.
/// Works inside spawned threads too (each gets its own parker).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Parker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// The [`block_on`] waker: unparks the parked polling thread. Spurious
/// unparks are harmless — `block_on` just re-polls and re-parks.
struct Parker(std::thread::Thread);

impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
