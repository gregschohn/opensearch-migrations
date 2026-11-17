//! The composition root: start-up ordering, the accept loop, signals, and shutdown
//! (traceability rows PROXY-03, PROXY-11).
//!
//! # Start-up ordering is a safety property, not a style choice
//!
//! `proxyCaptureProtocol.md` §3.2 fixes the order, and the reason is the capture gap: a client that
//! connects before capture is usable would be served **uncaptured**, silently. So the listening
//! socket is the *last* thing created, and a client that arrives earlier is refused by the
//! operating system — there is no socket to accept it, no accept backlog, and nothing to log.
//!
//! ```text
//! 1. resolve and validate configuration        (no runtime, no socket, no log backend yet)
//! 2. build the Tokio runtime                   (sized from --num-threads)
//! 3. install telemetry export                  (must precede logging; needs a runtime for OTLP)
//! 4. install logging                           (attaches the trace layer export just created)
//! 5. install the panic hook                    (so a panic from here on halts with status 78)
//! 6. build every handshake-capable object      (a bad certificate must not be connection 1's problem)
//! 7. bring Kafka to readiness                  (capture mode only: capability, membership, first generation)
//! 8. emit capture_startup_summary
//! 9. bind the listener                         <-- the first instant a client can be served
//! ```
//!
//! Steps 3 and 4 are ordered by [`capture_observability`] itself: `export::install` builds the
//! tracer provider that `logging::install` attaches as a layer, and the gRPC transport needs a
//! Tokio runtime context whenever an OTLP endpoint is configured. Both happen inside
//! [`tokio::runtime::Runtime::block_on`] for that reason.
//!
//! # Exit codes
//!
//! | Status | Meaning |
//! |---|---|
//! | 0 | clean shutdown, or `--help` / `--version` |
//! | 2 | configuration error ([`capture_config::CONFIG_ERROR_EXIT_CODE`]) |
//! | 3 | destination URI could not be parsed ([`capture_config::DESTINATION_URI_EXIT_CODE`]) |
//! | 70 | start-up failed for a non-configuration reason: TLS material, telemetry, or bind |
//! | 78 | capture was compromised, or capture start-up was refused ([`crate::CAPTURE_FAILURE_EXIT_CODE`]) |

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use capture_config::{ApplicationConfig, ConfigError, EnvLookup, ProcessEnv, redact_args};
use capture_observability::process_metrics::ProcessIdentity;
use capture_observability::{events, export, logging, root::RootTelemetry};
use capture_transport::listener::{AcceptError, Accepted};
use capture_transport::{
    CaptureSink, CloseReason, Connection, ConnectionConfig, Listener, ListenerConfig,
    worker_thread_count,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::bind_address::{describe_exposure, select_bind_address};
use crate::capture_sink::NoCaptureSink;
use crate::connection_factory::{ClientAcceptError, ConnectionFactory};
use crate::kafka_capture::CaptureMode;
use crate::policy::{connection_config, suppression_policy};
use crate::shutdown::{CaptureRetirement, ConnectionSet, ShutdownTiming, perform_orderly_shutdown};
use crate::{CAPTURE_FAILURE_EXIT_CODE, CaptureFailureTerminator, install_panic_hook};

/// Exit status for a start-up failure that is not a configuration error.
///
/// `EX_SOFTWARE`. Distinct from the configuration statuses so an operator can tell "you configured
/// this wrong" from "your certificate file is unreadable" or "that port is already in use", and
/// distinct from [`CAPTURE_FAILURE_EXIT_CODE`] so neither is mistaken for the other.
pub const STARTUP_FAILURE_EXIT_CODE: i32 = 70;

/// Runs the proxy and returns the process exit status.
///
/// `raw_args` must exclude the program name.
#[must_use]
pub fn run(raw_args: &[String]) -> i32 {
    run_with_environment(raw_args, &ProcessEnv)
}

/// [`run`], with the environment injected so the start-up path is testable.
#[must_use]
pub fn run_with_environment(raw_args: &[String], environment: &dyn EnvLookup) -> i32 {
    // Step 1. Nothing is logged through `tracing` yet: the subscriber is installed at step 4, and
    // a configuration error must still be legible, so these go straight to stderr. Java does the
    // same (CaptureProxy.java:339).
    let selection = match select_bind_address(raw_args, environment) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("capture-proxy: {error}");
            return capture_config::CONFIG_ERROR_EXIT_CODE;
        }
    };

    let config = match ApplicationConfig::from_args(&selection.remaining_args, environment) {
        Ok(config) => config,
        Err(ConfigError::HelpRequested { message }) => {
            // --help and --version are a successful run with a different output. The image build
            // smoke-checks `capture-proxy --version`, so this path is load-bearing.
            print!("{message}");
            return 0;
        }
        Err(error) => {
            eprintln!("capture-proxy: {error}");
            return error.exit_code();
        }
    };

    // One identity for the whole process: the ported ProcessHelpers name plus one activation id.
    // Deliberately not `ProcessIdentity::detect()`, which would derive a *second* node name from
    // its own UUID and leave the process with two different ids in its logs and metrics.
    let identity = ProcessIdentity::new(
        crate::node_instance_name(),
        uuid::Uuid::new_v4().to_string(),
    );

    // Step 2.
    let worker_threads = worker_thread_count(config.worker_threads());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("capture-proxy-worker")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("capture-proxy: unable to build the async runtime: {error}");
            return STARTUP_FAILURE_EXIT_CODE;
        }
    };

    let exit_code = runtime.block_on(serve(&config, identity, selection.bind_address, raw_args));

    // The runtime is shut down with a bounded budget: a connection task that is wedged in a
    // syscall must not keep the process alive past the point the shutdown sequence declared
    // itself finished.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    exit_code
}

/// Whether export was configured, which the start-up summary reports.
#[derive(Debug, Clone, Copy)]
struct ExportState {
    traces: bool,
    metrics: bool,
}

/// Steps 3 to 5: telemetry export, logging, and the panic hook, in that mandatory order.
///
/// `export::install` must precede `logging::install` because logging attaches a layer built from
/// the tracer provider export creates, and both must run inside a Tokio runtime context because
/// the OTLP gRPC transport requires one whenever an endpoint is configured. Nothing before this
/// point can use `tracing`, so its own failures go to stderr.
fn install_observability(
    config: &ApplicationConfig,
    identity: ProcessIdentity,
) -> Result<(Arc<RootTelemetry>, ExportState), i32> {
    let telemetry_export = export::install(
        &export::CollectorEndpoints {
            trace: config.telemetry().traces.clone(),
            metrics: config.telemetry().metrics.clone(),
        },
        identity.process_id(),
    )
    .map_err(|error| {
        eprintln!("capture-proxy: unable to install telemetry export: {error}");
        STARTUP_FAILURE_EXIT_CODE
    })?;

    logging::install(
        &logging::LoggingConfig::from_env(),
        telemetry_export.tracer_provider(),
    )
    .map_err(|error| {
        eprintln!("capture-proxy: unable to install logging: {error}");
        STARTUP_FAILURE_EXIT_CODE
    })?;

    let exports = ExportState {
        traces: telemetry_export.trace_export_enabled(),
        metrics: telemetry_export.metric_export_enabled(),
    };
    let telemetry = Arc::new(RootTelemetry::new(telemetry_export, identity));

    // Step 5. From here a panic flushes diagnostics and halts with the distinct capture-failure
    // status instead of aborting with SIGABRT and no explanation.
    let flush_telemetry = Arc::clone(&telemetry);
    let terminator = Arc::new(CaptureFailureTerminator::production(
        Arc::new(move || flush_telemetry.export().force_flush()),
        // `std::process::exit` rather than a direct `_exit`: `unsafe_code` is forbidden in this
        // workspace, and `libc::_exit` cannot be called without it. The difference that matters
        // here — not unwinding, not running other threads' destructors — holds for both.
        Arc::new(|code| std::process::exit(code)),
    ));
    // One terminator for the whole process: the panic hook and the capture gate share it, so its
    // one-shot guard means at most one termination sequence ever runs.
    install_panic_hook(Arc::clone(&terminator));
    TERMINATOR
        .set(terminator)
        .unwrap_or_else(|_| tracing::warn!("the capture-failure terminator was already installed"));

    Ok((telemetry, exports))
}

/// The process's capture-failure terminator, installed once by `install_observability`.
static TERMINATOR: std::sync::OnceLock<Arc<CaptureFailureTerminator>> = std::sync::OnceLock::new();

/// Says, at WARN, who can reach this listener and what stops them.
///
/// Unconditional rather than only on the wildcard bind: an operator reading the log should not
/// have to know the default to know the exposure. `capture_transport::Listener::bind` emits its own
/// line for the wildcard case; this one adds the part that actually decides authorisation, which is
/// whether a client certificate is required.
fn warn_about_exposure(
    config: &ApplicationConfig,
    bind_address: IpAddr,
    factory: &ConnectionFactory,
) {
    tracing::warn!(
        bind_address = %bind_address,
        listen_port = config.listen_port(),
        require_client_auth = factory.requires_client_auth(),
        front_tls = factory.front_tls_enabled(),
        "{}",
        describe_exposure(bind_address, factory.requires_client_auth())
    );
    if factory.destination_validation_disabled() {
        tracing::warn!(
            destination = %factory.destination(),
            "--insecure-destination is set: destination certificate validation is DISABLED, so \
             ANY certificate the destination presents is accepted and the proxy cannot detect an \
             impersonated upstream"
        );
    }
}

/// Steps 3 to 9, plus the accept loop and shutdown. Runs inside the runtime.
#[allow(
    clippy::too_many_lines,
    reason = "the start-up order is a single safety-critical sequence (proxyCaptureProtocol 3.2); \
              splitting it further would hide which step precedes the bind"
)]
async fn serve(
    config: &ApplicationConfig,
    identity: ProcessIdentity,
    bind_address: IpAddr,
    raw_args: &[String],
) -> i32 {
    let (telemetry, exports) = match install_observability(config, identity) {
        Ok(installed) => installed,
        Err(exit_code) => return exit_code,
    };

    // The argument echo, redacted. Java echoes its whole argument vector unredacted, twice
    // (CaptureProxy.java:337, :558), which puts --kafkaPassword and the base64 inline-JSON
    // document — which may itself carry the password — into the log.
    tracing::info!(
        args = ?redact_args(raw_args),
        source = ?config.source(),
        "capture-proxy starting"
    );

    // Step 6. A certificate that cannot be loaded, or a destination host that is not a usable TLS
    // peer name, must stop start-up here rather than fail the first connection.
    let factory = match ConnectionFactory::new(config) {
        Ok(factory) => Arc::new(factory),
        Err(error) => {
            tracing::error!(%error, "unable to build the connection TLS material");
            return STARTUP_FAILURE_EXIT_CODE;
        }
    };

    warn_about_exposure(config, bind_address, &factory);

    let predicate = match suppression_policy(config) {
        Ok(policy) => Arc::new(policy),
        Err(error) => {
            tracing::error!(%error, "unable to compile the capture suppression policy");
            return STARTUP_FAILURE_EXIT_CODE;
        }
    };
    let connection_config = match connection_config(config) {
        Ok(connection_config) => connection_config,
        Err(error) => {
            tracing::error!(%error, "unable to build the header rewrite configuration");
            return STARTUP_FAILURE_EXIT_CODE;
        }
    };

    // Step 7. Capture mode brings Kafka to readiness here, before any socket exists. There is
    // deliberately no `--no-capture` fallback on failure: forwarding traffic that nobody captures,
    // because capture would not start, is precisely the silent gap the protocol forbids.
    let mode = if config.sink().kafka().is_some() {
        // `start_capture` polls metadata, membership, and the first generation against finite
        // budgets, blocking as it goes. `block_in_place` keeps it off the shared worker pool.
        let started =
            tokio::task::block_in_place(|| CaptureMode::start(config, &telemetry, &ProcessEnv));
        match started {
            Ok(mode) => CaptureTarget::Kafka(Arc::new(mode)),
            Err(error) if error.is_configuration_error() => {
                tracing::error!(
                    event = "capture_configuration_rejected",
                    %error,
                    "the capture configuration is invalid; refusing to start"
                );
                return capture_config::CONFIG_ERROR_EXIT_CODE;
            }
            Err(error) => {
                tracing::error!(
                    event = "capture_startup_refused",
                    %error,
                    "capture could not be brought to readiness; refusing to start rather than \
                     forward traffic uncaptured"
                );
                return CAPTURE_FAILURE_EXIT_CODE;
            }
        }
    } else {
        // --no-capture is a capture gap by construction, and the whole protocol exists to make
        // capture gaps loud. Said once, at WARN, with the word an operator greps for.
        tracing::warn!(
            event = "capture_startup_summary",
            capture_mode = "disabled",
            "--no-capture is set: this process forwards traffic and captures NOTHING. No request \
             or response reaches Kafka, and the replayer will see no traffic from this proxy."
        );
        CaptureTarget::Disabled
    };

    // The process-wide, one-way capture gate. One per process, shared by every connection: that
    // sharing is what makes the first required-capture failure close the gate for everyone.
    let gate = Arc::new(crate::CaptureProcessState::new(config.failure_policy()));
    // proxyCaptureProtocol.md 7.2: under fail-closed, and for any unstable failure, the process
    // terminates immediately with the distinct capture-failure status. The gate's termination
    // listener is what turns the state transition into the exit; without it the process logged
    // "will terminate" and then stayed alive with its listener open.
    if let Some(terminator) = TERMINATOR.get() {
        let terminator = Arc::clone(terminator);
        gate.add_termination_listener(
            &(Arc::new(move |failure: &crate::CaptureFailure| {
                terminator.terminate(failure);
            }) as crate::capture_process_state::TerminationListener),
        );
    }
    if let CaptureTarget::Kafka(kafka) = &mode {
        kafka.bridge_terminal_failure(&gate);
    }

    // Step 8.
    events::log_startup_summary(&events::StartupSummary {
        identity: telemetry.identity(),
        listen_port: config.listen_port(),
        capture_mode: match &mode {
            CaptureTarget::Disabled => events::CaptureMode::Disabled,
            CaptureTarget::Kafka(_) => events::CaptureMode::Kafka,
        },
        kafka_topic: match &mode {
            CaptureTarget::Disabled => None,
            CaptureTarget::Kafka(kafka) => Some(kafka.topic()),
        },
        failure_policy: failure_policy_label(config.failure_policy()),
        trace_export_enabled: exports.traces,
        metric_export_enabled: exports.metrics,
    });

    // Step 9. Last of all.
    let listener_config = ListenerConfig::new(config.listen_port())
        .with_bind_address(bind_address)
        .with_worker_threads(config.worker_threads());
    let listener = match Listener::bind(&listener_config).await {
        Ok(listener) => Arc::new(listener),
        Err(error) => {
            tracing::error!(
                address = %listener_config.socket_address(),
                %error,
                "unable to bind the capture proxy listener"
            );
            return STARTUP_FAILURE_EXIT_CODE;
        }
    };
    tracing::info!(
        address = %listener.local_addr(),
        worker_threads = listener.worker_threads(),
        destination = %factory.destination(),
        "the capture proxy is accepting connections"
    );

    let shutdown_requested = Arc::new(Notify::new());
    spawn_signal_handler(Arc::clone(&shutdown_requested));

    accept_loop(
        &listener,
        &factory,
        &telemetry,
        &connection_config,
        &predicate,
        &shutdown_requested,
        &mode,
        &gate,
    )
    .await;

    shut_down(&listener, &telemetry, &mode).await
}

/// Where this process's capture stream goes.
#[derive(Debug, Clone)]
enum CaptureTarget {
    /// `--no-capture`.
    Disabled,
    /// A ready Kafka capture subsystem.
    Kafka(Arc<CaptureMode>),
}

/// Accepts until a signal arrives or the listener stops accepting.
#[allow(
    clippy::too_many_arguments,
    reason = "the accept loop needs every piece of per-connection state the process built; \
              bundling them into a struct would only move the same list somewhere less visible"
)]
async fn accept_loop<P>(
    listener: &Arc<Listener>,
    factory: &Arc<ConnectionFactory>,
    telemetry: &Arc<RootTelemetry>,
    connection_config: &ConnectionConfig,
    predicate: &Arc<P>,
    shutdown_requested: &Arc<Notify>,
    mode: &CaptureTarget,
    gate: &Arc<crate::CaptureProcessState>,
) where
    P: capture_http_scan::CapturePredicate + Send + Sync + 'static,
{
    static CONNECTION_ORDINAL: AtomicU64 = AtomicU64::new(0);
    let mut consecutive_accept_errors: u32 = 0;

    loop {
        let accepted = tokio::select! {
            () = shutdown_requested.notified() => {
                tracing::info!("received a shutdown signal; starting orderly shutdown");
                break;
            }
            accepted = listener.accept() => accepted,
        };

        match accepted {
            Ok(accepted) => {
                consecutive_accept_errors = 0;
                let ordinal = CONNECTION_ORDINAL.fetch_add(1, Ordering::Relaxed);
                // Java uses Netty's channel id. Any per-process-unique, stable string works: it is
                // the replayer's grouping key and, in capture mode, the Kafka routing key.
                let connection_id = format!("{}-{ordinal:016x}", telemetry.identity().process_id());
                tokio::spawn(serve_connection(
                    accepted,
                    connection_id,
                    Arc::clone(factory),
                    Arc::clone(telemetry),
                    connection_config.clone(),
                    Arc::clone(predicate),
                    mode.clone(),
                    Arc::clone(gate),
                ));
            }
            Err(AcceptError::StoppedAccepting) => break,
            Err(AcceptError::Io(error)) => {
                consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                tracing::warn!(
                    %error,
                    consecutive_accept_errors,
                    "failed to accept a client connection"
                );
                // A persistent accept failure — the classic one is EMFILE — would otherwise spin
                // this loop at full speed and starve the connections already being served.
                if consecutive_accept_errors > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}

/// Serves one accepted client connection end to end.
///
/// The connection's capture sink is created **before** the frontside handshake, matching Java,
/// where the capture offloader is built at channel initialisation (`PCI`). That ordering is what
/// lets a TLS handshake failure still emit its terminal `CloseObservation`.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a distinct piece of process-level state this connection needs; \
              see accept_loop"
)]
async fn serve_connection<P>(
    accepted: Accepted,
    connection_id: String,
    factory: Arc<ConnectionFactory>,
    telemetry: Arc<RootTelemetry>,
    connection_config: ConnectionConfig,
    predicate: Arc<P>,
    mode: CaptureTarget,
    gate: Arc<crate::CaptureProcessState>,
) where
    P: capture_http_scan::CapturePredicate,
{
    // The lifetime budget starts at accept, not after the handshakes: LHH:287-303 schedules the
    // deadline at pipeline install.
    let accepted_at = Instant::now();
    telemetry.connections().connection_opened();
    // WIRE-10: activeConnection (queried by the k6 Grafana dashboard as activeConnection_count),
    // captureConnectionCount/Duration/ExceptionCount. Opened at accept, closed on drop at the end of
    // this function, so every path -- refusal, handshake failure, normal close -- is counted.
    let connection_span =
        telemetry.open_connection_span(connection_id.clone(), telemetry.identity().process_id());

    match mode {
        CaptureTarget::Disabled => {
            serve_with_sink(
                accepted,
                accepted_at,
                NoCaptureSink::new(),
                &connection_id,
                &factory,
                &telemetry,
                connection_config,
                predicate.as_ref(),
                &connection_span,
            )
            .await;
        }
        // proxyCaptureProtocol.md 7.3, unmanaged fail-open: after the irreversible transition,
        // "existing and new TCP connections may continue forwarding without capture". Capture is
        // abandoned for the process, so Kafka is not consulted at all.
        CaptureTarget::Kafka(_) if gate.is_pass_through() => {
            serve_with_sink(
                accepted,
                accepted_at,
                NoCaptureSink::new(),
                &connection_id,
                &factory,
                &telemetry,
                connection_config,
                predicate.as_ref(),
                &connection_span,
            )
            .await;
        }
        CaptureTarget::Kafka(kafka) => {
            match kafka.create_sink(&connection_id, Arc::clone(&gate)) {
                Ok(sink) => {
                    serve_with_sink(
                        accepted,
                        accepted_at,
                        sink,
                        &connection_id,
                        &factory,
                        &telemetry,
                        connection_config,
                        predicate.as_ref(),
                        &connection_span,
                    )
                    .await;
                }
                Err(reason) if gate.is_pass_through() => {
                    // The process went pass-through while this connection was being routed (the
                    // refusal is usually that very failure). Under fail-open it is served
                    // uncaptured, like every later connection.
                    tracing::warn!(
                        connection = %connection_id,
                        reason = %reason,
                        "serving a connection uncaptured: capture was abandoned for this process"
                    );
                    serve_with_sink(
                        accepted,
                        accepted_at,
                        NoCaptureSink::new(),
                        &connection_id,
                        &factory,
                        &telemetry,
                        connection_config,
                        predicate.as_ref(),
                        &connection_span,
                    )
                    .await;
                }
                Err(reason) => {
                    // Capture cannot be established while the process is still
                    // capture-authoritative (or is terminating), so the connection is closed
                    // rather than served. Serving it would be the silent capture gap the protocol
                    // forbids, and `accepted` is dropped here, which closes the socket.
                    tracing::error!(
                        connection = %connection_id,
                        reason = %reason,
                        "refusing a connection because capture could not be established for it"
                    );
                    finish_connection(&telemetry, &connection_id, CloseReason::CaptureFailure);
                }
            }
        }
    }
}

/// The part of serving a connection that does not depend on which capture sink is in use.
#[allow(
    clippy::too_many_arguments,
    reason = "see serve_connection; this is the same argument list minus the mode"
)]
async fn serve_with_sink<K, P>(
    accepted: Accepted,
    accepted_at: Instant,
    mut sink: K,
    connection_id: &str,
    factory: &ConnectionFactory,
    telemetry: &RootTelemetry,
    connection_config: ConnectionConfig,
    predicate: P,
    connection_span: &capture_observability::connection_span::ConnectionSpan,
) where
    K: CaptureSink,
    P: capture_http_scan::CapturePredicate,
{
    let Accepted {
        stream,
        peer,
        guard,
    } = accepted;

    let (client, lifetime_origin) = match factory.prepare_client(stream, peer, accepted_at).await {
        Ok(prepared) => prepared,
        Err(ClientAcceptError::Handshake(_)) => {
            // PCI:206-215, spec 13-D10: the failure is already logged (deduplicated) and the
            // terminal CloseObservation is still emitted, but deliberately NO
            // connectionException observation. `close` without `connection_exception` is exactly
            // that distinction.
            let _ = sink.close(std::time::SystemTime::now(), CloseReason::ClientIoError);
            finish_connection(telemetry, connection_id, CloseReason::ClientIoError);
            return;
        }
        Err(error @ ClientAcceptError::Io(_)) => {
            let now = std::time::SystemTime::now();
            let _ = sink.connection_exception(now, &error.to_string());
            let _ = sink.close(now, CloseReason::ClientIoError);
            finish_connection(telemetry, connection_id, CloseReason::ClientIoError);
            return;
        }
    };

    let source = match factory.connect_source().await {
        Ok(source) => source,
        Err(error) => {
            // BH:50-56 closes the client channel rather than firing an exception on it, so a
            // source-side failure produces no connectionException either.
            tracing::warn!(
                connection = %connection_id,
                destination = %factory.destination(),
                %error,
                "unable to connect to the destination"
            );
            let _ = sink.close(std::time::SystemTime::now(), CloseReason::SourceIoError);
            finish_connection(telemetry, connection_id, CloseReason::SourceIoError);
            return;
        }
    };

    // Wire-phase telemetry for every connection, captured or not, exactly as Java records it.
    let sink = crate::metered_sink::MeteredSink::new(sink, telemetry.wire_phases());
    let outcome = Connection::new(client, source, sink, predicate, connection_config)
        .with_lifetime_origin(lifetime_origin)
        .run(Some(Arc::clone(guard.shutdown())))
        .await;

    tracing::debug!(
        connection = %connection_id,
        peer = %peer,
        reason = outcome.close_reason.label(),
        client_bytes_read = outcome.client_bytes_read,
        source_bytes_written = outcome.source_bytes_written,
        source_bytes_read = outcome.source_bytes_read,
        client_bytes_written = outcome.client_bytes_written,
        requests_completed = outcome.requests_completed,
        requests_forwarded = outcome.requests_forwarded,
        "connection finished"
    );
    connection_span.add_bytes_read(outcome.client_bytes_read);
    connection_span.add_bytes_written(outcome.client_bytes_written);
    if outcome.close_reason.emits_connection_exception() {
        connection_span.record_exception();
    }
    finish_connection(telemetry, connection_id, outcome.close_reason);
    drop(guard);
}

/// Records one connection's close against the metric and, when it was not an ordinary close, the
/// operational event.
fn finish_connection(telemetry: &RootTelemetry, connection_id: &str, reason: CloseReason) {
    let metric_reason = metric_close_reason(reason);
    telemetry.connections().connection_closed(metric_reason);
    if reason != CloseReason::Normal {
        events::log_connection_dropped(connection_id, metric_reason);
        // The metric and event vocabulary is Java's six reasons; the transport distinguishes
        // thirteen. The precise one is kept here so a diagnosis is not limited to the collapsed
        // label. See `metric_close_reason`.
        tracing::debug!(
            connection = connection_id,
            transport_reason = reason.label(),
            "connection close reason, at full transport fidelity"
        );
    }
}

/// Collapses the transport's thirteen close reasons onto the six the metric contract allows.
///
/// AWKWARD: `capture_observability::connection_metrics::CloseReason` and
/// `events::log_connection_dropped` model only `CaptureConnectionMetrics.CloseReason`'s six Java
/// values, because the `reason` dimension is read by checked-in dashboards and must stay a closed,
/// bounded set. `capture_transport::CloseReason` has seven more, all introduced by owner decision
/// OD-1, which makes malformed framing terminal where Java tunnelled it. They have to be collapsed
/// to be recorded at all, so the mapping is explicit here rather than implicit anywhere:
///
/// * `RequestLineBytes` joins `HeaderBytes` — both are head-size budgets;
/// * every I/O and framing failure joins `Normal`, because Java reached `Normal` for all of them
///   (`LoggingHttpHandler.java:232` initialises the reason to `normal` and nothing else marks it).
///
/// The collapse loses nothing an operator needs: `finish_connection` logs the precise reason, and
/// the framing reasons additionally produce a `connectionException` observation in the capture
/// stream.
fn metric_close_reason(
    reason: CloseReason,
) -> capture_observability::connection_metrics::CloseReason {
    use capture_observability::connection_metrics::CloseReason as Metric;
    match reason {
        CloseReason::AssemblyDuration => Metric::AssemblyDuration,
        CloseReason::HeaderBytes | CloseReason::RequestLineBytes => Metric::HeaderBytes,
        CloseReason::TotalBytes => Metric::TotalBytes,
        CloseReason::ConnectionDuration => Metric::ConnectionDuration,
        CloseReason::CaptureFailure => Metric::CaptureFailure,
        CloseReason::Normal
        | CloseReason::MalformedFraming
        | CloseReason::MalformedResponse
        | CloseReason::ClientIoError
        | CloseReason::ClientWriteFailure
        | CloseReason::SourceIoError
        | CloseReason::Disconnected => Metric::Normal,
    }
}

/// Maps the configured policy onto the start-up summary's spelling.
fn failure_policy_label(
    policy: capture_config::CaptureFailurePolicy,
) -> events::FailurePolicyLabel {
    match policy {
        capture_config::CaptureFailurePolicy::FailClosed => events::FailurePolicyLabel::FailClosed,
        capture_config::CaptureFailurePolicy::FailOpen => events::FailurePolicyLabel::FailOpen,
    }
}

/// Triggers `shutdown_requested` on `SIGTERM` or `SIGINT`.
///
/// Both, because the container runtime sends `SIGTERM` and an operator at a terminal sends
/// `SIGINT`, and the orderly sequence must be the same either way.
fn spawn_signal_handler(shutdown_requested: Arc<Notify>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = match signal(SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::error!(%error, "unable to install the SIGTERM handler");
                    return;
                }
            };
            let mut interrupt = match signal(SignalKind::interrupt()) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::error!(%error, "unable to install the SIGINT handler");
                    return;
                }
            };
            let received = tokio::select! {
                _ = terminate.recv() => "SIGTERM",
                _ = interrupt.recv() => "SIGINT",
            };
            // Java writes this to stderr as well, because the log backend is one of the things a
            // shutdown may be racing (CaptureProxy.java:654).
            eprintln!("Received {received}.  Trying to shutdown cleanly");
            tracing::info!(signal = received, "received a termination signal");
        }
        #[cfg(not(unix))]
        {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "unable to wait for the interrupt signal");
                return;
            }
            tracing::info!(signal = "ctrl-c", "received a termination signal");
        }
        shutdown_requested.notify_waiters();
        shutdown_requested.notify_one();
    });
}

/// The listener and its registry, as the shutdown sequence's connection set.
struct ListenerConnections(Arc<Listener>);

impl ConnectionSet for ListenerConnections {
    fn stop_accepting(&self) {
        self.0.stop_accepting();
    }

    fn active_connection_count(&self) -> usize {
        self.0.registry().active_connection_count()
    }

    async fn disconnect_active(&self) {
        // Requests every live connection close, and pre-triggers any that registers afterwards.
        self.0.registry().disconnect_all();
        self.0.registry().wait_for_no_active_connections().await;
    }

    async fn wait_until_idle(&self) {
        self.0.registry().wait_for_no_active_connections().await;
    }
}

/// Capture retirement in `--no-capture` mode: there is no publisher to retire.
struct NoCaptureRetirement;

impl CaptureRetirement for NoCaptureRetirement {
    async fn retire(&self) -> Result<(), String> {
        Ok(())
    }

    async fn close(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Runs the orderly shutdown under an independent hard-stop watchdog, and returns the exit status.
async fn shut_down(
    listener: &Arc<Listener>,
    telemetry: &Arc<RootTelemetry>,
    mode: &CaptureTarget,
) -> i32 {
    let timing = ShutdownTiming::default();
    let watchdog = HardStopWatchdog::arm(timing.hard_stop(), telemetry.identity().clone());

    let connections = ListenerConnections(Arc::clone(listener));
    let on_deadline_missed = |still_active: usize| {
        events::log_shutdown_failed(
            telemetry.identity(),
            &format!(
                "orderly retirement did not complete before its deadline with {still_active} \
                 connections still active"
            ),
        );
    };

    // The two retirement implementations cannot be unified behind one `dyn` object: the
    // `CaptureRetirement` trait's methods return `impl Future`, which is not dyn-compatible. So the
    // sequence is instantiated per mode, which is also what keeps `--no-capture` free of any Kafka
    // code path at run time.
    let outcome = match mode {
        CaptureTarget::Disabled => {
            perform_orderly_shutdown(
                &connections,
                &NoCaptureRetirement,
                timing,
                on_deadline_missed,
            )
            .await
        }
        CaptureTarget::Kafka(kafka) => {
            perform_orderly_shutdown(&connections, kafka.as_ref(), timing, on_deadline_missed).await
        }
    };

    let summary = match outcome {
        crate::shutdown::ShutdownOutcome::Completed => events::ShutdownOutcome::Completed,
        crate::shutdown::ShutdownOutcome::Incomplete => events::ShutdownOutcome::Incomplete,
    };
    events::log_shutdown_summary(summary, telemetry.identity());

    // The last transition points and the shutdown summary must reach the collector before the
    // process is gone; a periodic reader would usually have exported them, and "usually" is not a
    // guarantee.
    telemetry.export().force_flush();
    telemetry.export().shutdown();

    // Disarmed only now: everything above is inside the budget the watchdog is enforcing.
    watchdog.disarm();

    match outcome {
        crate::shutdown::ShutdownOutcome::Completed => 0,
        crate::shutdown::ShutdownOutcome::Incomplete => STARTUP_FAILURE_EXIT_CODE,
    }
}

/// The independent hard-stop watchdog.
///
/// `CaptureProxy.java:634-649` schedules this and halts with the capture-failure status when the
/// orderly sequence overruns [`crate::ORDERLY_SHUTDOWN_HARD_STOP`].
///
/// It runs on its own **operating-system thread**, not as a Tokio task, and that is the whole
/// point: the condition it exists to escape is a shutdown that is stuck, and a shutdown can be
/// stuck precisely because the runtime is wedged — every worker blocked in a syscall, or an
/// `.await` that will never resolve. A timer scheduled on that runtime would never fire. A plain
/// thread with a condition-variable wait is scheduled by the kernel and cannot be starved by
/// anything the runtime does.
struct HardStopWatchdog {
    completed: std::sync::mpsc::Sender<()>,
}

impl HardStopWatchdog {
    /// Starts the watchdog thread.
    fn arm(hard_stop: std::time::Duration, identity: ProcessIdentity) -> Self {
        let (completed, finished) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("proxy-orderly-shutdown-watchdog".to_owned())
            .spawn(move || {
                match finished.recv_timeout(hard_stop) {
                    // Disarmed, or the sender was dropped because shutdown finished.
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        events::log_shutdown_summary(events::ShutdownOutcome::HardStop, &identity);
                        eprintln!(
                            "Orderly proxy shutdown exceeded its hard-stop deadline of {}s; \
                             halting now",
                            hard_stop.as_secs()
                        );
                        std::process::exit(CAPTURE_FAILURE_EXIT_CODE);
                    }
                }
            });
        if spawned.is_err() {
            // Report it rather than pretend the guarantee holds. An unbounded shutdown is a
            // Kubernetes SIGKILL rather than an unbounded hang, so this is a degradation, not a
            // reason to refuse to shut down.
            tracing::error!(
                "unable to start the orderly-shutdown hard-stop watchdog; shutdown is unbounded"
            );
        }
        Self { completed }
    }

    /// Stops the watchdog, because shutdown finished inside its budget.
    fn disarm(self) {
        let _ = self.completed.send(());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct StubEnv(HashMap<String, String>);

    impl EnvLookup for StubEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn version_is_a_successful_run() {
        // The image build runs `capture-proxy --version` as its smoke check. If this is not 0 the
        // container image does not build at all.
        assert_eq!(
            run_with_environment(&args(&["--version"]), &StubEnv::default()),
            0
        );
    }

    #[test]
    fn help_is_a_successful_run() {
        assert_eq!(
            run_with_environment(&args(&["--help"]), &StubEnv::default()),
            0
        );
    }

    #[test]
    fn a_configuration_error_exits_2() {
        // Neither --no-capture nor --kafka-brokers: starting would forward while capturing nothing.
        assert_eq!(
            run_with_environment(
                &args(&[
                    "--destination-uri",
                    "http://upstream.test",
                    "--listen-port",
                    "9200"
                ]),
                &StubEnv::default()
            ),
            capture_config::CONFIG_ERROR_EXIT_CODE
        );
        assert_eq!(capture_config::CONFIG_ERROR_EXIT_CODE, 2);
    }

    #[test]
    fn a_bad_destination_uri_exits_3() {
        assert_eq!(
            run_with_environment(
                &args(&[
                    "--destination-uri",
                    "",
                    "--listen-port",
                    "9200",
                    "--no-capture"
                ]),
                &StubEnv::default()
            ),
            capture_config::DESTINATION_URI_EXIT_CODE
        );
        assert_eq!(capture_config::DESTINATION_URI_EXIT_CODE, 3);
    }

    #[test]
    fn a_malformed_bind_address_exits_2_without_building_a_runtime() {
        assert_eq!(
            run_with_environment(
                &args(&[
                    "--bind-address",
                    "localhost",
                    "--destination-uri",
                    "http://upstream.test",
                    "--listen-port",
                    "9200",
                    "--no-capture"
                ]),
                &StubEnv::default()
            ),
            capture_config::CONFIG_ERROR_EXIT_CODE
        );
    }

    #[test]
    fn every_transport_close_reason_maps_onto_the_bounded_metric_set() {
        use capture_observability::connection_metrics::CloseReason as Metric;
        for (transport, expected) in [
            (CloseReason::Normal, Metric::Normal),
            (CloseReason::AssemblyDuration, Metric::AssemblyDuration),
            (CloseReason::HeaderBytes, Metric::HeaderBytes),
            (CloseReason::RequestLineBytes, Metric::HeaderBytes),
            (CloseReason::TotalBytes, Metric::TotalBytes),
            (CloseReason::ConnectionDuration, Metric::ConnectionDuration),
            (CloseReason::CaptureFailure, Metric::CaptureFailure),
            (CloseReason::MalformedFraming, Metric::Normal),
            (CloseReason::MalformedResponse, Metric::Normal),
            (CloseReason::ClientIoError, Metric::Normal),
            (CloseReason::ClientWriteFailure, Metric::Normal),
            (CloseReason::SourceIoError, Metric::Normal),
            (CloseReason::Disconnected, Metric::Normal),
        ] {
            assert_eq!(
                metric_close_reason(transport),
                expected,
                "{} must stay inside the frozen metric vocabulary",
                transport.label()
            );
        }
    }

    #[test]
    fn the_failure_policy_label_keeps_the_underscored_spelling() {
        assert_eq!(
            failure_policy_label(capture_config::CaptureFailurePolicy::FailOpen).as_log_value(),
            "fail_open"
        );
        assert_eq!(
            failure_policy_label(capture_config::CaptureFailurePolicy::FailClosed).as_log_value(),
            "fail_closed"
        );
    }

    #[test]
    fn the_watchdog_can_be_disarmed_without_halting() {
        // If this test's process survives, the watchdog honoured the disarm.
        let watchdog = HardStopWatchdog::arm(
            std::time::Duration::from_millis(50),
            ProcessIdentity::new("process-1", "activation-1"),
        );
        watchdog.disarm();
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}
