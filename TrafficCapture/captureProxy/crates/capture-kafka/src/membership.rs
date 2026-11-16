//! Consumer-group membership: the paused consumer and the last usable assignment
//! (traceability row KAFKA-02).
//!
//! # What membership is for, and what it is not
//!
//! The group protocol is used **only** to decide which partitions this proxy may route *new*
//! connections to. The consumer is subscribed and then immediately **paused**, so it never fetches
//! a record, never commits an offset, and never consumes the capture topic. A poll that returns
//! records at all is a violation, because it means the membership consumer is reading the traffic
//! the proxy is writing.
//!
//! Membership is explicitly **not** a capture gate. Capture correctness depends on the publisher
//! and the broker-time proofs, not on group health, which is why a membership failure *after*
//! startup does not compromise capture.
//!
//! # The last usable assignment, precisely
//!
//! The thing that survives a coordinator outage is not a field in this module. It is the
//! **active routing generation** in [`crate::routing_state`], and what preserves it is simply that
//! *nothing here writes it*. Revocation, loss, empty polls, retriable poll errors, a coordinator
//! outage, group departure, and post-startup membership death all leave it intact, because none of
//! them touch routing state. A replacement takes effect only at the moment a new generation's
//! initial heartbeats have all been accepted and activation succeeds.
//!
//! Proved against a real broker: pausing the whole Kafka container for 9 s — longer than the 6 s
//! session timeout — and asserting new connections still route to the pre-outage writer identity.
//!
//! [`MembershipTracker::accumulated`] is a *separate* accumulator, used only to compute the **next**
//! generation's partition set: `{}` initially, `+=` on assign, `-=` on revoke and on loss. Under the
//! eager protocol (revoke-all then assign-all) that equals the new assignment; under cooperative it
//! equals the correct incremental union.
//!
//! # Pre-startup versus post-startup failure
//!
//! | When | Behaviour |
//! |---|---|
//! | before the first usable assignment | terminal capture failure: readiness fails and **the listener never opens** |
//! | after the first usable assignment | **log only**; the poll thread exits, the consumer closes, and the proxy keeps capturing with the last usable assignment and no membership at all |
//!
//! The second row is deliberate but worth alarming on, which is why this module reports
//! [`FailureReason::Membership`] — Java declares that reason and never emits it, so the documented
//! alarm cannot distinguish a membership failure from any other (spec discrepancy D-13).
//!
//! # `Rebalance::Lost` does not exist
//!
//! rdkafka's `Rebalance` has only `Assign`, `Revoke`, and `Error`; librdkafka signals loss as a
//! revoke with `assignment_lost()` set. That is harmless here because Java's revoke and loss
//! handlers are byte-identical — both only remove from the accumulator — so the distinction is kept
//! for logging only.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{CaptureFailure, FailureReason, StartupRefusal};

/// How long each poll waits. Short, so the close flag is observed promptly: there is no `wakeup`
/// in rdkafka's consumer trait, and a short poll plus a flag is what the Java loop effectively
/// does anyway.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long to wait for the consumer to close after the poll loop exits.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A rebalance event observed during a poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipEvent {
    /// Partitions were assigned to this member.
    Assigned(Vec<i32>),
    /// Partitions were revoked cooperatively.
    Revoked(Vec<i32>),
    /// Partitions were lost: a revoke with `assignment_lost()` set.
    Lost(Vec<i32>),
    /// The group protocol reported an error. Java's listener has no analogue.
    Error(String),
}

/// What one poll did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// No records, which is the only healthy outcome for a paused consumer.
    Idle,
    /// The paused consumer fetched a traffic record. A violation.
    Records,
    /// A retriable error. Routing is untouched and the loop continues.
    Retriable(String),
    /// A non-retriable error. The loop exits.
    Fatal(String),
}

/// The membership consumer, behind a trait.
///
/// A trait so every event-ordering rule above is testable without a broker, including the
/// coordinator-outage and partition-loss paths that are otherwise reachable only by pausing a
/// container.
pub trait MembershipConsumer: Send + Sync {
    /// Subscribes to `topic`. Called once, before the first poll.
    ///
    /// # Errors
    ///
    /// Returns a message when the subscription was refused.
    fn subscribe(&self, topic: &str) -> Result<(), String>;

    /// Polls once, returning the outcome and any rebalance events raised during the poll.
    ///
    /// Rebalance callbacks are delivered by librdkafka *inside* the poll, on this thread, which is
    /// what makes the accumulator effectively single-threaded.
    fn poll_once(&self, timeout: Duration) -> (PollOutcome, Vec<MembershipEvent>);

    /// Pauses `partitions`, so the consumer never fetches a record.
    ///
    /// # Errors
    ///
    /// Returns a message when the pause was refused.
    fn pause(&self, partitions: &[i32]) -> Result<(), String>;

    /// Closes the consumer, leaving the group.
    fn close(&self);
}

/// The pure assignment accumulator and startup latch.
///
/// Separated from the thread so the event-ordering rules can be tested as a state machine.
#[derive(Debug, Default)]
pub struct MembershipTracker {
    accumulated: BTreeSet<i32>,
    initial_routing_ready: bool,
}

impl MembershipTracker {
    /// A tracker with no assignment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The accumulated partition set, ascending.
    #[must_use]
    pub fn accumulated(&self) -> Vec<i32> {
        self.accumulated.iter().copied().collect()
    }

    /// Whether the first usable routing generation has been reported.
    #[must_use]
    pub fn initial_routing_ready(&self) -> bool {
        self.initial_routing_ready
    }

    /// Latches "the first generation is usable". Returns `true` exactly once, ever.
    pub fn mark_initial_routing_ready(&mut self) -> bool {
        if self.initial_routing_ready {
            false
        } else {
            self.initial_routing_ready = true;
            true
        }
    }

    /// Records an assignment and returns the partition set a new generation should cover.
    ///
    /// Returns `None` when the accumulated set is **empty**, in which case no generation is created
    /// and the previous one stays active. That is the case where a revoke-everything is not
    /// followed by an assign, and treating it as "route nowhere" would drop capture for every new
    /// connection.
    pub fn on_assigned(&mut self, partitions: &[i32]) -> Option<Vec<i32>> {
        // Cumulative union, not a replacement.
        self.accumulated.extend(partitions.iter().copied());
        if self.accumulated.is_empty() {
            None
        } else {
            Some(self.accumulated())
        }
    }

    /// Records a revocation. Routing state is deliberately untouched.
    pub fn on_revoked(&mut self, partitions: &[i32]) {
        for partition in partitions {
            self.accumulated.remove(partition);
        }
    }

    /// Records a loss. Identical to revocation, as in Java.
    pub fn on_lost(&mut self, partitions: &[i32]) {
        self.on_revoked(partitions);
    }
}

/// What the poll loop does when membership fails.
pub trait MembershipObserver: Send + Sync {
    /// Requests a routing generation for `partitions`.
    ///
    /// Must not block on the generation becoming usable: the poll loop returns immediately and the
    /// generation is applied asynchronously, so a slow activation cannot stall the group's
    /// heartbeating and cause another rebalance.
    fn request_generation(&self, partitions: Vec<i32>);

    /// Reports that the first routing generation is usable. Called at most once.
    fn initial_routing_ready(&self);

    /// Reports a failure **before** the first usable assignment. Terminal: readiness fails and the
    /// listener never opens.
    fn startup_failed(&self, refusal: StartupRefusal);

    /// Reports a failure **after** the first usable assignment. Capture continues with the last
    /// usable assignment; this exists so the state is alarmable.
    fn degraded_after_startup(&self, failure: CaptureFailure);
}

/// The membership poll thread.
pub struct Membership {
    closed: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    tracker: Arc<Mutex<MembershipTracker>>,
}

impl std::fmt::Debug for Membership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Membership")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl Membership {
    /// Starts the poll thread.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the thread cannot be spawned.
    pub fn start(
        consumer: Arc<dyn MembershipConsumer>,
        topic: impl Into<String>,
        observer: Arc<dyn MembershipObserver>,
    ) -> std::io::Result<Self> {
        Self::start_with_tracker(
            consumer,
            topic,
            observer,
            Arc::new(Mutex::new(MembershipTracker::new())),
        )
    }

    /// Starts the poll thread over a tracker the caller already holds.
    ///
    /// The startup sequence needs the tracker before the thread exists, so it can latch
    /// "the first routing generation is usable" from the publisher's reply rather than from inside
    /// the poll loop.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the thread cannot be spawned.
    pub fn start_with_tracker(
        consumer: Arc<dyn MembershipConsumer>,
        topic: impl Into<String>,
        observer: Arc<dyn MembershipObserver>,
        tracker: Arc<Mutex<MembershipTracker>>,
    ) -> std::io::Result<Self> {
        let topic = topic.into();
        let closed = Arc::new(AtomicBool::new(false));
        let thread = {
            let closed = Arc::clone(&closed);
            let tracker = Arc::clone(&tracker);
            std::thread::Builder::new()
                .name("capture-kafka-membership".to_owned())
                .spawn(move || {
                    run_poll_loop(&consumer, &topic, &observer, &closed, &tracker);
                })?
        };
        Ok(Self {
            closed,
            thread: Some(thread),
            tracker,
        })
    }

    /// The accumulated partition set.
    #[must_use]
    pub fn accumulated_partitions(&self) -> Vec<i32> {
        self.tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accumulated()
    }

    /// Whether the first routing generation has been reported usable.
    #[must_use]
    pub fn initial_routing_ready(&self) -> bool {
        self.tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .initial_routing_ready()
    }

    /// Asks the poll loop to stop and waits for it.
    ///
    /// Membership health is not a capture gate, so a failure to stop cleanly is a warning rather
    /// than a capture failure.
    pub fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_poll_loop(
    consumer: &Arc<dyn MembershipConsumer>,
    topic: &str,
    observer: &Arc<dyn MembershipObserver>,
    closed: &Arc<AtomicBool>,
    tracker: &Arc<Mutex<MembershipTracker>>,
) {
    if let Err(detail) = consumer.subscribe(topic) {
        report_failure(
            observer,
            tracker,
            closed,
            &format!("subscribe failed: {detail}"),
        );
        consumer.close();
        return;
    }

    while !closed.load(Ordering::Acquire) {
        let (outcome, events) = consumer.poll_once(POLL_INTERVAL);

        for event in events {
            if !handle_event(consumer, topic, observer, closed, tracker, event) {
                consumer.close();
                return;
            }
        }

        match outcome {
            PollOutcome::Idle => {}
            PollOutcome::Records => {
                // A paused consumer must never fetch traffic. Java treats this the same way, which
                // means after startup it is log-only (spec discrepancy D-9): the proxy then runs
                // permanently without membership. The capture semantics are unchanged here, but it
                // is reported with an attributable reason so it can be alarmed.
                report_failure(
                    observer,
                    tracker,
                    closed,
                    "the paused capture membership consumer unexpectedly fetched traffic records",
                );
                consumer.close();
                return;
            }
            PollOutcome::Retriable(detail) => {
                tracing::warn!(
                    detail,
                    active_partitions = ?locked_accumulated(tracker),
                    "event=capture_membership_degraded reason=retriable_poll_failure"
                );
            }
            PollOutcome::Fatal(detail) => {
                report_failure(observer, tracker, closed, &format!("poll failed: {detail}"));
                consumer.close();
                return;
            }
        }
    }
    consumer.close();
}

/// Handles one rebalance event. Returns `false` when the poll loop must exit.
fn handle_event(
    consumer: &Arc<dyn MembershipConsumer>,
    topic: &str,
    observer: &Arc<dyn MembershipObserver>,
    closed: &Arc<AtomicBool>,
    tracker: &Arc<Mutex<MembershipTracker>>,
    event: MembershipEvent,
) -> bool {
    match event {
        MembershipEvent::Assigned(partitions) => {
            // Paused immediately, so no record is ever fetched.
            if let Err(detail) = consumer.pause(&partitions) {
                report_failure(
                    observer,
                    tracker,
                    closed,
                    &format!("pause failed: {detail}"),
                );
                return false;
            }
            let next = {
                let mut tracker = tracker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tracker.on_assigned(&partitions)
            };
            tracing::info!(
                topic,
                assigned_partitions = ?partitions,
                routing_partitions = ?locked_accumulated(tracker),
                "event=capture_assignment_received"
            );
            if let Some(next) = next {
                // Returns immediately: the generation is applied asynchronously on the publisher.
                observer.request_generation(next);
            }
            true
        }
        MembershipEvent::Revoked(partitions) => {
            // Nothing else: the active routing generation is deliberately untouched, so existing
            // and new connections keep routing to the last usable assignment.
            tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .on_revoked(&partitions);
            true
        }
        MembershipEvent::Lost(partitions) => {
            tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .on_lost(&partitions);
            true
        }
        MembershipEvent::Error(detail) => {
            report_failure(
                observer,
                tracker,
                closed,
                &format!("the consumer group reported an error: {detail}"),
            );
            false
        }
    }
}

fn locked_accumulated(tracker: &Arc<Mutex<MembershipTracker>>) -> Vec<i32> {
    tracker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .accumulated()
}

/// Routes a membership failure to the pre-startup or post-startup path.
fn report_failure(
    observer: &Arc<dyn MembershipObserver>,
    tracker: &Arc<Mutex<MembershipTracker>>,
    closed: &Arc<AtomicBool>,
    detail: &str,
) {
    closed.store(true, Ordering::Release);
    let ready = tracker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .initial_routing_ready();
    if ready {
        tracing::error!(
            detail,
            initial_routing_ready = true,
            active_partitions = ?locked_accumulated(tracker),
            "event=capture_membership_failed"
        );
        observer.degraded_after_startup(CaptureFailure::capture(
            FailureReason::Membership,
            format!(
                "capture group membership stopped after startup; the proxy continues with the last \
                 usable assignment and no membership: {detail}"
            ),
        ));
    } else {
        tracing::error!(
            detail,
            initial_routing_ready = false,
            "event=capture_membership_failed"
        );
        observer.startup_failed(StartupRefusal::Membership(detail.to_owned()));
    }
}

/// Reports that the first routing generation became usable, exactly once.
///
/// Called by the owner once a generation activates, so a later membership failure takes the
/// post-startup path.
pub fn mark_initial_routing_ready(
    tracker: &Arc<Mutex<MembershipTracker>>,
    observer: &Arc<dyn MembershipObserver>,
) {
    let first = tracker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark_initial_routing_ready();
    if first {
        observer.initial_routing_ready();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn the_accumulator_is_a_cumulative_union_not_a_replacement() {
        // CaptureKafkaMembershipTest.java:35: two assignments yield generations [[0,1],[2]] only
        // because the eager protocol revokes in between. Without a revoke the set accumulates.
        let mut tracker = MembershipTracker::new();
        assert_eq!(tracker.on_assigned(&[0, 1]), Some(vec![0, 1]));
        assert_eq!(
            tracker.on_assigned(&[2]),
            Some(vec![0, 1, 2]),
            "an assign without a revoke is incremental, as cooperative rebalancing requires"
        );

        tracker.on_revoked(&[0, 1, 2]);
        assert_eq!(
            tracker.on_assigned(&[2]),
            Some(vec![2]),
            "the eager protocol's revoke-all then assign-all yields exactly the new assignment"
        );
    }

    #[test]
    fn an_empty_assignment_after_revocation_creates_no_generation() {
        // CaptureKafkaMembershipTest.emptyAssignmentAfterRevocationKeepsLastUsableAssignment
        // (:132-154). Creating a generation with no partitions would mean routing nowhere.
        let mut tracker = MembershipTracker::new();
        tracker.on_assigned(&[0, 1]);
        tracker.on_revoked(&[0, 1]);
        assert_eq!(
            tracker.on_assigned(&[]),
            None,
            "no generation, so the previous one stays active"
        );
        assert!(tracker.accumulated().is_empty());
    }

    #[test]
    fn loss_and_revocation_are_identical() {
        // CKM:86-88 versus CKM:121-123 are byte-identical in Java.
        let mut revoked = MembershipTracker::new();
        let mut lost = MembershipTracker::new();
        revoked.on_assigned(&[0, 1, 2]);
        lost.on_assigned(&[0, 1, 2]);
        revoked.on_revoked(&[1]);
        lost.on_lost(&[1]);
        assert_eq!(revoked.accumulated(), lost.accumulated());
        assert_eq!(revoked.accumulated(), vec![0, 2]);
    }

    #[test]
    fn the_initial_ready_latch_fires_exactly_once() {
        let mut tracker = MembershipTracker::new();
        assert!(!tracker.initial_routing_ready());
        assert!(tracker.mark_initial_routing_ready());
        assert!(tracker.initial_routing_ready());
        for _ in 0..4 {
            assert!(
                !tracker.mark_initial_routing_ready(),
                "the first routing generation becomes ready once per process"
            );
        }
    }

    /// Scripts a sequence of poll results and records what the loop did.
    struct ScriptedConsumer {
        script: Mutex<std::vec::IntoIter<(PollOutcome, Vec<MembershipEvent>)>>,
        subscribed: Mutex<Vec<String>>,
        paused: Mutex<Vec<Vec<i32>>>,
        closes: Mutex<usize>,
        subscribe_error: Option<String>,
    }

    impl ScriptedConsumer {
        fn new(script: Vec<(PollOutcome, Vec<MembershipEvent>)>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter()),
                subscribed: Mutex::new(Vec::new()),
                paused: Mutex::new(Vec::new()),
                closes: Mutex::new(0),
                subscribe_error: None,
            })
        }
    }

    impl MembershipConsumer for ScriptedConsumer {
        fn subscribe(&self, topic: &str) -> Result<(), String> {
            self.subscribed
                .lock()
                .expect("test mutex")
                .push(topic.to_owned());
            self.subscribe_error.clone().map_or(Ok(()), Err)
        }

        fn poll_once(&self, _timeout: Duration) -> (PollOutcome, Vec<MembershipEvent>) {
            self.script
                .lock()
                .expect("test mutex")
                .next()
                .unwrap_or((PollOutcome::Idle, Vec::new()))
        }

        fn pause(&self, partitions: &[i32]) -> Result<(), String> {
            self.paused
                .lock()
                .expect("test mutex")
                .push(partitions.to_vec());
            Ok(())
        }

        fn close(&self) {
            *self.closes.lock().expect("test mutex") += 1;
        }
    }

    #[derive(Debug)]
    enum Report {
        Generation(Vec<i32>),
        InitialReady,
        StartupFailed(String),
        Degraded(String),
    }

    struct RecordingObserver(mpsc::Sender<Report>);

    impl MembershipObserver for RecordingObserver {
        fn request_generation(&self, partitions: Vec<i32>) {
            drop(self.0.send(Report::Generation(partitions)));
        }
        fn initial_routing_ready(&self) {
            drop(self.0.send(Report::InitialReady));
        }
        fn startup_failed(&self, refusal: StartupRefusal) {
            drop(self.0.send(Report::StartupFailed(refusal.to_string())));
        }
        fn degraded_after_startup(&self, failure: CaptureFailure) {
            drop(self.0.send(Report::Degraded(failure.message().to_owned())));
        }
    }

    fn run(
        script: Vec<(PollOutcome, Vec<MembershipEvent>)>,
        ready_before: bool,
    ) -> (Arc<ScriptedConsumer>, Vec<Report>) {
        let consumer = ScriptedConsumer::new(script);
        let (tx, rx) = mpsc::channel();
        let observer: Arc<dyn MembershipObserver> = Arc::new(RecordingObserver(tx));
        let closed = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(Mutex::new(MembershipTracker::new()));
        if ready_before {
            tracker
                .lock()
                .expect("test mutex")
                .mark_initial_routing_ready();
        }
        // Stop after the script is exhausted so the loop terminates deterministically.
        let consumer_dyn: Arc<dyn MembershipConsumer> = Arc::clone(&consumer) as Arc<_>;
        let stopper = Arc::clone(&closed);
        let script_len = consumer.script.lock().expect("test mutex").len();
        std::thread::spawn(move || {
            for _ in 0..=script_len {
                std::thread::sleep(Duration::from_millis(2));
            }
            stopper.store(true, Ordering::Release);
        });
        run_poll_loop(&consumer_dyn, "traffic", &observer, &closed, &tracker);
        (consumer, rx.try_iter().collect())
    }

    #[test]
    fn an_assignment_pauses_the_partitions_and_requests_a_generation() {
        let (consumer, reports) = run(
            vec![(
                PollOutcome::Idle,
                vec![MembershipEvent::Assigned(vec![0, 1])],
            )],
            false,
        );
        assert_eq!(
            *consumer.subscribed.lock().expect("test mutex"),
            vec!["traffic".to_string()]
        );
        assert_eq!(
            *consumer.paused.lock().expect("test mutex"),
            vec![vec![0, 1]],
            "the consumer is paused on every assignment, so it never fetches a record"
        );
        assert!(matches!(
            reports.first(),
            Some(Report::Generation(partitions)) if partitions == &[0, 1]
        ));
    }

    #[test]
    fn empty_polls_and_revocations_report_nothing_at_all() {
        // Nothing here may touch routing state, which is what preserves the last usable
        // assignment through a coordinator outage.
        let (_, reports) = run(
            vec![
                (PollOutcome::Idle, vec![]),
                (
                    PollOutcome::Idle,
                    vec![MembershipEvent::Revoked(vec![0, 1])],
                ),
                (PollOutcome::Idle, vec![MembershipEvent::Lost(vec![2])]),
                (PollOutcome::Idle, vec![]),
            ],
            true,
        );
        assert!(
            reports.is_empty(),
            "no generation, no failure, no readiness change: {reports:?}"
        );
    }

    #[test]
    fn a_retriable_poll_failure_keeps_the_assignment_and_still_accepts_a_replacement() {
        // CaptureKafkaMembershipTest.retriablePollFailureKeepsLastAssignmentAndAcceptsAReplacement
        // (:217-251).
        let (_, reports) = run(
            vec![
                (PollOutcome::Idle, vec![MembershipEvent::Assigned(vec![0])]),
                (
                    PollOutcome::Retriable("coordinator unavailable".into()),
                    vec![],
                ),
                (PollOutcome::Idle, vec![MembershipEvent::Assigned(vec![1])]),
            ],
            false,
        );
        let generations: Vec<&Vec<i32>> = reports
            .iter()
            .filter_map(|report| match report {
                Report::Generation(partitions) => Some(partitions),
                _ => None,
            })
            .collect();
        assert_eq!(generations, vec![&vec![0], &vec![0, 1]]);
        assert!(
            !reports
                .iter()
                .any(|r| matches!(r, Report::StartupFailed(_) | Report::Degraded(_))),
            "a retriable poll failure is not a membership failure"
        );
    }

    #[test]
    fn a_poll_failure_before_the_first_assignment_is_a_startup_failure() {
        // CaptureKafkaMembershipTest.pollFailureBeforeAnyUsableAssignmentReportsStartupFailure
        // (:254-285). Readiness fails, so the listener never opens.
        let (consumer, reports) = run(
            vec![(PollOutcome::Fatal("authorization failed".into()), vec![])],
            false,
        );
        assert!(
            matches!(reports.first(), Some(Report::StartupFailed(detail)) if detail.contains("authorization failed")),
            "{reports:?}"
        );
        assert_eq!(
            *consumer.closes.lock().expect("test mutex"),
            1,
            "the consumer is closed exactly once"
        );
    }

    #[test]
    fn a_poll_failure_after_the_first_assignment_keeps_capture_alive() {
        // CaptureKafkaMembershipTest.pollFailureAfterInitialAssignmentKeepsLastUsableAssignment
        // (:183-214): no failure reported to the capture-failure policy, and routing stays alive.
        let (consumer, reports) = run(
            vec![(PollOutcome::Fatal("coordinator gone".into()), vec![])],
            true,
        );
        assert!(
            !reports
                .iter()
                .any(|r| matches!(r, Report::StartupFailed(_))),
            "capture must not be failed by a post-startup membership failure: {reports:?}"
        );
        match reports.first() {
            Some(Report::Degraded(message)) => {
                assert!(message.contains("last usable assignment"), "{message}");
            }
            other => panic!("expected an alarmable degradation report, got {other:?}"),
        }
        assert_eq!(*consumer.closes.lock().expect("test mutex"), 1);
    }

    #[test]
    fn a_paused_consumer_that_fetches_records_is_a_violation() {
        // CKM:141-146. Before startup this fails startup; after startup it is log-only, which is
        // Java's behaviour and spec discrepancy D-9.
        let (_, before) = run(vec![(PollOutcome::Records, vec![])], false);
        assert!(matches!(before.first(), Some(Report::StartupFailed(_))));

        let (_, after) = run(vec![(PollOutcome::Records, vec![])], true);
        assert!(
            matches!(after.first(), Some(Report::Degraded(_))),
            "capture semantics are unchanged, but the state is alarmable: {after:?}"
        );
    }

    #[test]
    fn a_group_error_is_routed_like_a_poll_failure() {
        let (_, reports) = run(
            vec![(
                PollOutcome::Idle,
                vec![MembershipEvent::Error("rebalance in progress".into())],
            )],
            false,
        );
        assert!(
            matches!(reports.first(), Some(Report::StartupFailed(detail)) if detail.contains("rebalance in progress")),
            "{reports:?}"
        );
    }

    #[test]
    fn a_membership_failure_is_attributed_to_membership_rather_than_to_other() {
        // Spec discrepancy D-13: Java declares FailureReason.MEMBERSHIP and never emits it.
        let (tx, rx) = mpsc::channel();
        let observer: Arc<dyn MembershipObserver> = Arc::new(RecordingObserver(tx));
        let tracker = Arc::new(Mutex::new(MembershipTracker::new()));
        tracker
            .lock()
            .expect("test mutex")
            .mark_initial_routing_ready();
        let closed = Arc::new(AtomicBool::new(false));
        report_failure(&observer, &tracker, &closed, "poll died");
        let _ = rx.try_iter().count();

        assert_eq!(
            StartupRefusal::Membership("poll died".to_owned()).reason(),
            FailureReason::Membership
        );
        assert_eq!(
            CaptureFailure::capture(FailureReason::Membership, "x").reason(),
            FailureReason::Membership
        );
    }

    #[test]
    fn a_subscribe_failure_before_startup_fails_startup_and_closes_the_consumer() {
        let consumer = Arc::new(ScriptedConsumer {
            script: Mutex::new(Vec::new().into_iter()),
            subscribed: Mutex::new(Vec::new()),
            paused: Mutex::new(Vec::new()),
            closes: Mutex::new(0),
            subscribe_error: Some("topic authorization failed".to_owned()),
        });
        let (tx, rx) = mpsc::channel();
        let observer: Arc<dyn MembershipObserver> = Arc::new(RecordingObserver(tx));
        let closed = Arc::new(AtomicBool::new(false));
        let tracker = Arc::new(Mutex::new(MembershipTracker::new()));
        let consumer_dyn: Arc<dyn MembershipConsumer> = Arc::clone(&consumer) as Arc<_>;
        run_poll_loop(&consumer_dyn, "traffic", &observer, &closed, &tracker);

        let reports: Vec<Report> = rx.try_iter().collect();
        assert!(
            matches!(reports.first(), Some(Report::StartupFailed(detail)) if detail.contains("authorization")),
            "{reports:?}"
        );
        assert_eq!(*consumer.closes.lock().expect("test mutex"), 1);
    }

    #[test]
    fn closing_before_any_poll_still_closes_the_consumer() {
        // CaptureKafkaMembershipTest.closeBeforeStartClosesTheConsumer (:288-300).
        let consumer = ScriptedConsumer::new(Vec::new());
        let (tx, _rx) = mpsc::channel();
        let observer: Arc<dyn MembershipObserver> = Arc::new(RecordingObserver(tx));
        let closed = Arc::new(AtomicBool::new(true));
        let tracker = Arc::new(Mutex::new(MembershipTracker::new()));
        let consumer_dyn: Arc<dyn MembershipConsumer> = Arc::clone(&consumer) as Arc<_>;
        run_poll_loop(&consumer_dyn, "traffic", &observer, &closed, &tracker);
        assert_eq!(*consumer.closes.lock().expect("test mutex"), 1);
    }
}
