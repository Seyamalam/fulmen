//! `serve` and [`Server`]: runtime, accept loop and graceful shutdown.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};

use crate::conn::{IoTimeouts, Phase, SharedPhase, TimeoutIo};
use crate::date::DateCache;
use crate::service::{ConnCtx, handle};
use crate::{Dispatcher, ServerConfig};

/// State shared by the accept loop and every connection task.
struct Shared {
    dispatcher: Arc<dyn Dispatcher>,
    config: Arc<ServerConfig>,
    date: Arc<DateCache>,
    connections: AtomicUsize,
}

/// A running server returned by [`serve`].
///
/// `shutdown()` is idempotent and non-blocking; `wait()` blocks the
/// calling thread until the listener is closed and connections are
/// drained or force-closed after `shutdown_timeout`.
#[derive(Debug)]
pub struct Server {
    runtime: Runtime,
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

impl Server {
    /// The bound address. With port `0` this is the ephemeral port the OS
    /// picked, which is how tests find the server.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Trigger graceful shutdown: stop accepting, let in-flight
    /// connections finish, close the rest after `shutdown_timeout`
    /// (`docs/DESIGN.md` §14.4). Idempotent.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Block until the server has fully stopped, then consume the handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the supervisor task itself failed.
    pub fn wait(self) -> io::Result<()> {
        let Self {
            runtime,
            supervisor,
            ..
        } = self;
        runtime
            .block_on(supervisor)
            .map_err(|err| io::Error::other(format!("server supervisor failed: {err}")))
    }
}

/// Bind the socket, build the tokio runtime and start serving.
///
/// The returned [`Server`] owns the runtime. `SIGINT`/`SIGTERM` trigger
/// graceful shutdown unless [`ServerConfig::handle_signals`] is `false`;
/// embedders can always call [`Server::shutdown`] themselves.
///
/// The bind runs inside the new runtime and is awaited over a channel, so
/// `serve` does not need `Runtime::block_on` and is safe to call inside or
/// outside an async context.
///
/// # Errors
///
/// Returns an error if the configuration is invalid, the runtime cannot
/// be built, or the bind fails.
pub fn serve<D: Dispatcher>(config: ServerConfig, dispatcher: D) -> io::Result<Server> {
    config.validate()?;
    let runtime = Builder::new_multi_thread()
        .worker_threads(config.worker_threads)
        .thread_name("fulmen-server")
        .enable_all()
        .build()?;

    // Bind inside the new runtime without `block_on`, so `serve` also
    // works when the caller is already inside a tokio runtime.
    let bind = config.bind;
    let (bind_tx, bind_rx) = mpsc::channel();
    runtime.spawn(async move {
        let _ = bind_tx.send(TcpListener::bind(bind).await);
    });
    let listener = bind_rx
        .recv()
        .map_err(|_| io::Error::other("bind task ended without a result"))??;
    let local_addr = listener.local_addr()?;

    let (shutdown, rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        dispatcher: Arc::new(dispatcher),
        config: Arc::new(config),
        date: Arc::new(DateCache::new()),
        connections: AtomicUsize::new(0),
    });
    let supervisor = runtime.spawn(supervise(listener, shared, shutdown.clone(), rx));

    Ok(Server {
        runtime,
        local_addr,
        shutdown,
        supervisor,
    })
}

/// Accept connections until shutdown, then drain them.
async fn supervise(
    listener: TcpListener,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    mut rx: watch::Receiver<bool>,
) {
    let mut conns = JoinSet::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(refresh_dates(Arc::clone(&shared.date), rx.clone()));
    if shared.config.handle_signals {
        tasks.spawn(watch_signals(shutdown.clone(), rx.clone()));
    }

    loop {
        tokio::select! {
            biased;
            _ = rx.changed() => break,
            _ = conns.join_next(), if !conns.is_empty() => {}
            accept = listener.accept() => match accept {
                Ok((stream, peer)) => accept_conn(stream, peer, &shared, rx.clone(), &mut conns),
                Err(_) => sleep(Duration::from_millis(10)).await,
            },
        }
    }
    drop(listener);
    // Whatever triggered the stop, make sure every connection task sees
    // it so each can run its own graceful shutdown.
    let _ = shutdown.send(true);

    let drain = async { while conns.join_next().await.is_some() {} };
    if timeout(shared.config.shutdown_timeout, drain)
        .await
        .is_err()
    {
        conns.abort_all();
        while conns.join_next().await.is_some() {}
    }
    tasks.shutdown().await;
}

/// Check the connection limit and spawn the connection task.
fn accept_conn(
    stream: TcpStream,
    peer: SocketAddr,
    shared: &Arc<Shared>,
    rx: watch::Receiver<bool>,
    conns: &mut JoinSet<()>,
) {
    let held = shared.connections.fetch_add(1, Ordering::Relaxed) + 1;
    if held > shared.config.max_connections {
        shared.connections.fetch_sub(1, Ordering::Relaxed);
        drop(stream); // refused per DESIGN §14.3: no response is owed
        return;
    }
    conns.spawn(run_conn(stream, peer, Arc::clone(shared), rx));
}

/// Serve one connection: hyper HTTP/1.1 with keep-alive and pipelining,
/// wrapped in the phase-aware read timeouts.
async fn run_conn(
    stream: TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let _guard = ConnGuard::new(Arc::clone(&shared));
    let _ = stream.set_nodelay(true);

    let phase: SharedPhase = Arc::new(Mutex::new(Phase::Head {
        until: Instant::now() + shared.config.header_read_timeout,
    }));
    let io = TokioIo::new(TimeoutIo::new(
        stream,
        Arc::clone(&phase),
        IoTimeouts {
            header_read: shared.config.header_read_timeout,
            keep_alive_idle: shared.config.keep_alive_idle_timeout,
        },
        Arc::clone(&shared.date),
    ));
    let ctx = ConnCtx {
        dispatcher: Arc::clone(&shared.dispatcher),
        config: Arc::clone(&shared.config),
        date: Arc::clone(&shared.date),
        phase,
        remote_addr: peer,
    };
    let service = service_fn(move |req| {
        let ctx = ConnCtx {
            dispatcher: Arc::clone(&ctx.dispatcher),
            config: Arc::clone(&ctx.config),
            date: Arc::clone(&ctx.date),
            phase: Arc::clone(&ctx.phase),
            remote_addr: ctx.remote_addr,
        };
        handle(ctx, req)
    });

    let mut builder = http1::Builder::new();
    builder
        .keep_alive(true)
        // `pipeline_flush` is marked experimental and drops hyper's
        // automatic error responses (observed: >100 headers closed the
        // connection without the promised 431), so it stays off.
        .pipeline_flush(false)
        .timer(TokioTimer::new())
        .max_buf_size(shared.config.head_buffer_size())
        // hyper's parser caps header fields at 100 by default and answers
        // 431 itself; point the cap at the configured limit so raised or
        // lowered limits get the same real response.
        .max_headers(shared.config.max_header_count)
        // Backstop only: TimeoutIo already enforces the real deadlines;
        // this bounds even a buggy phase machine.
        .header_read_timeout(
            shared
                .config
                .header_read_timeout
                .saturating_add(shared.config.keep_alive_idle_timeout),
        );
    let mut conn = Box::pin(builder.serve_connection(io, service));

    tokio::select! {
        _ = &mut conn => {}
        _ = shutdown.changed() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

/// Decrement the live-connection count when a connection task ends.
struct ConnGuard {
    shared: Arc<Shared>,
}

impl ConnGuard {
    fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.shared.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Refresh the cached `Date` header once a second until shutdown.
async fn refresh_dates(date: Arc<DateCache>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => date.refresh(),
            _ = shutdown.changed() => break,
        }
    }
}

/// Forward `SIGINT`/`SIGTERM` to the shutdown channel.
async fn watch_signals(shutdown: watch::Sender<bool>, mut rx: watch::Receiver<bool>) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                    _ = rx.changed() => {}
                }
            }
            Err(_) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = rx.changed() => {}
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = ctrl_c => {}
            _ = rx.changed() => {}
        }
    }
    let _ = shutdown.send(true);
}
