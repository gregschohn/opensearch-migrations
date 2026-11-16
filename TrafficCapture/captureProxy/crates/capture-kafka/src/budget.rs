//! Bounded publication memory, in bytes **and** records, with a reserved control reservation.
//!
//! # What is bounded, and why it is lossless
//!
//! Without a bound, a slow or unreachable broker makes accepted-but-unacknowledged records
//! accumulate until the process is killed by the OS — which loses *every* in-flight record at once
//! and gives no diagnosis. So publication is bounded.
//!
//! The bound is applied as **backpressure, never as loss**. A traffic record that cannot be
//! admitted right now is held, in submission order, and admitted when a delivery report frees
//! budget. Nothing is ever dropped, truncated, or sampled: a dropped record is a
//! `connectionObservationSequence` gap, which the replayer treats as a protocol violation on every
//! restart, so dropping is strictly worse than backpressure.
//!
//! The held queue is bounded structurally rather than by a second limit. `capture-wire`'s
//! per-connection [`PublishChain`](capture_wire::PublishChain) hands the sink at most **one**
//! unacknowledged record per connection, so the number of records awaiting budget can never exceed
//! the number of live connections, and each is at most the configured payload limit. The connection
//! count is already bounded by the proxy's own connection limits.
//!
//! # The control reservation
//!
//! Heartbeats are how this process proves to itself that capture is still sound. If traffic could
//! consume the entire budget, heartbeats would be delayed, their `E` deadlines would expire, and
//! the proxy would declare capture compromised **because it was busy capturing**. That is a
//! self-inflicted outage, so a slice of the budget is reserved and traffic may never touch it:
//!
//! * traffic is admitted up to `max - reserved`;
//! * control records are admitted up to `max`.
//!
//! # Relationship to librdkafka's own queue limits
//!
//! librdkafka has `queue.buffering.max.kbytes` / `queue.buffering.max.messages` and returns
//! `QueueFull` from the enqueue call when they are exceeded. This budget is configured *below*
//! those, so backpressure is applied here, where a record can be held in order. A `QueueFull` from
//! librdkafka is still handled losslessly by re-holding the record rather than failing it.

use std::sync::Mutex;

/// Which budget a record draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetClass {
    /// A `TrafficStream` record. Limited to `max - reserved`.
    Traffic,
    /// A heartbeat or capability probe. Limited to `max`, and always has the reservation.
    Control,
}

/// How many bytes and records may be awaiting acknowledgement at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetLimits {
    /// Total payload bytes awaiting acknowledgement.
    pub max_bytes: u64,
    /// Total records awaiting acknowledgement.
    pub max_records: u64,
    /// Bytes within `max_bytes` that only control records may use.
    pub reserved_control_bytes: u64,
    /// Records within `max_records` that only control records may use.
    pub reserved_control_records: u64,
}

impl BudgetLimits {
    /// Validates the limits.
    ///
    /// # Errors
    ///
    /// Rejects a zero total, and a reservation that leaves no room for traffic — either would stall
    /// capture permanently rather than bound it.
    pub fn validate(self) -> Result<Self, BudgetError> {
        if self.max_bytes == 0 || self.max_records == 0 {
            return Err(BudgetError::ZeroTotal);
        }
        if self.reserved_control_bytes >= self.max_bytes
            || self.reserved_control_records >= self.max_records
        {
            return Err(BudgetError::ReservationLeavesNoTrafficBudget {
                max_bytes: self.max_bytes,
                max_records: self.max_records,
                reserved_control_bytes: self.reserved_control_bytes,
                reserved_control_records: self.reserved_control_records,
            });
        }
        Ok(self)
    }

    /// Limits derived from the per-record payload limit and the expected lane count.
    ///
    /// The reservation is sized for two outstanding control records per lane, which covers one
    /// in-flight heartbeat plus one that was just submitted on every lane of both a current and a
    /// draining generation. A control record is tens of bytes, so the reservation costs almost
    /// nothing in bytes and everything in certainty.
    #[must_use]
    pub fn for_lanes(max_bytes: u64, max_records: u64, lanes: u64) -> Self {
        /// A heartbeat envelope is well under this; it is a deliberate over-estimate.
        const CONTROL_RECORD_BYTES: u64 = 512;
        let reserved_control_records = lanes.saturating_mul(2).max(2);
        Self {
            max_bytes,
            max_records,
            reserved_control_bytes: reserved_control_records
                .saturating_mul(CONTROL_RECORD_BYTES)
                .min(max_bytes / 2),
            reserved_control_records: reserved_control_records.min(max_records / 2).max(1),
        }
    }
}

/// A publication budget was rejected at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// A zero total budget admits nothing.
    #[error("the capture publication budget must allow at least one record and one byte")]
    ZeroTotal,
    /// The control reservation consumed the whole budget.
    #[error(
        "the capture control reservation ({reserved_control_bytes} bytes, \
         {reserved_control_records} records) leaves no traffic budget within {max_bytes} bytes and \
         {max_records} records"
    )]
    ReservationLeavesNoTrafficBudget {
        /// Configured total bytes.
        max_bytes: u64,
        /// Configured total records.
        max_records: u64,
        /// Configured reserved bytes.
        reserved_control_bytes: u64,
        /// Configured reserved records.
        reserved_control_records: u64,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    bytes: u64,
    records: u64,
}

/// A point-in-time view of the budget, for the health report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Payload bytes awaiting acknowledgement.
    pub bytes_in_flight: u64,
    /// Records awaiting acknowledgement.
    pub records_in_flight: u64,
    /// Bytes a traffic record may still claim.
    pub traffic_bytes_available: u64,
    /// Records a traffic record may still claim.
    pub traffic_records_available: u64,
    /// How many traffic admissions have been deferred for budget since startup.
    pub traffic_admissions_deferred: u64,
}

/// The shared publication budget.
///
/// Reservation and release are both two-dimensional, so they are made under one lock rather than
/// with two atomics: a record admitted on bytes but not on records, or vice versa, would corrupt
/// the accounting that retirement depends on.
#[derive(Debug)]
pub struct PublicationBudget {
    limits: BudgetLimits,
    state: Mutex<BudgetUsage>,
}

#[derive(Debug, Default)]
struct BudgetUsage {
    traffic: Usage,
    control: Usage,
    deferred: u64,
}

impl PublicationBudget {
    /// Creates a budget.
    ///
    /// # Errors
    ///
    /// Propagates [`BudgetLimits::validate`].
    pub fn new(limits: BudgetLimits) -> Result<Self, BudgetError> {
        Ok(Self {
            limits: limits.validate()?,
            state: Mutex::new(BudgetUsage::default()),
        })
    }

    /// The configured limits.
    #[must_use]
    pub fn limits(&self) -> BudgetLimits {
        self.limits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetUsage> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Attempts to admit one record of `bytes` in `class`.
    ///
    /// Returns `false` when the record must be held for later. Holding is the caller's job, so that
    /// submission order is preserved per connection.
    pub fn try_reserve(&self, class: BudgetClass, bytes: u64) -> bool {
        let mut usage = self.lock();
        let total_bytes = usage.traffic.bytes + usage.control.bytes;
        let total_records = usage.traffic.records + usage.control.records;
        let (byte_ceiling, record_ceiling) = match class {
            // Traffic may never touch the control reservation.
            BudgetClass::Traffic => (
                self.limits.max_bytes - self.limits.reserved_control_bytes,
                self.limits.max_records - self.limits.reserved_control_records,
            ),
            BudgetClass::Control => (self.limits.max_bytes, self.limits.max_records),
        };
        if total_bytes.saturating_add(bytes) > byte_ceiling
            || total_records.saturating_add(1) > record_ceiling
        {
            if class == BudgetClass::Traffic {
                usage.deferred += 1;
            }
            return false;
        }
        let slot = match class {
            BudgetClass::Traffic => &mut usage.traffic,
            BudgetClass::Control => &mut usage.control,
        };
        slot.bytes += bytes;
        slot.records += 1;
        true
    }

    /// Releases one record of `bytes` in `class`.
    ///
    /// Saturating rather than panicking: a double release would be an accounting bug, and
    /// permanently wedging publication is a worse response to it than under-counting. The actor
    /// detects the real invariant violation — a negative per-lane accepted-send count — and
    /// reports it as corruption.
    pub fn release(&self, class: BudgetClass, bytes: u64) {
        let mut usage = self.lock();
        let slot = match class {
            BudgetClass::Traffic => &mut usage.traffic,
            BudgetClass::Control => &mut usage.control,
        };
        slot.bytes = slot.bytes.saturating_sub(bytes);
        slot.records = slot.records.saturating_sub(1);
    }

    /// The health-report view.
    #[must_use]
    pub fn snapshot(&self) -> BudgetSnapshot {
        let usage = self.lock();
        let bytes_in_flight = usage.traffic.bytes + usage.control.bytes;
        let records_in_flight = usage.traffic.records + usage.control.records;
        BudgetSnapshot {
            bytes_in_flight,
            records_in_flight,
            traffic_bytes_available: (self.limits.max_bytes - self.limits.reserved_control_bytes)
                .saturating_sub(bytes_in_flight),
            traffic_records_available: (self.limits.max_records
                - self.limits.reserved_control_records)
                .saturating_sub(records_in_flight),
            traffic_admissions_deferred: usage.deferred,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(
        max_bytes: u64,
        max_records: u64,
        reserved_bytes: u64,
        reserved_records: u64,
    ) -> PublicationBudget {
        PublicationBudget::new(BudgetLimits {
            max_bytes,
            max_records,
            reserved_control_bytes: reserved_bytes,
            reserved_control_records: reserved_records,
        })
        .expect("valid limits")
    }

    #[test]
    fn traffic_is_capped_below_the_control_reservation() {
        let budget = budget(1_000, 10, 200, 2);
        // Traffic may use 800 bytes / 8 records.
        for _ in 0..8 {
            assert!(budget.try_reserve(BudgetClass::Traffic, 100));
        }
        assert!(
            !budget.try_reserve(BudgetClass::Traffic, 1),
            "traffic is capped by the record ceiling"
        );
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.records_in_flight, 8);
        assert_eq!(snapshot.traffic_records_available, 0);
        assert_eq!(snapshot.traffic_admissions_deferred, 1);
    }

    #[test]
    fn a_heartbeat_is_admitted_even_when_traffic_has_taken_everything_it_may() {
        // The whole point of the reservation: capture must not declare itself compromised because
        // it was busy capturing.
        let budget = budget(1_000, 10, 200, 2);
        for _ in 0..8 {
            assert!(budget.try_reserve(BudgetClass::Traffic, 100));
        }
        assert!(
            !budget.try_reserve(BudgetClass::Traffic, 1),
            "traffic is full"
        );
        assert!(
            budget.try_reserve(BudgetClass::Control, 100),
            "a heartbeat may always use the reservation"
        );
        assert!(
            budget.try_reserve(BudgetClass::Control, 100),
            "and the second reserved slot"
        );
        assert!(
            !budget.try_reserve(BudgetClass::Control, 100),
            "but the reservation is finite too"
        );
    }

    #[test]
    fn releasing_budget_readmits_a_deferred_record() {
        let budget = budget(1_000, 4, 200, 1);
        for _ in 0..3 {
            assert!(budget.try_reserve(BudgetClass::Traffic, 100));
        }
        assert!(!budget.try_reserve(BudgetClass::Traffic, 100));
        budget.release(BudgetClass::Traffic, 100);
        assert!(
            budget.try_reserve(BudgetClass::Traffic, 100),
            "a delivery report frees the slot for the held record"
        );
    }

    #[test]
    fn the_byte_ceiling_and_the_record_ceiling_are_independent() {
        let budget = budget(500, 100, 100, 1);
        // One huge record exhausts bytes long before records.
        assert!(budget.try_reserve(BudgetClass::Traffic, 400));
        assert!(
            !budget.try_reserve(BudgetClass::Traffic, 1),
            "the byte ceiling binds even with 99 record slots free"
        );
        assert_eq!(budget.snapshot().traffic_bytes_available, 0);
    }

    #[test]
    fn a_record_larger_than_the_whole_traffic_budget_is_deferred_not_dropped() {
        // The caller must hold it; the budget never decides to lose a record.
        let budget = budget(1_000, 10, 200, 2);
        assert!(
            !budget.try_reserve(BudgetClass::Traffic, 5_000),
            "it cannot be admitted"
        );
        assert_eq!(
            budget.snapshot().traffic_admissions_deferred,
            1,
            "it is counted as deferred, which is the caller's signal to hold it"
        );
    }

    #[test]
    fn invalid_limits_are_rejected_at_startup() {
        assert_eq!(
            PublicationBudget::new(BudgetLimits {
                max_bytes: 0,
                max_records: 1,
                reserved_control_bytes: 0,
                reserved_control_records: 0,
            })
            .expect_err("zero bytes"),
            BudgetError::ZeroTotal
        );
        assert!(matches!(
            PublicationBudget::new(BudgetLimits {
                max_bytes: 100,
                max_records: 4,
                reserved_control_bytes: 100,
                reserved_control_records: 1,
            })
            .expect_err("the reservation is the whole budget"),
            BudgetError::ReservationLeavesNoTrafficBudget { .. }
        ));
    }

    #[test]
    fn derived_limits_always_leave_room_for_both_classes() {
        for lanes in [1_u64, 2, 8, 64, 1_024] {
            let limits = BudgetLimits::for_lanes(64 * 1024 * 1024, 4_096, lanes);
            let limits = limits.validate().expect("derived limits are always valid");
            assert!(limits.reserved_control_records >= 1);
            assert!(limits.reserved_control_bytes < limits.max_bytes);
            assert!(limits.reserved_control_records < limits.max_records);
        }
    }

    #[test]
    fn release_is_saturating_rather_than_wedging_publication() {
        let budget = budget(1_000, 10, 200, 2);
        assert!(budget.try_reserve(BudgetClass::Traffic, 100));
        budget.release(BudgetClass::Traffic, 100);
        budget.release(BudgetClass::Traffic, 100);
        assert_eq!(budget.snapshot().bytes_in_flight, 0);
        assert!(
            budget.try_reserve(BudgetClass::Traffic, 100),
            "publication keeps working"
        );
    }
}
