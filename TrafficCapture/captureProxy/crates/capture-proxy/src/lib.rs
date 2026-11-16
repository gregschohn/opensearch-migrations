//! The `capture-proxy` binary: process lifecycle, startup ordering, readiness, capture
//! process state, and shutdown.
//!
//! This crate is the composition root. Every other crate in the workspace is a finished component
//! with no knowledge of the others; this one wires them into a process:
//!
//! | Module | Row | Responsibility |
//! |---|---|---|
//! | [`app`] | PROXY-03, PROXY-11 | start-up ordering, the accept loop, signals, orderly shutdown |
//! | [`bind_address`] | — | the `--bind-address` parameter and the start-up exposure statement |
//! | [`capture_process_state`] | HTTP-02 | the one-way, process-wide capture gate |
//! | [`capture_sink`] | — | the capture boundary: `capture_transport::CaptureSink` implementations |
//! | [`connection_factory`] | PROXY-12 | per-connection stream acquisition and TLS selection |
//! | [`kafka_capture`] | KAFKA-08, WIRE-04 | Kafka readiness, per-connection capture, retirement |
//! | [`policy`] | HTTP-04, PROXY-09 | configuration to suppression predicate and header rewrite |
//! | [`process`] | SHARED-02, PROXY-01 | process identity and the hard process boundary |
//! | [`publication`] | — | the publication boundary the capture gate awaits on |
//! | [`shutdown`] | PROXY-03 | the orderly-shutdown timing policy and sequence |
//! | [`stream`] | — | TLS-or-plaintext stream wrappers for both sides |
//!
//! See [`app`] for the start-up order and the exit-status table, and [`bind_address`] for the
//! exposure decision the default bind represents.

pub mod app;
pub mod bind_address;
pub mod capture_process_state;
pub mod capture_sink;
pub mod connection_factory;
pub mod kafka_capture;
pub mod metered_sink;
pub mod policy;
pub mod process;
pub mod publication;
pub mod shutdown;
pub mod stream;

pub use capture_process_state::{CaptureFailure, CaptureProcessState, State};
pub use process::{
    CAPTURE_FAILURE_EXIT_CODE, CAPTURE_FAILURE_LOG_FLUSH_TIMEOUT, CaptureFailureTerminator,
    install_panic_hook, node_instance_name,
};
pub use shutdown::{
    CaptureRetirement, ConnectionSet, ORDERLY_NATURAL_DRAIN, ORDERLY_RETIREMENT_DEADLINE,
    ORDERLY_SHUTDOWN_HARD_STOP, ORDERLY_SHUTDOWN_HOOK_COMPLETION, ShutdownOutcome, ShutdownTiming,
    ShutdownTimingError, perform_orderly_shutdown,
};
