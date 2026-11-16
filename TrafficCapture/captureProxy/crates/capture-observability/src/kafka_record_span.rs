//! The capture-record publication span (KAFKA-13).
//!
//! One span per record published to Kafka, carrying the full correlation chain the runbook
//! prescribes — `topic`, `partition`, `generation`, `offset` to follow one record, and `writer`,
//! `connection`, `request` to follow its descendants (`observabilityOperations.md:291-293`) — and
//! measuring publish latency from submission to terminal outcome.
//!
//! # The `kafkaCommit` / `stream_flush_called` naming mismatch, resolved
//!
//! `KafkaRecordContext` declares `ACTIVITY_NAME = "kafkaCommit"` (line 17) and builds its
//! instruments from it, so the metrics are `kafkaCommitCount`, `kafkaCommitDuration`, and
//! `kafkaCommitExceptionCount`. But `getActivityName()` returns the unrelated literal
//! `"stream_flush_called"` (line 59), and that is what becomes the **span** name. One publication
//! therefore shows up as `kafkaCommit*` in metrics and `stream_flush_called` in traces, which
//! breaks the correlation an operator needs when a latency panel spikes.
//!
//! This crate keeps the three metric names verbatim — they are externally consumed
//! (`kafkaCommitDuration` on both dashboards, `kafkaCommitCount_total` asserted by
//! `migrationConsole/lib/integ_test/integ_test/replayer_tests.py:69`) — and renames the **span** to
//! `kafkaCommit` so the two agree. A repository-wide grep for `stream_flush_called` outside Java
//! source finds no consumer, so nothing reads the old span name.
//!
//! # Latency semantics
//!
//! `kafkaCommitDuration` measures submission to terminal outcome, i.e. including the broker
//! acknowledgement wait. The dashboards plot its mean and p95 as the capture pipeline's publish
//! latency, so closing the span at submission rather than at acknowledgement would understate it by
//! the whole broker round trip.

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::metrics::Meter;
use tracing::{Span, debug_span};

use crate::activity::ActivityInstruments;
use crate::contract;

/// The instruments a capture-record publication records.
#[derive(Debug)]
pub struct KafkaRecordInstruments {
    family: ActivityInstruments,
}

impl KafkaRecordInstruments {
    /// Registers the `kafkaCommit` family.
    ///
    /// # Panics
    ///
    /// Panics when a name is absent from the metric contract.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        let activity = contract::ACTIVITIES
            .iter()
            .find(|activity| activity.name == "kafkaCommit")
            .expect("kafkaCommit is a declared activity");
        Self {
            family: ActivityInstruments::new(meter, activity),
        }
    }

    /// Records one traffic-record publication timed outside a span: submission to delivery
    /// report, matching Java's `KafkaRecordContext`, which opens at `send` and closes in the
    /// producer callback.
    pub fn record_publication(&self, elapsed: std::time::Duration) {
        self.family.record_completed(elapsed);
    }

    /// Counts one failed traffic-record publication. Its latency is recorded separately through
    /// [`Self::record_publication`], exactly as [`KafkaRecordSpan::failed`] does.
    pub fn record_publication_failure(&self) {
        self.family.record_exception();
    }
}

/// The correlation keys one capture-record publication carries.
///
/// **Borrowed and `Copy`**, which is load-bearing rather than stylistic: this struct is built once
/// per published record on the Kafka data path, and telemetry already accounts for 21% of the Java
/// proxy's allocation. Owning `String` fields here would allocate four times per record. The
/// `record_index` is the record's position within its connection rather than a pre-rendered
/// `"<connection>.<index>"` string, so the composite identity is formatted only inside the span
/// macro — which means only when a trace collector or a debug console filter has actually enabled
/// it.
///
/// Every field is a **span** field. None becomes a metric dimension: `topic`, `partition`,
/// `generation`, and the record index are unbounded in combination and would multiply the Kafka
/// metric series by the whole routing space.
#[derive(Debug, Clone, Copy)]
pub struct RecordCorrelation<'a> {
    /// The source connection whose traffic this record carries.
    pub connection_id: &'a str,
    /// The writer-partition lane that owns the publication.
    pub writer: &'a str,
    /// The Kafka topic.
    pub topic: &'a str,
    /// The Kafka partition.
    pub partition: i32,
    /// The routing generation that selected the lane.
    pub generation: u64,
    /// The record's index within its connection, which with `connection_id` is its identity.
    pub record_index: u64,
    /// The serialised record size in bytes.
    pub record_size: u64,
}

#[derive(Debug)]
struct SpanState {
    instruments: Arc<KafkaRecordInstruments>,
    started: Instant,
    span: Span,
}

/// An in-flight capture-record publication.
///
/// Closes exactly once, whether by [`Self::acknowledged`], [`Self::failed`], or drop, so the
/// latency histogram cannot lose an observation on an error path.
#[derive(Debug)]
pub struct KafkaRecordSpan {
    state: Option<SpanState>,
}

impl KafkaRecordSpan {
    /// Begins observing one publication.
    ///
    /// The correlation is consumed into the span and not retained, so the handle itself carries
    /// nothing but one reference count, one `Instant`, and the span.
    #[must_use]
    pub fn begin(
        instruments: Arc<KafkaRecordInstruments>,
        correlation: RecordCorrelation<'_>,
    ) -> Self {
        // DEVIATION: Java names this span `stream_flush_called`
        // (KafkaRecordContext.getActivityName, line 59) while building its metrics from
        // ACTIVITY_NAME = "kafkaCommit" (line 17), so one publication appears under two unrelated
        // names. The metric names are kept verbatim because they are externally consumed; the span
        // is renamed to match them. A repository-wide grep for `stream_flush_called` outside Java
        // source finds no consumer.
        let span = debug_span!(
            "kafkaCommit",
            connection = %correlation.connection_id,
            writer = %correlation.writer,
            topic = %correlation.topic,
            partition = correlation.partition,
            generation = correlation.generation,
            record = correlation.record_index,
            record_size = correlation.record_size,
            offset = tracing::field::Empty
        );
        Self {
            state: Some(SpanState {
                instruments,
                started: Instant::now(),
                span,
            }),
        }
    }

    /// The span, for parenting or for further correlation fields.
    #[must_use]
    pub fn span(&self) -> Option<&Span> {
        self.state.as_ref().map(|state| &state.span)
    }

    /// The broker acknowledged the record at `offset`: records the publish latency.
    pub fn acknowledged(mut self, offset: i64) {
        if let Some(state) = &self.state {
            state.span.record("offset", offset);
        }
        self.finish();
    }

    /// The publication failed terminally: counts the exception and records the latency.
    ///
    /// Both are recorded, matching Java, where the exception counter is independent of the scope
    /// counter. A failed publication's latency is still latency, and excluding it would make the
    /// p95 panel optimistic exactly when something is wrong.
    pub fn failed(mut self) {
        if let Some(state) = &self.state {
            state.instruments.family.record_exception();
        }
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(state) = self.state.take() {
            state
                .instruments
                .family
                .record_completed(state.started.elapsed());
            drop(state.span);
        }
    }
}

impl Drop for KafkaRecordSpan {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SCOPE_NAME;
    use crate::snapshot::recording_meter_provider;
    use opentelemetry::metrics::MeterProvider;

    fn correlation() -> RecordCorrelation<'static> {
        RecordCorrelation {
            connection_id: "c-1",
            writer: "w-0",
            topic: "logging-traffic-topic",
            partition: 3,
            generation: 7,
            record_index: 0,
            record_size: 2048,
        }
    }

    /// The Kafka data path must not allocate per record. `Copy` is the structural guard: a `String`
    /// field would break this.
    #[test]
    fn the_correlation_is_copy_so_publishing_allocates_nothing() {
        const fn assert_copy<T: Copy>() {}
        assert_copy::<RecordCorrelation<'_>>();
    }

    #[test]
    fn an_acknowledged_publication_records_latency_without_dimensions() {
        let (provider, recorder) = recording_meter_provider("kafka-span");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(KafkaRecordInstruments::new(&meter));
        KafkaRecordSpan::begin(instruments, correlation()).acknowledged(42);

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        let duration = snapshot
            .point_for("kafkaCommitDuration")
            .expect("kafkaCommitDuration exported");
        assert_eq!(duration.unit, "ms");
        assert!(
            duration.attributes.is_empty(),
            "topic/partition/recordId must not reach the metric point"
        );
        assert!(snapshot.point_for("kafkaCommitCount").is_some());
        assert!(snapshot.point_for("kafkaCommitExceptionCount").is_none());
    }

    #[test]
    fn a_failed_publication_counts_the_exception_and_the_latency() {
        let (provider, recorder) = recording_meter_provider("kafka-span-fail");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(KafkaRecordInstruments::new(&meter));
        KafkaRecordSpan::begin(instruments, correlation()).failed();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(snapshot.points_for("kafkaCommitDuration").len(), 1);
        assert!(snapshot.point_for("kafkaCommitExceptionCount").is_some());
    }

    #[test]
    fn an_abandoned_publication_still_records_one_latency_observation() {
        let (provider, recorder) = recording_meter_provider("kafka-span-drop");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(KafkaRecordInstruments::new(&meter));
        drop(KafkaRecordSpan::begin(instruments, correlation()));

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(snapshot.points_for("kafkaCommitDuration").len(), 1);
    }
}
