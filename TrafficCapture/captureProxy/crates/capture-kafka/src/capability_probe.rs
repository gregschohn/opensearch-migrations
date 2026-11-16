//! The startup capability probe: proving the traffic topic assigns broker time
//! (traceability row KAFKA-01).
//!
//! # What is being proved, and why it must be proved before the listener opens
//!
//! Every freshness and continuity decision in this crate compares two broker `LogAppendTime`
//! values. If the topic is configured `CreateTime`, the broker stores the **producer's own clock**
//! verbatim, so those comparisons become the proxy comparing its clock against itself and calling
//! the result a broker-time proof. Capture-before-forward would then release source mutations on
//! evidence that does not exist. So the topic's timestamp behaviour is qualified against the real
//! brokers, once, before any traffic can arrive.
//!
//! This is not theoretical: **MSK Serverless defaults topics to `CreateTime`**. Proved by
//! controlled experiment on a live cluster — same client, same cluster, only the topic
//! configuration differing — a `timestamp=1000` produce came back as `CreateTime(1000)` on the
//! default topic, the bogus 1970 value stored verbatim, and as `LogAppendTime(1794790930807)` on
//! the configured one. MSK Serverless also reports
//! `message.timestamp.difference.max.ms = i64::MAX`, so the broker will not reject a wildly skewed
//! producer timestamp either. `message.timestamp.type=LogAppendTime` **is** an accepted topic
//! override there, so this is opt-in rather than unavailable.
//!
//! # Owner decision OD-4: the timestamp **type**, not a positive value
//!
//! The documented check is "produce with a timestamp of zero and accept only a positive returned
//! timestamp", inferring that a positive value must have been assigned by the broker. That check is
//! **impossible** through librdkafka and, worse, silently passes:
//!
//! * `rust-rdkafka` sends `record.timestamp.unwrap_or(0)`, so `Some(0)` and `None` are
//!   indistinguishable; and
//! * librdkafka substitutes the client's own wall clock for a zero timestamp.
//!
//! Observed, not inferred: a zero-timestamp produce to a `CreateTime` topic returned
//! `CreateTime(1794790929566)` — positive, and byte-identical to the client's own pre-produce wall
//! clock. The documented test would have passed a topic it exists to fail.
//!
//! So the probe produces with an ordinary timestamp and accepts an acknowledgement **only** when
//! the delivery report reports `Timestamp::LogAppendTime(t)` with `t > 0`. That reads the broker's
//! own assertion about where the timestamp came from instead of inferring it, and it is strictly
//! stronger than the zero trick. The partition is checked too, which Java's probe never did.
//!
//! # Owner decision OD-5: a non-`LogAppendTime` topic refuses startup
//!
//! Not a warning, not a degraded mode: the process exits with a distinct capture-configuration
//! failure **before the listener opens**. A `CreateTime` topic makes capture-before-forward
//! validation and replayer expiration unsound, so refusing to start is safer than capturing. The
//! proxy does not attempt to change the topic configuration (OD-7).
//!
//! # One probe per distinct current leader
//!
//! What is being qualified is a *broker's* behaviour, so the probe set is one partition per distinct
//! current leader — the lowest-indexed partition each leader leads, in leader-id order. The on-wire
//! record is unchanged from Java, including `writerNodeId = captureActivationId + ":PROBE"`.

use std::collections::BTreeMap;

use rdkafka::Timestamp;

use crate::error::StartupRefusal;
use crate::publisher_actor::{DeliveryNotice, OutboundRecord, ProduceError, RecordProducer};
use crate::records::{ProbeRecord, probe_writer_node_id};

/// How the probe set progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeProgress {
    /// This acknowledgement was accepted; more are outstanding.
    Accepted,
    /// Every representative partition has now been qualified.
    Complete,
    /// The delivery report was for a record this probe did not submit.
    Unknown,
}

/// The startup capability probe over one topic's representative partitions.
#[derive(Debug)]
pub struct CapabilityProbe {
    topic: String,
    writer_node_id: String,
    outstanding: BTreeMap<u64, ProbeRecord>,
    accepted: Vec<i32>,
}

impl CapabilityProbe {
    /// Builds the probe set for `representative_partitions`.
    ///
    /// `probe_ids` supplies one unique id per partition, in order; production passes fresh UUIDs and
    /// tests pass fixed strings.
    ///
    /// # Errors
    ///
    /// All of these are caught **before any record is produced**, because a malformed probe set
    /// would otherwise publish records that can never be qualified:
    ///
    /// * a blank `capture_activation_id` — every writer identity derives from it;
    /// * an empty, duplicated, or negative partition list;
    /// * a blank probe id, or the wrong number of probe ids.
    pub fn new(
        topic: impl Into<String>,
        capture_activation_id: &str,
        representative_partitions: &[i32],
        probe_ids: &[String],
    ) -> Result<Self, StartupRefusal> {
        let refuse = |detail: String| StartupRefusal::CapabilityProbe(detail);
        if capture_activation_id.trim().is_empty() {
            return Err(refuse("captureActivationId must not be blank".to_owned()));
        }
        if representative_partitions.is_empty() {
            return Err(refuse(
                "at least one representative partition is required".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for partition in representative_partitions {
            if *partition < 0 {
                return Err(refuse(format!(
                    "representative partitions must not be negative, but {partition} is"
                )));
            }
            if !seen.insert(*partition) {
                return Err(refuse(format!(
                    "representative partitions must be unique, but {partition} appears twice"
                )));
            }
        }
        if probe_ids.len() != representative_partitions.len() {
            return Err(refuse(format!(
                "{} probe ids were supplied for {} representative partitions",
                probe_ids.len(),
                representative_partitions.len()
            )));
        }

        let mut outstanding = BTreeMap::new();
        for (index, (partition, probe_id)) in representative_partitions
            .iter()
            .zip(probe_ids.iter())
            .enumerate()
        {
            if probe_id.trim().is_empty() {
                return Err(refuse("probeId must not be blank".to_owned()));
            }
            let send_id = index as u64 + 1;
            outstanding.insert(
                send_id,
                ProbeRecord::build(capture_activation_id, probe_id.clone(), *partition),
            );
        }

        Ok(Self {
            topic: topic.into(),
            writer_node_id: probe_writer_node_id(capture_activation_id),
            outstanding,
            accepted: Vec::new(),
        })
    }

    /// The probe identity, `captureActivationId:PROBE`.
    #[must_use]
    pub fn writer_node_id(&self) -> &str {
        &self.writer_node_id
    }

    /// The partitions still awaiting qualification, ascending by submission order.
    #[must_use]
    pub fn outstanding_partitions(&self) -> Vec<i32> {
        self.outstanding
            .values()
            .map(ProbeRecord::partition)
            .collect()
    }

    /// The partitions qualified so far, in acceptance order.
    #[must_use]
    pub fn accepted_partitions(&self) -> &[i32] {
        &self.accepted
    }

    /// Whether every representative partition has been qualified.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// The records this probe will produce, in submission order.
    #[must_use]
    pub fn records(&self) -> Vec<OutboundRecord> {
        self.outstanding
            .iter()
            .map(|(send_id, record)| OutboundRecord {
                topic: self.topic.clone(),
                partition: record.partition(),
                key: record.key().to_owned(),
                // No producer timestamp is set: see OD-4 on the module.
                value: record.value().to_vec(),
                send_id: *send_id,
                // A probe is a control record, but it is published before the publication budget
                // exists: startup qualification happens before the publisher is built.
                budget_class: crate::budget::BudgetClass::Control,
                bytes: record.value().len() as u64,
                deadline: None,
            })
            .collect()
    }

    /// Submits every probe.
    ///
    /// The loop stops at the first synchronous failure, so one broker refusing a produce suppresses
    /// the rest: there is nothing to learn from probing further once qualification has failed.
    ///
    /// # Errors
    ///
    /// [`StartupRefusal::CapabilityProbe`] when the producer refused a record.
    pub fn submit(&self, producer: &dyn RecordProducer) -> Result<(), StartupRefusal> {
        for record in self.records() {
            let partition = record.partition;
            producer.enqueue(record).map_err(|error| {
                StartupRefusal::CapabilityProbe(match error {
                    ProduceError::QueueFull => format!(
                        "the producer queue was full while probing partition {partition}; the \
                         capture topic could not be qualified"
                    ),
                    ProduceError::Fatal(detail) => format!(
                        "the producer refused the capability probe for partition {partition}: \
                         {detail}"
                    ),
                })
            })?;
        }
        Ok(())
    }

    /// Validates one probe's delivery report.
    ///
    /// # Errors
    ///
    /// * [`StartupRefusal::CapabilityProbe`] when the produce failed, or when Kafka appended to a
    ///   different partition than requested.
    /// * [`StartupRefusal::TopicIsNotLogAppendTime`] when the report does not carry
    ///   `Timestamp::LogAppendTime` with a positive value — owner decisions OD-4 and OD-5.
    pub fn accept(&mut self, notice: &DeliveryNotice) -> Result<ProbeProgress, StartupRefusal> {
        let Some(record) = self.outstanding.get(&notice.send_id) else {
            return Ok(ProbeProgress::Unknown);
        };
        let expected_partition = record.partition();

        if let Some(detail) = &notice.error {
            return Err(StartupRefusal::CapabilityProbe(format!(
                "the capability probe for {}/{expected_partition} failed: {detail}",
                self.topic
            )));
        }

        // Java's probe never checks this; the critical-mutation path does. Checking it here costs
        // nothing and means a probe can never qualify a partition it did not reach.
        if let Some(actual) = notice.partition
            && actual != expected_partition
        {
            return Err(StartupRefusal::CapabilityProbe(format!(
                "the capability probe for {}/{expected_partition} was acknowledged on partition \
                 {actual}",
                self.topic
            )));
        }

        match notice.timestamp {
            Timestamp::LogAppendTime(time) if time > 0 => {
                self.outstanding.remove(&notice.send_id);
                self.accepted.push(expected_partition);
                if self.is_complete() {
                    Ok(ProbeProgress::Complete)
                } else {
                    Ok(ProbeProgress::Accepted)
                }
            }
            other => Err(StartupRefusal::TopicIsNotLogAppendTime {
                topic: self.topic.clone(),
                partition: expected_partition,
                observed: describe_timestamp(other),
            }),
        }
    }
}

/// Renders a timestamp for a refusal message, keeping its **type** visible.
fn describe_timestamp(timestamp: Timestamp) -> String {
    match timestamp {
        Timestamp::NotAvailable => "no timestamp".to_owned(),
        Timestamp::CreateTime(time) => format!("CreateTime({time})"),
        Timestamp::LogAppendTime(time) => format!("LogAppendTime({time})"),
    }
}

/// Generates one fresh probe id per partition.
#[must_use]
pub fn fresh_probe_ids(count: usize) -> Vec<String> {
    (0..count)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct CollectingProducer {
        records: Mutex<Vec<OutboundRecord>>,
        fail_from: Mutex<Option<usize>>,
    }

    impl RecordProducer for CollectingProducer {
        fn enqueue(&self, record: OutboundRecord) -> Result<(), ProduceError> {
            let mut records = self.records.lock().expect("test mutex");
            if let Some(fail_from) = *self.fail_from.lock().expect("test mutex")
                && records.len() >= fail_from
            {
                return Err(ProduceError::Fatal("broker refused".to_owned()));
            }
            records.push(record);
            Ok(())
        }

        fn flush(&self, _timeout: std::time::Duration) -> Result<(), String> {
            Ok(())
        }
    }

    fn notice(send_id: u64, partition: i32, timestamp: Timestamp) -> DeliveryNotice {
        DeliveryNotice {
            send_id,
            partition: Some(partition),
            timestamp,
            error: None,
            deadline_claimed: None,
            deadline_status: None,
        }
    }

    fn probe(partitions: &[i32]) -> CapabilityProbe {
        let ids: Vec<String> = (0..partitions.len())
            .map(|index| format!("probe-{index}"))
            .collect();
        CapabilityProbe::new("traffic", "activation", partitions, &ids).expect("valid probe set")
    }

    #[test]
    fn one_probe_per_representative_partition_in_order() {
        // CaptureKafkaCapabilityProbeTest.java:30: representatives [1, 3], probe identity
        // activation:PROBE, key activation:PROBE:<probeId>.
        let probe = probe(&[1, 3]);
        assert_eq!(probe.writer_node_id(), "activation:PROBE");
        assert_eq!(probe.outstanding_partitions(), vec![1, 3]);

        let records = probe.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].partition, 1);
        assert_eq!(records[0].key, "activation:PROBE:probe-0");
        assert_eq!(records[1].partition, 3);
        assert_eq!(records[1].key, "activation:PROBE:probe-1");
        for record in &records {
            assert_eq!(record.topic, "traffic");
            assert!(!record.value.is_empty());
        }
    }

    #[test]
    fn a_log_append_time_topic_is_qualified() {
        let mut probe = probe(&[0, 1]);
        assert_eq!(
            probe
                .accept(&notice(1, 0, Timestamp::LogAppendTime(1_794_790_930_717)))
                .expect("accepted"),
            ProbeProgress::Accepted
        );
        assert!(!probe.is_complete());
        assert_eq!(
            probe
                .accept(&notice(2, 1, Timestamp::LogAppendTime(1_794_790_930_807)))
                .expect("accepted"),
            ProbeProgress::Complete
        );
        assert!(probe.is_complete());
        assert_eq!(probe.accepted_partitions(), [0, 1]);
    }

    #[test]
    fn a_create_time_topic_refuses_startup_even_with_a_positive_timestamp() {
        // Owner decisions OD-4 and OD-5, and the exact value observed on the live cluster: a
        // zero-timestamp produce to a CreateTime topic returned CreateTime(1794790929566), which is
        // positive and identical to the client's own clock. A "positive timestamp" test would have
        // passed this topic.
        let mut probe = probe(&[0]);
        let error = probe
            .accept(&notice(1, 0, Timestamp::CreateTime(1_794_790_929_566)))
            .expect_err("a positive CreateTime proves nothing");
        match &error {
            StartupRefusal::TopicIsNotLogAppendTime {
                topic,
                partition,
                observed,
            } => {
                assert_eq!(topic, "traffic");
                assert_eq!(*partition, 0);
                assert_eq!(observed, "CreateTime(1794790929566)");
            }
            other => panic!("expected a timestamp-type refusal, got {other}"),
        }
        assert!(
            error
                .to_string()
                .contains("message.timestamp.type=LogAppendTime"),
            "the refusal must name the remedy"
        );
        assert!(!probe.is_complete());
    }

    #[test]
    fn a_missing_timestamp_refuses_startup() {
        // CaptureKafkaCapabilityProbeTest.missingLogAppendTimeRejectsStartupQualification (:99).
        let mut probe = probe(&[0]);
        let error = probe
            .accept(&notice(1, 0, Timestamp::NotAvailable))
            .expect_err("no timestamp");
        assert!(matches!(
            error,
            StartupRefusal::TopicIsNotLogAppendTime { .. }
        ));
    }

    #[test]
    fn a_non_positive_log_append_time_refuses_startup() {
        // CaptureKafkaCapabilityProbeTest.zeroLogAppendTimeRejectsStartupQualification (:81).
        for time in [0_i64, -1] {
            let mut probe = probe(&[0]);
            let error = probe
                .accept(&notice(1, 0, Timestamp::LogAppendTime(time)))
                .expect_err("a non-positive broker time is not a proof");
            match &error {
                StartupRefusal::TopicIsNotLogAppendTime { observed, .. } => {
                    assert!(observed.contains(&time.to_string()));
                }
                other => panic!("expected a timestamp refusal, got {other}"),
            }
        }
    }

    #[test]
    fn a_probe_acknowledged_on_the_wrong_partition_refuses_startup() {
        // Stronger than Java, which never checks the acknowledged partition on the probe path.
        let mut probe = probe(&[1]);
        let error = probe
            .accept(&notice(1, 2, Timestamp::LogAppendTime(1_000)))
            .expect_err("partition 2 is not partition 1");
        assert!(error.to_string().contains("acknowledged on partition 2"));
    }

    #[test]
    fn any_failed_probe_refuses_startup() {
        // CaptureKafkaCapabilityProbeTest.anyFailedProbeFailsStartupQualification (:60-79).
        let mut probe = probe(&[0, 1]);
        probe
            .accept(&notice(1, 0, Timestamp::LogAppendTime(1_000)))
            .expect("the first partition qualified");
        let mut failed = notice(2, 1, Timestamp::NotAvailable);
        failed.error = Some("NotLeaderForPartition".to_owned());
        let error = probe.accept(&failed).expect_err("the second probe failed");
        assert!(error.to_string().contains("NotLeaderForPartition"));
        assert!(
            !probe.is_complete(),
            "qualification is all-or-nothing across leaders"
        );
    }

    #[test]
    fn an_unknown_delivery_report_is_ignored_rather_than_mistaken_for_a_probe() {
        let mut probe = probe(&[0]);
        assert_eq!(
            probe
                .accept(&notice(99, 0, Timestamp::LogAppendTime(1_000)))
                .expect("not ours"),
            ProbeProgress::Unknown
        );
        assert!(!probe.is_complete());
    }

    #[test]
    fn submission_hands_every_probe_to_the_producer() {
        let probe = probe(&[0, 2, 5]);
        let producer = CollectingProducer::default();
        probe.submit(&producer).expect("all accepted");
        let records = producer.records.lock().expect("test mutex");
        assert_eq!(
            records.iter().map(|r| r.partition).collect::<Vec<_>>(),
            vec![0, 2, 5]
        );
    }

    #[test]
    fn a_synchronously_failing_produce_suppresses_the_remaining_probes() {
        // CKCP:71-73 breaks the loop early.
        let probe = probe(&[0, 1, 2]);
        let producer = CollectingProducer::default();
        *producer.fail_from.lock().expect("test mutex") = Some(1);
        let error = probe
            .submit(&producer)
            .expect_err("the second produce failed");
        assert!(error.to_string().contains("broker refused"));
        assert_eq!(
            producer.records.lock().expect("test mutex").len(),
            1,
            "there is nothing to learn from probing further once qualification has failed"
        );
    }

    #[test]
    fn a_malformed_probe_set_is_refused_before_anything_is_produced() {
        // CKCP:53-55, :78-92, :103-106. All synchronous, all before the first send.
        assert!(CapabilityProbe::new("traffic", "   ", &[0], &["p".to_owned()]).is_err());
        assert!(CapabilityProbe::new("traffic", "activation", &[], &[]).is_err());
        assert!(
            CapabilityProbe::new(
                "traffic",
                "activation",
                &[0, 0],
                &["a".to_owned(), "b".to_owned()]
            )
            .is_err(),
            "duplicate representatives"
        );
        assert!(
            CapabilityProbe::new("traffic", "activation", &[-1], &["a".to_owned()]).is_err(),
            "negative partition"
        );
        assert!(
            CapabilityProbe::new("traffic", "activation", &[0], &["   ".to_owned()]).is_err(),
            "blank probe id"
        );
        assert!(
            CapabilityProbe::new("traffic", "activation", &[0, 1], &["a".to_owned()]).is_err(),
            "one probe id for two partitions"
        );
    }

    #[test]
    fn fresh_probe_ids_are_unique() {
        let ids = fresh_probe_ids(8);
        assert_eq!(ids.len(), 8);
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 8);
    }
}
