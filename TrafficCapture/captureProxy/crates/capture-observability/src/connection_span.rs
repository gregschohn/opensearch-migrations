//! The source-connection span (WIRE-10).
//!
//! One span per source connection, carrying the connection's identity, its lifetime, and its byte
//! totals. It replaces `trafficcapture.tracing.ConnectionContext`, whose
//! `captureConnection{Count,Duration,ExceptionCount}` family and `activeConnection` gauge this
//! module registers.
//!
//! # Two live-connection gauges, deliberately kept
//!
//! `activeConnection` (unit `count`, anchored on span open/close) and
//! [`crate::connection_metrics`]'s `captureConnectionsActive` (unit `connections`, anchored on
//! handler add/remove) measure almost the same thing. The input specification recommended dropping
//! `activeConnection` as a duplicate. That recommendation is **wrong for this repository**: the k6
//! load-test Grafana dashboard queries `activeConnection_count{exported_job="capture"}`
//! (`deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json:882,1312`), so
//! dropping it would blank a shipped panel. Both are therefore registered, with their original
//! units, and the difference between them is itself diagnostic: a persistent gap means connections
//! are outliving their capture spans.
//!
//! # Cost
//!
//! One of these exists per source connection, not per request. It holds one `Arc` clone, two
//! relaxed atomics for the byte totals, one [`Instant`], and one span handle, all inline. The span
//! is created at `debug`, so with no trace collector and the console at its default `info` it is a
//! disabled-callsite check and the identity fields are never formatted — while
//! `captureConnectionDuration`, which a dashboard reads, is recorded unconditionally. Nothing here
//! joins a process-wide collection of live connections: Java's `ActiveContextTracker` has no
//! counterpart, so opening and closing a connection touches no shared ordered container.
//!
//! # Byte totals are span fields, not metrics
//!
//! The per-connection byte totals are recorded on the span at close. The fleet-wide byte rate comes
//! from the dimensionless `bytesRead` / `bytesWritten` counters in [`crate::wire_activity`]. Putting
//! per-connection totals on a metric would need a `connectionId` dimension, which is exactly what
//! the runbook forbids.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use opentelemetry::metrics::{Meter, UpDownCounter};
use tracing::{Span, debug_span};

use crate::activity::ActivityInstruments;
use crate::contract;

/// The instruments a source-connection span records.
#[derive(Debug)]
pub struct ConnectionSpanInstruments {
    family: ActivityInstruments,
    active: UpDownCounter<i64>,
}

impl ConnectionSpanInstruments {
    /// Registers the `captureConnection` family and the `activeConnection` gauge.
    ///
    /// # Panics
    ///
    /// Panics when a name is absent from the metric contract.
    // DEVIATION: the input specification recommended dropping `activeConnection` as a duplicate
    // of `captureConnectionsActive`. It is kept, because the k6 load-test Grafana dashboard queries
    // `activeConnection_count{exported_job="capture"}`
    // (deployment/k8s/charts/components/k6LoadTest/files/grafana/load-test.json:882,1312), so
    // dropping it would blank a shipped panel. The specification's "no consumer found" finding is
    // wrong for this repository.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        let activity = contract::ACTIVITIES
            .iter()
            .find(|activity| activity.name == "captureConnection")
            .expect("captureConnection is a declared activity");
        Self {
            family: ActivityInstruments::new(meter, activity),
            active: crate::instruments::up_down_counter(meter, "activeConnection"),
        }
    }
}

#[derive(Debug)]
struct SpanState {
    instruments: Arc<ConnectionSpanInstruments>,
    started: Instant,
    span: Span,
}

/// An open source-connection span.
///
/// Closes exactly once, whether by [`Self::close`] or by drop, so the lifetime histogram and the
/// live-connection gauges cannot be left unbalanced by an error path.
#[derive(Debug)]
pub struct ConnectionSpan {
    state: Option<SpanState>,
    connection_id: Arc<str>,
    node_id: Arc<str>,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

impl ConnectionSpan {
    /// Opens the span for one source connection.
    ///
    /// `connection_id` and `node_id` are the span's correlation fields, matching
    /// `ConnectionContext.java:22-25`. Neither reaches a metric point.
    #[must_use]
    pub fn open(
        instruments: Arc<ConnectionSpanInstruments>,
        connection_id: impl Into<Arc<str>>,
        node_id: impl Into<Arc<str>>,
    ) -> Self {
        let connection_id = connection_id.into();
        let node_id = node_id.into();
        // Created at debug: with no trace collector and the console at info this is a disabled
        // callsite and the fields below are never evaluated. bytes_read and bytes_written are
        // declared empty and filled in at close, so the totals land on the span that owns them
        // rather than on a second event.
        // DEVIATION: Java creates this span unconditionally (ConnectionContext.java:35). Here it
        // is debug-gated, so with no trace collector and the console at its default `info` level
        // the callsite is disabled and the identity fields are never formatted. The externally
        // consumed captureConnectionDuration histogram and activeConnection gauge are recorded
        // either way, so observability does not depend on the span being enabled.
        let span = debug_span!(
            "captureConnection",
            connection = %connection_id,
            node = %node_id,
            bytes_read = tracing::field::Empty,
            bytes_written = tracing::field::Empty
        );
        instruments.active.add(1, &[]);
        Self {
            state: Some(SpanState {
                instruments,
                started: Instant::now(),
                span,
            }),
            connection_id,
            node_id,
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        }
    }

    /// The connection identity.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// The capture node identity this connection was recorded under.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The span, for attaching further correlation fields or parenting a child span.
    #[must_use]
    pub fn span(&self) -> Option<&Span> {
        self.state.as_ref().map(|state| &state.span)
    }

    /// Accumulates request bytes read on this connection.
    pub fn add_bytes_read(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Accumulates response bytes written on this connection.
    pub fn add_bytes_written(&self, bytes: u64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Request bytes read so far.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    /// Response bytes written so far.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// An exception was observed on this connection.
    pub fn record_exception(&self) {
        if let Some(state) = &self.state {
            state.instruments.family.record_exception();
        }
    }

    /// Closes the span, recording the lifetime and the byte totals.
    pub fn close(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        state.span.record("bytes_read", self.bytes_read());
        state.span.record("bytes_written", self.bytes_written());
        state
            .instruments
            .family
            .record_completed(state.started.elapsed());
        state.instruments.active.add(-1, &[]);
        drop(state.span);
    }
}

impl Drop for ConnectionSpan {
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

    #[test]
    fn a_connection_span_records_its_lifetime_once_and_balances_the_gauge() {
        let (provider, recorder) = recording_meter_provider("conn-span");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(ConnectionSpanInstruments::new(&meter));

        let span = ConnectionSpan::open(Arc::clone(&instruments), "c-1", "node-1");
        span.add_bytes_read(10);
        span.add_bytes_written(20);
        assert_eq!(span.bytes_read(), 10);
        assert_eq!(span.bytes_written(), 20);
        span.close();

        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(snapshot.points_for("captureConnectionDuration").len(), 1);
        let duration = snapshot
            .point_for("captureConnectionDuration")
            .expect("exported");
        assert_eq!(duration.unit, "ms");
        assert_eq!(
            duration.bucket_bounds.as_deref(),
            Some(contract::LONG_DURATION_BUCKETS_MS),
            "connection lifetimes use the long bucket set"
        );
        let active = snapshot.point_for("activeConnection").expect("exported");
        assert_eq!(
            active.unit, "count",
            "the k6 dashboard queries activeConnection_count"
        );
        assert!(active.attributes.is_empty());
    }

    #[test]
    fn dropping_a_span_closes_it() {
        let (provider, recorder) = recording_meter_provider("conn-drop");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = Arc::new(ConnectionSpanInstruments::new(&meter));
        {
            let span = ConnectionSpan::open(instruments, "c-2", "node-2");
            span.record_exception();
        }
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert_eq!(snapshot.points_for("captureConnectionDuration").len(), 1);
        assert!(
            snapshot
                .point_for("captureConnectionExceptionCount")
                .is_some()
        );
    }
}
