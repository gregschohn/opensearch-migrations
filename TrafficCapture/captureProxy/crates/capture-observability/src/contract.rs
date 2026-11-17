//! The frozen metric contract, expressed as data.
//!
//! Every instrument the capture proxy exports is declared once, here, with its **name**,
//! **instrument kind**, **declared `OpenTelemetry` unit string**, **dimension keys**, and each
//! dimension's **closed value set**. The registry in [`crate::root`] builds its instruments from
//! this table, and `tests/metric_contract.rs` replays every recording path and compares what the
//! real `OpenTelemetry` pipeline emitted against this table. A rename or a re-unit therefore fails
//! a test rather than silently emptying a shipped dashboard panel.
//!
//! # Why the unit string is as load-bearing as the name
//!
//! The Kubernetes Prometheus exporter derives the exported series suffix from the declared unit, so
//! `captureWritersActive` with unit `writers` becomes the series `captureWritersActive_writers`
//! that the Grafana dashboard hard-codes. Changing the unit to `count` would publish
//! `captureWritersActive_count` and the panel would go blank with no error anywhere.
//! [`Instrument::exported_series_name`] implements that derivation so the contract test can assert
//! the consumer-visible strings directly, not just the instrument names.
//!
//! # Cardinality is a contract too
//!
//! `docs/captureAndReplay/observabilityOperations.md:19-20` requires traffic identities to stay in
//! logs and out of metrics. Every dimension here is therefore either a [`DimensionDomain::Closed`]
//! enumeration or the [`DimensionDomain::ProcessIdentity`] pair that contributes exactly one value
//! per process. There is no free-form string dimension, and [`FORBIDDEN_DIMENSION_KEYS`] names the
//! identity keys that must never appear.

use std::time::Duration;

/// The single meter scope every proxy instrument lands in.
///
/// `RootCaptureContext.java:14`. The shipped dashboards filter on it exclusively
/// (`OTelLib="captureProxy"` in the `CloudWatch` generator, `otel_scope_name="captureProxy"` in the
/// Grafana generator), so a second scope would hide metrics from both.
pub const SCOPE_NAME: &str = "captureProxy";

/// The `service.name` resource attribute (`CaptureProxy.java:569`).
pub const SERVICE_NAME: &str = "capture";

/// Metric export cadence (`RootOtelContext.java:48-53`).
///
/// One export per second per process is what makes the `CloudWatch` `Sum/PERIOD` = fleet-total
/// convention in `generate-capture-replay-dashboard.py:9-12` arithmetically correct. Changing it
/// silently rescales every rate panel.
pub const METRIC_EXPORT_INTERVAL: Duration = Duration::from_secs(1);

/// The Java default duration-histogram bucket bounds in milliseconds.
///
/// `CommonScopedMetricInstruments.java:23-26`: `OpenTelemetry`'s defaults extended from 10 s to 30 s
/// so a slow tail stays distinguishable from the overflow bucket.
///
/// **No instrument in this crate uses this set.** It is retained because it is the Java baseline
/// these histograms are compared against, and because reading it next to
/// [`GATE_LATENCY_BUCKETS_MS`] is what explains why the latency histograms were re-bucketed: its
/// first bound is 5 ms, so everything faster than 5 ms falls into one bucket and no percentile below
/// that is recoverable. Measured on the Java baseline, **99.045% of `kafkaCommitDuration` samples
/// landed in that single `(0, 5]` bucket**, which makes the only honest statements about Java's
/// acknowledgement latency "mean = 1.249 ms" and "p99 <= 5 ms".
pub const DURATION_BUCKETS_MS: &[f64] = &[
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1_000.0, 2_500.0, 5_000.0,
    7_500.0, 10_000.0, 15_000.0, 30_000.0,
];

/// Bucket bounds for every latency histogram a performance gate depends on.
///
/// # Why these exist
///
/// Phase-1 Gate 5 is "Kafka acknowledgement p99 within 10% of Java" and Gate 3 / final gate F4 are
/// stated on **proxy-added p99 latency**. Neither is decidable against
/// [`DURATION_BUCKETS_MS`], whose lowest bound is 5 ms: with 99.045% of Java's acknowledgement
/// samples inside `(0, 5]`, "p99" from that histogram means nothing finer than "at most 5 ms", and a
/// Rust implementation could be twice as slow or twice as fast without the number moving.
///
/// These bounds put six resolution points below 5 ms — 0.1, 0.25, 0.5, 1, 2.5, 5 — which brackets
/// the Java mean of 1.249 ms on both sides, while keeping the same top end so a slow tail stays
/// distinguishable from the overflow bucket. There are 17 bounds, exactly as many as
/// [`DURATION_BUCKETS_MS`], so the change costs **no additional Prometheus series**.
///
/// # Both sides must use them
///
/// A comparison is only valid if the baseline is re-bucketed too. Apply these same bounds to the
/// Java build with an `OpenTelemetry` view over the instruments named in
/// [`GATE_LATENCY_INSTRUMENTS`], then re-measure; otherwise the gate is comparing a 17-bucket
/// histogram against a 1-bucket one.
// DEVIATION: Java uses DURATION_BUCKETS_MS for every duration histogram. The four histograms a
// performance gate reads use these bounds instead, because the Java bounds cannot express the
// quantity the gate is stated on. Safe for both shipped consumers: the Grafana p95 panel is
// `histogram_quantile(0.95, sum by (le) (rate(..._bucket[..])))` and its mean is `_sum / _count`
// (generate-capture-replay-grafana-dashboard.py:36-45), both bucket-agnostic, and CloudWatch reads
// only the mean (generate-capture-replay-dashboard.py:280).
pub const GATE_LATENCY_BUCKETS_MS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0,
    5_000.0, 10_000.0, 30_000.0,
];

/// The instruments that use [`GATE_LATENCY_BUCKETS_MS`], and the gate each one serves.
///
/// | Instrument | Gate |
/// |---|---|
/// | `kafkaCommitDuration` | Phase-1 Gate 5, Kafka acknowledgement p99 within 10% of Java |
/// | `gatheringRequestDuration` | Phase-1 Gate 3 and F4, proxy-added p99 latency |
/// | `blockedDuration` | Phase-1 Gate 3 and F4, the pre-forward capture gate's share of proxy-added latency |
/// | `gatheringResponseDuration` | Phase-1 Gate 3 and F4, proxy-added p99 latency |
///
/// `captureConnectionDuration` and `waitingForResponseDuration` are deliberately **not** here. The
/// first measures connection lifetime and the second measures how long the *source* took to answer;
/// neither is proxy-added latency, no gate is stated on either, and both legitimately run to
/// minutes, which is what [`LONG_DURATION_BUCKETS_MS`] is for.
pub const GATE_LATENCY_INSTRUMENTS: &[&str] = &[
    "kafkaCommitDuration",
    "gatheringRequestDuration",
    "blockedDuration",
    "gatheringResponseDuration",
];

/// Bucket bounds for durations that legitimately run past 30 s.
///
/// `CommonScopedMetricInstruments.java:32-35`: connection lifetimes and waits on a slow peer. Each
/// added bound costs one Prometheus series per histogram per process, so these are opted into.
pub const LONG_DURATION_BUCKETS_MS: &[f64] = &[
    0.0,
    5.0,
    10.0,
    25.0,
    50.0,
    75.0,
    100.0,
    250.0,
    500.0,
    750.0,
    1_000.0,
    2_500.0,
    5_000.0,
    7_500.0,
    10_000.0,
    15_000.0,
    30_000.0,
    60_000.0,
    120_000.0,
    300_000.0,
    600_000.0,
    1_800_000.0,
    3_600_000.0,
];

/// Dimension keys that must never appear on a metric point.
///
/// These are the per-connection, per-record, and per-request identities the runbook requires to
/// stay in log events and spans (`observabilityOperations.md:13, 19-20`;
/// `CaptureConnectionMetrics.java:14-16`; `CapturePublishingMetrics.java:16-18`). They are the
/// correlation keys an operator follows *in the logs*; on a metric they would multiply the series
/// count without bound.
pub const FORBIDDEN_DIMENSION_KEYS: &[&str] = &[
    "connectionId",
    "nodeId",
    "topic",
    "recordId",
    "recordSize",
    "sourceRequestIndex",
    "partition",
    "offset",
    "generation",
    "writer",
    "lifetime",
    "request",
    "attempt",
    "channel",
];

/// What kind of `OpenTelemetry` instrument a contract row is.
///
/// The kind decides both the aggregation and the Prometheus series suffix: a monotonic counter gains
/// `_total`, an up-down counter does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstrumentKind {
    /// Monotonic `u64` sum.
    Counter,
    /// Non-monotonic `i64` sum, used for "currently this many" signals.
    UpDownCounter,
    /// Explicit-bucket `f64` histogram.
    Histogram,
}

impl InstrumentKind {
    /// The kind's stable name, used in contract assertions and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::UpDownCounter => "up_down_counter",
            Self::Histogram => "histogram",
        }
    }
}

/// The complete set of values a dimension may ever take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimensionDomain {
    /// A closed enumeration. Every value the process can emit is listed.
    Closed(&'static [&'static str]),
    /// The process's own identity: one value for the lifetime of the process.
    ///
    /// `processId` and `captureActivationId` (`CaptureProcessMetrics.java:21-23`). These are the
    /// only bridge between a `captureGapActive` metric point and the `capture_startup_summary` /
    /// `capture_shutdown_summary` log lines, so they are deliberately on the metric despite being
    /// unenumerable: they add exactly one series per process, which is the same bound as the
    /// process count itself.
    ProcessIdentity,
}

impl DimensionDomain {
    /// How many distinct values one process can emit for this dimension.
    #[must_use]
    pub const fn values_per_process(self) -> usize {
        match self {
            Self::Closed(values) => values.len(),
            Self::ProcessIdentity => 1,
        }
    }
}

/// One dimension (`OpenTelemetry` attribute key) of an instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimension {
    /// The attribute key, exactly as emitted.
    pub key: &'static str,
    /// The complete value set.
    pub domain: DimensionDomain,
}

/// Who outside this crate reads an instrument, and therefore how expensive a rename is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// A shipped dashboard, an operator script, or an integration test reads this series.
    ///
    /// Renaming or re-uniting it breaks a checked-in consumer. See the module-level list in
    /// [`crate::contract`] tests for the per-instrument consumer citations, and
    /// `tests/dashboard_contract.rs`, which reads the shipped consumers and fails if one queries a
    /// series, unit suffix, or dimension the contract does not export.
    ExternallyConsumed,
    /// Named in the operations runbook's metric inventory but not plotted.
    ///
    /// `docs/captureAndReplay/observabilityOperations.md:57-77`. A weaker contract than a dashboard
    /// panel, but still the documented operator interface.
    RunbookDocumented,
    /// No dashboard, runbook, script, or test reference found in this repository.
    Internal,
}

/// One row of the frozen metric contract.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Instrument {
    /// The instrument name, exactly as registered with the meter.
    pub name: &'static str,
    /// The instrument kind.
    pub kind: InstrumentKind,
    /// The declared `OpenTelemetry` unit string, or `""` when the Java code declares no unit.
    pub unit: &'static str,
    /// The instrument description, carried through to the exporter.
    pub description: &'static str,
    /// The dimension keys and their closed value sets.
    pub dimensions: &'static [Dimension],
    /// Explicit histogram bucket bounds, or `None` for non-histograms.
    pub buckets: Option<&'static [f64]>,
    /// Who reads this series.
    pub exposure: Exposure,
}

impl Instrument {
    /// The series name the Kubernetes Prometheus exporter publishes for this instrument.
    ///
    /// Derived from the declared unit and kind, matching the verified exported names the Grafana
    /// generator hard-codes (`observabilityOperations.md:40-43` and the worked examples in
    /// `generate-capture-replay-grafana-dashboard.py`): the normalised unit is appended to the
    /// name, and a monotonic counter additionally gains `_total`. For a histogram this is the base
    /// name; Prometheus appends `_bucket`, `_sum`, and `_count` to it.
    ///
    /// This exists so the contract test can assert the strings a dashboard actually queries, which
    /// is what a changed unit silently breaks.
    #[must_use]
    pub fn exported_series_name(&self) -> String {
        let unit = normalise_prometheus_unit(self.unit);
        let mut series = String::from(self.name);
        if !unit.is_empty() {
            series.push('_');
            series.push_str(unit);
        }
        if self.kind == InstrumentKind::Counter {
            series.push_str("_total");
        }
        series
    }

    /// The maximum number of metric series one process can publish for this instrument.
    ///
    /// The product of the per-process value counts of its dimensions, which is finite for every row
    /// in [`INSTRUMENTS`] precisely because no dimension is free-form.
    #[must_use]
    pub fn series_per_process(&self) -> usize {
        self.dimensions
            .iter()
            .map(|dimension| dimension.domain.values_per_process())
            .product()
    }
}

/// The Prometheus unit suffix for a declared `OpenTelemetry` unit string.
///
/// `By` is UCUM for byte and normalises to `bytes`, which is why `capturePublishingBytesAccepted`
/// (unit `By`) and `bytesRead` (unit `bytes`) both export as `_bytes_total`. `ms` normalises to
/// `milliseconds`. Every other unit this crate declares passes through verbatim, so `records`,
/// `connections`, `writers`, `processes`, `writes`, `failures`, and `violations` are **not**
/// interchangeable.
#[must_use]
pub fn normalise_prometheus_unit(unit: &str) -> &str {
    match unit {
        "By" | "bytes" => "bytes",
        "ms" => "milliseconds",
        other => other,
    }
}

/// Values of the `reason` dimension on `captureConnectionCloses`.
///
/// `CaptureConnectionMetrics.CloseReason` (lines 19-26), lowercased. The set is closed: every
/// connection close is attributed to exactly one of these.
pub const CLOSE_REASONS: &[&str] = &[
    "normal",
    "assembly_duration",
    "header_bytes",
    "total_bytes",
    "connection_duration",
    "capture_failure",
];

/// Values of the `recordKind` dimension (`CapturePublishingMetrics.RecordKind`, lines 21-24).
pub const RECORD_KINDS: &[&str] = &["traffic", "heartbeat"];

/// Values of the `outcome` dimension (`CapturePublishingMetrics.PublishOutcome`, lines 26-29).
pub const PUBLISH_OUTCOMES: &[&str] = &["acknowledged", "failed"];

/// Values of the `transition` dimension (`CapturePublishingMetrics.RoutingTransition`, lines 31-35).
pub const ROUTING_TRANSITIONS: &[&str] = &[
    "assignment_received",
    "generation_prepared",
    "generation_activated",
];

/// Values of the `reason` dimension on the Kafka failure instruments.
///
/// `CapturePublishingMetrics.FailureReason` (lines 37-46), lowercased.
pub const FAILURE_REASONS: &[&str] = &[
    "heartbeat_deadline",
    "broker_time",
    "payload_limit",
    "kafka_send",
    "corrupted_state",
    "executor_rejected",
    "membership",
    "other",
];

/// Values of the `mode` dimension on the capture-gap instruments.
///
/// `CaptureProcessMetrics.java:57` only ever writes `fail-open`: a fail-closed process terminates
/// instead of living in a gap, so there is no gap point to label. Note the **hyphen**, matching the
/// configuration spelling, where `failurePolicy` in the startup log line uses an underscore.
pub const OPERATING_MODES: &[&str] = &["fail-open"];

/// Values of the `sourceForwardingContinued` dimension.
///
/// `CaptureProcessMetrics.java:58` only ever writes `true`: the dimension exists to state, on the
/// metric itself, that this process is still forwarding to the source while not capturing.
pub const SOURCE_FORWARDING_CONTINUED: &[&str] = &["true"];

const PROCESS_IDENTITY: &[Dimension] = &[
    Dimension {
        key: "processId",
        domain: DimensionDomain::ProcessIdentity,
    },
    Dimension {
        key: "captureActivationId",
        domain: DimensionDomain::ProcessIdentity,
    },
];

const CAPTURE_GAP_DIMENSIONS: &[Dimension] = &[
    Dimension {
        key: "processId",
        domain: DimensionDomain::ProcessIdentity,
    },
    Dimension {
        key: "captureActivationId",
        domain: DimensionDomain::ProcessIdentity,
    },
    Dimension {
        key: "mode",
        domain: DimensionDomain::Closed(OPERATING_MODES),
    },
    Dimension {
        key: "sourceForwardingContinued",
        domain: DimensionDomain::Closed(SOURCE_FORWARDING_CONTINUED),
    },
];

const REASON_CLOSE: &[Dimension] = &[Dimension {
    key: "reason",
    domain: DimensionDomain::Closed(CLOSE_REASONS),
}];

const REASON_FAILURE: &[Dimension] = &[Dimension {
    key: "reason",
    domain: DimensionDomain::Closed(FAILURE_REASONS),
}];

const RECORD_KIND: &[Dimension] = &[Dimension {
    key: "recordKind",
    domain: DimensionDomain::Closed(RECORD_KINDS),
}];

const RECORD_KIND_AND_OUTCOME: &[Dimension] = &[
    Dimension {
        key: "recordKind",
        domain: DimensionDomain::Closed(RECORD_KINDS),
    },
    Dimension {
        key: "outcome",
        domain: DimensionDomain::Closed(PUBLISH_OUTCOMES),
    },
];

const TRANSITION: &[Dimension] = &[Dimension {
    key: "transition",
    domain: DimensionDomain::Closed(ROUTING_TRANSITIONS),
}];

const NO_DIMENSIONS: &[Dimension] = &[];

/// Name of the counter generated for activity `activity`.
///
/// `CommonScopedMetricInstruments.fromActivityName` (lines 44-50). Declared with **no unit**, which
/// is why it exports as `<activity>Count_total`.
#[must_use]
pub fn activity_count_name(activity: &str) -> String {
    format!("{activity}Count")
}

/// Name of the duration histogram generated for activity `activity`.
///
/// `CommonScopedMetricInstruments.java:17, 87`. Unit `ms`, so it exports as
/// `<activity>Duration_milliseconds`.
#[must_use]
pub fn activity_duration_name(activity: &str) -> String {
    format!("{activity}Duration")
}

/// Name of the exception counter generated for activity `activity`.
///
/// `CommonMetricInstruments.Labels.fromActivityName` (lines 12-14). Declared with no unit.
#[must_use]
pub fn activity_exception_name(activity: &str) -> String {
    format!("{activity}ExceptionCount")
}

/// Which bucket set a duration histogram uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketSet {
    /// [`GATE_LATENCY_BUCKETS_MS`]: sub-millisecond resolution, for a histogram a gate reads.
    Gate,
    /// [`LONG_DURATION_BUCKETS_MS`]: for durations that legitimately run to minutes.
    Long,
    /// [`DURATION_BUCKETS_MS`]: the Java default, used by no instrument in this crate.
    JavaDefault,
}

impl BucketSet {
    /// The bucket bounds in milliseconds.
    #[must_use]
    pub const fn bounds(self) -> &'static [f64] {
        match self {
            Self::Gate => GATE_LATENCY_BUCKETS_MS,
            Self::Long => LONG_DURATION_BUCKETS_MS,
            Self::JavaDefault => DURATION_BUCKETS_MS,
        }
    }
}

/// An activity whose span-shaped family of three instruments is generated from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    /// The activity name, which prefixes all three generated instrument names.
    pub name: &'static str,
    /// The duration histogram's bucket set.
    pub buckets: BucketSet,
}

/// Every activity that generates a `Count` / `Duration` / `ExceptionCount` family.
///
/// Six activities, eighteen instruments. The bucket choice per activity is load-bearing in two
/// directions: `captureConnection` and `waitingForResponse` legitimately exceed 30 s
/// (`ConnectionContext.java:43`, `WireCaptureContexts.java:226`) and so keep the long set, while the
/// four phases a performance gate is stated on need resolution below 5 ms and so take
/// [`BucketSet::Gate`]. See [`GATE_LATENCY_INSTRUMENTS`].
pub const ACTIVITIES: &[Activity] = &[
    Activity {
        name: "captureConnection",
        buckets: BucketSet::Long,
    },
    Activity {
        name: "gatheringRequest",
        buckets: BucketSet::Gate,
    },
    Activity {
        name: "blocked",
        buckets: BucketSet::Gate,
    },
    Activity {
        name: "waitingForResponse",
        buckets: BucketSet::Long,
    },
    Activity {
        name: "gatheringResponse",
        buckets: BucketSet::Gate,
    },
    Activity {
        name: "kafkaCommit",
        buckets: BucketSet::Gate,
    },
];

/// The complete, frozen instrument contract: 47 instruments in one table.
///
/// Rows are grouped by the module that records them. The eighteen generated activity instruments
/// come first, in [`ACTIVITIES`] order, because `tests/metric_contract.rs` relies on the whole
/// table being a flat list it can compare set-wise against what the pipeline exported.
pub const INSTRUMENTS: &[Instrument] = &[
    // ---- generated activity families: captureConnection (crate::connection_span) ----
    Instrument {
        name: "captureConnectionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Capture-proxy source connection spans opened",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "captureConnectionDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Capture-proxy source connection lifetime",
        dimensions: NO_DIMENSIONS,
        buckets: Some(LONG_DURATION_BUCKETS_MS),
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "captureConnectionExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed on a capture-proxy source connection span",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    // ---- gatheringRequest (crate::wire_activity) ----
    Instrument {
        name: "gatheringRequestCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Request-gathering phases entered",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "gatheringRequestDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Time spent gathering one source request",
        dimensions: NO_DIMENSIONS,
        buckets: Some(GATE_LATENCY_BUCKETS_MS),
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "gatheringRequestExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed while gathering a source request",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- blocked ----
    Instrument {
        name: "blockedCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Request-blocking phases entered",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "blockedDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Time a request was blocked awaiting capture acknowledgement",
        dimensions: NO_DIMENSIONS,
        buckets: Some(GATE_LATENCY_BUCKETS_MS),
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "blockedExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed while a request was blocked",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    // ---- waitingForResponse ----
    Instrument {
        name: "waitingForResponseCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Response-wait phases entered",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "waitingForResponseDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Time spent waiting for the source's first response byte",
        dimensions: NO_DIMENSIONS,
        buckets: Some(LONG_DURATION_BUCKETS_MS),
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "waitingForResponseExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed while waiting for a source response",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    // ---- gatheringResponse ----
    Instrument {
        name: "gatheringResponseCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Response-gathering phases entered",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "gatheringResponseDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Time spent relaying one source response",
        dimensions: NO_DIMENSIONS,
        buckets: Some(GATE_LATENCY_BUCKETS_MS),
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "gatheringResponseExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed while relaying a source response",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- kafkaCommit (crate::kafka_record_span) ----
    Instrument {
        name: "kafkaCommitCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Capture record publications started",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "kafkaCommitDuration",
        kind: InstrumentKind::Histogram,
        unit: "ms",
        description: "Time from capture record submission to its terminal publish outcome",
        dimensions: NO_DIMENSIONS,
        buckets: Some(GATE_LATENCY_BUCKETS_MS),
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "kafkaCommitExceptionCount",
        kind: InstrumentKind::Counter,
        unit: "",
        description: "Exceptions observed while publishing a capture record",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- wire activity counters (crate::wire_activity) ----
    Instrument {
        name: "unregistered",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Capture handlers unregistered from a source channel",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "removed",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Capture handlers removed from a source channel",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "blockingRequest",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Requests held at the pre-forward capture gate",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "captureSuppressed",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Requests forwarded without capture by a suppression rule",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "fullRequest",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Fully parsed source requests",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "bytesRead",
        kind: InstrumentKind::Counter,
        unit: "bytes",
        description: "Request bytes read from source connections",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "bytesWritten",
        kind: InstrumentKind::Counter,
        unit: "bytes",
        description: "Response bytes written to source connections",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- connection span gauge (crate::connection_span) ----
    Instrument {
        name: "activeConnection",
        kind: InstrumentKind::UpDownCounter,
        unit: "count",
        description: "Source connections with an open capture span",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- connection metrics (crate::connection_metrics) ----
    Instrument {
        name: "captureConnectionsActive",
        kind: InstrumentKind::UpDownCounter,
        unit: "connections",
        description: "Capture-proxy source connections currently open",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "captureConnectionCloses",
        kind: InstrumentKind::Counter,
        unit: "connections",
        description: "Capture-proxy source connection closes by bounded reason",
        dimensions: REASON_CLOSE,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- process metrics (crate::process_metrics) ----
    Instrument {
        name: "captureProcessPassThroughTransitions",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Irreversible proxy transitions from authoritative capture to pass-through",
        dimensions: CAPTURE_GAP_DIMENSIONS,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "captureProcessTerminationTransitions",
        kind: InstrumentKind::Counter,
        unit: "count",
        description: "Proxy transitions to immediate process termination",
        dimensions: PROCESS_IDENTITY,
        buckets: None,
        exposure: Exposure::Internal,
    },
    Instrument {
        name: "captureGapActive",
        kind: InstrumentKind::UpDownCounter,
        unit: "processes",
        description: "Whether this proxy is alive in irreversible pass-through after capture failure",
        dimensions: CAPTURE_GAP_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    // ---- Kafka publishing (crate::kafka_metrics) ----
    Instrument {
        name: "capturePublishingRecordsAccepted",
        kind: InstrumentKind::Counter,
        unit: "records",
        description: "Capture records accepted by the Kafka producer boundary",
        dimensions: RECORD_KIND,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "capturePublishingBytesAccepted",
        kind: InstrumentKind::Counter,
        unit: "By",
        description: "Capture payload bytes accepted by the Kafka producer boundary",
        dimensions: RECORD_KIND,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "capturePublishingCompletions",
        kind: InstrumentKind::Counter,
        unit: "records",
        description: "Terminal outcomes for accepted capture publications",
        dimensions: RECORD_KIND_AND_OUTCOME,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "capturePublishingRecordsAwaitingAcknowledgement",
        kind: InstrumentKind::UpDownCounter,
        unit: "records",
        description: "Accepted capture records awaiting Kafka acknowledgement",
        dimensions: RECORD_KIND,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "capturePublishingBytesAwaitingAcknowledgement",
        kind: InstrumentKind::UpDownCounter,
        unit: "By",
        description: "Accepted capture payload bytes awaiting Kafka acknowledgement",
        dimensions: RECORD_KIND,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureHeartbeatSubmissions",
        kind: InstrumentKind::Counter,
        unit: "records",
        description: "Writer-partition heartbeat records accepted for publication",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureHeartbeatAcknowledgements",
        kind: InstrumentKind::Counter,
        unit: "records",
        description: "Writer-partition heartbeat acknowledgements accepted",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureHeartbeatDeadlineViolations",
        kind: InstrumentKind::Counter,
        unit: "violations",
        description: "Writer-partition heartbeat acknowledgement deadlines exceeded",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "captureBrokerTimeViolations",
        kind: InstrumentKind::Counter,
        unit: "violations",
        description: "Acknowledgements rejected by capture broker-time continuity checks",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureRoutingTransitions",
        kind: InstrumentKind::Counter,
        unit: "transitions",
        description: "Capture assignment and routing-generation transitions",
        dimensions: TRANSITION,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureWritersActive",
        kind: InstrumentKind::UpDownCounter,
        unit: "writers",
        description: "Writer-partition lanes in the active routing generation",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "captureWritersDraining",
        kind: InstrumentKind::UpDownCounter,
        unit: "writers",
        description: "Superseded writer-partition lanes draining existing connections",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "captureWritersRetired",
        kind: InstrumentKind::Counter,
        unit: "writers",
        description: "Writer-partition lanes retired after draining",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureWriteGateTransitions",
        kind: InstrumentKind::Counter,
        unit: "transitions",
        description: "Irreversible transitions that close the capture Kafka write gate",
        dimensions: REASON_FAILURE,
        buckets: None,
        exposure: Exposure::RunbookDocumented,
    },
    Instrument {
        name: "captureWritesRejected",
        kind: InstrumentKind::Counter,
        unit: "writes",
        description: "Capture write submissions rejected after the write gate closed",
        dimensions: NO_DIMENSIONS,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
    Instrument {
        name: "capturePublisherFailures",
        kind: InstrumentKind::Counter,
        unit: "failures",
        description: "Terminal capture Kafka publisher failures by bounded reason",
        dimensions: REASON_FAILURE,
        buckets: None,
        exposure: Exposure::ExternallyConsumed,
    },
];

/// Looks up a contract row by instrument name.
#[must_use]
pub fn instrument(name: &str) -> Option<&'static Instrument> {
    INSTRUMENTS.iter().find(|row| row.name == name)
}

/// The maximum number of metric series one proxy process can publish.
///
/// Finite because every dimension in [`INSTRUMENTS`] is closed or process-scoped. A histogram's
/// bucket series are not counted here; this bounds the attribute-set cardinality, which is the axis
/// a dashboard gets swamped on.
#[must_use]
pub fn total_series_per_process() -> usize {
    INSTRUMENTS.iter().map(Instrument::series_per_process).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn instrument_names_are_unique() {
        let mut seen = BTreeSet::new();
        for row in INSTRUMENTS {
            assert!(
                seen.insert(row.name),
                "duplicate instrument name {}",
                row.name
            );
        }
        assert_eq!(seen.len(), 47, "the contract is 47 instruments");
    }

    #[test]
    fn every_activity_generates_its_three_instruments() {
        for activity in ACTIVITIES {
            for name in [
                activity_count_name(activity.name),
                activity_duration_name(activity.name),
                activity_exception_name(activity.name),
            ] {
                assert!(
                    instrument(&name).is_some(),
                    "generated instrument {name} is missing from the contract"
                );
            }
        }
        assert_eq!(ACTIVITIES.len() * 3, 18, "six activities, eighteen series");
    }

    #[test]
    fn duration_histograms_use_the_declared_bucket_set() {
        for activity in ACTIVITIES {
            let name = activity_duration_name(activity.name);
            let row = instrument(&name).expect("duration histogram in contract");
            assert_eq!(
                row.buckets,
                Some(activity.buckets.bounds()),
                "{name} bucket set"
            );
            assert_eq!(row.unit, "ms", "{name} unit");
            assert_eq!(row.kind, InstrumentKind::Histogram, "{name} kind");
        }
    }

    #[test]
    fn every_gate_latency_instrument_uses_the_gate_bucket_set() {
        for name in GATE_LATENCY_INSTRUMENTS {
            let row = instrument(name).unwrap_or_else(|| panic!("{name} missing from contract"));
            assert_eq!(
                row.buckets,
                Some(GATE_LATENCY_BUCKETS_MS),
                "{name} is gate-relevant but is not bucketed for it"
            );
        }
        // And nothing else does, so the gate set stays the explicit, reviewed list.
        for activity in ACTIVITIES {
            let name = activity_duration_name(activity.name);
            assert_eq!(
                activity.buckets == BucketSet::Gate,
                GATE_LATENCY_INSTRUMENTS.contains(&name.as_str()),
                "{name} bucket choice disagrees with GATE_LATENCY_INSTRUMENTS"
            );
        }
    }

    /// The whole point of the gate bucket set is resolution below 5 ms, where 99.045% of Java's
    /// acknowledgement samples live.
    #[test]
    fn the_gate_bucket_set_resolves_below_five_milliseconds() {
        let below_five = GATE_LATENCY_BUCKETS_MS
            .iter()
            .filter(|bound| **bound <= 5.0)
            .count();
        assert!(
            below_five >= 6,
            "only {below_five} bounds at or below 5 ms; Gate 5 would stay undecidable"
        );
        // The Java mean of 1.249 ms must be bracketed on both sides.
        assert!(GATE_LATENCY_BUCKETS_MS.contains(&1.0));
        assert!(GATE_LATENCY_BUCKETS_MS.contains(&2.5));
        // No extra Prometheus series compared with the Java default.
        assert_eq!(
            GATE_LATENCY_BUCKETS_MS.len(),
            DURATION_BUCKETS_MS.len(),
            "the re-bucketing must not change the exported series count"
        );
        // The slow tail stays distinguishable from the overflow bucket.
        assert_eq!(GATE_LATENCY_BUCKETS_MS.last(), Some(&30_000.0));
    }

    /// The Java default's first bound is why Gate 5 was undecidable; record that as an assertion so
    /// nobody restores it for the gate-relevant histograms.
    #[test]
    fn the_java_default_bucket_set_cannot_resolve_a_gate() {
        assert_eq!(
            &DURATION_BUCKETS_MS[..2],
            [0.0, 5.0].as_slice(),
            "the Java default's first non-zero bound is 5 ms, which is the whole problem"
        );
        for name in GATE_LATENCY_INSTRUMENTS {
            let row = instrument(name).expect("declared");
            assert_ne!(row.buckets, Some(DURATION_BUCKETS_MS), "{name}");
        }
    }

    #[test]
    fn long_buckets_extend_the_default_set() {
        assert_eq!(
            &LONG_DURATION_BUCKETS_MS[..DURATION_BUCKETS_MS.len()],
            DURATION_BUCKETS_MS
        );
        assert_eq!(
            &LONG_DURATION_BUCKETS_MS[DURATION_BUCKETS_MS.len()..],
            &[
                60_000.0,
                120_000.0,
                300_000.0,
                600_000.0,
                1_800_000.0,
                3_600_000.0
            ]
        );
    }

    #[test]
    fn bucket_bounds_are_sorted_and_unique() {
        for bounds in [DURATION_BUCKETS_MS, LONG_DURATION_BUCKETS_MS] {
            for window in bounds.windows(2) {
                assert!(window[0] < window[1], "bounds must be strictly increasing");
            }
        }
    }

    #[test]
    fn exported_series_names_match_the_verified_consumer_strings() {
        // Every pair below is a literal string a checked-in consumer queries. Left column cites the
        // consumer; see the crate-level docs for the full consumer table.
        let verified: &[(&str, &str)] = &[
            // generate-capture-replay-grafana-dashboard.py
            ("fullRequest", "fullRequest_count_total"),
            ("bytesRead", "bytesRead_bytes_total"),
            ("bytesWritten", "bytesWritten_bytes_total"),
            (
                "capturePublishingRecordsAccepted",
                "capturePublishingRecordsAccepted_records_total",
            ),
            (
                "capturePublishingBytesAccepted",
                "capturePublishingBytesAccepted_bytes_total",
            ),
            (
                "capturePublishingRecordsAwaitingAcknowledgement",
                "capturePublishingRecordsAwaitingAcknowledgement_records",
            ),
            (
                "captureConnectionsActive",
                "captureConnectionsActive_connections",
            ),
            ("captureWritersActive", "captureWritersActive_writers"),
            ("captureWritersDraining", "captureWritersDraining_writers"),
            ("captureGapActive", "captureGapActive_processes"),
            (
                "captureWritesRejected",
                "captureWritesRejected_writes_total",
            ),
            (
                "capturePublisherFailures",
                "capturePublisherFailures_failures_total",
            ),
            (
                "captureHeartbeatDeadlineViolations",
                "captureHeartbeatDeadlineViolations_violations_total",
            ),
            (
                "gatheringRequestDuration",
                "gatheringRequestDuration_milliseconds",
            ),
            (
                "gatheringRequestExceptionCount",
                "gatheringRequestExceptionCount_total",
            ),
            // deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json:882,1312
            ("activeConnection", "activeConnection_count"),
            // migrationConsole/lib/integ_test/integ_test/replayer_tests.py:69
            ("kafkaCommitCount", "kafkaCommitCount_total"),
            // generate-capture-replay-grafana-dashboard.py:250-252
            (
                "gatheringResponseDuration",
                "gatheringResponseDuration_milliseconds",
            ),
            ("kafkaCommitDuration", "kafkaCommitDuration_milliseconds"),
        ];
        for (name, expected_series) in verified {
            let row = instrument(name).unwrap_or_else(|| panic!("{name} missing from contract"));
            assert_eq!(
                &row.exported_series_name(),
                expected_series,
                "{name} exports the wrong Prometheus series; a consumer query would go blank"
            );
        }
    }

    #[test]
    fn no_dimension_is_a_traffic_identity() {
        for row in INSTRUMENTS {
            for dimension in row.dimensions {
                assert!(
                    !FORBIDDEN_DIMENSION_KEYS.contains(&dimension.key),
                    "{} carries identity dimension {}; identities belong in logs",
                    row.name,
                    dimension.key
                );
            }
        }
    }

    #[test]
    fn every_dimension_value_set_is_closed_or_process_scoped() {
        for row in INSTRUMENTS {
            for dimension in row.dimensions {
                match dimension.domain {
                    DimensionDomain::Closed(values) => {
                        assert!(
                            !values.is_empty(),
                            "{}/{} declares an empty value set",
                            row.name,
                            dimension.key
                        );
                        let unique: BTreeSet<_> = values.iter().collect();
                        assert_eq!(
                            unique.len(),
                            values.len(),
                            "{}/{} repeats a value",
                            row.name,
                            dimension.key
                        );
                    }
                    DimensionDomain::ProcessIdentity => {
                        assert!(
                            dimension.key == "processId" || dimension.key == "captureActivationId",
                            "{} is not one of the two process-identity keys",
                            dimension.key
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn total_cardinality_is_small_and_bounded() {
        // 47 instruments; the widest are captureWriteGateTransitions and capturePublisherFailures,
        // each at the 8 closed FailureReason values.
        let total = total_series_per_process();
        assert_eq!(
            total, 75,
            "per-process series count changed; re-check the dashboards"
        );
        for row in INSTRUMENTS {
            assert!(
                row.series_per_process() <= 8,
                "{} can emit {} series from one process",
                row.name,
                row.series_per_process()
            );
        }
    }

    #[test]
    fn units_are_from_the_declared_set() {
        let allowed = [
            "",
            "count",
            "bytes",
            "By",
            "ms",
            "records",
            "connections",
            "writers",
            "processes",
            "writes",
            "failures",
            "violations",
            "transitions",
        ];
        for row in INSTRUMENTS {
            assert!(
                allowed.contains(&row.unit),
                "{} declares unknown unit {:?}",
                row.name,
                row.unit
            );
        }
    }

    #[test]
    fn only_histograms_declare_buckets() {
        for row in INSTRUMENTS {
            assert_eq!(
                row.buckets.is_some(),
                row.kind == InstrumentKind::Histogram,
                "{} bucket declaration does not match its kind",
                row.name
            );
        }
    }

    #[test]
    fn externally_consumed_set_is_exactly_the_verified_list() {
        // Independently verified by grepping every instrument name across *.py, *.sh, *.md, *.json,
        // *.yaml, *.ts outside Java source. Changing this set means a dashboard, operator script,
        // or integration test must change in the same commit. tests/dashboard_contract.rs
        // re-derives this set from the shipped dashboards and scripts and requires equality.
        let expected: BTreeSet<&str> = [
            "fullRequest",
            "bytesRead",
            "bytesWritten",
            "gatheringRequestDuration",
            "waitingForResponseDuration",
            "gatheringResponseDuration",
            "kafkaCommitDuration",
            "captureConnectionDuration",
            "gatheringRequestExceptionCount",
            "gatheringResponseExceptionCount",
            "kafkaCommitExceptionCount",
            "captureConnectionsActive",
            "captureConnectionCloses",
            "captureGapActive",
            "capturePublishingRecordsAccepted",
            "capturePublishingBytesAccepted",
            "capturePublishingCompletions",
            "capturePublishingRecordsAwaitingAcknowledgement",
            "captureWritersActive",
            "captureWritersDraining",
            "capturePublisherFailures",
            "captureWritesRejected",
            "captureHeartbeatDeadlineViolations",
            // Found by this crate's own consumer sweep, not listed in the input specification:
            // deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json:882,1312
            "activeConnection",
            // migrationConsole/lib/integ_test/integ_test/replayer_tests.py:69 asserts the proxy
            // scope exports kafkaCommitCount_total.
            "kafkaCommitCount",
        ]
        .into_iter()
        .collect();
        let actual: BTreeSet<&str> = INSTRUMENTS
            .iter()
            .filter(|row| row.exposure == Exposure::ExternallyConsumed)
            .map(|row| row.name)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unit_normalisation_matches_the_exporter() {
        assert_eq!(normalise_prometheus_unit("By"), "bytes");
        assert_eq!(normalise_prometheus_unit("bytes"), "bytes");
        assert_eq!(normalise_prometheus_unit("ms"), "milliseconds");
        assert_eq!(normalise_prometheus_unit("count"), "count");
        assert_eq!(normalise_prometheus_unit("records"), "records");
        assert_eq!(normalise_prometheus_unit(""), "");
    }
}
