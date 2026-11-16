//! Orderly retirement and incomplete-retirement reporting (traceability row WIRE-06).
//!
//! # What retirement is, and what it is not
//!
//! Retirement is **entirely local**. No Kafka record of any kind is published during it: the only
//! three record kinds are traffic, heartbeat, and capability probe, and retirement adds no fourth.
//! Two Java tests pin the record count as unchanged across retirement, and the consequence for the
//! replayer is that retirement does **not** settle replayer state — only a connection's
//! `CloseObservation` and the broker-time expiration proof do. So a Rust port must never invent a
//! tombstone or terminal control record to mark a writer as finished.
//!
//! # The layering
//!
//! This module does not disconnect anything. The Kafka component **refuses** to retire while
//! connections remain; disconnecting connections, and the timing of that disconnection, belong to
//! the proxy layer (spec discrepancy D-19 — the composite behaviour matches the protocol document,
//! the layering differs). The sequence is therefore:
//!
//! | Boundary | Action |
//! |---|---|
//! | `t0` | stop accepting new connections, then begin retirement |
//! | `t0 + natural_drain` | force-disconnect whatever has not drained |
//! | `t0 + retirement_deadline` | emit diagnostics for whatever has still not retired |
//! | `t0 + hook_completion` | cleanup must have finished |
//! | `t0 + hard_stop` | an independent watchdog halts the process |
//!
//! # Incomplete retirement is reported, not hidden
//!
//! A shutdown that ends with a writer partition not `RETIRED` is reported with **which** partitions
//! and **why** — connections still registered, or sends still outstanding. Silently reporting
//! success would mean an operator cannot tell a clean handover from one that abandoned in-flight
//! captures.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::CaptureFailure;
use crate::publisher_actor::PublisherHandle;
use crate::routing_state::{RoutingState, WriterPartition};

/// Natural-drain boundary: how long connections are left to finish on their own.
pub const ORDERLY_NATURAL_DRAIN: Duration = Duration::from_mins(1);

/// Retirement-diagnostics boundary.
pub const ORDERLY_RETIREMENT_DEADLINE: Duration = Duration::from_mins(4);

/// Cleanup-completion boundary.
pub const ORDERLY_SHUTDOWN_HOOK_COMPLETION: Duration = Duration::from_secs(270);

/// Hard-stop boundary, enforced by an independent watchdog.
pub const ORDERLY_SHUTDOWN_HARD_STOP: Duration = Duration::from_mins(5);

/// The orderly-shutdown timing boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownBudget {
    natural_drain: Duration,
    retirement_deadline: Duration,
    hook_completion: Duration,
    hard_stop: Duration,
}

impl Default for ShutdownBudget {
    fn default() -> Self {
        Self {
            natural_drain: ORDERLY_NATURAL_DRAIN,
            retirement_deadline: ORDERLY_RETIREMENT_DEADLINE,
            hook_completion: ORDERLY_SHUTDOWN_HOOK_COMPLETION,
            hard_stop: ORDERLY_SHUTDOWN_HARD_STOP,
        }
    }
}

impl ShutdownBudget {
    /// Validates the boundaries.
    ///
    /// # Errors
    ///
    /// Rejects boundaries that are not strictly increasing. A budget whose diagnostics fire after
    /// its hard stop would never emit them.
    pub fn new(
        natural_drain: Duration,
        retirement_deadline: Duration,
        hook_completion: Duration,
        hard_stop: Duration,
    ) -> Result<Self, ShutdownBudgetError> {
        if !(natural_drain < retirement_deadline
            && retirement_deadline < hook_completion
            && hook_completion < hard_stop)
        {
            return Err(ShutdownBudgetError::NotIncreasing {
                natural_drain,
                retirement_deadline,
                hook_completion,
                hard_stop,
            });
        }
        Ok(Self {
            natural_drain,
            retirement_deadline,
            hook_completion,
            hard_stop,
        })
    }

    /// The natural-drain boundary.
    #[must_use]
    pub fn natural_drain(&self) -> Duration {
        self.natural_drain
    }

    /// The retirement-diagnostics boundary.
    #[must_use]
    pub fn retirement_deadline(&self) -> Duration {
        self.retirement_deadline
    }

    /// The cleanup-completion boundary.
    #[must_use]
    pub fn hook_completion(&self) -> Duration {
        self.hook_completion
    }

    /// The hard-stop boundary.
    #[must_use]
    pub fn hard_stop(&self) -> Duration {
        self.hard_stop
    }

    /// Whether this budget can actually run inside an orchestrator's termination grace period.
    ///
    /// # Why this exists
    ///
    /// Production Kubernetes sets no `terminationGracePeriodSeconds`, so the **default 30 s**
    /// `SIGKILL` pre-empts the entire tested 60/240/270/300 s sequence: the natural drain never
    /// even completes, let alone retirement. A budget that cannot run is worse than a short one,
    /// because the sequence appears configured and never executes. Whether the orchestration gains
    /// an explicit grace period or the budget shrinks to fit is an open owner decision; this method
    /// makes the mismatch detectable either way.
    #[must_use]
    pub fn fits_within(&self, termination_grace: Duration) -> bool {
        self.hard_stop <= termination_grace
    }

    /// A budget scaled to fit `termination_grace`, preserving the boundary proportions.
    ///
    /// Returns `None` when the grace period is too short to leave a strictly increasing sequence.
    #[must_use]
    pub fn scaled_to_fit(termination_grace: Duration) -> Option<Self> {
        let total = termination_grace.as_millis();
        if total < 8 {
            return None;
        }
        let at = |numerator: u128| {
            u64::try_from(total * numerator / 300).map_or(Duration::MAX, Duration::from_millis)
        };
        // The same 60/240/270/300 proportions, scaled.
        Self::new(at(60), at(240), at(270), at(300)).ok()
    }
}

/// A shutdown budget was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShutdownBudgetError {
    /// The boundaries are not strictly increasing.
    #[error(
        "orderly shutdown boundaries must strictly increase, but natural drain {natural_drain:?}, \
         retirement deadline {retirement_deadline:?}, hook completion {hook_completion:?}, and \
         hard stop {hard_stop:?} do not"
    )]
    NotIncreasing {
        /// Configured natural-drain boundary.
        natural_drain: Duration,
        /// Configured retirement-diagnostics boundary.
        retirement_deadline: Duration,
        /// Configured cleanup-completion boundary.
        hook_completion: Duration,
        /// Configured hard-stop boundary.
        hard_stop: Duration,
    },
}

/// Why a writer partition did not retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnretiredWriter {
    /// Which writer partition.
    pub writer_partition: WriterPartition,
    /// Its routing status at the end of shutdown.
    pub status: crate::routing_state::WriterStatus,
    /// Whether connections were still registered against it.
    pub has_connections: bool,
}

/// How orderly retirement ended.
///
/// Deliberately not `PartialEq`: a [`CaptureFailure`] has identity rather than value equality, so
/// comparing two outcomes by value would silently treat two distinct failures as the same one.
#[derive(Debug, Clone)]
pub enum ShutdownOutcome {
    /// Every writer partition retired.
    Completed {
        /// How long retirement took.
        elapsed: Duration,
    },
    /// Retirement did not finish within its budget.
    ///
    /// Reported with the specific writer partitions and their statuses, because an operator needs
    /// to know whether in-flight captures were abandoned.
    Incomplete {
        /// How long was spent waiting.
        elapsed: Duration,
        /// The writer partitions that did not retire.
        unretired: Vec<UnretiredWriter>,
        /// How many connections were still registered.
        remaining_connections: usize,
    },
    /// Retirement was refused or failed.
    Refused {
        /// The failure the publisher reported.
        failure: CaptureFailure,
        /// The writer partitions that did not retire.
        unretired: Vec<UnretiredWriter>,
    },
}

impl ShutdownOutcome {
    /// The `capture_shutdown_summary` outcome token.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Incomplete { .. } | Self::Refused { .. } => "incomplete",
        }
    }

    /// Whether every writer partition retired.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// Runs orderly retirement of every writer partition.
///
/// Call this **after** the proxy layer has stopped accepting connections and after existing
/// connections have drained or been disconnected: retirement is refused while any connection is
/// still registered, and that refusal is deliberate rather than something to retry around.
///
/// Returns within `budget.retirement_deadline()` either way, so the caller's own boundaries stay
/// meaningful.
pub fn retire_all_writers(
    publisher: &PublisherHandle,
    routing: &Arc<Mutex<RoutingState>>,
    budget: ShutdownBudget,
) -> ShutdownOutcome {
    let started = Instant::now();
    let reply = match publisher.retire_all_writers() {
        Ok(reply) => reply,
        Err(failure) => {
            return ShutdownOutcome::Refused {
                failure,
                unretired: unretired_writers(routing),
            };
        }
    };

    match reply.recv_timeout(budget.retirement_deadline()) {
        Ok(Ok(())) => {
            let elapsed = started.elapsed();
            tracing::info!(
                elapsed_millis = elapsed.as_millis(),
                "event=capture_shutdown_summary outcome=completed"
            );
            ShutdownOutcome::Completed { elapsed }
        }
        Ok(Err(failure)) => {
            let unretired = unretired_writers(routing);
            tracing::error!(
                detail = failure.message(),
                unretired = unretired.len(),
                "event=capture_shutdown_failed"
            );
            ShutdownOutcome::Refused { failure, unretired }
        }
        Err(_) => {
            // The budget elapsed, or the publisher is gone. Either way the honest answer is
            // "incomplete", with the detail an operator needs.
            let elapsed = started.elapsed();
            let unretired = unretired_writers(routing);
            let remaining_connections = routing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connection_count();
            tracing::error!(
                elapsed_millis = elapsed.as_millis(),
                unretired = unretired.len(),
                remaining_connections,
                "event=capture_shutdown_summary outcome=incomplete"
            );
            for writer in &unretired {
                tracing::error!(
                    writer_node_id = %writer.writer_partition.writer_node_id(),
                    partition = writer.writer_partition.partition(),
                    status = ?writer.status,
                    has_connections = writer.has_connections,
                    "event=capture_writer_retirement_incomplete"
                );
            }
            ShutdownOutcome::Incomplete {
                elapsed,
                unretired,
                remaining_connections,
            }
        }
    }
}

/// The writer partitions that have not reached `RETIRED`, with why.
#[must_use]
pub fn unretired_writers(routing: &Arc<Mutex<RoutingState>>) -> Vec<UnretiredWriter> {
    let routing = routing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    routing
        .writer_partitions_not_retired()
        .into_iter()
        .map(|writer_partition| UnretiredWriter {
            status: routing
                .writer_status(&writer_partition)
                .unwrap_or(crate::routing_state::WriterStatus::Initializing),
            has_connections: routing.has_connections(&writer_partition),
            writer_partition,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_matches_the_protocol_boundaries() {
        let budget = ShutdownBudget::default();
        assert_eq!(budget.natural_drain(), Duration::from_mins(1));
        assert_eq!(budget.retirement_deadline(), Duration::from_mins(4));
        assert_eq!(budget.hook_completion(), Duration::from_secs(270));
        assert_eq!(budget.hard_stop(), Duration::from_mins(5));
    }

    #[test]
    fn boundaries_must_strictly_increase() {
        assert!(matches!(
            ShutdownBudget::new(
                Duration::from_mins(1),
                Duration::from_mins(1),
                Duration::from_secs(270),
                Duration::from_mins(5),
            ),
            Err(ShutdownBudgetError::NotIncreasing { .. })
        ));
        assert!(
            ShutdownBudget::new(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(3),
                Duration::from_secs(4),
            )
            .is_ok()
        );
    }

    #[test]
    fn the_default_budget_cannot_run_inside_the_default_kubernetes_grace_period() {
        // Production Kubernetes sets no terminationGracePeriodSeconds, so the default 30 s SIGKILL
        // pre-empts the whole sequence: not even the 60 s natural drain completes. This assertion
        // exists so the mismatch is a visible fact rather than a surprise in production.
        let default_kubernetes_grace = Duration::from_secs(30);
        let budget = ShutdownBudget::default();
        assert!(
            !budget.fits_within(default_kubernetes_grace),
            "a 300 s hard stop cannot run inside a 30 s grace period"
        );
        assert!(
            budget.natural_drain() > default_kubernetes_grace,
            "the drain alone outlives the grace period"
        );
        assert!(budget.fits_within(Duration::from_mins(5)));
        assert!(budget.fits_within(Duration::from_mins(10)));
    }

    #[test]
    fn a_budget_can_be_scaled_to_fit_a_short_grace_period() {
        let scaled = ShutdownBudget::scaled_to_fit(Duration::from_secs(30))
            .expect("30 s leaves room for a strictly increasing sequence");
        assert!(scaled.fits_within(Duration::from_secs(30)));
        assert_eq!(scaled.hard_stop(), Duration::from_secs(30));
        assert_eq!(scaled.natural_drain(), Duration::from_secs(6));
        assert_eq!(scaled.retirement_deadline(), Duration::from_secs(24));
        assert_eq!(scaled.hook_completion(), Duration::from_secs(27));

        assert!(
            ShutdownBudget::scaled_to_fit(Duration::from_millis(1)).is_none(),
            "a grace period too short for a strictly increasing sequence is rejected, not rounded"
        );
    }

    #[test]
    fn the_outcome_label_matches_the_shutdown_summary_tokens() {
        assert_eq!(
            ShutdownOutcome::Completed {
                elapsed: Duration::ZERO
            }
            .label(),
            "completed"
        );
        assert_eq!(
            ShutdownOutcome::Incomplete {
                elapsed: Duration::ZERO,
                unretired: Vec::new(),
                remaining_connections: 1,
            }
            .label(),
            "incomplete"
        );
        assert!(
            ShutdownOutcome::Completed {
                elapsed: Duration::ZERO
            }
            .is_complete()
        );
        assert!(
            !ShutdownOutcome::Incomplete {
                elapsed: Duration::ZERO,
                unretired: Vec::new(),
                remaining_connections: 0,
            }
            .is_complete()
        );
    }

    #[test]
    fn unretired_writers_are_reported_with_the_reason_they_did_not_retire() {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 2).expect("valid"),
        ));
        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state.prepare_routing_generation(&[0, 1]).expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        1_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
            state.route_new_connection("conn-1").expect("active");
        }

        let unretired = unretired_writers(&routing);
        assert_eq!(unretired.len(), 2, "neither partition has retired");
        assert!(
            unretired.iter().any(|writer| writer.has_connections),
            "the partition holding the connection is reported as such"
        );
        assert!(
            unretired
                .iter()
                .all(|writer| writer.status == crate::routing_state::WriterStatus::Current),
            "both are still current: retirement never began"
        );
    }

    #[test]
    fn a_fully_retired_state_reports_nothing_unretired() {
        let routing = Arc::new(Mutex::new(
            RoutingState::new("activation", 1).expect("valid"),
        ));
        {
            let mut state = routing.lock().expect("test mutex");
            let pending = state.prepare_routing_generation(&[0]).expect("valid");
            for writer_partition in pending.writer_partitions() {
                state
                    .accept_heartbeat_log_append_time(
                        &writer_partition,
                        1_000,
                        Duration::from_secs(30),
                        Duration::from_secs(5),
                    )
                    .expect("baseline");
            }
            state
                .activate_routing_generation(&pending)
                .expect("activation");
            state.begin_orderly_retirement().expect("no connections");
            for writer_partition in state.prepare_drained_writer_retirements() {
                state
                    .complete_writer_retirement(&writer_partition)
                    .expect("drained");
            }
        }
        assert!(unretired_writers(&routing).is_empty());
        assert!(
            routing
                .lock()
                .expect("test mutex")
                .all_writer_partitions_retired()
        );
    }
}
