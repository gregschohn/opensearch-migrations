//! The one telemetry registry (HTTP-13, PROXY-04, WIRE-11, KAFKA-12, HTTP-11).
//!
//! Java reaches this same set of instruments through a hierarchy of root-context interfaces —
//! `IRootOffloaderContext`, `IRootKafkaOffloaderContext`, `IRootWireLoggingContext` — that
//! `RootCaptureContext` composes by multiple interface inheritance. Each interface exists to hand
//! one subsystem a getter for the instrument family it owns, and `RootCaptureContext` exists to
//! satisfy all of them at once. The inheritance is pure plumbing: nothing dispatches on it, and the
//! four Retired traceability rows say so.
//!
//! [`RootTelemetry`] replaces the whole hierarchy with one struct holding explicit, named handles.
//! A subsystem is given the handle it needs, not a trait object that could reach anything. There is
//! no trait, no `dyn`, and no way to construct a registry that is missing an instrument: every
//! instrument is a non-optional field, built from [`crate::contract`] at construction time, so a
//! renamed or deleted contract row is a compile or start-up failure rather than a silently absent
//! series.
//!
//! # What this registry deliberately does not do
//!
//! It does **not** track live contexts. Java's root contexts carry an `IContextTracker`, whose
//! production implementations `ActiveContextTracker` and `ActiveContextTrackerByActivityType` are
//! `ConcurrentSkipListMap`s with a composite comparator that every span insert into and remove
//! from. Profiling attributed **23.1% of all Java-thread CPU** to that sorted-map and comparator
//! work, and disabling the `OpenTelemetry` endpoint recovered only 12% of it — the cost is the
//! bookkeeping,
//! not the export.
//!
//! There is therefore no `on_context_created` / `on_context_closed` pair here, no live-span
//! collection, and no way to enumerate open activities. Anything in the Java sources that appears
//! to require one is an implementation artefact, not a contract: no dashboard, runbook, script, or
//! test reads a live-context count. Span lifetime is tracked by the handle the caller holds, and
//! `tracing-subscriber`'s registry — a sharded slab with O(1) insert and removal and no ordering —
//! does the only live-span bookkeeping that remains, and only when a subscriber is actually
//! interested in the span.
//!
//! # One scope, deliberately
//!
//! Java declares two scope names (`captureProxy` and `NettyCapture`) but the production proxy
//! overrides the second, so every instrument lands in `captureProxy`. Both shipped dashboards
//! filter on that scope exclusively, which makes the second scope name a trap rather than a
//! feature. This registry has one meter and therefore one scope.

use std::sync::Arc;

use crate::connection_metrics::ConnectionMetrics;
use crate::connection_span::{ConnectionSpan, ConnectionSpanInstruments};
use crate::export::TelemetryExport;
use crate::kafka_metrics::KafkaMetrics;
use crate::kafka_record_span::{KafkaRecordInstruments, KafkaRecordSpan, RecordCorrelation};
use crate::process_metrics::{CaptureProcessMetrics, ProcessIdentity};
use crate::wire_activity::{CapturingConnection, WireInstruments, WirePhaseTracker};

/// Every instrument and span factory the capture proxy has.
///
/// Built once at start-up and shared as `Arc<RootTelemetry>`. Instrument handles are themselves
/// cheap to clone, so handing a subsystem its own handle costs one atomic increment, not a lookup.
#[derive(Debug)]
pub struct RootTelemetry {
    export: TelemetryExport,
    identity: ProcessIdentity,
    process: CaptureProcessMetrics,
    connections: ConnectionMetrics,
    kafka: KafkaMetrics,
    wire: Arc<WireInstruments>,
    connection_spans: Arc<ConnectionSpanInstruments>,
    kafka_records: Arc<KafkaRecordInstruments>,
}

impl RootTelemetry {
    /// Registers every contracted instrument on the export's meter.
    ///
    /// # Panics
    ///
    /// Panics when an instrument name used by a submodule is absent from
    /// [`crate::contract::INSTRUMENTS`], or is declared there with a different kind or bucket set.
    /// That is an internal invariant: the contract table and the registry are compiled together, and
    /// a process that came up publishing an unexpected series would quietly break a dashboard
    /// instead of failing loudly.
    // DEVIATION: Java's root contexts carry an IContextTracker, implemented in production by
    // ActiveContextTracker and ActiveContextTrackerByActivityType -- ConcurrentSkipListMaps with a
    // composite comparator that every span inserts into and removes from. Profiling attributed
    // 23.1% of all Java-thread CPU to that sorted-map and comparator work, and disabling the
    // OpenTelemetry endpoint recovered only 12% of it. There is no counterpart here: no
    // on_context_created/on_context_closed pair, no live-span collection, no way to enumerate open
    // activities. No dashboard, runbook, script, or test reads a live-context count.
    #[must_use]
    pub fn new(export: TelemetryExport, identity: ProcessIdentity) -> Self {
        let meter = export.meter();
        Self {
            process: CaptureProcessMetrics::new(&meter, &identity),
            connections: ConnectionMetrics::new(&meter),
            kafka: KafkaMetrics::new(&meter),
            wire: Arc::new(WireInstruments::new(&meter)),
            connection_spans: Arc::new(ConnectionSpanInstruments::new(&meter)),
            kafka_records: Arc::new(KafkaRecordInstruments::new(&meter)),
            identity,
            export,
        }
    }

    /// This process's identity, which keys both the capture-gap metrics and the summary log lines.
    #[must_use]
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    /// The installed exporters, for flush and shutdown.
    #[must_use]
    pub fn export(&self) -> &TelemetryExport {
        &self.export
    }

    /// Capture-process state and capture-gap instruments (PROXY-02).
    #[must_use]
    pub fn process(&self) -> &CaptureProcessMetrics {
        &self.process
    }

    /// Source-connection count and close reasons (HTTP-10).
    #[must_use]
    pub fn connections(&self) -> &ConnectionMetrics {
        &self.connections
    }

    /// Kafka publication, routing, and write-gate instruments (KAFKA-11).
    #[must_use]
    pub fn kafka(&self) -> &KafkaMetrics {
        &self.kafka
    }

    /// An owned wire-phase driver for one connection, for callback-driven data planes.
    #[must_use]
    pub fn wire_phases(&self) -> WirePhaseTracker {
        WirePhaseTracker::new(Arc::clone(&self.wire))
    }

    /// Records one traffic-record publication's submit-to-delivery latency (`kafkaCommit*`).
    pub fn record_kafka_commit(&self, elapsed: std::time::Duration) {
        self.kafka_records.record_publication(elapsed);
    }

    /// Counts one failed traffic-record publication (`kafkaCommitExceptionCount`).
    pub fn record_kafka_commit_failure(&self) {
        self.kafka_records.record_publication_failure();
    }

    /// Begins observing one source connection's wire phases (HTTP-12, HTTP-14).
    #[must_use]
    pub fn capture_connection(&self, connection_id: impl Into<Arc<str>>) -> CapturingConnection {
        CapturingConnection::new(Arc::clone(&self.wire), connection_id)
    }

    /// Opens the lifetime span for one source connection (WIRE-10).
    #[must_use]
    pub fn open_connection_span(
        &self,
        connection_id: impl Into<Arc<str>>,
        node_id: impl Into<Arc<str>>,
    ) -> ConnectionSpan {
        ConnectionSpan::open(Arc::clone(&self.connection_spans), connection_id, node_id)
    }

    /// Begins observing one capture-record publication (KAFKA-13).
    #[must_use]
    pub fn begin_record_publication(&self, correlation: RecordCorrelation) -> KafkaRecordSpan {
        KafkaRecordSpan::begin(Arc::clone(&self.kafka_records), correlation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{self, SCOPE_NAME};
    use crate::export::{CollectorEndpoints, install};

    #[test]
    fn a_registry_builds_every_contracted_instrument_without_an_exporter() {
        let export = install(&CollectorEndpoints::default(), "process-root").expect("install");
        let registry = RootTelemetry::new(
            export,
            ProcessIdentity::new("process-root", "activation-root"),
        );
        assert_eq!(registry.identity().process_id(), "process-root");
        assert!(!registry.export().trace_export_enabled());
        assert!(!registry.export().metric_export_enabled());
    }

    #[test]
    fn the_registry_exposes_one_handle_per_traceability_row() {
        let export = install(&CollectorEndpoints::default(), "process-rows").expect("install");
        let registry = RootTelemetry::new(export, ProcessIdentity::new("p", "a"));
        // Each of these is the single entry point a subsystem gets; no subsystem receives the
        // registry itself, which is what replaces Java's root-context interfaces.
        let _ = registry.process();
        let _ = registry.connections();
        let _ = registry.kafka();
        let connection = registry.capture_connection("c-1");
        drop(connection.begin_request());
        registry.open_connection_span("c-1", "node-1").close();
        registry
            .begin_record_publication(RecordCorrelation {
                connection_id: "c-1",
                writer: "w-0",
                topic: "t",
                partition: 0,
                generation: 1,
                record_index: 0,
                record_size: 1,
            })
            .acknowledged(0);
    }

    #[test]
    fn the_meter_scope_is_the_one_the_dashboards_query() {
        assert_eq!(SCOPE_NAME, "captureProxy");
        assert_eq!(contract::SERVICE_NAME, "capture");
    }
}
