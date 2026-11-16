//! The warm pre-connect pool (traceability rows PROXY-05, PROXY-07).
//!
//! Ports spec §10: [`PreconnectPool`] is Java's `BacksideConnectionPool` (`BCP:32-50`) and
//! [`ExpiringSlotPool`] is its `ExpiringSubstitutableItemPool` (`ESIP:31-344`). The module is
//! deliberately generic over [`SlotFactory`] rather than tied to a socket, so the pool's queueing,
//! expiry and retry rules can be tested without a network.
//!
//! # What the pool is and is not
//!
//! It is a **pre-connect cache**, not a reuse pool. There is no `release`: an acquired item is
//! gone, and the slot it vacated is refilled by creating a *new* item. `ESIP:243` polls the ready
//! queue and the Java class has no return path at all. Keeping that shape matters for parity,
//! because a destination connection that has already carried one client's request must never be
//! handed to another client.
//!
//! # Disabled by default
//!
//! `--destinationConnectionPoolSize` defaults to **0** (`CP:162`), which disables the pool
//! entirely: `BCP:46-48` short-circuits to a fresh connect for every client connection. See
//! [`SlotPoolConfig`] and [`PreconnectPool::is_enabled`].
//!
//! # Two deliberate divergences from Java
//!
//! Both are marked `// DEVIATION:` at their implementation sites:
//!
//! 1. **One shared pool instead of one per event loop.** Java holds the pool in a
//!    `FastThreadLocal` (`BCP:28`, `ESIP:26-29`) precisely so it needs no locks, which makes the
//!    real warm count `poolSize * numThreads` rather than `poolSize` (`CP:159-161` documents the
//!    multiplication). This module keeps one pool behind a `std::sync::Mutex`, so the configured
//!    size is the actual warm count.
//! 2. **Backoff on a failed warm creation.** `ESIP:200-204` retries immediately and carries an
//!    in-source `TODO - add some backoff here`; spec §13-D12 requires Rust to add it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rand::Rng as _;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// `--destinationConnectionPoolSize` default: the pool is **disabled** (`CP:162`).
pub const DEFAULT_POOL_SIZE: usize = 0;

/// `--destinationConnectionPoolTimeout` default: `"PT30S"` (`CP:163-169`).
///
/// `CP:602-604` forces `Duration.ZERO` instead whenever the size is 0, and a zero timeout means a
/// **closed** pool (`ESIP:238-240`). [`SlotPoolConfig::default`] reproduces that pairing.
pub const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

/// Default first retry delay after a failed warm creation.
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(100);

/// Default ceiling on the retry delay after repeated failed warm creations.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Default jitter fraction: each delay is reduced by up to half.
pub const DEFAULT_BACKOFF_JITTER_FRACTION: f64 = 0.5;

/// A [`SlotFactory`] failed to create an item.
///
/// Carries a rendered message rather than a typed cause because the pool only ever logs it or
/// hands it to a caller as [`PoolError::Creation`]; it never branches on it. Java does not even do
/// that much — `ESIP:195-205` drops the cause with the comment "the calling context should track
/// failures too - no reason to log".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PoolFactoryError(pub String);

impl PoolFactoryError {
    /// Builds a factory error from anything string-like.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Acquiring an item from the pool failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// The pool is closed: either the inactivity timeout is zero (`ESIP:238-240`) or
    /// [`ExpiringSlotPool::shutdown`] has run.
    ///
    /// Java throws `PoolClosedException` here.
    #[error("the pre-connect pool is closed")]
    Closed,
    /// Creating an item failed, and no warm item was available to serve the caller instead.
    #[error("creating a pooled item failed: {0}")]
    Creation(String),
}

/// Creates and destroys the items a pool holds.
///
/// For the capture proxy an item is a connected — and, over `https`, fully handshaked —
/// destination socket, matching `BCP:86-120`, where the promise handed to the pool completes only
/// after the back-side TLS handshake (`BCP:105-111`).
pub trait SlotFactory: Send + Sync + 'static {
    /// What the factory produces.
    type Item: Send + 'static;

    /// Creates one item.
    ///
    /// Called from a spawned warm-up/refill task, and inline from a cold acquire that found
    /// nothing ready and nothing in flight (`ESIP:269`).
    fn create(&self) -> impl Future<Output = Result<Self::Item, PoolFactoryError>> + Send;

    /// Destroys an item the pool is giving up on.
    ///
    /// Called for an expired item — Java's `onExpirationConsumer`, which
    /// `BCP:59` sets to `x -> x.channel().close()` — for every ready item at shutdown, and for an
    /// item whose cold acquirer disappeared before it finished. Must not block for long: it runs
    /// on whichever task noticed, and the pool lock is **not** held.
    fn discard(&self, item: Self::Item);
}

/// Exponential-backoff parameters for retrying a failed warm creation.
///
/// This whole type is the §13-D12 addition; Java has no equivalent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffConfig {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Ceiling on the delay, before jitter.
    pub max: Duration,
    /// Fraction of the delay that jitter may remove, clamped to `0.0..=1.0`.
    ///
    /// Jitter only ever *reduces* a delay, so [`BackoffConfig::max`] is a true ceiling.
    pub jitter_fraction: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: DEFAULT_BACKOFF_INITIAL,
            max: DEFAULT_BACKOFF_MAX,
            jitter_fraction: DEFAULT_BACKOFF_JITTER_FRACTION,
        }
    }
}

impl BackoffConfig {
    /// The delay to wait before the `consecutive_failures`-th retry.
    ///
    /// `consecutive_failures` counts from 1, so the first retry waits
    /// [`BackoffConfig::initial`]. The delay doubles per failure, saturates at
    /// [`BackoffConfig::max`], and is then reduced by a random fraction of itself.
    fn delay_after(&self, consecutive_failures: u32) -> Duration {
        let doublings = consecutive_failures.saturating_sub(1).min(31);
        let base = self
            .initial
            .saturating_mul(2_u32.saturating_pow(doublings))
            .min(self.max);

        let fraction = self.jitter_fraction.clamp(0.0, 1.0);
        if fraction <= 0.0 {
            return base;
        }
        let keep = 1.0 - fraction * rand::rng().random::<f64>();
        Duration::try_from_secs_f64(base.as_secs_f64() * keep).unwrap_or(base)
    }
}

/// How many warm items to hold, for how long, and how to back off after a failure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotPoolConfig {
    /// How many warm items to hold.
    ///
    /// **0 means disabled.** [`PreconnectPool`] then creates an item per acquire and holds none;
    /// see [`PreconnectPool::new`].
    pub size: usize,
    /// How long after becoming ready an item is discarded and replaced.
    ///
    /// **`Duration::ZERO` means closed**, matching `ESIP:238-240`: `getAvailableOrNewItem` and
    /// `beginLoadingNewItemIfNecessary` both throw `PoolClosedException` when the timeout is zero.
    /// That is reachable only because `CP:602-604` forces zero when the size is 0, a combination
    /// `BCP:46-48` short-circuits before ever touching the pool.
    pub inactivity_timeout: Duration,
    /// Retry policy for a failed warm creation; see [`BackoffConfig`].
    pub backoff: BackoffConfig,
}

impl Default for SlotPoolConfig {
    /// The Java defaults: size 0 (`CP:162`) paired with a zero timeout (`CP:602-604`).
    fn default() -> Self {
        Self {
            size: DEFAULT_POOL_SIZE,
            inactivity_timeout: Duration::ZERO,
            backoff: BackoffConfig::default(),
        }
    }
}

/// A snapshot of one pool's counters and queue depths.
///
/// Java's `ESIP.Stats` (`ESIP:48-130`) also tracks total build time and total caller wait time,
/// which it uses only for a 30-second INFO log line (`BCP:63-65`, `BCP:72-84`); those are latency
/// metrics and belong to the observability layer rather than here.
///
/// `created` counts **successful** creations. Java's `nItemsCreated` increments from a listener
/// that fires on failure too (`ESIP:187-191`), so Java's count conflates the two; this type keeps
/// `created` and `failed_creations` separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotPoolStats {
    /// Items created successfully, including inline cold creations.
    pub created: u64,
    /// Ready items discarded because they exceeded the inactivity timeout.
    pub expired: u64,
    /// Acquires served from the ready queue — Java's `nHotGets`.
    pub hot_acquires: u64,
    /// Acquires that found nothing ready — Java's `nColdGets`.
    pub cold_acquires: u64,
    /// Creation attempts that returned an error.
    pub failed_creations: u64,
    /// Items warm and ready right now.
    pub ready: usize,
    /// Creations in flight for the pool right now.
    ///
    /// Excludes a creation that a cold acquire has taken over: Java likewise removes it from
    /// `inProgressItems` at takeover (`ESIP:260-263`), because it is no longer the pool's.
    pub in_progress: usize,
}

/// One warm item plus the moment it became usable.
struct ReadySlot<I> {
    item: I,
    /// When the item's creation **succeeded**, not when it was requested.
    ///
    /// `ESIP:37-39` sets `Entry.timestamp = Instant.now()` in the constructor, and `ESIP:198`
    /// constructs the entry inside the success branch of the completion listener.
    ready_at: Instant,
}

/// Counters, kept with the queues so a snapshot is internally consistent.
#[derive(Default)]
struct Counters {
    created: u64,
    expired: u64,
    hot_acquires: u64,
    cold_acquires: u64,
    failed_creations: u64,
}

/// Everything the pool lock protects.
struct State<I> {
    /// Ready items, oldest first. Kept chronological; see the `debug_assert` in `finish_success`.
    ready: VecDeque<ReadySlot<I>>,
    /// Creation ids in flight, oldest first, so a cold acquire can take the oldest (`ESIP:260`).
    in_progress: VecDeque<u64>,
    /// Creations a cold acquire has taken over, keyed by creation id.
    claimed: HashMap<u64, oneshot::Sender<Result<I, PoolFactoryError>>>,
    counters: Counters,
    consecutive_failures: u32,
    closed: bool,
}

/// The pool's shared interior, held by the handle and by every spawned task.
struct Shared<F: SlotFactory> {
    factory: Arc<F>,
    config: SlotPoolConfig,
    /// DEVIATION: Java needs no lock because `BCP:28` keeps one pool per Netty event loop in a
    /// `FastThreadLocal` (`ESIP:26-29` spells the assumption out). The cost of that design is that
    /// the configured size is not the warm count: `CP:159-161` documents the real figure as
    /// `poolSize * numThreads`, so an operator asking for 2 warm connections on a 32-core host
    /// gets 128 of them, and a hard-down destination gets 128 independent retry loops. This
    /// implementation keeps **one** pool guarded by a `std::sync::Mutex`, held only across queue
    /// manipulation and never across an `await`, so `size` is exactly the number of warm items.
    state: Mutex<State<F::Item>>,
    /// Creation-id source. Outside the lock so an id can be minted without one.
    next_id: AtomicU64,
}

impl<F: SlotFactory> Shared<F> {
    /// Takes the pool lock, recovering from poisoning rather than propagating a panic.
    ///
    /// The release profile sets `panic = "abort"`, so a poisoned lock is unreachable there; in a
    /// debug or test build, recovering keeps the failure attributable to the original panic
    /// instead of burying it under a second one.
    fn lock(&self) -> MutexGuard<'_, State<F::Item>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts one creation if the pool is below capacity.
    ///
    /// `ESIP:312`'s guard exactly: build one item iff `poolSize > inProgressItems.size() +
    /// readyItems.size()`. Every refill site in Java funnels through this, which is what makes
    /// "refill immediately on consumption, on expiry, and after a failure" safe to state as three
    /// separate rules.
    fn refill_locked(self: &Arc<Self>, state: &mut State<F::Item>) {
        if state.closed || self.config.size <= state.ready.len() + state.in_progress.len() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.in_progress.push_back(id);
        let shared = Arc::clone(self);
        drop(tokio::spawn(async move {
            let outcome = shared.factory.create().await;
            match outcome {
                Ok(item) => shared.finish_success(id, item),
                Err(error) => shared.finish_failure(id, error),
            }
        }));
    }

    /// Completes a successful warm creation.
    fn finish_success(self: &Arc<Self>, id: u64, item: F::Item) {
        let mut state = self.lock();
        state.counters.created += 1;
        state.consecutive_failures = 0;

        if let Some(claimer) = state.claimed.remove(&id) {
            drop(state);
            if let Err(Ok(unwanted)) = claimer.send(Ok(item)) {
                // The cold acquirer gave up; the item would otherwise leak.
                self.factory.discard(unwanted);
            }
            return;
        }

        retain_other_ids(&mut state.in_progress, id);
        if state.closed {
            drop(state);
            self.factory.discard(item);
            return;
        }

        let ready_at = Instant::now();
        debug_assert!(
            state
                .ready
                .back()
                .is_none_or(|newest| newest.ready_at <= ready_at),
            "ESIP:298-301: ready items must stay in chronological order, so a fixed timeout can \
             never let a younger item cut in front of an older one"
        );
        state.ready.push_back(ReadySlot { item, ready_at });
        drop(state);

        // ESIP:199 schedules a sweep one inactivity timeout after each item becomes ready.
        self.schedule_sweep(self.config.inactivity_timeout);
    }

    /// Completes a failed warm creation.
    fn finish_failure(self: &Arc<Self>, id: u64, error: PoolFactoryError) {
        let mut state = self.lock();
        state.counters.failed_creations += 1;

        if let Some(claimer) = state.claimed.remove(&id) {
            drop(state);
            // A taken-over creation retries nothing: `ESIP:261-262` strips the
            // `shuffleInProgressToReady` listener, and the slot was already refilled at takeover.
            drop(claimer.send(Err(error)));
            return;
        }

        retain_other_ids(&mut state.in_progress, id);
        if state.closed {
            return;
        }
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let delay = self.config.backoff.delay_after(state.consecutive_failures);
        drop(state);

        // DEVIATION: `ESIP:200-204` calls `beginLoadingNewItemIfNecessary()` straight from the
        // failure branch, with an in-source `TODO - add some backoff here`. A destination that is
        // hard down therefore spins a connect loop per warm slot per event loop, which is
        // `poolSize * numThreads` tight loops (spec §13-D12). The only reason that is not an
        // incident today is that the default size is 0. Rust waits an exponentially growing,
        // jittered delay instead, reset by the next success.
        let shared = Arc::clone(self);
        drop(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let mut state = shared.lock();
            shared.refill_locked(&mut state);
        }));
    }

    /// Schedules an expiry sweep.
    fn schedule_sweep(self: &Arc<Self>, after: Duration) {
        let shared = Arc::clone(self);
        drop(tokio::spawn(async move {
            tokio::time::sleep(after).await;
            shared.expire_items();
        }));
    }

    /// Discards every ready item older than the inactivity timeout, then refills.
    ///
    /// `ESIP:285-307`: pop from the front while the oldest is past the threshold, hand each to the
    /// expiration consumer, refill, and reschedule for the gap to the new oldest item.
    fn expire_items(self: &Arc<Self>) {
        let mut discarded = Vec::new();
        let next_sweep;
        {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let now = Instant::now();
            let mut gap = None;
            while let Some(oldest) = state.ready.front() {
                let age = now.saturating_duration_since(oldest.ready_at);
                if age < self.config.inactivity_timeout {
                    // Reschedule for the gap to the oldest survivor, as `ESIP:290-293` does.
                    // `age` is strictly below the timeout here, so this is always `Some`.
                    gap = self.config.inactivity_timeout.checked_sub(age);
                    break;
                }
                let Some(expired) = state.ready.pop_front() else {
                    break;
                };
                state.counters.expired += 1;
                discarded.push(expired.item);
            }
            // ESIP:304 refills once per expired item.
            for _ in 0..discarded.len() {
                self.refill_locked(&mut state);
            }
            next_sweep = gap;
        }

        for item in discarded {
            self.factory.discard(item);
        }
        if let Some(after) = next_sweep {
            self.schedule_sweep(after);
        }
    }

    /// Acquires one item.
    ///
    /// `ESIP:236-270` in order: closed check, hot path, cold takeover, inline creation.
    async fn acquire(self: &Arc<Self>) -> Result<F::Item, PoolError> {
        let claim = {
            let mut state = self.lock();
            if state.closed {
                return Err(PoolError::Closed);
            }
            if let Some(slot) = state.ready.pop_front() {
                state.counters.hot_acquires += 1;
                // ESIP:248 — consume-without-return, so the vacated slot is refilled at once.
                self.refill_locked(&mut state);
                return Ok(slot.item);
            }
            state.counters.cold_acquires += 1;
            if let Some(oldest) = state.in_progress.pop_front() {
                // ESIP:260-268 — take over the oldest creation in flight. This never blocks on a
                // warm slot being available and never fails for the lack of one.
                let (sender, receiver) = oneshot::channel();
                state.claimed.insert(oldest, sender);
                self.refill_locked(&mut state);
                Some(receiver)
            } else {
                None
            }
        };

        match claim {
            Some(receiver) => match receiver.await {
                Ok(Ok(item)) => Ok(item),
                Ok(Err(error)) => Err(PoolError::Creation(error.0)),
                // The sender was dropped, which only happens at shutdown.
                Err(_) => Err(PoolError::Closed),
            },
            // ESIP:269 — nothing ready and nothing in flight: build inline. Java does not refill
            // on this path, and neither does this.
            None => match self.factory.create().await {
                Ok(item) => {
                    self.lock().counters.created += 1;
                    Ok(item)
                }
                Err(error) => {
                    self.lock().counters.failed_creations += 1;
                    Err(PoolError::Creation(error.0))
                }
            },
        }
    }

    /// Closes the pool and discards every ready item. Idempotent.
    fn shutdown(&self) {
        let discarded = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            // Waiting cold acquirers see their sender drop and report `PoolError::Closed`.
            state.claimed.clear();
            state
                .ready
                .drain(..)
                .map(|slot| slot.item)
                .collect::<Vec<_>>()
        };
        for item in discarded {
            self.factory.discard(item);
        }
    }

    fn stats(&self) -> SlotPoolStats {
        let state = self.lock();
        SlotPoolStats {
            created: state.counters.created,
            expired: state.counters.expired,
            hot_acquires: state.counters.hot_acquires,
            cold_acquires: state.counters.cold_acquires,
            failed_creations: state.counters.failed_creations,
            ready: state.ready.len(),
            in_progress: state.in_progress.len(),
        }
    }
}

/// Removes one creation id from the in-flight queue, preserving the order of the rest.
fn retain_other_ids(in_progress: &mut VecDeque<u64>, id: u64) {
    in_progress.retain(|queued| *queued != id);
}

/// A pool of warm items that expire after a period of inactivity.
///
/// Java's `ExpiringSubstitutableItemPool` (`ESIP:31-344`). Warm-up starts in
/// [`ExpiringSlotPool::new`]; there is no separate `start`.
pub struct ExpiringSlotPool<F: SlotFactory> {
    shared: Arc<Shared<F>>,
}

impl<F: SlotFactory> ExpiringSlotPool<F> {
    /// Builds the pool and starts warming it up to `config.size`.
    ///
    /// A zero `config.inactivity_timeout` builds a **closed** pool that warms nothing and fails
    /// every acquire, matching `ESIP:238-240`.
    ///
    /// Java warms up by scheduling `numItemsToLoad + 2` zero-gap `beginLoadingNewItemIfNecessary`
    /// calls on the event loop (`ESIP:272-279`, `BCP:55-62`); the two extra calls are no-ops
    /// because of the capacity guard. This issues exactly `config.size` capacity-guarded refills,
    /// which has the same effect without the off-by-two.
    ///
    /// # Panics
    ///
    /// Requires a Tokio runtime: warm-up and refill use [`tokio::spawn`], which panics when there
    /// is no reactor. The equivalent Java constraint is that `BCP:52-70` only ever builds a pool
    /// from inside an event loop.
    #[must_use]
    pub fn new(factory: Arc<F>, config: SlotPoolConfig) -> Self {
        let size = config.size;
        let closed = config.inactivity_timeout.is_zero();
        let shared = Arc::new(Shared {
            factory,
            config,
            state: Mutex::new(State {
                ready: VecDeque::new(),
                in_progress: VecDeque::new(),
                claimed: HashMap::new(),
                counters: Counters::default(),
                consecutive_failures: 0,
                closed,
            }),
            next_id: AtomicU64::new(0),
        });

        if !closed {
            let mut state = shared.lock();
            for _ in 0..size {
                Shared::refill_locked(&shared, &mut state);
            }
        }

        Self { shared }
    }

    /// Takes one item out of the pool.
    ///
    /// Never blocks waiting for a warm slot: if nothing is ready it takes over the oldest creation
    /// in flight, and if nothing is in flight it creates one inline (`ESIP:260-269`). There is no
    /// matching `release` — see the module documentation.
    ///
    /// # Errors
    ///
    /// [`PoolError::Closed`] when the pool is closed, and [`PoolError::Creation`] when the
    /// creation this caller ended up waiting on — taken over or inline — failed.
    pub async fn acquire(&self) -> Result<F::Item, PoolError> {
        self.shared.acquire().await
    }

    /// A snapshot of the pool's counters and queue depths.
    #[must_use]
    pub fn stats(&self) -> SlotPoolStats {
        self.shared.stats()
    }

    /// Whether the pool refuses every acquire.
    ///
    /// True from construction when the inactivity timeout is zero (`ESIP:238-240`), and from
    /// [`ExpiringSlotPool::shutdown`] onward.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    /// Closes the pool, discarding every ready item through [`SlotFactory::discard`].
    ///
    /// Idempotent. Stops all further refills, including a retry already sleeping out its backoff,
    /// and makes every waiting cold acquirer observe [`PoolError::Closed`]. A creation still in
    /// flight runs to completion — nothing can cancel the factory — but its item is discarded
    /// rather than queued, so no task does useful pool work after this returns.
    pub fn shutdown(&self) {
        self.shared.shutdown();
    }
}

impl<F: SlotFactory> Drop for ExpiringSlotPool<F> {
    /// Dropping the pool closes it, so warm items are released rather than leaked.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The `BacksideConnectionPool` equivalent: a pre-connect pool that a size of 0 switches off.
///
/// `BCP:45-50`:
///
/// ```java
/// public ChannelFuture getOutboundConnectionFuture(EventLoop eventLoop) {
///     if (poolSize == 0) return buildConnectionFuture(eventLoop);              // BCP:46-48
///     return getExpiringWarmChannelPool(eventLoop).getAvailableOrNewItem();    // BCP:49
/// }
/// ```
pub struct PreconnectPool<F: SlotFactory> {
    factory: Arc<F>,
    inner: Option<ExpiringSlotPool<F>>,
}

impl<F: SlotFactory> PreconnectPool<F> {
    /// Builds the pool, or the disabled short-circuit when `config.size` is 0.
    ///
    /// A size of 0 is the default (`CP:162`) and means every acquire creates an item directly.
    /// `CP:602-604` pairs that size with a forced `Duration.ZERO` timeout, which would make the
    /// inner pool closed (`ESIP:238-240`); since `BCP:46-48` never reaches the pool in that case,
    /// no inner pool is built at all and the configured timeout is irrelevant.
    ///
    /// # Panics
    ///
    /// As [`ExpiringSlotPool::new`], when the size is non-zero.
    #[must_use]
    pub fn new(factory: Arc<F>, config: SlotPoolConfig) -> Self {
        let inner = if config.size == 0 {
            None
        } else {
            Some(ExpiringSlotPool::new(Arc::clone(&factory), config))
        };
        Self { factory, inner }
    }

    /// Takes one item: from the warm pool when enabled, otherwise straight from the factory.
    ///
    /// # Errors
    ///
    /// [`PoolError::Creation`] when the creation this caller waited on failed, and
    /// [`PoolError::Closed`] when an enabled pool has been shut down.
    pub async fn acquire(&self) -> Result<F::Item, PoolError> {
        match &self.inner {
            Some(pool) => pool.acquire().await,
            None => self
                .factory
                .create()
                .await
                .map_err(|error| PoolError::Creation(error.0)),
        }
    }

    /// Whether any warm items are held, i.e. whether the configured size was non-zero.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// The inner pool's statistics, or `None` when the pool is disabled.
    ///
    /// A disabled pool has no queues and no counters to report: `BCP:46-48` never constructs one.
    #[must_use]
    pub fn stats(&self) -> Option<SlotPoolStats> {
        self.inner.as_ref().map(ExpiringSlotPool::stats)
    }

    /// Closes the inner pool if there is one. Idempotent, and a no-op when disabled.
    pub fn shutdown(&self) {
        if let Some(pool) = &self.inner {
            pool.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A fake factory that hands out increasing `u64` ids and counts everything it is asked to do.
    #[derive(Debug, Default)]
    struct FakeFactory {
        attempts: AtomicU64,
        discarded: AtomicUsize,
        always_fail: bool,
        delay: Duration,
    }

    impl FakeFactory {
        fn immediate() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn delayed(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                ..Self::default()
            })
        }

        fn always_failing() -> Arc<Self> {
            Arc::new(Self {
                always_fail: true,
                ..Self::default()
            })
        }

        fn attempts(&self) -> u64 {
            self.attempts.load(Ordering::SeqCst)
        }

        fn discarded(&self) -> usize {
            self.discarded.load(Ordering::SeqCst)
        }
    }

    impl SlotFactory for FakeFactory {
        type Item = u64;

        async fn create(&self) -> Result<u64, PoolFactoryError> {
            // Counted before the delay so an in-flight creation is observable.
            let id = self.attempts.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.always_fail {
                Err(PoolFactoryError::new("destination unreachable"))
            } else {
                Ok(id)
            }
        }

        fn discard(&self, _item: u64) {
            self.discarded.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn enabled_config(size: usize, inactivity_timeout: Duration) -> SlotPoolConfig {
        SlotPoolConfig {
            size,
            inactivity_timeout,
            backoff: BackoffConfig {
                // Deterministic: jitter only ever reduces a delay, and zero removes it entirely.
                jitter_fraction: 0.0,
                ..BackoffConfig::default()
            },
        }
    }

    /// Yields until `condition` holds, without advancing the paused clock.
    async fn settle_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..1024 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the pool never reached the expected state");
    }

    #[test]
    fn the_default_configuration_is_a_disabled_pool() {
        let config = SlotPoolConfig::default();
        assert_eq!(config.size, 0, "CP:162 defaults the pool size to 0");
        assert!(
            config.inactivity_timeout.is_zero(),
            "CP:602-604 forces Duration.ZERO whenever the size is 0"
        );
    }

    #[test]
    fn the_documented_pool_timeout_default_is_thirty_seconds() {
        // CP:163-169 spells it "PT30S"; asserted in seconds so the number is pinned.
        assert_eq!(DEFAULT_INACTIVITY_TIMEOUT.as_secs(), 30);
    }

    #[test]
    fn the_backoff_defaults_are_a_hundred_milliseconds_growing_to_thirty_seconds() {
        let backoff = BackoffConfig::default();
        assert_eq!(backoff.initial.as_millis(), 100);
        assert_eq!(backoff.max.as_secs(), 30);
        assert!((backoff.jitter_fraction - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn the_backoff_delay_doubles_and_then_saturates_at_the_configured_ceiling() {
        let backoff = BackoffConfig {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(1),
            jitter_fraction: 0.0,
        };
        assert_eq!(backoff.delay_after(1), Duration::from_millis(100));
        assert_eq!(backoff.delay_after(2), Duration::from_millis(200));
        assert_eq!(backoff.delay_after(3), Duration::from_millis(400));
        assert_eq!(backoff.delay_after(4), Duration::from_millis(800));
        assert_eq!(backoff.delay_after(5), Duration::from_secs(1));
        // Saturating, not overflowing, however many failures accumulate.
        assert_eq!(backoff.delay_after(u32::MAX), Duration::from_secs(1));
    }

    #[test]
    fn jitter_only_ever_reduces_a_delay_so_the_ceiling_is_real() {
        let backoff = BackoffConfig {
            initial: Duration::from_secs(10),
            max: Duration::from_secs(10),
            jitter_fraction: 0.5,
        };
        for _ in 0..200 {
            let delay = backoff.delay_after(9);
            assert!(delay <= Duration::from_secs(10), "{delay:?} exceeded max");
            assert!(delay >= Duration::from_secs(5), "{delay:?} below max/2");
        }
    }

    #[tokio::test]
    async fn warm_up_fills_the_pool_to_the_configured_size() {
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(3, DEFAULT_INACTIVITY_TIMEOUT),
        );

        settle_until(|| pool.stats().ready == 3).await;

        let stats = pool.stats();
        assert_eq!(stats.ready, 3);
        assert_eq!(stats.in_progress, 0);
        assert_eq!(stats.created, 3);
        assert_eq!(stats.failed_creations, 0);
        // DEVIATION check: one shared pool means the configured size IS the warm count. Java's
        // per-event-loop pools make the real figure poolSize * numThreads (CP:159-161).
        assert_eq!(factory.attempts(), 3);
    }

    #[tokio::test]
    async fn a_hot_acquire_takes_a_ready_item_and_triggers_exactly_one_refill() {
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(2, DEFAULT_INACTIVITY_TIMEOUT),
        );
        settle_until(|| pool.stats().ready == 2).await;

        let item = pool.acquire().await.expect("a warm item is ready");
        let stats = pool.stats();
        assert_eq!(stats.hot_acquires, 1);
        assert_eq!(stats.cold_acquires, 0);
        // ESIP:248 — the slot is refilled at once, so one creation is now in flight.
        assert_eq!(stats.ready, 1);
        assert_eq!(stats.in_progress, 1);

        settle_until(|| pool.stats().ready == 2).await;
        assert_eq!(
            factory.attempts(),
            3,
            "2 warm-up creations plus exactly one refill"
        );

        // And it stays at one refill: the capacity guard (ESIP:312) stops the pool overshooting.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(factory.attempts(), 3);
        assert_eq!(pool.stats().ready, 2);
        assert!(
            item < 2,
            "the served item came from warm-up, not from an inline creation"
        );
    }

    #[tokio::test]
    async fn consuming_every_slot_yields_distinct_items_because_nothing_is_ever_returned() {
        // There is no `release`: ESIP:243 polls the ready queue and the Java class has no return
        // path, so four acquires from a size-2 pool require four creations.
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(2, DEFAULT_INACTIVITY_TIMEOUT),
        );
        settle_until(|| pool.stats().ready == 2).await;

        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(pool.acquire().await.expect("acquire"));
            settle_until(|| pool.stats().ready == 2).await;
        }

        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 4, "an acquired item must never come back");
        assert_eq!(pool.stats().hot_acquires, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cold_acquire_takes_over_the_oldest_creation_in_flight() {
        let factory = FakeFactory::delayed(Duration::from_secs(1));
        let pool = Arc::new(ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(2, DEFAULT_INACTIVITY_TIMEOUT),
        ));
        settle_until(|| pool.stats().in_progress == 2).await;
        assert_eq!(pool.stats().ready, 0);

        let acquirer = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move { pool.acquire().await })
        };

        // The takeover itself must not block: once it has happened a replacement creation has
        // already been started (ESIP:266), which is observable as a third attempt.
        settle_until(|| factory.attempts() == 3).await;
        let stats = pool.stats();
        assert_eq!(stats.cold_acquires, 1, "nothing was ready, so this is cold");
        assert_eq!(stats.hot_acquires, 0);
        assert_eq!(
            stats.in_progress, 2,
            "the taken-over creation left the pool's in-flight set, and one replaced it"
        );

        let item = acquirer.await.expect("task").expect("cold acquire");
        assert_eq!(
            item, 0,
            "ESIP:260-263 takes the OLDEST creation in flight, which is the first one started"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cold_acquire_with_nothing_in_flight_creates_inline_and_reports_a_failure() {
        // ESIP:269: with the ready queue empty and nothing in progress, the caller's own creation
        // is the one that runs, so its failure is the caller's failure.
        let factory = FakeFactory::always_failing();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(1, DEFAULT_INACTIVITY_TIMEOUT),
        );
        settle_until(|| pool.stats().failed_creations >= 1).await;
        assert_eq!(
            pool.stats().in_progress,
            0,
            "the failed creation is waiting out its backoff, so nothing is in flight"
        );

        let error = pool.acquire().await.expect_err("inline creation must fail");
        assert!(
            matches!(&error, PoolError::Creation(message) if message.contains("unreachable")),
            "PoolError must carry enough to log: {error:?}"
        );
        assert_eq!(pool.stats().cold_acquires, 1);
    }

    #[tokio::test]
    async fn a_size_of_zero_bypasses_the_pool_entirely_and_reports_no_statistics() {
        let factory = FakeFactory::immediate();
        let pool = PreconnectPool::new(Arc::clone(&factory), SlotPoolConfig::default());

        assert!(
            !pool.is_enabled(),
            "CP:162 default size 0 disables the pool"
        );
        assert!(pool.stats().is_none());
        assert_eq!(factory.attempts(), 0, "a disabled pool warms nothing");

        let item = pool.acquire().await.expect("direct creation");
        assert_eq!(item, 0);
        assert_eq!(
            factory.attempts(),
            1,
            "BCP:46-48 — exactly one connect per acquire, with nothing held warm"
        );
        assert!(pool.stats().is_none());

        pool.shutdown();
        pool.shutdown();
        assert!(pool.acquire().await.is_ok(), "a disabled pool cannot close");
    }

    #[tokio::test]
    async fn an_enabled_preconnect_pool_reports_statistics_and_serves_warm_items() {
        let factory = FakeFactory::immediate();
        let pool = PreconnectPool::new(
            Arc::clone(&factory),
            enabled_config(2, DEFAULT_INACTIVITY_TIMEOUT),
        );
        assert!(pool.is_enabled());
        settle_until(|| pool.stats().is_some_and(|stats| stats.ready == 2)).await;

        pool.acquire().await.expect("warm item");
        assert_eq!(
            pool.stats()
                .expect("enabled pools report stats")
                .hot_acquires,
            1
        );

        pool.shutdown();
        assert!(matches!(pool.acquire().await, Err(PoolError::Closed)));
    }

    #[tokio::test]
    async fn a_zero_inactivity_timeout_makes_the_pool_closed() {
        // ESIP:238-240 — both getAvailableOrNewItem and beginLoadingNewItemIfNecessary throw
        // PoolClosedException when the timeout is zero.
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            SlotPoolConfig {
                size: 2,
                inactivity_timeout: Duration::ZERO,
                ..SlotPoolConfig::default()
            },
        );

        assert!(pool.is_closed());
        assert!(matches!(pool.acquire().await, Err(PoolError::Closed)));
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(factory.attempts(), 0, "a closed pool must not warm up");
        assert_eq!(pool.stats(), SlotPoolStats::default());
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_item_is_discarded_and_replaced() {
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(1, Duration::from_secs(5)),
        );
        settle_until(|| pool.stats().ready == 1).await;
        assert_eq!(factory.discarded(), 0);

        // The sweep is scheduled one inactivity timeout after the item became ready (ESIP:199).
        tokio::time::sleep(Duration::from_secs(6)).await;
        settle_until(|| factory.discarded() == 1 && pool.stats().ready == 1).await;

        let stats = pool.stats();
        assert_eq!(stats.expired, 1);
        assert_eq!(
            stats.created, 2,
            "ESIP:304 refills the slot the expired item vacated"
        );
        assert_eq!(stats.ready, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_rate_is_bounded_under_a_permanently_failing_factory() {
        // Spec §13-D12. Java's ESIP:200-204 retries straight from the failure branch, so this
        // factory would be called as fast as the event loop could dispatch — the `TODO - add some
        // backoff here` is the whole defect. With backoff, the attempts in a fixed window are
        // bounded by window/max plus the handful of doublings it takes to reach `max`.
        let factory = FakeFactory::always_failing();
        let backoff = BackoffConfig {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(1),
            jitter_fraction: 0.0,
        };
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            SlotPoolConfig {
                size: 1,
                inactivity_timeout: Duration::from_mins(5),
                backoff,
            },
        );

        let window = Duration::from_secs(10);
        tokio::time::sleep(window).await;
        let attempts = factory.attempts();

        // Schedule with these constants: attempts at t = 0, 0.1, 0.3, 0.7, 1.5, then every 1.0 s.
        // That is 5 + 8 = 13 within a 10 s window. The bound below is derived, not observed:
        // window/max = 10, plus the 4 sub-ceiling delays, plus the initial attempt.
        let ceiling = 10 + 4 + 1;
        assert!(
            attempts <= ceiling,
            "{attempts} creation attempts in {window:?} exceeds the derived bound of {ceiling}"
        );
        assert!(
            attempts >= 5,
            "{attempts} attempts means the pool stopped retrying altogether"
        );
        assert_eq!(pool.stats().failed_creations, attempts);
        assert_eq!(pool.stats().ready, 0);
    }

    #[tokio::test]
    async fn shutdown_discards_every_ready_item_and_stops_refilling() {
        let factory = FakeFactory::immediate();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(3, DEFAULT_INACTIVITY_TIMEOUT),
        );
        settle_until(|| pool.stats().ready == 3).await;
        let attempts_before = factory.attempts();

        pool.shutdown();
        assert!(pool.is_closed());
        assert_eq!(
            factory.discarded(),
            3,
            "every warm item is handed to discard"
        );
        assert!(matches!(pool.acquire().await, Err(PoolError::Closed)));

        for _ in 0..128 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            factory.attempts(),
            attempts_before,
            "no task may keep refilling after shutdown"
        );

        // Idempotent: a second shutdown discards nothing twice.
        pool.shutdown();
        assert_eq!(factory.discarded(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_a_backoff_retry_that_is_still_sleeping() {
        let factory = FakeFactory::always_failing();
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            SlotPoolConfig {
                size: 1,
                inactivity_timeout: Duration::from_mins(5),
                backoff: BackoffConfig {
                    initial: Duration::from_secs(5),
                    max: Duration::from_secs(5),
                    jitter_fraction: 0.0,
                },
            },
        );
        settle_until(|| factory.attempts() == 1).await;

        pool.shutdown();
        tokio::time::sleep(Duration::from_mins(1)).await;
        assert_eq!(
            factory.attempts(),
            1,
            "the sleeping retry must find the pool closed and do nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_item_created_after_shutdown_is_discarded_rather_than_queued() {
        let factory = FakeFactory::delayed(Duration::from_secs(1));
        let pool = ExpiringSlotPool::new(
            Arc::clone(&factory),
            enabled_config(1, DEFAULT_INACTIVITY_TIMEOUT),
        );
        settle_until(|| pool.stats().in_progress == 1).await;

        pool.shutdown();
        tokio::time::sleep(Duration::from_secs(5)).await;

        assert_eq!(factory.discarded(), 1, "the in-flight item must not leak");
        assert_eq!(pool.stats().ready, 0);
    }

    #[tokio::test]
    async fn dropping_the_pool_releases_its_warm_items() {
        let factory = FakeFactory::immediate();
        {
            let pool = ExpiringSlotPool::new(
                Arc::clone(&factory),
                enabled_config(2, DEFAULT_INACTIVITY_TIMEOUT),
            );
            settle_until(|| pool.stats().ready == 2).await;
        }
        assert_eq!(factory.discarded(), 2);
    }
}
