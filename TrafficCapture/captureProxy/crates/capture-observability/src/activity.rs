//! The generated `Count` / `Duration` / `ExceptionCount` family.
//!
//! Java generates three instruments per activity name from two base classes
//! (`CommonMetricInstruments`, `CommonScopedMetricInstruments`) that every span-shaped context
//! inherits. Six activities therefore produce eighteen instruments. This module replaces that
//! inheritance with one explicit value type, constructed once per activity and held by the
//! registry.
//!
//! # Recording points
//!
//! `IScopedInstrumentationAttributes.sendMeterEventsForEnd` (lines 75-78) increments the counter
//! **and** records the duration when the scope closes, so `<activity>Count` counts *completed*
//! activities, not entered ones. [`ActivityInstruments::record_completed`] keeps that coupling: the
//! two can never disagree, which is what makes `Duration`'s count and `Count` interchangeable in a
//! dashboard.
//!
//! # These instruments are dimensionless, and that is deliberate
//!
//! `IInstrumentationAttributes.getPopulatedMetricAttributes` (line 21) returns only the attributes
//! explicitly passed at the call site, and every generated-family call site passes an empty builder.
//! No proxy context overrides it, so the span's `connectionId`, `nodeId`, `sourceRequestIndex`,
//! `topic`, and `recordId` attributes reach the **span** and never the metric point. That is also
//! what the runbook requires (`observabilityOperations.md:19-20`), and it is why the dashboards can
//! plot these series without a `group by`.

use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::contract::{self, Activity};
use crate::instruments;

/// The three instruments generated for one activity name.
#[derive(Debug)]
pub struct ActivityInstruments {
    activity: &'static str,
    completions: Counter<u64>,
    duration: Histogram<f64>,
    exceptions: Counter<u64>,
}

impl ActivityInstruments {
    /// Registers `<activity>Count`, `<activity>Duration`, and `<activity>ExceptionCount`.
    ///
    /// # Panics
    ///
    /// Panics when any of the three names is absent from the metric contract, or when the duration
    /// histogram's contract row declares a different bucket set than `activity` asks for. Both mean
    /// the contract table and [`contract::ACTIVITIES`] have diverged.
    #[must_use]
    pub fn new(meter: &Meter, activity: &Activity) -> Self {
        let duration_name = contract::activity_duration_name(activity.name);
        let expected_buckets = activity.buckets.bounds();
        let row = contract::instrument(&duration_name)
            .unwrap_or_else(|| panic!("{duration_name} is not declared in the metric contract"));
        assert_eq!(
            row.buckets,
            Some(expected_buckets),
            "{duration_name} bucket set disagrees with its activity declaration"
        );
        Self {
            activity: activity.name,
            completions: instruments::counter(meter, &contract::activity_count_name(activity.name)),
            duration: instruments::histogram(meter, &duration_name),
            exceptions: instruments::counter(
                meter,
                &contract::activity_exception_name(activity.name),
            ),
        }
    }

    /// The activity name these instruments were generated from.
    #[must_use]
    pub const fn activity(&self) -> &'static str {
        self.activity
    }

    /// One activity completed after `elapsed`.
    ///
    /// Increments the completion counter and records the duration in milliseconds, as one
    /// indivisible pair.
    pub fn record_completed(&self, elapsed: Duration) {
        self.completions.add(1, &[]);
        self.duration.record(elapsed.as_secs_f64() * 1_000.0, &[]);
    }

    /// One exception was observed inside the activity.
    ///
    /// `IInstrumentationAttributes.addTraceException` (lines 29-31) counts the exception
    /// independently of whether the activity went on to complete, so this is not mutually exclusive
    /// with [`Self::record_completed`].
    pub fn record_exception(&self) {
        self.exceptions.add(1, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SCOPE_NAME;
    use crate::snapshot::recording_meter_provider;
    use opentelemetry::metrics::MeterProvider;

    #[test]
    fn every_activity_registers_three_dimensionless_instruments() {
        let (provider, recorder) = recording_meter_provider("activities");
        let meter = provider.meter(SCOPE_NAME);
        for activity in contract::ACTIVITIES {
            let instruments = ActivityInstruments::new(&meter, activity);
            instruments.record_completed(Duration::from_millis(7));
            instruments.record_exception();
        }
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();

        for activity in contract::ACTIVITIES {
            for name in [
                contract::activity_count_name(activity.name),
                contract::activity_duration_name(activity.name),
                contract::activity_exception_name(activity.name),
            ] {
                let point = snapshot
                    .point_for(&name)
                    .unwrap_or_else(|| panic!("{name} was not exported"));
                assert!(
                    point.attributes.is_empty(),
                    "{name} must be dimensionless; traffic identities belong on the span"
                );
            }
            let duration_name = contract::activity_duration_name(activity.name);
            let point = snapshot.point_for(&duration_name).expect("histogram");
            assert_eq!(point.unit, "ms", "{duration_name} unit");
            assert_eq!(
                point.bucket_bounds.as_deref(),
                Some(activity.buckets.bounds()),
                "{duration_name} exported the wrong bucket bounds"
            );
        }
    }

    #[test]
    fn completion_records_count_and_duration_together() {
        let (provider, recorder) = recording_meter_provider("pairing");
        let meter = provider.meter(SCOPE_NAME);
        let instruments = ActivityInstruments::new(&meter, &contract::ACTIVITIES[1]);
        instruments.record_completed(Duration::from_millis(3));
        instruments.record_completed(Duration::from_millis(4));
        provider.force_flush().expect("flush");
        let snapshot = recorder.take();
        assert!(snapshot.point_for("gatheringRequestCount").is_some());
        assert!(snapshot.point_for("gatheringRequestDuration").is_some());
        assert!(
            snapshot
                .point_for("gatheringRequestExceptionCount")
                .is_none(),
            "no exception was recorded, so no exception series should exist yet"
        );
    }
}
