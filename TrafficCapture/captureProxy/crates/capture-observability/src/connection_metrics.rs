//! Active source connections and bounded close reasons (HTTP-10).
//!
//! Two instruments: how many source connections are open right now, and why connections closed.
//! Connection **identity** is deliberately absent from both
//! (`CaptureConnectionMetrics.java:14-16`) and belongs in the matching
//! `event=capture_connection_dropped` log line. That division of labour is what keeps the series
//! count independent of traffic volume, and it is the reason the dashboards can plot
//! `captureConnectionCloses` grouped by `reason` without a cardinality risk.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter, UpDownCounter};

use crate::instruments;

/// Why a source connection closed.
///
/// `CaptureConnectionMetrics.CloseReason` (lines 19-26). The set is **closed**: every close is
/// attributed to exactly one variant, and the label is the variant name lowercased, which is what
/// the `CloudWatch` and Grafana panels group by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// The connection ended without hitting a limit or a capture failure.
    ///
    /// The initial value: a close is `normal` unless something marked it otherwise
    /// (`LoggingHttpHandler.java:232`).
    Normal,
    /// One request exceeded the maximum assembly duration.
    AssemblyDuration,
    /// One incomplete request exceeded the maximum aggregate header bytes.
    HeaderBytes,
    /// One incomplete request exceeded the maximum aggregate wire bytes.
    TotalBytes,
    /// The connection exceeded its maximum lifetime.
    ConnectionDuration,
    /// Required capture failed, so the connection was dropped rather than served uncaptured.
    CaptureFailure,
}

impl CloseReason {
    /// The `reason` dimension value, lowercased exactly as Java emits it.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::AssemblyDuration => "assembly_duration",
            Self::HeaderBytes => "header_bytes",
            Self::TotalBytes => "total_bytes",
            Self::ConnectionDuration => "connection_duration",
            Self::CaptureFailure => "capture_failure",
        }
    }

    /// Every close reason, for exhaustive iteration in tests and diagnostics.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Normal,
            Self::AssemblyDuration,
            Self::HeaderBytes,
            Self::TotalBytes,
            Self::ConnectionDuration,
            Self::CaptureFailure,
        ]
    }
}

/// The capture-proxy source-connection instruments.
#[derive(Debug)]
pub struct ConnectionMetrics {
    connections_active: UpDownCounter<i64>,
    connection_closes: Counter<u64>,
}

impl ConnectionMetrics {
    /// Registers `captureConnectionsActive` and `captureConnectionCloses`.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            connections_active: instruments::up_down_counter(meter, "captureConnectionsActive"),
            connection_closes: instruments::counter(meter, "captureConnectionCloses"),
        }
    }

    /// A source connection became visible to the capture handlers.
    ///
    /// Anchored on handler-added, not on span open, matching `LoggingHttpHandler.java:274`.
    pub fn connection_opened(&self) {
        self.connections_active.add(1, &[]);
    }

    /// A source connection closed, attributed to `reason`.
    ///
    /// Recorded exactly once per connection, with whatever reason was last marked
    /// (`LoggingHttpHandler.java:349`).
    pub fn connection_closed(&self, reason: CloseReason) {
        self.connections_active.add(-1, &[]);
        self.connection_closes
            .add(1, &[KeyValue::new("reason", reason.as_label())]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CLOSE_REASONS, SCOPE_NAME};
    use crate::snapshot::recording_meter_provider;
    use opentelemetry::metrics::MeterProvider;
    use std::collections::BTreeSet;

    #[test]
    fn close_reason_labels_are_exactly_the_contract_value_set() {
        let labels: Vec<&str> = CloseReason::all()
            .iter()
            .map(|reason| reason.as_label())
            .collect();
        assert_eq!(labels, CLOSE_REASONS);
    }

    #[test]
    fn every_close_reason_produces_one_bounded_series() {
        let (provider, recorder) = recording_meter_provider("close-reasons");
        let meter = provider.meter(SCOPE_NAME);
        let metrics = ConnectionMetrics::new(&meter);
        for reason in CloseReason::all() {
            metrics.connection_opened();
            metrics.connection_closed(*reason);
        }
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();

        let reasons: BTreeSet<String> = snapshot
            .points_for("captureConnectionCloses")
            .into_iter()
            .map(|point| {
                point
                    .attributes
                    .get("reason")
                    .expect("every close point carries a reason")
                    .clone()
            })
            .collect();
        assert_eq!(reasons.len(), CLOSE_REASONS.len());
        for expected in CLOSE_REASONS {
            assert!(reasons.contains(*expected), "missing reason {expected}");
        }

        let active = snapshot
            .point_for("captureConnectionsActive")
            .expect("active gauge exported");
        assert!(
            active.attributes.is_empty(),
            "captureConnectionsActive is dimensionless; the dashboards do not group it"
        );
        assert_eq!(active.unit, "connections");
    }
}
