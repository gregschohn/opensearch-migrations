//! The single-owner publisher: lanes, heartbeats, acknowledgement linearisation, failure
//! broadcast, and the retirement barrier (traceability row KAFKA-03).
//!
//! # What single ownership does and does not mean
//!
//! The protocol requires **one submission order per `(writerNodeId, partition)`** and
//! non-overlapping heartbeat handoff. It does **not** require that one OS thread perform all
//! encoding, all producer calls, and all callback processing. Those are different claims, and
//! conflating them is how the Java implementation ended up architecturally capped at roughly
//! 1.8-2.5 of its 5 allocated cores, with the `capture-kafka-publisher` thread plus the producer
//! network thread carrying 44.5% of all Java-thread CPU on two threads while ten Netty event loops
//! shared the rest.
//!
//! This design keeps one owner and moves everything that does not need to be ordered off it:
//!
//! | Work | Thread | Why |
//! |---|---|---|
//! | record encoding | the connection owner, in `capture-wire` | a record arrives here already encoded, as an [`EncodedRecord`]; the actor never serialises a payload |
//! | per-connection acknowledgement chain | the connection owner, in `capture-wire` | record N+1 waits for N's acknowledgement per connection, so connections never queue behind each other |
//! | the acknowledgement CAS | the delivery-report thread | an acknowledgement that physically arrived before `E` must win even when the actor is backlogged past `E` |
//! | publication budget release | the delivery-report thread | backpressure lifts the moment Kafka acknowledges, not when the actor gets round to it |
//! | the `submit -> delivery` latency sample | the delivery-report thread | measured where it happens |
//! | the `E` deadline | the deadline clock thread | a backlogged publisher must not be able to delay the detector of its own stall |
//! | ordering, lane state, routing state, retirement | **the actor** | these are exactly the decisions that must be totally ordered |
//!
//! What remains on the actor per record is budget admission, one routing-state critical section,
//! lane bookkeeping, and the producer enqueue — a few microseconds, with no I/O, no encoding, and
//! no allocation of the payload.
//!
//! # The coupling this design cannot remove
//!
//! Under the default fail-closed policy every critical mutation blocks on its own acknowledgement
//! before the request is forwarded, and the benchmark corpus is 82% critical mutations. Proxy
//! throughput is therefore inversely proportional to Kafka acknowledgement latency — measured on
//! the Java baseline over 38 valid runs, with only acknowledgement latency varying: 0.70 ms gives
//! 32,245 req/s, 2.73 ms gives 7,396 req/s, 5.54 ms gives 3,595 req/s, 808 ms gives 95 req/s.
//!
//! That coupling is a **protocol property**, not an implementation artefact: the whole point of
//! capture-before-forward is that the mutation is not applied at the source until Kafka has
//! durably accepted its record. No amount of CPU efficiency or concurrency changes it, and
//! weakening it would mean forwarding a mutation this process cannot prove it captured. What an
//! implementation *can* do is avoid adding latency of its own, which is why `linger.ms` is forced
//! to 0 (see [`crate::client_config`]) and why the delivery-report path never queues behind
//! unrelated publisher work.
//!
//! # Where lane sharding would slot in, and what it would have to prove
//!
//! Sharding is deliberately **not** done now: the plan mandates the single-owner design for Phase 1
//! and parity risk dominates. The seam is [`PublisherActor::lanes`], a map keyed by
//! `(writerNodeId, partition)`. Each lane's mutable state is already reachable only through that
//! key, and [`RecordProducer`] is already shared rather than owned per lane, so a shard would own a
//! disjoint key subset and the producer handle would not change.
//!
//! The actor loop is the only thing that assumes a single owner today. A sharded version would have
//! to prove four things, none of which the current tests cover:
//!
//! 1. **Callbacks** — a delivery report must reach the shard that owns its lane, and the
//!    acknowledgement CAS must still happen before any hop, so the dispatch must be by lane key on
//!    the callback thread.
//! 2. **Heartbeats** — `allOf` over a generation's initial heartbeats spans shards, so generation
//!    activation needs a cross-shard join that cannot activate on a partial set.
//! 3. **Failure broadcast** — the first failure must still be canonical process-wide and must fail
//!    in-flight work in *every* shard; the write gate already provides the single order, but each
//!    shard needs its own cleanup pass.
//! 4. **Retirement** — the retirement barrier spans shards, so "every writer partition retired"
//!    becomes a cross-shard quorum rather than a local scan.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use capture_config::CaptureTiming;
use capture_wire::EncodedRecord;
use rdkafka::Timestamp;

use crate::ack_deadline::{
    AcknowledgementDeadline, DeadlineClockHandle, DeadlineSink, DeadlineStatus, LaneId,
};
use crate::budget::{BudgetClass, PublicationBudget};
use crate::error::{CaptureFailure, CorruptedCaptureState, CorruptionCheck, FailureReason};
use crate::metrics::{CaptureMetrics, PublishOutcome, RecordKind, RoutingTransition};
use crate::publisher_lane::PublisherLane;
use crate::records::HeartbeatRecord;
use crate::routing_state::{
    ConnectionRoute, PendingRoutingGeneration, RoutingError, RoutingState, WriterPartition,
};
use crate::write_gate::WriteGate;

/// How often the health report is emitted.
pub const HEALTH_REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// A record handed to the Kafka producer.
///
/// Carries an explicit partition, because the proxy owns the routing decision: the partition list
/// is the *assignment subset*, not the whole topic, so librdkafka's own partitioners cannot
/// reproduce it. With an explicit partition librdkafka bypasses partitioning entirely.
///
/// There is no header field, and there never will be: `CaptureRecord.payload` is the only record
/// type discriminator.
#[derive(Debug, Clone)]
pub struct OutboundRecord {
    /// The traffic topic.
    pub topic: String,
    /// The explicit partition.
    pub partition: i32,
    /// The record key.
    pub key: String,
    /// The encoded `CaptureRecord` envelope.
    pub value: Vec<u8>,
    /// Identifies this submission to the delivery report.
    pub send_id: u64,
    /// Which budget this record drew on, so the delivery-report thread can release it without
    /// waiting for the actor.
    pub budget_class: BudgetClass,
    /// Payload bytes, for the budget release.
    pub bytes: u64,
    /// For a heartbeat, the deadline the delivery-report thread must claim **before** handing the
    /// report to the actor. That ordering is what lets an acknowledgement that physically arrived
    /// before `E` win even when the actor is backlogged past `E`.
    pub deadline: Option<Arc<AcknowledgementDeadline>>,
}

/// Why a producer refused a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProduceError {
    /// The producer's own queue is full. **Retriable and lossless**: the caller holds the record
    /// and offers it again when a delivery report frees room. Never a capture failure.
    QueueFull,
    /// The producer rejected the record permanently.
    Fatal(String),
}

/// The Kafka producer, behind a trait.
///
/// A trait for two reasons. It makes every acknowledgement-ordering and failure rule in this module
/// testable without a broker — the Rust counterpart of the Java suite's `MockProducer`,
/// `TimestampingProducer`, `CallbackThenBlockProducer`, and `BlockedSendProducer`. And it is the
/// seam a sharded publisher would share rather than duplicate.
///
/// Implementations must be `Send + Sync` and must not block for I/O: librdkafka's enqueue is a
/// memcpy into its own queue and returns immediately, which is what makes it safe to call from the
/// actor.
pub trait RecordProducer: Send + Sync {
    /// Hands `record` to the producer.
    ///
    /// # Errors
    ///
    /// [`ProduceError::QueueFull`] when the record must be offered again later, or
    /// [`ProduceError::Fatal`] for a permanent refusal.
    fn enqueue(&self, record: OutboundRecord) -> Result<(), ProduceError>;

    /// Flushes outstanding records, blocking up to `timeout`.
    ///
    /// # Errors
    ///
    /// Returns a message when the flush did not complete.
    fn flush(&self, timeout: Duration) -> Result<(), String>;
}

/// What a delivery report told us.
#[derive(Debug, Clone)]
pub struct DeliveryNotice {
    /// Which submission this report is for.
    pub send_id: u64,
    /// The partition Kafka actually appended to.
    pub partition: Option<i32>,
    /// The timestamp Kafka reported, carrying its **type**.
    pub timestamp: Timestamp,
    /// The failure, if the record was not appended.
    pub error: Option<String>,
    /// Whether the acknowledgement CAS won its race. `None` for a record with no deadline.
    pub deadline_claimed: Option<bool>,
    /// The deadline's status after the CAS, for diagnosing a lost race.
    pub deadline_status: Option<DeadlineStatus>,
}

/// A successful traffic publication, as the connection owner needs to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcknowledgedRecord {
    /// The partition Kafka appended to.
    pub partition: i32,
    /// The broker-assigned `LogAppendTime`.
    pub log_append_time: i64,
}

/// How one traffic submission ended.
#[derive(Debug, Clone)]
pub struct TrafficOutcome {
    /// The connection-local ticket the caller supplied.
    pub ticket: u64,
    /// The outcome.
    pub result: Result<AcknowledgedRecord, CaptureFailure>,
}

/// Delivers one connection's publication outcomes and wakes its asynchronous waiter.
///
/// The outcome itself travels over a `std` channel so the delivery-report thread never touches a
/// runtime; the optional [`tokio::sync::Notify`] is signalled *after* the send, so a waiter that
/// re-drains on wake-up always finds the outcome. `notify_one` stores a permit when nobody is
/// waiting yet, so an outcome that lands between a drain and the next wait is never lost.
///
/// This replaces a fixed-interval poll in the capture-before-forward gate: the poll added up to
/// its interval to every Critical Mutation Traffic forward and woke every blocked connection
/// repeatedly, both of which land directly on the Phase 1 latency and CPU gates.
#[derive(Debug, Clone)]
pub struct OutcomeSender {
    tx: mpsc::Sender<TrafficOutcome>,
    notify: Option<Arc<tokio::sync::Notify>>,
}

impl OutcomeSender {
    /// An outcome channel whose sends also wake `notify`.
    #[must_use]
    pub fn new(tx: mpsc::Sender<TrafficOutcome>, notify: Arc<tokio::sync::Notify>) -> Self {
        Self {
            tx,
            notify: Some(notify),
        }
    }

    /// Sends `outcome`, then wakes the waiter.
    ///
    /// # Errors
    ///
    /// Returns the outcome when the receiving connection is gone, exactly like
    /// [`mpsc::Sender::send`].
    pub fn send(&self, outcome: TrafficOutcome) -> Result<(), mpsc::SendError<TrafficOutcome>> {
        let sent = self.tx.send(outcome);
        if let Some(notify) = &self.notify {
            notify.notify_one();
        }
        sent
    }
}

impl From<mpsc::Sender<TrafficOutcome>> for OutcomeSender {
    /// A plain channel with nobody to wake, for callers that drain synchronously.
    fn from(tx: mpsc::Sender<TrafficOutcome>) -> Self {
        Self { tx, notify: None }
    }
}

/// Commands accepted by the actor.
enum PublisherCommand {
    InitializeGeneration {
        partitions: Vec<i32>,
        reply: mpsc::Sender<Result<Arc<str>, CaptureFailure>>,
    },
    PublishTraffic(Box<TrafficSubmission>),
    Delivery(DeliveryNotice),
    AbandonConnection {
        route: ConnectionRoute,
    },
    GateTripped(CaptureFailure),
    DeadlineExpired(Arc<AcknowledgementDeadline>),
    RetireAllWriters {
        reply: mpsc::Sender<Result<(), CaptureFailure>>,
    },
    ReportHealth,
    Close,
}

struct TrafficSubmission {
    route: ConnectionRoute,
    record: EncodedRecord,
    is_terminal: bool,
    ticket: u64,
    outcomes: OutcomeSender,
}

/// A cheap, cloneable handle to the publisher actor.
#[derive(Clone)]
pub struct PublisherHandle {
    commands: mpsc::Sender<PublisherCommand>,
    failure: Arc<Mutex<Option<CaptureFailure>>>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for PublisherHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherHandle")
            .field("failed", &self.failure().is_some())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PublisherHandle {
    /// The terminal failure, once the publisher has one.
    #[must_use]
    pub fn failure(&self) -> Option<CaptureFailure> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn send(&self, command: PublisherCommand) -> Result<(), CaptureFailure> {
        self.commands.send(command).map_err(|_| {
            self.failure().unwrap_or_else(|| {
                CaptureFailure::capture(
                    FailureReason::ExecutorRejected,
                    "the capture Kafka publisher is no longer accepting work",
                )
            })
        })
    }

    /// Requests a routing generation for `partitions`.
    ///
    /// The reply carries the activated writer identity, and arrives only after an initial heartbeat
    /// has been acknowledged and accepted on **every** partition.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already failed or closed.
    pub fn initialize_generation(
        &self,
        partitions: Vec<i32>,
    ) -> Result<mpsc::Receiver<Result<Arc<str>, CaptureFailure>>, CaptureFailure> {
        let (reply, answer) = mpsc::channel();
        self.send(PublisherCommand::InitializeGeneration { partitions, reply })?;
        Ok(answer)
    }

    /// Submits one traffic record.
    ///
    /// Does not block on Kafka: the outcome arrives on `outcomes`.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already failed or closed, in which case the
    /// record has been dropped and its buffer released.
    pub fn publish_traffic(
        &self,
        route: ConnectionRoute,
        record: EncodedRecord,
        is_terminal: bool,
        ticket: u64,
        outcomes: impl Into<OutcomeSender>,
    ) -> Result<(), CaptureFailure> {
        self.send(PublisherCommand::PublishTraffic(Box::new(
            TrafficSubmission {
                route,
                record,
                is_terminal,
                ticket,
                outcomes: outcomes.into(),
            },
        )))
    }

    /// Reports that a connection closed without ever publishing.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already failed or closed.
    pub fn abandon_connection(&self, route: ConnectionRoute) -> Result<(), CaptureFailure> {
        self.send(PublisherCommand::AbandonConnection { route })
    }

    /// Begins orderly retirement of every writer partition.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already failed or closed.
    pub fn retire_all_writers(
        &self,
    ) -> Result<mpsc::Receiver<Result<(), CaptureFailure>>, CaptureFailure> {
        let (reply, answer) = mpsc::channel();
        self.send(PublisherCommand::RetireAllWriters { reply })?;
        Ok(answer)
    }

    /// Forces one health report. The test hook for `reportHealthNow`.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already failed or closed.
    pub fn report_health(&self) -> Result<(), CaptureFailure> {
        self.send(PublisherCommand::ReportHealth)
    }

    /// Fails the publisher with `failure`, from outside.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already closed.
    pub fn fail(&self, failure: CaptureFailure) -> Result<(), CaptureFailure> {
        self.send(PublisherCommand::GateTripped(failure))
    }

    /// Asks the actor to stop. Does not wait.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already closed.
    pub fn close(&self) -> Result<(), CaptureFailure> {
        self.closed.store(true, Ordering::Release);
        self.send(PublisherCommand::Close)
    }

    /// Delivers a delivery report to the actor.
    ///
    /// Called from the producer's delivery-report thread, **after** the acknowledgement CAS.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher is already closed; a delivery report arriving
    /// after close needs no action.
    pub fn deliver(&self, notice: DeliveryNotice) -> Result<(), CaptureFailure> {
        self.send(PublisherCommand::Delivery(notice))
    }
}

/// The deadline sink: tripping the gate is a mutex operation, so it cannot be starved by publisher
/// backlog. The actor is told afterwards, as cleanup, not as the decision.
struct PublisherDeadlineSink {
    gate: Arc<WriteGate>,
    metrics: Arc<dyn CaptureMetrics>,
    handle: Mutex<Option<PublisherHandle>>,
    closed: Arc<AtomicBool>,
}

impl DeadlineSink for PublisherDeadlineSink {
    fn is_moot(&self) -> bool {
        self.closed.load(Ordering::Acquire) || !self.gate.is_open()
    }

    fn on_expired(&self, deadline: &Arc<AcknowledgementDeadline>) {
        self.metrics.heartbeat_deadline_violated();
        tracing::warn!(
            lane = deadline.lane().0,
            epoch = deadline.epoch(),
            "event=capture_heartbeat_late"
        );
        // The decision is made here, on the clock thread, by tripping the gate. The actor's own
        // cleanup follows as a message and cannot delay it.
        self.gate.trip(CaptureFailure::capture(
            FailureReason::HeartbeatDeadline,
            format!(
                "Heartbeat acknowledgement deadline expired for lane {} epoch {}",
                deadline.lane().0,
                deadline.epoch()
            ),
        ));
        if let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            drop(handle.send(PublisherCommand::DeadlineExpired(Arc::clone(deadline))));
        }
    }
}

struct InFlightRecord {
    kind: RecordKind,
    writer_partition: WriterPartition,
    bytes: u64,
    accepted_at: Instant,
    submitted_at: Instant,
    /// For traffic: who to tell and under which ticket.
    traffic: Option<(ConnectionRoute, u64, OutcomeSender, bool)>,
}

struct HeldTraffic {
    submission: Box<TrafficSubmission>,
    accepted_at: Instant,
    bytes: u64,
}

struct GenerationRequest {
    partitions: Vec<i32>,
    reply: mpsc::Sender<Result<Arc<str>, CaptureFailure>>,
}

struct InProgressGeneration {
    pending: PendingRoutingGeneration,
    awaiting: usize,
    reply: mpsc::Sender<Result<Arc<str>, CaptureFailure>>,
}

/// Everything the actor needs to run.
pub struct PublisherConfig {
    /// The traffic topic.
    pub topic: String,
    /// The validated `F`/`H`/`E`/`S` intervals.
    pub timing: CaptureTiming,
    /// The maximum serialized `CaptureRecord` size.
    pub payload_size_limit: usize,
    /// The shared routing state.
    pub routing: Arc<Mutex<RoutingState>>,
    /// The process-wide write gate.
    pub gate: Arc<WriteGate>,
    /// The publication budget.
    pub budget: Arc<PublicationBudget>,
    /// The metric sink.
    pub metrics: Arc<dyn CaptureMetrics>,
    /// The producer.
    pub producer: Arc<dyn RecordProducer>,
    /// Called when a partition above the known count is assigned, to refresh the topic layout.
    /// Returns the refreshed partition count. See [`RoutingError::PartitionAboveKnownCount`].
    pub refresh_partition_count: Arc<dyn Fn() -> Option<usize> + Send + Sync>,
}

/// The single-owner publisher actor.
pub struct PublisherActor {
    config: PublisherConfig,
    commands: mpsc::Receiver<PublisherCommand>,
    handle: PublisherHandle,
    deadline_clock: DeadlineClockHandle,
    /// SHARDING SEAM: one owner per `(writerNodeId, partition)`.
    ///
    /// A sharded publisher partitions **this map** by lane key and nothing else: the producer
    /// handle is already shared rather than owned per lane, lane state is already reachable only
    /// through this key, and encoding and the per-connection acknowledgement chain are already
    /// outside the actor. The actor loop is the only remaining thing that assumes a single owner.
    ///
    /// Four proofs a sharded version would need, none of which the current tests cover — see the
    /// module documentation for why each is load-bearing: delivery-report dispatch by lane key with
    /// the acknowledgement CAS still ahead of any hop; a cross-shard join for a generation's
    /// initial heartbeats that cannot activate on a partial set; per-shard failure cleanup behind
    /// the one canonical process-wide cause; and a cross-shard quorum for the retirement barrier.
    lanes: HashMap<WriterPartition, PublisherLane>,
    lane_keys: HashMap<LaneId, WriterPartition>,
    next_lane_id: u64,
    next_send_id: u64,
    in_flight: HashMap<u64, InFlightRecord>,
    held_traffic: VecDeque<HeldTraffic>,
    generation_queue: VecDeque<GenerationRequest>,
    generation_in_progress: Option<InProgressGeneration>,
    retirement_reply: Option<mpsc::Sender<Result<(), CaptureFailure>>>,
    failure: Option<CaptureFailure>,
    next_health_report: Instant,
    stop: bool,
}

/// A started publisher: the handle plus the thread it runs on.
pub struct PublisherRuntime {
    /// The handle for submitting work.
    pub handle: PublisherHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for PublisherRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherRuntime")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl PublisherRuntime {
    /// Waits for the actor thread to finish.
    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for PublisherRuntime {
    fn drop(&mut self) {
        drop(self.handle.close());
        self.join();
    }
}

/// Starts the publisher actor, its deadline clock, and wires the gate listener.
///
/// # Errors
///
/// Returns the OS error if a thread cannot be spawned. A capture process that cannot own its
/// publisher must not start.
pub fn start(
    config: PublisherConfig,
) -> std::io::Result<(PublisherRuntime, crate::ack_deadline::DeadlineClock)> {
    let (commands, receiver) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let handle = PublisherHandle {
        commands,
        failure: Arc::new(Mutex::new(None)),
        closed: Arc::clone(&closed),
    };

    let sink = Arc::new(PublisherDeadlineSink {
        gate: Arc::clone(&config.gate),
        metrics: Arc::clone(&config.metrics),
        handle: Mutex::new(Some(handle.clone())),
        closed: Arc::clone(&closed),
    });
    let clock =
        crate::ack_deadline::DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)?;
    let deadline_clock = clock.handle();

    // Tripping the gate from anywhere — the deadline clock, the startup sequence, another
    // component — fails the publisher. The actor's `failure` latch makes the resulting re-entry a
    // no-op.
    {
        let handle = handle.clone();
        config
            .gate
            .add_terminal_failure_listener(Box::new(move |failure| {
                drop(handle.send(PublisherCommand::GateTripped(failure.clone())));
            }));
    }

    let mut actor = PublisherActor {
        config,
        commands: receiver,
        handle: handle.clone(),
        deadline_clock,
        lanes: HashMap::new(),
        lane_keys: HashMap::new(),
        next_lane_id: 1,
        next_send_id: 1,
        in_flight: HashMap::new(),
        held_traffic: VecDeque::new(),
        generation_queue: VecDeque::new(),
        generation_in_progress: None,
        retirement_reply: None,
        failure: None,
        next_health_report: Instant::now() + HEALTH_REPORT_INTERVAL,
        stop: false,
    };
    let thread = std::thread::Builder::new()
        .name("capture-kafka-publisher".to_owned())
        .spawn(move || actor.run())?;

    Ok((
        PublisherRuntime {
            handle,
            thread: Some(thread),
        },
        clock,
    ))
}

impl PublisherActor {
    fn run(&mut self) {
        while !self.stop {
            let timeout = self.time_until_next_timer();
            match self.commands.recv_timeout(timeout) {
                Ok(command) => self.handle_command(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.run_due_timers();
        }
    }

    fn time_until_next_timer(&self) -> Duration {
        let now = Instant::now();
        let mut next = self.next_health_report;
        for lane in self.lanes.values() {
            if let Some(due) = lane.next_heartbeat_at() {
                next = next.min(due);
            }
        }
        next.saturating_duration_since(now)
            .min(HEALTH_REPORT_INTERVAL)
    }

    fn run_due_timers(&mut self) {
        let now = Instant::now();
        if now >= self.next_health_report {
            self.next_health_report = now + HEALTH_REPORT_INTERVAL;
            self.report_health();
        }
        if self.failure.is_some() {
            return;
        }
        let due: Vec<WriterPartition> = self
            .lanes
            .iter()
            .filter(|(_, lane)| lane.next_heartbeat_at().is_some_and(|due| due <= now))
            .map(|(key, _)| key.clone())
            .collect();
        for writer_partition in due {
            if let Some(lane) = self.lanes.get_mut(&writer_partition) {
                lane.cancel_next_heartbeat();
            }
            if let Err(error) = self.publish_heartbeat(&writer_partition, false) {
                self.fail(error);
                return;
            }
        }
    }

    fn handle_command(&mut self, command: PublisherCommand) {
        match command {
            PublisherCommand::InitializeGeneration { partitions, reply } => {
                if let Some(failure) = self.failure.clone() {
                    drop(reply.send(Err(failure)));
                    return;
                }
                self.generation_queue
                    .push_back(GenerationRequest { partitions, reply });
                self.start_next_generation();
            }
            PublisherCommand::PublishTraffic(submission) => self.accept_traffic(submission),
            PublisherCommand::Delivery(notice) => self.handle_delivery(&notice),
            PublisherCommand::AbandonConnection { route } => self.abandon_connection(&route),
            PublisherCommand::GateTripped(failure) => self.fail(failure),
            PublisherCommand::DeadlineExpired(deadline) => self.handle_deadline_expired(&deadline),
            PublisherCommand::RetireAllWriters { reply } => self.retire_all_writers(reply),
            PublisherCommand::ReportHealth => self.report_health(),
            PublisherCommand::Close => self.close(),
        }
    }

    // ---------------------------------------------------------------- generations

    fn start_next_generation(&mut self) {
        // Strictly one generation initialization at a time, so replacement generations initialize
        // in membership-callback order. Without this, two assignments could interleave their
        // initial heartbeats and a later assignment could activate before an earlier one.
        if self.generation_in_progress.is_some() {
            return;
        }
        let Some(request) = self.generation_queue.pop_front() else {
            return;
        };
        if let Err(failure) = self.begin_generation(&request) {
            let canonical = self.fail_canonical(failure);
            drop(request.reply.send(Err(canonical)));
        }
    }

    fn begin_generation(&mut self, request: &GenerationRequest) -> Result<(), CaptureFailure> {
        let pending = self.prepare_generation(&request.partitions)?;
        self.config
            .metrics
            .routing_transition(RoutingTransition::GenerationPrepared);
        tracing::info!(
            writer_node_id = %pending.writer_node_id(),
            generation = pending.sequence(),
            partitions = ?pending.partitions(),
            "event=capture_generation_prepared"
        );

        let writer_partitions = pending.writer_partitions();
        self.generation_in_progress = Some(InProgressGeneration {
            pending,
            awaiting: writer_partitions.len(),
            reply: request.reply.clone(),
        });

        // Lanes are created in ascending partition order, and each one's initial heartbeat is
        // submitted as it is created.
        for writer_partition in &writer_partitions {
            let lane_id = LaneId(self.next_lane_id);
            self.next_lane_id += 1;
            if self.lanes.contains_key(writer_partition) {
                return Err(CaptureFailure::corrupted(CorruptedCaptureState::new(
                    CorruptionCheck::PublisherLaneAlreadyExists,
                    writer_partition.to_string(),
                )));
            }
            self.lanes.insert(
                writer_partition.clone(),
                PublisherLane::new(writer_partition.clone(), lane_id),
            );
            self.lane_keys.insert(lane_id, writer_partition.clone());
            self.publish_heartbeat(writer_partition, true)?;
        }
        Ok(())
    }

    /// Prepares a generation, refreshing the topic layout once if a partition is above the known
    /// count.
    fn prepare_generation(
        &mut self,
        partitions: &[i32],
    ) -> Result<PendingRoutingGeneration, CaptureFailure> {
        match self.with_routing(|routing| routing.prepare_routing_generation(partitions)) {
            Ok(pending) => Ok(pending),
            Err(RoutingError::PartitionAboveKnownCount {
                partition,
                known_count,
            }) => {
                // DEVIATION, spec discrepancy D-15 under owner decision OD-7: an out-of-band
                // partition increase is the intended operational path, so being assigned a new
                // partition must not terminate capture. Java freezes the count at construction and
                // treats this as fatal, which would mean every intentional partition increase
                // causes a capture outage on every running proxy.
                tracing::info!(
                    partition,
                    known_count,
                    "event=capture_topic_growth_detected reason=assignment_above_known_count"
                );
                let refreshed = (self.config.refresh_partition_count)().ok_or_else(|| {
                    CaptureFailure::capture(
                        FailureReason::Other,
                        format!(
                            "partition {partition} was assigned but the traffic topic layout could \
                             not be refreshed to confirm it ({known_count} partitions known)"
                        ),
                    )
                })?;
                let grown = self.with_routing(|routing| {
                    Ok::<bool, RoutingError>(routing.raise_topic_partition_count(refreshed))
                });
                match grown {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(CaptureFailure::capture(
                            FailureReason::Other,
                            format!(
                                "partition {partition} was assigned but a metadata refresh still \
                                 reports only {refreshed} partitions"
                            ),
                        ));
                    }
                    Err(error) => return Err(error.into_capture_failure()),
                }
                self.with_routing(|routing| routing.prepare_routing_generation(partitions))
                    .map_err(RoutingError::into_capture_failure)
            }
            Err(error) => Err(error.into_capture_failure()),
        }
    }

    fn finish_generation(&mut self) {
        let Some(in_progress) = self.generation_in_progress.take() else {
            return;
        };
        let activation = match self
            .with_routing(|routing| routing.activate_routing_generation(&in_progress.pending))
        {
            Ok(activation) => activation,
            Err(error) => {
                let canonical = self.fail_canonical(error.into_capture_failure());
                drop(in_progress.reply.send(Err(canonical)));
                return;
            }
        };

        self.config
            .metrics
            .routing_transition(RoutingTransition::GenerationActivated);
        let activated = activation.partitions.len() as u64;
        let previously_active = activation.previous_partitions.len() as u64;
        self.config
            .metrics
            .writers_activated(activated, previously_active);
        if let Some(previous) = &activation.previous_writer_node_id {
            tracing::info!(
                writer_node_id = %previous,
                partitions = ?activation.previous_partitions,
                "event=capture_writer_draining reason=replaced"
            );
        }
        tracing::info!(
            writer_node_id = %activation.writer_node_id,
            generation = activation.sequence,
            partitions = ?activation.partitions,
            "event=capture_generation_activated"
        );
        drop(
            in_progress
                .reply
                .send(Ok(Arc::clone(&activation.writer_node_id))),
        );

        // A superseded generation with no connections retires immediately.
        if let Err(failure) = self.begin_drained_writer_retirements() {
            self.fail(failure);
            return;
        }
        self.start_next_generation();
    }

    // ---------------------------------------------------------------- heartbeats

    fn publish_heartbeat(
        &mut self,
        writer_partition: &WriterPartition,
        initial: bool,
    ) -> Result<(), CaptureFailure> {
        let baseline = self
            .with_routing(|routing| {
                routing.last_accepted_heartbeat_log_append_time(writer_partition)
            })
            .map_err(RoutingError::into_capture_failure)?;

        let lane = self.lanes.get_mut(writer_partition).ok_or_else(|| {
            CaptureFailure::corrupted(CorruptedCaptureState::new(
                CorruptionCheck::NoPublisherLaneExists,
                writer_partition.to_string(),
            ))
        })?;
        lane.begin_heartbeat().map_err(CaptureFailure::corrupted)?;

        // An initial heartbeat gets a brand-new deadline whose timer is armed only *after* the
        // enqueue returns. A periodic heartbeat reuses the deadline created when the previous
        // acknowledgement was accepted, which was armed at acceptance time — so heartbeat n+1's
        // deadline is E after heartbeat n's acceptance, not E after n+1's submission.
        let (deadline, arm_after_submission) = if baseline.is_none() {
            (lane.replace_acknowledgement_deadline(), true)
        } else {
            (
                lane.require_acknowledgement_deadline()
                    .map_err(CaptureFailure::corrupted)?,
                false,
            )
        };
        debug_assert_eq!(
            initial,
            baseline.is_none(),
            "initial heartbeat iff no baseline"
        );

        let record = HeartbeatRecord::build(
            writer_partition,
            i64::try_from(self.config.timing.heartbeat_interval().as_millis()).unwrap_or(i64::MAX),
            Some(now_millis()),
        );
        let bytes = record.value().len() as u64;

        // Control records draw on the reserved budget, so traffic can never starve a heartbeat and
        // make the proxy declare capture compromised because it was busy capturing.
        if !self.config.budget.try_reserve(BudgetClass::Control, bytes) {
            return Err(CaptureFailure::capture(
                FailureReason::Other,
                format!(
                    "the capture control budget is exhausted, so the heartbeat for \
                     {writer_partition} cannot be published"
                ),
            ));
        }

        let send_id = self.next_send_id;
        self.next_send_id += 1;
        let accepted_at = Instant::now();
        let outbound = OutboundRecord {
            topic: self.config.topic.clone(),
            partition: record.partition(),
            key: record.key().to_owned(),
            value: record.value().to_vec(),
            send_id,
            budget_class: BudgetClass::Control,
            bytes,
            deadline: Some(Arc::clone(&deadline)),
        };

        match self.submit(
            outbound,
            RecordKind::Heartbeat,
            writer_partition.clone(),
            bytes,
            accepted_at,
            None,
        ) {
            Ok(()) => {
                if arm_after_submission {
                    // Armed here, never before the enqueue: a blocked enqueue must not be charged
                    // against E.
                    self.deadline_clock
                        .arm(&deadline, self.config.timing.heartbeat_expiration());
                }
                Ok(())
            }
            Err(failure) => {
                self.config.budget.release(BudgetClass::Control, bytes);
                if let Some(lane) = self.lanes.get_mut(writer_partition) {
                    lane.abandon_in_flight_heartbeat();
                }
                Err(failure)
            }
        }
    }

    fn accept_heartbeat_acknowledgement(
        &mut self,
        writer_partition: &WriterPartition,
        log_append_time: i64,
    ) -> Result<(), CaptureFailure> {
        self.with_routing(|routing| {
            routing.accept_heartbeat_log_append_time(
                writer_partition,
                log_append_time,
                self.config.timing.heartbeat_expiration(),
                self.config.timing.clock_skew_allowance(),
            )
        })
        .map_err(|error| {
            if !error.is_corruption() {
                self.config.metrics.broker_time_violated();
                tracing::warn!(
                    writer_node_id = %writer_partition.writer_node_id(),
                    partition = writer_partition.partition(),
                    "event=capture_broker_time_violation"
                );
            }
            error.into_capture_failure()
        })?;

        let expiration = self.config.timing.heartbeat_expiration();
        let interval = self.config.timing.heartbeat_interval();
        let lane = self.lanes.get_mut(writer_partition).ok_or_else(|| {
            CaptureFailure::corrupted(CorruptedCaptureState::new(
                CorruptionCheck::NoPublisherLaneExists,
                writer_partition.to_string(),
            ))
        })?;
        lane.accept_heartbeat_acknowledgement()
            .map_err(CaptureFailure::corrupted)?;

        if lane.status() == crate::publisher_lane::PublisherLaneStatus::Open {
            // Renew: a fresh deadline, armed now, covering the *next* heartbeat. The next H timer
            // also starts now, at acceptance, not at the previous submission.
            let renewed = lane.replace_acknowledgement_deadline();
            lane.schedule_next_heartbeat(Instant::now() + interval);
            self.deadline_clock.arm(&renewed, expiration);
        } else {
            // A lane that is no longer OPEN stops heartbeating, and its deadline is resolved by the
            // publisher rather than left to expire.
            lane.cancel_acknowledgement_deadline();
        }

        // A generation becomes usable only once every one of its partitions has reached this point.
        let completed = self
            .generation_in_progress
            .as_mut()
            .is_some_and(|in_progress| {
                if in_progress
                    .pending
                    .writer_partitions()
                    .contains(writer_partition)
                    && in_progress.awaiting > 0
                {
                    in_progress.awaiting -= 1;
                    in_progress.awaiting == 0
                } else {
                    false
                }
            });
        if completed {
            self.finish_generation();
        }
        Ok(())
    }

    // ---------------------------------------------------------------- traffic

    fn accept_traffic(&mut self, submission: Box<TrafficSubmission>) {
        if let Some(failure) = self.failure.clone() {
            Self::report_traffic_outcome(&submission, Err(failure));
            return;
        }
        if let Some(failure) = self.config.gate.failure_if_not_writable() {
            Self::report_traffic_outcome(&submission, Err(failure.clone()));
            self.fail(failure);
            return;
        }

        // The record's identity must match the route it was created under. A mismatch means the
        // connection's records could be attributed to the wrong writer or partition, which no
        // policy can recover from.
        if submission.record.node_id() != submission.route.writer_node_id()
            || submission.record.connection_id() != submission.route.connection_id()
        {
            let canonical =
                self.fail_canonical(CaptureFailure::corrupted(CorruptedCaptureState::new(
                    CorruptionCheck::TrafficIdentityMismatch,
                    format!(
                        "record {}#{} against route {}",
                        submission.record.node_id(),
                        submission.record.connection_id(),
                        submission.route
                    ),
                )));
            Self::report_traffic_outcome(&submission, Err(canonical));
            return;
        }

        let bytes = submission.record.payload_len() as u64;
        if submission.record.payload_len() > self.config.payload_size_limit {
            // Terminal, not a retry and not a split: the record is already encoded, and splitting
            // it here would change record boundaries the replayer depends on.
            let canonical = self.fail_canonical(CaptureFailure::capture(
                FailureReason::PayloadLimit,
                format!(
                    "a capture record of {} bytes exceeds the configured payload limit of {} bytes",
                    submission.record.payload_len(),
                    self.config.payload_size_limit
                ),
            ));
            Self::report_traffic_outcome(&submission, Err(canonical));
            return;
        }

        if let Err(error) = self.with_routing(|routing| {
            routing.accept_traffic_submission(&submission.route, submission.is_terminal)
        }) {
            let canonical = self.fail_canonical(error.into_capture_failure());
            Self::report_traffic_outcome(&submission, Err(canonical));
            return;
        }

        let accepted_at = Instant::now();
        if self.config.budget.try_reserve(BudgetClass::Traffic, bytes) {
            self.submit_traffic(submission, bytes, accepted_at);
        } else {
            // Held, never dropped. The per-connection chain hands this sink at most one
            // unacknowledged record per connection, so the held queue is bounded by the live
            // connection count.
            self.config
                .metrics
                .queue_depth_changed(RecordKind::Traffic, 1, bytes.cast_signed());
            self.held_traffic.push_back(HeldTraffic {
                submission,
                accepted_at,
                bytes,
            });
        }
    }

    fn submit_traffic(
        &mut self,
        submission: Box<TrafficSubmission>,
        bytes: u64,
        accepted_at: Instant,
    ) {
        let send_id = self.next_send_id;
        self.next_send_id += 1;
        let writer_partition = submission.route.writer_partition();
        let outbound = OutboundRecord {
            topic: self.config.topic.clone(),
            partition: submission.route.partition(),
            key: submission.route.connection_id().to_owned(),
            value: submission.record.payload().to_vec(),
            send_id,
            budget_class: BudgetClass::Traffic,
            bytes,
            deadline: None,
        };
        let traffic = (
            submission.route.clone(),
            submission.ticket,
            submission.outcomes.clone(),
            submission.is_terminal,
        );

        match self.submit(
            outbound,
            RecordKind::Traffic,
            writer_partition,
            bytes,
            accepted_at,
            Some(traffic),
        ) {
            Ok(()) => {
                // The encoded record's buffer is released here: librdkafka copied the payload into
                // its own queue, so holding it would double the publication memory for no reason.
                drop(submission);
            }
            Err(failure) => {
                self.config.budget.release(BudgetClass::Traffic, bytes);
                let canonical = self.fail_canonical(failure);
                Self::report_traffic_outcome(&submission, Err(canonical));
            }
        }
    }

    /// Hands one record to the producer under the write gate.
    fn submit(
        &mut self,
        outbound: OutboundRecord,
        kind: RecordKind,
        writer_partition: WriterPartition,
        bytes: u64,
        accepted_at: Instant,
        traffic: Option<(ConnectionRoute, u64, OutcomeSender, bool)>,
    ) -> Result<(), CaptureFailure> {
        let send_id = outbound.send_id;
        let producer = Arc::clone(&self.config.producer);
        let enqueued = self
            .config
            .gate
            .submit_if_writable(move || producer.enqueue(outbound))?;

        match enqueued {
            Ok(()) => {}
            Err(ProduceError::QueueFull) => {
                // Lossless: the budget slot is released and the caller re-offers the record.
                return Err(CaptureFailure::capture(
                    FailureReason::Other,
                    "the Kafka producer queue is full".to_owned(),
                ));
            }
            Err(ProduceError::Fatal(detail)) => {
                return Err(CaptureFailure::capture(FailureReason::KafkaSend, detail));
            }
        }

        let submitted_at = Instant::now();
        self.config
            .metrics
            .accept_to_submit(kind, submitted_at.saturating_duration_since(accepted_at));
        self.config.metrics.publication_accepted(kind, bytes);
        if let Some(lane) = self.lanes.get_mut(&writer_partition) {
            lane.add_in_flight();
        }
        self.in_flight.insert(
            send_id,
            InFlightRecord {
                kind,
                writer_partition,
                bytes,
                accepted_at,
                submitted_at,
                traffic,
            },
        );
        Ok(())
    }

    fn drain_held_traffic(&mut self) {
        while let Some(held) = self.held_traffic.front() {
            if !self
                .config
                .budget
                .try_reserve(BudgetClass::Traffic, held.bytes)
            {
                return;
            }
            let held = self
                .held_traffic
                .pop_front()
                .expect("the front was just observed");
            self.config.metrics.queue_depth_changed(
                RecordKind::Traffic,
                -1,
                -held.bytes.cast_signed(),
            );
            self.submit_traffic(held.submission, held.bytes, held.accepted_at);
            if self.failure.is_some() {
                return;
            }
        }
    }

    // ---------------------------------------------------------------- delivery

    fn handle_delivery(&mut self, notice: &DeliveryNotice) {
        // A duplicate delivery callback must be detected from the **deadline arbiter**, not from
        // in-flight bookkeeping: the first report already removed the record, so by the time the
        // second arrives there is nothing left to look up. The arbiter is authoritative because
        // exactly one claim out of `Pending` is permitted, so a claim that lost to an
        // already-`Acknowledged` deadline can only be a second callback for one record. That makes
        // the in-flight accounting retirement depends on unreliable, which no capture-failure
        // policy can recover from.
        if notice.deadline_claimed == Some(false)
            && notice.deadline_status == Some(DeadlineStatus::Acknowledged)
        {
            self.fail(CaptureFailure::corrupted(CorruptedCaptureState::new(
                CorruptionCheck::DuplicateDeliveryCallback,
                format!("submission {}", notice.send_id),
            )));
            return;
        }
        let Some(record) = self.in_flight.remove(&notice.send_id) else {
            // A delivery report for an unknown submission means the failure path already settled
            // it. There is nothing left to do.
            return;
        };
        self.config
            .metrics
            .submit_to_delivery(record.kind, record.submitted_at.elapsed());

        if let Some(Err(corruption)) = self
            .lanes
            .get_mut(&record.writer_partition)
            .map(PublisherLane::remove_in_flight)
        {
            self.fail(CaptureFailure::corrupted(corruption));
            return;
        }

        let outcome = if notice.error.is_none() && notice.deadline_claimed != Some(false) {
            PublishOutcome::Acknowledged
        } else {
            PublishOutcome::Failed
        };
        self.config
            .metrics
            .publication_completed(record.kind, outcome, record.bytes);

        let result = self.settle_delivery(notice, &record);
        self.drain_held_traffic();
        if let Err(failure) = result {
            self.fail(failure);
        }
    }

    fn settle_delivery(
        &mut self,
        notice: &DeliveryNotice,
        record: &InFlightRecord,
    ) -> Result<(), CaptureFailure> {
        if let Some(detail) = &notice.error {
            let failure = CaptureFailure::capture(
                FailureReason::KafkaSend,
                format!(
                    "Kafka reported a send failure for {} on {}: {detail}",
                    record.kind.as_str(),
                    record.writer_partition
                ),
            );
            if let Some((_, ticket, outcomes, _)) = &record.traffic {
                drop(outcomes.send(TrafficOutcome {
                    ticket: *ticket,
                    result: Err(failure.clone()),
                }));
            }
            return Err(failure);
        }

        match record.kind {
            RecordKind::Heartbeat => self.settle_heartbeat(notice, record),
            RecordKind::Traffic => self.settle_traffic(notice, record),
        }
    }

    fn settle_heartbeat(
        &mut self,
        notice: &DeliveryNotice,
        record: &InFlightRecord,
    ) -> Result<(), CaptureFailure> {
        if notice.deadline_claimed == Some(false) {
            // The acknowledgement lost its race. What happens next depends on who won, and in two
            // of the three cases the correct behaviour is to do *nothing*: a late acknowledgement
            // must not update the baseline, activate a generation, or complete retirement.
            match notice.deadline_status {
                Some(DeadlineStatus::Expired) => {
                    tracing::debug!(
                        writer_node_id = %record.writer_partition.writer_node_id(),
                        partition = record.writer_partition.partition(),
                        "event=capture_heartbeat_acknowledged_after_deadline"
                    );
                    if let Some(lane) = self.lanes.get_mut(&record.writer_partition) {
                        lane.abandon_in_flight_heartbeat();
                    }
                    return Ok(());
                }
                Some(DeadlineStatus::Cancelled) => {
                    // DEVIATION, spec discrepancy D-11: Java returns here without completing the
                    // heartbeat's future, so a generation whose lane was cancelled mid-flight can
                    // leave its initial-heartbeat join waiting forever. Latent in Java because the
                    // failure path fails every in-flight future first; resolved explicitly here.
                    if let Some(lane) = self.lanes.get_mut(&record.writer_partition) {
                        lane.abandon_in_flight_heartbeat();
                    }
                    self.cancel_generation_if_awaiting(
                        &record.writer_partition,
                        "the heartbeat's acknowledgement deadline was cancelled before the \
                         acknowledgement arrived",
                    );
                    return Ok(());
                }
                Some(DeadlineStatus::Acknowledged) => {
                    // A second delivery report for one record. The in-flight accounting that
                    // retirement depends on is now unreliable.
                    return Err(CaptureFailure::corrupted(CorruptedCaptureState::new(
                        CorruptionCheck::DuplicateDeliveryCallback,
                        record.writer_partition.to_string(),
                    )));
                }
                Some(DeadlineStatus::Pending) | None => return Ok(()),
            }
        }

        let log_append_time = require_log_append_time(notice.timestamp, "heartbeat")?;
        self.accept_heartbeat_acknowledgement(&record.writer_partition, log_append_time)?;
        self.complete_lane_retirement_if_ready(&record.writer_partition)
    }

    fn settle_traffic(
        &mut self,
        notice: &DeliveryNotice,
        record: &InFlightRecord,
    ) -> Result<(), CaptureFailure> {
        let Some((route, ticket, outcomes, is_terminal)) = &record.traffic else {
            return Ok(());
        };
        let log_append_time = match require_log_append_time(notice.timestamp, "traffic") {
            Ok(time) => time,
            Err(failure) => {
                drop(outcomes.send(TrafficOutcome {
                    ticket: *ticket,
                    result: Err(failure.clone()),
                }));
                return Err(failure);
            }
        };

        if *is_terminal
            && let Err(error) =
                self.with_routing(|routing| routing.remove_after_terminal_acknowledgement(route))
        {
            let failure = error.into_capture_failure();
            drop(outcomes.send(TrafficOutcome {
                ticket: *ticket,
                result: Err(failure.clone()),
            }));
            return Err(failure);
        }

        drop(outcomes.send(TrafficOutcome {
            ticket: *ticket,
            result: Ok(AcknowledgedRecord {
                partition: notice.partition.unwrap_or(route.partition()),
                log_append_time,
            }),
        }));

        if *is_terminal {
            self.begin_drained_writer_retirements()?;
        }
        self.complete_lane_retirement_if_ready(&record.writer_partition)
    }

    fn cancel_generation_if_awaiting(&mut self, writer_partition: &WriterPartition, reason: &str) {
        let affected = self
            .generation_in_progress
            .as_ref()
            .is_some_and(|in_progress| {
                in_progress
                    .pending
                    .writer_partitions()
                    .contains(writer_partition)
            });
        if !affected {
            return;
        }
        if let Some(in_progress) = self.generation_in_progress.take() {
            let failure = CaptureFailure::capture(
                FailureReason::HeartbeatDeadline,
                format!("{writer_partition}: {reason}"),
            );
            drop(in_progress.reply.send(Err(failure)));
        }
    }

    fn handle_deadline_expired(&mut self, deadline: &Arc<AcknowledgementDeadline>) {
        // The gate was already tripped on the clock thread, so the gate listener has queued a
        // `GateTripped` and the failure latch will take care of cleanup. This only records which
        // lane it was, for the health report and the log.
        if let Some(writer_partition) = self.lane_keys.get(&deadline.lane()).cloned() {
            tracing::error!(
                writer_node_id = %writer_partition.writer_node_id(),
                partition = writer_partition.partition(),
                "event=capture_heartbeat_deadline_expired"
            );
        }
    }

    // ---------------------------------------------------------------- retirement

    fn abandon_connection(&mut self, route: &ConnectionRoute) {
        if let Err(error) =
            self.with_routing(|routing| routing.abandon_unpublished_connection(route))
        {
            let failure = error.into_capture_failure();
            self.fail(failure);
            return;
        }
        tracing::warn!(
            writer_node_id = %route.writer_node_id(),
            partition = route.partition(),
            connection_id = %route.connection_id(),
            "event=capture_connection_dropped reason=abandoned_before_publication"
        );
        if let Err(failure) = self.begin_drained_writer_retirements() {
            self.fail(failure);
        }
    }

    fn begin_drained_writer_retirements(&mut self) -> Result<(), CaptureFailure> {
        let drained = self.with_routing(|routing| {
            Ok::<Vec<WriterPartition>, RoutingError>(routing.prepare_drained_writer_retirements())
        });
        let drained = match drained {
            Ok(drained) => drained,
            Err(error) => return Err(error.into_capture_failure()),
        };
        for writer_partition in drained {
            let lane = self.lanes.get_mut(&writer_partition).ok_or_else(|| {
                CaptureFailure::corrupted(CorruptedCaptureState::new(
                    CorruptionCheck::NoPublisherLaneExists,
                    writer_partition.to_string(),
                ))
            })?;
            lane.begin_retiring().map_err(CaptureFailure::corrupted)?;
            // An in-flight heartbeat keeps its deadline armed through the RETIRING transition, so a
            // heartbeat that goes missing during retirement still compromises capture.
            if !lane.heartbeat_in_flight() {
                lane.cancel_acknowledgement_deadline();
            }
            self.complete_lane_retirement_if_ready(&writer_partition)?;
        }
        Ok(())
    }

    fn complete_lane_retirement_if_ready(
        &mut self,
        writer_partition: &WriterPartition,
    ) -> Result<(), CaptureFailure> {
        let has_connections = self
            .with_routing(|routing| {
                Ok::<bool, RoutingError>(routing.has_connections(writer_partition))
            })
            .map_err(RoutingError::into_capture_failure)?;
        let ready = self
            .lanes
            .get(writer_partition)
            .is_some_and(|lane| lane.is_ready_to_retire(has_connections));
        if !ready {
            return Ok(());
        }

        if let Some(lane) = self.lanes.get_mut(writer_partition) {
            lane.cancel_acknowledgement_deadline();
        }
        self.with_routing(|routing| routing.complete_writer_retirement(writer_partition))
            .map_err(RoutingError::into_capture_failure)?;
        if let Some(lane) = self.lanes.get_mut(writer_partition) {
            lane.complete_retirement()
                .map_err(CaptureFailure::corrupted)?;
        }
        self.config.metrics.writer_retired();
        tracing::info!(
            writer_node_id = %writer_partition.writer_node_id(),
            partition = writer_partition.partition(),
            "event=capture_writer_retired"
        );
        self.check_retirement_barrier();
        Ok(())
    }

    fn retire_all_writers(&mut self, reply: mpsc::Sender<Result<(), CaptureFailure>>) {
        if let Some(failure) = self.failure.clone() {
            drop(reply.send(Err(failure)));
            return;
        }
        let retirement = match self.with_routing(RoutingState::begin_orderly_retirement) {
            Ok(retirement) => retirement,
            Err(error) => {
                let failure = error.into_capture_failure();
                // Refusing to retire while connections remain is this layer's job and is reported
                // to the caller, which owns disconnection and timing. But a corrupted state still
                // terminates, and it is latched before the reply so the caller cannot observe a
                // reply without the failure.
                let reported = if failure.always_terminates() {
                    self.fail_canonical(failure)
                } else {
                    failure
                };
                drop(reply.send(Err(reported)));
                return;
            }
        };
        let count = retirement.partitions.len() as u64;
        self.config.metrics.active_writers_began_draining(count);
        if let Some(writer_node_id) = &retirement.writer_node_id {
            tracing::info!(
                writer_node_id = %writer_node_id,
                partitions = ?retirement.partitions,
                "event=capture_writer_draining reason=shutdown"
            );
        }
        self.retirement_reply = Some(reply);
        if let Err(failure) = self.begin_drained_writer_retirements() {
            self.fail(failure);
            return;
        }
        self.check_retirement_barrier();
    }

    fn check_retirement_barrier(&mut self) {
        if self.retirement_reply.is_none() {
            return;
        }
        let all_retired = self
            .with_routing(|routing| {
                Ok::<bool, RoutingError>(routing.all_writer_partitions_retired())
            })
            .unwrap_or(false);
        if !all_retired {
            return;
        }
        if let Some(reply) = self.retirement_reply.take() {
            drop(reply.send(Ok(())));
        }
    }

    // ---------------------------------------------------------------- failure

    /// Fails the publisher and returns the canonical cause.
    ///
    /// Always latch **before** replying to a caller. Otherwise a caller that observes its reply can
    /// race ahead of the failure latch and see a publisher that has not failed yet, which makes the
    /// irreversibility guarantee unobservable at exactly the moment it matters.
    fn fail_canonical(&mut self, failure: CaptureFailure) -> CaptureFailure {
        self.fail(failure.clone());
        self.failure.clone().unwrap_or(failure)
    }

    fn fail(&mut self, failure: CaptureFailure) {
        // Idempotent by the latch: the first failure is canonical and everything that follows is
        // cleanup, not a new decision.
        if self.failure.is_some() {
            return;
        }
        let canonical = self.config.gate.trip(failure);
        self.failure = Some(canonical.clone());
        *self
            .handle
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(canonical.clone());
        self.config.metrics.publisher_failed(canonical.reason());

        tracing::warn!(
            reason = canonical.reason().as_str(),
            "event=capture_write_gate_changed state=closed"
        );
        tracing::error!(
            reason = canonical.reason().as_str(),
            in_flight_records = self.in_flight.len(),
            "event=capture_publisher_failed message=Capture_Kafka_publisher_stopped_permanently"
        );

        // Fail every accepted-but-unsettled publication with the canonical cause, and release its
        // budget. There is no resubmission anywhere: a record whose outcome is unknown stays
        // unknown.
        for (_, record) in std::mem::take(&mut self.in_flight) {
            self.config.metrics.publication_completed(
                record.kind,
                PublishOutcome::Failed,
                record.bytes,
            );
            self.config.budget.release(
                match record.kind {
                    RecordKind::Traffic => BudgetClass::Traffic,
                    RecordKind::Heartbeat => BudgetClass::Control,
                },
                record.bytes,
            );
            if let Some((_, ticket, outcomes, _)) = &record.traffic {
                drop(outcomes.send(TrafficOutcome {
                    ticket: *ticket,
                    result: Err(canonical.clone()),
                }));
            }
        }
        for held in std::mem::take(&mut self.held_traffic) {
            self.config.metrics.queue_depth_changed(
                RecordKind::Traffic,
                -1,
                -held.bytes.cast_signed(),
            );
            drop(held.submission.outcomes.send(TrafficOutcome {
                ticket: held.submission.ticket,
                result: Err(canonical.clone()),
            }));
        }

        for lane in self.lanes.values_mut() {
            lane.cancel_next_heartbeat();
            lane.cancel_acknowledgement_deadline();
        }
        if let Some(in_progress) = self.generation_in_progress.take() {
            drop(in_progress.reply.send(Err(canonical.clone())));
        }
        for request in std::mem::take(&mut self.generation_queue) {
            drop(request.reply.send(Err(canonical.clone())));
        }
        if let Some(reply) = self.retirement_reply.take() {
            drop(reply.send(Err(canonical)));
        }
    }

    fn close(&mut self) {
        self.stop = true;
        self.with_routing(|routing| {
            routing.begin_shutdown();
            Ok::<(), RoutingError>(())
        })
        .ok();
        for lane in self.lanes.values_mut() {
            lane.cancel_next_heartbeat();
            lane.cancel_acknowledgement_deadline();
        }
        if let Err(detail) = self.config.producer.flush(Duration::from_secs(30)) {
            tracing::warn!(detail, "Unable to flush capture Kafka publisher cleanly");
        }
    }

    // ---------------------------------------------------------------- diagnostics

    fn report_health(&mut self) {
        let snapshot = self
            .with_routing(|routing| {
                Ok::<crate::routing_state::DiagnosticSnapshot, RoutingError>(
                    routing.diagnostic_snapshot(),
                )
            })
            .unwrap_or_default();
        let budget = self.config.budget.snapshot();
        let oldest = self
            .in_flight
            .values()
            .map(|record| record.accepted_at)
            .min()
            .map_or(Duration::ZERO, |accepted_at| accepted_at.elapsed());
        self.config.metrics.oldest_unacknowledged_age(oldest);

        tracing::info!(
            active_writers = snapshot.active_writers,
            draining_writers = snapshot.draining_writers,
            retiring_writers = snapshot.retiring_writers,
            retired_writers = snapshot.retired_writers,
            partitions = snapshot.partitions,
            active_connections = snapshot.active_connections,
            in_flight_records = self.in_flight.len(),
            in_flight_bytes = budget.bytes_in_flight,
            oldest_unacknowledged_age_millis = oldest.as_millis(),
            last_heartbeat_broker_time = snapshot
                .last_heartbeat_broker_time
                .map_or_else(|| "none".to_owned(), |time| time.to_string()),
            // Unlike Java's publisherQueueDepth, this is real backlog: it excludes not-yet-due
            // heartbeat timers, which Java counted as queue depth.
            publisher_queue_depth = self.held_traffic.len(),
            publisher_queue_bytes = self.held_traffic.iter().map(|held| held.bytes).sum::<u64>(),
            write_gate = self.config.gate.state_label(),
            "event=capture_health"
        );
    }

    fn with_routing<T, E>(
        &self,
        action: impl FnOnce(&mut RoutingState) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut routing = self
            .config
            .routing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&mut routing)
    }

    fn report_traffic_outcome(
        submission: &TrafficSubmission,
        result: Result<AcknowledgedRecord, CaptureFailure>,
    ) {
        drop(submission.outcomes.send(TrafficOutcome {
            ticket: submission.ticket,
            result,
        }));
    }
}

/// Requires a positive broker-assigned `LogAppendTime`.
///
/// # Owner decision OD-4
///
/// The **type** is checked, not merely the value. `Timestamp::CreateTime` means the broker stored
/// the producer's own clock, which proves nothing about broker time: a proxy comparing its own
/// clock against itself and calling the result a broker-time proof is exactly the unsound state the
/// capture protocol forbids. `rust-rdkafka` exposes the type directly, which the Java client does
/// not — it collapses both into one `long`.
fn require_log_append_time(timestamp: Timestamp, context: &str) -> Result<i64, CaptureFailure> {
    match timestamp {
        Timestamp::LogAppendTime(time) if time > 0 => Ok(time),
        Timestamp::LogAppendTime(time) => Err(CaptureFailure::capture(
            FailureReason::BrokerTime,
            format!(
                "Kafka reported a non-positive LogAppendTime for an acknowledged {context}: {time}"
            ),
        )),
        Timestamp::CreateTime(time) => Err(CaptureFailure::capture(
            FailureReason::BrokerTime,
            format!(
                "Kafka reported CreateTime({time}) for an acknowledged {context}, not \
                 LogAppendTime: the traffic topic must use message.timestamp.type=LogAppendTime, \
                 or the stored timestamp is the proxy's own clock and proves nothing"
            ),
        )),
        Timestamp::NotAvailable => Err(CaptureFailure::capture(
            FailureReason::BrokerTime,
            format!("Kafka reported no timestamp for an acknowledged {context}"),
        )),
    }
}

/// The local wall clock in milliseconds, for the heartbeat's diagnostic `emittedAtMillis` only.
///
/// No decision anywhere reads this value, which is the only reason a wall clock is acceptable.
/// Every freshness and continuity comparison uses broker `LogAppendTime` on both sides.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}
