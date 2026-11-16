//! Per-`(writerNodeId, partition)` publisher lane state (traceability row KAFKA-03).
//!
//! # The second state machine
//!
//! A lane's status is `OPEN -> RETIRING -> RETIRED`. It is **not** the routing status in
//! [`crate::routing_state`], which has five states and carries `DRAINING`. Keeping them separate is
//! load-bearing: a lane stays `OPEN` while its writer status is `DRAINING`, and that is exactly how
//! a superseded generation keeps heartbeating and keeps serving the connections it already owns. A
//! port that merges the two enums silently stops a superseded generation's heartbeats, and the
//! replayer then sees that generation's records expire.
//!
//! # Single owner
//!
//! Every field here is mutated only on the publisher actor thread. Java's `acceptedSends` is a
//! plain `int` incremented on the publisher thread and decremented on whichever thread runs the
//! delivery callback (spec discrepancy D-10: a lost decrement means a lane that never retires, a
//! torn read means a lane that retires with sends outstanding). The actor design removes the race
//! by construction — the delivery callback only sends a message — so the counter needs no
//! synchronisation and the invariant can be asserted instead of hoped for.
//!
//! # The non-overlapping-heartbeat rule
//!
//! At most one heartbeat per lane is ever in flight, and the next `H` timer starts at
//! **acknowledgement-acceptance** time rather than submission time. There is therefore no
//! coalescing queue and never more than one pending heartbeat timer.

use std::sync::Arc;
use std::time::Instant;

use crate::ack_deadline::{AcknowledgementDeadline, LaneId};
use crate::error::{CorruptedCaptureState, CorruptionCheck};
use crate::routing_state::WriterPartition;

/// The publication lifecycle of one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherLaneStatus {
    /// Accepts traffic and publishes heartbeats. Also the status of a lane whose **writer** is
    /// `DRAINING`.
    Open,
    /// Heartbeats have stopped; the lane is waiting for its outstanding sends to settle.
    Retiring,
    /// Finished. No transition leaves this state.
    Retired,
}

/// One publisher lane.
#[derive(Debug)]
pub struct PublisherLane {
    writer_partition: WriterPartition,
    lane_id: LaneId,
    status: PublisherLaneStatus,
    heartbeat_in_flight: bool,
    accepted_sends: u64,
    acknowledgement_deadline: Option<Arc<AcknowledgementDeadline>>,
    deadline_epoch: u64,
    next_heartbeat_at: Option<Instant>,
    heartbeats_submitted: u64,
    heartbeats_accepted: u64,
}

impl PublisherLane {
    /// Opens a lane for `writer_partition`.
    #[must_use]
    pub fn new(writer_partition: WriterPartition, lane_id: LaneId) -> Self {
        Self {
            writer_partition,
            lane_id,
            status: PublisherLaneStatus::Open,
            heartbeat_in_flight: false,
            accepted_sends: 0,
            acknowledgement_deadline: None,
            deadline_epoch: 0,
            next_heartbeat_at: None,
            heartbeats_submitted: 0,
            heartbeats_accepted: 0,
        }
    }

    /// The `(writerNodeId, partition)` this lane publishes for.
    #[must_use]
    pub fn writer_partition(&self) -> &WriterPartition {
        &self.writer_partition
    }

    /// This lane's identifier for the deadline clock.
    #[must_use]
    pub fn lane_id(&self) -> LaneId {
        self.lane_id
    }

    /// The lane's publication status.
    #[must_use]
    pub fn status(&self) -> PublisherLaneStatus {
        self.status
    }

    /// Whether a heartbeat is awaiting acknowledgement.
    #[must_use]
    pub fn heartbeat_in_flight(&self) -> bool {
        self.heartbeat_in_flight
    }

    /// Records accepted by the producer and not yet settled.
    #[must_use]
    pub fn accepted_sends(&self) -> u64 {
        self.accepted_sends
    }

    /// Heartbeats submitted on this lane since it opened.
    #[must_use]
    pub fn heartbeats_submitted(&self) -> u64 {
        self.heartbeats_submitted
    }

    /// Heartbeat acknowledgements accepted on this lane since it opened.
    #[must_use]
    pub fn heartbeats_accepted(&self) -> u64 {
        self.heartbeats_accepted
    }

    /// The deadline covering the in-flight or next heartbeat.
    #[must_use]
    pub fn acknowledgement_deadline(&self) -> Option<&Arc<AcknowledgementDeadline>> {
        self.acknowledgement_deadline.as_ref()
    }

    /// When the next periodic heartbeat is due, if one is scheduled.
    #[must_use]
    pub fn next_heartbeat_at(&self) -> Option<Instant> {
        self.next_heartbeat_at
    }

    /// Schedules the next periodic heartbeat, replacing any existing schedule.
    ///
    /// There is only ever one pending timer per lane, so this is a replace rather than an add.
    pub fn schedule_next_heartbeat(&mut self, at: Instant) {
        self.next_heartbeat_at = Some(at);
    }

    /// Cancels any pending periodic heartbeat.
    pub fn cancel_next_heartbeat(&mut self) {
        self.next_heartbeat_at = None;
    }

    /// Claims the lane for one heartbeat submission.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] when the lane is not `OPEN`, or when a heartbeat is already in
    /// flight. Overlapping heartbeats would make it impossible to say which acknowledgement a
    /// deadline covered, so this is an invariant violation rather than a condition to wait on.
    pub fn begin_heartbeat(&mut self) -> Result<(), CorruptedCaptureState> {
        if self.status != PublisherLaneStatus::Open || self.heartbeat_in_flight {
            return Err(CorruptedCaptureState::new(
                CorruptionCheck::HeartbeatSubmissionNotPermitted,
                format!(
                    "{} status={:?} heartbeatInFlight={}",
                    self.writer_partition, self.status, self.heartbeat_in_flight
                ),
            ));
        }
        self.heartbeat_in_flight = true;
        self.heartbeats_submitted += 1;
        Ok(())
    }

    /// Clears the in-flight heartbeat after its acknowledgement was accepted.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] when no heartbeat was in flight.
    pub fn accept_heartbeat_acknowledgement(&mut self) -> Result<(), CorruptedCaptureState> {
        if !self.heartbeat_in_flight {
            return Err(CorruptedCaptureState::new(
                CorruptionCheck::HeartbeatAcknowledgementWithoutInFlight,
                self.writer_partition.to_string(),
            ));
        }
        self.heartbeat_in_flight = false;
        self.heartbeats_accepted += 1;
        Ok(())
    }

    /// Clears the in-flight heartbeat without accepting it, after a failure or a lost race.
    pub fn abandon_in_flight_heartbeat(&mut self) {
        self.heartbeat_in_flight = false;
    }

    /// Installs a fresh deadline for this lane and returns it.
    ///
    /// Any previous deadline is cancelled, so a replaced deadline can never later expire and
    /// attribute its expiry to the new heartbeat.
    pub fn replace_acknowledgement_deadline(&mut self) -> Arc<AcknowledgementDeadline> {
        if let Some(previous) = self.acknowledgement_deadline.take() {
            previous.cancel();
        }
        self.deadline_epoch += 1;
        let deadline = AcknowledgementDeadline::new(self.lane_id, self.deadline_epoch);
        self.acknowledgement_deadline = Some(Arc::clone(&deadline));
        deadline
    }

    /// Requires the lane to already have a deadline, for a periodic heartbeat.
    ///
    /// A periodic heartbeat reuses the deadline created when the previous acknowledgement was
    /// accepted, whose timer was armed at **acceptance** time. So heartbeat *n+1*'s deadline is
    /// `E` after heartbeat *n*'s acceptance, not `E` after *n+1*'s submission.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] when the lane has no deadline. An active lane without one has no
    /// way to notice a missing acknowledgement, which is the one thing it must never lose.
    pub fn require_acknowledgement_deadline(
        &self,
    ) -> Result<Arc<AcknowledgementDeadline>, CorruptedCaptureState> {
        self.acknowledgement_deadline.clone().ok_or_else(|| {
            CorruptedCaptureState::new(
                CorruptionCheck::ActiveHeartbeatLaneHasNoDeadline,
                self.writer_partition.to_string(),
            )
        })
    }

    /// Cancels and forgets this lane's deadline, if it has one.
    ///
    /// Returns the cancelled deadline so the caller can resolve a pending heartbeat result.
    pub fn cancel_acknowledgement_deadline(&mut self) -> Option<Arc<AcknowledgementDeadline>> {
        let deadline = self.acknowledgement_deadline.take()?;
        deadline.cancel();
        Some(deadline)
    }

    /// Counts one record accepted by the producer.
    pub fn add_in_flight(&mut self) {
        self.accepted_sends += 1;
    }

    /// Counts one record settled.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] on underflow. Java's unsynchronised counter can lose a decrement
    /// and never notice; here the only way to underflow is a double settlement, which means the
    /// in-flight bookkeeping that retirement depends on is wrong.
    pub fn remove_in_flight(&mut self) -> Result<(), CorruptedCaptureState> {
        self.accepted_sends = self.accepted_sends.checked_sub(1).ok_or_else(|| {
            CorruptedCaptureState::new(
                CorruptionCheck::AcceptedSendCountNegative,
                self.writer_partition.to_string(),
            )
        })?;
        Ok(())
    }

    /// Moves the lane to `RETIRING`, stopping heartbeats.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] when the lane is not `OPEN`: the routing state selected it for
    /// retirement, so a lane in any other status means the two machines disagree.
    pub fn begin_retiring(&mut self) -> Result<(), CorruptedCaptureState> {
        if self.status != PublisherLaneStatus::Open {
            return Err(CorruptedCaptureState::new(
                CorruptionCheck::PublisherLaneNotOpenAtRetirement,
                format!("{} status={:?}", self.writer_partition, self.status),
            ));
        }
        self.status = PublisherLaneStatus::Retiring;
        self.next_heartbeat_at = None;
        Ok(())
    }

    /// Whether the lane may complete retirement.
    ///
    /// All four conditions must hold. In particular an in-flight heartbeat keeps its `E` deadline
    /// armed through the `RETIRING` transition, so a heartbeat that goes missing during retirement
    /// still compromises capture.
    #[must_use]
    pub fn is_ready_to_retire(&self, writer_partition_has_connections: bool) -> bool {
        self.status == PublisherLaneStatus::Retiring
            && self.accepted_sends == 0
            && !self.heartbeat_in_flight
            && !writer_partition_has_connections
    }

    /// Marks the lane retired.
    ///
    /// # Errors
    ///
    /// [`CorruptedCaptureState`] when the lane was not `RETIRING`.
    pub fn complete_retirement(&mut self) -> Result<(), CorruptedCaptureState> {
        if self.status != PublisherLaneStatus::Retiring {
            return Err(CorruptedCaptureState::new(
                CorruptionCheck::WriterPartitionCannotCompleteRetirement,
                format!("{} status={:?}", self.writer_partition, self.status),
            ));
        }
        self.status = PublisherLaneStatus::Retired;
        self.acknowledgement_deadline = None;
        self.next_heartbeat_at = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ack_deadline::DeadlineStatus;

    fn lane() -> PublisherLane {
        PublisherLane::new(
            WriterPartition::new(Arc::from("activation:1"), 2),
            LaneId(1),
        )
    }

    #[test]
    fn a_lane_opens_quiet_and_idle() {
        let lane = lane();
        assert_eq!(lane.status(), PublisherLaneStatus::Open);
        assert!(!lane.heartbeat_in_flight());
        assert_eq!(lane.accepted_sends(), 0);
        assert!(lane.acknowledgement_deadline().is_none());
        assert!(lane.next_heartbeat_at().is_none());
    }

    #[test]
    fn two_overlapping_heartbeats_are_corruption() {
        let mut lane = lane();
        lane.begin_heartbeat().expect("the first heartbeat");
        let error = lane
            .begin_heartbeat()
            .expect_err("a second heartbeat while one is in flight");
        assert_eq!(
            error.check(),
            CorruptionCheck::HeartbeatSubmissionNotPermitted
        );
        assert_eq!(lane.heartbeats_submitted(), 1);
    }

    #[test]
    fn a_heartbeat_acknowledgement_requires_a_heartbeat_in_flight() {
        let mut lane = lane();
        let error = lane
            .accept_heartbeat_acknowledgement()
            .expect_err("nothing is in flight");
        assert_eq!(
            error.check(),
            CorruptionCheck::HeartbeatAcknowledgementWithoutInFlight
        );

        lane.begin_heartbeat().expect("submit");
        lane.accept_heartbeat_acknowledgement().expect("accept");
        assert!(!lane.heartbeat_in_flight());
        assert_eq!(lane.heartbeats_accepted(), 1);
        lane.begin_heartbeat()
            .expect("the next heartbeat may now be submitted");
    }

    #[test]
    fn replacing_a_deadline_cancels_the_previous_one() {
        // A replaced deadline must never later expire and be attributed to the new heartbeat.
        let mut lane = lane();
        let first = lane.replace_acknowledgement_deadline();
        assert_eq!(first.epoch(), 1);
        let second = lane.replace_acknowledgement_deadline();
        assert_eq!(second.epoch(), 2);
        assert_eq!(first.status(), DeadlineStatus::Cancelled);
        assert_eq!(second.status(), DeadlineStatus::Pending);
        assert!(!first.expire(), "a cancelled deadline cannot expire");
    }

    #[test]
    fn a_periodic_heartbeat_reuses_the_deadline_from_the_previous_acceptance() {
        let mut lane = lane();
        let initial = lane.replace_acknowledgement_deadline();
        assert!(
            Arc::ptr_eq(
                &lane.require_acknowledgement_deadline().expect("present"),
                &initial
            ),
            "the periodic heartbeat must reuse the object armed at acceptance time"
        );
    }

    #[test]
    fn an_active_lane_without_a_deadline_is_corruption() {
        let lane = lane();
        let error = lane
            .require_acknowledgement_deadline()
            .expect_err("no deadline installed");
        assert_eq!(
            error.check(),
            CorruptionCheck::ActiveHeartbeatLaneHasNoDeadline
        );
    }

    #[test]
    fn an_underflowing_in_flight_count_is_corruption_rather_than_a_silent_wrap() {
        let mut lane = lane();
        lane.add_in_flight();
        lane.remove_in_flight().expect("one settlement");
        let error = lane
            .remove_in_flight()
            .expect_err("a second settlement for one send");
        assert_eq!(error.check(), CorruptionCheck::AcceptedSendCountNegative);
    }

    #[test]
    fn retirement_needs_all_four_conditions() {
        let mut lane = lane();
        assert!(
            !lane.is_ready_to_retire(false),
            "an OPEN lane is not retiring"
        );

        lane.add_in_flight();
        lane.begin_heartbeat().expect("heartbeat");
        lane.begin_retiring().expect("open to retiring");
        assert_eq!(lane.status(), PublisherLaneStatus::Retiring);

        assert!(!lane.is_ready_to_retire(false), "a send is outstanding");
        lane.remove_in_flight().expect("settled");
        assert!(
            !lane.is_ready_to_retire(false),
            "a heartbeat is still in flight, and its deadline is still armed"
        );
        lane.accept_heartbeat_acknowledgement().expect("accepted");
        assert!(
            !lane.is_ready_to_retire(true),
            "the writer partition still has connections"
        );
        assert!(lane.is_ready_to_retire(false));

        lane.complete_retirement().expect("all four conditions");
        assert_eq!(lane.status(), PublisherLaneStatus::Retired);
        assert!(lane.acknowledgement_deadline().is_none());
    }

    #[test]
    fn retiring_a_lane_twice_is_corruption() {
        let mut lane = lane();
        lane.begin_retiring().expect("first");
        let error = lane.begin_retiring().expect_err("already retiring");
        assert_eq!(
            error.check(),
            CorruptionCheck::PublisherLaneNotOpenAtRetirement
        );
    }

    #[test]
    fn completing_retirement_from_open_is_corruption() {
        let mut lane = lane();
        let error = lane
            .complete_retirement()
            .expect_err("never began retiring");
        assert_eq!(
            error.check(),
            CorruptionCheck::WriterPartitionCannotCompleteRetirement
        );
    }

    #[test]
    fn beginning_retirement_stops_heartbeats_but_not_an_armed_deadline() {
        // CaptureKafkaPublisher.java:792-794: an in-flight heartbeat keeps its deadline through
        // the RETIRING transition, so a heartbeat lost during retirement still compromises
        // capture.
        let mut lane = lane();
        let deadline = lane.replace_acknowledgement_deadline();
        lane.begin_heartbeat().expect("heartbeat");
        lane.schedule_next_heartbeat(Instant::now());
        lane.begin_retiring().expect("retiring");

        assert!(
            lane.next_heartbeat_at().is_none(),
            "no further heartbeat is scheduled"
        );
        assert_eq!(
            deadline.status(),
            DeadlineStatus::Pending,
            "the in-flight heartbeat's deadline stays armed"
        );
        assert!(lane.heartbeat_in_flight());
    }

    #[test]
    fn a_retiring_lane_refuses_a_new_heartbeat() {
        let mut lane = lane();
        lane.begin_retiring().expect("retiring");
        let error = lane.begin_heartbeat().expect_err("not open");
        assert_eq!(
            error.check(),
            CorruptionCheck::HeartbeatSubmissionNotPermitted
        );
    }
}
