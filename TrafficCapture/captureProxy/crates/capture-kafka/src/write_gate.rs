//! The process-wide, one-way capture write gate (traceability row KAFKA-04).
//!
//! # The guarantee, exactly
//!
//! There is a **single process-wide total order** between "this submission was admitted" and "the
//! gate is closed". Every submission that observed an open gate is admitted and stays part of the
//! publisher's in-flight work; every submission that starts after the trip is rejected with the
//! **same canonical first** cause. [`WriteGate::trip`] does **not** wait for an already-admitted
//! submission to finish.
//!
//! That last clause is the subtle one and it is deliberate
//! (`CaptureKafkaWriteGate.java:47-56`, pinned by
//! `CaptureKafkaWriteGateTest.tripDoesNotWaitForAnAcceptedSubmissionAndRejectsEveryLaterSubmission`):
//! the submission closure runs **outside** the lock, so a trip concurrent with an in-progress
//! `producer.send` completes immediately rather than blocking on Kafka. Making `trip` wait would
//! let a stuck broker delay the capture-failure decision, which is the one decision that must never
//! be delayed.
//!
//! # Irreversibility
//!
//! The first failure is canonical and there is no reset path. A second [`WriteGate::trip`] returns
//! the first cause, does not re-notify, and does not re-count the transition metric
//! (`CaptureKafkaWriteGate.java:76-78`).
//!
//! # Late listeners
//!
//! A listener registered **after** the trip is invoked immediately with the canonical first cause,
//! exactly once. The "exactly once" is non-obvious and easy to break: the list snapshot and the
//! failure assignment happen in **one** critical section, and notification happens outside it, so a
//! listener is either in `trip`'s snapshot (and `trip` notifies it) or not (and registration sees
//! the non-`None` failure and notifies it). It can never be both or neither. Spec discrepancy D-12
//! records this as the place a reimplementation most easily introduces a double notification.

use std::sync::{Mutex, MutexGuard};

use crate::error::CaptureFailure;
use crate::metrics::CaptureMetrics;

/// A terminal-failure listener.
pub type TerminalFailureListener = Box<dyn Fn(&CaptureFailure) + Send + Sync>;

struct GateState {
    terminal_failure: Option<CaptureFailure>,
    listeners: Vec<TerminalFailureListener>,
}

/// The one-way capture write gate.
///
/// Share it as an `Arc<WriteGate>` across the publisher actor, the startup sequence, and the
/// connection-forwarding gate; all three must observe the same single order.
pub struct WriteGate {
    state: Mutex<GateState>,
    metrics: Box<dyn CaptureMetrics>,
}

impl std::fmt::Debug for WriteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let closed = self.failure_if_not_writable();
        f.debug_struct("WriteGate")
            .field("closed", &closed.is_some())
            .field("failure", &closed.as_ref().map(CaptureFailure::message))
            .finish_non_exhaustive()
    }
}

impl WriteGate {
    /// An open gate reporting to `metrics`.
    #[must_use]
    pub fn new(metrics: Box<dyn CaptureMetrics>) -> Self {
        Self {
            state: Mutex::new(GateState {
                terminal_failure: None,
                listeners: Vec::new(),
            }),
            metrics,
        }
    }

    /// An open gate that discards metrics. For tests and embedders without observability.
    #[must_use]
    pub fn without_metrics() -> Self {
        Self::new(Box::new(crate::metrics::NoopMetrics))
    }

    /// Recovers the guard even if a holder panicked.
    ///
    /// A poisoned gate must still be trippable and still reject writes: refusing to lock would turn
    /// one panic into a process that silently forwards uncaptured traffic. The protected state is
    /// two fields with no cross-field invariant that a panic can break mid-update.
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `submission` if and only if the gate is open.
    ///
    /// `submission` runs **outside** the lock, so a concurrent [`WriteGate::trip`] does not wait
    /// for it.
    ///
    /// # Errors
    ///
    /// Returns the canonical terminal failure, and does not run `submission`, when the gate is
    /// closed. The rejection is counted once, by the gate.
    pub fn submit_if_writable<R>(
        &self,
        submission: impl FnOnce() -> R,
    ) -> Result<R, CaptureFailure> {
        {
            let state = self.lock();
            if let Some(failure) = state.terminal_failure.clone() {
                drop(state);
                self.metrics.write_rejected();
                return Err(failure);
            }
        }
        Ok(submission())
    }

    /// Closes the gate, irreversibly.
    ///
    /// Returns the **canonical** failure: the first cause ever passed to this method, which may not
    /// be `failure`. Listeners are notified outside the lock, once each, with that canonical cause.
    pub fn trip(&self, failure: CaptureFailure) -> CaptureFailure {
        let (canonical, transitioned, listeners) = {
            let mut state = self.lock();
            if let Some(existing) = &state.terminal_failure {
                (existing.clone(), false, Vec::new())
            } else {
                let canonical = failure.clone();
                state.terminal_failure = Some(failure);
                // Snapshotting the listener set in the same critical section as the assignment is
                // what makes "exactly once" hold against a concurrent registration.
                (canonical, true, std::mem::take(&mut state.listeners))
            }
        };

        if transitioned {
            self.metrics.write_gate_tripped(canonical.reason());
        }
        for listener in &listeners {
            listener(&canonical);
        }
        canonical
    }

    /// Registers `listener`, invoking it immediately if the gate is already closed.
    ///
    /// The listener is invoked exactly once, ever.
    pub fn add_terminal_failure_listener(&self, listener: TerminalFailureListener) {
        // Already closed: `trip` has taken its listener snapshot, so this listener can never be in
        // it. Notify it here instead, and do not retain it.
        let existing = {
            let mut state = self.lock();
            let existing = state.terminal_failure.clone();
            if existing.is_none() {
                state.listeners.push(listener);
                return;
            }
            existing
        };
        if let Some(failure) = existing {
            listener(&failure);
        }
    }

    /// The canonical failure, if the gate is closed.
    #[must_use]
    pub fn failure_if_not_writable(&self) -> Option<CaptureFailure> {
        self.lock().terminal_failure.clone()
    }

    /// Whether the gate is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.lock().terminal_failure.is_none()
    }

    /// `open` or `closed`, for the `capture_health` log line's `writeGate` field.
    #[must_use]
    pub fn state_label(&self) -> &'static str {
        if self.is_open() { "open" } else { "closed" }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::error::FailureReason;

    fn failure(message: &str) -> CaptureFailure {
        CaptureFailure::capture(FailureReason::KafkaSend, message)
    }

    /// Counts the two gate-owned metric emissions.
    #[derive(Default)]
    struct Counting {
        tripped: AtomicUsize,
        rejected: AtomicUsize,
    }

    impl CaptureMetrics for Counting {
        fn write_gate_tripped(&self, _reason: FailureReason) {
            self.tripped.fetch_add(1, Ordering::SeqCst);
        }

        fn write_rejected(&self) {
            self.rejected.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Lets the test keep a handle on the counters the gate owns.
    struct Shared(Arc<Counting>);

    impl CaptureMetrics for Shared {
        fn write_gate_tripped(&self, reason: FailureReason) {
            self.0.write_gate_tripped(reason);
        }

        fn write_rejected(&self) {
            self.0.write_rejected();
        }
    }

    #[test]
    fn trip_does_not_wait_for_an_accepted_submission_and_rejects_every_later_submission() {
        // Port of CaptureKafkaWriteGateTest.java:25-49.
        let gate = Arc::new(WriteGate::without_metrics());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let admitted = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.submit_if_writable(|| {
                    entered_tx.send(()).expect("test receiver alive");
                    release_rx.recv().expect("released by the test");
                })
            })
        };

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the submission runs");

        // The submission is parked inside the closure. `trip` must still return promptly.
        let tripped = {
            let gate = Arc::clone(&gate);
            let first = failure("first");
            std::thread::spawn(move || gate.trip(first))
        };
        let canonical = tripped.join().expect("trip thread");
        assert_eq!(
            canonical.message(),
            "first",
            "trip returned without waiting for the parked submission"
        );

        // A later submission is rejected with the same canonical cause and does not run.
        let ran_later = AtomicUsize::new(0);
        let rejection = gate
            .submit_if_writable(|| ran_later.fetch_add(1, Ordering::SeqCst))
            .expect_err("the gate is closed");
        assert!(rejection.is_same(&canonical), "same canonical instance");
        assert_eq!(
            ran_later.load(Ordering::SeqCst),
            0,
            "a rejected submission's closure must never run"
        );

        release_tx.send(()).expect("submission thread alive");
        assert!(
            admitted.join().expect("submission thread").is_ok(),
            "a submission admitted before the trip stays admitted"
        );
    }

    #[test]
    fn the_terminal_listener_receives_only_the_first_cause() {
        // Port of CaptureKafkaWriteGateTest.java:51-64.
        let gate = WriteGate::without_metrics();
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = Arc::clone(&seen);
            gate.add_terminal_failure_listener(Box::new(move |failure| {
                seen.lock()
                    .expect("test mutex")
                    .push(failure.message().to_owned());
            }));
        }

        let first = failure("first");
        let returned_first = gate.trip(first.clone());
        let returned_second = gate.trip(failure("second"));

        assert!(returned_first.is_same(&first));
        assert!(
            returned_second.is_same(&first),
            "the first failure is canonical forever"
        );
        assert_eq!(
            *seen.lock().expect("test mutex"),
            vec!["first".to_string()],
            "the listener observes the irreversible transition exactly once"
        );
    }

    #[test]
    fn a_listener_registered_after_the_trip_is_notified_immediately_and_once() {
        let gate = WriteGate::without_metrics();
        let first = failure("first");
        gate.trip(first.clone());

        let count = Arc::new(AtomicUsize::new(0));
        {
            let count = Arc::clone(&count);
            let first = first.clone();
            gate.add_terminal_failure_listener(Box::new(move |observed| {
                assert!(observed.is_same(&first), "canonical cause");
                count.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // A further trip must not re-notify the late listener.
        gate.trip(failure("third"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_transition_metric_counts_only_the_real_transition() {
        let counting = Arc::new(Counting::default());
        let gate = WriteGate::new(Box::new(Shared(Arc::clone(&counting))));
        gate.trip(failure("first"));
        gate.trip(failure("second"));
        gate.trip(failure("third"));
        assert_eq!(counting.tripped.load(Ordering::SeqCst), 1);

        assert_eq!(counting.rejected.load(Ordering::SeqCst), 0);
        let _ = gate.submit_if_writable(|| ());
        let _ = gate.submit_if_writable(|| ());
        assert_eq!(
            counting.rejected.load(Ordering::SeqCst),
            2,
            "the gate counts each rejection exactly once"
        );
    }

    #[test]
    fn an_open_gate_runs_the_submission_and_returns_its_value() {
        let gate = WriteGate::without_metrics();
        assert!(gate.is_open());
        assert_eq!(gate.state_label(), "open");
        assert_eq!(gate.submit_if_writable(|| 7_u32).expect("open"), 7);
        gate.trip(failure("closed now"));
        assert!(!gate.is_open());
        assert_eq!(gate.state_label(), "closed");
    }

    #[test]
    fn a_poisoned_gate_still_rejects_writes() {
        // A panic inside a submission must not make the gate unusable: that would mean a process
        // forwarding uncaptured traffic because a lock could not be taken.
        let gate = Arc::new(WriteGate::without_metrics());
        let panicked = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.trip(CaptureFailure::capture(
                    FailureReason::Other,
                    "before panic",
                ));
                let _unused: MutexGuard<'_, GateState> = gate.lock();
                panic!("poison the mutex while holding it");
            })
        };
        assert!(panicked.join().is_err(), "the thread panicked as designed");
        assert!(
            !gate.is_open(),
            "the gate is still readable and still closed"
        );
        assert!(gate.submit_if_writable(|| ()).is_err());
    }
}
