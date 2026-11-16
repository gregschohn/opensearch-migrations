//! An in-process metric sink, so the contract can be asserted against the real pipeline.
//!
//! The metric contract is only meaningful if it describes what the `OpenTelemetry` SDK actually
//! emits, not what this crate intended to declare. [`recording_meter_provider`] builds a real
//! [`SdkMeterProvider`] whose exporter keeps every exported point in memory, so a test can drive the
//! ordinary recording APIs, flush, and compare the resulting names, units, instrument kinds,
//! attribute keys, and histogram bucket bounds against [`crate::contract::INSTRUMENTS`].
//!
//! It is also useful operationally: a diagnostic command can dump the live instrument set without
//! an `OpenTelemetry` collector.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{SdkMeterProvider, Temporality};

use crate::contract::{InstrumentKind, SERVICE_NAME};

/// One exported metric series: an instrument plus one attribute set.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricPoint {
    /// The instrumentation scope that produced the point.
    pub scope: String,
    /// The instrument name.
    pub name: String,
    /// The declared unit, or `""` when the instrument declares none.
    pub unit: String,
    /// The instrument description.
    pub description: String,
    /// The instrument kind, recovered from the aggregation the SDK chose.
    pub kind: InstrumentKind,
    /// The point's attribute set, keyed by attribute key and rendered as a string.
    pub attributes: BTreeMap<String, String>,
    /// Explicit histogram bucket bounds, for histogram points only.
    pub bucket_bounds: Option<Vec<f64>>,
}

/// Every series exported during one flush.
#[derive(Debug, Clone, Default)]
pub struct MetricSnapshot {
    /// The exported series, in export order.
    pub points: Vec<MetricPoint>,
}

impl MetricSnapshot {
    /// Every distinct instrument name in the snapshot.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .points
            .iter()
            .map(|point| point.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Every point for one instrument name.
    #[must_use]
    pub fn points_for(&self, name: &str) -> Vec<&MetricPoint> {
        self.points
            .iter()
            .filter(|point| point.name == name)
            .collect()
    }

    /// The first point for one instrument name, if the instrument exported at all.
    #[must_use]
    pub fn point_for(&self, name: &str) -> Option<&MetricPoint> {
        self.points.iter().find(|point| point.name == name)
    }
}

/// Reads back what the meter provider exported.
#[derive(Debug, Clone)]
pub struct MetricRecorder {
    sink: Arc<Mutex<Vec<MetricPoint>>>,
}

impl MetricRecorder {
    /// Removes and returns everything exported so far.
    ///
    /// # Panics
    ///
    /// Panics if the recording mutex was poisoned, which can only happen if a previous export
    /// panicked. That is an internal invariant failure and must not be papered over.
    #[must_use]
    pub fn take(&self) -> MetricSnapshot {
        let mut sink = self.sink.lock().expect("metric recorder mutex poisoned");
        MetricSnapshot {
            points: std::mem::take(&mut *sink),
        }
    }
}

#[derive(Debug, Clone)]
struct RecordingExporter {
    sink: Arc<Mutex<Vec<MetricPoint>>>,
}

impl RecordingExporter {
    fn collect(&self, metrics: &ResourceMetrics) {
        let mut points = Vec::new();
        for scope_metrics in metrics.scope_metrics() {
            let scope = scope_metrics.scope().name().to_owned();
            for metric in scope_metrics.metrics() {
                append_points(&scope, metric, &mut points);
            }
        }
        if let Ok(mut sink) = self.sink.lock() {
            sink.extend(points);
        }
    }
}

fn append_points(
    scope: &str,
    metric: &opentelemetry_sdk::metrics::data::Metric,
    out: &mut Vec<MetricPoint>,
) {
    let base = |kind: InstrumentKind,
                attributes: BTreeMap<String, String>,
                bucket_bounds: Option<Vec<f64>>| MetricPoint {
        scope: scope.to_owned(),
        name: metric.name().to_owned(),
        unit: metric.unit().to_owned(),
        description: metric.description().to_owned(),
        kind,
        attributes,
        bucket_bounds,
    };

    match metric.data() {
        AggregatedMetrics::U64(data) => append_typed(data, out, &base),
        AggregatedMetrics::I64(data) => append_typed(data, out, &base),
        AggregatedMetrics::F64(data) => append_typed(data, out, &base),
    }
}

fn append_typed<T, F>(data: &MetricData<T>, out: &mut Vec<MetricPoint>, base: &F)
where
    F: Fn(InstrumentKind, BTreeMap<String, String>, Option<Vec<f64>>) -> MetricPoint,
{
    match data {
        MetricData::Sum(sum) => {
            let kind = if sum.is_monotonic() {
                InstrumentKind::Counter
            } else {
                InstrumentKind::UpDownCounter
            };
            for point in sum.data_points() {
                out.push(base(kind, attributes_of(point.attributes()), None));
            }
        }
        MetricData::Histogram(histogram) => {
            for point in histogram.data_points() {
                out.push(base(
                    InstrumentKind::Histogram,
                    attributes_of(point.attributes()),
                    Some(point.bounds().collect()),
                ));
            }
        }
        // This crate registers no gauges or exponential histograms. Anything arriving here is a
        // contract violation, and the metric-contract test reports it as an unexpected instrument
        // rather than silently dropping it.
        MetricData::Gauge(gauge) => {
            for point in gauge.data_points() {
                out.push(base(
                    InstrumentKind::UpDownCounter,
                    attributes_of(point.attributes()),
                    None,
                ));
            }
        }
        MetricData::ExponentialHistogram(histogram) => {
            for point in histogram.data_points() {
                out.push(base(
                    InstrumentKind::Histogram,
                    attributes_of(point.attributes()),
                    None,
                ));
            }
        }
    }
}

fn attributes_of<'a>(
    attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect()
}

impl PushMetricExporter for RecordingExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        self.collect(metrics);
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        // Cumulative, matching the production exporter: the collector converts only its AWSEMF
        // branch to delta (RootOtelContext.java:51). Asserting the contract under the production
        // temporality is the point.
        Temporality::Cumulative
    }
}

/// Builds a meter provider whose exports are kept in memory.
///
/// The returned provider carries the production resource identity, so a snapshot also proves
/// `service.name` and `service.instance.id`. Collection is driven explicitly: call
/// [`SdkMeterProvider::force_flush`] and then [`MetricRecorder::take`].
#[must_use]
pub fn recording_meter_provider(process_id: &str) -> (SdkMeterProvider, MetricRecorder) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let exporter = RecordingExporter {
        sink: Arc::clone(&sink),
    };
    let provider = SdkMeterProvider::builder()
        .with_resource(
            Resource::builder()
                .with_service_name(SERVICE_NAME)
                .with_attribute(opentelemetry::KeyValue::new(
                    "service.instance.id",
                    process_id.to_owned(),
                ))
                .build(),
        )
        .with_periodic_exporter(exporter)
        .build();
    (provider, MetricRecorder { sink })
}
