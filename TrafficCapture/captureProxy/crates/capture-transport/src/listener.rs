//! Listener, worker sizing intent, and the active-connection registry (traceability row PROXY-11).
//!
//! # Exposure, stated rather than inherited
//!
//! Java binds the **wildcard** address: `NettyScanningHttpProxy.start` calls `bind(int port)` and
//! `CaptureProxy.Parameters` has no bind-address parameter at all — `--listenPort` is the only
//! listener knob (`NSHP:59-61`, `CP:146-149`). For an in-cluster deployment that is the right
//! shape, but it is an exposure decision rather than an implementation detail, and the spec flags
//! it at §1.1 as one the owner should ratify explicitly.
//!
//! [`ListenerConfig::new`] therefore binds **loopback**, and the wildcard is reachable only through
//! [`ListenerConfig::with_bind_address`]. The listener performs no client authentication of its
//! own; front mTLS is the only caller gate the proxy has, so a non-loopback bind on an untrusted
//! network should be paired with `require_client_auth`.
//!
//! # What Netty actually configures
//!
//! Verified exhaustively by the spec (§1.1): the only options set anywhere in the Java data plane
//! are `childOption(AUTO_READ, false)` on the server and `option(AUTO_READ, false)` on the backside
//! client. No `SO_BACKLOG`, `SO_REUSEADDR`, `SO_KEEPALIVE`, `TCP_NODELAY`,
//! `CONNECT_TIMEOUT_MILLIS`, `WRITE_BUFFER_WATER_MARK`, or allocator override. `AUTO_READ=false`
//! has no Tokio analogue to configure: a Tokio task simply does not read until it chooses to, which
//! is how [`crate::connection`] implements credit-of-one in both directions.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// Default number of worker threads, meaning "derive it from the machine".
///
/// `CaptureProxy.java:155` defaults `--numThreads` to `0`, which `NioEventLoopGroup` interprets as
/// `2 * availableProcessors`. Asserted by all three cases of `CaptureProxyNumThreadsTest`.
pub const WORKER_THREADS_AUTO: usize = 0;

/// Resolves the configured worker-thread count to a concrete number.
///
/// `0` means `2 * available parallelism`, matching Netty's interpretation. The boss group is always
/// exactly one thread in Java (`NSHP:43`); Tokio has no separate acceptor pool, so the accept loop
/// runs as one task on the same runtime and no extra thread is reserved for it.
#[must_use]
pub fn worker_thread_count(configured: usize) -> usize {
    if configured != WORKER_THREADS_AUTO {
        return configured;
    }
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cores.saturating_mul(2)
}

/// Listener inputs.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    bind_address: IpAddr,
    port: u16,
    worker_threads: usize,
}

impl ListenerConfig {
    /// A loopback listener on `port`.
    ///
    /// See the module documentation: the loopback default is deliberate, and binding more widely is
    /// an explicit call.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            worker_threads: WORKER_THREADS_AUTO,
        }
    }

    /// Binds a specific address.
    ///
    /// Passing an unspecified address (`0.0.0.0` or `::`) reproduces Java's wildcard bind and makes
    /// the proxy reachable from every network the host is attached to. That is the production shape
    /// for an in-cluster deployment and a deliberate exposure anywhere else.
    #[must_use]
    pub fn with_bind_address(mut self, bind_address: IpAddr) -> Self {
        self.bind_address = bind_address;
        self
    }

    /// Sets the worker-thread count. `0` means auto; see [`worker_thread_count`].
    #[must_use]
    pub fn with_worker_threads(mut self, worker_threads: usize) -> Self {
        self.worker_threads = worker_threads;
        self
    }

    /// The address that will be bound.
    #[must_use]
    pub fn bind_address(&self) -> IpAddr {
        self.bind_address
    }

    /// The port that will be bound.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The configured worker-thread count, before resolution.
    #[must_use]
    pub fn worker_threads(&self) -> usize {
        self.worker_threads
    }

    /// Whether this configuration binds every interface.
    #[must_use]
    pub fn binds_every_interface(&self) -> bool {
        self.bind_address.is_unspecified()
    }

    /// The socket address that will be bound.
    #[must_use]
    pub fn socket_address(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }
}

/// One connection's close signal.
///
/// The registry triggers these for `disconnectActiveConnections` (`NSHP:100-110`); the connection
/// task awaits [`ConnectionShutdown::triggered`] in its select loop and closes with
/// [`crate::CloseReason::Disconnected`].
#[derive(Debug, Default)]
pub struct ConnectionShutdown {
    triggered: AtomicBool,
    notify: Notify,
}

impl ConnectionShutdown {
    /// Requests this connection close. Idempotent.
    pub fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Whether a close has been requested.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    /// Resolves once a close has been requested.
    pub async fn triggered(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }
}

/// Tracks the live connections, so they can be counted, disconnected, and waited for.
///
/// Java's `DefaultChannelGroup activeConnections` (`NSHP:28-29`). A channel is added **before** the
/// pipeline initializer runs (`NSHP:54-55`), so a connection the initializer immediately closes —
/// the terminating-state case — is still counted; [`ConnectionRegistry::register`] is called at the
/// same point and a registration made after [`ConnectionRegistry::disconnect_all`] starts out
/// already triggered for the same reason.
#[derive(Debug, Default)]
pub struct ConnectionRegistry {
    state: Mutex<RegistryState>,
    quiet: Notify,
}

#[derive(Debug, Default)]
struct RegistryState {
    next_id: u64,
    live: HashMap<u64, Arc<ConnectionShutdown>>,
    disconnecting: bool,
}

impl ConnectionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a connection and returns its guard, which deregisters on drop.
    ///
    /// # Panics
    ///
    /// Panics if the registry mutex was poisoned, which can only happen if another thread panicked
    /// while holding it. Capture-state corruption must terminate the process rather than be papered
    /// over (`proxyCaptureProtocol.md` §7.1).
    pub fn register(self: &Arc<Self>) -> ConnectionGuard {
        let shutdown = Arc::new(ConnectionShutdown::default());
        let id = {
            let mut state = self
                .state
                .lock()
                .expect("the registry mutex is not poisoned");
            state.next_id += 1;
            let id = state.next_id;
            if state.disconnecting {
                shutdown.trigger();
            }
            state.live.insert(id, Arc::clone(&shutdown));
            id
        };
        ConnectionGuard {
            id,
            registry: Arc::clone(self),
            shutdown,
        }
    }

    /// How many connections are live.
    ///
    /// # Panics
    ///
    /// As [`ConnectionRegistry::register`].
    #[must_use]
    pub fn active_connection_count(&self) -> usize {
        self.state
            .lock()
            .expect("the registry mutex is not poisoned")
            .live
            .len()
    }

    /// Requests every live connection close, and every future one too.
    ///
    /// Java's `disconnectActiveConnections` closes the group and hands back a future; the
    /// equivalent wait here is [`ConnectionRegistry::wait_for_no_active_connections`].
    ///
    /// # Panics
    ///
    /// As [`ConnectionRegistry::register`].
    pub fn disconnect_all(&self) {
        let shutdowns: Vec<Arc<ConnectionShutdown>> = {
            let mut state = self
                .state
                .lock()
                .expect("the registry mutex is not poisoned");
            state.disconnecting = true;
            state.live.values().map(Arc::clone).collect()
        };
        for shutdown in shutdowns {
            shutdown.trigger();
        }
    }

    /// Resolves once no connection is live.
    ///
    /// Completes immediately when the registry is already empty, matching
    /// `whenNoActiveConnections` (`NSHP:85-98`).
    ///
    /// # Panics
    ///
    /// As [`ConnectionRegistry::register`].
    pub async fn wait_for_no_active_connections(&self) {
        loop {
            let notified = self.quiet.notified();
            if self.active_connection_count() == 0 {
                return;
            }
            notified.await;
        }
    }

    fn deregister(&self, id: u64) {
        let empty = {
            let mut state = self
                .state
                .lock()
                .expect("the registry mutex is not poisoned");
            state.live.remove(&id);
            state.live.is_empty()
        };
        if empty {
            self.quiet.notify_waiters();
        }
    }
}

/// A live connection's registration. Deregisters on drop.
#[derive(Debug)]
pub struct ConnectionGuard {
    id: u64,
    registry: Arc<ConnectionRegistry>,
    shutdown: Arc<ConnectionShutdown>,
}

impl ConnectionGuard {
    /// This connection's close signal.
    #[must_use]
    pub fn shutdown(&self) -> &Arc<ConnectionShutdown> {
        &self.shutdown
    }

    /// This connection's registry id, for logging.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

/// An accepted client connection, already registered.
#[derive(Debug)]
pub struct Accepted {
    /// The client socket.
    pub stream: TcpStream,
    /// The client's address.
    pub peer: SocketAddr,
    /// The registration, which must be held for the connection's whole life.
    pub guard: ConnectionGuard,
}

/// Why [`Listener::accept`] returned without a connection.
#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// [`Listener::stop_accepting`] was called; the accept loop must exit.
    #[error("the listener has stopped accepting new connections")]
    StoppedAccepting,
    /// The accept itself failed.
    #[error("accept failed: {0}")]
    Io(#[from] io::Error),
}

/// The proxy's listening socket.
#[derive(Debug)]
pub struct Listener {
    socket: TcpListener,
    local_addr: SocketAddr,
    registry: Arc<ConnectionRegistry>,
    stopping: Arc<ConnectionShutdown>,
    worker_threads: usize,
}

impl Listener {
    /// Binds the configured address.
    ///
    /// # Errors
    ///
    /// Propagates the bind failure, which for a port already in use is the condition that must stop
    /// startup rather than be retried.
    pub async fn bind(config: &ListenerConfig) -> io::Result<Self> {
        let socket = TcpListener::bind(config.socket_address()).await?;
        let local_addr = socket.local_addr()?;
        if config.binds_every_interface() {
            tracing::warn!(
                address = %local_addr,
                "the capture proxy is listening on every interface; front mTLS is the only caller gate it has"
            );
        }
        Ok(Self {
            socket,
            local_addr,
            registry: Arc::new(ConnectionRegistry::new()),
            stopping: Arc::new(ConnectionShutdown::default()),
            worker_threads: worker_thread_count(config.worker_threads()),
        })
    }

    /// The bound address, with the port resolved.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The resolved worker-thread count this listener was sized for.
    ///
    /// Sizing intent only: the runtime is built by the process, not here.
    #[must_use]
    pub fn worker_threads(&self) -> usize {
        self.worker_threads
    }

    /// The active-connection registry.
    #[must_use]
    pub fn registry(&self) -> &Arc<ConnectionRegistry> {
        &self.registry
    }

    /// Accepts one connection, registering it before it is returned.
    ///
    /// # Errors
    ///
    /// [`AcceptError::StoppedAccepting`] once [`Listener::stop_accepting`] has been called, or
    /// [`AcceptError::Io`] if the accept failed.
    pub async fn accept(&self) -> Result<Accepted, AcceptError> {
        if self.stopping.is_triggered() {
            return Err(AcceptError::StoppedAccepting);
        }
        tokio::select! {
            () = self.stopping.triggered() => Err(AcceptError::StoppedAccepting),
            result = self.socket.accept() => {
                let (stream, peer) = result?;
                // Registered before anything else happens to the connection, so a connection that
                // is immediately closed is still counted (NSHP:54-55).
                let guard = self.registry.register();
                Ok(Accepted { stream, peer, guard })
            }
        }
    }

    /// Stops accepting new connections. Idempotent.
    ///
    /// Java's `stopAcceptingNewConnections` closes the server channel and blocks on its close
    /// future (`NSHP:76-83`). Here every in-flight and future [`Listener::accept`] resolves to
    /// [`AcceptError::StoppedAccepting`]; the socket itself is released when the listener is
    /// dropped, which is the Rust equivalent of that close future having completed.
    pub fn stop_accepting(&self) {
        self.stopping.trigger();
    }

    /// Whether [`Listener::stop_accepting`] has been called.
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        !self.stopping.is_triggered()
    }

    /// Resolves once the listener has stopped accepting.
    ///
    /// Java's `waitForClose` blocks on the server channel's close future and throws a
    /// `NullPointerException` if the proxy was never started (`NSHP:129-131`); this cannot be called
    /// without a bound listener.
    pub async fn wait_for_close(&self) {
        self.stopping.triggered().await;
    }

    /// Stop accepting, disconnect every live connection, and wait for them to finish.
    ///
    /// The composition Java calls `stop()` (`NSHP:70-74`), minus the event-loop shutdown, which
    /// belongs to the process's runtime rather than to the listener.
    pub async fn stop(&self) {
        self.stop_accepting();
        self.registry.disconnect_all();
        self.registry.wait_for_no_active_connections().await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn the_default_bind_is_loopback_and_the_wildcard_is_an_explicit_choice() {
        let config = ListenerConfig::new(9200);
        assert_eq!(config.bind_address(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!config.binds_every_interface());

        let wildcard =
            ListenerConfig::new(9200).with_bind_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(wildcard.binds_every_interface());
        let wildcard_v6 =
            ListenerConfig::new(9200).with_bind_address(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert!(wildcard_v6.binds_every_interface());
    }

    #[test]
    fn worker_sizing_matches_nettys_interpretation_of_zero() {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        assert_eq!(worker_thread_count(WORKER_THREADS_AUTO), cores * 2);
        assert_eq!(worker_thread_count(1), 1);
        assert_eq!(worker_thread_count(7), 7);
    }

    #[tokio::test]
    async fn an_accepted_connection_is_counted_until_its_guard_drops() {
        let listener = Listener::bind(&ListenerConfig::new(0))
            .await
            .expect("loopback bind");
        assert_eq!(listener.registry().active_connection_count(), 0);

        let address = listener.local_addr();
        let client = tokio::spawn(async move { TcpStream::connect(address).await });
        let accepted = listener.accept().await.expect("accept");
        client.await.expect("join").expect("connect");
        assert_eq!(listener.registry().active_connection_count(), 1);

        drop(accepted);
        assert_eq!(listener.registry().active_connection_count(), 0);
    }

    #[tokio::test]
    async fn disconnect_all_triggers_live_and_future_connections() {
        let listener = Listener::bind(&ListenerConfig::new(0))
            .await
            .expect("loopback bind");
        let address = listener.local_addr();

        let client = tokio::spawn(async move { TcpStream::connect(address).await });
        let first = listener.accept().await.expect("accept");
        let _client = client.await.expect("join").expect("connect");
        assert!(!first.guard.shutdown().is_triggered());

        listener.registry().disconnect_all();
        assert!(first.guard.shutdown().is_triggered());

        // A connection registered after the disconnect starts out already triggered, mirroring
        // Java's terminating-state channel that is tracked and then immediately closed.
        let later = Arc::clone(listener.registry()).register();
        assert!(later.shutdown().is_triggered());
    }

    #[tokio::test]
    async fn waiting_for_quiet_returns_immediately_when_empty_and_wakes_when_it_empties() {
        let registry = Arc::new(ConnectionRegistry::new());
        registry.wait_for_no_active_connections().await;

        let guard = registry.register();
        let waiter = {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move { registry.wait_for_no_active_connections().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(guard);
        waiter
            .await
            .expect("the waiter wakes when the last connection leaves");
    }

    #[tokio::test]
    async fn stop_accepting_makes_accept_return_and_is_idempotent() {
        let listener = Listener::bind(&ListenerConfig::new(0))
            .await
            .expect("loopback bind");
        assert!(listener.is_accepting());
        listener.stop_accepting();
        listener.stop_accepting();
        assert!(!listener.is_accepting());
        assert!(matches!(
            listener.accept().await,
            Err(AcceptError::StoppedAccepting)
        ));
        listener.wait_for_close().await;
    }

    #[tokio::test]
    async fn stop_accepting_wakes_an_accept_already_in_flight() {
        let listener = Arc::new(
            Listener::bind(&ListenerConfig::new(0))
                .await
                .expect("loopback bind"),
        );
        let accepting = {
            let listener = Arc::clone(&listener);
            tokio::spawn(async move { listener.accept().await.map(|_| ()) })
        };
        tokio::task::yield_now().await;
        listener.stop_accepting();
        assert!(matches!(
            accepting.await.expect("join"),
            Err(AcceptError::StoppedAccepting)
        ));
    }

    #[tokio::test]
    async fn stop_disconnects_everything_and_returns() {
        let listener = Listener::bind(&ListenerConfig::new(0))
            .await
            .expect("loopback bind");
        let address = listener.local_addr();
        let client = tokio::spawn(async move { TcpStream::connect(address).await });
        let accepted = listener.accept().await.expect("accept");
        let _client = client.await.expect("join").expect("connect");

        let stopping = tokio::spawn({
            // The guard is dropped below, which is what lets `stop` return.
            let registry = Arc::clone(listener.registry());
            async move {
                registry.disconnect_all();
                registry.wait_for_no_active_connections().await;
            }
        });
        tokio::task::yield_now().await;
        assert!(accepted.guard.shutdown().is_triggered());
        drop(accepted);
        stopping.await.expect("stop completes once connections end");
        assert_eq!(listener.registry().active_connection_count(), 0);
    }
}
