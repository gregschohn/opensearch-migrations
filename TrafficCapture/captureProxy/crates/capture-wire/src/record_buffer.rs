//! RAII-owned record buffers (traceability row WIRE-01, treatment *Retired*).
//!
//! # What this module retires
//!
//! Java models a record under construction as a `CodedOutputStreamHolder` whose `close()` releases a
//! **pooled** composite `ByteBuf` exactly once via a `released` flag
//! (`KafkaCaptureFactory`:1209-1215). Release is therefore a manual obligation discharged at six
//! different sites, and the wire spec records one site where it is *not* discharged: defect J-3, the
//! pass-through teardown path, leaks the open record's pooled buffer for every affected connection
//! because `IChannelConnectionCaptureSerializer` exposes no `close()`/`abort()` at all.
//!
//! Here, release is ownership. [`RecordBuf`] owns its bytes and returns them to its [`BufferRecycler`]
//! in `Drop`, so:
//!
//! * a buffer is released exactly once on **success** (the sink takes the [`EncodedRecord`] and
//!   drops it after publishing);
//! * exactly once on **rejection** (a poisoned publish chain drops the queued record);
//! * exactly once on **failure** (encoding returns `Err` and consumes the record);
//! * and exactly once on **teardown without publication**, via
//!   [`ConnectionRecorder::abort`](crate::connection_recorder::ConnectionRecorder::abort), which is
//!   the J-3 fix.
//!
//! There is no code path in this crate that can drop a buffer without releasing it, because there is
//! no path that can drop a buffer without running its destructor.

use std::fmt;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes reserved at the front of every record buffer for the `CaptureRecord` envelope prefix.
///
/// The envelope is `tag(1, LEN)` plus `varint(len)`, so it never exceeds one tag byte plus five
/// varint bytes. Reserving the maximum up front lets the finished record be handed to the sink as
/// one contiguous allocation with no copy: the prefix is back-filled into the reserved area and the
/// payload slice simply starts later (wire spec §2).
pub const ENVELOPE_PREFIX_RESERVE: usize = 6;

/// Where a released [`RecordBuf`] returns its bytes.
///
/// The production sink can hand out pooled allocations and reclaim them here; the default
/// [`HeapRecycler`] simply drops them. Implementations must be cheap and must not panic: `recycle`
/// runs in a destructor.
pub trait BufferRecycler: fmt::Debug + Send + Sync {
    /// Takes back the bytes of a released buffer. Called exactly once per buffer.
    fn recycle(&self, bytes: Vec<u8>);

    /// Notes that a buffer bound to this recycler was allocated.
    ///
    /// Called exactly once per [`RecordBuf::with_recycler`], so an implementation that counts both
    /// hooks can assert release-exactly-once regardless of who allocated the buffer.
    fn note_allocated(&self) {}
}

/// A recycler that lets the allocator reclaim the bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeapRecycler;

impl BufferRecycler for HeapRecycler {
    fn recycle(&self, bytes: Vec<u8>) {
        drop(bytes);
    }
}

/// A recycler that counts allocations and releases. Intended for tests and leak diagnostics.
#[derive(Debug, Default)]
pub struct CountingRecycler {
    taken: AtomicUsize,
    released: AtomicUsize,
}

impl CountingRecycler {
    /// Creates a recycler with zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a buffer that will report its release to this recycler.
    pub fn take(self: &Arc<Self>, capacity: usize) -> RecordBuf {
        RecordBuf::with_recycler(capacity, Arc::clone(self) as Arc<dyn BufferRecycler>)
    }

    /// Number of buffers bound to this recycler, however they were allocated.
    #[must_use]
    pub fn taken(&self) -> usize {
        self.taken.load(Ordering::Relaxed)
    }

    /// Number of buffers released back. Must equal [`CountingRecycler::taken`] once every record is
    /// dropped, and must never exceed it.
    #[must_use]
    pub fn released(&self) -> usize {
        self.released.load(Ordering::Relaxed)
    }

    /// Buffers still outstanding.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.taken().saturating_sub(self.released())
    }
}

impl BufferRecycler for CountingRecycler {
    fn recycle(&self, bytes: Vec<u8>) {
        self.released.fetch_add(1, Ordering::Relaxed);
        drop(bytes);
    }

    fn note_allocated(&self) {
        self.taken.fetch_add(1, Ordering::Relaxed);
    }
}

/// An owned, append-only byte buffer for one capture record.
///
/// The first [`ENVELOPE_PREFIX_RESERVE`] bytes are reserved for the `CaptureRecord` envelope and are
/// not part of the appended content.
pub struct RecordBuf {
    bytes: Vec<u8>,
    recycler: Option<Arc<dyn BufferRecycler>>,
}

impl RecordBuf {
    /// Allocates a heap buffer with room for `capacity` content bytes plus the envelope reserve.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let mut bytes = Vec::with_capacity(capacity.saturating_add(ENVELOPE_PREFIX_RESERVE));
        bytes.resize(ENVELOPE_PREFIX_RESERVE, 0);
        Self {
            bytes,
            recycler: None,
        }
    }

    /// Allocates a buffer whose bytes return to `recycler` when it is dropped.
    #[must_use]
    pub fn with_recycler(capacity: usize, recycler: Arc<dyn BufferRecycler>) -> Self {
        recycler.note_allocated();
        let mut buf = Self::new(capacity);
        buf.recycler = Some(recycler);
        buf
    }

    /// Number of content bytes appended so far, excluding the envelope reserve.
    #[must_use]
    pub fn content_len(&self) -> usize {
        self.bytes.len() - ENVELOPE_PREFIX_RESERVE
    }

    /// Whether any content byte has been appended.
    #[must_use]
    pub fn is_content_empty(&self) -> bool {
        self.content_len() == 0
    }

    /// The content bytes appended so far.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.bytes[ENVELOPE_PREFIX_RESERVE..]
    }

    /// Appends one byte.
    pub fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    /// Appends a slice.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Appends `value` as a base-128 varint.
    pub fn push_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push(value.to_le_bytes()[0] | 0x80);
            value >>= 7;
        }
        self.bytes.push(value.to_le_bytes()[0]);
    }

    /// Appends a field tag.
    pub fn push_tag(&mut self, field_number: u32, wire_type: u32) {
        self.push_varint((u64::from(field_number) << 3) | u64::from(wire_type));
    }

    /// Back-fills the `CaptureRecord` envelope prefix and returns the offset the payload starts at.
    ///
    /// The prefix is written into the reserved head so the result is one contiguous allocation with
    /// no copy of the already-encoded `TrafficStream`.
    fn seal_envelope(&mut self, traffic_stream_tag: u32) -> usize {
        let len = self.content_len();
        let len_u64 = len as u64;
        let varint_bytes = crate::protobuf_size::varint_len(len_u64) as usize;
        let prefix_len = 1 + varint_bytes;
        debug_assert!(
            prefix_len <= ENVELOPE_PREFIX_RESERVE,
            "envelope prefix of {prefix_len} bytes exceeds the {ENVELOPE_PREFIX_RESERVE}-byte reserve"
        );
        let start = ENVELOPE_PREFIX_RESERVE - prefix_len;
        let tag = (u64::from(traffic_stream_tag) << 3)
            | u64::from(crate::protobuf_size::WIRE_TYPE_LENGTH_DELIMITED);
        self.bytes[start] = tag.to_le_bytes()[0];
        let mut at = start + 1;
        let mut value = len_u64;
        while value >= 0x80 {
            self.bytes[at] = value.to_le_bytes()[0] | 0x80;
            value >>= 7;
            at += 1;
        }
        self.bytes[at] = value.to_le_bytes()[0];
        start
    }
}

impl fmt::Debug for RecordBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordBuf")
            .field("content_len", &self.content_len())
            .field("capacity", &self.bytes.capacity())
            .field("recycled", &self.recycler.is_some())
            .finish()
    }
}

impl Drop for RecordBuf {
    fn drop(&mut self) {
        if let Some(recycler) = self.recycler.take() {
            recycler.recycle(mem::take(&mut self.bytes));
        }
    }
}

/// A finished record, detached from its connection and ready for a [`RecordSink`].
///
/// [`RecordSink`]: crate::record_sink::RecordSink
///
/// Owning this value owns the buffer. There is no `release`, `close`, or `free` to forget: dropping
/// it releases the buffer exactly once, whether it was published, rejected by a poisoned chain, or
/// discarded by a failing encode.
#[derive(Debug)]
pub struct EncodedRecord {
    buffer: RecordBuf,
    payload_start: usize,
    connection_id: Arc<str>,
    node_id: Arc<str>,
    index: u32,
    is_final: bool,
    observation_count: u32,
    first_sequence: u64,
    last_sequence: u64,
}

impl EncodedRecord {
    /// Seals `buffer` into a publishable record. Crate-internal: only the encoder may build one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn seal(
        mut buffer: RecordBuf,
        connection_id: Arc<str>,
        node_id: Arc<str>,
        index: u32,
        is_final: bool,
        observation_count: u32,
        first_sequence: u64,
        last_sequence: u64,
    ) -> Self {
        let payload_start =
            buffer.seal_envelope(crate::protobuf_size::field::CAPTURE_RECORD_TRAFFIC_STREAM);
        Self {
            buffer,
            payload_start,
            connection_id,
            node_id,
            index,
            is_final,
            observation_count,
            first_sequence,
            last_sequence,
        }
    }

    /// The complete Kafka record value: one canonical `CaptureRecord` with the `trafficStream`
    /// payload set.
    ///
    /// `CaptureRecord.payload` is the only record-type discriminator; the protocol uses no Kafka
    /// header as a second discriminator and no payload carries a partition
    /// (`proxyCaptureProtocol.md` §4.1).
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.buffer.bytes[self.payload_start..]
    }

    /// The already-encoded `TrafficStream` bytes, without the `CaptureRecord` envelope.
    #[must_use]
    pub fn traffic_stream_bytes(&self) -> &[u8] {
        self.buffer.content()
    }

    /// The connection this record belongs to. The Kafka record key, for partition locality.
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// The writer identity (`TrafficStream.nodeId`) stamped into this record.
    ///
    /// A publisher must cross-check this against its routing decision before sending: a mismatch
    /// means the process's capture state is corrupted (wire spec §2).
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The 1-based, contiguous per-connection record index carried by the `index` oneof.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Whether this record carries `numberOfThisLastChunk`, i.e. is the connection's final record.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }

    /// How many `TrafficObservation` entries this record carries. Never zero.
    #[must_use]
    pub fn observation_count(&self) -> u32 {
        self.observation_count
    }

    /// `connectionObservationSequence` of this record's first observation.
    #[must_use]
    pub fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    /// `connectionObservationSequence` of this record's last observation.
    #[must_use]
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Length in bytes of [`EncodedRecord::payload`].
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.buffer.bytes.len() - self.payload_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_through_the_buffer() {
        for value in [0u64, 1, 127, 128, 300, 16_383, 16_384, u64::from(u32::MAX)] {
            let mut buf = RecordBuf::new(16);
            buf.push_varint(value);
            assert_eq!(
                buf.content_len(),
                crate::protobuf_size::varint_len(value) as usize,
                "varint of {value} used an unexpected number of bytes"
            );
            let mut slice = buf.content();
            let decoded = prost::encoding::decode_varint(&mut slice).expect("decodes");
            assert_eq!(decoded, value);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn sealing_back_fills_the_envelope_without_copying_the_stream() {
        let mut buf = RecordBuf::new(8);
        buf.extend_from_slice(b"abc");
        let start = buf.seal_envelope(1);
        assert_eq!(start, ENVELOPE_PREFIX_RESERVE - 2);
        assert_eq!(&buf.bytes[start..], &[0x0A, 0x03, b'a', b'b', b'c']);
    }

    #[test]
    fn sealing_a_large_stream_uses_a_longer_prefix() {
        let mut buf = RecordBuf::new(200);
        buf.extend_from_slice(&[7u8; 200]);
        let start = buf.seal_envelope(1);
        // 200 needs a two-byte varint, so the prefix is three bytes.
        assert_eq!(start, ENVELOPE_PREFIX_RESERVE - 3);
        assert_eq!(buf.bytes[start], 0x0A);
        let mut slice = &buf.bytes[start + 1..];
        assert_eq!(
            prost::encoding::decode_varint(&mut slice).expect("decodes"),
            200
        );
    }

    #[test]
    fn a_dropped_buffer_is_released_exactly_once() {
        let recycler = Arc::new(CountingRecycler::new());
        {
            let mut buf = recycler.take(32);
            buf.extend_from_slice(b"payload");
            assert_eq!(recycler.released(), 0);
        }
        assert_eq!(recycler.taken(), 1);
        assert_eq!(recycler.released(), 1);
        assert_eq!(recycler.outstanding(), 0);
    }
}
