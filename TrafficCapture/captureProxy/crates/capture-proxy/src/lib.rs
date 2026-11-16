//! The `capture-proxy` binary: process lifecycle, startup ordering, readiness, capture
//! process state, and shutdown.

pub mod capture_process_state;
pub mod process;

pub use capture_process_state::{CaptureFailure, CaptureProcessState, State};
pub use process::{
    CAPTURE_FAILURE_EXIT_CODE, CAPTURE_FAILURE_LOG_FLUSH_TIMEOUT, CaptureFailureTerminator,
    install_panic_hook, node_instance_name,
};
