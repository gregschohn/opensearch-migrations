//! Traffic-topic layout discovery and validation (traceability row KAFKA-10).
//!
//! # Owner decision OD-7: validation only, never mutation
//!
//! The Java proxy implements "increase but never reduce" partition growth: when the topic has fewer
//! partitions than `--minimumKafkaTopicPartitions`, it calls
//! `Admin.createPartitions(increaseTo(n))`. **That obligation is retired by owner ruling**, not by
//! implementation convenience: the proxy should never increase the partition count, and a partition
//! increase must always be out-of-band and intentional.
//!
//! So this module discovers and **validates**. It never creates a topic, never calls
//! `create_partitions`, and performs no admin mutation of any kind. A topic below the configured
//! minimum fails startup with a message naming the out-of-band remedy, before the listener opens.
//!
//! # No admin client at all
//!
//! Layout comes from producer/consumer metadata (`fetch_metadata`, `MetadataPartition::leader`),
//! never from `AdminClient`. This is not a style preference: `rust-rdkafka` 0.39's `AdminClient`
//! **never authenticates over MSK IAM**. Every admin call timed out and librdkafka never even
//! requested an OAUTHBEARER token, while the producer and consumer both did. The proxy therefore
//! has no admin dependency, so the defect cannot affect it. Do not re-introduce one believing it
//! works under IAM.
//!
//! Java has two near-identical validation paths, one over `Producer.partitionsFor` and one over
//! `Admin.describeTopics`, which exist only because the two Java APIs expose different metadata
//! types and are otherwise line-for-line duplicates. They are collapsed into one here.
//!
//! # Representative partitions
//!
//! The capability probe needs **one partition per distinct current leader**, because what it
//! qualifies is a broker's timestamp behaviour. The representative for a leader is the
//! **lowest-indexed** partition that leader currently leads, and the representatives are ordered by
//! **leader id ascending** — both deterministic, so two proxies probing the same topic probe the
//! same partitions and the logs are comparable.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::error::TopicLayoutError;

/// One partition and its current leader, as reported by Kafka metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PartitionLayout {
    /// The partition index.
    pub partition: i32,
    /// The current leader's broker id. librdkafka reports `-1` for no leader, which matches Java's
    /// `leader == null || leader.id() < 0` test.
    pub leader: i32,
}

/// Fetches topic metadata.
///
/// A trait so the validation above can be tested exhaustively without a broker, and so the
/// production implementation can be the producer's or the consumer's client handle
/// interchangeably.
pub trait MetadataSource {
    /// Fetches the partition layout of `topic`.
    ///
    /// # Errors
    ///
    /// Returns a message when metadata could not be fetched. The caller treats that as retriable:
    /// an unreachable broker is a transient fact, not a configuration error.
    fn fetch_partitions(
        &self,
        topic: &str,
        timeout: Duration,
    ) -> Result<Vec<PartitionLayout>, String>;
}

/// A [`MetadataSource`] backed by a closure, so a client handle can be adapted without a wrapper
/// type per client kind.
pub struct FnMetadataSource<F>(pub F);

impl<F> MetadataSource for FnMetadataSource<F>
where
    F: Fn(&str, Duration) -> Result<Vec<PartitionLayout>, String>,
{
    fn fetch_partitions(
        &self,
        topic: &str,
        timeout: Duration,
    ) -> Result<Vec<PartitionLayout>, String> {
        (self.0)(topic, timeout)
    }
}

/// A validated traffic-topic layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficTopicMetadata {
    topic: String,
    partition_count: usize,
    /// Leader id to that leader's lowest-indexed partition, ordered by leader id.
    representatives: BTreeMap<i32, i32>,
}

impl TrafficTopicMetadata {
    /// Validates `partitions` as a usable traffic-topic layout.
    ///
    /// # Errors
    ///
    /// * [`TopicLayoutError::NoPartitions`] when the list is empty — the topic may not exist yet,
    ///   so this is retriable. The proxy never creates the topic.
    /// * [`TopicLayoutError::NonContiguous`] when the ascending indices are not `0..N-1`. Not
    ///   retriable: the routing hash indexes a dense partition list, so a hole is a layout an
    ///   operator must explain.
    /// * [`TopicLayoutError::NoLeader`] when any partition has no current leader. Retriable:
    ///   leadership may be in election.
    /// * [`TopicLayoutError::BelowConfiguredMinimum`] when there are fewer partitions than
    ///   `minimum_partitions`. Not retriable, per owner decision OD-7.
    pub fn validate(
        topic: impl Into<String>,
        partitions: &[PartitionLayout],
        minimum_partitions: usize,
    ) -> Result<Self, TopicLayoutError> {
        let topic = topic.into();
        if partitions.is_empty() {
            return Err(TopicLayoutError::NoPartitions { topic });
        }

        let mut sorted = partitions.to_vec();
        sorted.sort_unstable_by_key(|layout| layout.partition);

        for (index, layout) in sorted.iter().enumerate() {
            let expected = i32::try_from(index).map_err(|_| TopicLayoutError::NonContiguous {
                topic: topic.clone(),
                expected_count: sorted.len(),
                index: i32::MAX,
                partition: layout.partition,
            })?;
            if layout.partition != expected {
                return Err(TopicLayoutError::NonContiguous {
                    topic,
                    expected_count: sorted.len(),
                    index: expected,
                    partition: layout.partition,
                });
            }
            if layout.leader < 0 {
                return Err(TopicLayoutError::NoLeader {
                    topic,
                    partition: layout.partition,
                });
            }
        }

        if sorted.len() < minimum_partitions {
            return Err(TopicLayoutError::BelowConfiguredMinimum {
                topic,
                actual: sorted.len(),
                minimum: minimum_partitions,
            });
        }

        // `putIfAbsent` while walking partitions ascending, so a leader's representative is the
        // lowest-indexed partition it leads. The BTreeMap then orders them by leader id.
        let mut representatives = BTreeMap::new();
        for layout in &sorted {
            representatives
                .entry(layout.leader)
                .or_insert(layout.partition);
        }

        Ok(Self {
            topic,
            partition_count: sorted.len(),
            representatives,
        })
    }

    /// Fetches and validates the layout once.
    ///
    /// # Errors
    ///
    /// Propagates [`TrafficTopicMetadata::validate`], and reports a fetch failure as the retriable
    /// [`TopicLayoutError::MetadataUnavailable`].
    pub fn discover(
        source: &dyn MetadataSource,
        topic: &str,
        minimum_partitions: usize,
        timeout: Duration,
    ) -> Result<Self, TopicLayoutError> {
        let partitions = source.fetch_partitions(topic, timeout).map_err(|detail| {
            TopicLayoutError::MetadataUnavailable {
                topic: topic.to_owned(),
                detail,
            }
        })?;
        Self::validate(topic, &partitions, minimum_partitions)
    }

    /// The traffic topic.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// How many partitions the topic has.
    #[must_use]
    pub fn partition_count(&self) -> usize {
        self.partition_count
    }

    /// One partition per distinct current leader, ordered by leader id ascending.
    #[must_use]
    pub fn representative_partitions(&self) -> Vec<i32> {
        self.representatives.values().copied().collect()
    }

    /// The distinct current leader ids, ascending.
    #[must_use]
    pub fn leader_ids(&self) -> Vec<i32> {
        self.representatives.keys().copied().collect()
    }

    /// Whether every leader in `other` is also a leader here.
    ///
    /// A **new** leader appearing after the probes means a broker that was never qualified is now
    /// accepting writes, so the probe must be repeated against the refreshed layout. A leader that
    /// merely *disappeared* needs no re-probe: nothing unqualified gained a partition.
    #[must_use]
    pub fn covers_leaders_of(&self, other: &Self) -> bool {
        other
            .representatives
            .keys()
            .all(|leader| self.representatives.contains_key(leader))
    }
}

/// Extracts the partition layout from an rdkafka metadata response.
///
/// Separated from the client call so the mapping — in particular librdkafka's `-1` for "no leader"
/// — is testable without a broker.
#[must_use]
pub fn partitions_from_metadata(
    metadata: &rdkafka::metadata::Metadata,
    topic: &str,
) -> Vec<PartitionLayout> {
    metadata
        .topics()
        .iter()
        .filter(|candidate| candidate.name() == topic)
        .flat_map(rdkafka::metadata::MetadataTopic::partitions)
        .map(|partition| PartitionLayout {
            partition: partition.id(),
            leader: partition.leader(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(pairs: &[(i32, i32)]) -> Vec<PartitionLayout> {
        pairs
            .iter()
            .map(|(partition, leader)| PartitionLayout {
                partition: *partition,
                leader: *leader,
            })
            .collect()
    }

    #[test]
    fn a_valid_layout_reports_its_shape() {
        // TrafficTopicMetadataTest.java:14, forTopic(7).
        let partitions: Vec<PartitionLayout> = (0..7)
            .map(|partition| PartitionLayout {
                partition,
                leader: 0,
            })
            .collect();
        let metadata = TrafficTopicMetadata::validate("traffic", &partitions, 0).expect("valid");
        assert_eq!(metadata.topic(), "traffic");
        assert_eq!(metadata.partition_count(), 7);
        assert_eq!(metadata.representative_partitions(), vec![0]);
        assert_eq!(metadata.leader_ids(), vec![0]);
    }

    #[test]
    fn discovery_chooses_the_lowest_partition_of_each_current_leader() {
        // TrafficTopicMetadataTest.discoveryChoosesOnePartitionPerCurrentLeader (:25-40): leaders
        // {10, 11} over 4 partitions yield representatives [0, 1].
        let metadata = TrafficTopicMetadata::validate(
            "traffic",
            &layout(&[(0, 10), (1, 11), (2, 10), (3, 11)]),
            0,
        )
        .expect("valid");
        assert_eq!(metadata.representative_partitions(), vec![0, 1]);
        assert_eq!(metadata.leader_ids(), vec![10, 11]);
    }

    #[test]
    fn representatives_are_ordered_by_leader_id_not_by_partition() {
        // The lowest partition belongs to the highest leader id here, so a partition-ordered
        // implementation would disagree.
        let metadata =
            TrafficTopicMetadata::validate("traffic", &layout(&[(0, 9), (1, 2), (2, 5)]), 0)
                .expect("valid");
        assert_eq!(metadata.leader_ids(), vec![2, 5, 9]);
        assert_eq!(
            metadata.representative_partitions(),
            vec![1, 2, 0],
            "leader 2 leads partition 1, leader 5 leads 2, leader 9 leads 0"
        );
    }

    #[test]
    fn an_unsorted_metadata_response_is_sorted_before_validation() {
        let metadata =
            TrafficTopicMetadata::validate("traffic", &layout(&[(2, 0), (0, 0), (1, 0)]), 0)
                .expect("order of arrival is not a layout property");
        assert_eq!(metadata.partition_count(), 3);
    }

    #[test]
    fn an_empty_topic_is_retriable_and_the_proxy_never_creates_it() {
        // KafkaCaptureFactory retries forever; --kafkaTopic is never created by the proxy.
        let error = TrafficTopicMetadata::validate("traffic", &[], 0).expect_err("no partitions");
        assert!(matches!(error, TopicLayoutError::NoPartitions { .. }));
        assert!(error.is_retriable());
    }

    #[test]
    fn a_non_contiguous_layout_is_rejected() {
        // TrafficTopicMetadata.java:76-83. The Java suite never covers this directly; this is one
        // of the tests the spec asks to add.
        let error = TrafficTopicMetadata::validate("traffic", &layout(&[(0, 0), (2, 0)]), 0)
            .expect_err("partition 1 is missing");
        match error {
            TopicLayoutError::NonContiguous {
                index, partition, ..
            } => {
                assert_eq!(index, 1);
                assert_eq!(partition, 2);
            }
            other => panic!("expected a contiguity failure, got {other}"),
        }
        assert!(
            !TopicLayoutError::NonContiguous {
                topic: "traffic".into(),
                expected_count: 2,
                index: 1,
                partition: 2
            }
            .is_retriable(),
            "a hole in the partition list needs an operator, not a retry"
        );
    }

    #[test]
    fn a_layout_starting_above_zero_is_non_contiguous() {
        let error = TrafficTopicMetadata::validate("traffic", &layout(&[(1, 0), (2, 0)]), 0)
            .expect_err("partition 0 is missing");
        assert!(matches!(error, TopicLayoutError::NonContiguous { .. }));
    }

    #[test]
    fn a_missing_partition_leader_is_retriable() {
        // TrafficTopicMetadataTest.missingPartitionLeaderRetriesAsUnavailableMetadata (:43-52).
        // librdkafka reports -1 where the Java client reports a null Node.
        for leader in [-1_i32, i32::MIN] {
            let error =
                TrafficTopicMetadata::validate("traffic", &layout(&[(0, 0), (1, leader)]), 0)
                    .expect_err("partition 1 has no leader");
            match error {
                TopicLayoutError::NoLeader { partition, .. } => assert_eq!(partition, 1),
                other => panic!("expected a leader failure, got {other}"),
            }
            assert!(error.is_retriable(), "leader election is transient");
        }
    }

    #[test]
    fn a_topic_below_the_configured_minimum_fails_startup_rather_than_being_grown() {
        // Owner decision OD-7. Java would call Admin.createPartitions(increaseTo(2)) here.
        let error = TrafficTopicMetadata::validate("traffic", &layout(&[(0, 0)]), 2)
            .expect_err("one partition, two required");
        match &error {
            TopicLayoutError::BelowConfiguredMinimum {
                actual, minimum, ..
            } => {
                assert_eq!(*actual, 1);
                assert_eq!(*minimum, 2);
            }
            other => panic!("expected a minimum failure, got {other}"),
        }
        assert!(
            !error.is_retriable(),
            "the proxy will never grow the topic, so retrying would loop forever"
        );
        let text = error.to_string();
        assert!(text.contains("never changes the partition count"), "{text}");
        assert!(text.contains("out of band"), "{text}");
    }

    #[test]
    fn a_topic_at_or_above_the_minimum_is_accepted_unchanged() {
        for partition_count in 2..5 {
            let partitions: Vec<PartitionLayout> = (0..partition_count)
                .map(|partition| PartitionLayout {
                    partition,
                    leader: 0,
                })
                .collect();
            let metadata =
                TrafficTopicMetadata::validate("traffic", &partitions, 2).expect("at or above");
            assert_eq!(
                metadata.partition_count(),
                usize::try_from(partition_count).expect("small"),
                "the proxy reports what the topic has and changes nothing"
            );
        }
    }

    #[test]
    fn a_new_leader_after_probing_requires_a_re_probe_but_a_departed_one_does_not() {
        // KafkaCaptureFactory.java:683-699.
        let probed = TrafficTopicMetadata::validate("traffic", &layout(&[(0, 10), (1, 11)]), 0)
            .expect("valid");
        let same = TrafficTopicMetadata::validate("traffic", &layout(&[(0, 10), (1, 11)]), 0)
            .expect("valid");
        let leader_departed =
            TrafficTopicMetadata::validate("traffic", &layout(&[(0, 10), (1, 10)]), 0)
                .expect("valid");
        let leader_appeared =
            TrafficTopicMetadata::validate("traffic", &layout(&[(0, 10), (1, 12)]), 0)
                .expect("valid");

        assert!(probed.covers_leaders_of(&same), "no change, no re-probe");
        assert!(
            probed.covers_leaders_of(&leader_departed),
            "a departed leader gained nothing, so nothing is unqualified"
        );
        assert!(
            !probed.covers_leaders_of(&leader_appeared),
            "leader 12 was never qualified and is now accepting writes"
        );
    }

    #[test]
    fn a_fetch_failure_is_reported_as_retriable_metadata_unavailability() {
        let source = FnMetadataSource(|_topic: &str, _timeout: Duration| {
            Err("Broker transport failure".to_owned())
        });
        let error = TrafficTopicMetadata::discover(&source, "traffic", 0, Duration::from_secs(5))
            .expect_err("the broker is unreachable");
        match &error {
            TopicLayoutError::MetadataUnavailable { detail, .. } => {
                assert!(detail.contains("transport"));
            }
            other => panic!("expected metadata unavailability, got {other}"),
        }
        assert!(error.is_retriable());
    }

    #[test]
    fn discovery_passes_the_topic_and_timeout_through() {
        let source = FnMetadataSource(|topic: &str, timeout: Duration| {
            assert_eq!(topic, "traffic");
            assert_eq!(timeout, Duration::from_secs(7));
            Ok(vec![PartitionLayout {
                partition: 0,
                leader: 3,
            }])
        });
        let metadata =
            TrafficTopicMetadata::discover(&source, "traffic", 1, Duration::from_secs(7))
                .expect("valid");
        assert_eq!(metadata.leader_ids(), vec![3]);
    }
}
