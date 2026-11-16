//! The record sink abstraction (traceability row WIRE-09, treatment *Semantic*).
//!
//! The sink is where a detached record leaves this crate. It is deliberately free of Kafka types:
//! `capture-kafka` implements it over a producer, lane, and routing state, and tests implement it
//! over a `Vec`. The four operations of Java's `StreamLifecycleManager` plus its publication
//! callbacks map onto this surface as:
//!
//! | Java | Here |
//! |---|---|
//! | `StreamLifecycleManager.createStream()` | [`RecordSink::create_buffer`] |
//! | detach (`flushCommitAndResetStream`) | [`RecordEncoder::finish`](crate::record_encoder::RecordEncoder::finish) |
//! | `closeStream` → `kickoffCloseStream` → `KafkaProducer.send` | [`RecordSink::submit`] |
//! | the producer callback completing the publication future | [`PublishChain::acknowledge`](crate::publish_chain::PublishChain::acknowledge) |
//! | the producer callback failing the publication future | [`PublishChain::fail`](crate::publish_chain::PublishChain::fail) |
//!
//! # Ownership
//!
//! [`RecordSink::submit`] takes the [`EncodedRecord`] **by value**, and ownership transfers
//! unconditionally: on `Err` the sink has already dropped the record, which releases its buffer
//! exactly once. That is the direct analogue of `OrderedStreamLifecyleManager` calling
//! `holder.close()` when `kickoffCloseStream` throws, with the difference that here it cannot be
//! forgotten.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::record_buffer::{BufferRecycler, EncodedRecord, RecordBuf};

/// A failure reported by a [`RecordSink`].
///
/// Intentionally opaque: the wire layer never interprets a sink failure, it only poisons the
/// connection's publication chain with it (`proxyCaptureProtocol.md` §4.2, §7).
#[derive(Debug)]
pub struct SinkError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl SinkError {
    /// A failure with only a message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// A failure wrapping an underlying cause.
    #[must_use]
    pub fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// The failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SinkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|s| &**s as &(dyn Error + 'static))
    }
}

/// Creates record buffers and takes ownership of detached records.
pub trait RecordSink {
    /// Allocates a buffer for a new record with room for at least `capacity` content bytes.
    ///
    /// A pooled implementation returns a [`RecordBuf`] built with
    /// [`RecordBuf::with_recycler`] so the allocation returns to the pool when the record is
    /// dropped.
    fn create_buffer(&mut self, capacity: usize) -> RecordBuf;

    /// Takes ownership of a detached record and begins its publication.
    ///
    /// Must not block the caller's event loop: the capture protocol forbids waiting for Kafka on the
    /// connection's thread (`proxyCaptureProtocol.md` §4.2).
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] if the record could not be handed to the transport at all. The record
    /// has been consumed and released either way.
    fn submit(&mut self, record: EncodedRecord) -> Result<(), SinkError>;
}

/// A sink that keeps every submitted record in memory. Intended for tests.
#[derive(Debug)]
pub struct CollectingSink {
    records: Vec<EncodedRecord>,
    recycler: Arc<dyn BufferRecycler>,
    fail_next: Option<SinkError>,
    submit_calls: usize,
}

impl CollectingSink {
    /// A sink whose buffers are plain heap allocations.
    #[must_use]
    pub fn new() -> Self {
        Self::with_recycler(Arc::new(crate::record_buffer::HeapRecycler))
    }

    /// A sink whose buffers are drawn from `recycler`, so a test can assert release-exactly-once.
    #[must_use]
    pub fn with_recycler(recycler: Arc<dyn BufferRecycler>) -> Self {
        Self {
            records: Vec::new(),
            recycler,
            fail_next: None,
            submit_calls: 0,
        }
    }

    /// Makes the next [`RecordSink::submit`] fail with `error`.
    pub fn fail_next_submit(&mut self, error: SinkError) {
        self.fail_next = Some(error);
    }

    /// The records submitted so far, in submission order.
    #[must_use]
    pub fn records(&self) -> &[EncodedRecord] {
        &self.records
    }

    /// How many times [`RecordSink::submit`] has been called, successfully or not.
    ///
    /// The analogue of the Java `OrderedStreamLifecyleManagerTest` `kickoffCalls` counter.
    #[must_use]
    pub fn submit_calls(&self) -> usize {
        self.submit_calls
    }

    /// Drops every retained record, releasing their buffers.
    pub fn clear(&mut self) {
        self.records.clear();
    }
}

impl Default for CollectingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordSink for CollectingSink {
    fn create_buffer(&mut self, capacity: usize) -> RecordBuf {
        RecordBuf::with_recycler(capacity, Arc::clone(&self.recycler))
    }

    fn submit(&mut self, record: EncodedRecord) -> Result<(), SinkError> {
        self.submit_calls += 1;
        if let Some(error) = self.fail_next.take() {
            // Ownership transferred: dropping here releases the buffer exactly once.
            drop(record);
            return Err(error);
        }
        self.records.push(record);
        Ok(())
    }
}
