//! Cross-language golden records: the Phase 2 exit gate.
//!
//! > Java replayer utilities decode Rust-generated golden records with equivalent fields and exact
//! > embedded traffic bytes.
//!
//! Every file under `fixtures/goldens/` is produced here by driving the **real** encoder path —
//! [`ConnectionRecorder`] over a [`CollectingSink`], and `capture-kafka`'s real heartbeat and probe
//! builders — with fixed connection and node ids, a [`ManualClock`], and fixed timestamps. Nothing is
//! hand-built protobuf. The Java side, `RustGoldenRecordCompatibilityTest` in `trafficReplayer`, reads
//! the same files from the repository, decodes the records with the replayer's own classes, runs them
//! through the replayer's own intake and reconstruction, and asserts the `expected` block.
//!
//! # Where each expectation comes from
//!
//! * `records[*].valueBase64` — the bytes the encoder produced, verbatim.
//! * `expected.records` — those bytes decoded by `prost` from the same unchanged `.proto`, an
//!   implementation independent of the single-pass encoder. Java must decode the same fields.
//! * `expected.connections[*].events` — built from the scenario **inputs** (the bytes the client sent
//!   and the source returned), never from decoding. Each event's ordinal, bytes, packet count and
//!   timestamps are what the replayer must reconstruct. Response completion evidence is declared by
//!   the scenario author from HTTP framing, not inferred.
//!
//! Before writing anything the generator also checks the decoded records against the inputs (byte
//! exactness of every read, write and interim response; contiguous sequences; contiguous indices;
//! `CloseObservation` last), so a Java failure points at Java decoding or reconstruction rather than
//! at a bad golden.
//!
//! # Kafka metadata
//!
//! Partition, key, offset and `LogAppendTime` are what the harness assigns in place of a broker:
//! the partition is the real Java-compatible routing hash over eight partitions, the key is the real
//! key builder's output, offsets count up per partition, and the broker appends each record one
//! millisecond of [`ManualClock`] time after it is handed over. Producer-level facts — zero Kafka
//! headers and the `LogAppendTime` timestamp type — are not representable here and remain covered by
//! the end-to-end wire-proof differential.
//!
//! # Updating
//!
//! The test fails when a golden is stale. Regenerate and review the diff with:
//!
//! ```sh
//! CAPTURE_GOLDENS_UPDATE=1 cargo test -p capture-test-support --test golden_records
//! ```
//!
//! then re-run the Java side from the repository root. The replayer's `build.gradle` declares the
//! golden directory as a test-task input, so a changed golden re-runs the test:
//!
//! ```sh
//! ./gradlew :TrafficCapture:trafficReplayer:test \
//!   --tests 'org.opensearch.migrations.replay.kafka.RustGoldenRecordCompatibilityTest'
//! ```

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use capture_kafka::records::{HeartbeatRecord, ProbeRecord, traffic_key};
use capture_kafka::routing_state::WriterPartition;
use capture_kafka::select_partition;
use capture_wire::proto::capture_record::Payload;
use capture_wire::proto::traffic_observation::Capture;
use capture_wire::proto::traffic_stream::Index;
use capture_wire::proto::{CaptureRecord, TrafficObservation, TrafficStream};
use capture_wire::record_sink::CollectingSink;
use capture_wire::{
    CaptureTimestamp, ConnectionRecorder, ConnectionRecorderConfig, ManualClock, MonotonicClock,
    PublishOutcome, ScheduledFlush,
};
use prost::Message as _;
use serde::Serialize;

// ------------------------------------------------------------------------------------------------
// Fixed inputs
// ------------------------------------------------------------------------------------------------

const FORMAT: &str = "capture-golden-v1";
const GENERATOR: &str =
    "TrafficCapture/captureProxy/crates/capture-test-support/tests/golden_records.rs";
const UPDATE_ENV: &str = "CAPTURE_GOLDENS_UPDATE";
const TOPIC: &str = "capture-golden";

/// One capture activation; the writer identity is `captureActivationId:routingGenerationSequence`.
const ACTIVATION_ID: &str = "00000000-0000-4000-8000-00000000c0de";
const NODE_ID: &str = "00000000-0000-4000-8000-00000000c0de:1";
const PROBE_ID: &str = "00000000-0000-4000-8000-0000000000aa";
/// The Rust proxy's connection-id prefix: `${HOSTNAME}_<per-process UUID>`.
const PROCESS_ID: &str = "golden-host_00000000-0000-4000-8000-000000000001";
const PARTITIONS: [i32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Wall-clock origin of every scenario: an exact second, so the first observation's timestamp has
/// `nanos == 0` and exercises the canonical omission of that field.
const BASE_WALL_NANOS: u64 = 1_794_800_000_000_000_000;
/// Time that passes after every observation. Deliberately not a round number so `nanos` varies.
const OBSERVATION_STEP: Duration = Duration::from_nanos(1_250_125);
/// Time between a record reaching the sink and the broker appending it.
const BROKER_APPEND_LATENCY: Duration = Duration::from_millis(1);
/// The heartbeat interval `H`, informational only on the wire.
const HEARTBEAT_INTERVAL_MILLIS: i64 = 10_000;
/// A payload ceiling small enough that ordinary messages are segmented across several records.
const SMALL_PAYLOAD_LIMIT: u32 = 1024;

const COMMON_HEADERS: &[&str] = &[
    "User-Agent: capture-golden/1.0 (cross-language)",
    "Accept: application/json",
    "Accept-Encoding: identity",
    "Connection: keep-alive",
];
/// A long value, unusual casing and a duplicated name, so header bytes are not only the trivial case.
const RICH_HEADERS: &[&str] = &[
    "x-ODD-CaSiNg: MixedCaseValue",
    "X-Repeated: first",
    "X-Repeated: second",
    "X-Long: abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij\
     abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij\
     abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij",
];

/// Every scenario the Phase 2 gate requires, by golden file stem.
const REQUIRED_SCENARIOS: &[&str] = &[
    "keep_alive_gets",
    "critical_mutations",
    "chunked_with_trailers",
    "expect_continue_interim",
    "suppressed_request_dropped",
    "segmented_large_response",
    "segmented_large_request",
    "connection_exception",
    "close_terminal_record",
    "periodic_flush",
    "writer_partition_heartbeat",
    "capture_capability_probe",
];

// ------------------------------------------------------------------------------------------------
// The golden document
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Golden {
    format: &'static str,
    scenario: &'static str,
    description: &'static str,
    generator: &'static str,
    topic: &'static str,
    records: Vec<GoldenRecord>,
    expected: Expected,
}

/// One Kafka record exactly as the replayer's consumer would receive it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoldenRecord {
    partition: i32,
    offset: i64,
    log_append_time_millis: i64,
    key: String,
    value_base64: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    /// One entry per `records` entry, in the same order.
    records: Vec<DecodedRecord>,
    /// The replayer's reconstruction, per captured connection.
    connections: Vec<ExpectedConnection>,
    /// Writer-time transitions replay intake must report, when the scenario declares them.
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_time_transitions: Option<BTreeMap<&'static str, u32>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "payload", rename_all_fields = "camelCase")]
enum DecodedRecord {
    #[serde(rename = "trafficStream")]
    TrafficStream {
        connection_id: String,
        node_id: String,
        index: DecodedIndex,
        /// `null` means absent on the wire, which the replayer distinguishes from a present zero.
        prior_requests_received: Option<i32>,
        /// `null` means absent on the wire.
        last_observation_was_unterminated_read: Option<bool>,
        observations: Vec<DecodedObservation>,
    },
    #[serde(rename = "writerPartitionHeartbeat")]
    WriterPartitionHeartbeat {
        writer_node_id: String,
        heartbeat_interval_millis: i64,
        /// `null` means absent on the wire.
        emitted_at_millis: Option<i64>,
    },
    #[serde(rename = "captureCapabilityProbe")]
    CaptureCapabilityProbe {
        writer_node_id: String,
        probe_id: String,
    },
}

#[derive(Debug, Serialize)]
struct DecodedIndex {
    /// `number` or `numberOfThisLastChunk`: which arm of the `index` oneof is set.
    field: &'static str,
    value: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DecodedObservation {
    sequence: u64,
    ts: Ts,
    /// The proto field name of the `Capture` oneof arm.
    capture: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_line_byte_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers_byte_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Ts {
    seconds: i64,
    nanos: i32,
}

impl From<CaptureTimestamp> for Ts {
    fn from(ts: CaptureTimestamp) -> Self {
        Self {
            seconds: ts.seconds,
            nanos: ts.nanos,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedConnection {
    connection_id: String,
    node_id: &'static str,
    partition: i32,
    /// Source-assembly callbacks in the order the replayer must make them.
    events: Vec<Event>,
    /// Reconstruction when replay begins part-way through the connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    resume: Option<Resume>,
}

/// Replay starting at a later record of the connection, as after a consumer restart. This is what
/// proves `priorRequestsReceived` and `lastObservationWasUnterminatedRead` mean the same thing to the
/// replayer as they did to the writer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Resume {
    from_offset: i64,
    events: Vec<Event>,
}

/// One source-assembly callback.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
enum Event {
    /// `onRequestReconstituted`.
    #[serde(rename = "REQ")]
    Request {
        ordinal: u64,
        message: MessageBytes,
        first_byte_ts: Ts,
        end_of_message_ts: Ts,
        completing_log_append_time_millis: i64,
    },
    /// `onSourceInterimResponse`.
    #[serde(rename = "INT")]
    Interim { ordinal: u64, message: MessageBytes },
    /// `onSourceResponseComplete`.
    #[serde(rename = "RSP")]
    Response {
        ordinal: u64,
        message: MessageBytes,
        evidence: &'static str,
        request_forwarded_ts: Option<Ts>,
    },
    /// `onCapturedClose`.
    #[serde(rename = "CLOSED")]
    Closed { ordinal: u64, ts: Ts },
}

impl Event {
    fn ordinal(&self) -> u64 {
        match self {
            Self::Request { ordinal, .. }
            | Self::Interim { ordinal, .. }
            | Self::Response { ordinal, .. }
            | Self::Closed { ordinal, .. } => *ordinal,
        }
    }
}

/// An HTTP message's reconstructed bytes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageBytes {
    /// For a reviewer and for the dump transcript; `bytesBase64` is authoritative.
    first_line: String,
    byte_length: usize,
    /// How many captured observations carried the message: one per whole observation, and one per
    /// segmented run however many segments it had.
    packet_count: usize,
    bytes_base64: String,
}

impl MessageBytes {
    fn of(bytes: &[u8], packet_count: usize) -> Self {
        let line_end = bytes
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .unwrap_or(bytes.len());
        Self {
            first_line: String::from_utf8_lossy(&bytes[..line_end]).into_owned(),
            byte_length: bytes.len(),
            packet_count,
            bytes_base64: BASE64.encode(bytes),
        }
    }
}

/// How the replayer learns a source response ended (`SourceResponseCompletionEvidence`).
#[derive(Debug, Clone, Copy)]
enum Evidence {
    HttpFraming,
    ConnectionClose,
    ConnectionException,
}

impl Evidence {
    fn java_name(self) -> &'static str {
        match self {
            Self::HttpFraming => "HTTP_FRAMING",
            Self::ConnectionClose => "CONNECTION_CLOSE",
            Self::ConnectionException => "CONNECTION_EXCEPTION",
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The harness: a broker log and a connection driver
// ------------------------------------------------------------------------------------------------

/// A record the broker appended.
#[derive(Debug)]
struct Appended {
    partition: i32,
    key: String,
    value: Vec<u8>,
    log_append_time_millis: i64,
    /// The connection and the record's 0-based position within it, for traffic records.
    connection: Option<(String, usize)>,
}

type BrokerLog = Rc<RefCell<Vec<Appended>>>;

fn wall_nanos(clock: &ManualClock) -> u64 {
    BASE_WALL_NANOS + clock.now_nanos()
}

fn wall_timestamp(clock: &ManualClock) -> CaptureTimestamp {
    let nanos = wall_nanos(clock);
    CaptureTimestamp::new(
        i64::try_from(nanos / 1_000_000_000).expect("seconds fit in i64"),
        i32::try_from(nanos % 1_000_000_000).expect("sub-second nanos fit in i32"),
    )
}

fn wall_millis(clock: &ManualClock) -> i64 {
    i64::try_from(wall_nanos(clock) / 1_000_000).expect("millis fit in i64")
}

/// One scenario: a shared clock, a shared broker log, and the connections captured into it.
struct Scenario {
    name: &'static str,
    description: &'static str,
    clock: Arc<ManualClock>,
    log: BrokerLog,
    connections: Vec<FinishedConnection>,
    writer_time_transitions: Option<BTreeMap<&'static str, u32>>,
}

impl Scenario {
    fn new(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            clock: Arc::new(ManualClock::new()),
            log: Rc::new(RefCell::new(Vec::new())),
            connections: Vec::new(),
            writer_time_transitions: None,
        }
    }

    /// Opens connection number `ordinal` of the process, as the proxy would name it.
    fn connection(&self, ordinal: u64, payload_limit: Option<u32>) -> ConnectionDriver {
        ConnectionDriver::open(
            format!("{PROCESS_ID}-{ordinal:016x}"),
            payload_limit,
            Arc::clone(&self.clock),
            Rc::clone(&self.log),
        )
    }

    fn add(&mut self, connection: ConnectionDriver) {
        self.connections.push(connection.finish());
    }

    /// Appends a heartbeat built by the real control-record builder.
    fn heartbeat(&self, partition: i32, emitted_at_millis: Option<i64>) {
        let record = HeartbeatRecord::build(
            &WriterPartition::new(Arc::from(NODE_ID), partition),
            HEARTBEAT_INTERVAL_MILLIS,
            emitted_at_millis,
        );
        self.append_control(record.partition(), record.key(), record.value());
    }

    /// Appends a capability probe built by the real control-record builder.
    fn probe(&self, partition: i32) {
        let record = ProbeRecord::build(ACTIVATION_ID, PROBE_ID, partition);
        self.append_control(record.partition(), record.key(), record.value());
    }

    fn append_control(&self, partition: i32, key: &str, value: &[u8]) {
        self.clock.advance(BROKER_APPEND_LATENCY);
        self.log.borrow_mut().push(Appended {
            partition,
            key: key.to_owned(),
            value: value.to_vec(),
            log_append_time_millis: wall_millis(&self.clock),
            connection: None,
        });
    }

    fn idle(&self, duration: Duration) {
        self.clock.advance(duration);
    }

    fn build(self) -> Golden {
        let log = self.log.borrow();
        let mut next_offset: BTreeMap<i32, i64> = BTreeMap::new();
        let mut offsets = Vec::with_capacity(log.len());
        for appended in &*log {
            let offset = next_offset.entry(appended.partition).or_insert(0);
            offsets.push(*offset);
            *offset += 1;
        }

        let records = log
            .iter()
            .zip(&offsets)
            .map(|(appended, offset)| GoldenRecord {
                partition: appended.partition,
                offset: *offset,
                log_append_time_millis: appended.log_append_time_millis,
                key: appended.key.clone(),
                value_base64: BASE64.encode(&appended.value),
            })
            .collect();
        let decoded = log
            .iter()
            .map(|appended| decode_record(&appended.value))
            .collect();

        let connections = self
            .connections
            .iter()
            .map(|connection| {
                verify_connection(connection, &log);
                connection.expected(&log, &offsets)
            })
            .collect();

        Golden {
            format: FORMAT,
            scenario: self.name,
            description: self.description,
            generator: GENERATOR,
            topic: TOPIC,
            records,
            expected: Expected {
                records: decoded,
                connections,
                writer_time_transitions: self.writer_time_transitions,
            },
        }
    }
}

/// A message being captured: its bytes, how many observations carried them, and when it started.
#[derive(Debug)]
struct Pending {
    bytes: Vec<u8>,
    packets: usize,
    first_ts: CaptureTimestamp,
}

impl Pending {
    fn new(first_ts: CaptureTimestamp) -> Self {
        Self {
            bytes: Vec::new(),
            packets: 0,
            first_ts,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        self.packets += 1;
    }
}

#[derive(Debug)]
struct PendingResponse {
    ordinal: u64,
    message: Option<Pending>,
    forwarded_ts: Option<CaptureTimestamp>,
}

/// Drives one [`ConnectionRecorder`] and records, from its inputs, what the replayer must rebuild.
struct ConnectionDriver {
    connection_id: Arc<str>,
    partition: i32,
    payload_limit: Option<u32>,
    recorder: ConnectionRecorder<CollectingSink>,
    clock: Arc<ManualClock>,
    log: BrokerLog,
    /// Records of this connection the broker has appended: `(first sequence, last sequence, LAT)`.
    appended: Vec<(u64, u64, i64)>,
    /// Request boundaries so far, completed and dropped alike: the next request's ordinal.
    ordinal: u64,
    request: Option<Pending>,
    pending_interims: Vec<Pending>,
    response: Option<PendingResponse>,
    any_request_reconstituted: bool,
    events: Vec<Event>,
    /// `(event index, end-of-message sequence)`, resolved to a `LogAppendTime` at the end.
    unresolved_requests: Vec<(usize, u64)>,
    /// `(record position, first ordinal)` to replay from, if the scenario declares a resume.
    resume: Option<(usize, u64)>,
    all_reads: Vec<u8>,
    all_writes: Vec<u8>,
    all_interims: Vec<u8>,
}

impl ConnectionDriver {
    fn open(
        connection_id: String,
        payload_limit: Option<u32>,
        clock: Arc<ManualClock>,
        log: BrokerLog,
    ) -> Self {
        let connection_id: Arc<str> = Arc::from(connection_id);
        let partition = select_partition(&connection_id, &PARTITIONS);
        let mut config = ConnectionRecorderConfig::new(Arc::clone(&connection_id), NODE_ID);
        config.payload_limit = payload_limit;
        let recorder = ConnectionRecorder::with_clock(
            config,
            CollectingSink::new(),
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        )
        .expect("a valid recorder configuration");
        Self {
            connection_id,
            partition,
            payload_limit,
            recorder,
            clock,
            log,
            appended: Vec::new(),
            ordinal: 0,
            request: None,
            pending_interims: Vec::new(),
            response: None,
            any_request_reconstituted: false,
            events: Vec::new(),
            unresolved_requests: Vec::new(),
            resume: None,
            all_reads: Vec::new(),
            all_writes: Vec::new(),
            all_interims: Vec::new(),
        }
    }

    fn now(&self) -> CaptureTimestamp {
        wall_timestamp(&self.clock)
    }

    fn step(&self) {
        self.clock.advance(OBSERVATION_STEP);
    }

    /// Plays Kafka: every record the chain releases is appended and acknowledged, in order.
    fn settle(&mut self) {
        loop {
            let records = self.recorder.sink().records();
            while self.appended.len() < records.len() {
                let record = &records[self.appended.len()];
                self.clock.advance(BROKER_APPEND_LATENCY);
                let log_append_time_millis = wall_millis(&self.clock);
                self.log.borrow_mut().push(Appended {
                    partition: self.partition,
                    key: traffic_key(&self.connection_id).to_string(),
                    value: record.payload().to_vec(),
                    log_append_time_millis,
                    connection: Some((self.connection_id.to_string(), self.appended.len())),
                });
                self.appended.push((
                    record.first_sequence(),
                    record.last_sequence(),
                    log_append_time_millis,
                ));
            }
            if self.recorder.chain().in_flight().is_none() {
                break;
            }
            self.recorder.acknowledge_publication();
        }
        while let Some((_, outcome)) = self.recorder.poll_publication_outcome() {
            assert!(
                matches!(outcome, PublishOutcome::Acknowledged),
                "every golden publication is acknowledged"
            );
        }
    }

    // ---------------------------------------------------------------- observations

    /// One client read.
    fn read(&mut self, data: &[u8]) {
        assert!(
            self.response.is_none(),
            "declare how the previous response completed before the next request starts"
        );
        let ts = self.now();
        self.request
            .get_or_insert_with(|| Pending::new(ts))
            .push(data);
        self.all_reads.extend_from_slice(data);
        self.recorder.record_read(ts, data).expect("read");
        self.step();
        self.settle();
    }

    /// Client reads of `data` cut into fragments of the given sizes, then the remainder.
    fn read_fragments(&mut self, data: &[u8], sizes: &[usize]) {
        let mut offset = 0;
        for size in sizes {
            self.read(&data[offset..offset + size]);
            offset += size;
        }
        assert!(offset < data.len(), "the fragment plan leaves a remainder");
        self.read(&data[offset..]);
    }

    /// The end of the request whose head is `head`, with the lengths the HTTP scanner reports.
    fn end_of_message(&mut self, head: &str) {
        let (first_line, headers) = end_of_message_lengths(head);
        let ts = self.now();
        self.recorder.set_end_of_first_line(first_line);
        self.recorder.set_end_of_headers(headers);
        let sequence = self.recorder.commit_end_of_message(ts).expect("EOM");
        let request = self
            .request
            .take()
            .expect("an end of message ends a request");
        self.unresolved_requests.push((self.events.len(), sequence));
        self.events.push(Event::Request {
            ordinal: self.ordinal,
            message: MessageBytes::of(&request.bytes, request.packets),
            first_byte_ts: request.first_ts.into(),
            end_of_message_ts: ts.into(),
            completing_log_append_time_millis: 0,
        });
        // An interim response that arrived while the request was still being read belongs to it,
        // and is delivered once the request exists.
        for interim in std::mem::take(&mut self.pending_interims) {
            self.events.push(Event::Interim {
                ordinal: self.ordinal,
                message: MessageBytes::of(&interim.bytes, interim.packets),
            });
        }
        self.response = Some(PendingResponse {
            ordinal: self.ordinal,
            message: None,
            forwarded_ts: None,
        });
        self.ordinal += 1;
        self.any_request_reconstituted = true;
        self.step();
        self.settle();
    }

    /// The Critical Mutation Traffic flush: the completed request is published before forwarding.
    fn critical_flush(&mut self) {
        let ticket = self.recorder.flush().expect("flush");
        assert!(
            ticket.is_some(),
            "a completed request leaves a record to flush"
        );
        self.settle();
    }

    fn forwarded(&mut self) {
        let ts = self.now();
        self.recorder
            .record_request_forwarded(ts)
            .expect("forwarded");
        self.response
            .as_mut()
            .expect("a forward follows a captured end of message")
            .forwarded_ts = Some(ts);
        self.step();
        self.settle();
    }

    /// One write of source response bytes to the client.
    fn write(&mut self, data: &[u8]) {
        let ts = self.now();
        let response = self.response.as_mut().expect("a write answers a request");
        response
            .message
            .get_or_insert_with(|| Pending::new(ts))
            .push(data);
        self.all_writes.extend_from_slice(data);
        self.recorder.record_write(ts, data).expect("write");
        self.step();
        self.settle();
    }

    /// A complete non-switching 1xx header block.
    fn interim_response(&mut self, data: &[u8]) {
        let ts = self.now();
        let mut interim = Pending::new(ts);
        interim.push(data);
        if self.request.is_some() {
            self.pending_interims.push(interim);
        } else {
            let ordinal = self
                .response
                .as_ref()
                .expect("an interim answers a request")
                .ordinal;
            self.events.push(Event::Interim {
                ordinal,
                message: MessageBytes::of(&interim.bytes, interim.packets),
            });
        }
        self.all_interims.extend_from_slice(data);
        self.recorder
            .record_interim_response(ts, data)
            .expect("interim response");
        self.step();
        self.settle();
    }

    /// Declares how the current response ended. Records nothing; it is the expectation.
    fn complete_response(&mut self, evidence: Evidence) {
        let response = self.response.take().expect("a response to complete");
        let message = response.message.expect("the response carried bytes");
        self.events.push(Event::Response {
            ordinal: response.ordinal,
            message: MessageBytes::of(&message.bytes, message.packets),
            evidence: evidence.java_name(),
            request_forwarded_ts: response.forwarded_ts.map(Ts::from),
        });
    }

    /// Capture policy suppressed the request after a prefix of it was captured.
    fn request_dropped(&mut self) {
        let ts = self.now();
        self.request
            .take()
            .expect("a dropped request has a captured prefix");
        self.pending_interims.clear();
        self.recorder
            .cancel_capture_for_current_request(ts)
            .expect("request dropped");
        self.ordinal += 1;
        self.step();
        self.settle();
    }

    fn connection_exception(&mut self, message: &str) {
        let ts = self.now();
        self.recorder
            .record_connection_exception(ts, message)
            .expect("connection exception");
        self.step();
        self.settle();
    }

    /// Lets the open record's fixed `F` deadline arrive and runs the owner's scheduled flush.
    fn scheduled_flush_at_deadline(&mut self) {
        let deadline = self.recorder.flush_deadline().expect("an open record");
        let wait = self.recorder.time_until_flush_deadline().expect("armed");
        self.clock.advance(wait);
        match self
            .recorder
            .run_scheduled_flush(deadline.generation)
            .expect("scheduled flush")
        {
            ScheduledFlush::Detached { lateness, .. } => {
                assert_eq!(lateness, Duration::ZERO, "the callback runs on time");
            }
            other => panic!("the deadline must detach the open record, got {other:?}"),
        }
        self.settle();
    }

    fn close(&mut self) {
        assert!(
            self.response.is_none(),
            "declare how the last response completed before the close"
        );
        let ts = self.now();
        self.recorder.close(ts).expect("close");
        if self.any_request_reconstituted {
            // A request-less lifetime has no connection owner to tell.
            self.events.push(Event::Closed {
                ordinal: self.ordinal,
                ts: ts.into(),
            });
        }
        self.step();
        self.settle();
    }

    /// Declares that replay may also begin at this connection's record `position`, after which the
    /// first request the replayer can reconstruct is `first_ordinal`.
    fn resume_from(&mut self, position: usize, first_ordinal: u64) {
        self.resume = Some((position, first_ordinal));
    }

    fn finish(mut self) -> FinishedConnection {
        assert!(
            self.recorder.is_terminated(),
            "every golden connection closes"
        );
        assert!(self.recorder.chain().in_flight().is_none());
        for (event_index, sequence) in std::mem::take(&mut self.unresolved_requests) {
            let log_append_time = self
                .appended
                .iter()
                .find(|(first, last, _)| (*first..=*last).contains(&sequence))
                .map(|(_, _, lat)| *lat)
                .expect("the end-of-message observation was published");
            if let Event::Request {
                completing_log_append_time_millis,
                ..
            } = &mut self.events[event_index]
            {
                *completing_log_append_time_millis = log_append_time;
            }
        }
        FinishedConnection {
            connection_id: self.connection_id.to_string(),
            partition: self.partition,
            payload_limit: self.payload_limit,
            record_count: self.appended.len(),
            events: self.events,
            resume: self.resume,
            all_reads: self.all_reads,
            all_writes: self.all_writes,
            all_interims: self.all_interims,
        }
    }
}

struct FinishedConnection {
    connection_id: String,
    partition: i32,
    payload_limit: Option<u32>,
    record_count: usize,
    events: Vec<Event>,
    resume: Option<(usize, u64)>,
    all_reads: Vec<u8>,
    all_writes: Vec<u8>,
    all_interims: Vec<u8>,
}

impl FinishedConnection {
    fn expected(&self, log: &[Appended], offsets: &[i64]) -> ExpectedConnection {
        let resume = self.resume.map(|(position, first_ordinal)| {
            let index = log
                .iter()
                .position(|appended| {
                    appended
                        .connection
                        .as_ref()
                        .is_some_and(|(id, at)| *id == self.connection_id && *at == position)
                })
                .expect("the resume record exists");
            Resume {
                from_offset: offsets[index],
                events: self
                    .events
                    .iter()
                    .filter(|event| {
                        matches!(event, Event::Closed { .. }) || event.ordinal() >= first_ordinal
                    })
                    .cloned()
                    .collect(),
            }
        });
        ExpectedConnection {
            connection_id: self.connection_id.clone(),
            node_id: NODE_ID,
            partition: self.partition,
            events: self.events.clone(),
            resume,
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Decoding (prost) and the generator's own checks
// ------------------------------------------------------------------------------------------------

fn decode_record(value: &[u8]) -> DecodedRecord {
    let envelope = CaptureRecord::decode(value).expect("every value is a CaptureRecord");
    match envelope.payload.expect("the payload oneof is set") {
        Payload::TrafficStream(stream) => {
            let index = match stream.index.expect("the index oneof is set") {
                Index::Number(value) => DecodedIndex {
                    field: "number",
                    value,
                },
                Index::NumberOfThisLastChunk(value) => DecodedIndex {
                    field: "numberOfThisLastChunk",
                    value,
                },
            };
            DecodedRecord::TrafficStream {
                observations: stream.sub_stream.iter().map(decode_observation).collect(),
                connection_id: stream.connection_id,
                node_id: stream.node_id,
                index,
                prior_requests_received: stream.prior_requests_received,
                last_observation_was_unterminated_read: stream
                    .last_observation_was_unterminated_read,
            }
        }
        Payload::WriterPartitionHeartbeat(heartbeat) => DecodedRecord::WriterPartitionHeartbeat {
            writer_node_id: heartbeat.writer_node_id,
            heartbeat_interval_millis: heartbeat.heartbeat_interval_millis,
            emitted_at_millis: heartbeat.emitted_at_millis,
        },
        Payload::CaptureCapabilityProbe(probe) => DecodedRecord::CaptureCapabilityProbe {
            writer_node_id: probe.writer_node_id,
            probe_id: probe.probe_id,
        },
    }
}

/// The proto field name of an observation's `Capture` arm and its data length, if it carries data.
fn capture_kind(capture: &Capture) -> (&'static str, Option<usize>) {
    match capture {
        Capture::Bind(_) => ("bind", None),
        Capture::Connect(_) => ("connect", None),
        Capture::Read(read) => ("read", Some(read.data.len())),
        Capture::ReadSegment(segment) => ("readSegment", Some(segment.data.len())),
        Capture::RequestForwarded(_) => ("requestForwarded", None),
        Capture::Write(write) => ("write", Some(write.data.len())),
        Capture::WriteSegment(segment) => ("writeSegment", Some(segment.data.len())),
        Capture::Disconnect(_) => ("disconnect", None),
        Capture::Close(_) => ("close", None),
        Capture::ConnectionException(_) => ("connectionException", None),
        Capture::SegmentEnd(_) => ("segmentEnd", None),
        Capture::EndOfMessageIndicator(_) => ("endOfMessageIndicator", None),
        Capture::RequestDropped(_) => ("requestDropped", None),
        Capture::InterimResponse(interim) => ("interimResponse", Some(interim.data.len())),
        Capture::InterimResponseSegment(segment) => {
            ("interimResponseSegment", Some(segment.data.len()))
        }
    }
}

fn decode_observation(observation: &TrafficObservation) -> DecodedObservation {
    let ts = observation.ts.as_ref().expect("every observation has a ts");
    let capture = observation
        .capture
        .as_ref()
        .expect("the capture oneof is set");
    let (kind, data_length) = capture_kind(capture);
    let mut decoded = DecodedObservation {
        sequence: observation
            .connection_observation_sequence
            .expect("every observation has a sequence"),
        ts: Ts {
            seconds: ts.seconds,
            nanos: ts.nanos,
        },
        capture: kind,
        data_length,
        first_line_byte_length: None,
        headers_byte_length: None,
        message: None,
    };
    match capture {
        Capture::EndOfMessageIndicator(eom) => {
            decoded.first_line_byte_length = eom.first_line_byte_length;
            decoded.headers_byte_length = eom.headers_byte_length;
        }
        Capture::ConnectionException(exception) => {
            decoded.message = Some(exception.message.clone());
        }
        _ => {}
    }
    decoded
}

fn traffic_stream(value: &[u8]) -> TrafficStream {
    match CaptureRecord::decode(value).expect("decodes").payload {
        Some(Payload::TrafficStream(stream)) => stream,
        other => panic!("a traffic record carries a trafficStream, got {other:?}"),
    }
}

/// The wire-level obligations, checked on `prost`'s reading of the bytes before any golden is
/// written: identity, routing, index and sequence contiguity, the terminal close, and byte
/// exactness of every captured read, write and interim response against the scenario inputs.
fn verify_connection(connection: &FinishedConnection, log: &[Appended]) {
    let id = &connection.connection_id;
    let mine: Vec<&Appended> = log
        .iter()
        .filter(|appended| appended.connection.as_ref().is_some_and(|(c, _)| c == id))
        .collect();
    assert_eq!(mine.len(), connection.record_count, "{id}: record count");

    let mut next_sequence = 1;
    let (mut reads, mut writes, mut interims) = (Vec::new(), Vec::new(), Vec::new());
    for (position, appended) in mine.iter().enumerate() {
        assert_eq!(&appended.key, id, "the traffic key is the connection id");
        assert_eq!(appended.partition, select_partition(id, &PARTITIONS));
        if let Some(limit) = connection.payload_limit {
            let limit = usize::try_from(limit).expect("fits");
            assert!(appended.value.len() <= limit, "{id}: payload limit");
        }
        let stream = traffic_stream(&appended.value);
        assert_eq!(&stream.connection_id, id);
        assert_eq!(stream.node_id, NODE_ID);
        let expected_index = i32::try_from(position + 1).expect("small");
        let is_last = position + 1 == mine.len();
        assert_eq!(
            stream.index,
            Some(if is_last {
                Index::NumberOfThisLastChunk(expected_index)
            } else {
                Index::Number(expected_index)
            }),
            "{id}: contiguous index with numberOfThisLastChunk on the final record only"
        );
        assert!(!stream.sub_stream.is_empty(), "no empty TrafficStream");
        for (at, observation) in stream.sub_stream.iter().enumerate() {
            assert_eq!(
                observation.connection_observation_sequence,
                Some(next_sequence),
                "{id}: contiguous connectionObservationSequence"
            );
            next_sequence += 1;
            let capture = observation.capture.as_ref().expect("capture set");
            let terminal = is_last && at + 1 == stream.sub_stream.len();
            assert_eq!(
                matches!(capture, Capture::Close(_)),
                terminal,
                "{id}: CloseObservation is exactly the last observation of the last record"
            );
            match capture {
                Capture::Read(read) => reads.extend_from_slice(&read.data),
                Capture::ReadSegment(segment) => reads.extend_from_slice(&segment.data),
                Capture::Write(write) => writes.extend_from_slice(&write.data),
                Capture::WriteSegment(segment) => writes.extend_from_slice(&segment.data),
                Capture::InterimResponse(interim) => interims.extend_from_slice(&interim.data),
                Capture::InterimResponseSegment(segment) => {
                    interims.extend_from_slice(&segment.data);
                }
                _ => {}
            }
        }
    }
    assert!(reads == connection.all_reads, "{id}: read bytes are exact");
    assert!(
        writes == connection.all_writes,
        "{id}: write bytes are exact"
    );
    assert!(
        interims == connection.all_interims,
        "{id}: interim bytes are exact"
    );
}

// ------------------------------------------------------------------------------------------------
// HTTP inputs
// ------------------------------------------------------------------------------------------------

/// `EndOfMessageIndication` lengths as the HTTP scanner reports them: the request line without its
/// CR LF, and the sum of header lines without their CR LFs or the blank line.
fn end_of_message_lengths(head: &str) -> (i32, i32) {
    let mut lines = head.split("\r\n");
    let first = lines.next().expect("a request line").len();
    let headers: usize = lines.map(str::len).sum();
    (
        i32::try_from(first).expect("small"),
        i32::try_from(headers).expect("small"),
    )
}

/// Request framing.
enum Body<'a> {
    Length(&'a str),
    Chunked {
        chunks: &'a [&'a str],
        trailers: &'a [&'a str],
    },
}

/// A request's head and body bytes, in the shape of the wire-proof corpus.
fn request(
    method: &str,
    uri: &str,
    tag: &str,
    extra_headers: &[&str],
    body: &Body<'_>,
) -> (String, String) {
    let mut head =
        format!("{method} {uri} HTTP/1.1\r\nHost: golden.invalid\r\nX-Golden: {tag}\r\n");
    for header in COMMON_HEADERS.iter().chain(extra_headers) {
        head.push_str(header);
        head.push_str("\r\n");
    }
    let body = match body {
        Body::Length(content) => {
            write!(head, "Content-Length: {}\r\n", content.len()).expect("writing to a String");
            String::from(*content)
        }
        Body::Chunked { chunks, trailers } => {
            head.push_str("Transfer-Encoding: chunked\r\n");
            if !trailers.is_empty() {
                let names: Vec<&str> = trailers
                    .iter()
                    .map(|trailer| trailer.split(':').next().expect("a trailer name"))
                    .collect();
                write!(head, "Trailer: {}\r\n", names.join(", ")).expect("writing to a String");
            }
            let mut encoded = String::new();
            for chunk in *chunks {
                write!(encoded, "{:x}\r\n{chunk}\r\n", chunk.len()).expect("writing to a String");
            }
            encoded.push_str("0\r\n");
            for trailer in *trailers {
                encoded.push_str(trailer);
                encoded.push_str("\r\n");
            }
            encoded.push_str("\r\n");
            encoded
        }
    };
    head.push_str("\r\n");
    (head, body)
}

/// A deterministic NDJSON-ish body of exactly `length` bytes.
fn bulk_body(length: usize) -> String {
    let mut body = String::from("{\"index\":{\"_index\":\"golden\",\"_id\":\"");
    let mut digit = 0u8;
    while body.len() < length - 2 {
        body.push(char::from(b'0' + digit));
        digit = (digit + 1) % 10;
    }
    body.truncate(length - 2);
    body.push_str("\"}");
    assert_eq!(body.len(), length);
    body
}

/// A deterministic source response of exactly `body_length` body bytes, framed by Content-Length.
fn json_response(tag: &str, body_length: usize) -> Vec<u8> {
    let mut body = format!("{{\"tag\":\"{tag}\",\"pad\":\"");
    while body.len() < body_length - 2 {
        body.push('x');
    }
    body.push_str("\"}");
    assert_eq!(body.len(), body_length);
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Golden: {tag}\r\n\
         Content-Length: {body_length}\r\n\r\n{body}"
    )
    .into_bytes()
}

const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

// ------------------------------------------------------------------------------------------------
// Scenarios
// ------------------------------------------------------------------------------------------------

fn keep_alive_gets() -> Golden {
    let mut scenario = Scenario::new(
        "keep_alive_gets",
        "Three GETs on one keep-alive connection, each request fragmented across several reads. \
         No GET is a critical mutation, so the whole connection is one terminal record.",
    );
    let mut c = scenario.connection(1, None);
    let fragments: [&[usize]; 3] = [&[28, 391], &[89, 48, 13, 40, 25], &[23, 91, 27, 53]];
    for (k, plan) in fragments.iter().enumerate() {
        let tag = format!("c0/{k}");
        let extra: &[&str] = if k == 0 { RICH_HEADERS } else { &[] };
        let uri = format!("/c0/doc{k}?pretty=true");
        let (head, _) = request("GET", &uri, &tag, extra, &Body::Length(""));
        c.read_fragments(head.as_bytes(), plan);
        c.end_of_message(&head);
        c.forwarded();
        c.write(&json_response(&tag, 280));
        c.complete_response(Evidence::HttpFraming);
    }
    c.close();
    scenario.add(c);
    scenario.build()
}

fn critical_mutations() -> Golden {
    let mut scenario = Scenario::new(
        "critical_mutations",
        "POST, PUT, DELETE and PATCH on one connection. Each is Critical Mutation Traffic, so the \
         record holding its end of message is flushed before it is forwarded, and the next record \
         opens with its requestForwarded. Replay may also start at the third record, whose \
         priorRequestsReceived of 2 must make the DELETE ordinal 2.",
    );
    let mut c = scenario.connection(2, None);
    let bulk = bulk_body(512);
    let doc = bulk_body(128);
    let patch = bulk_body(64);
    let json: &[&str] = &["Content-Type: application/json"];
    let mutations = [
        Mutation {
            method: "POST",
            uri: "/c1/_bulk",
            body: &bulk,
            extra_headers: json,
            head_plan: &[192, 30],
            body_plan: &[292],
        },
        Mutation {
            method: "PUT",
            uri: "/c1/idx/_doc/1",
            body: &doc,
            extra_headers: json,
            head_plan: &[45, 107, 35, 18, 2],
            body_plan: &[110],
        },
        Mutation {
            method: "DELETE",
            uri: "/c1/idx/_doc/1",
            body: "",
            extra_headers: &[],
            head_plan: &[105],
            body_plan: &[],
        },
        Mutation {
            method: "PATCH",
            uri: "/c1/idx/_mapping",
            body: &patch,
            extra_headers: json,
            head_plan: &[],
            body_plan: &[14, 2, 6, 3, 19],
        },
    ];
    for (k, mutation) in mutations.iter().enumerate() {
        let tag = format!("c1/{k}");
        let (head, body) = request(
            mutation.method,
            mutation.uri,
            &tag,
            mutation.extra_headers,
            &Body::Length(mutation.body),
        );
        read_planned(&mut c, head.as_bytes(), mutation.head_plan);
        read_planned(&mut c, body.as_bytes(), mutation.body_plan);
        c.end_of_message(&head);
        c.critical_flush();
        c.forwarded();
        c.write(&json_response(&tag, 280));
        c.complete_response(Evidence::HttpFraming);
    }
    c.close();
    c.resume_from(2, 2);
    scenario.add(c);
    scenario.build()
}

/// One fixed-length mutating request and how its head and body arrive.
struct Mutation<'a> {
    method: &'a str,
    uri: &'a str,
    body: &'a str,
    extra_headers: &'a [&'a str],
    head_plan: &'a [usize],
    body_plan: &'a [usize],
}

/// Reads `data` as planned fragments; an empty plan is one whole read, and empty data no read.
fn read_planned(c: &mut ConnectionDriver, data: &[u8], plan: &[usize]) {
    if data.is_empty() {
        return;
    }
    if plan.is_empty() {
        c.read(data);
    } else {
        c.read_fragments(data, plan);
    }
}

fn chunked_with_trailers() -> Golden {
    let mut scenario = Scenario::new(
        "chunked_with_trailers",
        "A chunked POST of three chunks, a terminal chunk and two announced trailers. The request \
         bytes, including the chunk framing and trailers, must reconstruct exactly.",
    );
    let mut c = scenario.connection(3, None);
    let chunks = [bulk_body(96), bulk_body(32), bulk_body(200)];
    let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
    let (head, body) = request(
        "POST",
        "/c2/_bulk",
        "c2/0",
        &["Content-Type: application/x-ndjson"],
        &Body::Chunked {
            chunks: &chunk_refs,
            trailers: &["X-Checksum: deadbeef", "X-Trailer-Two: 42"],
        },
    );
    c.read_fragments(head.as_bytes(), &[70, 30, 34]);
    c.read_fragments(body.as_bytes(), &[11]);
    c.end_of_message(&head);
    c.critical_flush();
    c.forwarded();
    c.write(&json_response("c2/0", 280));
    c.complete_response(Evidence::HttpFraming);
    c.close();
    scenario.add(c);
    scenario.build()
}

fn expect_continue_interim() -> Golden {
    let mut scenario = Scenario::new(
        "expect_continue_interim",
        "A PUT with Expect: 100-continue. The 100 Continue arrives after the headers and before \
         the body, is captured as an interimResponse distinct from the final write, and is \
         delivered for the request once the request is reconstituted.",
    );
    let mut c = scenario.connection(4, None);
    let doc = bulk_body(256);
    let (head, body) = request(
        "PUT",
        "/c3/idx/_doc/9",
        "c3/0",
        &["Content-Type: application/json", "Expect: 100-continue"],
        &Body::Length(&doc),
    );
    c.read_fragments(head.as_bytes(), &[head.len() - 1]);
    c.interim_response(CONTINUE);
    c.read(body.as_bytes());
    c.end_of_message(&head);
    c.critical_flush();
    c.forwarded();
    c.write(&json_response("c3/0", 280));
    c.complete_response(Evidence::HttpFraming);
    c.close();
    scenario.add(c);
    scenario.build()
}

fn suppressed_request_dropped() -> Golden {
    let mut scenario = Scenario::new(
        "suppressed_request_dropped",
        "A request whose capture policy suppresses it, between two captured GETs. Two reads of its \
         prefix were captured before the read completing its headers decided suppression, so the \
         stream carries requestDropped; the replayer discards the prefix and the suppressed \
         request still consumes ordinal 1.",
    );
    let mut c = scenario.connection(5, None);
    let (before, _) = request("GET", "/c4/before", "c4/0", &[], &Body::Length(""));
    c.read(before.as_bytes());
    c.end_of_message(&before);
    c.forwarded();
    c.write(&json_response("c4/0", 280));
    c.complete_response(Evidence::HttpFraming);

    let (suppressed, _) = request("GET", "/_suppressed/health", "c4/1", &[], &Body::Length(""));
    c.read(&suppressed.as_bytes()[..124]);
    c.read(&suppressed.as_bytes()[124..169]);
    // The read holding the rest of the headers is the one that decided suppression; it is not
    // captured, and neither is the suppressed request's response.
    c.request_dropped();

    let (after, _) = request("GET", "/c4/after", "c4/2", &[], &Body::Length(""));
    c.read_fragments(after.as_bytes(), &[4, 16, 130, 43, 5]);
    c.end_of_message(&after);
    c.forwarded();
    c.write(&json_response("c4/2", 280));
    c.complete_response(Evidence::HttpFraming);
    c.close();
    scenario.add(c);
    scenario.build()
}

fn segmented_large_response() -> Golden {
    let mut scenario = Scenario::new(
        "segmented_large_response",
        "A GET whose 6 KiB response cannot fit one record under a 1024-byte payload limit, so it \
         is written as writeSegment observations spread over several records and ended by \
         segmentEnd. The replayer must join the segments into one packet of exact bytes.",
    );
    let mut c = scenario.connection(6, Some(SMALL_PAYLOAD_LIMIT));
    let (head, _) = request("GET", "/c5/large", "c5/0", &[], &Body::Length(""));
    c.read(head.as_bytes());
    c.end_of_message(&head);
    c.forwarded();
    c.write(&json_response("c5/0", 6 * 1024));
    c.complete_response(Evidence::HttpFraming);
    c.close();
    scenario.add(c);
    scenario.build()
}

fn segmented_large_request() -> Golden {
    let mut scenario = Scenario::new(
        "segmented_large_request",
        "A PUT whose 3000-byte body read cannot fit one record under a 1024-byte payload limit, so \
         it is captured as readSegment observations across records, ended by segmentEnd, followed \
         by the end of message and the critical flush.",
    );
    let mut c = scenario.connection(7, Some(SMALL_PAYLOAD_LIMIT));
    let doc = bulk_body(3000);
    let (head, body) = request(
        "PUT",
        "/c6/idx/_doc/big",
        "c6/0",
        &["Content-Type: application/json"],
        &Body::Length(&doc),
    );
    c.read(head.as_bytes());
    c.read(body.as_bytes());
    c.end_of_message(&head);
    c.critical_flush();
    c.forwarded();
    c.write(&json_response("c6/0", 200));
    c.complete_response(Evidence::HttpFraming);
    c.close();
    scenario.add(c);
    scenario.build()
}

fn connection_exception() -> Golden {
    let mut scenario = Scenario::new(
        "connection_exception",
        "The source fails part-way through a Content-Length response. The truncated bytes are \
         captured, then connectionException, then close. The exception ends the response with \
         completion unproven and is not itself terminal.",
    );
    let mut c = scenario.connection(8, None);
    let (head, _) = request("GET", "/c7/doc", "c7/0", &[], &Body::Length(""));
    c.read(head.as_bytes());
    c.end_of_message(&head);
    c.forwarded();
    let full = json_response("c7/0", 280);
    c.write(&full[..150]);
    c.complete_response(Evidence::ConnectionException);
    c.connection_exception("Connection reset by peer");
    c.close();
    scenario.add(c);
    scenario.build()
}

fn close_terminal_record() -> Golden {
    let mut scenario = Scenario::new(
        "close_terminal_record",
        "A response delimited only by the connection close. The F deadline detaches the record \
         holding it, so the terminal record carries a single CloseObservation with \
         numberOfThisLastChunk, and that close completes the response unproven. A second \
         connection is accepted and closed with no traffic, as a readiness probe is: one record, \
         one close, and no transaction.",
    );
    let mut c = scenario.connection(9, None);
    let (head, _) = request("GET", "/c8/stream", "c8/0", &[], &Body::Length(""));
    c.read(head.as_bytes());
    c.end_of_message(&head);
    c.forwarded();
    c.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n");
    c.write(b"streamed until the source closes the connection\n");
    c.scheduled_flush_at_deadline();
    c.complete_response(Evidence::ConnectionClose);
    c.close();
    scenario.add(c);

    let mut probe = scenario.connection(10, None);
    probe.close();
    scenario.add(probe);
    scenario.build()
}

fn periodic_flush() -> Golden {
    let mut scenario = Scenario::new(
        "periodic_flush",
        "Three GETs. The fixed F deadline of the first record arrives after the first fragment of \
         the second request, so the scheduled flush splits that request across two records. The \
         second record opens with priorRequestsReceived 1 and lastObservationWasUnterminatedRead \
         true; replay starting there discards the inherited tail and reconstructs the third \
         request as ordinal 2.",
    );
    let mut c = scenario.connection(11, None);
    for k in 0..3 {
        let tag = format!("c9/{k}");
        let uri = format!("/c9/doc{k}");
        let (head, _) = request("GET", &uri, &tag, &[], &Body::Length(""));
        if k == 1 {
            c.read(&head.as_bytes()[..60]);
            c.scheduled_flush_at_deadline();
            c.read_fragments(&head.as_bytes()[60..], &[50]);
        } else {
            c.read(head.as_bytes());
        }
        c.end_of_message(&head);
        c.forwarded();
        c.write(&json_response(&tag, 280));
        c.complete_response(Evidence::HttpFraming);
    }
    c.close();
    c.resume_from(1, 2);
    scenario.add(c);
    scenario.build()
}

fn writer_partition_heartbeat() -> Golden {
    let mut scenario = Scenario::new(
        "writer_partition_heartbeat",
        "A routing generation's initial heartbeat on each of eight partitions, then a later \
         heartbeat on partition 0 with emittedAtMillis absent. Each is keyed \
         writerNodeId:heartbeat:partition and carries no partition in its payload.",
    );
    for partition in PARTITIONS {
        let emitted_at = wall_millis(&scenario.clock);
        scenario.heartbeat(partition, Some(emitted_at));
    }
    scenario.idle(Duration::from_millis(
        u64::try_from(HEARTBEAT_INTERVAL_MILLIS).expect("positive"),
    ));
    scenario.heartbeat(0, None);
    scenario.writer_time_transitions = Some(BTreeMap::from([
        ("EXACT_HEARTBEAT_STARTED", 8),
        ("TIMELY_HEARTBEAT_ACCEPTED", 1),
    ]));
    scenario.build()
}

fn capture_capability_probe() -> Golden {
    let mut scenario = Scenario::new(
        "capture_capability_probe",
        "The startup capability probe on a representative partition. Its writer identity is \
         captureActivationId:PROBE, which no routing generation can produce, and replay intake \
         creates no writer or connection state for it.",
    );
    scenario.probe(0);
    scenario.writer_time_transitions = Some(BTreeMap::new());
    scenario.build()
}

fn all_scenarios() -> Vec<Golden> {
    vec![
        keep_alive_gets(),
        critical_mutations(),
        chunked_with_trailers(),
        expect_continue_interim(),
        suppressed_request_dropped(),
        segmented_large_response(),
        segmented_large_request(),
        connection_exception(),
        close_terminal_record(),
        periodic_flush(),
        writer_partition_heartbeat(),
        capture_capability_probe(),
    ]
}

// ------------------------------------------------------------------------------------------------
// Files
// ------------------------------------------------------------------------------------------------

fn goldens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("goldens")
}

fn render(golden: &Golden) -> String {
    let mut rendered = serde_json::to_string_pretty(golden).expect("a golden serializes");
    rendered.push('\n');
    rendered
}

fn update_requested() -> bool {
    std::env::var_os(UPDATE_ENV).is_some_and(|value| value == "1")
}

/// Fails when any golden differs from what the encoder produces now, or when a file no scenario
/// produces is present. With `CAPTURE_GOLDENS_UPDATE=1` rewrites them instead.
#[test]
fn goldens_are_current() {
    let dir = goldens_dir();
    let update = update_requested();
    if update {
        fs::create_dir_all(&dir).expect("create the goldens directory");
    }
    let mut stale = Vec::new();
    let mut produced = BTreeSet::new();
    for golden in all_scenarios() {
        let file_name = format!("{}.json", golden.scenario);
        let path = dir.join(&file_name);
        let rendered = render(&golden);
        let current = fs::read_to_string(&path).ok();
        if current.as_deref() != Some(rendered.as_str()) {
            if update {
                fs::write(&path, &rendered).expect("write golden");
            } else {
                stale.push(file_name.clone());
            }
        }
        produced.insert(file_name);
    }
    // A missing directory has already made every golden stale above.
    for entry in fs::read_dir(&dir).into_iter().flatten() {
        let path = entry.expect("a directory entry").path();
        let name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_owned();
        if path.extension().is_some_and(|ext| ext == "json") && !produced.contains(&name) {
            if update {
                fs::remove_file(&path).expect("remove orphaned golden");
            } else {
                stale.push(format!("{name} (no scenario produces it)"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "stale golden records: {stale:?}. If the change is intended, regenerate with \
         `{UPDATE_ENV}=1 cargo test -p capture-test-support --test golden_records`, review the \
         diff, and run the Java RustGoldenRecordCompatibilityTest (see the module docs). A wire change needs an owner decision first."
    );
}

#[test]
fn generation_is_deterministic() {
    let first: Vec<String> = all_scenarios().iter().map(render).collect();
    let second: Vec<String> = all_scenarios().iter().map(render).collect();
    assert_eq!(first, second);
}

#[test]
fn every_required_scenario_is_generated() {
    let names: BTreeSet<&str> = all_scenarios()
        .iter()
        .map(|golden| golden.scenario)
        .collect();
    for required in REQUIRED_SCENARIOS {
        assert!(names.contains(required), "missing scenario {required}");
    }
    assert_eq!(names.len(), REQUIRED_SCENARIOS.len());
}

/// The structural facts each scenario exists to pin, so a refactor of the generator cannot quietly
/// stop producing the case it is named for.
#[test]
fn each_scenario_exercises_its_case() {
    let goldens: BTreeMap<&str, Golden> = all_scenarios()
        .into_iter()
        .map(|golden| (golden.scenario, golden))
        .collect();
    let captures = |name: &str| -> Vec<&'static str> {
        goldens[name]
            .expected
            .records
            .iter()
            .flat_map(|record| match record {
                DecodedRecord::TrafficStream { observations, .. } => {
                    observations.iter().map(|o| o.capture).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect()
    };
    let traffic_records = |name: &str| -> usize {
        goldens[name]
            .expected
            .records
            .iter()
            .filter(|record| matches!(record, DecodedRecord::TrafficStream { .. }))
            .count()
    };

    assert_eq!(traffic_records("keep_alive_gets"), 1);
    assert_eq!(traffic_records("critical_mutations"), 5);
    assert!(captures("expect_continue_interim").contains(&"interimResponse"));
    assert!(captures("suppressed_request_dropped").contains(&"requestDropped"));
    assert!(captures("segmented_large_response").contains(&"writeSegment"));
    assert!(traffic_records("segmented_large_response") > 2);
    assert!(captures("segmented_large_request").contains(&"readSegment"));
    assert!(captures("connection_exception").contains(&"connectionException"));
    assert_eq!(traffic_records("periodic_flush"), 2);
    assert!(
        goldens["writer_partition_heartbeat"]
            .expected
            .records
            .iter()
            .all(|record| { matches!(record, DecodedRecord::WriterPartitionHeartbeat { .. }) })
    );
    assert!(matches!(
        goldens["capture_capability_probe"]
            .expected
            .records
            .as_slice(),
        [DecodedRecord::CaptureCapabilityProbe { .. }]
    ));
    match goldens["close_terminal_record"].expected.records.as_slice() {
        [
            _,
            DecodedRecord::TrafficStream {
                index,
                observations,
                ..
            },
            ..,
        ] => {
            assert_eq!(index.field, "numberOfThisLastChunk");
            assert_eq!(
                observations.iter().map(|o| o.capture).collect::<Vec<_>>(),
                ["close"]
            );
        }
        other => panic!("close_terminal_record records: {other:?}"),
    }
}
