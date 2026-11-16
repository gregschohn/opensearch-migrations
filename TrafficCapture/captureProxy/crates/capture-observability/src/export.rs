//! OTLP export wiring: on when an endpoint is configured, off when it is not.
//!
//! Export is **opt-in per signal**. Omitting the trace endpoint disables trace export; omitting the
//! metric endpoint disables metric export; omitting both makes the whole telemetry stack a no-op
//! that still runs every recording path (`CaptureProxy.java:566-570`,
//! `RootOtelContext.initializeOpenTelemetryWithCollectorsOrAsNoop`). Instrument construction and
//! span creation are unconditional, so turning export on later cannot change the proxy's behaviour
//! — only its visibility.
//!
//! # Settings that are load-bearing
//!
//! | Setting | Value | Why |
//! |---|---|---|
//! | `service.name` | `capture` | `CaptureProxy.java:569`; both dashboards filter on it |
//! | `service.instance.id` | the process id | Prometheus exposes it as `exported_instance`, which the Grafana per-pod skew panel groups by |
//! | meter scope | `captureProxy` | the dashboards query this scope exclusively |
//! | metric export interval | 1000 ms | the `CloudWatch` `Sum/PERIOD` = fleet-total convention depends on exactly one export per second per process |
//! | metric temporality | cumulative | the collector converts only its AWSEMF branch to delta |
//!
//! # Runtime requirement
//!
//! The OTLP transport is gRPC over `tonic`, so [`install`] must be called from inside a Tokio
//! runtime context when either endpoint is configured. With both endpoints absent there is no
//! exporter and no such requirement.

use std::time::Duration;

use opentelemetry::metrics::Meter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::SdkTracerProvider;

use crate::contract::{METRIC_EXPORT_INTERVAL, SCOPE_NAME, SERVICE_NAME};

/// How long the span exporter waits for one export before giving up.
///
/// `RootOtelContext.java:76`. A short, fixed timeout keeps a wedged collector from holding span
/// batches, which is the behaviour a capture proxy needs: telemetry must never be the reason
/// traffic stalls.
const SPAN_EXPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// The collector endpoints, one per signal.
///
/// Both are optional and independent. A blank string is an error rather than a synonym for absent,
/// matching `RootOtelContext.normalizeOtlpEndpoint`: silently treating a blank value as "disabled"
/// hides a broken deployment template that meant to configure export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectorEndpoints {
    /// OTLP endpoint for traces, or `None` to disable trace export.
    pub trace: Option<String>,
    /// OTLP endpoint for metrics, or `None` to disable metric export.
    pub metrics: Option<String>,
}

impl CollectorEndpoints {
    /// Whether any signal is configured.
    #[must_use]
    pub fn any_configured(&self) -> bool {
        self.trace.is_some() || self.metrics.is_some()
    }
}

/// A collector endpoint could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// The endpoint was present but blank.
    #[error(
        "OpenTelemetry {signal} endpoint cannot be blank; omit the endpoint option to disable \
         {signal} export"
    )]
    Blank {
        /// The signal name, `trace` or `metrics`.
        signal: &'static str,
    },
    /// The endpoint did not parse as a URI.
    #[error("OpenTelemetry {signal} endpoint is not a valid URI: {endpoint}")]
    NotAUri {
        /// The signal name.
        signal: &'static str,
        /// The offending value.
        endpoint: String,
    },
    /// The endpoint used a scheme other than `http` or `https`.
    #[error("OpenTelemetry {signal} endpoint must use http or https: {endpoint}")]
    UnsupportedScheme {
        /// The signal name.
        signal: &'static str,
        /// The offending value.
        endpoint: String,
    },
    /// The endpoint had no host component.
    #[error("OpenTelemetry {signal} endpoint must include a host: {endpoint}")]
    MissingHost {
        /// The signal name.
        signal: &'static str,
        /// The offending value.
        endpoint: String,
    },
}

/// Telemetry export could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// An endpoint was rejected.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// The OTLP exporter could not be built.
    #[error("the OpenTelemetry {signal} exporter could not be built: {source}")]
    Exporter {
        /// The signal name.
        signal: &'static str,
        /// The underlying builder error.
        #[source]
        source: opentelemetry_otlp::ExporterBuildError,
    },
}

/// Normalises one collector endpoint.
///
/// A bare `host:port` gains an `http://` scheme, matching
/// `RootOtelContext.normalizeOtlpEndpoint` (lines 115-139), because the deployment templates pass
/// unschemed collector addresses.
///
/// # Errors
///
/// Returns [`EndpointError`] when the value is blank, unparseable, hostless, or uses a scheme other
/// than `http` or `https`.
pub fn normalise_endpoint(endpoint: &str, signal: &'static str) -> Result<String, EndpointError> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return Err(EndpointError::Blank { signal });
    }
    // Split on the scheme separator *before* defaulting, so an explicit unsupported scheme is
    // rejected rather than prefixed into "http://grpc://host".
    let (scheme, remainder) = trimmed.split_once("://").unwrap_or(("http", trimmed));
    if scheme != "http" && scheme != "https" {
        return Err(EndpointError::UnsupportedScheme {
            signal,
            endpoint: endpoint.to_owned(),
        });
    }
    if remainder.contains("://") {
        return Err(EndpointError::NotAUri {
            signal,
            endpoint: endpoint.to_owned(),
        });
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    if host.is_empty() {
        return Err(EndpointError::MissingHost {
            signal,
            endpoint: endpoint.to_owned(),
        });
    }
    Ok(format!("{scheme}://{remainder}"))
}

/// The installed providers, held for the process's lifetime.
///
/// Dropping this stops export. The proxy keeps it alive until after the shutdown summary is logged,
/// so the terminal `captureGapActive` and termination points still make it out.
#[derive(Debug)]
pub struct TelemetryExport {
    meter_provider: SdkMeterProvider,
    tracer_provider: Option<SdkTracerProvider>,
    metric_export_enabled: bool,
}

impl TelemetryExport {
    /// Wraps an already-built meter provider, with no span export.
    ///
    /// This is how the metric-contract test drives the real recording paths into
    /// [`crate::snapshot`]'s in-memory sink instead of a collector, and how an in-process
    /// diagnostic can dump the live instrument set. `metric_export_enabled` reports what the
    /// caller configured, so a start-up summary built from a wrapped provider still states the
    /// truth about export.
    #[must_use]
    pub fn from_meter_provider(
        meter_provider: SdkMeterProvider,
        metric_export_enabled: bool,
    ) -> Self {
        Self {
            meter_provider,
            tracer_provider: None,
            metric_export_enabled,
        }
    }

    /// The single meter every instrument in this crate is registered on.
    #[must_use]
    pub fn meter(&self) -> Meter {
        use opentelemetry::metrics::MeterProvider;
        self.meter_provider.meter(SCOPE_NAME)
    }

    /// Whether span export is configured.
    #[must_use]
    pub fn trace_export_enabled(&self) -> bool {
        self.tracer_provider.is_some()
    }

    /// Whether metric export is configured.
    #[must_use]
    pub fn metric_export_enabled(&self) -> bool {
        self.metric_export_enabled
    }

    /// The tracer provider, when trace export is configured.
    ///
    /// [`crate::logging`] installs a `tracing-opentelemetry` layer from it.
    #[must_use]
    pub fn tracer_provider(&self) -> Option<&SdkTracerProvider> {
        self.tracer_provider.as_ref()
    }

    /// Exports everything buffered right now.
    ///
    /// Call this before a terminal exit: with a one-second metric interval the last transition
    /// point would usually make it out anyway, but "usually" is not a guarantee, and the
    /// termination counter is the only signal that distinguishes a fail-closed exit from a crash.
    pub fn force_flush(&self) {
        if let Err(error) = self.meter_provider.force_flush() {
            tracing::warn!("event=capture_telemetry_flush_failed signal=metrics detail={error}");
        }
        if let Some(provider) = &self.tracer_provider
            && let Err(error) = provider.force_flush()
        {
            tracing::warn!("event=capture_telemetry_flush_failed signal=traces detail={error}");
        }
    }

    /// Flushes and shuts both providers down.
    pub fn shutdown(&self) {
        if let Err(error) = self.meter_provider.shutdown() {
            tracing::warn!("event=capture_telemetry_shutdown_failed signal=metrics detail={error}");
        }
        if let Some(provider) = &self.tracer_provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!("event=capture_telemetry_shutdown_failed signal=traces detail={error}");
        }
    }
}

fn resource(process_id: &str) -> Resource {
    Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(opentelemetry::KeyValue::new(
            "service.instance.id",
            process_id.to_owned(),
        ))
        .build()
}

/// Installs the exporters the endpoints ask for.
///
/// With both endpoints absent this returns a provider with no reader: instruments still exist and
/// every recording path still runs, nothing is exported, and no network connection is attempted.
///
/// # Errors
///
/// Returns [`ExportError`] when an endpoint is malformed or an exporter cannot be built.
///
/// # Panics
///
/// Does not panic. The gRPC transport does require a Tokio runtime context when an endpoint is
/// configured; `tonic` reports that as a build error rather than a panic.
pub fn install(
    endpoints: &CollectorEndpoints,
    process_id: &str,
) -> Result<TelemetryExport, ExportError> {
    let resource = resource(process_id);

    let tracer_provider = match endpoints.trace.as_deref() {
        None => None,
        Some(endpoint) => {
            let endpoint = normalise_endpoint(endpoint, "trace")?;
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .with_timeout(SPAN_EXPORT_TIMEOUT)
                .build()
                .map_err(|source| ExportError::Exporter {
                    signal: "trace",
                    source,
                })?;
            Some(
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_batch_exporter(exporter)
                    .build(),
            )
        }
    };

    let mut meter_builder = SdkMeterProvider::builder().with_resource(resource);
    let metric_export_enabled = endpoints.metrics.is_some();
    if let Some(endpoint) = endpoints.metrics.as_deref() {
        let endpoint = normalise_endpoint(endpoint, "metrics")?;
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_temporality(Temporality::Cumulative)
            .build()
            .map_err(|source| ExportError::Exporter {
                signal: "metrics",
                source,
            })?;
        meter_builder = meter_builder.with_reader(
            PeriodicReader::builder(exporter)
                .with_interval(METRIC_EXPORT_INTERVAL)
                .build(),
        );
    }

    Ok(TelemetryExport {
        meter_provider: meter_builder.build(),
        tracer_provider,
        metric_export_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_authority_gains_an_http_scheme() {
        assert_eq!(
            normalise_endpoint("otel-collector:4317", "metrics").expect("valid"),
            "http://otel-collector:4317"
        );
    }

    #[test]
    fn an_explicit_scheme_is_preserved() {
        assert_eq!(
            normalise_endpoint("https://collector.example:4317", "trace").expect("valid"),
            "https://collector.example:4317"
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            normalise_endpoint("  collector:4317 ", "metrics").expect("valid"),
            "http://collector:4317"
        );
    }

    #[test]
    fn a_blank_endpoint_is_an_error_not_a_disable() {
        assert_eq!(
            normalise_endpoint("   ", "metrics"),
            Err(EndpointError::Blank { signal: "metrics" })
        );
    }

    #[test]
    fn a_non_http_scheme_is_rejected() {
        assert_eq!(
            normalise_endpoint("grpc://collector:4317", "trace"),
            Err(EndpointError::UnsupportedScheme {
                signal: "trace",
                endpoint: "grpc://collector:4317".to_owned()
            })
        );
    }

    #[test]
    fn a_doubled_scheme_is_rejected_rather_than_prefixed() {
        assert_eq!(
            normalise_endpoint("http://grpc://collector:4317", "trace"),
            Err(EndpointError::NotAUri {
                signal: "trace",
                endpoint: "http://grpc://collector:4317".to_owned()
            })
        );
    }

    #[test]
    fn a_path_is_preserved() {
        assert_eq!(
            normalise_endpoint("collector:4317/v1/metrics", "metrics").expect("valid"),
            "http://collector:4317/v1/metrics"
        );
    }

    #[test]
    fn a_hostless_endpoint_is_rejected() {
        assert_eq!(
            normalise_endpoint("http://:4317", "metrics"),
            Err(EndpointError::MissingHost {
                signal: "metrics",
                endpoint: "http://:4317".to_owned()
            })
        );
    }

    #[test]
    fn omitting_both_endpoints_installs_a_noop_that_still_records() {
        let export =
            install(&CollectorEndpoints::default(), "process-noop").expect("noop installs");
        assert!(!export.trace_export_enabled());
        assert!(!export.metric_export_enabled());
        // Instruments still exist, so every recording path runs unchanged.
        let registry = crate::root::RootTelemetry::new(
            export,
            crate::process_metrics::ProcessIdentity::new("process-noop", "activation-noop"),
        );
        registry.connections().connection_opened();
        registry
            .connections()
            .connection_closed(crate::connection_metrics::CloseReason::Normal);
        registry.export().force_flush();
    }

    #[test]
    fn endpoint_configuration_is_per_signal() {
        let endpoints = CollectorEndpoints {
            trace: None,
            metrics: Some("collector:4317".to_owned()),
        };
        assert!(endpoints.any_configured());
        assert!(!CollectorEndpoints::default().any_configured());
    }
}
