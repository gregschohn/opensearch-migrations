//! The heartbeat acknowledgement deadline arbiter and its independent clock
//! (traceability row KAFKA-03).
//!
//! # Why this is its own thread
//!
//! The `E` deadline exists to detect that a heartbeat acknowledgement did **not** arrive. If the
//! component that notices were the same component that is busy publishing, then publisher backlog
//! would delay the timer and a stalled publisher could hide its own stall — the failure mode the
//! deadline is for. So the clock is a dedicated OS thread with its own monotonic timer heap
//! (`CaptureKafkaPublisher.java:259-263`: *"publisher backlog cannot win a heartbeat-deadline race
//! by delaying the timer"*), and it never touches the actor's mailbox before deciding.
//!
//! # The arbiter
//!
//! [`AcknowledgementDeadline`] is a four-state cell and **exactly one** transition out of
//! [`DeadlineStatus::Pending`] is permitted. The decision is made by a compare-and-exchange, *not*
//! by cancelling a timer: cancellation is best-effort cleanup and the status is authoritative. That
//! is what makes the outcome well-defined when an acknowledgement and its deadline are
//! simultaneous.
//!
//! # Where the CAS must run
//!
//! [`AcknowledgementDeadline::acknowledge`] must be called **on the delivery-report thread, before
//! any hop to the publisher actor**. An acknowledgement that physically arrived before `E` must
//! win even if the actor is backlogged past `E`, because the record *was* durably appended — the
//! protocol question is when Kafka acknowledged, not when this process got round to processing it.
//! Pinned by
//! `CaptureKafkaPublisherTest.acknowledgementThatArrivesBeforeDeadlineWinsEvenIfPublisherThreadProcessesItLater`.
//!
//! # Arming order
//!
//! An initial heartbeat's deadline is armed **after** the producer enqueue call returns, never
//! before (`CaptureKafkaPublisher.java:952-954`, spec discrepancy D-18). A blocked enqueue
//! therefore cannot be charged against `E`. The worst-case window is `enqueue block time + E`.
//!
//! A periodic heartbeat reuses the deadline created when the **previous** acknowledgement was
//! accepted, armed at acceptance time, so heartbeat *n+1*'s deadline is `E` after heartbeat *n*'s
//! acceptance.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Identifies one publisher lane to the deadline clock.
///
/// Deliberately an opaque integer rather than a `(writerNodeId, partition)` pair: the clock must
/// hold nothing that needs allocation or locking to compare, and the actor owns the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LaneId(pub u64);

/// The four states of one acknowledgement deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineStatus {
    /// Neither the acknowledgement nor the deadline has resolved it.
    Pending,
    /// Kafka acknowledged in time. The deadline lost.
    Acknowledged,
    /// `E` elapsed first. Capture is compromised.
    Expired,
    /// The publisher or lane resolved it first: retirement, replacement, failure cleanup, or close.
    Cancelled,
}

impl DeadlineStatus {
    const PENDING: u8 = 0;
    const ACKNOWLEDGED: u8 = 1;
    const EXPIRED: u8 = 2;
    const CANCELLED: u8 = 3;

    fn from_u8(value: u8) -> Self {
        match value {
            Self::PENDING => Self::Pending,
            Self::ACKNOWLEDGED => Self::Acknowledged,
            Self::EXPIRED => Self::Expired,
            Self::CANCELLED => Self::Cancelled,
            other => unreachable!("deadline status {other} is not one of the four states"),
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Pending => Self::PENDING,
            Self::Acknowledged => Self::ACKNOWLEDGED,
            Self::Expired => Self::EXPIRED,
            Self::Cancelled => Self::CANCELLED,
        }
    }

    /// Whether this is a resolved state.
    #[must_use]
    pub fn is_resolved(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One heartbeat's acknowledgement deadline.
///
/// Shared between the publisher actor (which creates, renews, and cancels it), the delivery-report
/// thread (which acknowledges it), and the deadline clock (which expires it).
#[derive(Debug)]
pub struct AcknowledgementDeadline {
    status: AtomicU8,
    lane: LaneId,
    epoch: u64,
}

impl AcknowledgementDeadline {
    /// A fresh `Pending` deadline for `lane`.
    ///
    /// `epoch` distinguishes successive deadlines for the same lane in logs and in the clock's
    /// bookkeeping; the arbitration itself needs only this cell's identity.
    #[must_use]
    pub fn new(lane: LaneId, epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            status: AtomicU8::new(DeadlineStatus::PENDING),
            lane,
            epoch,
        })
    }

    /// Which lane this deadline belongs to.
    #[must_use]
    pub fn lane(&self) -> LaneId {
        self.lane
    }

    /// This deadline's epoch within its lane.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The current status.
    #[must_use]
    pub fn status(&self) -> DeadlineStatus {
        DeadlineStatus::from_u8(self.status.load(AtomicOrdering::Acquire))
    }

    fn transition(&self, to: DeadlineStatus) -> bool {
        self.status
            .compare_exchange(
                DeadlineStatus::PENDING,
                to.as_u8(),
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// Claims the deadline for an arriving acknowledgement.
    ///
    /// Returns `true` when this call won. **Call this on the delivery-report thread before any hop
    /// to the publisher actor.**
    pub fn acknowledge(&self) -> bool {
        self.transition(DeadlineStatus::Acknowledged)
    }

    /// Claims the deadline for expiry. Returns `true` when this call won.
    pub fn expire(&self) -> bool {
        self.transition(DeadlineStatus::Expired)
    }

    /// Claims the deadline for the publisher. Returns `true` when this call won.
    ///
    /// Call sites, all on the publisher actor: replacing a lane's deadline, accepting an
    /// acknowledgement for a lane that is no longer `OPEN`, beginning retirement with no heartbeat
    /// in flight, completing lane retirement, failure cleanup, and close.
    pub fn cancel(&self) -> bool {
        self.transition(DeadlineStatus::Cancelled)
    }

    /// Whether this deadline is still awaiting resolution.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.status().is_resolved()
    }
}

/// What the deadline clock does when a deadline expires.
///
/// Implemented by the publisher's failure path. Both methods run **on the clock thread**, so
/// neither may block on the publisher's mailbox, on Kafka, or on anything that a backlogged
/// publisher holds.
pub trait DeadlineSink: Send + Sync {
    /// Whether every pending deadline is already moot.
    ///
    /// Checked **before** the expiry CAS, matching `CaptureKafkaPublisher.java:750`: a deadline
    /// firing after an unrelated capture failure leaves its status `Pending` so the later cancel
    /// records who really resolved it. Without this ordering a post-failure timer would masquerade
    /// as the cause.
    fn is_moot(&self) -> bool;

    /// Reports that `deadline` expired and that this call won the CAS.
    ///
    /// Must trip the capture write gate and count the violation. Must not wait for the publisher.
    fn on_expired(&self, deadline: &Arc<AcknowledgementDeadline>);
}

struct Scheduled {
    at: Instant,
    sequence: u64,
    deadline: Arc<AcknowledgementDeadline>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.sequence == other.sequence
    }
}

impl Eq for Scheduled {}

impl Ord for Scheduled {
    /// Reversed so `BinaryHeap` (a max-heap) yields the **earliest** deadline first.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .at
            .cmp(&self.at)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct ClockQueue {
    heap: BinaryHeap<Scheduled>,
    stopped: bool,
}

/// The independent monotonic deadline clock.
///
/// Owns one OS thread named `capture-kafka-heartbeat-deadline`. Time is measured with
/// [`Instant`] — never a wall clock, so a system time adjustment cannot move a deadline.
pub struct DeadlineClock {
    shared: Arc<ClockShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct ClockShared {
    queue: Mutex<ClockQueue>,
    wakeup: Condvar,
    sequence: AtomicU64,
    sink: Arc<dyn DeadlineSink>,
}

impl std::fmt::Debug for DeadlineClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadlineClock")
            .field("armed", &self.armed_len())
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl DeadlineClock {
    /// Starts the clock thread.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the thread cannot be spawned. A capture process that cannot own its
    /// deadline clock must not start.
    pub fn start(sink: Arc<dyn DeadlineSink>) -> std::io::Result<Self> {
        let shared = Arc::new(ClockShared {
            queue: Mutex::new(ClockQueue::default()),
            wakeup: Condvar::new(),
            sequence: AtomicU64::new(0),
            sink,
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("capture-kafka-heartbeat-deadline".to_owned())
            .spawn(move || run_clock(&worker))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// A handle for arming deadlines from the publisher actor.
    #[must_use]
    pub fn handle(&self) -> DeadlineClockHandle {
        DeadlineClockHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// How many deadlines are armed, including resolved ones not yet reaped.
    #[must_use]
    pub fn armed_len(&self) -> usize {
        lock_queue(&self.shared.queue).heap.len()
    }

    /// Stops the clock thread and waits for it, dropping every armed deadline.
    ///
    /// Pending deadlines are **not** expired on shutdown: shutdown is not evidence that an
    /// acknowledgement was late.
    pub fn stop(&mut self) {
        {
            let mut queue = lock_queue(&self.shared.queue);
            queue.stopped = true;
            queue.heap.clear();
        }
        self.shared.wakeup.notify_all();
        if let Some(thread) = self.thread.take() {
            // A panicked clock thread is reported by the caller's own failure path; there is
            // nothing useful to do with the payload here.
            drop(thread.join());
        }
    }
}

impl Drop for DeadlineClock {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Arms deadlines on a running [`DeadlineClock`].
#[derive(Clone)]
pub struct DeadlineClockHandle {
    shared: Arc<ClockShared>,
}

impl std::fmt::Debug for DeadlineClockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadlineClockHandle")
            .finish_non_exhaustive()
    }
}

impl DeadlineClockHandle {
    /// Arms `deadline` to expire `after` from now.
    ///
    /// Arming an already-resolved deadline is a no-op, which closes the window where an
    /// acknowledgement or a cancel wins *before* the arming call completes
    /// (`AcknowledgementDeadline.setExpirationTask`, `CaptureKafkaPublisher.java:156-161`).
    pub fn arm(&self, deadline: &Arc<AcknowledgementDeadline>, after: Duration) {
        self.arm_at(deadline, Instant::now() + after);
    }

    /// Arms `deadline` to expire at the monotonic instant `at`.
    pub fn arm_at(&self, deadline: &Arc<AcknowledgementDeadline>, at: Instant) {
        if !deadline.is_pending() {
            return;
        }
        let sequence = self.shared.sequence.fetch_add(1, AtomicOrdering::Relaxed);
        {
            let mut queue = lock_queue(&self.shared.queue);
            if queue.stopped {
                return;
            }
            queue.heap.push(Scheduled {
                at,
                sequence,
                deadline: Arc::clone(deadline),
            });
        }
        self.shared.wakeup.notify_all();
    }
}

/// Recovers the queue guard even if a holder panicked.
///
/// The queue is a heap plus a flag with no invariant that a mid-update panic can break, and a clock
/// that refused to lock would silently stop detecting late acknowledgements.
fn lock_queue(queue: &Mutex<ClockQueue>) -> std::sync::MutexGuard<'_, ClockQueue> {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run_clock(shared: &Arc<ClockShared>) {
    loop {
        let due = {
            let mut queue = lock_queue(&shared.queue);
            loop {
                if queue.stopped {
                    return;
                }
                let Some(next_at) = queue.heap.peek().map(|s| s.at) else {
                    // Nothing armed: sleep until something is.
                    let (guard, _) = shared
                        .wakeup
                        .wait_timeout(queue, Duration::from_millis(250))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    queue = guard;
                    continue;
                };
                let now = Instant::now();
                if next_at <= now {
                    break queue.heap.pop();
                }
                let (guard, _) = shared
                    .wakeup
                    .wait_timeout(queue, next_at - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue = guard;
            }
        };

        let Some(scheduled) = due else { continue };

        // Reap a deadline that the acknowledgement, a cancel, or an earlier expiry already
        // resolved. This is the common case: most heartbeats are acknowledged well inside `E`.
        if !scheduled.deadline.is_pending() {
            continue;
        }
        // The moot check precedes the CAS deliberately; see `DeadlineSink::is_moot`.
        if shared.sink.is_moot() {
            continue;
        }
        if scheduled.deadline.expire() {
            shared.sink.on_expired(&scheduled.deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        moot: AtomicBool,
        expired: StdMutex<Vec<(LaneId, u64)>>,
        notify: StdMutex<Option<mpsc::Sender<LaneId>>>,
    }

    impl DeadlineSink for RecordingSink {
        fn is_moot(&self) -> bool {
            self.moot.load(AtomicOrdering::Acquire)
        }

        fn on_expired(&self, deadline: &Arc<AcknowledgementDeadline>) {
            self.expired
                .lock()
                .expect("test mutex")
                .push((deadline.lane(), deadline.epoch()));
            if let Some(tx) = self.notify.lock().expect("test mutex").as_ref() {
                let _ = tx.send(deadline.lane());
            }
        }
    }

    #[test]
    fn exactly_one_transition_out_of_pending_is_permitted() {
        // Exhaustive over every ordering of the three claims, which is the whole state space of
        // the arbiter. No sleeps, no threads: the arbitration is pure.
        type Claim = (&'static str, fn(&AcknowledgementDeadline) -> bool);
        let claims: [Claim; 3] = [
            ("acknowledge", AcknowledgementDeadline::acknowledge),
            ("expire", AcknowledgementDeadline::expire),
            ("cancel", AcknowledgementDeadline::cancel),
        ];
        let expected_status = |name: &str| match name {
            "acknowledge" => DeadlineStatus::Acknowledged,
            "expire" => DeadlineStatus::Expired,
            "cancel" => DeadlineStatus::Cancelled,
            other => unreachable!("{other}"),
        };

        for first in 0..3 {
            for second in 0..3 {
                for third in 0..3 {
                    if first == second || second == third || first == third {
                        continue;
                    }
                    let deadline = AcknowledgementDeadline::new(LaneId(1), 0);
                    assert_eq!(deadline.status(), DeadlineStatus::Pending);

                    let order = [claims[first], claims[second], claims[third]];
                    let mut winners = Vec::new();
                    for (name, claim) in order {
                        if claim(&deadline) {
                            winners.push(name);
                        }
                    }
                    assert_eq!(
                        winners,
                        vec![order[0].0],
                        "only the first claim may win, order {:?}",
                        order.map(|c| c.0)
                    );
                    assert_eq!(deadline.status(), expected_status(order[0].0));

                    // Repeating every claim changes nothing.
                    for (_, claim) in order {
                        assert!(!claim(&deadline), "a resolved deadline never re-resolves");
                    }
                    assert_eq!(deadline.status(), expected_status(order[0].0));
                }
            }
        }
    }

    #[test]
    fn a_concurrent_acknowledge_and_expire_produce_exactly_one_winner() {
        // Run the genuine two-thread race many times. The assertion is not "who wins" (that is
        // legitimately nondeterministic) but the invariant: exactly one winner, and the status
        // agrees with it.
        for _ in 0..200 {
            let deadline = AcknowledgementDeadline::new(LaneId(7), 3);
            let ack_thread = {
                let deadline = Arc::clone(&deadline);
                std::thread::spawn(move || deadline.acknowledge())
            };
            let expire_thread = {
                let deadline = Arc::clone(&deadline);
                std::thread::spawn(move || deadline.expire())
            };
            let acked = ack_thread.join().expect("ack thread");
            let expired = expire_thread.join().expect("expire thread");
            assert_ne!(acked, expired, "exactly one of the two claims must win");
            assert_eq!(
                deadline.status(),
                if acked {
                    DeadlineStatus::Acknowledged
                } else {
                    DeadlineStatus::Expired
                }
            );
        }
    }

    #[test]
    fn an_acknowledged_deadline_is_never_expired_by_the_clock() {
        let sink = Arc::new(RecordingSink::default());
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");
        let deadline = AcknowledgementDeadline::new(LaneId(1), 0);

        // The acknowledgement happens first, exactly as it would on the delivery-report thread.
        assert!(deadline.acknowledge());
        clock.handle().arm(&deadline, Duration::from_millis(1));

        // Arming a resolved deadline is a no-op, so nothing is even queued.
        assert_eq!(clock.armed_len(), 0);
        clock.stop();
        assert!(
            sink.expired.lock().expect("test mutex").is_empty(),
            "an acknowledged heartbeat must never be reported late"
        );
    }

    #[test]
    fn the_clock_expires_a_deadline_that_nobody_claimed() {
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(RecordingSink::default());
        *sink.notify.lock().expect("test mutex") = Some(tx);
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");

        let deadline = AcknowledgementDeadline::new(LaneId(42), 9);
        clock.handle().arm(&deadline, Duration::from_millis(5));

        let lane = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the deadline fires");
        assert_eq!(lane, LaneId(42));
        assert_eq!(deadline.status(), DeadlineStatus::Expired);
        assert_eq!(
            *sink.expired.lock().expect("test mutex"),
            vec![(LaneId(42), 9)]
        );
        clock.stop();
    }

    #[test]
    fn a_moot_deadline_is_left_pending_rather_than_expired() {
        // CaptureKafkaPublisher.java:750 checks the gate before the CAS, so a timer firing after an
        // unrelated failure does not masquerade as the cause.
        let sink = Arc::new(RecordingSink::default());
        sink.moot.store(true, AtomicOrdering::Release);
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");

        let deadline = AcknowledgementDeadline::new(LaneId(3), 0);
        clock.handle().arm(&deadline, Duration::from_millis(2));

        // Give the clock thread room to process the entry, then assert it took no action.
        let deadline_for_wait = Arc::clone(&deadline);
        let drained = wait_until(Duration::from_secs(10), || {
            !deadline_for_wait.is_pending() || clock.armed_len() == 0
        });
        assert!(drained, "the clock processed the entry");
        assert_eq!(
            deadline.status(),
            DeadlineStatus::Pending,
            "a moot deadline stays Pending so a later cancel records the real resolver"
        );
        assert!(sink.expired.lock().expect("test mutex").is_empty());

        assert!(deadline.cancel(), "the publisher resolves it afterwards");
        clock.stop();
    }

    #[test]
    fn the_clock_fires_on_time_while_a_publisher_mailbox_is_saturated() {
        // The Rust analogue of the Java "ack wins despite publisher backlog" test, inverted: the
        // deadline must fire even when the publisher side is wedged. The clock shares no queue,
        // channel, or runtime with the publisher, so a blocked publisher cannot delay it.
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(RecordingSink::default());
        *sink.notify.lock().expect("test mutex") = Some(tx);
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");

        // A stand-in for a wedged single-threaded publisher: a bounded channel nobody drains.
        let (wedge_tx, wedge_rx) = mpsc::sync_channel::<u64>(1);
        let wedged = std::thread::spawn(move || {
            // Fills the channel, then blocks forever on the second send.
            for i in 0..1_000_000 {
                if wedge_tx.send(i).is_err() {
                    return;
                }
            }
        });

        let deadline = AcknowledgementDeadline::new(LaneId(11), 0);
        let armed_at = Instant::now();
        clock.handle().arm(&deadline, Duration::from_millis(20));

        let lane = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the deadline fires despite the wedged publisher");
        let fired_after = armed_at.elapsed();
        assert_eq!(lane, LaneId(11));
        assert!(
            fired_after >= Duration::from_millis(20),
            "the deadline must not fire early: {fired_after:?}"
        );

        drop(wedge_rx);
        drop(wedged.join());
        clock.stop();
    }

    #[test]
    fn deadlines_fire_in_monotonic_order_regardless_of_arming_order() {
        let sink = Arc::new(RecordingSink::default());
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");
        let handle = clock.handle();

        let base = Instant::now() + Duration::from_millis(30);
        // Armed last-first, so a heap bug would surface as the wrong report order.
        let third = AcknowledgementDeadline::new(LaneId(3), 0);
        let first = AcknowledgementDeadline::new(LaneId(1), 0);
        let second = AcknowledgementDeadline::new(LaneId(2), 0);
        handle.arm_at(&third, base + Duration::from_millis(20));
        handle.arm_at(&first, base);
        handle.arm_at(&second, base + Duration::from_millis(10));

        let sink_for_wait = Arc::clone(&sink);
        assert!(
            wait_until(Duration::from_secs(10), || {
                sink_for_wait.expired.lock().expect("test mutex").len() == 3
            }),
            "all three deadlines fire"
        );
        assert_eq!(
            *sink.expired.lock().expect("test mutex"),
            vec![(LaneId(1), 0), (LaneId(2), 0), (LaneId(3), 0)]
        );
        clock.stop();
    }

    #[test]
    fn stopping_the_clock_does_not_expire_pending_deadlines() {
        let sink = Arc::new(RecordingSink::default());
        let mut clock = DeadlineClock::start(Arc::clone(&sink) as Arc<dyn DeadlineSink>)
            .expect("clock thread starts");
        let deadline = AcknowledgementDeadline::new(LaneId(5), 0);
        clock.handle().arm(&deadline, Duration::from_hours(1));
        clock.stop();

        assert_eq!(
            deadline.status(),
            DeadlineStatus::Pending,
            "shutdown is not evidence that an acknowledgement was late"
        );
        assert!(sink.expired.lock().expect("test mutex").is_empty());
    }

    /// Polls `condition` until it holds or `budget` elapses. Used only for liveness assertions,
    /// where some wall-clock wait is unavoidable; never to "give the code time to be correct".
    fn wait_until(budget: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::yield_now();
            std::thread::sleep(Duration::from_millis(1));
        }
        condition()
    }
}
