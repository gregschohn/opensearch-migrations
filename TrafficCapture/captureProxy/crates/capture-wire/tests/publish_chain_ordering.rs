//! Ported `OrderedStreamLifecyleManagerTest` plus the ordering obligations of
//! `proxyCaptureProtocol.md` §4.2 (traceability row WIRE-07, treatment *Exact*).

use std::sync::Arc;

use capture_wire::protobuf_size::field;
use capture_wire::publish_chain::{PublishChain, PublishFailure, PublishOutcome};
use capture_wire::record_buffer::{CountingRecycler, EncodedRecord};
use capture_wire::record_encoder::{ObservationBody, ObservationPlan, RecordEncoder, RecordHeader};
use capture_wire::record_sink::{CollectingSink, RecordSink, SinkError};
use capture_wire::{CaptureTimestamp, PublishTicket};

fn make_record(sink: &mut CollectingSink, index: u32) -> EncodedRecord {
    let header = RecordHeader {
        connection_id: Arc::from("conn-1"),
        node_id: Arc::from("node-1"),
        prior_request_boundaries: 0,
        reads_awaiting_end_of_message: false,
    };
    let buffer = sink.create_buffer(128);
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

fn outcomes(chain: &mut PublishChain) -> Vec<(PublishTicket, PublishOutcome)> {
    let mut all = Vec::new();
    while let Some(outcome) = chain.poll_outcome() {
        all.push(outcome);
    }
    all
}

/// O1 — `releasesAQueuedStreamWhenThePreviousPublicationFails`.
///
/// Java asserts: `kickoffCalls == 1`; both futures fail with the **same** exception instance,
/// unwrapped from `CompletionException`; the first holder is **not** closed by the manager because
/// ownership had already passed to kickoff; the queued holder is closed exactly once.
#[test]
fn o1_releases_a_queued_record_when_the_previous_publication_fails() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut sink = CollectingSink::with_recycler(Arc::clone(&recycler) as Arc<_>);
    let first = make_record(&mut sink, 1);
    let queued = make_record(&mut sink, 2);
    assert_eq!(recycler.taken(), 2);
    let mut chain = PublishChain::new();

    let first_ticket = chain.submit(first, &mut sink);
    let queued_ticket = chain.submit(queued, &mut sink);

    // kickoffCalls == 1: record 2 never reached the sink.
    assert_eq!(sink.submit_calls(), 1);
    assert_eq!(chain.queued_len(), 1);

    let failure = PublishFailure::new(SinkError::new("publication failed"));
    chain.fail(failure.clone());

    let reported = outcomes(&mut chain);
    assert_eq!(reported.len(), 2);
    assert_eq!(reported[0].0, first_ticket);
    assert_eq!(reported[1].0, queued_ticket);
    for (ticket, outcome) in &reported {
        match outcome {
            PublishOutcome::Failed(reported_failure) => assert!(
                reported_failure.is_same(&failure),
                "ticket {ticket:?} must report the same failure instance, not a copy"
            ),
            PublishOutcome::Acknowledged => panic!("ticket {ticket:?} should have failed"),
        }
    }

    // first.closeCalls == 0: the sink owns record 1 and still holds it.
    assert_eq!(sink.records().len(), 1);
    // queued.closeCalls == 1: the queued record was released exactly once, without publishing.
    assert_eq!(recycler.released(), 1);
    assert_eq!(recycler.outstanding(), 1);

    // There is no reset: the chain is poisoned forever.
    assert!(chain.is_poisoned());
    let later = make_record(&mut sink, 3);
    let later_ticket = chain.submit(later, &mut sink);
    assert_eq!(
        sink.submit_calls(),
        1,
        "a poisoned chain never reaches the sink again"
    );
    match chain.poll_outcome() {
        Some((ticket, PublishOutcome::Failed(reported_failure))) => {
            assert_eq!(ticket, later_ticket);
            assert!(
                reported_failure.is_same(&failure),
                "a later submission carries the ORIGINAL failure"
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(
        recycler.released(),
        2,
        "the rejected record was released too"
    );
}

/// O2 — `releasesAStreamWhenPublicationKickoffThrows`.
///
/// Java asserts the returned future fails with that exception and the holder is closed exactly once.
#[test]
fn o2_releases_a_record_when_submission_fails() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut sink = CollectingSink::with_recycler(Arc::clone(&recycler) as Arc<_>);
    let record = make_record(&mut sink, 1);
    sink.fail_next_submit(SinkError::new("kickoff failed"));
    let mut chain = PublishChain::new();

    let ticket = chain.submit(record, &mut sink);

    assert_eq!(sink.submit_calls(), 1);
    match chain.poll_outcome() {
        Some((reported, PublishOutcome::Failed(failure))) => {
            assert_eq!(reported, ticket);
            assert_eq!(failure.error().message(), "kickoff failed");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // Closed exactly once: ownership transferred to the sink, which dropped it.
    assert_eq!(recycler.taken(), 1);
    assert_eq!(recycler.released(), 1);
    assert!(chain.is_poisoned());
    assert!(chain.in_flight().is_none());
}

/// `proxyCaptureProtocol.md` §4.1: record N+1 is not passed to the producer until Kafka
/// acknowledges record N.
#[test]
fn records_reach_the_sink_strictly_one_acknowledgement_at_a_time() {
    let mut sink = CollectingSink::new();
    let records: Vec<_> = (1..=4).map(|index| make_record(&mut sink, index)).collect();
    let mut chain = PublishChain::new();

    let mut tickets = Vec::new();
    for record in records {
        tickets.push(chain.submit(record, &mut sink));
    }
    assert_eq!(sink.submit_calls(), 1, "only record 1 may be in flight");
    assert_eq!(chain.queued_len(), 3);

    for (position, ticket) in tickets.iter().enumerate() {
        assert_eq!(chain.in_flight(), Some(*ticket));
        assert_eq!(chain.acknowledge(&mut sink), Some(*ticket));
        let expected_submissions = (position + 2).min(4);
        assert_eq!(
            sink.submit_calls(),
            expected_submissions,
            "exactly one more record is dispatched per acknowledgement"
        );
    }

    assert_eq!(chain.queued_len(), 0);
    assert!(chain.in_flight().is_none());
    // Every record was published, in submission order.
    let indices: Vec<u32> = sink.records().iter().map(EncodedRecord::index).collect();
    assert_eq!(indices, vec![1, 2, 3, 4]);
    let reported = outcomes(&mut chain);
    assert_eq!(reported.len(), 4);
    assert!(
        reported
            .iter()
            .all(|(_, o)| matches!(o, PublishOutcome::Acknowledged))
    );
}

/// A failure in the middle of a chain poisons only the remainder, and the already-acknowledged
/// prefix stays published.
#[test]
fn a_mid_chain_failure_poisons_the_remainder_only() {
    let mut sink = CollectingSink::new();
    let records: Vec<_> = (1..=3).map(|index| make_record(&mut sink, index)).collect();
    let mut chain = PublishChain::new();
    let tickets: Vec<_> = records
        .into_iter()
        .map(|record| chain.submit(record, &mut sink))
        .collect();

    chain.acknowledge(&mut sink);
    assert_eq!(sink.submit_calls(), 2);

    let failure = PublishFailure::new(SinkError::new("ambiguous outcome"));
    chain.fail(failure.clone());

    let reported = outcomes(&mut chain);
    assert!(matches!(reported[0], (t, PublishOutcome::Acknowledged) if t == tickets[0]));
    for entry in &reported[1..] {
        match entry {
            (_, PublishOutcome::Failed(reported_failure)) => {
                assert!(reported_failure.is_same(&failure));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    assert_eq!(reported.len(), 3);
    assert_eq!(chain.queued_len(), 0);
}

/// A sink failure on a *queued* record's dispatch also poisons the chain and releases everything
/// behind it.
#[test]
fn a_failure_dispatching_a_queued_record_poisons_the_chain() {
    let recycler = Arc::new(CountingRecycler::new());
    let mut sink = CollectingSink::with_recycler(Arc::clone(&recycler) as Arc<_>);
    let records: Vec<_> = (1..=3).map(|index| make_record(&mut sink, index)).collect();
    let mut chain = PublishChain::new();
    for record in records {
        chain.submit(record, &mut sink);
    }
    while chain.poll_outcome().is_some() {}

    sink.fail_next_submit(SinkError::new("dispatch of record 2 failed"));
    chain.acknowledge(&mut sink);

    assert!(chain.is_poisoned());
    assert_eq!(chain.queued_len(), 0);
    let reported = outcomes(&mut chain);
    // Record 1 acknowledged, record 2 failed on dispatch, record 3 rejected by the poison.
    assert_eq!(reported.len(), 3);
    assert!(matches!(reported[0].1, PublishOutcome::Acknowledged));
    let PublishOutcome::Failed(second) = &reported[1].1 else {
        panic!("record 2 must fail")
    };
    let PublishOutcome::Failed(third) = &reported[2].1 else {
        panic!("record 3 must fail")
    };
    assert!(
        second.is_same(third),
        "both must carry the same latched failure"
    );
    // Record 2 released by the sink, record 3 released by the chain; record 1 is still held.
    assert_eq!(recycler.released(), 2);
}
