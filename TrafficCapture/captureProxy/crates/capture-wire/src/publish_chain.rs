//! Per-connection ordered publication chain (traceability row WIRE-07, treatment *Exact*).
//!
//! # The invariant
//!
//! Record N+1 for a connection is not handed to the sink until record N is **acknowledged**
//! (`proxyCaptureProtocol.md` §4.1, §4.2; `OrderedStreamLifecyleManager`:7-35). The acknowledgement
//! wait is deliberately stronger than producer submission ordering or producer idempotence: if N
//! fails or is ambiguous after N+1 was already submitted, Kafka may append N+1 without N and the
//! replayer then sees a permanent `connectionObservationSequence` gap, which it treats as a protocol
//! violation on every restart.
//!
//! # Poisoning
//!
//! Once any link fails, the chain stays failed forever. Every later submission is rejected
//! immediately, carrying the **original** failure, and its record is released without publishing.
//! There is no reset. Java achieves this because `futureForLastClose` remains a failed future;
//! here the failure is latched explicitly.
//!
//! # Mapping from Java futures
//!
//! Java returns a `CompletableFuture` per record and routes its completion into the
//! capture-failure path (`StreamChannelConnectionCaptureSerializer.observeDetachedRecord`), with the
//! explicit contract that correctness must not depend on a caller awaiting it. This crate reports
//! the same information synchronously: [`PublishChain::submit`] hands back a [`PublishTicket`], and
//! every completion is queued as a [`PublishOutcome`] for [`PublishChain::poll_outcome`]. The owner
//! drains outcomes and routes failures; nothing in the capture path blocks on them.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::record_buffer::EncodedRecord;
use crate::record_sink::{RecordSink, SinkError};

/// A cheap, cloneable handle to one publication failure.
///
/// Clones share one underlying error, so [`PublishFailure::is_same`] answers the question the Java
/// `OrderedStreamLifecyleManagerTest` asks with `assertSame`: every poisoned link reports the *same*
/// failure instance, not a copy or a wrapper.
#[derive(Debug, Clone)]
pub struct PublishFailure(Arc<SinkError>);

impl PublishFailure {
    /// Latches `error` as a publication failure.
    #[must_use]
    pub fn new(error: SinkError) -> Self {
        Self(Arc::new(error))
    }

    /// Whether both handles refer to the identical failure.
    #[must_use]
    pub fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The underlying sink error.
    #[must_use]
    pub fn error(&self) -> &SinkError {
        &self.0
    }
}

impl fmt::Display for PublishFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for PublishFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

/// Identifies one submission to the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublishTicket(u64);

impl PublishTicket {
    /// The ticket's monotonic ordinal, for diagnostics.
    #[must_use]
    pub fn ordinal(self) -> u64 {
        self.0
    }
}

/// How one submission ended.
#[derive(Debug, Clone)]
pub enum PublishOutcome {
    /// The sink reported the record durably published.
    Acknowledged,
    /// The record was never published, or its publication failed or was ambiguous.
    Failed(PublishFailure),
}

/// The connection-local acknowledgement chain.
///
/// At most one record is in flight. Later records queue behind it in submission order; queue age,
/// count, and bytes are observable but create no additional capture-compromise rule
/// (`proxyCaptureProtocol.md` §4.2).
#[derive(Debug)]
pub struct PublishChain {
    in_flight: Option<PublishTicket>,
    queue: VecDeque<(PublishTicket, EncodedRecord)>,
    poison: Option<PublishFailure>,
    next_ticket: u64,
    outcomes: VecDeque<(PublishTicket, PublishOutcome)>,
}

impl Default for PublishChain {
    fn default() -> Self {
        Self::new()
    }
}

impl PublishChain {
    /// An empty, unpoisoned chain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_flight: None,
            queue: VecDeque::new(),
            poison: None,
            next_ticket: 1,
            outcomes: VecDeque::new(),
        }
    }

    /// Hands a detached record to the chain.
    ///
    /// Dispatches it to `sink` immediately if nothing is in flight; otherwise queues it. On a
    /// poisoned chain the record is released without publishing and an immediate
    /// [`PublishOutcome::Failed`] carrying the original failure is queued.
    pub fn submit<S: RecordSink + ?Sized>(
        &mut self,
        record: EncodedRecord,
        sink: &mut S,
    ) -> PublishTicket {
        let ticket = PublishTicket(self.next_ticket);
        self.next_ticket += 1;

        if let Some(failure) = self.poison.clone() {
            // Rejection path: dropping the record releases its buffer exactly once.
            drop(record);
            self.outcomes
                .push_back((ticket, PublishOutcome::Failed(failure)));
            return ticket;
        }
        if self.in_flight.is_some() {
            self.queue.push_back((ticket, record));
            return ticket;
        }
        self.dispatch(ticket, record, sink);
        ticket
    }

    fn dispatch<S: RecordSink + ?Sized>(
        &mut self,
        ticket: PublishTicket,
        record: EncodedRecord,
        sink: &mut S,
    ) {
        match sink.submit(record) {
            Ok(()) => self.in_flight = Some(ticket),
            Err(error) => {
                // The sink already consumed and released the record.
                let failure = PublishFailure::new(error);
                self.outcomes
                    .push_back((ticket, PublishOutcome::Failed(failure.clone())));
                self.poison_with(failure);
            }
        }
    }

    /// Reports that the in-flight record was acknowledged and dispatches the next queued record.
    ///
    /// Returns the acknowledged ticket, or `None` if nothing was in flight.
    pub fn acknowledge<S: RecordSink + ?Sized>(&mut self, sink: &mut S) -> Option<PublishTicket> {
        let ticket = self.in_flight.take()?;
        self.outcomes
            .push_back((ticket, PublishOutcome::Acknowledged));
        if let Some((next_ticket, next_record)) = self.queue.pop_front() {
            self.dispatch(next_ticket, next_record, sink);
        }
        Some(ticket)
    }

    /// Reports that the in-flight record failed, or that its outcome is ambiguous.
    ///
    /// Poisons the chain: every queued record is released without publishing and reports the same
    /// failure instance, and every later submission does too.
    pub fn fail(&mut self, failure: PublishFailure) {
        if let Some(ticket) = self.in_flight.take() {
            self.outcomes
                .push_back((ticket, PublishOutcome::Failed(failure.clone())));
        }
        self.poison_with(failure);
    }

    fn poison_with(&mut self, failure: PublishFailure) {
        if self.poison.is_none() {
            self.poison = Some(failure.clone());
        }
        let latched = self.poison.clone().unwrap_or(failure);
        self.in_flight = None;
        while let Some((ticket, record)) = self.queue.pop_front() {
            drop(record);
            self.outcomes
                .push_back((ticket, PublishOutcome::Failed(latched.clone())));
        }
    }

    /// Takes the oldest completed outcome, if any.
    pub fn poll_outcome(&mut self) -> Option<(PublishTicket, PublishOutcome)> {
        self.outcomes.pop_front()
    }

    /// The record currently awaiting acknowledgement.
    #[must_use]
    pub fn in_flight(&self) -> Option<PublishTicket> {
        self.in_flight
    }

    /// How many records are waiting behind the in-flight record.
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// The latched failure, once the chain has been poisoned.
    #[must_use]
    pub fn failure(&self) -> Option<&PublishFailure> {
        self.poison.as_ref()
    }

    /// Whether the chain has been poisoned and will never publish again.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poison.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::protobuf_size::{CaptureTimestamp, field};
    use crate::record_buffer::CountingRecycler;
    use crate::record_encoder::{ObservationBody, ObservationPlan, RecordEncoder, RecordHeader};
    use crate::record_sink::CollectingSink;

    fn record(sink: &mut CollectingSink, index: u32) -> EncodedRecord {
        let header = RecordHeader {
            connection_id: Arc::from("conn"),
            node_id: Arc::from("node"),
            prior_request_boundaries: 0,
            reads_awaiting_end_of_message: false,
        };
        let buffer = sink.create_buffer(64);
        let mut encoder = RecordEncoder::start(buffer, header, index, None);
        let body = ObservationBody::Empty;
        let plan = ObservationPlan::new(
            u64::from(index),
            CaptureTimestamp::EPOCH,
            field::REQUEST_FORWARDED,
            &body,
        );
        encoder.write_observation(&plan, &body);
        encoder.finish(false).expect("non-final record")
    }

    #[test]
    fn record_two_waits_for_record_one_to_be_acknowledged() {
        let recycler = Arc::new(CountingRecycler::new());
        let mut sink = CollectingSink::with_recycler(Arc::clone(&recycler) as Arc<_>);
        let first = record(&mut sink, 1);
        let second = record(&mut sink, 2);
        let mut chain = PublishChain::new();

        let t1 = chain.submit(first, &mut sink);
        assert_eq!(t1.ordinal(), 1);
        assert_eq!(sink.submit_calls(), 1);
        let t2 = chain.submit(second, &mut sink);
        assert_eq!(
            sink.submit_calls(),
            1,
            "record 2 must not reach the sink before record 1 is acked"
        );
        assert_eq!(chain.queued_len(), 1);

        assert_eq!(chain.acknowledge(&mut sink), Some(t1));
        assert_eq!(
            sink.submit_calls(),
            2,
            "record 2 is dispatched on record 1's acknowledgement"
        );
        assert_eq!(chain.in_flight(), Some(t2));
        assert!(matches!(chain.poll_outcome(), Some((t, PublishOutcome::Acknowledged)) if t == t1));
    }
}
