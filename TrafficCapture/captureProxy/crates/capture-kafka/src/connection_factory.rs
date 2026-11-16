//! Per-connection routing, the Kafka record sink, and the pre-forward gate
//! (traceability rows KAFKA-08, WIRE-04, WIRE-05).
//!
//! # What a connection gets, once
//!
//! A connection is routed exactly once, at accept time, and the resulting [`ConnectionRoute`] is
//! immutable. Every record for that connection then carries the original writer identity and goes
//! to the original partition — including records published long after that generation was
//! superseded. That is what lets a rebalance change only *new*-connection routing while an existing
//! connection's records stay contiguous in one partition, which is the only arrangement the
//! replayer can reassemble.
//!
//! # Refusal before readiness invokes no policy
//!
//! A connection arriving before the first routing generation is usable is **refused**, and the
//! refusal does not touch the capture-failure policy. Java builds this condition *and* calls
//! `failCapture`, so under fail-closed an early connection terminates the process (spec discrepancy
//! D-5) — while `proxyCaptureProtocol.md` §3.2 says the opposite: "a client connecting earlier is
//! refused, and no capture-failure policy is invoked by that refusal." In the real proxy the
//! listener is not bound until readiness resolves, so this is unreachable there; it is reachable for
//! any embedder, and under fail-open where readiness returns early.
//!
//! # The acknowledgement chain stays with the connection
//!
//! Record N+1 for a connection is not handed to Kafka until record N is acknowledged. That
//! ordering lives in `capture-wire`'s [`PublishChain`], owned here, **not** in the publisher actor:
//! the actor's guarantee is a single total order of producer calls, which is weaker and different.
//! Keeping the chain per connection is also what stops connections from queueing behind each other.
//!
//! # The pre-forward gate
//!
//! [`ConnectionCapture::validate_critical_mutation`] is the check that releases a source write. It
//! takes the write gate as an **explicit input** and fails when the gate is closed. Java's
//! equivalent checks only the acknowledgement metadata and the freshness inequality, never the gate
//! (spec discrepancy D-3), so a request whose publication was acknowledged *before* a trip and
//! whose validation runs *after* it passes validation and is forwarded. One atomic load removes the
//! ambiguity.

use std::sync::{Arc, Mutex, mpsc};

use capture_config::CaptureTiming;
use capture_wire::record_buffer::{BufferRecycler, HeapRecycler, RecordBuf};
use capture_wire::{
    EncodedRecord, PublishChain, PublishFailure, PublishTicket, RecordSink, SinkError,
};

use crate::error::CaptureFailure;
use crate::publisher_actor::{AcknowledgedRecord, OutcomeSender, PublisherHandle, TrafficOutcome};
use crate::routing_state::{ConnectionRoute, RoutingError, RoutingState};
use crate::write_gate::WriteGate;

/// Why a connection was not accepted for capture.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConnectionRefusal {
    /// The first routing generation is not usable yet. **Invokes no capture-failure policy.**
    #[error(
        "capture is not accepting new connections before its first routing generation is ready"
    )]
    NotReady,
    /// Retirement or shutdown has begun.
    #[error("capture routing is shutting down")]
    ShuttingDown,
    /// Capture has failed terminally. Under fail-closed the process is terminating; under fail-open
    /// the caller forwards uncaptured.
    #[error("capture is permanently unavailable: {0}")]
    CaptureFailed(CaptureFailure),
    /// The connection id is unusable.
    #[error("{0}")]
    InvalidConnectionId(String),
    /// Internal state corruption. Terminates the process regardless of policy.
    #[error("{0}")]
    Corrupted(CaptureFailure),
}

impl ConnectionRefusal {
    /// Whether this refusal is itself a capture failure the policy must act on.
    ///
    /// [`ConnectionRefusal::NotReady`] and [`ConnectionRefusal::ShuttingDown`] are **not**: they are
    /// ordinary refusals of a connection the proxy was never ready to capture.
    #[must_use]
    pub fn is_capture_failure(&self) -> bool {
        matches!(self, Self::CaptureFailed(_) | Self::Corrupted(_))
    }

    /// The terminal failure, when this refusal carries one.
    #[must_use]
    pub fn failure(&self) -> Option<&CaptureFailure> {
        match self {
            Self::CaptureFailed(failure) | Self::Corrupted(failure) => Some(failure),
            _ => None,
        }
    }
}

/// Creates per-connection capture.
pub struct ConnectionCaptureFactory {
    routing: Arc<Mutex<RoutingState>>,
    publisher: PublisherHandle,
    gate: Arc<WriteGate>,
    timing: CaptureTiming,
    recycler: Arc<dyn BufferRecycler>,
}

impl std::fmt::Debug for ConnectionCaptureFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionCaptureFactory")
            .field("gate", &self.gate.state_label())
            .finish_non_exhaustive()
    }
}

impl ConnectionCaptureFactory {
    /// Builds a factory over a running publisher.
    #[must_use]
    pub fn new(
        routing: Arc<Mutex<RoutingState>>,
        publisher: PublisherHandle,
        gate: Arc<WriteGate>,
        timing: CaptureTiming,
    ) -> Self {
        Self {
            routing,
            publisher,
            gate,
            timing,
            recycler: Arc::new(HeapRecycler),
        }
    }

    /// Uses `recycler` for record buffers, so buffers return to a pool instead of the heap.
    #[must_use]
    pub fn with_recycler(mut self, recycler: Arc<dyn BufferRecycler>) -> Self {
        self.recycler = recycler;
        self
    }

    /// Routes `connection_id` and returns its capture.
    ///
    /// Routing happens synchronously, on the caller's thread, because the caller needs the writer
    /// identity before it can encode the connection's first record.
    ///
    /// # Errors
    ///
    /// See [`ConnectionRefusal`]. Only [`ConnectionRefusal::CaptureFailed`] and
    /// [`ConnectionRefusal::Corrupted`] are capture failures; the rest are plain refusals.
    pub fn create_connection(
        &self,
        connection_id: impl Into<Arc<str>>,
    ) -> Result<ConnectionCapture, ConnectionRefusal> {
        if let Some(failure) = self.publisher.failure() {
            return Err(ConnectionRefusal::CaptureFailed(failure));
        }
        if let Some(failure) = self.gate.failure_if_not_writable() {
            return Err(ConnectionRefusal::CaptureFailed(failure));
        }

        let route = {
            let mut routing = self
                .routing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            routing.route_new_connection(connection_id)
        }
        .map_err(|error| match error {
            RoutingError::NotReady => ConnectionRefusal::NotReady,
            RoutingError::ShuttingDown => ConnectionRefusal::ShuttingDown,
            RoutingError::InvalidArgument(detail) => ConnectionRefusal::InvalidConnectionId(detail),
            other if other.is_corruption() => {
                ConnectionRefusal::Corrupted(other.into_capture_failure())
            }
            other => ConnectionRefusal::CaptureFailed(other.into_capture_failure()),
        })?;

        tracing::debug!(
            writer_node_id = %route.writer_node_id(),
            partition = route.partition(),
            connection_id = %route.connection_id(),
            "event=capture_route_selected"
        );

        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        let outcome_notify = Arc::new(tokio::sync::Notify::new());
        Ok(ConnectionCapture {
            chain: PublishChain::new(),
            sink: KafkaRecordSink {
                route: route.clone(),
                publisher: self.publisher.clone(),
                outcomes: OutcomeSender::new(outcomes_tx, Arc::clone(&outcome_notify)),
                next_ticket: 0,
                recycler: Arc::clone(&self.recycler),
            },
            outcomes: outcomes_rx,
            outcome_notify,
            route,
            routing: Arc::clone(&self.routing),
            gate: Arc::clone(&self.gate),
            timing: self.timing,
            last_acknowledgement: None,
            abandoned: false,
        })
    }
}

/// The Kafka-backed [`RecordSink`] for one connection.
///
/// Separate from [`ConnectionCapture`] so the chain and the sink are disjoint fields, which is what
/// lets the chain dispatch into the sink.
struct KafkaRecordSink {
    route: ConnectionRoute,
    publisher: PublisherHandle,
    outcomes: OutcomeSender,
    next_ticket: u64,
    recycler: Arc<dyn BufferRecycler>,
}

impl RecordSink for KafkaRecordSink {
    fn create_buffer(&mut self, capacity: usize) -> RecordBuf {
        RecordBuf::with_recycler(capacity, Arc::clone(&self.recycler))
    }

    fn submit(&mut self, record: EncodedRecord) -> Result<(), SinkError> {
        self.next_ticket += 1;
        let is_terminal = record.is_final();
        self.publisher
            .publish_traffic(
                self.route.clone(),
                record,
                is_terminal,
                self.next_ticket,
                self.outcomes.clone(),
            )
            .map_err(|failure| {
                // Ownership already transferred: the publisher released the record.
                SinkError::new(format!(
                    "capture publication is unavailable for {}: {failure}",
                    self.route
                ))
            })
    }
}

/// One connection's capture: its immutable route, its acknowledgement chain, and its gate.
pub struct ConnectionCapture {
    chain: PublishChain,
    sink: KafkaRecordSink,
    outcomes: mpsc::Receiver<TrafficOutcome>,
    /// Signalled after every outcome is sent, so an async waiter need not poll.
    outcome_notify: Arc<tokio::sync::Notify>,
    route: ConnectionRoute,
    routing: Arc<Mutex<RoutingState>>,
    gate: Arc<WriteGate>,
    timing: CaptureTiming,
    last_acknowledgement: Option<AcknowledgedRecord>,
    abandoned: bool,
}

impl std::fmt::Debug for ConnectionCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionCapture")
            .field("route", &self.route.to_string())
            .field("in_flight", &self.chain.in_flight())
            .field("queued", &self.chain.queued_len())
            .field("poisoned", &self.chain.is_poisoned())
            .finish_non_exhaustive()
    }
}

impl ConnectionCapture {
    /// This connection's immutable route.
    #[must_use]
    pub fn route(&self) -> &ConnectionRoute {
        &self.route
    }

    /// Woken after every publication outcome for this connection is delivered.
    ///
    /// Await it, then call [`Self::drain_outcomes`]; a wake-up with nothing to drain is harmless.
    #[must_use]
    pub fn outcome_notifier(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.outcome_notify)
    }

    /// Hands a detached record to the connection's chain.
    ///
    /// Records queue behind the in-flight one, so record N+1 never reaches Kafka before N is
    /// acknowledged.
    pub fn submit(&mut self, record: EncodedRecord) -> PublishTicket {
        self.chain.submit(record, &mut self.sink)
    }

    /// Applies every Kafka outcome that has arrived, advancing or poisoning the chain.
    ///
    /// Returns the number of outcomes applied. Never blocks.
    pub fn drain_outcomes(&mut self) -> usize {
        let mut applied = 0;
        while let Ok(outcome) = self.outcomes.try_recv() {
            applied += 1;
            match outcome.result {
                Ok(acknowledged) => {
                    self.last_acknowledgement = Some(acknowledged);
                    self.chain.acknowledge(&mut self.sink);
                }
                Err(failure) => {
                    self.chain.fail(PublishFailure::new(SinkError::new(
                        failure.message().to_owned(),
                    )));
                }
            }
        }
        applied
    }

    /// Waits until an outcome arrives for every submitted record, or `timeout` elapses.
    ///
    /// Returns whether the chain became quiet. Intended for the connection's own close path, where
    /// the caller must know whether its final record was acknowledged.
    pub fn await_quiet(&mut self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.drain_outcomes();
            if self.chain.in_flight().is_none() && self.chain.queued_len() == 0 {
                return true;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.outcomes.recv_timeout(remaining) {
                Ok(outcome) => match outcome.result {
                    Ok(acknowledged) => {
                        self.last_acknowledgement = Some(acknowledged);
                        self.chain.acknowledge(&mut self.sink);
                    }
                    Err(failure) => {
                        self.chain.fail(PublishFailure::new(SinkError::new(
                            failure.message().to_owned(),
                        )));
                    }
                },
                Err(_) => return false,
            }
        }
    }

    /// The most recent acknowledgement for this connection, if any.
    #[must_use]
    pub fn last_acknowledgement(&self) -> Option<AcknowledgedRecord> {
        self.last_acknowledgement
    }

    /// Whether this connection's publication chain has failed permanently.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.chain.is_poisoned()
    }

    /// The pre-forward gate for Critical Mutation Traffic.
    ///
    /// Call this with the acknowledgement of the record that captured the mutation, and forward the
    /// request to the source **only** if it returns `Ok`.
    ///
    /// Three things are checked, in this order:
    ///
    /// 1. the capture write gate is still open — the explicit input Java lacks (spec discrepancy
    ///    D-3);
    /// 2. the acknowledgement names the partition this connection is bound to, which is the only
    ///    place a returned partition is compared against the intended one;
    /// 3. the broker `LogAppendTime` is positive and less than `E` past the writer partition's
    ///    heartbeat baseline, with broker time on both sides and no local clock involved. There is
    ///    deliberately no lower bound: an observation *preceding* the baseline means the liveness
    ///    proof is newer than the record being forwarded (owner decision OD-6 as amended).
    ///
    /// # Errors
    ///
    /// Returns the terminal failure that must be routed to the capture-failure policy. A
    /// corrupted-state failure terminates the process regardless of policy.
    pub fn validate_critical_mutation(
        &self,
        acknowledgement: AcknowledgedRecord,
    ) -> Result<(), CaptureFailure> {
        if let Some(failure) = self.gate.failure_if_not_writable() {
            return Err(failure);
        }
        if acknowledgement.partition != self.route.partition() {
            return Err(CaptureFailure::capture(
                crate::error::FailureReason::BrokerTime,
                format!(
                    "Kafka returned invalid acknowledgement metadata for Critical Mutation \
                     Traffic on {}: acknowledged partition {} is not the connection's partition {}",
                    self.route,
                    acknowledgement.partition,
                    self.route.partition()
                ),
            ));
        }
        let routing = self
            .routing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routing
            .validate_critical_mutation_traffic_acknowledgement(
                &self.route,
                acknowledgement.log_append_time,
                self.timing.heartbeat_expiration(),
            )
            .map_err(RoutingError::into_capture_failure)
    }

    /// Releases a connection that never published anything.
    ///
    /// # Errors
    ///
    /// Returns the terminal failure if the publisher refuses, which happens when any traffic
    /// publication for this connection was already accepted — its records are in Kafka and the
    /// replayer will expect a terminal record.
    pub fn abandon(mut self) -> Result<(), CaptureFailure> {
        self.abandoned = true;
        self.sink.publisher.abandon_connection(self.route.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::error::FailureReason;
    use crate::metrics::NoopMetrics;

    fn timing() -> CaptureTiming {
        CaptureTiming::new(
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .expect("valid timing")
    }

    /// A factory over a routing state with no running publisher, for the refusal paths.
    fn factory(
        routing: Arc<Mutex<RoutingState>>,
        gate: Arc<WriteGate>,
    ) -> ConnectionCaptureFactory {
        // A handle whose actor is already gone: every refusal path below is decided before the
        // publisher is consulted, which is itself worth pinning.
        let (runtime, clock) =
            crate::publisher_actor::start(crate::publisher_actor::PublisherConfig {
                topic: "traffic".to_owned(),
                timing: timing(),
                payload_size_limit: 1024 * 1024,
                routing: Arc::clone(&routing),
                gate: Arc::clone(&gate),
                budget: Arc::new(
                    crate::budget::PublicationBudget::new(crate::budget::BudgetLimits::for_lanes(
                        1024 * 1024,
                        128,
                        4,
                    ))
                    .expect("valid budget"),
                ),
                metrics: Arc::new(NoopMetrics),
                producer: Arc::new(NullProducer),
                refresh_partition_count: Arc::new(|| None),
            })
            .expect("the publisher starts");
        let handle = runtime.handle.clone();
        // The runtime is deliberately leaked for the duration of the test so the handle stays
        // usable; the test process exits long before that matters.
        std::mem::forget(runtime);
        std::mem::forget(clock);
        ConnectionCaptureFactory::new(routing, handle, gate, timing())
    }

    struct NullProducer;

    impl crate::publisher_actor::RecordProducer for NullProducer {
        fn enqueue(
            &self,
            _record: crate::publisher_actor::OutboundRecord,
        ) -> Result<(), crate::publisher_actor::ProduceError> {
            Ok(())
        }

        fn flush(&self, _timeout: Duration) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn a_connection_before_readiness_is_refused_without_invoking_any_policy() {
        // Spec discrepancy D-5: the refusal carries no capture failure.
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 4).expect("valid"),
        ));
        let gate = Arc::new(WriteGate::without_metrics());
        let factory = factory(routing, gate);

        let refusal = factory
            .create_connection("early")
            .expect_err("no generation yet");
        assert!(matches!(refusal, ConnectionRefusal::NotReady));
        assert!(
            !refusal.is_capture_failure(),
            "an early connection must not invoke the capture-failure policy"
        );
        assert!(refusal.failure().is_none());
    }

    #[test]
    fn a_connection_after_shutdown_is_refused_without_invoking_any_policy() {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 4).expect("valid"),
        ));
        routing.lock().expect("test mutex").begin_shutdown();
        let factory = factory(routing, Arc::new(WriteGate::without_metrics()));

        let refusal = factory
            .create_connection("late")
            .expect_err("shutting down");
        assert!(matches!(refusal, ConnectionRefusal::ShuttingDown));
        assert!(!refusal.is_capture_failure());
    }

    #[test]
    fn a_connection_after_a_capture_failure_is_refused_with_that_failure() {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 4).expect("valid"),
        ));
        let gate = Arc::new(WriteGate::without_metrics());
        let cause = CaptureFailure::capture(FailureReason::KafkaSend, "the broker rejected a send");
        gate.trip(cause.clone());
        let factory = factory(routing, Arc::clone(&gate));

        let refusal = factory
            .create_connection("after-failure")
            .expect_err("capture has failed");
        assert!(refusal.is_capture_failure());
        assert!(
            refusal.failure().is_some_and(|f| f.is_same(&cause)),
            "the refusal carries the canonical first cause"
        );
    }

    #[test]
    fn an_empty_connection_id_is_refused_because_partition_locality_needs_a_stable_key() {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 4).expect("valid"),
        ));
        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state.prepare_routing_generation(&[0]).expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        1_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
        }
        let factory = factory(routing, Arc::new(WriteGate::without_metrics()));

        let refusal = factory.create_connection("").expect_err("no id");
        assert!(matches!(
            refusal,
            ConnectionRefusal::InvalidConnectionId(detail) if detail.contains("partition locality")
        ));
    }

    /// Builds an activated routing state and a factory over it.
    fn ready_factory() -> (
        Arc<Mutex<RoutingState>>,
        Arc<WriteGate>,
        ConnectionCaptureFactory,
    ) {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 4).expect("valid"),
        ));
        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state
                .prepare_routing_generation(&[0, 1, 2, 3])
                .expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        1_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
        }
        let gate = Arc::new(WriteGate::without_metrics());
        let factory = factory(Arc::clone(&routing), Arc::clone(&gate));
        (routing, gate, factory)
    }

    #[test]
    fn a_connection_is_routed_once_and_its_route_never_changes() {
        let (routing, _gate, factory) = ready_factory();
        let capture = factory.create_connection("conn-1").expect("ready");
        let route = capture.route().clone();

        // A replacement generation supersedes the first.
        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state
                .prepare_routing_generation(&[0, 1, 2, 3])
                .expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        2_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
        }

        assert_eq!(capture.route().partition(), route.partition());
        assert_eq!(capture.route().writer_node_id(), route.writer_node_id());
        assert_eq!(
            capture.route().writer_node_id(),
            "activation:1",
            "an existing connection keeps the identity it was routed under"
        );

        let fresh = factory.create_connection("conn-2").expect("ready");
        assert_eq!(
            fresh.route().writer_node_id(),
            "activation:2",
            "a new connection uses the current generation"
        );
    }

    #[test]
    fn the_same_connection_id_routes_to_the_same_partition_under_any_generation() {
        // The routing hash is a pure function of the connection id and the sorted partition list,
        // so two generations over the same partitions agree. This is the property the
        // cross-implementation conformance test relies on.
        let (routing, _gate, factory) = ready_factory();
        let first = factory.create_connection("stable-id").expect("ready");
        let first_partition = first.route().partition();

        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state
                .prepare_routing_generation(&[0, 1, 2, 3])
                .expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        2_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
        }
        let second = factory.create_connection("stable-id").expect("ready");
        assert_eq!(
            second.route().partition(),
            first_partition,
            "same id, same sorted partition list, same partition"
        );
        assert_ne!(
            second.route().writer_node_id(),
            first.route().writer_node_id(),
            "but a different writer identity"
        );
    }

    #[test]
    fn the_pre_forward_gate_refuses_once_the_write_gate_is_closed() {
        // Spec discrepancy D-3: Java never consults the gate here, so a request acknowledged
        // before a trip and validated after it is forwarded.
        let (_routing, gate, factory) = ready_factory();
        let capture = factory.create_connection("conn-1").expect("ready");
        let acknowledgement = AcknowledgedRecord {
            partition: capture.route().partition(),
            log_append_time: 1_500,
        };

        capture
            .validate_critical_mutation(acknowledgement)
            .expect("fresh, matching partition, gate open");

        let cause = CaptureFailure::capture(FailureReason::Other, "an unrelated failure");
        gate.trip(cause.clone());
        let failure = capture
            .validate_critical_mutation(acknowledgement)
            .expect_err("the gate is closed");
        assert!(
            failure.is_same(&cause),
            "the pre-forward gate reports the canonical capture failure"
        );
    }

    #[test]
    fn the_pre_forward_gate_requires_the_connection_s_own_partition() {
        let (_routing, _gate, factory) = ready_factory();
        let capture = factory.create_connection("conn-1").expect("ready");
        let wrong = AcknowledgedRecord {
            partition: capture.route().partition() + 1,
            log_append_time: 1_500,
        };
        let failure = capture
            .validate_critical_mutation(wrong)
            .expect_err("the acknowledged partition is not this connection's");
        assert!(
            failure
                .message()
                .contains("invalid acknowledgement metadata")
        );
    }

    #[test]
    fn the_pre_forward_gate_enforces_the_freshness_window_on_broker_time() {
        let (_routing, _gate, factory) = ready_factory();
        let capture = factory.create_connection("conn-1").expect("ready");
        let partition = capture.route().partition();

        for (log_append_time, expected) in [
            (1_000_i64, true),
            (30_999, true),
            (31_000, false),
            (0, false),
            (-1, false),
            // No lower bound (owner decision OD-6 as amended): an observation before the baseline
            // means the liveness proof is newer than the record, which is stronger evidence.
            (996, true),
            (1, true),
        ] {
            let result = capture.validate_critical_mutation(AcknowledgedRecord {
                partition,
                log_append_time,
            });
            assert_eq!(
                result.is_ok(),
                expected,
                "observation {log_append_time} against baseline 1000 with E=30s, S=5s"
            );
        }
    }
}
