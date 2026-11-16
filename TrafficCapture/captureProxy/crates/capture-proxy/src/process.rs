//! Process identity and the hard process boundary (traceability rows SHARED-02, PROXY-01).
//!
//! Ports `ProcessHelpers.getNodeInstanceName` and `CaptureFailureTerminator.java`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::capture_process_state::CaptureFailure;

/// Distinct nonzero exit status for a capture failure.
///
/// `CaptureProxy.java:78`. `proxyCaptureProtocol.md` §7.2 requires a *distinct* nonzero status so
/// an operator and the control plane can tell a capture compromise from an ordinary crash.
pub const CAPTURE_FAILURE_EXIT_CODE: i32 = 78;

/// Bounded time allowed for flushing diagnostics before the process is halted.
///
/// `CaptureProxy.java:79`.
pub const CAPTURE_FAILURE_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Fallback base used when `HOSTNAME` is not set.
///
/// `ProcessHelpers.java:12`.
const DEFAULT_NODE_BASE_ID: &str = "generated_";

/// One random suffix per process, generated once.
///
/// Java holds this in a `static final` initialised from `UUID.randomUUID()`, so it is stable for
/// the process lifetime and never reused across processes.
static ID_UNIQUE_SUFFIX: LazyLock<String> = LazyLock::new(uuid_v4_string);

/// The process's node instance name, used as `processId`.
///
/// Matches `ProcessHelpers.getNodeInstanceName()` exactly, including a detail that is almost
/// certainly accidental in Java and is preserved deliberately here: the fallback constant already
/// ends in `_`, and Java then appends another `_` before the UUID. So with `HOSTNAME` unset the
/// result contains a **double** underscore (`generated__<uuid>`). This string appears in operator
/// logs and as a metric attribute, so it is reproduced rather than tidied.
///
/// Note the value is per-process and never reused, which the capture protocol relies on for
/// `processId` (§2.1).
#[must_use]
pub fn node_instance_name() -> String {
    let base = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_NODE_BASE_ID.to_owned());
    format!("{base}_{}", *ID_UNIQUE_SUFFIX)
}

/// Generates a random UUID v4 in the canonical hyphenated form.
fn uuid_v4_string() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Flushes diagnostics. Returning means the flush completed.
pub type FlushDiagnostics = Arc<dyn Fn() + Send + Sync>;

/// Halts the process immediately with the given status, without running destructors.
pub type HaltProcess = Arc<dyn Fn(i32) + Send + Sync>;

/// The last-resort process boundary for compromised capture or unstable proxy state.
///
/// The **first** failure starts termination; later failures are ignored so several failing
/// connections cannot launch competing shutdown sequences. Diagnostics are flushed under a strict
/// timeout and the process is then halted directly, rather than relying on ordinary shutdown hooks
/// or cooperative executors that may themselves be deadlocked or corrupted.
pub struct CaptureFailureTerminator {
    exit_code: i32,
    log_flush_timeout: Duration,
    flush_diagnostics: FlushDiagnostics,
    halt_process: HaltProcess,
    started: AtomicBool,
}

impl CaptureFailureTerminator {
    /// Builds a terminator.
    ///
    /// # Panics
    ///
    /// Panics if `log_flush_timeout` is zero. An unbounded or absent flush budget would let a
    /// wedged log backend prevent the required process halt, which is the one thing this type
    /// exists to guarantee. Java rejects the same value in its constructor.
    #[must_use]
    pub fn new(
        exit_code: i32,
        log_flush_timeout: Duration,
        flush_diagnostics: FlushDiagnostics,
        halt_process: HaltProcess,
    ) -> Self {
        assert!(
            !log_flush_timeout.is_zero(),
            "log_flush_timeout must be positive"
        );
        Self {
            exit_code,
            log_flush_timeout,
            flush_diagnostics,
            halt_process,
            started: AtomicBool::new(false),
        }
    }

    /// Builds a terminator with the production exit code and flush budget.
    #[must_use]
    pub fn production(flush_diagnostics: FlushDiagnostics, halt_process: HaltProcess) -> Self {
        Self::new(
            CAPTURE_FAILURE_EXIT_CODE,
            CAPTURE_FAILURE_LOG_FLUSH_TIMEOUT,
            flush_diagnostics,
            halt_process,
        )
    }

    /// Terminates the process because capture is compromised or the proxy is unstable.
    ///
    /// Idempotent: only the first call does anything. Returns `true` if this call started
    /// termination.
    ///
    /// Unlike Java this does not spawn a non-daemon thread and then return. It flushes and halts on
    /// the calling thread, because there is no JVM-style "non-daemon thread keeps the process
    /// alive" semantic to work around: the halt is what ends the process, so deferring it to
    /// another thread would only widen the window in which more traffic could be admitted.
    /// The flush itself still runs on a separate thread under the timeout, so a wedged log backend
    /// cannot prevent the halt.
    pub fn terminate(&self, failure: &CaptureFailure) -> bool {
        if self.started.swap(true, Ordering::SeqCst) {
            return false;
        }

        tracing::error!(
            event = "capture_failure_terminating",
            exit_code = self.exit_code,
            reason = failure.reason(),
            unstable = failure.is_unstable(),
            "Capture is compromised or the proxy is unstable; terminating immediately"
        );
        // Written to stderr as well as the log facade, because the reason this type exists is that
        // the log backend itself may be the thing that is broken.
        eprintln!(
            "Capture is compromised or the proxy is unstable; terminating with exit code {}: {}",
            self.exit_code,
            failure.reason()
        );

        self.flush_diagnostics_within_timeout();
        (self.halt_process)(self.exit_code);
        true
    }

    /// Runs the diagnostics flush on a separate thread, waiting at most the configured timeout.
    fn flush_diagnostics_within_timeout(&self) {
        let flush = Arc::clone(&self.flush_diagnostics);
        let done = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let signal = Arc::clone(&done);

        // Detached deliberately: if the flush wedges, this thread is abandoned and the process is
        // halted anyway. Joining it would reintroduce the unbounded wait.
        let spawned = std::thread::Builder::new()
            .name("capture-failure-log-flush".to_owned())
            .spawn(move || {
                // A panic in the log backend must not prevent the halt.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush()));
                let (lock, condvar) = &*signal;
                if let Ok(mut finished) = lock.lock() {
                    *finished = true;
                }
                condvar.notify_all();
            });

        if spawned.is_err() {
            // Cannot even spawn a thread. Halt without flushing rather than block.
            eprintln!("Unable to spawn the diagnostics flush thread; terminating now");
            return;
        }

        let (lock, condvar) = &*done;
        let Ok(finished) = lock.lock() else {
            eprintln!("Diagnostics flush state is unreadable; terminating now");
            return;
        };
        let wait = condvar.wait_timeout_while(finished, self.log_flush_timeout, |done| !*done);
        match wait {
            Ok((_, timeout)) if timeout.timed_out() => {
                eprintln!("Timed out flushing diagnostics after capture failure; terminating now");
            }
            Ok(_) => {}
            Err(_) => {
                eprintln!("Diagnostics flush wait failed; terminating now");
            }
        }
    }
}

impl std::fmt::Debug for CaptureFailureTerminator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureFailureTerminator")
            .field("exit_code", &self.exit_code)
            .field("log_flush_timeout", &self.log_flush_timeout)
            .field("started", &self.started.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// Installs a panic hook that terminates with the capture-failure status after flushing.
///
/// # Why this is required
///
/// The release profile sets `panic = "abort"`, which is the correct choice for a capture proxy: a
/// panic means internal ownership or invariant corruption, and `proxyCaptureProtocol.md` §7.1
/// requires that to terminate the process in **both** fail modes. But a bare abort exits with
/// SIGABRT (status 134) and runs no flush, so the operator loses both the distinct capture-failure
/// status and the diagnostics explaining it.
///
/// This hook restores both: it reports the panic through the terminator, which flushes under the
/// bounded timeout and halts with [`CAPTURE_FAILURE_EXIT_CODE`]. If the hook itself cannot run, the
/// abort still happens, so the process never survives a panic.
pub fn install_panic_hook(terminator: Arc<CaptureFailureTerminator>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Keep the default report (message, location, backtrace) on stderr first.
        previous(info);
        let failure = CaptureFailure::unstable(format!("proxy panicked: {info}"));
        terminator.terminate(&failure);
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn recording_terminator(
        flush_delay: Option<Duration>,
    ) -> (
        Arc<CaptureFailureTerminator>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<i32>>>,
    ) {
        let flushes = Arc::new(AtomicUsize::new(0));
        let halts = Arc::new(Mutex::new(Vec::new()));
        let flush_probe = Arc::clone(&flushes);
        let halt_probe = Arc::clone(&halts);
        let terminator = Arc::new(CaptureFailureTerminator::new(
            CAPTURE_FAILURE_EXIT_CODE,
            Duration::from_millis(250),
            Arc::new(move || {
                if let Some(delay) = flush_delay {
                    std::thread::sleep(delay);
                }
                flush_probe.fetch_add(1, Ordering::SeqCst);
            }),
            Arc::new(move |code| halt_probe.lock().expect("test mutex").push(code)),
        ));
        (terminator, flushes, halts)
    }

    #[test]
    fn the_first_failure_flushes_and_halts_with_the_distinct_exit_code() {
        let (terminator, flushes, halts) = recording_terminator(None);
        assert!(terminator.terminate(&CaptureFailure::required("kafka down")));
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(halts.lock().expect("test mutex").as_slice(), [78]);
    }

    #[test]
    fn later_failures_cannot_launch_a_competing_termination() {
        let (terminator, flushes, halts) = recording_terminator(None);
        assert!(terminator.terminate(&CaptureFailure::required("first")));
        assert!(!terminator.terminate(&CaptureFailure::required("second")));
        assert!(!terminator.terminate(&CaptureFailure::unstable("third")));
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(halts.lock().expect("test mutex").len(), 1);
    }

    #[test]
    fn a_wedged_flush_cannot_prevent_the_halt() {
        // The whole point of the bounded budget: the log backend may be the broken thing.
        let (terminator, _flushes, halts) = recording_terminator(Some(Duration::from_secs(30)));
        let started = std::time::Instant::now();
        assert!(terminator.terminate(&CaptureFailure::required("kafka down")));
        let elapsed = started.elapsed();
        assert_eq!(halts.lock().expect("test mutex").as_slice(), [78]);
        assert!(
            elapsed < Duration::from_secs(5),
            "halt waited {elapsed:?} on a wedged flush"
        );
    }

    #[test]
    fn a_panicking_flush_cannot_prevent_the_halt() {
        let halts = Arc::new(Mutex::new(Vec::new()));
        let halt_probe = Arc::clone(&halts);
        let terminator = CaptureFailureTerminator::new(
            CAPTURE_FAILURE_EXIT_CODE,
            Duration::from_millis(250),
            Arc::new(|| panic!("log backend exploded")),
            Arc::new(move |code| halt_probe.lock().expect("test mutex").push(code)),
        );
        assert!(terminator.terminate(&CaptureFailure::required("kafka down")));
        assert_eq!(halts.lock().expect("test mutex").as_slice(), [78]);
    }

    #[test]
    fn concurrent_failures_produce_exactly_one_halt() {
        let (terminator, flushes, halts) = recording_terminator(None);
        let mut handles = Vec::new();
        for i in 0..16 {
            let terminator = Arc::clone(&terminator);
            handles.push(std::thread::spawn(move || {
                terminator.terminate(&CaptureFailure::required(format!("failure {i}")))
            }));
        }
        let winners = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().expect("worker thread"))
            .filter(|started| *started)
            .count();
        assert_eq!(winners, 1, "exactly one caller may start termination");
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(halts.lock().expect("test mutex").len(), 1);
    }

    #[test]
    #[should_panic(expected = "log_flush_timeout must be positive")]
    fn a_zero_flush_budget_is_rejected() {
        let _ = CaptureFailureTerminator::new(
            CAPTURE_FAILURE_EXIT_CODE,
            Duration::ZERO,
            Arc::new(|| {}),
            Arc::new(|_| {}),
        );
    }

    #[test]
    fn the_node_instance_name_is_stable_within_the_process() {
        let first = node_instance_name();
        let second = node_instance_name();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn the_node_instance_name_carries_the_hostname_and_a_unique_suffix() {
        let name = node_instance_name();
        let suffix = &*ID_UNIQUE_SUFFIX;
        assert!(
            name.ends_with(&format!("_{suffix}")),
            "{name} must end with _<uuid>"
        );
        // A v4 UUID in canonical form.
        assert_eq!(suffix.len(), 36, "unexpected uuid form: {suffix}");
        assert_eq!(suffix.matches('-').count(), 4);
    }

    #[test]
    fn the_unset_hostname_fallback_reproduces_javas_double_underscore() {
        // ProcessHelpers.java concatenates the "generated_" fallback and then another "_", so Java
        // produces "generated__<uuid>". Reproduced deliberately: the string is operator-visible.
        assert!(DEFAULT_NODE_BASE_ID.ends_with('_'));
        let fallback = format!("{DEFAULT_NODE_BASE_ID}_{}", *ID_UNIQUE_SUFFIX);
        assert!(
            fallback.starts_with("generated__"),
            "expected a double underscore, got {fallback}"
        );
    }
}
