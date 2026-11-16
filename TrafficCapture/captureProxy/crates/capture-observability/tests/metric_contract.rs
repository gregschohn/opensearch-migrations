//! The metric contract, asserted against the real `OpenTelemetry` pipeline.
//!
//! This is the crate's main proof. It builds the actual registry on a real `SdkMeterProvider`,
//! drives **every ordinary recording path** at least once and every closed dimension value at least
//! once, exports, and then compares what came out against
//! [`capture_observability::contract::INSTRUMENTS`] in both directions.
//!
//! Three separate things are checked, because three different mistakes are possible:
//!
//! 1. [`the_instrument_contract_snapshot_is_unchanged`] is a literal snapshot of name, kind, unit,
//!    dimension keys, and the derived Prometheus series name for all 47 instruments. It is a second,
//!    independent statement of the contract, so a rename or a re-unit shows up as a readable diff
//!    rather than as a blank dashboard panel in production.
//! 2. [`every_exported_point_matches_its_contract_row`] checks the pipeline against the table:
//!    nothing exports that is not declared, nothing declared fails to export, and every point's
//!    unit, kind, bucket bounds, scope, and attribute keys are the declared ones.
//! 3. [`cardinality_is_closed_and_enumerable`] checks that no dimension value escaped its enum, so
//!    a dashboard cannot be swamped by unbounded label values.

use std::collections::{BTreeMap, BTreeSet};

use capture_observability::connection_metrics::CloseReason;
use capture_observability::contract::{
    self, DimensionDomain, Exposure, Instrument, InstrumentKind, SCOPE_NAME,
};
use capture_observability::export::{CollectorEndpoints, TelemetryExport};
use capture_observability::kafka_metrics::{
    FailureReason, PublishOutcome, RecordKind, RoutingTransition,
};
use capture_observability::kafka_record_span::RecordCorrelation;
use capture_observability::process_metrics::{CaptureProcessState, ProcessIdentity};
use capture_observability::root::RootTelemetry;
use capture_observability::snapshot::{MetricSnapshot, recording_meter_provider};

const PROCESS_ID: &str = "process-contract";
const ACTIVATION_ID: &str = "activation-contract";

/// Drives every recording path in the crate and returns what the pipeline exported.
fn exercise_everything() -> MetricSnapshot {
    let (provider, recorder) = recording_meter_provider(PROCESS_ID);
    let export = TelemetryExport::from_meter_provider(provider, false);
    let registry = RootTelemetry::new(export, ProcessIdentity::new(PROCESS_ID, ACTIVATION_ID));

    // --- process state (PROXY-02): both transitions and the gap latch ---
    registry
        .process()
        .record_transition(CaptureProcessState::PassThrough)
        .expect("pass-through is a transition");
    registry
        .process()
        .record_transition(CaptureProcessState::Terminating)
        .expect("terminating is a transition");

    // --- connections (HTTP-10): every close reason ---
    for reason in CloseReason::all() {
        registry.connections().connection_opened();
        registry.connections().connection_closed(*reason);
    }

    // --- connection span (WIRE-10) ---
    let span = registry.open_connection_span("c-1", "node-1");
    span.add_bytes_read(4_096);
    span.add_bytes_written(8_192);
    span.record_exception();
    span.close();

    // --- wire phases (HTTP-12, HTTP-14): every phase, every counter, every exception ---
    let connection = registry.capture_connection("c-1");
    let request = connection.begin_request();
    request.record_bytes_read(512);
    request.record_blocking_request();
    request.record_capture_suppressed();
    request.record_fully_parsed_request();
    request.record_exception();
    let blocking = request.block();
    blocking.record_exception();
    let waiting = blocking.await_response();
    waiting.record_exception();
    let response = waiting.gather_response();
    response.record_bytes_written(1_024);
    response.record_exception();
    let next = response.next_request();
    drop(next);
    connection.record_unregistered();
    connection.record_removed();

    // --- Kafka record spans (KAFKA-13): one acknowledged, one failed ---
    registry
        .begin_record_publication(correlation(0))
        .acknowledged(17);
    registry.begin_record_publication(correlation(1)).failed();

    // --- Kafka metrics (KAFKA-11): every kind, outcome, transition, and reason ---
    let kafka = registry.kafka();
    for kind in RecordKind::all() {
        for outcome in PublishOutcome::all() {
            kafka.publication_accepted(*kind, 2_048);
            kafka.publication_completed(*kind, *outcome, 2_048);
        }
    }
    kafka.heartbeat_deadline_violated();
    kafka.broker_time_violated();
    for transition in RoutingTransition::all() {
        kafka.routing_transition(*transition);
    }
    for reason in FailureReason::all() {
        kafka.write_gate_tripped(*reason);
        kafka.publisher_failed(*reason);
    }
    kafka.writers_activated(4, 0);
    kafka.writers_activated(4, 4);
    kafka.active_writers_began_draining(1);
    kafka.writer_retired();
    kafka.write_rejected();

    registry.export().force_flush();
    recorder.take()
}

fn correlation(record_index: u64) -> RecordCorrelation<'static> {
    RecordCorrelation {
        connection_id: "c-1",
        writer: "w-0",
        topic: "logging-traffic-topic",
        partition: 3,
        generation: 7,
        record_index,
        record_size: 2_048,
    }
}

fn render(row: &Instrument) -> String {
    let dimensions: Vec<&str> = row.dimensions.iter().map(|d| d.key).collect();
    let buckets = match row.buckets {
        None => "none",
        Some(bounds) if bounds == contract::GATE_LATENCY_BUCKETS_MS => "gate",
        Some(bounds) if bounds == contract::LONG_DURATION_BUCKETS_MS => "long",
        Some(bounds) if bounds == contract::DURATION_BUCKETS_MS => "java-default",
        Some(_) => "unrecognised",
    };
    format!(
        "{name} kind={kind} unit={unit:?} dims=[{dims}] buckets={buckets} series={series} \
         exposure={exposure}",
        name = row.name,
        kind = row.kind.as_str(),
        unit = row.unit,
        dims = dimensions.join(","),
        series = row.exported_series_name(),
        exposure = match row.exposure {
            Exposure::ExternallyConsumed => "external",
            Exposure::RunbookDocumented => "runbook",
            Exposure::Internal => "internal",
        }
    )
}

/// The frozen snapshot.
///
/// Every line was checked against the Java declaration it replaces: the generated families against
/// `CommonMetricInstruments.java:12-14` and `CommonScopedMetricInstruments.java:44-50, 86-91`, the
/// wire counters against `WireCaptureContexts.java:35-36, 137-144, 260` with
/// `COUNT_UNITS`/`BYTES_UNIT` from lines 13-14, `activeConnection` against
/// `ConnectionContext.java:44-46`, the connection pair against `CaptureConnectionMetrics.java:36-43`,
/// the process trio against `CaptureProcessMetrics.java:60-71`, and the sixteen Kafka instruments
/// against `CapturePublishingMetrics.java:101-167`.
///
/// If this assertion fails, the right response is almost never to update the snapshot. It is to
/// check whether the consumer named in the `exposure` column still works.
const EXPECTED_SNAPSHOT: &str = "\
activeConnection kind=up_down_counter unit=\"count\" dims=[] buckets=none series=activeConnection_count exposure=external
blockedCount kind=counter unit=\"\" dims=[] buckets=none series=blockedCount_total exposure=internal
blockedDuration kind=histogram unit=\"ms\" dims=[] buckets=gate series=blockedDuration_milliseconds exposure=internal
blockedExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=blockedExceptionCount_total exposure=internal
blockingRequest kind=counter unit=\"count\" dims=[] buckets=none series=blockingRequest_count_total exposure=internal
bytesRead kind=counter unit=\"bytes\" dims=[] buckets=none series=bytesRead_bytes_total exposure=external
bytesWritten kind=counter unit=\"bytes\" dims=[] buckets=none series=bytesWritten_bytes_total exposure=external
captureBrokerTimeViolations kind=counter unit=\"violations\" dims=[] buckets=none series=captureBrokerTimeViolations_violations_total exposure=runbook
captureConnectionCloses kind=counter unit=\"connections\" dims=[reason] buckets=none series=captureConnectionCloses_connections_total exposure=external
captureConnectionCount kind=counter unit=\"\" dims=[] buckets=none series=captureConnectionCount_total exposure=internal
captureConnectionDuration kind=histogram unit=\"ms\" dims=[] buckets=long series=captureConnectionDuration_milliseconds exposure=external
captureConnectionExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=captureConnectionExceptionCount_total exposure=internal
captureConnectionsActive kind=up_down_counter unit=\"connections\" dims=[] buckets=none series=captureConnectionsActive_connections exposure=external
captureGapActive kind=up_down_counter unit=\"processes\" dims=[processId,captureActivationId,mode,sourceForwardingContinued] buckets=none series=captureGapActive_processes exposure=external
captureHeartbeatAcknowledgements kind=counter unit=\"records\" dims=[] buckets=none series=captureHeartbeatAcknowledgements_records_total exposure=runbook
captureHeartbeatDeadlineViolations kind=counter unit=\"violations\" dims=[] buckets=none series=captureHeartbeatDeadlineViolations_violations_total exposure=external
captureHeartbeatSubmissions kind=counter unit=\"records\" dims=[] buckets=none series=captureHeartbeatSubmissions_records_total exposure=runbook
captureProcessPassThroughTransitions kind=counter unit=\"count\" dims=[processId,captureActivationId,mode,sourceForwardingContinued] buckets=none series=captureProcessPassThroughTransitions_count_total exposure=internal
captureProcessTerminationTransitions kind=counter unit=\"count\" dims=[processId,captureActivationId] buckets=none series=captureProcessTerminationTransitions_count_total exposure=internal
capturePublisherFailures kind=counter unit=\"failures\" dims=[reason] buckets=none series=capturePublisherFailures_failures_total exposure=external
capturePublishingBytesAccepted kind=counter unit=\"By\" dims=[recordKind] buckets=none series=capturePublishingBytesAccepted_bytes_total exposure=external
capturePublishingBytesAwaitingAcknowledgement kind=up_down_counter unit=\"By\" dims=[recordKind] buckets=none series=capturePublishingBytesAwaitingAcknowledgement_bytes exposure=runbook
capturePublishingCompletions kind=counter unit=\"records\" dims=[recordKind,outcome] buckets=none series=capturePublishingCompletions_records_total exposure=external
capturePublishingRecordsAccepted kind=counter unit=\"records\" dims=[recordKind] buckets=none series=capturePublishingRecordsAccepted_records_total exposure=external
capturePublishingRecordsAwaitingAcknowledgement kind=up_down_counter unit=\"records\" dims=[recordKind] buckets=none series=capturePublishingRecordsAwaitingAcknowledgement_records exposure=external
captureRoutingTransitions kind=counter unit=\"transitions\" dims=[transition] buckets=none series=captureRoutingTransitions_transitions_total exposure=runbook
captureSuppressed kind=counter unit=\"count\" dims=[] buckets=none series=captureSuppressed_count_total exposure=internal
captureWriteGateTransitions kind=counter unit=\"transitions\" dims=[reason] buckets=none series=captureWriteGateTransitions_transitions_total exposure=runbook
captureWritersActive kind=up_down_counter unit=\"writers\" dims=[] buckets=none series=captureWritersActive_writers exposure=external
captureWritersDraining kind=up_down_counter unit=\"writers\" dims=[] buckets=none series=captureWritersDraining_writers exposure=external
captureWritersRetired kind=counter unit=\"writers\" dims=[] buckets=none series=captureWritersRetired_writers_total exposure=runbook
captureWritesRejected kind=counter unit=\"writes\" dims=[] buckets=none series=captureWritesRejected_writes_total exposure=external
fullRequest kind=counter unit=\"count\" dims=[] buckets=none series=fullRequest_count_total exposure=external
gatheringRequestCount kind=counter unit=\"\" dims=[] buckets=none series=gatheringRequestCount_total exposure=internal
gatheringRequestDuration kind=histogram unit=\"ms\" dims=[] buckets=gate series=gatheringRequestDuration_milliseconds exposure=external
gatheringRequestExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=gatheringRequestExceptionCount_total exposure=external
gatheringResponseCount kind=counter unit=\"\" dims=[] buckets=none series=gatheringResponseCount_total exposure=internal
gatheringResponseDuration kind=histogram unit=\"ms\" dims=[] buckets=gate series=gatheringResponseDuration_milliseconds exposure=external
gatheringResponseExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=gatheringResponseExceptionCount_total exposure=external
kafkaCommitCount kind=counter unit=\"\" dims=[] buckets=none series=kafkaCommitCount_total exposure=external
kafkaCommitDuration kind=histogram unit=\"ms\" dims=[] buckets=gate series=kafkaCommitDuration_milliseconds exposure=external
kafkaCommitExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=kafkaCommitExceptionCount_total exposure=external
removed kind=counter unit=\"count\" dims=[] buckets=none series=removed_count_total exposure=internal
unregistered kind=counter unit=\"count\" dims=[] buckets=none series=unregistered_count_total exposure=internal
waitingForResponseCount kind=counter unit=\"\" dims=[] buckets=none series=waitingForResponseCount_total exposure=internal
waitingForResponseDuration kind=histogram unit=\"ms\" dims=[] buckets=long series=waitingForResponseDuration_milliseconds exposure=external
waitingForResponseExceptionCount kind=counter unit=\"\" dims=[] buckets=none series=waitingForResponseExceptionCount_total exposure=internal";

#[test]
#[ignore = "prints the current rendering so a reviewed snapshot can be refreshed deliberately"]
fn print_contract_rendering() {
    let mut rendered: Vec<String> = contract::INSTRUMENTS.iter().map(render).collect();
    rendered.sort();
    println!("{}", rendered.join("\n"));
}

#[test]
fn the_instrument_contract_snapshot_is_unchanged() {
    let mut rendered: Vec<String> = contract::INSTRUMENTS.iter().map(render).collect();
    rendered.sort();
    assert_eq!(
        rendered.join("\n"),
        EXPECTED_SNAPSHOT,
        "the metric contract changed; before updating this snapshot, check the consumer named in \
         the exposure column"
    );
}

#[test]
fn the_contract_is_exactly_forty_seven_instruments() {
    assert_eq!(contract::INSTRUMENTS.len(), 47);
    assert_eq!(EXPECTED_SNAPSHOT.lines().count(), 47);
}

#[test]
fn every_exported_point_matches_its_contract_row() {
    let snapshot = exercise_everything();
    assert!(!snapshot.points.is_empty(), "nothing was exported at all");

    for point in &snapshot.points {
        let row = contract::instrument(&point.name).unwrap_or_else(|| {
            panic!(
                "{} was exported but is not declared in the metric contract",
                point.name
            )
        });
        assert_eq!(
            point.scope, SCOPE_NAME,
            "{} landed in a foreign scope",
            row.name
        );
        assert_eq!(point.unit, row.unit, "{} exported the wrong unit", row.name);
        assert_eq!(point.kind, row.kind, "{} exported the wrong kind", row.name);
        assert_eq!(
            point.description, row.description,
            "{} exported the wrong description",
            row.name
        );
        assert_eq!(
            point.bucket_bounds.as_deref(),
            row.buckets,
            "{} exported the wrong bucket bounds",
            row.name
        );
        let declared: BTreeSet<&str> = row.dimensions.iter().map(|d| d.key).collect();
        let observed: BTreeSet<&str> = point.attributes.keys().map(String::as_str).collect();
        assert_eq!(
            observed, declared,
            "{} exported dimension keys that do not match its contract row",
            row.name
        );
    }
}

#[test]
fn every_contracted_instrument_is_reachable_from_the_public_api() {
    let snapshot = exercise_everything();
    let exported: BTreeSet<&str> = snapshot.names().into_iter().collect();
    let declared: BTreeSet<&str> = contract::INSTRUMENTS.iter().map(|row| row.name).collect();
    let missing: Vec<&&str> = declared.difference(&exported).collect();
    assert!(
        missing.is_empty(),
        "declared but never exported by any recording path: {missing:?}"
    );
    let undeclared: Vec<&&str> = exported.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "exported but not declared: {undeclared:?}"
    );
}

#[test]
fn cardinality_is_closed_and_enumerable() {
    let snapshot = exercise_everything();

    // Every observed value must come from its dimension's closed set, or be the process identity.
    for point in &snapshot.points {
        let row = contract::instrument(&point.name).expect("declared");
        for dimension in row.dimensions {
            let observed = point
                .attributes
                .get(dimension.key)
                .unwrap_or_else(|| panic!("{}/{} missing", row.name, dimension.key));
            match dimension.domain {
                DimensionDomain::Closed(values) => assert!(
                    values.contains(&observed.as_str()),
                    "{}/{} emitted {observed:?}, which is outside its closed value set {values:?}",
                    row.name,
                    dimension.key
                ),
                DimensionDomain::ProcessIdentity => {
                    let expected = if dimension.key == "processId" {
                        PROCESS_ID
                    } else {
                        ACTIVATION_ID
                    };
                    assert_eq!(
                        observed, expected,
                        "{}/{} is not the process identity",
                        row.name, dimension.key
                    );
                }
            }
        }
    }

    // No instrument may exceed the series count its contract row allows.
    let mut per_instrument: BTreeMap<&str, BTreeSet<Vec<(&str, &str)>>> = BTreeMap::new();
    for point in &snapshot.points {
        let series: Vec<(&str, &str)> = point
            .attributes
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        per_instrument
            .entry(point.name.as_str())
            .or_default()
            .insert(series);
    }
    for (name, series) in &per_instrument {
        let row = contract::instrument(name).expect("declared");
        assert!(
            series.len() <= row.series_per_process(),
            "{name} emitted {} series, more than the {} its dimensions allow",
            series.len(),
            row.series_per_process()
        );
    }

    let total: usize = per_instrument.values().map(BTreeSet::len).sum();
    assert!(
        total <= contract::total_series_per_process(),
        "the process emitted {total} series, above the declared bound of {}",
        contract::total_series_per_process()
    );
}

#[test]
fn exercising_every_closed_value_produces_every_series() {
    let snapshot = exercise_everything();
    // The fully exercised run must reach the declared bound exactly: if it does not, either a
    // dimension value is unreachable from the public API or the table over-declares.
    let mut observed: BTreeSet<(String, Vec<(String, String)>)> = BTreeSet::new();
    for point in &snapshot.points {
        observed.insert((
            point.name.clone(),
            point
                .attributes
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ));
    }
    assert_eq!(
        observed.len(),
        contract::total_series_per_process(),
        "the exercised run produced {} series but the contract declares {}",
        observed.len(),
        contract::total_series_per_process()
    );
}

/// Bucket bounds reach the exporter as declared, including the re-bucketed gate histograms. If
/// these drift, Gate 5 and the proxy-added-latency gates silently become undecidable again.
#[test]
fn histogram_bucket_bounds_reach_the_exporter_as_declared() {
    let snapshot = exercise_everything();
    for activity in contract::ACTIVITIES {
        let name = contract::activity_duration_name(activity.name);
        let point = snapshot
            .point_for(&name)
            .unwrap_or_else(|| panic!("{name} was not exported"));
        assert_eq!(
            point.bucket_bounds.as_deref(),
            Some(activity.buckets.bounds()),
            "{name} bucket bounds"
        );
        assert_eq!(point.kind, InstrumentKind::Histogram, "{name} kind");
        assert_eq!(point.unit, "ms", "{name} unit");
    }
}

/// Phase-1 Gate 5 is "Kafka acknowledgement p99 within 10% of Java", and it is only decidable if
/// the acknowledgement histogram resolves below 5 ms: 99.045% of the Java baseline's samples land
/// in its single (0, 5] bucket, so "p99" there means nothing finer than "at most 5 ms".
#[test]
fn the_acknowledgement_latency_histogram_can_decide_gate_five() {
    let snapshot = exercise_everything();
    let bounds = snapshot
        .point_for("kafkaCommitDuration")
        .expect("kafkaCommitDuration exported")
        .bucket_bounds
        .clone()
        .expect("explicit bounds");
    assert_eq!(bounds.as_slice(), contract::GATE_LATENCY_BUCKETS_MS);
    let below_five = bounds.iter().filter(|bound| **bound <= 5.0).count();
    assert!(
        below_five >= 6,
        "only {below_five} bounds at or below 5 ms; Gate 5 stays undecidable"
    );
    assert!(
        bounds.iter().any(|bound| *bound < 1.249),
        "the Java mean of 1.249 ms must be bracketed from below"
    );
}

/// Every instrument a performance gate reads must be bucketed for it, and the list must stay the
/// explicit reviewed one so the same bounds can be applied to the Java baseline via a view.
#[test]
fn every_gate_relevant_histogram_is_bucketed_for_its_gate() {
    let snapshot = exercise_everything();
    for name in contract::GATE_LATENCY_INSTRUMENTS {
        let point = snapshot
            .point_for(name)
            .unwrap_or_else(|| panic!("{name} was not exported"));
        assert_eq!(
            point.bucket_bounds.as_deref(),
            Some(contract::GATE_LATENCY_BUCKETS_MS),
            "{name} is gate-relevant but not bucketed for it"
        );
    }
    assert_eq!(contract::GATE_LATENCY_INSTRUMENTS.len(), 4);
}

#[test]
fn a_registry_without_an_exporter_still_runs_every_recording_path() {
    // Export disabled is the default in a dev stack, and it must not change behaviour.
    let export = capture_observability::export::install(&CollectorEndpoints::default(), PROCESS_ID)
        .expect("the no-op stack installs");
    let registry = RootTelemetry::new(export, ProcessIdentity::new(PROCESS_ID, ACTIVATION_ID));
    registry
        .process()
        .record_transition(CaptureProcessState::PassThrough)
        .expect("transition");
    registry.connections().connection_opened();
    registry
        .connections()
        .connection_closed(CloseReason::CaptureFailure);
    registry
        .kafka()
        .publication_accepted(RecordKind::Traffic, 1);
    registry.open_connection_span("c", "n").close();
    registry.export().force_flush();
    registry.export().shutdown();
}
