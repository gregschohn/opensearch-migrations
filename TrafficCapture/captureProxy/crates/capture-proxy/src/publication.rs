//! The publication boundary the capture sink awaits on.
//!
//! [`crate::capture_sink::RecordingCaptureSink`] drives a [`capture_wire::ConnectionRecorder`],
//! whose [`capture_wire::RecordSink`] hands detached records to Kafka. But the sink also has to
//! **wait** for those records to be durably acknowledged, and validate the acknowledgement, and
//! neither of those is part of `RecordSink`. [`PublishingRecordSink`] is that missing half: a
//! `RecordSink` that can additionally report what came back.
//!
//! Splitting it out this way is what makes the capture-before-forward gate testable without a
//! broker, a container, or a network — [`TestRecordSink`] implements the whole boundary over
//! [`capture_wire::CollectingSink`] and a hand-driven outcome queue, so every ordering and failure
//! rule the gate must honour is pinned by a unit test rather than by an integration environment.
//!
//! AWKWARD, and worth an owner decision:
//! `capture_kafka::connection_factory::ConnectionCapture` already contains a *second*
//! `capture_wire::PublishChain` behind its own `submit`, parallel to the one inside
//! `ConnectionRecorder`. Two chains in series are not incorrect — the outer one serialises to one
//! in-flight record, so the inner one never queues — but they do mean an acknowledgement has to be
//! bridged from one to the other, and the bridge can only count outcomes rather than match them to
//! tickets, because `ConnectionCapture::drain_outcomes` returns a `usize` and keeps the
//! per-outcome detail to itself. `ConnectionCapture` also exposes no awaitable readiness at all:
//! `drain_outcomes` is non-blocking and `await_quiet` blocks the calling **thread** on a
//! `std::sync::mpsc` receiver, which cannot be used on a Tokio worker. Either a
//! `drain_outcomes() -> impl Iterator<Item = TrafficOutcome>` or a `tokio::sync::Notify` on
//! `ConnectionCapture` would let this module match tickets exactly and stop polling.

use capture_wire::{EncodedRecord, RecordBuf, RecordSink, SinkError};

/// What Kafka said about one appended record.
///
/// The two fields the capture protocol validates, and nothing else: the partition, so it can be
/// compared against the partition the connection is bound to, and the broker's `LogAppendTime`, so
/// the heartbeat baseline check runs with broker time on both sides and no local clock involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acknowledgement {
    /// The partition Kafka appended to.
    pub partition: i32,
    /// The broker-assigned `LogAppendTime`.
    pub log_append_time: i64,
}

/// One record's terminal outcome.
#[derive(Debug, Clone)]
pub enum PublicationOutcome {
    /// Durably appended, with the metadata the gate must validate.
    Acknowledged(Acknowledgement),
    /// Failed, or ended ambiguously. Either way the connection's chain is poisoned: the capture
    /// protocol treats an ambiguous send exactly like a failed one, because neither establishes
    /// that the request reached Kafka before the proxy forwarded it.
    Failed(String),
}

/// A [`RecordSink`] that also reports what the transport did with each record.
///
/// Implemented once over Kafka and once over an in-memory queue for tests.
pub trait PublishingRecordSink: RecordSink {
    /// Takes every outcome that has arrived, in submission order. Never blocks.
    ///
    /// Returning an empty vector means nothing has completed yet, which is the normal case on the
    /// request path and must not be mistaken for an acknowledgement.
    fn take_outcomes(&mut self) -> Vec<PublicationOutcome>;

    /// Validates an acknowledgement for Critical Mutation Traffic.
    ///
    /// Three checks, in the protocol's order: the capture write gate is still open, the
    /// acknowledged partition is the connection's own partition, and the broker `LogAppendTime` is
    /// positive and within `[-S, E)` of the writer partition's heartbeat baseline.
    ///
    /// # Errors
    ///
    /// The operator-facing reason the acknowledgement is not good enough to forward on. The caller
    /// routes it to the capture-failure policy; it is never ignored.
    fn validate_critical_mutation(&self, acknowledged: Acknowledgement) -> Result<(), String>;

    /// A description of this connection's route, for diagnostics.
    fn route_description(&self) -> String;
}

/// A [`PublishingRecordSink`] whose outcomes are supplied by the test, not by a broker.
///
/// Deliberately in the non-test build: it is the only way the capture gate's rules can be pinned by
/// a unit test, and keeping it compiled always means it cannot rot.
#[derive(Debug)]
pub struct TestRecordSink {
    inner: capture_wire::CollectingSink,
    pending: std::collections::VecDeque<PublicationOutcome>,
    /// `Err` makes every validation fail, which is how the acknowledgement-validation branch of
    /// the gate is reached without a broker that returns bad metadata.
    validation: Result<(), String>,
    partition: i32,
}

impl Default for TestRecordSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRecordSink {
    /// A sink that acknowledges nothing until told to.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: capture_wire::CollectingSink::new(),
            pending: std::collections::VecDeque::new(),
            validation: Ok(()),
            partition: 7,
        }
    }

    /// Makes [`PublishingRecordSink::validate_critical_mutation`] fail with `reason`.
    #[must_use]
    pub fn rejecting_validation(mut self, reason: &str) -> Self {
        self.validation = Err(reason.to_owned());
        self
    }

    /// Queues a successful acknowledgement for the next record to complete.
    pub fn acknowledge_next(&mut self) {
        self.pending
            .push_back(PublicationOutcome::Acknowledged(Acknowledgement {
                partition: self.partition,
                log_append_time: 1_700_000_000_000,
            }));
    }

    /// Queues a failure for the next record to complete.
    pub fn fail_next(&mut self, reason: &str) {
        self.pending
            .push_back(PublicationOutcome::Failed(reason.to_owned()));
    }

    /// How many records have been handed to this sink.
    #[must_use]
    pub fn submitted_record_count(&self) -> usize {
        self.inner.records().len()
    }
}

impl RecordSink for TestRecordSink {
    fn create_buffer(&mut self, capacity: usize) -> RecordBuf {
        self.inner.create_buffer(capacity)
    }

    fn submit(&mut self, record: EncodedRecord) -> Result<(), SinkError> {
        self.inner.submit(record)
    }
}

impl PublishingRecordSink for TestRecordSink {
    fn take_outcomes(&mut self) -> Vec<PublicationOutcome> {
        self.pending.drain(..).collect()
    }

    fn validate_critical_mutation(&self, acknowledged: Acknowledgement) -> Result<(), String> {
        self.validation.clone()?;
        if acknowledged.partition == self.partition {
            Ok(())
        } else {
            Err(format!(
                "acknowledged partition {} is not the connection's partition {}",
                acknowledged.partition, self.partition
            ))
        }
    }

    fn route_description(&self) -> String {
        format!("test-writer/{}", self.partition)
    }
}
