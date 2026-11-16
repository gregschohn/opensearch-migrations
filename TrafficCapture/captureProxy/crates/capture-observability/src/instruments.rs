//! Instrument construction, driven by the contract table.
//!
//! Every instrument in this crate is built through these helpers, which look the name up in
//! [`crate::contract::INSTRUMENTS`] and take the unit, description, and bucket bounds from there.
//! That makes the contract table the single source of truth rather than a parallel description of
//! the code: a row cannot drift from the instrument it describes, because the instrument is
//! constructed from the row.

use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

use crate::contract::{self, InstrumentKind};

/// Looks the row up, asserting the declared kind.
///
/// # Panics
///
/// Panics when `name` is not in the contract table, or is declared with a different kind. Both are
/// internal invariant failures: the registry and the table are compiled together, so a mismatch
/// means the table was edited without the registry (or vice versa) and the process must not come up
/// publishing a series no consumer expects.
fn row(name: &str, kind: InstrumentKind) -> &'static contract::Instrument {
    let row = contract::instrument(name)
        .unwrap_or_else(|| panic!("instrument {name} is not declared in the metric contract"));
    assert_eq!(
        row.kind,
        kind,
        "instrument {name} is declared as {} in the metric contract",
        row.kind.as_str()
    );
    row
}

/// Builds the monotonic counter named `name`, exactly as the contract declares it.
///
/// # Panics
///
/// See [`row`].
pub(crate) fn counter(meter: &Meter, name: &str) -> Counter<u64> {
    let row = row(name, InstrumentKind::Counter);
    let mut builder = meter
        .u64_counter(row.name)
        .with_description(row.description);
    if !row.unit.is_empty() {
        builder = builder.with_unit(row.unit);
    }
    builder.build()
}

/// Builds the non-monotonic up-down counter named `name`, exactly as the contract declares it.
///
/// # Panics
///
/// See [`row`].
pub(crate) fn up_down_counter(meter: &Meter, name: &str) -> UpDownCounter<i64> {
    let row = row(name, InstrumentKind::UpDownCounter);
    let mut builder = meter
        .i64_up_down_counter(row.name)
        .with_description(row.description);
    if !row.unit.is_empty() {
        builder = builder.with_unit(row.unit);
    }
    builder.build()
}

/// Builds the duration histogram named `name`, with the contract's explicit bucket bounds.
///
/// # Panics
///
/// See [`row`]; additionally panics when the row declares no bucket bounds, since an implicit
/// bucket set would publish different Prometheus bucket series than the dashboards query.
pub(crate) fn histogram(meter: &Meter, name: &str) -> Histogram<f64> {
    let row = row(name, InstrumentKind::Histogram);
    let bounds = row
        .buckets
        .unwrap_or_else(|| panic!("histogram {name} declares no explicit bucket bounds"));
    let mut builder = meter
        .f64_histogram(row.name)
        .with_description(row.description)
        .with_boundaries(bounds.to_vec());
    if !row.unit.is_empty() {
        builder = builder.with_unit(row.unit);
    }
    builder.build()
}
