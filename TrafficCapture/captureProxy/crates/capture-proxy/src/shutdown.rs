// The shutdown intervals are stated in seconds by both `proxyCaptureProtocol.md` 3.5 and
// `CaptureProxy.java:80-83`. Expressing 60/240/300 as minutes to satisfy the pedantic unit lint
// would obscure that correspondence and leave 270 as the odd one out, so the lint is allowed here
// deliberately.
#![allow(clippy::duration_suboptimal_units)]

//! Orderly shutdown (traceability row PROXY-03).
//!
//! Ports `CaptureProxy.performOrderlyShutdown` and the surrounding watchdog. The timing policy is
//! normative: `proxyCaptureProtocol.md` §3.5 fixes the 60/240/270/300-second sequence and requires
//! every interval to be finite.
//!
//! # The deadlines are absolute, not cumulative
//!
//! This is the detail most likely to be mis-ported. Java computes `startNanos` once and compares
//! every deadline against *that* origin, so "240 seconds" means 240 seconds after shutdown began,
//! leaving 180 seconds after the 60-second natural drain — not 240 seconds after it. The protocol
//! document says the same: "60 seconds for connections to close naturally; 180 additional seconds
//! after the remaining connections are force-closed".
//!
//! # A note on the production grace period
//!
//! This sequence currently cannot complete in the shipped Kubernetes deployment, which sets no
//! `terminationGracePeriodSeconds` and so gets the 30-second default — less than the natural drain
//! alone. The whole mechanism is pre-empted by `SIGKILL`. That is open owner decision D6 in
//! `qualification/OWNER-DECISIONS.md`; this module implements the documented policy and keeps the
//! intervals configurable so either resolution is a configuration change.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// Natural connection-drain interval. `CaptureProxy.java:80`.
pub const ORDERLY_NATURAL_DRAIN: Duration = Duration::from_secs(60);

/// Deadline, from shutdown start, after which retirement is declared incomplete.
/// `CaptureProxy.java:81`.
pub const ORDERLY_RETIREMENT_DEADLINE: Duration = Duration::from_secs(240);

/// Deadline, from shutdown start, by which cleanup must finish. `CaptureProxy.java:82`.
pub const ORDERLY_SHUTDOWN_HOOK_COMPLETION: Duration = Duration::from_secs(270);

/// Independent hard-halt deadline, from shutdown start. `CaptureProxy.java:83`.
pub const ORDERLY_SHUTDOWN_HARD_STOP: Duration = Duration::from_secs(300);

/// The four orderly-shutdown intervals, all measured from shutdown start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownTiming {
    natural_drain: Duration,
    retirement_deadline: Duration,
    hook_completion: Duration,
    hard_stop: Duration,
}

impl Default for ShutdownTiming {
    fn default() -> Self {
        Self {
            natural_drain: ORDERLY_NATURAL_DRAIN,
            retirement_deadline: ORDERLY_RETIREMENT_DEADLINE,
            hook_completion: ORDERLY_SHUTDOWN_HOOK_COMPLETION,
            hard_stop: ORDERLY_SHUTDOWN_HARD_STOP,
        }
    }
}

impl ShutdownTiming {
    /// Validates the sequence.
    ///
    /// # Errors
    ///
    /// Rejects a non-positive interval and requires
    /// `natural_drain < retirement_deadline < hook_completion < hard_stop`.
    ///
    /// Java enforces the first two inequalities at `CaptureProxy.java:808-812`. The
    /// `hook_completion < hard_stop` check is added here: §3.5 describes the watchdog as firing
    /// only "if the shutdown hook does not finish", so a hard stop at or before the completion
    /// boundary would halt a shutdown still entitled to run. Java leaves that combination
    /// unvalidated.
    pub fn new(
        natural_drain: Duration,
        retirement_deadline: Duration,
        hook_completion: Duration,
        hard_stop: Duration,
    ) -> Result<Self, ShutdownTimingError> {
        for (name, value) in [
            ("natural-drain", natural_drain),
            ("retirement-deadline", retirement_deadline),
            ("shutdown-hook-completion", hook_completion),
            ("hard-stop", hard_stop),
        ] {
            if value.is_zero() {
                return Err(ShutdownTimingError::NonPositive { name });
            }
        }
        if natural_drain >= retirement_deadline
            || retirement_deadline >= hook_completion
            || hook_completion >= hard_stop
        {
            return Err(ShutdownTimingError::OutOfOrder {
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

    /// Natural connection-drain interval.
    #[must_use]
    pub fn natural_drain(&self) -> Duration {
        self.natural_drain
    }

    /// Retirement deadline, from shutdown start.
    #[must_use]
    pub fn retirement_deadline(&self) -> Duration {
        self.retirement_deadline
    }

    /// Cleanup completion boundary, from shutdown start.
    #[must_use]
    pub fn hook_completion(&self) -> Duration {
        self.hook_completion
    }

    /// Hard-halt deadline, from shutdown start.
    #[must_use]
    pub fn hard_stop(&self) -> Duration {
        self.hard_stop
    }
}

/// An invalid orderly-shutdown timing configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShutdownTimingError {
    /// An interval was zero.
    #[error("orderly shutdown interval {name} must be positive and finite")]
    NonPositive {
        /// Which interval.
        name: &'static str,
    },
    /// The intervals are not strictly increasing.
    #[error(
        "orderly shutdown requires natural-drain < retirement-deadline < \
         shutdown-hook-completion < hard-stop, got {natural_drain:?} / {retirement_deadline:?} / \
         {hook_completion:?} / {hard_stop:?}"
    )]
    OutOfOrder {
        /// Configured natural drain.
        natural_drain: Duration,
        /// Configured retirement deadline.
        retirement_deadline: Duration,
        /// Configured cleanup boundary.
        hook_completion: Duration,
        /// Configured hard stop.
        hard_stop: Duration,
    },
}

/// How an orderly shutdown ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Everything drained, retired, and cleaned up within its deadline.
    Completed,
    /// A deadline was missed or a step failed. A high-severity condition even if cleanup later
    /// finished (`proxyCaptureProtocol.md` §3.5).
    Incomplete,
}

impl ShutdownOutcome {
    /// The stable label used in the shutdown-summary operational event.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }
}

/// The listener and connection registry that shutdown drives.
pub trait ConnectionSet: Send + Sync {
    /// Stops accepting new connections. Must be idempotent.
    fn stop_accepting(&self);

    /// The number of connections still active.
    fn active_connection_count(&self) -> usize;

    /// Force-closes every still-active connection.
    fn disconnect_active(&self) -> impl Future<Output = ()> + Send;

    /// Resolves when no connections remain active.
    fn wait_until_idle(&self) -> impl Future<Output = ()> + Send;
}

/// Capture-side retirement, which must acknowledge every terminal observation before completing.
pub trait CaptureRetirement: Send + Sync {
    /// Drains registered connections and retires every publisher lane.
    ///
    /// `proxyCaptureProtocol.md` §4.3: a lane reaches `RETIRED` only after every connection using
    /// it has been fully retired, including Kafka acknowledgement of its terminal observation.
    fn retire(&self) -> impl Future<Output = Result<(), String>> + Send;

    /// Releases Kafka resources after retirement.
    fn close(&self) -> impl Future<Output = Result<(), String>> + Send;

    /// True when capture was compromised in this process.
    ///
    /// `proxyCaptureProtocol.md` §7.3: a fail-open process "does not claim orderly connection or
    /// publisher retirement for the compromised capture activation". Retirement is then skipped:
    /// it could never complete, because a connection whose capture was abandoned never publishes
    /// the terminal record that would release its route.
    fn capture_compromised(&self) -> bool {
        false
    }
}

/// Runs the orderly shutdown sequence.
///
/// Returns [`ShutdownOutcome::Completed`] only if retirement and the connection set drained and
/// cleanup finished, each within its absolute deadline.
///
/// `on_retirement_deadline_missed` is invoked when the retirement deadline passes with work
/// outstanding; it is where the high-severity diagnostic and thread dump belong.
pub async fn perform_orderly_shutdown<C, R>(
    connections: &C,
    retirement: &R,
    timing: ShutdownTiming,
    on_retirement_deadline_missed: impl FnOnce(usize) + Send,
) -> ShutdownOutcome
where
    C: ConnectionSet,
    R: CaptureRetirement,
{
    let start = Instant::now();
    let deadline = |offset: Duration| start + offset;

    // Step 1: close admission. Everything after this is draining.
    connections.stop_accepting();

    if retirement.capture_compromised() {
        return shut_down_compromised(connections, retirement, timing, deadline).await;
    }

    let mut outcome = ShutdownOutcome::Completed;

    let drain = async {
        let retired = retirement.retire().await;
        connections.wait_until_idle().await;
        retired
    };

    match tokio::time::timeout_at(deadline(timing.natural_drain), drain).await {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            // Retirement itself failed. Java force-closes and proceeds with bounded cleanup.
            outcome = ShutdownOutcome::Incomplete;
            tracing::error!(
                event = "capture_retirement_failed",
                reason = %reason,
                "Orderly proxy retirement failed; proceeding with bounded cleanup"
            );
            connections.disconnect_active().await;
        }
        Err(_) => {
            // The natural drain expired: force-close, then allow until the retirement deadline.
            tracing::warn!(
                event = "capture_drain_expired",
                active_connections = connections.active_connection_count(),
                "Natural proxy connection drain expired; disconnecting remaining connections"
            );
            connections.disconnect_active().await;

            let forced = async {
                let retired = retirement.retire().await;
                connections.wait_until_idle().await;
                retired
            };
            match tokio::time::timeout_at(deadline(timing.retirement_deadline), forced).await {
                Ok(Ok(())) => {}
                Ok(Err(reason)) => {
                    outcome = ShutdownOutcome::Incomplete;
                    tracing::error!(
                        event = "capture_retirement_failed",
                        reason = %reason,
                        "Orderly proxy retirement failed after force-close"
                    );
                }
                Err(_) => {
                    outcome = ShutdownOutcome::Incomplete;
                    let still_active = connections.active_connection_count();
                    tracing::error!(
                        event = "capture_retirement_deadline_missed",
                        active_connections = still_active,
                        "Orderly retirement did not complete before its deadline"
                    );
                    on_retirement_deadline_missed(still_active);
                }
            }
        }
    }

    // Bounded cleanup against the absolute completion boundary.
    match tokio::time::timeout_at(deadline(timing.hook_completion), retirement.close()).await {
        Ok(Ok(())) => outcome,
        Ok(Err(reason)) => {
            tracing::error!(
                event = "capture_cleanup_failed",
                reason = %reason,
                "Orderly proxy resource cleanup failed"
            );
            ShutdownOutcome::Incomplete
        }
        Err(_) => {
            tracing::error!(
                event = "capture_cleanup_deadline_missed",
                "Orderly proxy cleanup did not finish before the shutdown hook completion boundary"
            );
            ShutdownOutcome::Incomplete
        }
    }
}

/// Shutdown for a process whose capture was compromised (fail-open pass-through).
///
/// Connections still get their natural drain and are force-closed at its end, exactly as in the
/// orderly path, but no retirement is attempted and the outcome is always
/// [`ShutdownOutcome::Incomplete`]: orderly retirement is not claimed for a compromised capture
/// activation (`proxyCaptureProtocol.md` §7.3).
async fn shut_down_compromised<C, R>(
    connections: &C,
    retirement: &R,
    timing: ShutdownTiming,
    deadline: impl Fn(Duration) -> Instant,
) -> ShutdownOutcome
where
    C: ConnectionSet,
    R: CaptureRetirement,
{
    tracing::warn!(
        event = "capture_retirement_not_claimed",
        "capture was compromised in this process; orderly writer retirement is not claimed for \
         this capture activation"
    );
    if tokio::time::timeout_at(
        deadline(timing.natural_drain),
        connections.wait_until_idle(),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            event = "capture_drain_expired",
            active_connections = connections.active_connection_count(),
            "Natural proxy connection drain expired; disconnecting remaining connections"
        );
        connections.disconnect_active().await;
        let _ = tokio::time::timeout_at(
            deadline(timing.retirement_deadline),
            connections.wait_until_idle(),
        )
        .await;
    }
    if let Ok(Err(reason)) =
        tokio::time::timeout_at(deadline(timing.hook_completion), retirement.close()).await
    {
        tracing::error!(
            event = "capture_cleanup_failed",
            reason = %reason,
            "Proxy resource cleanup failed after a capture compromise"
        );
    }
    ShutdownOutcome::Incomplete
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    struct TestConnections {
        active: AtomicUsize,
        stop_accepting_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
        /// When true, `wait_until_idle` resolves only after a force-close clears the active set.
        hangs_until_disconnected: bool,
    }

    impl TestConnections {
        fn idle() -> Self {
            Self {
                active: AtomicUsize::new(0),
                stop_accepting_calls: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
                hangs_until_disconnected: false,
            }
        }

        fn hanging(active: usize) -> Self {
            Self {
                active: AtomicUsize::new(active),
                stop_accepting_calls: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
                hangs_until_disconnected: true,
            }
        }
    }

    impl ConnectionSet for TestConnections {
        fn stop_accepting(&self) {
            self.stop_accepting_calls.fetch_add(1, Ordering::SeqCst);
        }

        fn active_connection_count(&self) -> usize {
            self.active.load(Ordering::SeqCst)
        }

        async fn disconnect_active(&self) {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            self.active.store(0, Ordering::SeqCst);
        }

        async fn wait_until_idle(&self) {
            if !self.hangs_until_disconnected {
                return;
            }
            while self.active.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    struct TestRetirement {
        retire_result: Result<(), String>,
        close_result: Result<(), String>,
        retire_delay: Duration,
        close_delay: Duration,
        closed: AtomicBool,
    }

    impl TestRetirement {
        fn instant() -> Self {
            Self {
                retire_result: Ok(()),
                close_result: Ok(()),
                retire_delay: Duration::ZERO,
                close_delay: Duration::ZERO,
                closed: AtomicBool::new(false),
            }
        }
    }

    impl CaptureRetirement for TestRetirement {
        async fn retire(&self) -> Result<(), String> {
            if !self.retire_delay.is_zero() {
                tokio::time::sleep(self.retire_delay).await;
            }
            self.retire_result.clone()
        }

        async fn close(&self) -> Result<(), String> {
            if !self.close_delay.is_zero() {
                tokio::time::sleep(self.close_delay).await;
            }
            self.closed.store(true, Ordering::SeqCst);
            self.close_result.clone()
        }
    }

    #[test]
    fn the_default_timing_matches_the_protocol_document() {
        let timing = ShutdownTiming::default();
        assert_eq!(timing.natural_drain().as_secs(), 60);
        assert_eq!(timing.retirement_deadline().as_secs(), 240);
        assert_eq!(timing.hook_completion().as_secs(), 270);
        assert_eq!(timing.hard_stop().as_secs(), 300);
    }

    #[test]
    fn the_deadlines_are_absolute_leaving_180_seconds_after_the_drain() {
        // The mis-port to guard against: treating 240 as "240s after the 60s drain".
        let timing = ShutdownTiming::default();
        assert_eq!(
            timing
                .retirement_deadline()
                .checked_sub(timing.natural_drain())
                .expect("the deadlines are ordered"),
            Duration::from_secs(180),
            "protocol 3.5: 180 ADDITIONAL seconds after force-close"
        );
        assert_eq!(
            timing
                .hook_completion()
                .checked_sub(timing.retirement_deadline())
                .expect("the deadlines are ordered"),
            Duration::from_secs(30),
            "protocol 3.5: 30 seconds of bounded diagnostics"
        );
        assert_eq!(
            timing
                .hard_stop()
                .checked_sub(timing.hook_completion())
                .expect("the deadlines are ordered"),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn out_of_order_intervals_are_rejected() {
        let d = Duration::from_secs;
        assert!(matches!(
            ShutdownTiming::new(d(60), d(60), d(270), d(300)),
            Err(ShutdownTimingError::OutOfOrder { .. })
        ));
        assert!(matches!(
            ShutdownTiming::new(d(60), d(240), d(240), d(300)),
            Err(ShutdownTimingError::OutOfOrder { .. })
        ));
        // Java does not validate this one; we do.
        assert!(matches!(
            ShutdownTiming::new(d(60), d(240), d(300), d(300)),
            Err(ShutdownTimingError::OutOfOrder { .. })
        ));
        assert!(ShutdownTiming::new(d(60), d(240), d(270), d(300)).is_ok());
    }

    #[test]
    fn non_positive_intervals_are_rejected() {
        let d = Duration::from_secs;
        assert!(matches!(
            ShutdownTiming::new(Duration::ZERO, d(240), d(270), d(300)),
            Err(ShutdownTimingError::NonPositive {
                name: "natural-drain"
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_proxy_completes_immediately() {
        let connections = TestConnections::idle();
        let retirement = TestRetirement::instant();
        let started = Instant::now();
        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {
                panic!("the retirement deadline must not be reached")
            })
            .await;
        assert_eq!(outcome, ShutdownOutcome::Completed);
        assert_eq!(connections.stop_accepting_calls.load(Ordering::SeqCst), 1);
        assert_eq!(connections.disconnect_calls.load(Ordering::SeqCst), 0);
        assert!(retirement.closed.load(Ordering::SeqCst));
        assert!(
            Instant::now().duration_since(started) < Duration::from_secs(1),
            "an idle proxy must not wait out the drain interval"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connections_that_outlast_the_natural_drain_are_force_closed_at_60s() {
        let connections = TestConnections::hanging(3);
        let retirement = TestRetirement::instant();
        let started = Instant::now();
        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {
                panic!("force-close should have let the drain finish")
            })
            .await;
        assert_eq!(outcome, ShutdownOutcome::Completed);
        assert_eq!(
            connections.disconnect_calls.load(Ordering::SeqCst),
            1,
            "remaining connections must be force-closed exactly once"
        );
        let elapsed = Instant::now().duration_since(started);
        assert!(
            elapsed >= Duration::from_secs(60),
            "force-close happened before the natural drain expired: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(240),
            "force-close must not wait for the retirement deadline: {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retirement_that_never_finishes_reports_incomplete_at_240s() {
        let connections = TestConnections::hanging(2);
        let mut retirement = TestRetirement::instant();
        // Longer than the whole sequence, so retirement can never win.
        retirement.retire_delay = Duration::from_secs(3600);
        let notified = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&notified);

        let started = Instant::now();
        let outcome = perform_orderly_shutdown(
            &connections,
            &retirement,
            ShutdownTiming::default(),
            move |active| probe.store(active, Ordering::SeqCst),
        )
        .await;

        assert_eq!(outcome, ShutdownOutcome::Incomplete);
        let elapsed = Instant::now().duration_since(started);
        assert!(
            elapsed >= Duration::from_secs(240),
            "the retirement deadline must be honoured: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(300),
            "shutdown must finish before the hard stop: {elapsed:?}"
        );
        assert_eq!(connections.disconnect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            notified.load(Ordering::SeqCst),
            0,
            "the force-close zeroed the active set before the deadline diagnostic ran"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_retirement_force_closes_and_reports_incomplete() {
        let connections = TestConnections::idle();
        let mut retirement = TestRetirement::instant();
        retirement.retire_result = Err("kafka send was ambiguous".to_owned());

        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {
                panic!("the deadline path should not run")
            })
            .await;

        assert_eq!(outcome, ShutdownOutcome::Incomplete);
        assert_eq!(
            connections.disconnect_calls.load(Ordering::SeqCst),
            1,
            "a failed retirement must still force-close connections"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_that_overruns_the_completion_boundary_reports_incomplete() {
        let connections = TestConnections::idle();
        let mut retirement = TestRetirement::instant();
        retirement.close_delay = Duration::from_secs(3600);

        let started = Instant::now();
        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {})
                .await;

        assert_eq!(outcome, ShutdownOutcome::Incomplete);
        let elapsed = Instant::now().duration_since(started);
        assert!(
            elapsed >= Duration::from_secs(270),
            "cleanup must be allowed until the completion boundary: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(300),
            "cleanup must not run past the hard stop: {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_cleanup_reports_incomplete() {
        let connections = TestConnections::idle();
        let mut retirement = TestRetirement::instant();
        retirement.close_result = Err("producer close failed".to_owned());

        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {})
                .await;
        assert_eq!(outcome, ShutdownOutcome::Incomplete);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_always_closes_admission_first_on_every_path() {
        for retire_result in [Ok(()), Err("boom".to_owned())] {
            let connections = TestConnections::idle();
            let mut retirement = TestRetirement::instant();
            retirement.retire_result = retire_result;
            let _ = perform_orderly_shutdown(
                &connections,
                &retirement,
                ShutdownTiming::default(),
                |_| {},
            )
            .await;
            assert_eq!(connections.stop_accepting_calls.load(Ordering::SeqCst), 1);
        }
    }

    struct CompromisedRetirement {
        retire_called: AtomicBool,
        closed: AtomicBool,
    }

    impl CaptureRetirement for CompromisedRetirement {
        async fn retire(&self) -> Result<(), String> {
            self.retire_called.store(true, Ordering::SeqCst);
            Err("must not be called".to_owned())
        }
        async fn close(&self) -> Result<(), String> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn capture_compromised(&self) -> bool {
            true
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_compromised_process_skips_retirement_and_reports_incomplete_promptly() {
        // §7.3: orderly retirement is not claimed for a compromised activation. Before this, the
        // impossible retirement wait cost the full drain plus the forced wait on every shutdown.
        let connections = TestConnections::idle();
        let retirement = CompromisedRetirement {
            retire_called: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        };
        let started = Instant::now();
        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {
                panic!("no retirement deadline applies to a compromised process")
            })
            .await;
        assert_eq!(outcome, ShutdownOutcome::Incomplete);
        assert!(!retirement.retire_called.load(Ordering::SeqCst));
        assert!(retirement.closed.load(Ordering::SeqCst));
        assert!(Instant::now().duration_since(started) < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_compromised_process_still_gives_connections_their_natural_drain() {
        let connections = TestConnections::hanging(2);
        let retirement = CompromisedRetirement {
            retire_called: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        };
        let started = Instant::now();
        let outcome =
            perform_orderly_shutdown(&connections, &retirement, ShutdownTiming::default(), |_| {})
                .await;
        assert_eq!(outcome, ShutdownOutcome::Incomplete);
        let elapsed = Instant::now().duration_since(started);
        assert!(elapsed >= Duration::from_secs(60), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(240), "{elapsed:?}");
        assert_eq!(connections.disconnect_calls.load(Ordering::SeqCst), 1);
        assert!(!retirement.retire_called.load(Ordering::SeqCst));
    }

    #[test]
    fn the_outcome_labels_are_the_ones_the_summary_event_uses() {
        assert_eq!(ShutdownOutcome::Completed.as_str(), "completed");
        assert_eq!(ShutdownOutcome::Incomplete.as_str(), "incomplete");
    }
}
