//! The one-way, process-wide capture state machine (traceability row HTTP-02).
//!
//! Ports `CaptureProcessState.java`. The proxy begins capture-authoritative; a required-capture
//! failure moves it **exactly once** to permanent pass-through or to termination, and corrupted
//! execution state always moves it to termination — even after fail-open
//! (`proxyCaptureProtocol.md` §7.1).
//!
//! # The linearisation requirement, and why it is a blocking mutex
//!
//! `proxyCaptureProtocol.md` §5.5 requires that "the check and source-forwarding decision must
//! share a one-way local capture gate. Once the gate closes, no thread may newly pass the check.
//! This avoids a simple check-then-transition race inside the process."
//!
//! Java satisfies this with a `synchronized` method, `forwardSourceTrafficIfPermitted`, that holds
//! the monitor across the forwarding call. That call is Netty's `ctx.write(...)`, which *submits*
//! a write and returns a future — it does not block until the bytes reach the socket. So the
//! invariant Java actually establishes is: **admission and write submission are atomic with
//! respect to the state transition**, not "the write completes under the lock".
//!
//! This implementation establishes the same invariant the same way: [`Self::forward_if_permitted`]
//! holds a `std::sync::Mutex` across a **synchronous, non-blocking** closure. The closure's job is
//! to enqueue the bytes for the connection's own writer task; the actual `.await` on the socket
//! happens afterwards, outside the gate. Nothing in this module is `async`, and no lock is ever
//! held across an `.await`.
//!
//! An `AtomicU8` with compare-and-swap would **not** be sufficient. A load-then-write sequence
//! leaves a window in which thread A observes `Capture`, thread B closes the gate, and A then
//! submits a forward that the gate should have refused. The mutex is what removes that window, and
//! it is why this type is deliberately not lock-free.
//!
//! §5.5 also notes the boundary of what any local gate can achieve: "No local check can fence a
//! thread that has already entered an uninterruptible source syscall. That does not violate the
//! central invariant: the request was completely acknowledged to Kafka before that syscall was
//! allowed."

use std::sync::{Arc, Mutex, MutexGuard};

use capture_config::CaptureFailurePolicy;

/// The process-wide capture state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Capture is authoritative. The initial state.
    Capture,
    /// Capture has been permanently abandoned; traffic forwards uncaptured.
    ///
    /// Reached only under [`CaptureFailurePolicy::FailOpen`]. Capture never resumes
    /// (`proxyCaptureProtocol.md` §7.3).
    PassThrough,
    /// The process is terminating and must admit no further source forwarding.
    Terminating,
}

impl State {
    /// A short stable label for logs and metric dimensions.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::PassThrough => "pass_through",
            Self::Terminating => "terminating",
        }
    }
}

/// Why capture was compromised.
///
/// The capture protocol distinguishes ordinary capture failure, which obeys the configured policy,
/// from corrupted internal ownership, which terminates in **both** policies.
#[derive(Debug, Clone)]
pub struct CaptureFailure {
    /// Operator-facing description.
    reason: String,
    /// True when local execution ownership is no longer trustworthy.
    unstable: bool,
}

impl CaptureFailure {
    /// An ordinary required-capture failure, subject to `--capture-failure-policy`.
    #[must_use]
    pub fn required(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            unstable: false,
        }
    }

    /// An internal ownership or invariant corruption, which always terminates.
    ///
    /// Corresponds to Java's `unstableProcessFailed` path and to
    /// `CorruptedCaptureStateException`.
    #[must_use]
    pub fn unstable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            unstable: true,
        }
    }

    /// The operator-facing description.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// True when this failure terminates regardless of the configured policy.
    #[must_use]
    pub fn is_unstable(&self) -> bool {
        self.unstable
    }
}

/// A listener invoked once when the process begins terminating.
pub type TerminationListener = Arc<dyn Fn(&CaptureFailure) + Send + Sync>;

/// A listener invoked on every canonical state transition.
pub type TransitionListener = Arc<dyn Fn(State) + Send + Sync>;

/// Mutable state, held behind one mutex so admission and transition cannot interleave.
struct Inner {
    state: State,
    /// The first, canonical failure. Later failures never replace it
    /// (`proxyCaptureProtocol.md` §7.1: the **first** such event closes the gate).
    failure: Option<CaptureFailure>,
    termination_listeners: Vec<TerminationListener>,
}

/// The process-wide capture gate.
pub struct CaptureProcessState {
    failure_policy: CaptureFailurePolicy,
    inner: Mutex<Inner>,
    transition_listener: Option<TransitionListener>,
}

impl CaptureProcessState {
    /// Creates a gate in [`State::Capture`].
    #[must_use]
    pub fn new(failure_policy: CaptureFailurePolicy) -> Self {
        Self {
            failure_policy,
            inner: Mutex::new(Inner {
                state: State::Capture,
                failure: None,
                termination_listeners: Vec::new(),
            }),
            transition_listener: None,
        }
    }

    /// Creates a gate that reports every canonical transition to `listener`.
    ///
    /// The listener is invoked outside the gate's lock so a slow or panicking observer cannot
    /// stall admission.
    #[must_use]
    pub fn with_transition_listener(
        failure_policy: CaptureFailurePolicy,
        listener: TransitionListener,
    ) -> Self {
        Self {
            transition_listener: Some(listener),
            ..Self::new(failure_policy)
        }
    }

    /// The configured policy.
    #[must_use]
    pub fn failure_policy(&self) -> CaptureFailurePolicy {
        self.failure_policy
    }

    /// Locks the inner state, recovering from a poisoned mutex.
    ///
    /// A panic while the gate was held means some other thread's invariants are unknown, but the
    /// gate's own state is a small enum that is always left consistent, and refusing to read it
    /// would strand the process with no way to terminate. So the poison is deliberately ignored.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.lock().state
    }

    /// True while capture is authoritative.
    #[must_use]
    pub fn should_capture(&self) -> bool {
        self.state() == State::Capture
    }

    /// True once capture has been permanently abandoned.
    #[must_use]
    pub fn is_pass_through(&self) -> bool {
        self.state() == State::PassThrough
    }

    /// True once the process is terminating.
    #[must_use]
    pub fn is_terminating(&self) -> bool {
        self.state() == State::Terminating
    }

    /// The canonical capture failure, if the gate has closed.
    #[must_use]
    pub fn failure(&self) -> Option<CaptureFailure> {
        self.lock().failure.clone()
    }

    /// Linearises source forwarding against gate closure.
    ///
    /// Runs `submit` while holding the gate and returns its result, or returns `None` without
    /// running it because the process is already terminating.
    ///
    /// `submit` receives the state observed **inside** the critical section, which is the only
    /// reading of the state that is guaranteed not to be stale by the time the forward is admitted.
    /// A caller that needs to know whether it is forwarding captured or uncaptured traffic must use
    /// this value rather than calling [`Self::state`] separately.
    ///
    /// `submit` **must not block, must not await, and must not call back into this type** — the
    /// gate's mutex is held and is not reentrant. Its contract is to hand the bytes to the
    /// connection's writer, the equivalent of Netty's `ctx.write(...)` returning a future. See the
    /// module documentation for why this closure runs under a lock and why that is both necessary
    /// and sufficient.
    ///
    /// Note that [`State::PassThrough`] permits forwarding: fail-open's entire purpose is to keep
    /// forwarding uncaptured traffic. Only [`State::Terminating`] refuses.
    pub fn forward_if_permitted<R>(&self, submit: impl FnOnce(State) -> R) -> Option<R> {
        let guard = self.lock();
        if guard.state == State::Terminating {
            return None;
        }
        let observed = guard.state;
        let result = submit(observed);
        drop(guard);
        Some(result)
    }

    /// Registers a listener for the start of termination.
    ///
    /// If the process is already terminating, the listener is invoked immediately with the
    /// canonical failure, so a late observer cannot miss the event
    /// (`proxyCaptureProtocol.md` §7.1).
    pub fn add_termination_listener(&self, listener: &TerminationListener) {
        let existing = {
            let mut guard = self.lock();
            guard.termination_listeners.push(Arc::clone(listener));
            if guard.state == State::Terminating {
                guard.failure.clone()
            } else {
                None
            }
        };
        if let Some(failure) = existing {
            listener(&failure);
        }
    }

    /// Applies the configured policy to a required-capture failure, at most once.
    ///
    /// Returns the resulting state. A second call after the gate has closed is a no-op that
    /// returns the existing state, so the canonical cause is never overwritten.
    ///
    /// A [`CaptureFailure::unstable`] failure terminates regardless of policy, including from
    /// [`State::PassThrough`]: "Internal ownership or corruption failures terminate even after
    /// fail-open."
    pub fn capture_failed(&self, failure: &CaptureFailure) -> State {
        let (resulting_state, listeners, transitioned) = {
            let mut guard = self.lock();

            if failure.is_unstable() {
                if guard.state == State::Terminating {
                    return State::Terminating;
                }
                guard.failure.get_or_insert_with(|| failure.clone());
                guard.state = State::Terminating;
                let listeners = guard.termination_listeners.clone();
                (State::Terminating, listeners, true)
            } else {
                // An ordinary required-capture failure acts only from the authoritative state.
                // Once the process is in pass-through or terminating, the policy has already been
                // applied and is irreversible.
                if guard.state != State::Capture {
                    return guard.state;
                }
                guard.failure = Some(failure.clone());
                if self.failure_policy == CaptureFailurePolicy::FailOpen {
                    guard.state = State::PassThrough;
                    (State::PassThrough, Vec::new(), true)
                } else {
                    guard.state = State::Terminating;
                    let listeners = guard.termination_listeners.clone();
                    (State::Terminating, listeners, true)
                }
            }
        };

        if transitioned {
            match resulting_state {
                State::PassThrough => tracing::error!(
                    event = "capture_compromised",
                    mode = "pass_through",
                    reason = failure.reason(),
                    "Required capture failed; this process has irreversibly entered pass-through mode"
                ),
                State::Terminating if failure.is_unstable() => tracing::error!(
                    event = "capture_compromised",
                    mode = "terminating",
                    reason = failure.reason(),
                    "Proxy process execution is unstable; terminating immediately"
                ),
                State::Terminating => tracing::error!(
                    event = "capture_compromised",
                    mode = "terminating",
                    reason = failure.reason(),
                    "Required capture failed; this process will terminate"
                ),
                State::Capture => {
                    debug_assert!(false, "a transition can never land back in Capture");
                }
            }
            self.notify_transition(resulting_state);
            // The canonical failure, not necessarily this call's argument: an unstable failure
            // arriving after a fail-open transition must not relabel the original cause.
            let canonical = self.failure().unwrap_or_else(|| failure.clone());
            for listener in listeners {
                listener(&canonical);
            }
        }
        resulting_state
    }

    /// Notifies the transition listener, absorbing a panicking observer.
    ///
    /// Java logs and swallows a `RuntimeException` here so that a metrics failure cannot prevent
    /// the process from terminating; this does the same.
    fn notify_transition(&self, state: State) {
        if let Some(listener) = &self.transition_listener {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(state)));
            if result.is_err() {
                tracing::error!(
                    event = "capture_state_listener_failed",
                    state = state.as_str(),
                    "Unable to record the capture-process state transition"
                );
            }
        }
    }
}

impl std::fmt::Debug for CaptureProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureProcessState")
            .field("failure_policy", &self.failure_policy)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn fail_closed() -> CaptureProcessState {
        CaptureProcessState::new(CaptureFailurePolicy::FailClosed)
    }

    fn fail_open() -> CaptureProcessState {
        CaptureProcessState::new(CaptureFailurePolicy::FailOpen)
    }

    /// Coerces a closure into a [`TerminationListener`]; `&Arc::new(closure)` will not coerce to
    /// `&Arc<dyn Fn ...>` on its own.
    fn listener(f: impl Fn(&CaptureFailure) + Send + Sync + 'static) -> TerminationListener {
        Arc::new(f)
    }

    #[test]
    fn the_initial_state_is_capture_authoritative() {
        let gate = fail_closed();
        assert_eq!(gate.state(), State::Capture);
        assert!(gate.should_capture());
        assert!(!gate.is_pass_through());
        assert!(!gate.is_terminating());
        assert!(gate.failure().is_none());
    }

    #[test]
    fn fail_closed_moves_capture_to_terminating() {
        let gate = fail_closed();
        assert_eq!(
            gate.capture_failed(&CaptureFailure::required("kafka send failed")),
            State::Terminating
        );
        assert!(gate.is_terminating());
        assert!(!gate.should_capture());
    }

    #[test]
    fn fail_open_moves_capture_to_pass_through() {
        let gate = fail_open();
        assert_eq!(
            gate.capture_failed(&CaptureFailure::required("kafka send failed")),
            State::PassThrough
        );
        assert!(gate.is_pass_through());
        assert!(!gate.should_capture());
    }

    #[test]
    fn the_transition_happens_at_most_once_and_keeps_the_first_cause() {
        let gate = fail_open();
        gate.capture_failed(&CaptureFailure::required("first"));
        gate.capture_failed(&CaptureFailure::required("second"));
        assert_eq!(gate.state(), State::PassThrough);
        assert_eq!(
            gate.failure().expect("a failure is recorded").reason(),
            "first"
        );
    }

    #[test]
    fn an_unstable_failure_terminates_even_after_fail_open() {
        // proxyCaptureProtocol.md 7.1 / plan: "Internal ownership or corruption failures terminate
        // even after fail-open."
        let gate = fail_open();
        assert_eq!(
            gate.capture_failed(&CaptureFailure::required("kafka send failed")),
            State::PassThrough
        );
        assert_eq!(
            gate.capture_failed(&CaptureFailure::unstable("routing state corrupted")),
            State::Terminating
        );
        assert!(gate.is_terminating());
        // The canonical cause remains the first failure.
        assert_eq!(
            gate.failure().expect("a failure is recorded").reason(),
            "kafka send failed"
        );
    }

    #[test]
    fn an_unstable_failure_terminates_directly_from_capture() {
        let gate = fail_open();
        assert_eq!(
            gate.capture_failed(&CaptureFailure::unstable("ownership corrupted")),
            State::Terminating
        );
        assert_eq!(
            gate.failure().expect("recorded").reason(),
            "ownership corrupted"
        );
    }

    #[test]
    fn a_required_failure_after_terminating_is_a_no_op() {
        let gate = fail_closed();
        gate.capture_failed(&CaptureFailure::required("first"));
        assert_eq!(
            gate.capture_failed(&CaptureFailure::required("second")),
            State::Terminating
        );
        assert_eq!(gate.failure().expect("recorded").reason(), "first");
    }

    #[test]
    fn forwarding_is_permitted_while_capturing() {
        let gate = fail_closed();
        assert_eq!(gate.forward_if_permitted(|_| 7), Some(7));
    }

    #[test]
    fn forwarding_is_permitted_in_pass_through_because_that_is_its_purpose() {
        let gate = fail_open();
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        assert_eq!(gate.state(), State::PassThrough);
        assert_eq!(gate.forward_if_permitted(|_| 7), Some(7));
    }

    #[test]
    fn forwarding_is_refused_once_terminating() {
        let gate = fail_closed();
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        let ran = AtomicUsize::new(0);
        let permitted = gate.forward_if_permitted(|_| ran.fetch_add(1, Ordering::SeqCst));
        assert!(
            permitted.is_none(),
            "a terminating process must refuse to forward"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the submit closure must not run at all"
        );
    }

    #[test]
    fn a_termination_listener_fires_once_on_transition() {
        let gate = fail_closed();
        let seen = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&seen);
        gate.add_termination_listener(&listener(move |_| {
            probe.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        // A second failure must not re-notify.
        gate.capture_failed(&CaptureFailure::required("again"));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_late_termination_listener_observes_the_existing_failure() {
        let gate = fail_closed();
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&seen);
        gate.add_termination_listener(&listener(move |failure| {
            probe
                .lock()
                .expect("test mutex")
                .push(failure.reason().to_owned());
        }));
        assert_eq!(
            seen.lock().expect("test mutex").as_slice(),
            ["kafka down".to_owned()]
        );
    }

    #[test]
    fn a_fail_open_transition_does_not_notify_termination_listeners() {
        let gate = fail_open();
        let seen = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&seen);
        gate.add_termination_listener(&listener(move |_| {
            probe.fetch_add(1, Ordering::SeqCst);
        }));
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        assert_eq!(gate.state(), State::PassThrough);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "pass-through is not termination"
        );
    }

    #[test]
    fn the_transition_listener_sees_each_canonical_transition() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&seen);
        let gate = CaptureProcessState::with_transition_listener(
            CaptureFailurePolicy::FailOpen,
            Arc::new(move |state| probe.lock().expect("test mutex").push(state)),
        );
        gate.capture_failed(&CaptureFailure::required("kafka down"));
        gate.capture_failed(&CaptureFailure::unstable("corrupted"));
        assert_eq!(
            seen.lock().expect("test mutex").as_slice(),
            [State::PassThrough, State::Terminating]
        );
    }

    #[test]
    fn a_panicking_transition_listener_cannot_block_termination() {
        let gate = CaptureProcessState::with_transition_listener(
            CaptureFailurePolicy::FailClosed,
            Arc::new(|_| panic!("metrics exporter exploded")),
        );
        assert_eq!(
            gate.capture_failed(&CaptureFailure::required("kafka down")),
            State::Terminating
        );
        assert!(gate.is_terminating());
    }

    #[test]
    fn concurrent_forwarders_and_one_failure_never_disagree() {
        // The property that matters: no thread may observe a successful forward admission after the
        // gate has closed. A load-then-store atomic would permit exactly that interleaving.
        use std::sync::Barrier;

        for _ in 0..64 {
            let gate = Arc::new(fail_closed());
            let threads = 8;
            let barrier = Arc::new(Barrier::new(threads + 1));
            let admitted_after_close = Arc::new(AtomicUsize::new(0));

            let mut handles = Vec::new();
            for _ in 0..threads {
                let gate = Arc::clone(&gate);
                let barrier = Arc::clone(&barrier);
                let violations = Arc::clone(&admitted_after_close);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..64 {
                        // Inside the gate, re-read the state. If admission and closure were not
                        // mutually exclusive, this could observe Terminating.
                        // The closure receives the state read inside the critical
                        // section. It must never be Terminating: that would mean admission and
                        // closure were not mutually exclusive.
                        let observed = gate.forward_if_permitted(|state| state);
                        if observed == Some(State::Terminating) {
                            violations.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }));
            }

            let closer = {
                let gate = Arc::clone(&gate);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    gate.capture_failed(&CaptureFailure::required("kafka down"));
                })
            };

            for handle in handles {
                handle.join().expect("worker thread");
            }
            closer.join().expect("closer thread");

            assert_eq!(
                admitted_after_close.load(Ordering::SeqCst),
                0,
                "a forward was admitted while the gate was closed"
            );
            assert!(gate.is_terminating());
        }
    }
}
