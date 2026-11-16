//! Construction of the two control records: writer-partition heartbeats and capability probes
//! (traceability rows KAFKA-01 and KAFKA-03).
//!
//! # Three record kinds, and no fourth
//!
//! The capture topic carries exactly three record kinds — traffic, heartbeat, and capability probe
//! — and `CaptureRecord.payload` is the **only** type discriminator. Traffic records are built by
//! `capture-wire`; the two control records are built here.
//!
//! # No headers, ever
//!
//! Not one capture record carries a Kafka header. Java gets this by using the five-argument
//! `ProducerRecord(topic, partition, timestamp, key, value)` constructor, and asserts it in six
//! separate places. Here it is structural: these builders return a key and a value, and the
//! producer call site never touches `BaseRecord::headers`.
//!
//! # No partition on the wire
//!
//! `WriterPartitionHeartbeat` has `reserved 2` and `TrafficStream` has `reserved 8`, both for a
//! partition field that must never come back. A record's partition is Kafka record metadata and
//! nothing else, so a replayer can never disagree with Kafka about where a record lives. The
//! heartbeat's partition appears only in its **key**, where it cannot be mistaken for payload.
//!
//! # Timestamps
//!
//! Neither builder sets a producer timestamp.
//!
//! * Traffic and heartbeat records pass no timestamp in Java either (`null`), so the client stamps
//!   its own clock as `CreateTime` and the broker overwrites it with `LogAppendTime`. The proxy
//!   never reads the value it supplied; it reads only what the broker returned.
//! * The probe is a deliberate deviation: see [`ProbeRecord`].

use std::sync::Arc;

use capture_wire::proto::{
    CaptureCapabilityProbe, CaptureRecord, WriterPartitionHeartbeat, capture_record,
};
use prost::Message;

use crate::routing_state::WriterPartition;

/// The literal suffix that distinguishes a probe identity from a generation identity.
///
/// A generation identity is `captureActivationId:<sequence>` where the sequence is a number, so
/// `captureActivationId:PROBE` can never collide with one.
pub const PROBE_WRITER_SUFFIX: &str = ":PROBE";

/// The separator between a writer identity and the rest of a heartbeat key.
const HEARTBEAT_KEY_INFIX: &str = ":heartbeat:";

/// A heartbeat record, ready to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatRecord {
    key: String,
    value: Vec<u8>,
    partition: i32,
}

impl HeartbeatRecord {
    /// Builds the heartbeat for `writer_partition`.
    ///
    /// `heartbeat_interval_millis` is the configured `H` and is **informational only** — the
    /// replayer derives nothing from it. `emitted_at_millis` is the local wall clock at build time
    /// and is **diagnostic only**; no decision anywhere reads it, which is why a wall clock is
    /// acceptable there and nowhere else in this crate.
    #[must_use]
    pub fn build(
        writer_partition: &WriterPartition,
        heartbeat_interval_millis: i64,
        emitted_at_millis: Option<i64>,
    ) -> Self {
        let payload = WriterPartitionHeartbeat {
            writer_node_id: writer_partition.writer_node_id().to_owned(),
            heartbeat_interval_millis,
            emitted_at_millis,
        };
        let envelope = CaptureRecord {
            payload: Some(capture_record::Payload::WriterPartitionHeartbeat(payload)),
        };
        Self {
            key: format!(
                "{}{HEARTBEAT_KEY_INFIX}{}",
                writer_partition.writer_node_id(),
                writer_partition.partition()
            ),
            value: envelope.encode_to_vec(),
            partition: writer_partition.partition(),
        }
    }

    /// The Kafka record key, `<writerNodeId>:heartbeat:<partition>`.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The encoded `CaptureRecord` envelope.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// The explicit partition this record must be produced to.
    #[must_use]
    pub fn partition(&self) -> i32 {
        self.partition
    }
}

/// A capability-probe record, ready to produce.
///
/// # Owner decision OD-4
///
/// Java publishes the probe with a **literal zero** producer timestamp and accepts the
/// acknowledgement when the returned timestamp is positive, inferring that a positive value must
/// have come from the broker because zero was supplied. That inference cannot be ported:
///
/// * `rust-rdkafka` sends `record.timestamp.unwrap_or(0)`
///   (`base_producer.rs:481-482`), so `Some(0)` and `None` are indistinguishable; and
/// * librdkafka substitutes the client's own wall clock for a zero timestamp
///   (`rdkafka_msg.c:274-278`).
///
/// So a zero-timestamp produce to a `CreateTime` topic comes back with a *positive* timestamp
/// byte-identical to the client's own clock, and the "positive" test passes a topic it was designed
/// to fail. This was observed, not reasoned about: the MSK proof recorded
/// `CreateTime(1794790929566)` against a pre-produce wall clock of `1794790929566`.
///
/// The probe therefore sets **no** producer timestamp, and acceptance requires
/// `Timestamp::LogAppendTime(t)` with `t > 0` on the delivery report — the broker's own assertion
/// about where the timestamp came from, rather than an inference. The on-wire `CaptureRecord` is
/// unchanged, including `writerNodeId = captureActivationId + ":PROBE"`; nothing in the proxy or
/// the replayer reads a probe record's timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRecord {
    key: String,
    value: Vec<u8>,
    partition: i32,
    probe_id: String,
}

impl ProbeRecord {
    /// Builds the probe for `partition` under `capture_activation_id`.
    #[must_use]
    pub fn build(capture_activation_id: &str, probe_id: impl Into<String>, partition: i32) -> Self {
        let probe_id = probe_id.into();
        let writer_node_id = probe_writer_node_id(capture_activation_id);
        let payload = CaptureCapabilityProbe {
            writer_node_id: writer_node_id.clone(),
            probe_id: probe_id.clone(),
        };
        let envelope = CaptureRecord {
            payload: Some(capture_record::Payload::CaptureCapabilityProbe(payload)),
        };
        Self {
            key: format!("{writer_node_id}:{probe_id}"),
            value: envelope.encode_to_vec(),
            partition,
            probe_id,
        }
    }

    /// The Kafka record key, `<captureActivationId>:PROBE:<probeId>`.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The encoded `CaptureRecord` envelope.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }

    /// The representative partition this probe must be produced to.
    #[must_use]
    pub fn partition(&self) -> i32 {
        self.partition
    }

    /// This probe's unique id.
    #[must_use]
    pub fn probe_id(&self) -> &str {
        &self.probe_id
    }
}

/// The probe's writer identity, `captureActivationId:PROBE`.
#[must_use]
pub fn probe_writer_node_id(capture_activation_id: &str) -> String {
    format!("{capture_activation_id}{PROBE_WRITER_SUFFIX}")
}

/// Builds a traffic record's key.
///
/// The key is the connection id verbatim, which is what gives a connection's records partition
/// locality under any Kafka-side re-partitioning a tool might perform. The proxy itself always
/// supplies an explicit partition, so the key is never used for partitioning by the client.
#[must_use]
pub fn traffic_key(connection_id: &Arc<str>) -> Arc<str> {
    Arc::clone(connection_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer_partition(writer: &str, partition: i32) -> WriterPartition {
        WriterPartition::new(Arc::from(writer), partition)
    }

    #[test]
    fn a_heartbeat_round_trips_through_the_capture_record_envelope() {
        // CaptureKafkaPublisherTest.assertHeartbeat (:683-699), including the fixed-clock
        // emittedAtMillis assertion.
        let record = HeartbeatRecord::build(&writer_partition("activation:1", 2), 20, Some(1_234));
        assert_eq!(record.key(), "activation:1:heartbeat:2");
        assert_eq!(record.partition(), 2);

        let decoded = CaptureRecord::decode(record.value()).expect("a canonical envelope");
        let Some(capture_record::Payload::WriterPartitionHeartbeat(heartbeat)) = decoded.payload
        else {
            panic!("the payload oneof must identify a heartbeat");
        };
        assert_eq!(heartbeat.writer_node_id, "activation:1");
        assert_eq!(heartbeat.heartbeat_interval_millis, 20);
        assert_eq!(heartbeat.emitted_at_millis, Some(1_234));
    }

    #[test]
    fn a_heartbeat_payload_carries_no_partition() {
        // PROTO `reserved 2`: the partition comes only from Kafka record metadata. Field 2 must not
        // appear anywhere in the encoded heartbeat.
        let record = HeartbeatRecord::build(&writer_partition("activation:1", 7), 10_000, None);
        let decoded = CaptureRecord::decode(record.value()).expect("canonical");
        let Some(capture_record::Payload::WriterPartitionHeartbeat(heartbeat)) = decoded.payload
        else {
            panic!("heartbeat payload");
        };
        // Re-encoding the inner message and scanning for field 2's tag (2 << 3 | varint = 0x10,
        // or any wire type for field 2) proves the field is absent rather than merely unread.
        let inner = heartbeat.encode_to_vec();
        for wire_type in 0_u8..=5 {
            let tag = (2_u8 << 3) | wire_type;
            assert!(
                !inner.contains(&tag),
                "field 2 (wire type {wire_type}) must never appear in a heartbeat"
            );
        }
        assert_eq!(
            record.partition(),
            7,
            "the partition travels as Kafka metadata and in the key"
        );
        assert!(record.key().ends_with(":7"));
    }

    #[test]
    fn an_absent_emitted_at_is_absent_on_the_wire() {
        let record = HeartbeatRecord::build(&writer_partition("activation:1", 0), 10_000, None);
        let decoded = CaptureRecord::decode(record.value()).expect("canonical");
        let Some(capture_record::Payload::WriterPartitionHeartbeat(heartbeat)) = decoded.payload
        else {
            panic!("heartbeat payload");
        };
        assert_eq!(heartbeat.emitted_at_millis, None);
    }

    #[test]
    fn a_probe_uses_the_probe_writer_identity_and_cannot_collide_with_a_generation() {
        // CaptureKafkaCapabilityProbeTest.java:30 and CKCP:58, :119.
        let record = ProbeRecord::build("activation", "probe-7", 3);
        assert_eq!(record.key(), "activation:PROBE:probe-7");
        assert_eq!(record.partition(), 3);
        assert_eq!(record.probe_id(), "probe-7");

        let decoded = CaptureRecord::decode(record.value()).expect("canonical");
        let Some(capture_record::Payload::CaptureCapabilityProbe(probe)) = decoded.payload else {
            panic!("the payload oneof must identify a capability probe");
        };
        assert_eq!(probe.writer_node_id, "activation:PROBE");
        assert_eq!(probe.probe_id, "probe-7");

        // A generation identity is `activation:<number>`, so the suffix cannot be produced by any
        // sequence value.
        assert!(
            probe
                .writer_node_id
                .rsplit(':')
                .next()
                .is_some_and(|s| s.parse::<u64>().is_err()),
            "the probe suffix is never numeric, so it can never be read as a generation"
        );
    }

    #[test]
    fn the_payload_oneof_is_the_only_discriminator() {
        // Decoding one kind must not accidentally succeed as another: the oneof tag decides.
        let heartbeat =
            HeartbeatRecord::build(&writer_partition("activation:1", 0), 10_000, Some(5));
        let probe = ProbeRecord::build("activation", "p", 0);

        assert!(matches!(
            CaptureRecord::decode(heartbeat.value())
                .expect("canonical")
                .payload,
            Some(capture_record::Payload::WriterPartitionHeartbeat(_))
        ));
        assert!(matches!(
            CaptureRecord::decode(probe.value())
                .expect("canonical")
                .payload,
            Some(capture_record::Payload::CaptureCapabilityProbe(_))
        ));
        assert_ne!(heartbeat.value(), probe.value());
    }

    #[test]
    fn a_traffic_key_is_the_connection_id_verbatim() {
        let connection_id: Arc<str> = Arc::from("0242c0fffea82008-0000000a-00000003");
        assert_eq!(&*traffic_key(&connection_id), &*connection_id);
    }
}
