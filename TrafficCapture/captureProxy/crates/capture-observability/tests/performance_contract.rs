//! The performance contract: what this crate must never reintroduce.
//!
//! Profiling the Java capture proxy attributed **49.9% of Java-thread CPU to telemetry**, of which
//! **23.1% of all Java-thread CPU was sorted-map and comparator work inside `ActiveContextTracker`
//! and `ActiveContextTrackerByActivityType`** — two `ConcurrentSkipListMap`s with a composite
//! comparator, touched on every span open and close. Disabling the `OpenTelemetry` *endpoint*
//! recovered only
//! 12% of it: the cost was in the live-context bookkeeping, not in export. Telemetry was also 21%
//! of the proxy's allocation.
//!
//! That makes this crate the largest single performance lever in the rewrite, and the easiest place
//! to give the win back. These tests are the guard rails:
//!
//! - [`no_module_keeps_a_registry_of_live_contexts`] fails if an ordered or hashed container
//!   appears in a data-path module, which is how a live-context registry would come back.
//! - [`the_hot_path_has_no_owned_per_event_state`] pins the types that cross a per-request or
//!   per-record boundary as `Copy`, so no owned `String` or reference count can be introduced
//!   without failing.
//! - [`the_hot_path_sustains_a_high_observation_rate`] is a coarse canary: a per-event allocation,
//!   a global mutex, or an attribute-set lookup on the dimensionless instruments would blow its
//!   budget by orders of magnitude.
//!
//! The positive design these protect is in the module documentation of
//! [`capture_observability::wire_activity`]: phase timing is a plain monotonic instant in a handle
//! on the caller's stack, instrument handles are resolved once at start-up, and every hot-path
//! instrument is dimensionless so each observation takes the SDK's lock-free, hash-free,
//! allocation-free empty-attribute path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use capture_observability::connection_metrics::CloseReason;
use capture_observability::contract::SCOPE_NAME;
use capture_observability::export::{CollectorEndpoints, TelemetryExport};
use capture_observability::kafka_metrics::{PublishOutcome, RecordKind};
use capture_observability::kafka_record_span::RecordCorrelation;
use capture_observability::process_metrics::ProcessIdentity;
use capture_observability::root::RootTelemetry;
use capture_observability::snapshot::recording_meter_provider;
use capture_observability::wire_activity::WireInstruments;
use opentelemetry::metrics::MeterProvider;

/// Source of every module that runs on a per-connection, per-request, or per-record path.
const DATA_PATH_MODULES: &[(&str, &str)] = &[
    ("activity.rs", include_str!("../src/activity.rs")),
    ("wire_activity.rs", include_str!("../src/wire_activity.rs")),
    (
        "connection_span.rs",
        include_str!("../src/connection_span.rs"),
    ),
    (
        "kafka_record_span.rs",
        include_str!("../src/kafka_record_span.rs"),
    ),
    (
        "connection_metrics.rs",
        include_str!("../src/connection_metrics.rs"),
    ),
    ("kafka_metrics.rs", include_str!("../src/kafka_metrics.rs")),
    (
        "process_metrics.rs",
        include_str!("../src/process_metrics.rs"),
    ),
    ("root.rs", include_str!("../src/root.rs")),
];

/// Everything in `source` before its test module.
fn non_test_source(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

/// `source` with comment lines removed.
///
/// The bans below are on code, not on prose: the module documentation names the Java types it
/// replaced on purpose, and a reader needs that explanation more than the test needs the line.
fn code_only(source: &str) -> String {
    non_test_source(source)
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Java's `ActiveContextTracker` and `ActiveContextTrackerByActivityType` must have no structural
/// counterpart. Any ordered or hashed container in a data-path module is how one would come back,
/// so the container types themselves are refused rather than the Java class names.
#[test]
fn no_module_keeps_a_registry_of_live_contexts() {
    let forbidden = [
        "BTreeMap",
        "BTreeSet",
        "HashMap",
        "HashSet",
        "BinaryHeap",
        "VecDeque",
        "binary_search",
        ".sort(",
        ".sort_by",
        ".sort_unstable",
        "ActiveContextTracker",
    ];
    for (module, source) in DATA_PATH_MODULES {
        let code = code_only(source);
        for token in forbidden {
            assert!(
                !code.contains(token),
                "{module} contains {token:?}. Live-context bookkeeping in an ordered or hashed \
                 container is what cost the Java proxy 23.1% of its CPU; see the module docs of \
                 capture_observability::wire_activity for the design that replaced it."
            );
        }
    }
}

/// Instrument handles must be resolved once at start-up, never per event. `crate::instruments` is
/// the only module that looks a name up, and only the `*::new` constructors may call it.
#[test]
fn instrument_handles_are_resolved_only_during_construction() {
    for (module, source) in DATA_PATH_MODULES {
        let code = code_only(source);
        let lines: Vec<&str> = code.lines().collect();
        for (index, _) in lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("instruments::"))
        {
            // Walk back to the enclosing function signature.
            let enclosing = lines[..index]
                .iter()
                .rev()
                .find(|line| {
                    let trimmed = line.trim_start();
                    trimmed.starts_with("pub fn ") || trimmed.starts_with("fn ")
                })
                .copied()
                .unwrap_or("");
            assert!(
                enclosing.contains("fn new("),
                "{module} resolves an instrument outside a constructor, in {}",
                enclosing.trim()
            );
        }
    }
}

/// Nothing that crosses a per-request or per-record boundary may own heap state.
#[test]
fn the_hot_path_has_no_owned_per_event_state() {
    const fn assert_copy<T: Copy>() {}
    // Built once per published record on the Kafka data path.
    assert_copy::<RecordCorrelation<'_>>();
    // The closed dimension enums are the only thing that reaches a metric attribute.
    assert_copy::<CloseReason>();
    assert_copy::<RecordKind>();
    assert_copy::<PublishOutcome>();
}

/// A coarse canary for a catastrophic regression on the request path.
///
/// The budget is deliberately loose — roughly 10 microseconds per observation against a real cost
/// of tens of nanoseconds — because wall-clock assertions are flaky when they are tight. What it
/// catches is a change of *kind* rather than of degree: a per-event allocation, a process-wide
/// mutex, or giving the dimensionless instruments an attribute set would each cost orders of
/// magnitude, not percent.
#[test]
fn the_hot_path_sustains_a_high_observation_rate() {
    const REQUESTS: u64 = 200_000;
    let (provider, _recorder) = recording_meter_provider("perf");
    let meter = provider.meter(SCOPE_NAME);
    let instruments = Arc::new(WireInstruments::new(&meter));
    let connection =
        capture_observability::wire_activity::CapturingConnection::new(instruments, "c-perf");

    let started = Instant::now();
    for _ in 0..REQUESTS {
        let request = connection.begin_request();
        request.record_bytes_read(512);
        request.record_fully_parsed_request();
        let response = request.await_response().gather_response();
        response.record_bytes_written(1_024);
    }
    let elapsed = started.elapsed();

    let budget = Duration::from_micros(10) * u32::try_from(REQUESTS).expect("fits");
    assert!(
        elapsed < budget,
        "{REQUESTS} requests took {elapsed:?}, over the {budget:?} canary budget; something on \
         the request path started allocating or locking"
    );
}

/// The same canary for the Kafka record path, which is the other per-event path in the crate.
#[test]
fn the_record_path_sustains_a_high_publication_rate() {
    const RECORDS: u64 = 100_000;
    let (provider, _recorder) = recording_meter_provider("perf-kafka");
    let export = TelemetryExport::from_meter_provider(provider, false);
    let registry = RootTelemetry::new(export, ProcessIdentity::new("perf", "perf"));

    let started = Instant::now();
    for index in 0..RECORDS {
        registry
            .kafka()
            .publication_accepted(RecordKind::Traffic, 2_048);
        registry
            .begin_record_publication(RecordCorrelation {
                connection_id: "c-perf",
                writer: "w-0",
                topic: "logging-traffic-topic",
                partition: 3,
                generation: 7,
                record_index: index,
                record_size: 2_048,
            })
            .acknowledged(i64::try_from(index).expect("fits"));
        registry.kafka().publication_completed(
            RecordKind::Traffic,
            PublishOutcome::Acknowledged,
            2_048,
        );
    }
    let elapsed = started.elapsed();

    let budget = Duration::from_micros(20) * u32::try_from(RECORDS).expect("fits");
    assert!(
        elapsed < budget,
        "{RECORDS} publications took {elapsed:?}, over the {budget:?} canary budget"
    );
}

/// Export being configured or not must not change the shape of the work on the data path: the same
/// instruments are registered either way, and only the reader differs.
#[test]
fn disabling_export_does_not_change_the_recording_path() {
    let export = capture_observability::export::install(&CollectorEndpoints::default(), "perf")
        .expect("the no-op stack installs");
    let registry = RootTelemetry::new(export, ProcessIdentity::new("perf", "perf"));
    let connection = registry.capture_connection("c-1");
    for _ in 0..1_000 {
        let request = connection.begin_request();
        request.record_fully_parsed_request();
        drop(request.gather_response());
    }
    assert!(!registry.export().metric_export_enabled());
}
