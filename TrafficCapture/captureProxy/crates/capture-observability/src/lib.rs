//! The capture proxy's metric, span, and operational-event contracts.
//!
//! This crate is the proxy's whole observability surface: 47 instruments, the spans that correlate
//! them to traffic, the bounded `capture_*` event set an operator greps for, and the OTLP wiring
//! that ships all of it. Its job is to make that surface **hard to break**, because most of it is a
//! contract with software outside this repository's build.
//!
//! # What is frozen, and by whom
//!
//! Metric **names, units, and dimension keys** are an external contract. Twenty-five of the 47
//! instruments are read by checked-in consumers:
//!
//! | Consumer | Reads |
//! |---|---|
//! | `deployment/k8s/scripts/generate-capture-replay-dashboard.py` | the `CloudWatch` dashboard, checked in under `deployment/k8s/dashboards/` and the Argo chart |
//! | `deployment/k8s/scripts/generate-capture-replay-grafana-dashboard.py` | the Grafana dashboard, checked in as `grafanaDashboard.yaml`, using **unit-suffixed** series names |
//! | `deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json` | `fullRequest_count_total`, `activeConnection_count`, and the request/response duration buckets |
//! | `deployment/cdk/opensearch-service-migration/lib/components/capture-replay-dashboard.json` | `bytesRead` and `fullRequest` in the `captureProxy` scope, on the older CDK/ECS path |
//! | `deployment/k8s/scripts/capture-replay-gameday.sh` | `capturePublishingRecordsAwaitingAcknowledgement*` by prefix, and the proxy's logs by `event=capture_` |
//! | `migrationConsole/lib/integ_test/integ_test/replayer_tests.py` | asserts the proxy scope exports `kafkaCommitCount_total` |
//! | `migrationConsole/lib/integ_test/integ_test/metric_operations.py` | asserts recent data for `bytesRead` in the `captureProxy` scope |
//! | `docs/captureAndReplay/observabilityOperations.md` | the runbook's metric inventory and the event-name list |
//!
//! The **unit string is as load-bearing as the name**: the Prometheus exporter derives the series
//! suffix from it, so `captureWritersActive` with unit `writers` publishes
//! `captureWritersActive_writers` and re-uniting it to `count` would blank the panel with no error
//! anywhere. [`contract::Instrument::exported_series_name`] reproduces that derivation and
//! `tests/metric_contract.rs` asserts the consumer-visible strings directly.
//!
//! **No alarm, alert rule, or notification target in this repository references any capture-proxy
//! instrument.** The only Prometheus alert rules are Kafka broker and PVC infrastructure rules
//! (`kafkaStorageAlerts.yaml`), and the only `CloudWatch` alarms are ACK-managed Kafka PVC alarms.
//! Dashboards, the runbook, the gameday script, and two integration tests are therefore the entire
//! external contract.
//!
//! Log **formatting** is not frozen — [`logging`] renders a Rust-native structured format and can
//! emit JSON. The required **events** and their correlation fields are frozen, and so is the
//! literal `event=capture_` substring.
//!
//! # Layout
//!
//! | Module | Rows | Responsibility |
//! |---|---|---|
//! | [`contract`] | all | the frozen instrument table as data, plus the Prometheus suffix derivation |
//! | [`root`] | HTTP-13, PROXY-04, WIRE-11, KAFKA-12, HTTP-11 | one explicit registry replacing Java's root-context interface hierarchy |
//! | [`logging`] | PROXY-14, PROXY-15, PROXY-16 | the `tracing-subscriber` stack, per-component filters, native-dependency diagnostics |
//! | [`process_metrics`] | PROXY-02 | process identity, operating mode, the capture-gap latch |
//! | [`connection_metrics`] | HTTP-10 | live connection count and bounded close reasons |
//! | [`kafka_metrics`] | KAFKA-11 | the sixteen Kafka publication, routing, and write-gate instruments |
//! | [`kafka_record_span`] | KAFKA-13 | per-record correlation and publish latency |
//! | [`connection_span`] | WIRE-10 | connection identity, lifetime, byte totals |
//! | [`wire_activity`] | HTTP-12, HTTP-14 | typed request-phase handles and the wire counters |
//! | [`activity`] | — | the generated `Count`/`Duration`/`ExceptionCount` family |
//! | [`log_deduper`] | PROXY-13 | bounded diagnostics for unauthenticated clients |
//! | [`events`] | §8 | the `capture_*` event names and the two verbatim summary lines |
//! | [`export`] | — | OTLP exporters, enabled per signal, no-op when unconfigured |
//! | [`snapshot`] | — | an in-process metric sink, so the contract is asserted against the real SDK |
//!
//! # The performance contract
//!
//! Telemetry was **49.9% of Java-thread CPU** in the capture proxy and **21% of its allocation**,
//! and **23.1% of all Java-thread CPU was sorted-map and comparator work** inside
//! `ActiveContextTracker` and `ActiveContextTrackerByActivityType` — two `ConcurrentSkipListMap`s
//! with a composite comparator, touched on every span open and close. Disabling the
//! `OpenTelemetry` endpoint
//! recovered only 12% of that, so the cost was structural in the live-context bookkeeping rather
//! than in export. This crate is therefore the largest single performance lever in the rewrite,
//! and the rules that keep the win are not optional:
//!
//! - **There is no registry of live spans or activities.** Java's two context trackers have no
//!   structural counterpart, and nothing here keeps a process-wide collection keyed by a composite
//!   comparator. Phase and span timing is a plain monotonic [`std::time::Instant`] held in a handle
//!   on the caller's stack, in connection-local state.
//! - **Instrument handles are resolved once**, when [`root::RootTelemetry`] is built at start-up.
//!   Nothing on the data path looks an instrument up by name.
//! - **Hot-path instruments are dimensionless**, which puts each observation on the SDK's
//!   empty-attribute fast path: one atomic update, no hashing, no lock, no allocation
//!   (`opentelemetry_sdk-0.31.0/src/metrics/internal/mod.rs:103-107`). The only attributed
//!   per-event instruments are the Kafka `recordKind` ones, whose dimension is part of the external
//!   contract; their attribute sets are pre-built at start-up so the record path still allocates
//!   nothing.
//! - **Nothing crossing a per-request or per-record boundary owns heap state.**
//!   [`kafka_record_span::RecordCorrelation`] is borrowed and `Copy` for exactly this reason.
//! - **Spans are created at `debug`.** With no trace collector and the console at its default
//!   `info`, every span macro is a disabled-callsite check and its field expressions are never
//!   evaluated, while the externally consumed metrics are recorded unconditionally.
//!
//! `tests/performance_contract.rs` enforces these as tests rather than as prose.
//!
//! # Latency histograms a performance gate reads
//!
//! Phase-1 Gate 5 ("Kafka acknowledgement p99 within 10% of Java") and Gate 3 / F4 (proxy-added
//! p99 latency) were **undecidable** against Java's histogram buckets, whose lowest bound is 5 ms:
//! 99.045% of the Java baseline's `kafkaCommitDuration` samples landed in that single `(0, 5]`
//! bucket, so the only honest statements from it are "mean = 1.249 ms" and "p99 <= 5 ms".
//! [`contract::GATE_LATENCY_BUCKETS_MS`] replaces those bounds for the four gate-relevant
//! histograms listed in [`contract::GATE_LATENCY_INSTRUMENTS`], with six resolution points below
//! 5 ms and no increase in exported series count. **The same bounds must be applied to the Java
//! baseline through an `OpenTelemetry` view before the gates are measured**, or the comparison is
//! between a 17-bucket histogram and a 1-bucket one.
//!
//! # The division of labour that keeps cardinality bounded
//!
//! Traffic identities — connection, record, writer, partition, request — go on **spans and log
//! events**. Metric dimensions come only from closed enums, plus the two process-identity keys that
//! contribute exactly one series per process. No metric dimension in this crate is a free-form
//! string, which is why [`contract::total_series_per_process`] is a finite number a test can assert
//! (`observabilityOperations.md:13, 19-20`; `CaptureConnectionMetrics.java:14-16`;
//! `CapturePublishingMetrics.java:16-18`).
//!
//! # Start-up order
//!
//! ```no_run
//! use capture_observability::{events, export, logging, process_metrics, root};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let identity = process_metrics::ProcessIdentity::detect();
//! let endpoints = export::CollectorEndpoints::default(); // from configuration
//! let telemetry = export::install(&endpoints, identity.process_id())?;
//! logging::install(&logging::LoggingConfig::from_env(), telemetry.tracer_provider())?;
//!
//! let trace_export_enabled = telemetry.trace_export_enabled();
//! let metric_export_enabled = telemetry.metric_export_enabled();
//! let registry = root::RootTelemetry::new(telemetry, identity);
//!
//! events::log_startup_summary(&events::StartupSummary {
//!     identity: registry.identity(),
//!     listen_port: 9200,
//!     capture_mode: events::CaptureMode::Kafka,
//!     kafka_topic: Some("logging-traffic-topic"),
//!     failure_policy: events::FailurePolicyLabel::FailClosed,
//!     trace_export_enabled,
//!     metric_export_enabled,
//! });
//! # Ok(())
//! # }
//! ```
//!
//! [`export::install`] must run before [`logging::install`] so the trace layer can be attached, and
//! both must run before the listener opens.

#![warn(missing_docs)]

pub mod activity;
pub mod connection_metrics;
pub mod connection_span;
pub mod contract;
pub mod events;
pub mod export;
mod instruments;
pub mod kafka_metrics;
pub mod kafka_record_span;
pub mod log_deduper;
pub mod logging;
pub mod process_metrics;
pub mod root;
pub mod snapshot;
pub mod wire_activity;

pub use contract::{INSTRUMENTS, Instrument, InstrumentKind, SCOPE_NAME, SERVICE_NAME};
pub use root::RootTelemetry;
