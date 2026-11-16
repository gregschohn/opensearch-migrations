#!/usr/bin/env bash
# Single entrypoint for one measured run.
#
#   run-profile.sh <RUN_ID> <java|rust|none-direct> <profile> [duration_s] [warmup_s]
#
# Everything the run creates is named from RUN_ID — docker network, container
# names, Kafka topic (and therefore the membership consumer group, which the
# proxy derives from the topic), and output files — so concurrent runs cannot
# collide. No port is published to the host: all traffic stays on the run's own
# docker bridge network, which also means the proxy under test is never
# reachable from outside this machine.
set -euo pipefail
HARNESS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HARNESS_DIR/lib.sh"
source "$HARNESS_DIR/profiles.sh"

RUN_ID="${1:?RUN_ID required}"
PROXY_KIND="${2:?proxy target required: java|rust|none-direct}"
PROFILE="${3:?profile name required}"
DURATION_S="${4:-60}"
WARMUP_S="${5:-25}"

case "$PROXY_KIND" in java|rust|none-direct) ;; *) die "proxy must be java|rust|none-direct" ;; esac
load_profile "$PROFILE" || die "bad profile"

RUN="$RUN_ID"
OUTDIR="$RESULTS/$RUN"
mkdir -p "$OUTDIR"
RUNLOG="$OUTDIR/run.log"
exec 3>&1
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$RUNLOG" >&2; }

NET="net-$RUN"
TOPIC="cap-$RUN"
SRC_C="src-$RUN"; PX_C="px-$RUN"; KF_C="kafka-$RUN"; TX_C="toxi-$RUN"
SMP_C="smp-$RUN"; GEN_C="gen-$RUN"; OTEL_C="otel-$RUN"

trap 'rc=$?; log "tearing down"; save_logs; cleanup_run "$RUN"; exit $rc' EXIT

save_logs() {
  for c in "$PX_C" "$KF_C" "$SRC_C" "$TX_C" "$OTEL_C" "$GEN_C"; do
    docker logs "$c" > "$OUTDIR/${c}.log" 2>&1 || true
  done
}

LOAD_HOST_BEFORE="$(host_loadavg)"
log "RUN_ID=$RUN profile=$PROFILE proxy=$PROXY_KIND duration=${DURATION_S}s warmup=${WARMUP_S}s"
log "host loadavg before: $LOAD_HOST_BEFORE"

ensure_certs
docker network create "$NET" >/dev/null 2>&1 || die "could not create network $NET"

# ---------------------------------------------------------------------------
# 1. HTTP source, pinned to its own cores.
# ---------------------------------------------------------------------------
SRC_ARGS=(-mode source -source-addr :9200)
if [[ "$BACK_TLS" == 1 ]]; then
  SRC_ARGS+=(-source-tls -source-cert /certs/source-cert.pem -source-key /certs/source-key.pem)
fi
docker run -d --name "$SRC_C" --network "$NET" --network-alias qual-source \
  --cpuset-cpus="$CORES_SOURCE" --memory=2g \
  -v "$CERTS":/certs:ro "$GEN_IMAGE" "${SRC_ARGS[@]}" >/dev/null \
  || die "source container failed to start"
wait_tcp "$NET" "qual-source:9200" 30 || die "source never listened on 9200"
log "source up (back_tls=$BACK_TLS)"

# ---------------------------------------------------------------------------
# 2. Kafka (single broker, KRaft). message.timestamp.type=LogAppendTime is
#    required by the capture protocol's broker-time continuity checks, so the
#    broker is configured for it AND the topic sets it explicitly.
# ---------------------------------------------------------------------------
KAFKA_BOOTSTRAP=""
if [[ "$CAPTURE" == 1 ]]; then
  if [[ "$KAFKA_PATH" == toxi ]]; then
    ADVERTISED="PLAINTEXT://$TX_C:19092"
  else
    ADVERTISED="PLAINTEXT://$KF_C:9092"
  fi
  docker run -d --name "$KF_C" --network "$NET" --network-alias "$KF_C" \
    --cpuset-cpus="$CORES_KAFKA" --memory=6g \
    -e KAFKA_NODE_ID=1 \
    -e KAFKA_PROCESS_ROLES=broker,controller \
    -e KAFKA_LISTENERS="PLAINTEXT://:9092,CONTROLLER://:9093" \
    -e KAFKA_ADVERTISED_LISTENERS="$ADVERTISED" \
    -e KAFKA_CONTROLLER_QUORUM_VOTERS="1@$KF_C:9093" \
    -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
    -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP="CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT" \
    -e KAFKA_INTER_BROKER_LISTENER_NAME=PLAINTEXT \
    -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 \
    -e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 \
    -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
    -e KAFKA_LOG_MESSAGE_TIMESTAMP_TYPE=LogAppendTime \
    -e KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS=0 \
    -e KAFKA_NUM_PARTITIONS="${KAFKA_PARTITIONS:-8}" \
    -e CLUSTER_ID=qual-cluster-0000000001 \
    "$KAFKA_IMAGE" >/dev/null || die "kafka container failed to start"
  wait_tcp "$NET" "$KF_C:9092" 90 || { docker logs "$KF_C" | tail -40 >&2; die "kafka never listened"; }
  log "kafka up"

  if [[ "$KAFKA_PATH" == toxi ]]; then
    docker run -d --name "$TX_C" --network "$NET" --network-alias "$TX_C" \
      --cpuset-cpus="$CORES_INFRA" --memory=512m \
      "$TOXI_IMAGE" -host=0.0.0.0 >/dev/null || die "toxiproxy failed to start"
    wait_tcp "$NET" "$TX_C:8474" 30 || die "toxiproxy api never came up"
    api() { docker run --rm --network "$NET" --cpuset-cpus="$CORES_INFRA" \
              "$ALPINE_IMAGE" sh -c "wget -q -O- --post-data='$2' --header='Content-Type: application/json' http://$TX_C:8474$1" ; }
    api /proxies "{\"name\":\"kafka\",\"listen\":\"0.0.0.0:19092\",\"upstream\":\"$KF_C:9092\",\"enabled\":true}" >/dev/null \
      || die "toxiproxy: could not create kafka proxy"
    if [[ "$KAFKA_DELAY_MS" -gt 0 ]]; then
      api /proxies/kafka/toxics "{\"name\":\"lat_down\",\"type\":\"latency\",\"stream\":\"downstream\",\"toxicity\":1.0,\"attributes\":{\"latency\":$KAFKA_DELAY_MS,\"jitter\":0}}" >/dev/null \
        || die "toxiproxy: could not add latency toxic"
      log "toxiproxy: ${KAFKA_DELAY_MS}ms downstream latency on the Kafka path"
    else
      log "toxiproxy: pass-through (control for the delayed-Kafka run)"
    fi
    KAFKA_BOOTSTRAP="$TX_C:19092"
  else
    KAFKA_BOOTSTRAP="$KF_C:9092"
  fi

  # Create the topic up front with an explicit partition count and
  # LogAppendTime, rather than letting the proxy auto-create it, so the
  # partition count is identical in every run.
  docker exec "$KF_C" /opt/kafka/bin/kafka-topics.sh --bootstrap-server "$KF_C:9092" \
    --create --topic "$TOPIC" --partitions "${KAFKA_PARTITIONS:-8}" --replication-factor 1 \
    --config message.timestamp.type=LogAppendTime >>"$RUNLOG" 2>&1 \
    || die "could not create topic $TOPIC"
  log "topic $TOPIC created (${KAFKA_PARTITIONS:-8} partitions, LogAppendTime)"
fi

# ---------------------------------------------------------------------------
# 3. OTel collector. Collects the proxy's own metrics, notably
#    kafkaCommitDuration (the Kafka acknowledgement latency histogram) which no
#    external observer can reconstruct.
# ---------------------------------------------------------------------------
OTEL_ENDPOINT=""
if [[ -f "$HARNESS_DIR/otel-config.yaml" ]]; then
  docker run -d --name "$OTEL_C" --network "$NET" --network-alias "$OTEL_C" \
    --cpuset-cpus="$CORES_INFRA" --memory=1g \
    -v "$HARNESS_DIR/otel-config.yaml":/etc/otel/config.yaml:ro \
    "$OTEL_IMAGE" --config=/etc/otel/config.yaml >/dev/null \
    || die "otel collector failed to start"
  if wait_tcp "$NET" "$OTEL_C:4317" 30; then
    OTEL_ENDPOINT="http://$OTEL_C:4317"
    log "otel collector up"
  else
    log "WARNING: otel collector never listened on 4317; proceeding WITHOUT proxy-side metrics"
    OTEL_ENDPOINT=""
  fi
fi

# ---------------------------------------------------------------------------
# 4. Proxy under test.
# ---------------------------------------------------------------------------
PROXY_HOST=""
if [[ "$PROXY_KIND" == "none-direct" ]]; then
  # No proxy: the generator talks straight to the source. This is the
  # transport-free reference point.
  PROXY_HOST="qual-source:9200"
  FRONT_TLS="$BACK_TLS"
  log "none-direct: generator targets the source itself"
else
  DEST_SCHEME=http; [[ "$BACK_TLS" == 1 ]] && DEST_SCHEME=https
  PX_ARGS=(--listenPort 9201 --destinationUri "$DEST_SCHEME://qual-source:9200" --insecureDestination)
  if [[ "$FRONT_TLS" == 1 ]]; then
    PX_ARGS+=(--sslCertChainFile /certs/proxy-cert.pem --sslKeyFile /certs/proxy-key.pem)
  fi
  if [[ "$CAPTURE" == 1 ]]; then
    PX_ARGS+=(--kafkaBrokers "$KAFKA_BOOTSTRAP" --kafkaTopic "$TOPIC" --kafkaClientId "qual-$RUN")
  else
    PX_ARGS+=(--noCapture)
  fi
  # DISABLE_OTEL=1 runs the proxy with no metrics collector endpoint. Used to
  # quantify how much of the proxy's CPU is its own telemetry, which the
  # profile attributes to the OTel context trackers.
  if [[ -n "$OTEL_ENDPOINT" && "${DISABLE_OTEL:-0}" != 1 ]]; then
    PX_ARGS+=(--otelMetricsCollectorEndpoint "$OTEL_ENDPOINT")
  fi
  # Escape hatch for tuning experiments (e.g. --numThreads). Recorded in
  # meta.json so a tuned run can never be confused with a default-config one.
  if [[ -n "${PROXY_EXTRA_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    PX_ARGS+=(${PROXY_EXTRA_ARGS})
  fi

  # JFR is started at JVM launch so the whole run, warm-up included, is in the
  # recording; the analysis step filters to the measured window. NMT summary is
  # enabled for the memory baseline.
  # The capture_proxy image is amazoncorretto:21-al2023-HEADLESS: its JDK ships
  # only java, keytool and rmiregistry. There is no jcmd and no jfr binary, so
  # a recording cannot be dumped on demand and NMT cannot be queried at
  # runtime. Instead:
  #   * JFR is given an explicit `duration`, which makes the JVM itself write
  #     the file when the duration elapses — no external dump needed. The
  #     recording covers the whole run; the analysis step filters it to the
  #     measured window by event timestamp.
  #   * NMT is printed by the JVM at exit via PrintNMTStatistics, so the proxy
  #     is stopped with SIGTERM (docker stop) rather than SIGKILL and the
  #     summary is read out of the container log.
  JFR_OPTS=""
  if [[ "${ENABLE_PROFILE:-0}" == 1 ]]; then
    JFR_DUR=$(( WARMUP_S + DURATION_S + 30 ))
    # qual.jfc is profile.jfc with ExecutionSample and NativeMethodSample
    # raised from 10ms/20ms to 1ms. At the default rates a ~2.3-core workload
    # yielded under 800 Java samples in 115 s, far too few to attribute CPU.
    JFR_OPTS="-XX:StartFlightRecording=settings=/jfc/qual.jfc,duration=${JFR_DUR}s,filename=/profiles/$RUN.jfr"
    JFR_OPTS="$JFR_OPTS -XX:FlightRecorderOptions=stackdepth=160"
    JFR_OPTS="$JFR_OPTS -XX:NativeMemoryTracking=summary -XX:+UnlockDiagnosticVMOptions"
    JFR_OPTS="$JFR_OPTS -XX:+DebugNonSafepoints -XX:+PrintNMTStatistics"
    mkdir -p "$PROFILES_DIR"
  fi

  # MEASUREMENT_PATCH=1 shadows one class (RootOtelContext) with an out-of-tree
  # recompile that registers a View putting every histogram instrument on
  # sub-millisecond bucket boundaries, so acknowledgement-latency percentiles
  # become resolvable (gate 5). The repository is NOT modified: the patched
  # source is a copy under measurement-patch/src, compiled on the host, and
  # prepended to the image's own classpath at launch. Class loading takes the
  # first match, so /patch wins over /app/libs/coreUtilities-*.jar.
  #
  # HEAP_OPTS lets a run pin an explicit -Xmx so that working set is not
  # decided by -XX:MaxRAMPercentage=80.0's 3.91 GiB reservation (gate 4).
  PATCH_MOUNT=()
  if [[ "${MEASUREMENT_PATCH:-0}" == 1 ]]; then
    [[ -s "$ROOT/measurement-patch/classes/org/opensearch/migrations/tracing/RootOtelContext.class" ]] \
      || die "MEASUREMENT_PATCH=1 but the patched class is not built; see measurement-patch/build.sh"
    PATCH_MOUNT=(-v "$ROOT/measurement-patch/classes":/patch:ro)
    log "MEASUREMENT-ONLY image overlay active: fine histogram buckets"
  fi

  if [[ "$PROXY_KIND" == "rust" && "$CAPTURE" == 1 ]]; then
    # rust-ba76d762e and rust-748379d05 carry a capture-gate defect: ack waits
    # polled with tokio::time::sleep(200us), which the ~1 ms timer wheel rounds
    # up to ~1 ms per wait -- measured ~2x ack-latency inflation. Capture-path
    # numbers from those images must never be recorded. No-capture runs are
    # unaffected (the no-capture path is unchanged).
    case "$RUST_PROXY_IMAGE" in
      *rust-ba76d762e*|*rust-748379d05*|*rust-2ff6080df*)
        die "refusing capture-enabled Rust run on stale image $RUST_PROXY_IMAGE (capture-gate polling defect)";;
    esac
  fi
  if [[ "$PROXY_KIND" == "rust" ]]; then
    # Rust proxy. Same CLI contract as Java (the Java camelCase spellings are
    # live aliases); canonical kebab-case names are used here so a removed
    # alias can never silently change what is being measured. Same cores,
    # same memory limit, same certs, same Kafka topic and OTel endpoint.
    # No heap flags, no class overlay: GATE_LATENCY_BUCKETS_MS is compiled in.
    # The Rust binary binds its listener only after Kafka readiness in capture
    # mode, so wait_tcp success means ready, exactly as for Java.
    RX_ARGS=(--listen-port 9201 --destination-uri "$DEST_SCHEME://qual-source:9200" --insecure-destination)
    [[ "$FRONT_TLS" == 1 ]] && RX_ARGS+=(--ssl-cert-chain-file /certs/proxy-cert.pem --ssl-key-file /certs/proxy-key.pem)
    if [[ "$CAPTURE" == 1 ]]; then
      RX_ARGS+=(--kafka-brokers "$KAFKA_BOOTSTRAP" --kafka-topic "$TOPIC" --kafka-client-id "qual-$RUN")
    else
      RX_ARGS+=(--no-capture)
    fi
    if [[ -n "$OTEL_ENDPOINT" && "${DISABLE_OTEL:-0}" != 1 ]]; then
      RX_ARGS+=(--otel-metrics-collector-endpoint "$OTEL_ENDPOINT")
    fi
    if [[ -n "${PROXY_EXTRA_ARGS:-}" ]]; then
      # shellcheck disable=SC2206
      RX_ARGS+=(${PROXY_EXTRA_ARGS})
    fi
    # Diagnostic only: KAFKA_PROPS_FILE is mounted read-only and passed as the
    # producer property file (e.g. debug=broker,protocol).
    PROPS_MOUNT=()
    if [[ -n "${KAFKA_PROPS_FILE:-}" ]]; then
      PROPS_MOUNT=(-v "$KAFKA_PROPS_FILE":/props/kafka.properties:ro)
      RX_ARGS+=(--kafka-property-file /props/kafka.properties)
    fi
    RUST_CAPS=()
    [[ "${ENABLE_PERF:-0}" == 1 ]] && RUST_CAPS=(--cap-add SYS_ADMIN --cap-add PERFMON --security-opt seccomp=unconfined)
    docker run -d --name "$PX_C" --network "$NET" --network-alias "$PX_C" \
      --cpuset-cpus="$CORES_PROXY" --memory="$PROXY_MEM" \
      -v "$CERTS":/certs:ro ${RUST_CAPS[@]+"${RUST_CAPS[@]}"} ${PROPS_MOUNT[@]+"${PROPS_MOUNT[@]}"} \
      "$RUST_PROXY_IMAGE" "${RX_ARGS[@]}" >/dev/null || die "rust proxy container failed to start"
  elif [[ "${MEASUREMENT_PATCH:-0}" == 1 || -n "${HEAP_OPTS:-}" ]]; then
    docker run -d --name "$PX_C" --network "$NET" --network-alias "$PX_C" \
      --cpuset-cpus="$CORES_PROXY" --memory="$PROXY_MEM" \
      -v "$CERTS":/certs:ro -v "$PROFILES_DIR":/profiles -v "$HARNESS_DIR/jfc":/jfc:ro \
      "${PATCH_MOUNT[@]}" \
      -e JAVA_TOOL_OPTIONS="$JFR_OPTS" \
      --entrypoint sh "$JAVA_PROXY_IMAGE" -c \
      'CP="$(cat /app/jib-classpath-file)"; [ -d /patch ] && CP="/patch:$CP"; exec java -XX:MaxRAMPercentage=80.0 -XX:+ExitOnOutOfMemoryError '"${HEAP_OPTS:-}"' -cp "$CP" org.opensearch.migrations.trafficcapture.proxyserver.CaptureProxy "$@"' \
      sh "${PX_ARGS[@]}" >/dev/null || die "proxy container failed to start"
  else
    docker run -d --name "$PX_C" --network "$NET" --network-alias "$PX_C" \
      --cpuset-cpus="$CORES_PROXY" --memory="$PROXY_MEM" \
      -v "$CERTS":/certs:ro -v "$PROFILES_DIR":/profiles -v "$HARNESS_DIR/jfc":/jfc:ro \
      -e JAVA_TOOL_OPTIONS="$JFR_OPTS" \
      "$JAVA_PROXY_IMAGE" "${PX_ARGS[@]}" >/dev/null || die "proxy container failed to start"
  fi
  wait_tcp "$NET" "$PX_C:9201" 90 || { docker logs "$PX_C" | tail -60 >&2; die "proxy never listened on 9201"; }
  PROXY_HOST="$PX_C:9201"
  log "proxy up: $PROXY_HOST (front_tls=$FRONT_TLS back_tls=$BACK_TLS capture=$CAPTURE)"
fi

# ---------------------------------------------------------------------------
# 5. cgroup sampler sidecar.
# ---------------------------------------------------------------------------
CG_CSV="$OUTDIR/cgroup.csv"
if [[ "$PROXY_KIND" != "none-direct" ]]; then
  PX_CID="$(docker inspect -f '{{.Id}}' "$PX_C")"
  docker run -d --name "$SMP_C" --cgroupns=host --cpuset-cpus="$CORES_INFRA" \
    -v /sys/fs/cgroup:/hostcg:ro \
    -v "$HARNESS_DIR/cgsample.sh":/cgsample.sh:ro \
    -v "$OUTDIR":/out \
    "$ALPINE_IMAGE" sh /cgsample.sh "$PX_CID" "$SAMPLE_INTERVAL" "/out/cgroup.csv" >/dev/null \
    || die "cgroup sampler failed to start"
  sleep 2
  [[ -s "$CG_CSV" ]] || { docker logs "$SMP_C" >&2; die "cgroup sampler produced no output"; }
  log "cgroup sampler running at ${SAMPLE_INTERVAL}s interval"

  # Idle memory: sampled now, after the proxy is listening but before any load.
  IDLE_MEM="$(tail -1 "$CG_CSV" | cut -d, -f8)"
  log "idle memory.current = $IDLE_MEM bytes"
else
  IDLE_MEM="NA"
fi

# Kafka offsets before, so records/s and bytes/s come from the broker's own
# log rather than from the proxy's self-report.
kafka_offsets() {
  [[ "$CAPTURE" == 1 ]] || { echo 0; return; }
  docker exec "$KF_C" /opt/kafka/bin/kafka-get-offsets.sh \
    --bootstrap-server "$KF_C:9092" --topic "$TOPIC" 2>/dev/null \
    | awk -F: '{s+=$3} END{print s+0}'
}
# On-disk bytes for the topic, asked of the broker itself rather than guessed
# from a log-directory path. An earlier version used `du` against two hardcoded
# paths, neither of which is this image's log dir, and silently reported 0.
kafka_log_bytes() {
  [[ "$CAPTURE" == 1 ]] || { echo 0; return; }
  docker exec "$KF_C" /opt/kafka/bin/kafka-log-dirs.sh \
      --bootstrap-server "$KF_C:9092" --topic-list "$TOPIC" --describe 2>/dev/null \
    | tr ',' '\n' | sed -n 's/.*"size":\([0-9]*\).*/\1/p' \
    | awk '{s+=$1} END{print s+0}'
}

otel_scrape() {
  [[ -n "$OTEL_ENDPOINT" ]] || { : > "$1"; return; }
  docker run --rm --network "$NET" --cpuset-cpus="$CORES_INFRA" "$ALPINE_IMAGE" \
    sh -c "wget -q -O- http://$OTEL_C:8889/metrics" > "$1" 2>/dev/null || : > "$1"
}

# ---------------------------------------------------------------------------
# 6. Load.
# ---------------------------------------------------------------------------
GEN_OUT="$OUTDIR/gen.json"
GEN_ARGS=(-mode load -target "$PROXY_HOST" -host "$PROXY_HOST"
  -conns "$CONNS" -rate "$RATE" -churn "$CHURN" -frag-prob "$FRAG_PROB"
  -duration "${DURATION_S}s" -warmup "${WARMUP_S}s" -mix "$MIX"
  -run-id "$RUN" -profile "$PROFILE" -proxy "$PROXY_KIND" -out /out/gen.json)
[[ "$FRONT_TLS" == 1 ]] && GEN_ARGS+=(-tls)
# Concurrent direct-to-source probe lane: a low fixed rate straight to the
# source, running at the same time as the proxy load, so proxy-added latency is
# (proxy quantile - probe quantile) measured under the SAME source load rather
# than against a separately-timed baseline.
if [[ "$PROXY_KIND" != "none-direct" ]]; then
  GEN_ARGS+=(-probe-target qual-source:9200 -probe-rate 50)
  [[ "$BACK_TLS" == 1 ]] && GEN_ARGS+=(-probe-tls)
fi

VM_LOAD_BEFORE="$(vm_loadavg)"
log "vm loadavg before load: $VM_LOAD_BEFORE"

# Mark the warm-up boundary so the CSV can be sliced to the measured window.
T_START_MS=$(( $(date +%s) * 1000 ))

# Kafka offsets and the proxy's own OTel counters MUST be snapshotted at the
# warm-up boundary, not only at the end. These are lifetime cumulative
# counters: dividing an end-of-run total by the measured duration counts every
# warm-up record as if it had been produced inside the measured window, which
# inflated Kafka records/s by the ratio (warmup+duration)/duration — a factor of
# 1.4 at the settings used here. The boundary snapshot is taken by a background
# subshell that sleeps for exactly the generator's warm-up, pinned to the infra
# cores so it cannot steal proxy or generator time.
(
  sleep "$WARMUP_S"
  kafka_offsets > "$OUTDIR/kafka_records_at_warmup.txt" 2>/dev/null || echo 0 > "$OUTDIR/kafka_records_at_warmup.txt"
  kafka_log_bytes > "$OUTDIR/kafka_bytes_at_warmup.txt" 2>/dev/null || echo 0 > "$OUTDIR/kafka_bytes_at_warmup.txt"
  otel_scrape "$OUTDIR/otel-metrics-warmup.prom"
) &
BOUNDARY_PID=$!

docker run --name "$GEN_C" --network "$NET" --cpuset-cpus="$CORES_GEN" --memory=8g \
  -v "$OUTDIR":/out "$GEN_IMAGE" "${GEN_ARGS[@]}" >>"$RUNLOG" 2>&1
GEN_RC=$?
T_END_MS=$(( $(date +%s) * 1000 ))
wait "$BOUNDARY_PID" 2>/dev/null || true
VM_LOAD_AFTER="$(vm_loadavg)"
LOAD_HOST_AFTER="$(host_loadavg)"
log "generator exit=$GEN_RC ; vm loadavg after: $VM_LOAD_AFTER ; host loadavg after: $LOAD_HOST_AFTER"
[[ $GEN_RC -eq 0 ]] || die "generator failed (exit $GEN_RC) — see $RUNLOG"
[[ -s "$GEN_OUT" ]] || die "generator wrote no summary"

KAFKA_RECORDS="$(kafka_offsets)"
KAFKA_BYTES="$(kafka_log_bytes)"
KAFKA_RECORDS_WARMUP="$(cat "$OUTDIR/kafka_records_at_warmup.txt" 2>/dev/null || echo 0)"
KAFKA_BYTES_WARMUP="$(cat "$OUTDIR/kafka_bytes_at_warmup.txt" 2>/dev/null || echo 0)"
PEAK_MEM="NA"; BURST_MEM="NA"
if [[ -f "$CG_CSV" ]]; then
  PEAK_MEM="$(tail -1 "$CG_CSV" | cut -d, -f9)"
fi

# End-of-window OTel scrape happens HERE, before any recovery phase, so that
# differencing against the warm-up scrape covers exactly the measured window.
otel_scrape "$OUTDIR/otel-metrics.prom"
if [[ -s "$OUTDIR/otel-metrics.prom" ]]; then
  log "otel metrics scraped at end of window ($(wc -l < "$OUTDIR/otel-metrics.prom") lines)"
else
  log "WARNING: end-of-window otel scrape was empty"
fi

# Degraded-Kafka recovery check (gate 6): remove the toxic and watch memory
# come back down. Only meaningful when a toxic was applied.
RECOVERY_JSON="null"
if [[ "$CAPTURE" == 1 && "$KAFKA_PATH" == toxi && "$KAFKA_DELAY_MS" -gt 0 ]]; then
  log "removing latency toxic and observing recovery for 45s"
  docker run --rm --network "$NET" --cpuset-cpus="$CORES_INFRA" "$ALPINE_IMAGE" \
    sh -c "wget -q -O- --method=DELETE http://$TX_C:8474/proxies/kafka/toxics/lat_down" >/dev/null 2>&1 || true
  REC_START_MS=$(( $(date +%s) * 1000 ))
  sleep 45
  REC_END_MS=$(( $(date +%s) * 1000 ))
  otel_scrape "$OUTDIR/otel-metrics-recovery.prom"
  REC_RECORDS="$(kafka_offsets)"
  RECOVERY_JSON="{\"rec_start_ms\":$REC_START_MS,\"rec_end_ms\":$REC_END_MS,\"records_after_recovery\":${REC_RECORDS:-0}}"
  log "recovery window observed; records after recovery: ${REC_RECORDS:-0}"
fi

# Wait for the JFR recording to close itself, then stop the JVM gracefully so
# PrintNMTStatistics lands in the container log.
if [[ "${ENABLE_PROFILE:-0}" == 1 && "$PROXY_KIND" == java ]]; then
  JFR_FILE="$PROFILES_DIR/$RUN.jfr"
  for _ in $(seq 1 40); do
    [[ -s "$JFR_FILE" ]] && break
    sleep 2
  done
  if [[ -s "$JFR_FILE" ]]; then
    log "JFR recording written: $(du -h "$JFR_FILE" | awk '{print $1}')"
  else
    log "WARNING: JFR recording is empty or missing at $JFR_FILE"
  fi
  log "stopping proxy with SIGTERM to capture NMT summary"
  docker stop -t 40 "$PX_C" >/dev/null 2>&1 || log "WARNING: docker stop failed"
  docker logs "$PX_C" 2>&1 | sed -n '/Native Memory Tracking/,$p' > "$OUTDIR/nmt-summary.txt" || true
  if [[ -s "$OUTDIR/nmt-summary.txt" ]]; then
    log "NMT summary captured ($(wc -l < "$OUTDIR/nmt-summary.txt") lines)"
  else
    log "WARNING: NMT summary not present in the proxy log"
  fi
fi

# ---------------------------------------------------------------------------
# 7. Per-run JSON summary.
# ---------------------------------------------------------------------------
cat > "$OUTDIR/meta.json" <<EOF
{
  "run_id": "$RUN",
  "profile": "$PROFILE",
  "proxy_kind": "$PROXY_KIND",
  "image": "$( [[ "$PROXY_KIND" == rust ]] && echo "$RUST_PROXY_IMAGE" || echo "$JAVA_PROXY_IMAGE")",
  "image_id": "$( [[ "$PROXY_KIND" == rust ]] && docker image inspect -f '{{.Id}}' "$RUST_PROXY_IMAGE" 2>/dev/null || docker image inspect -f '{{.Id}}' "$JAVA_PROXY_IMAGE" 2>/dev/null)",
  "image_digest": "$( [[ "$PROXY_KIND" == rust ]] && echo "n/a (local image; see image_id)" || awk '{print $2}' "$LOGS/image-digest.txt" 2>/dev/null | tr -d '\r\n')",
  "front_tls": $FRONT_TLS, "back_tls": $BACK_TLS, "capture": $CAPTURE,
  "churn": $CHURN, "frag_prob": $FRAG_PROB, "conns": $CONNS, "rate": $RATE,
  "kafka_path": "$KAFKA_PATH", "kafka_delay_ms": $KAFKA_DELAY_MS,
  "topic": "$TOPIC", "proxy_cores": "$CORES_PROXY", "proxy_cores_n": $PROXY_CORES_N,
  "proxy_mem_limit": "$PROXY_MEM",
  "duration_s": $DURATION_S, "warmup_s": $WARMUP_S,
  "proxy_extra_args": "${PROXY_EXTRA_ARGS:-}",
  "measurement_patch": ${MEASUREMENT_PATCH:-0},
  "kafka_partitions": ${KAFKA_PARTITIONS:-8},
  "kafka_props_file": "${KAFKA_PROPS_FILE:-}",
  "heap_opts": "${HEAP_OPTS:-}",
  "t_start_ms": $T_START_MS, "t_end_ms": $T_END_MS,
  "sample_interval_s": $SAMPLE_INTERVAL,
  "idle_memory_current": "$IDLE_MEM",
  "memory_peak_end": "$PEAK_MEM",
  "kafka_records_total": ${KAFKA_RECORDS:-0},
  "kafka_log_bytes_total": ${KAFKA_BYTES:-0},
  "kafka_records_at_warmup": ${KAFKA_RECORDS_WARMUP:-0},
  "kafka_log_bytes_at_warmup": ${KAFKA_BYTES_WARMUP:-0},
  "recovery": $RECOVERY_JSON,
  "host_loadavg_before": "$LOAD_HOST_BEFORE",
  "host_loadavg_after": "$LOAD_HOST_AFTER",
  "vm_loadavg_before": "$VM_LOAD_BEFORE",
  "vm_loadavg_after": "$VM_LOAD_AFTER"
}
EOF

python3 -I "$HARNESS_DIR/summarize_run.py" "$OUTDIR" > "$OUTDIR/summary.json" \
  || die "summarize_run.py failed"
log "wrote $OUTDIR/summary.json"

# Validate before declaring success. Without this, a run in which the proxy
# died and every request failed to dial still exits 0.
if ! python3 -I "$HARNESS_DIR/validate_run.py" "$OUTDIR" | tee -a "$RUNLOG" >&2; then
  touch "$OUTDIR/REJECTED"
  die "run rejected by validate_run.py — not a usable measurement"
fi
python3 -I -c 'import json,sys;d=json.load(open(sys.argv[1]));print(json.dumps({k:d[k] for k in ("profile","proxy_kind","requests_per_s","cores_used","rps_per_core","p50_ms","p99_ms","proxy_added_p50_ms","proxy_added_p99_ms","steady_ws_mib","peak_ws_mib","kafka_records_per_s","errors_total") if k in d}, indent=2))' "$OUTDIR/summary.json" >&3
