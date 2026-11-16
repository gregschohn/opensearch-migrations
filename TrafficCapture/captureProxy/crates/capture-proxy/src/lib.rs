//! The `capture-proxy` binary: process lifecycle, startup ordering, readiness, capture
//! process state, and shutdown.

pub mod capture_process_state;

pub use capture_process_state::{CaptureFailure, CaptureProcessState, State};
