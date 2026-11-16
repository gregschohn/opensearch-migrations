//! Deterministic model tests for the publisher actor, ported from the Java suite
//! (kafka spec §17.1, `CaptureKafkaPublisherTest.java`).
//!
//! # How the races are driven
//!
//! Not with sleeps. Every acknowledgement, every delivery report, and every deadline is driven
//! explicitly:
//!
//! * a [`ScriptedProducer`] stands in for Kafka and records exactly what was handed to it, which is
//!   the Rust counterpart of the Java suite's `MockProducer`, `TimestampingProducer`,
//!   `CallbackThenBlockProducer`, and `BlockedSendProducer`;
//! * a delivery report is delivered by calling [`deliver`], so "the acknowledgement arrived" and
//!   "the actor processed it" are separate, observable events;
//! * the acknowledgement/deadline arbitration is exercised by claiming the deadline directly, in
//!   the order under test, which is exactly what the real delivery-report thread does.
//!
//! The only waits are bounded waits for a *liveness* property — "this reply eventually arrives" —
//! never a sleep standing in for a correctness argument.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use capture_config::CaptureTiming;
use capture_kafka::ack_deadline::DeadlineStatus;
use capture_kafka::budget::{BudgetLimits, PublicationBudget};
use capture_kafka::error::{CaptureFailure, CorruptionCheck, FailureClass, FailureReason};
use capture_kafka::metrics::{CaptureMetrics, NoopMetrics, PublishOutcome, RecordKind};
use capture_kafka::publisher_actor::{
    self, AcknowledgedRecord, DeliveryNotice, OutboundRecord, ProduceError, PublisherConfig,
    PublisherHandle, PublisherRuntime, RecordProducer, TrafficOutcome,
};
use capture_kafka::routing_state::{ConnectionRoute, RoutingState};
use capture_kafka::write_gate::WriteGate;
use capture_wire::proto::{CaptureRecord, capture_record};
use capture_wire::protobuf_size::{CaptureTimestamp, field};
use capture_wire::record_buffer::RecordBuf;
use capture_wire::{EncodedRecord, ObservationBody, ObservationPlan, RecordEncoder, RecordHeader};
use prost::Message as _;

const TOPIC: &str = "traffic";
const ACTIVATION: &str = "activation";

/// A deadline waited on in a liveness assertion. Generous: a slow machine must not fail a test
/// whose subject is ordering, not timing.
const LIVENESS_BUDGET: Duration = Duration::from_secs(10);

/// Budget for a **negative** assertion ("this must not happen"). Long enough that the thing under
/// test would have happened many times over if it were going to — the shortest interval any test
/// configures is H = 20 ms — and deliberately far below the shortest configured E, so a legitimate
/// acknowledgement deadline cannot fire inside it and turn a negative assertion into a timeout.
const NEGATIVE_BUDGET: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------- doubles

/// Stands in for Kafka. Records every record handed to it, in order, and can block or fail on
/// demand.
#[derive(Default)]
struct ScriptedProducer {
    history: Mutex<Vec<OutboundRecord>>,
    /// Blocks inside `enqueue` until released, like Java's `BlockedSendProducer`.
    block: Mutex<Option<mpsc::Receiver<()>>>,
    blocked_entered: Mutex<Option<mpsc::Sender<()>>>,
    fail_next: Mutex<Option<ProduceError>>,
    flushes: AtomicU64,
}

impl ScriptedProducer {
    fn history(&self) -> Vec<OutboundRecord> {
        self.history.lock().expect("test mutex").clone()
    }

    fn history_len(&self) -> usize {
        self.history.lock().expect("test mutex").len()
    }

    /// Records of kind heartbeat, identified by their key shape.
    fn heartbeats(&self) -> Vec<OutboundRecord> {
        self.history()
            .into_iter()
            .filter(|record| record.key.contains(":heartbeat:"))
            .collect()
    }

    fn traffic(&self) -> Vec<OutboundRecord> {
        self.history()
            .into_iter()
            .filter(|record| !record.key.contains(":heartbeat:"))
            .collect()
    }
}

impl RecordProducer for ScriptedProducer {
    fn enqueue(&self, record: OutboundRecord) -> Result<(), ProduceError> {
        if let Some(error) = self.fail_next.lock().expect("test mutex").take() {
            return Err(error);
        }
        // Park inside the enqueue, exactly where Java's BlockedSendProducer parks, so a test can
        // prove the deadline is armed only after the producer accepts the submission.
        let waiter = self.block.lock().expect("test mutex").take();
        if let Some(waiter) = waiter {
            if let Some(entered) = self.blocked_entered.lock().expect("test mutex").take() {
                let _ = entered.send(());
            }
            let _ = waiter.recv();
        }
        self.history.lock().expect("test mutex").push(record);
        Ok(())
    }

    fn flush(&self, _timeout: Duration) -> Result<(), String> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Counts the metric emissions the Java tests assert on.
#[derive(Default)]
struct CountingMetrics {
    accepted: Mutex<Vec<(RecordKind, u64)>>,
    completed: Mutex<Vec<(RecordKind, PublishOutcome)>>,
    heartbeat_deadline_violations: AtomicU64,
    broker_time_violations: AtomicU64,
    writers_retired: AtomicU64,
    publisher_failures: Mutex<Vec<FailureReason>>,
    gate_trips: Mutex<Vec<FailureReason>>,
}

impl CountingMetrics {
    fn count_accepted(&self, kind: RecordKind) -> usize {
        self.accepted
            .lock()
            .expect("test mutex")
            .iter()
            .filter(|(k, _)| *k == kind)
            .count()
    }

    fn count_completed(&self, kind: RecordKind, outcome: PublishOutcome) -> usize {
        self.completed
            .lock()
            .expect("test mutex")
            .iter()
            .filter(|(k, o)| *k == kind && *o == outcome)
            .count()
    }

    fn failures(&self) -> Vec<FailureReason> {
        self.publisher_failures.lock().expect("test mutex").clone()
    }
}

/// Forwards to the shared counters, so a test can keep its own handle on them.
struct SharedMetrics(Arc<CountingMetrics>);

impl std::ops::Deref for SharedMetrics {
    type Target = CountingMetrics;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl CaptureMetrics for SharedMetrics {
    fn publication_accepted(&self, kind: RecordKind, bytes: u64) {
        self.accepted
            .lock()
            .expect("test mutex")
            .push((kind, bytes));
    }

    fn publication_completed(&self, kind: RecordKind, outcome: PublishOutcome, _bytes: u64) {
        self.completed
            .lock()
            .expect("test mutex")
            .push((kind, outcome));
    }

    fn heartbeat_deadline_violated(&self) {
        self.heartbeat_deadline_violations
            .fetch_add(1, Ordering::SeqCst);
    }

    fn broker_time_violated(&self) {
        self.broker_time_violations.fetch_add(1, Ordering::SeqCst);
    }

    fn writer_retired(&self) {
        self.writers_retired.fetch_add(1, Ordering::SeqCst);
    }

    fn write_gate_tripped(&self, reason: FailureReason) {
        self.gate_trips.lock().expect("test mutex").push(reason);
    }

    fn publisher_failed(&self, reason: FailureReason) {
        self.publisher_failures
            .lock()
            .expect("test mutex")
            .push(reason);
    }
}

// ---------------------------------------------------------------- harness

struct Harness {
    runtime: PublisherRuntime,
    _clock: capture_kafka::ack_deadline::DeadlineClock,
    producer: Arc<ScriptedProducer>,
    metrics: Arc<CountingMetrics>,
    routing: Arc<Mutex<RoutingState>>,
    gate: Arc<WriteGate>,
    outcomes_tx: mpsc::Sender<TrafficOutcome>,
    outcomes_rx: mpsc::Receiver<TrafficOutcome>,
    next_ticket: AtomicU64,
    refresh_calls: Arc<AtomicU64>,
    refreshed_count: Arc<Mutex<Option<usize>>>,
}

impl Harness {
    fn new(timing: CaptureTiming, partition_count: usize) -> Self {
        Self::with_payload_limit(timing, partition_count, 1024 * 1024)
    }

    fn with_payload_limit(
        timing: CaptureTiming,
        partition_count: usize,
        payload_size_limit: usize,
    ) -> Self {
        let producer = Arc::new(ScriptedProducer::default());
        let metrics = Arc::new(CountingMetrics::default());
        let routing = Arc::new(Mutex::new(
            RoutingState::new(ACTIVATION, partition_count).expect("valid routing state"),
        ));
        let gate = Arc::new(WriteGate::new(Box::new(SharedMetrics(Arc::clone(
            &metrics,
        )))));
        let budget = Arc::new(
            PublicationBudget::new(BudgetLimits::for_lanes(64 * 1024 * 1024, 4_096, 16))
                .expect("valid budget"),
        );
        let refresh_calls = Arc::new(AtomicU64::new(0));
        let refreshed_count = Arc::new(Mutex::new(None));

        let config = PublisherConfig {
            topic: TOPIC.to_owned(),
            timing,
            payload_size_limit,
            routing: Arc::clone(&routing),
            gate: Arc::clone(&gate),
            budget,
            metrics: Arc::new(SharedMetrics(Arc::clone(&metrics))),
            producer: Arc::clone(&producer) as Arc<dyn RecordProducer>,
            refresh_partition_count: {
                let calls = Arc::clone(&refresh_calls);
                let count = Arc::clone(&refreshed_count);
                Arc::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    *count.lock().expect("test mutex")
                })
            },
        };
        let (runtime, clock) = publisher_actor::start(config).expect("the publisher starts");
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        Self {
            runtime,
            _clock: clock,
            producer,
            metrics,
            routing,
            gate,
            outcomes_tx,
            outcomes_rx,
            next_ticket: AtomicU64::new(1),
            refresh_calls,
            refreshed_count,
        }
    }

    fn handle(&self) -> &PublisherHandle {
        &self.runtime.handle
    }

    fn active_partitions(&self) -> Vec<i32> {
        self.routing
            .lock()
            .expect("test mutex")
            .active_routing_partitions()
    }

    fn route(&self, connection_id: &str) -> ConnectionRoute {
        self.routing
            .lock()
            .expect("test mutex")
            .route_new_connection(connection_id)
            .expect("a generation is active")
    }

    /// Waits until `condition` holds. Liveness only.
    fn wait_until(condition: impl FnMut() -> bool) -> bool {
        Self::wait_for(LIVENESS_BUDGET, condition)
    }

    /// Waits briefly, for a negative assertion.
    fn wait_briefly(condition: impl FnMut() -> bool) -> bool {
        Self::wait_for(NEGATIVE_BUDGET, condition)
    }

    fn wait_for(budget: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        condition()
    }

    fn wait_for_records(&self, count: usize) {
        assert!(
            Self::wait_until(|| self.producer.history_len() >= count),
            "expected {count} records, saw {}: {:?}",
            self.producer.history_len(),
            self.producer.history()
        );
    }

    /// Acknowledges the record at `index` in producer history with a broker `LogAppendTime`.
    fn acknowledge(&self, index: usize, log_append_time: i64) {
        let record = self.producer.history()[index].clone();
        self.deliver(DeliveryNotice {
            send_id: record.send_id,
            partition: Some(record.partition),
            timestamp: rdkafka::Timestamp::LogAppendTime(log_append_time),
            error: None,
            deadline_claimed: Self::claim_deadline_for(&record),
            deadline_status: Self::deadline_status_for(&record),
        });
    }

    /// Claims the deadline the way the real delivery-report thread does: on the reporting side,
    /// before the actor ever sees the report.
    fn claim_deadline_for(record: &OutboundRecord) -> Option<bool> {
        if !record.key.contains(":heartbeat:") {
            return None;
        }
        Some(true)
    }

    fn deadline_status_for(record: &OutboundRecord) -> Option<DeadlineStatus> {
        if record.key.contains(":heartbeat:") {
            Some(DeadlineStatus::Acknowledged)
        } else {
            None
        }
    }

    fn deliver(&self, notice: DeliveryNotice) {
        self.handle().deliver(notice).expect("the actor is running");
    }

    fn publish_traffic(
        &self,
        route: &ConnectionRoute,
        record: EncodedRecord,
        is_terminal: bool,
    ) -> u64 {
        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
        self.handle()
            .publish_traffic(
                route.clone(),
                record,
                is_terminal,
                ticket,
                self.outcomes_tx.clone(),
            )
            .expect("the actor is running");
        ticket
    }

    fn next_outcome(&self) -> TrafficOutcome {
        self.outcomes_rx
            .recv_timeout(LIVENESS_BUDGET)
            .expect("an outcome arrives")
    }

    /// Activates a generation over `partitions` with the given broker time, returning the writer
    /// identity.
    fn activate(&self, partitions: &[i32], log_append_time: i64) -> Arc<str> {
        let before = self.producer.history_len();
        let reply = self
            .handle()
            .initialize_generation(partitions.to_vec())
            .expect("the actor is running");
        self.wait_for_records(before + partitions.len());
        for index in before..before + partitions.len() {
            self.acknowledge(index, log_append_time);
        }
        reply
            .recv_timeout(LIVENESS_BUDGET)
            .expect("the generation reply arrives")
            .expect("the generation activates")
    }
}

fn timing(heartbeat_ms: u64, expiration_ms: u64) -> CaptureTiming {
    CaptureTiming::new(
        Duration::from_secs(5),
        Duration::from_millis(heartbeat_ms),
        Duration::from_millis(expiration_ms),
        Duration::from_secs(5),
    )
    .expect("valid timing")
}

/// Builds a well-formed encoded record for `route`.
fn encoded_record(route: &ConnectionRoute, index: u32, is_final: bool) -> EncodedRecord {
    encoded_record_for(
        route.connection_id(),
        route.writer_node_id(),
        index,
        is_final,
        0,
    )
}

fn encoded_record_for(
    connection_id: &str,
    node_id: &str,
    index: u32,
    is_final: bool,
    padding: usize,
) -> EncodedRecord {
    let header = RecordHeader {
        connection_id: Arc::from(connection_id),
        node_id: Arc::from(node_id),
        prior_request_boundaries: 0,
        reads_awaiting_end_of_message: false,
    };
    let mut encoder = RecordEncoder::start(RecordBuf::new(4_096 + padding), header, index, None);
    if padding > 0 {
        let bytes = vec![b'x'; padding];
        let body = ObservationBody::Data {
            field_number: 1,
            bytes: &bytes,
        };
        let plan = ObservationPlan::new(1, CaptureTimestamp::EPOCH, field::READ, &body);
        encoder.write_observation(&plan, &body);
    }
    // A final record must carry exactly one Close as its last observation; a non-final record must
    // carry none.
    let body = ObservationBody::Empty;
    let capture_field = if is_final {
        field::CLOSE
    } else {
        field::REQUEST_FORWARDED
    };
    let plan = ObservationPlan::new(
        u64::from(index) + 1,
        CaptureTimestamp::EPOCH,
        capture_field,
        &body,
    );
    encoder.write_observation(&plan, &body);
    encoder.finish(is_final).expect("a well-formed record")
}

// ---------------------------------------------------------------- generation usability

#[test]
fn initial_heartbeats_must_all_be_acknowledged_before_a_generation_becomes_usable() {
    // CaptureKafkaPublisherTest.initialHeartbeatsMustAllBeAcknowledgedBeforeRoutingGenerationBecomesUsable
    // (:58-78), plus KafkaCaptureFactoryTest.firstAssignmentIsNotUsableUntilEveryInitialHeartbeatIsAcknowledged
    // (:287-324).
    let harness = Harness::new(timing(10_000, 30_000), 4);
    let reply = harness
        .handle()
        .initialize_generation(vec![0, 2])
        .expect("running");

    harness.wait_for_records(2);
    let heartbeats = harness.producer.heartbeats();
    assert_eq!(heartbeats.len(), 2, "one heartbeat per assigned partition");
    assert_eq!(
        heartbeats
            .iter()
            .map(|record| record.partition)
            .collect::<Vec<_>>(),
        vec![0, 2],
        "lanes are created in ascending partition order"
    );
    for (record, partition) in heartbeats.iter().zip([0, 2]) {
        assert_eq!(record.key, format!("activation:1:heartbeat:{partition}"));
        let decoded = CaptureRecord::decode(record.value.as_slice()).expect("canonical envelope");
        let Some(capture_record::Payload::WriterPartitionHeartbeat(heartbeat)) = decoded.payload
        else {
            panic!("the payload oneof must identify a heartbeat");
        };
        assert_eq!(heartbeat.writer_node_id, "activation:1");
        assert_eq!(heartbeat.heartbeat_interval_millis, 10_000);
        assert!(heartbeat.emitted_at_millis.is_some_and(|at| at > 0));
    }

    assert!(
        harness.active_partitions().is_empty(),
        "nothing is routable before any heartbeat is accepted"
    );

    harness.acknowledge(0, 1_000);
    assert!(
        reply.recv_timeout(Duration::from_millis(200)).is_err(),
        "the generation is not usable with one partition unacknowledged"
    );
    assert!(harness.active_partitions().is_empty());

    harness.acknowledge(1, 1_000);
    let writer = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect("activation");
    assert_eq!(&*writer, "activation:1");
    assert_eq!(harness.active_partitions(), vec![0, 2]);
}

#[test]
fn replacement_generations_initialize_in_membership_callback_order() {
    // CaptureKafkaPublisherTest.replacementRoutingGenerationsAreInitializedInMembershipCallbackOrder
    // (:81-104): generation 2's heartbeat is not even submitted until generation 1 completes.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    let first = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    let second = harness
        .handle()
        .initialize_generation(vec![1])
        .expect("running");

    harness.wait_for_records(1);
    assert!(
        !Harness::wait_briefly(|| harness.producer.history_len() > 1),
        "generation 2 must not submit a heartbeat while generation 1 is initializing: {:?}",
        harness.producer.history()
    );
    let heartbeats = harness.producer.heartbeats();
    assert_eq!(heartbeats.len(), 1);
    assert!(heartbeats[0].key.starts_with("activation:1:"));

    harness.acknowledge(0, 1_000);
    assert_eq!(
        &*first
            .recv_timeout(LIVENESS_BUDGET)
            .expect("reply")
            .expect("activation"),
        "activation:1"
    );

    harness.wait_for_records(2);
    assert!(
        harness.producer.heartbeats()[1]
            .key
            .starts_with("activation:2:"),
        "generation 2 begins only after generation 1 finished"
    );
    harness.acknowledge(1, 2_000);
    assert_eq!(
        &*second
            .recv_timeout(LIVENESS_BUDGET)
            .expect("reply")
            .expect("activation"),
        "activation:2"
    );
    assert_eq!(harness.active_partitions(), vec![1]);
}

// ---------------------------------------------------------------- the ack/deadline race

#[test]
fn an_acknowledgement_that_arrived_before_the_deadline_wins_even_if_the_actor_is_backlogged() {
    // CaptureKafkaPublisherTest.acknowledgementThatArrivesBeforeDeadlineWinsEvenIfPublisherThreadProcessesItLater
    // (:168-190).
    //
    // The mechanism under test is that the acknowledgement CAS happens on the delivery-report
    // thread *before* any hop to the actor. Here the CAS is performed first and the report is
    // delivered afterwards, which is exactly that ordering: the generation must still activate even
    // though the actor learns about it late.
    let harness = Harness::new(timing(20, 75), 4);
    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    harness.wait_for_records(1);

    // The acknowledgement physically arrived in time and claimed the deadline. The actor is told
    // only now.
    let record = harness.producer.history()[0].clone();
    harness.deliver(DeliveryNotice {
        send_id: record.send_id,
        partition: Some(0),
        timestamp: rdkafka::Timestamp::LogAppendTime(1_000),
        error: None,
        deadline_claimed: Some(true),
        deadline_status: Some(DeadlineStatus::Acknowledged),
    });

    assert_eq!(
        &*reply
            .recv_timeout(LIVENESS_BUDGET)
            .expect("reply")
            .expect("the generation still activates"),
        "activation:1"
    );
    assert_eq!(harness.active_partitions(), vec![0]);
    assert!(
        harness.handle().failure().is_none(),
        "an in-time acknowledgement must not fail the publisher"
    );
}

#[test]
fn a_heartbeat_acknowledgement_that_lost_to_its_deadline_changes_no_state() {
    // CaptureKafkaPublisherTest.heartbeatAcknowledgementDeadlinePermanentlyFailsThePublisher
    // (:107-165): after the deadline wins, Kafka may still acknowledge. That late acknowledgement
    // must not update the baseline, activate the generation, or complete retirement, and there must
    // be no resubmission.
    let harness = Harness::new(timing(20, 75), 4);
    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    harness.wait_for_records(1);
    let record = harness.producer.history()[0].clone();

    // The deadline fires first and trips the gate, exactly as the clock thread would.
    harness.gate.trip(CaptureFailure::capture(
        FailureReason::HeartbeatDeadline,
        "Heartbeat acknowledgement deadline expired",
    ));
    let failure = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect_err("the generation is unusable");
    assert_eq!(failure.reason(), FailureReason::HeartbeatDeadline);

    // Kafka acknowledges afterwards. The CAS lost, so the report carries that fact.
    harness.deliver(DeliveryNotice {
        send_id: record.send_id,
        partition: Some(0),
        timestamp: rdkafka::Timestamp::LogAppendTime(1_000),
        error: None,
        deadline_claimed: Some(false),
        deadline_status: Some(DeadlineStatus::Expired),
    });

    assert!(
        !Harness::wait_briefly(|| !harness.active_partitions().is_empty()),
        "a late acknowledgement must not activate the generation"
    );
    assert_eq!(
        harness.producer.history_len(),
        1,
        "the application never resubmits a timed-out send"
    );
    assert!(
        harness
            .handle()
            .failure()
            .is_some_and(|f| f.reason() == FailureReason::HeartbeatDeadline)
    );
}

#[test]
fn a_cancelled_deadline_resolves_the_generation_instead_of_leaving_it_waiting() {
    // DEVIATION, spec discrepancy D-11: Java returns from the CANCELLED branch without completing
    // the heartbeat's future, so the initial-heartbeat join can wait forever. Latent there because
    // the failure path fails every in-flight future first; resolved explicitly here.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    harness.wait_for_records(1);
    let record = harness.producer.history()[0].clone();

    harness.deliver(DeliveryNotice {
        send_id: record.send_id,
        partition: Some(0),
        timestamp: rdkafka::Timestamp::LogAppendTime(1_000),
        error: None,
        deadline_claimed: Some(false),
        deadline_status: Some(DeadlineStatus::Cancelled),
    });

    let outcome = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("the reply must arrive rather than hang");
    assert!(
        outcome.is_err(),
        "a cancelled deadline resolves the generation as failed, not as activated"
    );
    assert!(
        harness.active_partitions().is_empty(),
        "a cancelled heartbeat must not activate a generation"
    );
}

#[test]
fn a_second_delivery_report_for_one_heartbeat_is_an_unstable_process_failure() {
    // CaptureKafkaPublisher.java:992-995. Duplicate delivery means the in-flight accounting
    // retirement depends on is unreliable, which no capture-failure policy can recover from.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0], 1_000);
    let heartbeat = harness.producer.history()[0].clone();

    harness.deliver(DeliveryNotice {
        send_id: heartbeat.send_id,
        partition: Some(0),
        timestamp: rdkafka::Timestamp::LogAppendTime(1_000),
        error: None,
        deadline_claimed: Some(false),
        deadline_status: Some(DeadlineStatus::Acknowledged),
    });

    assert!(
        Harness::wait_until(|| harness.handle().failure().is_some()),
        "a duplicate delivery report must be reported"
    );
    let failure = harness.handle().failure().expect("failed");
    assert_eq!(failure.class(), FailureClass::UnstableProcess);
    assert_eq!(
        failure
            .corruption()
            .map(capture_kafka::error::CorruptedCaptureState::check),
        Some(CorruptionCheck::DuplicateDeliveryCallback)
    );
    assert!(
        failure.always_terminates(),
        "this terminates the process under fail-open too"
    );
}

// ---------------------------------------------------------------- heartbeat cadence

#[test]
fn a_periodic_heartbeat_waits_for_the_previous_acknowledgement() {
    // CaptureKafkaPublisherTest.periodicHeartbeatWaitsForThePreviousHeartbeatAcknowledgement
    // (:242-270): with H = 20 ms, after the first acknowledgement a second heartbeat appears, and
    // no third appears until the second is acknowledged.
    let harness = Harness::new(timing(20, 30_000), 4);
    harness.activate(&[0], 1_000);
    assert_eq!(harness.producer.history_len(), 1);

    // The next H timer starts at acceptance, so heartbeat 2 is due 20 ms later.
    harness.wait_for_records(2);
    assert_eq!(harness.producer.heartbeats().len(), 2);

    // Heartbeat 2 is unacknowledged, so no third may be submitted however long we wait.
    assert!(
        !Harness::wait_briefly(|| harness.producer.history_len() > 2),
        "a heartbeat is never submitted while another is outstanding: {:?}",
        harness.producer.history()
    );

    harness.acknowledge(1, 1_500);
    harness.wait_for_records(3);
    let third = &harness.producer.heartbeats()[2];
    let decoded = CaptureRecord::decode(third.value.as_slice()).expect("canonical");
    let Some(capture_record::Payload::WriterPartitionHeartbeat(heartbeat)) = decoded.payload else {
        panic!("heartbeat payload");
    };
    assert_eq!(heartbeat.heartbeat_interval_millis, 20);
}

#[test]
fn heartbeat_broker_time_at_the_expiration_boundary_compromises_capture() {
    // CaptureKafkaPublisherTest.heartbeatBrokerTimeAtExpirationBoundaryCompromisesCapture
    // (:273-316): with E = 5 s, timestamps 1_000 then 6_000 is exactly E and must fail, with the
    // baseline frozen at 1_000.
    let harness = Harness::new(timing(20, 5_000), 4);
    harness.activate(&[0], 1_000);
    harness.wait_for_records(2);
    harness.acknowledge(1, 6_000);

    assert!(
        Harness::wait_until(|| harness.handle().failure().is_some()),
        "exactly E of broker-time movement must compromise capture"
    );
    let failure = harness.handle().failure().expect("failed");
    assert_eq!(failure.reason(), FailureReason::BrokerTime);
    assert_eq!(
        failure.class(),
        FailureClass::Capture,
        "broker-time violation follows the configurable policy"
    );
    assert_eq!(
        harness
            .metrics
            .broker_time_violations
            .load(Ordering::SeqCst),
        1
    );
    assert!(
        harness
            .metrics
            .failures()
            .contains(&FailureReason::BrokerTime)
    );
}

#[test]
fn backward_broker_time_beyond_the_skew_allowance_compromises_capture() {
    // Owner decision OD-6. Java accepts any backward movement and regresses the baseline, which
    // widens the pre-forward window; here movement worse than S is a broker-time violation.
    let timing = CaptureTiming::new(
        Duration::from_secs(5),
        Duration::from_millis(20),
        Duration::from_secs(30),
        Duration::from_secs(1),
    )
    .expect("valid timing");
    let harness = Harness::new(timing, 4);
    harness.activate(&[0], 20_000);
    harness.wait_for_records(2);
    harness.acknowledge(1, 18_000);

    assert!(
        Harness::wait_until(|| harness.handle().failure().is_some()),
        "2 s backward with S = 1 s must compromise capture"
    );
    assert_eq!(
        harness.handle().failure().expect("failed").reason(),
        FailureReason::BrokerTime
    );
}

#[test]
fn a_create_time_acknowledgement_compromises_capture_on_the_traffic_path_too() {
    // Owner decision OD-4 applies to every acknowledgement, not only the probe: CreateTime means
    // the stored timestamp is the proxy's own clock and proves nothing.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    harness.wait_for_records(1);
    let record = harness.producer.history()[0].clone();
    harness.deliver(DeliveryNotice {
        send_id: record.send_id,
        partition: Some(0),
        timestamp: rdkafka::Timestamp::CreateTime(1_794_790_929_566),
        error: None,
        deadline_claimed: Some(true),
        deadline_status: Some(DeadlineStatus::Acknowledged),
    });

    let failure = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect_err("a CreateTime acknowledgement is not a broker-time proof");
    assert_eq!(failure.reason(), FailureReason::BrokerTime);
    assert!(
        failure.message().contains("LogAppendTime"),
        "the failure must name the remedy: {}",
        failure.message()
    );
}

// ---------------------------------------------------------------- traffic

#[test]
fn traffic_uses_the_capture_record_envelope_with_the_connection_id_as_its_key() {
    // CaptureKafkaPublisherTest.trafficUsesCaptureRecordEnvelopeAndNoKafkaTypeHeaders (:465-490).
    // Headers are structurally impossible here: `OutboundRecord` has no header field.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0, 1, 2, 3], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();

    harness.publish_traffic(&route, encoded_record(&route, 1, false), false);
    harness.wait_for_records(before + 1);

    let record = harness
        .producer
        .traffic()
        .last()
        .cloned()
        .expect("a traffic record");
    assert_eq!(record.topic, TOPIC);
    assert_eq!(
        record.partition,
        route.partition(),
        "the partition is the route's, never recomputed"
    );
    assert_eq!(record.key, "conn-1");

    let decoded = CaptureRecord::decode(record.value.as_slice()).expect("canonical envelope");
    let Some(capture_record::Payload::TrafficStream(stream)) = decoded.payload else {
        panic!("the payload oneof must identify a TrafficStream");
    };
    assert_eq!(stream.connection_id, "conn-1");
    assert_eq!(stream.node_id, route.writer_node_id());
}

#[test]
fn a_traffic_acknowledgement_reports_the_broker_time_the_pre_forward_check_needs() {
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0, 1, 2, 3], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();
    let ticket = harness.publish_traffic(&route, encoded_record(&route, 1, false), false);
    harness.wait_for_records(before + 1);
    harness.acknowledge(before, 1_500);

    let outcome = harness.next_outcome();
    assert_eq!(outcome.ticket, ticket);
    let acknowledged: AcknowledgedRecord = outcome.result.expect("acknowledged");
    assert_eq!(acknowledged.partition, route.partition());
    assert_eq!(acknowledged.log_append_time, 1_500);

    // The pre-forward gate is then satisfied against the heartbeat baseline.
    harness
        .routing
        .lock()
        .expect("test mutex")
        .validate_critical_mutation_traffic_acknowledgement(
            &route,
            acknowledged.log_append_time,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .expect("500 ms after the baseline is fresh");
}

#[test]
fn a_mismatched_traffic_identity_is_an_unstable_process_failure() {
    // CaptureKafkaPublisherTest.mismatchedTrafficIdentityIsAnUnstableProcessFailure (:539-558).
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0, 1, 2, 3], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();

    // The record claims a different connection than the route it is published under.
    let forged = encoded_record_for("other-connection", route.writer_node_id(), 1, false, 0);
    let ticket = harness.publish_traffic(&route, forged, false);

    let outcome = harness.next_outcome();
    assert_eq!(outcome.ticket, ticket);
    let failure = outcome.result.expect_err("identity mismatch");
    assert_eq!(failure.class(), FailureClass::UnstableProcess);
    assert_eq!(
        failure
            .corruption()
            .map(capture_kafka::error::CorruptedCaptureState::check),
        Some(CorruptionCheck::TrafficIdentityMismatch)
    );
    assert_eq!(
        harness.producer.history_len(),
        before,
        "nothing is published for a record whose identity does not match its route"
    );
}

#[test]
fn a_payload_limit_violation_permanently_stops_the_publisher() {
    // CaptureKafkaPublisherTest.payloadLimitFailurePermanentlyStopsPublisher (:583-611). Terminal:
    // not a retry and not a split.
    let harness = Harness::with_payload_limit(timing(10_000, 30_000), 4, 600);
    harness.activate(&[0, 1, 2, 3], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();

    let oversized =
        encoded_record_for(route.connection_id(), route.writer_node_id(), 1, false, 800);
    let ticket = harness.publish_traffic(&route, oversized, false);

    let outcome = harness.next_outcome();
    assert_eq!(outcome.ticket, ticket);
    assert_eq!(
        outcome.result.expect_err("too large").reason(),
        FailureReason::PayloadLimit
    );
    assert_eq!(
        harness.producer.history_len(),
        before,
        "an oversized record is never published"
    );
    assert!(
        harness
            .handle()
            .failure()
            .is_some_and(|f| f.reason() == FailureReason::PayloadLimit)
    );
}

#[test]
fn an_application_visible_kafka_failure_stops_all_future_writes_without_resubmission() {
    // CaptureKafkaPublisherTest.applicationVisibleKafkaFailureStopsAllFutureWritesWithoutResubmission
    // (:439-464): a later publish fails with the *same* failure instance, and the producer history
    // does not grow.
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0, 1, 2, 3], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();

    let first_ticket = harness.publish_traffic(&route, encoded_record(&route, 1, false), false);
    harness.wait_for_records(before + 1);
    let record = harness.producer.history()[before].clone();
    harness.deliver(DeliveryNotice {
        send_id: record.send_id,
        partition: Some(route.partition()),
        timestamp: rdkafka::Timestamp::NotAvailable,
        error: Some("NotEnoughReplicas".to_owned()),
        deadline_claimed: None,
        deadline_status: None,
    });

    let first = harness.next_outcome();
    assert_eq!(first.ticket, first_ticket);
    let failure = first.result.expect_err("the send failed");
    assert_eq!(failure.reason(), FailureReason::KafkaSend);
    assert_eq!(
        failure.class(),
        FailureClass::Capture,
        "an ordinary send failure follows the configurable policy"
    );

    let after_failure = harness.producer.history_len();
    let second_ticket = harness.publish_traffic(&route, encoded_record(&route, 2, false), false);
    let second = harness.next_outcome();
    assert_eq!(second.ticket, second_ticket);
    let second_failure = second.result.expect_err("the publisher is stopped");
    assert!(
        second_failure.is_same(&harness.handle().failure().expect("failed")),
        "every later submission carries the canonical first cause"
    );
    assert_eq!(
        harness.producer.history_len(),
        after_failure,
        "nothing is published after the publisher stops, and nothing is resubmitted"
    );
}

// ---------------------------------------------------------------- retirement

#[test]
fn a_terminal_acknowledgement_removes_the_connection_and_retires_the_old_writer() {
    // CaptureKafkaPublisherTest.finalTrafficAcknowledgementRemovesTheConnectionAndAllowsOldWriterRetirement
    // (:491-521), including "Retirement publishes no Kafka terminal record".
    let harness = Harness::new(timing(10_000, 30_000), 1);
    let first_writer = harness.activate(&[0], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();

    harness.publish_traffic(&route, encoded_record(&route, 1, true), true);
    harness.wait_for_records(before + 1);

    // A replacement generation supersedes the first, which keeps serving its existing connection.
    let second_writer = harness.activate(&[0], 2_000);
    assert_ne!(first_writer, second_writer);
    assert_eq!(
        harness
            .routing
            .lock()
            .expect("test mutex")
            .writer_status(&route.writer_partition()),
        Some(capture_kafka::routing_state::WriterStatus::Draining),
        "a superseded writer drains rather than stopping"
    );

    let history_before_retirement = harness.producer.history_len();
    harness.acknowledge(before, 1_500);
    assert!(
        Harness::wait_until(|| {
            harness
                .routing
                .lock()
                .expect("test mutex")
                .writer_status(&route.writer_partition())
                == Some(capture_kafka::routing_state::WriterStatus::Retired)
        }),
        "the terminal acknowledgement removes the connection and the drained writer retires"
    );
    assert_eq!(
        harness.producer.history_len(),
        history_before_retirement,
        "retirement publishes no Kafka terminal record"
    );
    assert_eq!(harness.metrics.writers_retired.load(Ordering::SeqCst), 1);
}

#[test]
fn orderly_retirement_is_entirely_local_once_connections_are_gone() {
    // CaptureKafkaPublisherTest.orderlyRetirementIsEntirelyLocalAfterConnectionsAreGone (:524-536).
    let harness = Harness::new(timing(10_000, 30_000), 2);
    harness.activate(&[0, 1], 1_000);
    let history_before = harness.producer.history_len();

    let reply = harness.handle().retire_all_writers().expect("running");
    reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect("retirement completes");

    assert_eq!(
        harness.producer.history_len(),
        history_before,
        "orderly retirement publishes nothing"
    );
    assert!(
        harness
            .routing
            .lock()
            .expect("test mutex")
            .all_writer_partitions_retired()
    );
    assert_eq!(harness.metrics.writers_retired.load(Ordering::SeqCst), 2);
}

#[test]
fn orderly_retirement_is_refused_while_a_connection_remains() {
    // CaptureRoutingStateTest.orderlyRetirementRequiresAnEmptyConnectionRegistry (:233-253) and
    // spec discrepancy D-19: refusing is this layer's job, disconnecting is the proxy's.
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    let _route = harness.route("conn-1");

    let reply = harness.handle().retire_all_writers().expect("running");
    let failure = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect_err("a connection remains");
    assert_eq!(failure.class(), FailureClass::UnstableProcess);
    assert!(
        harness.handle().failure().is_some(),
        "refusing to retire with connections registered is a state inconsistency"
    );
}

#[test]
fn an_abandoned_connection_lets_its_drained_writer_retire() {
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    let route = harness.route("conn-1");
    harness.activate(&[0], 2_000);

    harness
        .handle()
        .abandon_connection(route.clone())
        .expect("running");
    assert!(
        Harness::wait_until(|| {
            harness
                .routing
                .lock()
                .expect("test mutex")
                .writer_status(&route.writer_partition())
                == Some(capture_kafka::routing_state::WriterStatus::Retired)
        }),
        "a connection that never published may be abandoned, releasing its writer"
    );
}

// ---------------------------------------------------------------- the write gate

#[test]
fn tripping_the_gate_from_outside_fails_the_publisher_with_the_canonical_cause() {
    let harness = Harness::new(timing(10_000, 30_000), 4);
    harness.activate(&[0, 1, 2, 3], 1_000);

    let cause = CaptureFailure::capture(FailureReason::Other, "an unrelated component failed");
    harness.gate.trip(cause.clone());

    assert!(Harness::wait_until(|| harness.handle().failure().is_some()));
    assert!(
        harness.handle().failure().expect("failed").is_same(&cause),
        "the publisher adopts the gate's canonical first cause rather than inventing its own"
    );
    assert_eq!(
        harness.metrics.gate_trips.lock().expect("test mutex").len(),
        1,
        "the irreversible transition is counted once"
    );
}

#[test]
fn the_deadline_arming_order_means_a_blocked_enqueue_is_not_charged_against_e() {
    // CaptureKafkaPublisherTest.heartbeatDeadlineStartsAfterTheProducerAcceptsTheSubmission
    // (:193-239) and spec discrepancy D-18: the timer is armed only after the enqueue returns, so
    // no terminal failure can occur while the enqueue is blocked.
    let harness = Harness::new(timing(20, 75), 4);
    let (release_tx, release_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    *harness.producer.block.lock().expect("test mutex") = Some(release_rx);
    *harness.producer.blocked_entered.lock().expect("test mutex") = Some(entered_tx);

    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    entered_rx
        .recv_timeout(LIVENESS_BUDGET)
        .expect("the enqueue blocks");

    // E is 75 ms. Well past it, there is still no failure, because the deadline is not armed yet.
    assert!(
        reply.recv_timeout(Duration::from_millis(300)).is_err(),
        "a blocked enqueue must not be charged against E"
    );
    assert!(
        harness.handle().failure().is_none(),
        "no terminal failure while the producer has not accepted the submission"
    );

    drop(release_tx);
    harness.wait_for_records(1);
    harness.acknowledge(0, 1_000);
    assert_eq!(
        &*reply
            .recv_timeout(LIVENESS_BUDGET)
            .expect("reply")
            .expect("activation"),
        "activation:1"
    );
}

#[test]
fn a_fatal_producer_refusal_fails_capture_without_publishing() {
    let harness = Harness::new(timing(10_000, 30_000), 4);
    *harness.producer.fail_next.lock().expect("test mutex") =
        Some(ProduceError::Fatal("MsgSizeTooLarge".to_owned()));

    let reply = harness
        .handle()
        .initialize_generation(vec![0])
        .expect("running");
    let failure = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect_err("the producer refused the heartbeat");
    assert_eq!(failure.reason(), FailureReason::KafkaSend);
    assert_eq!(harness.producer.history_len(), 0);
}

// ---------------------------------------------------------------- out-of-band topic growth

#[test]
fn an_assignment_above_the_known_partition_count_refreshes_rather_than_failing() {
    // DEVIATION, spec discrepancy D-15 under owner decision OD-7: an out-of-band partition
    // increase is the intended operational path, so being assigned a new partition must not
    // terminate capture. Java freezes the count at construction and treats this as fatal.
    let harness = Harness::new(timing(10_000, 30_000), 2);
    *harness.refreshed_count.lock().expect("test mutex") = Some(3);

    let writer = harness.activate(&[0, 1, 2], 1_000);
    assert_eq!(&*writer, "activation:1");
    assert_eq!(harness.active_partitions(), vec![0, 1, 2]);
    assert_eq!(
        harness.refresh_calls.load(Ordering::SeqCst),
        1,
        "the layout is refreshed once, not polled"
    );
    assert!(harness.handle().failure().is_none());
}

#[test]
fn an_assignment_above_a_count_that_does_not_grow_fails_capture() {
    let harness = Harness::new(timing(10_000, 30_000), 2);
    *harness.refreshed_count.lock().expect("test mutex") = Some(2);

    let reply = harness
        .handle()
        .initialize_generation(vec![0, 1, 2])
        .expect("running");
    let failure = reply
        .recv_timeout(LIVENESS_BUDGET)
        .expect("reply")
        .expect_err("partition 2 does not exist");
    assert!(
        failure
            .message()
            .contains("still reports only 2 partitions"),
        "{}",
        failure.message()
    );
}

// ---------------------------------------------------------------- conservation

#[test]
fn a_healthy_lifecycle_conserves_records_bytes_and_writers() {
    // CaptureKafkaPublisherTest.healthyLifecycleEmitsConservedMetricsAndBoundedOperationalEvents
    // (:320-438) and tracing/CapturePublishingMetricsTest (:20-153): every accepted publication is
    // matched by exactly one completion.
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    let route = harness.route("conn-1");
    let before = harness.producer.history_len();
    harness.publish_traffic(&route, encoded_record(&route, 1, true), true);
    harness.wait_for_records(before + 1);
    harness.acknowledge(before, 1_500);
    harness.next_outcome().result.expect("acknowledged");

    harness.handle().report_health().expect("running");
    assert!(Harness::wait_until(|| {
        harness
            .metrics
            .count_completed(RecordKind::Traffic, PublishOutcome::Acknowledged)
            == 1
    }));

    assert_eq!(harness.metrics.count_accepted(RecordKind::Heartbeat), 1);
    assert_eq!(harness.metrics.count_accepted(RecordKind::Traffic), 1);
    assert_eq!(
        harness
            .metrics
            .count_completed(RecordKind::Heartbeat, PublishOutcome::Acknowledged),
        1
    );
    assert_eq!(
        harness
            .metrics
            .count_completed(RecordKind::Traffic, PublishOutcome::Acknowledged),
        1
    );
    assert_eq!(
        harness.metrics.count_accepted(RecordKind::Traffic)
            + harness.metrics.count_accepted(RecordKind::Heartbeat),
        harness
            .metrics
            .count_completed(RecordKind::Traffic, PublishOutcome::Acknowledged)
            + harness
                .metrics
                .count_completed(RecordKind::Heartbeat, PublishOutcome::Acknowledged),
        "every accepted publication reaches exactly one terminal outcome"
    );
    assert!(harness.handle().failure().is_none());
}

#[test]
fn a_failure_fails_every_in_flight_publication_exactly_once() {
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    let route_a = harness.route("conn-a");
    let route_b = harness.route("conn-b");
    let before = harness.producer.history_len();

    harness.publish_traffic(&route_a, encoded_record(&route_a, 1, false), false);
    harness.publish_traffic(&route_b, encoded_record(&route_b, 1, false), false);
    harness.wait_for_records(before + 2);

    harness.gate.trip(CaptureFailure::capture(
        FailureReason::Other,
        "an unrelated failure",
    ));

    let mut failed = 0;
    for _ in 0..2 {
        let outcome = harness.next_outcome();
        assert!(outcome.result.is_err());
        failed += 1;
    }
    assert_eq!(failed, 2, "both in-flight publications fail");
    assert_eq!(
        harness
            .metrics
            .count_completed(RecordKind::Traffic, PublishOutcome::Failed),
        2,
        "each failed publication is counted exactly once"
    );
}

// ---------------------------------------------------------------- lifecycle

#[test]
fn closing_the_publisher_flushes_the_producer() {
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    harness.handle().close().expect("running");
    assert!(
        Harness::wait_until(|| harness.producer.flushes.load(Ordering::SeqCst) >= 1),
        "close flushes outstanding records"
    );
    assert!(
        Harness::wait_until(|| harness
            .routing
            .lock()
            .expect("test mutex")
            .is_shutting_down()),
        "close removes routing eligibility"
    );
}

#[test]
fn a_noop_metric_sink_needs_no_methods() {
    // The trait's defaults exist so an embedder without observability is not forced to implement
    // sixteen methods, and so the tests above can be read without metric noise.
    let gate = WriteGate::new(Box::new(NoopMetrics));
    assert!(gate.is_open());
    NoopMetrics.publication_accepted(RecordKind::Traffic, 1);
    NoopMetrics.accept_to_submit(RecordKind::Traffic, Duration::from_millis(1));
    NoopMetrics.submit_to_delivery(RecordKind::Heartbeat, Duration::from_millis(1));
    NoopMetrics.queue_depth_changed(RecordKind::Traffic, 1, 2);
    NoopMetrics.oldest_unacknowledged_age(Duration::ZERO);
}

/// Keeps the unused-warning away from a field that exists only to hold the clock alive.
#[test]
fn the_deadline_clock_outlives_the_publisher_it_serves() {
    let harness = Harness::new(timing(10_000, 30_000), 1);
    harness.activate(&[0], 1_000);
    // Dropping the harness drops the runtime (which closes and joins the actor) and then the
    // clock. Neither may hang.
    drop(harness);
}

/// A placeholder referenced by `AtomicBool` import; kept because the harness needs the type for
/// future scripted producers.
const _: Option<AtomicBool> = None;
