# Phase 1 owner review — Rust capture proxy

Date: 2026-11-16
Status: **DRAFT — Gate 5 acknowledgement-latency verdict pending (corrected re-measurement running)**

## 1. Revisions, images, environment

| Item | Value |
|---|---|
| Baseline | `opensearch-project/opensearch-migrations` `main` @ `059d7b6483f29759b1ee108b87b87c61b0ce3432` (matches the plan's analyzed baseline) |
| Branch | `rust-capture-proxy` |
| Phase 1 head | `03ddf2ab2` |
| Measurement images | Gates 2–4, 6: `rust-b60dc220e` (`sha256:3e3cd935…3b62`), the first image recording the same instrument set as Java; Gate 5 (re-measure) and confirmations: `rust-03ddf2ab2` (`sha256:90f39c70…1ddb`). Earlier primary results on `rust-748379d05` (Gate 2) and `rust-2ff6080df` (Gate 4) are reported with their confirmations. linux/arm64, AL2023, 62.6 MB image, 13.7 MB stripped binary, user 10001 |
| Java comparison image | `localhost:5001/migrations/capture_proxy:latest_arm64`, `sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357`, Jib `:TrafficCapture:trafficCaptureProxyServer:jib_arm64` at the baseline |
| Host | Apple M5 Pro, 18 cores, 64 GiB, macOS (Darwin 25.6.0), Docker Desktop 29.7.2 Linux/arm64 VM |
| Toolchains | Rust 1.97.1, Corretto 21.0.12, kafka-clients 4.2.0 (Java), librdkafka 2.12.1 via rdkafka 0.39 |
| Kafka | Apache Kafka 4.0.0 KRaft single broker per test, `message.timestamp.type=LogAppendTime`; plus a live AWS MSK Serverless cluster for IAM |

The commits after `2ff6080df` (`6d9dac9ed`, `6a2edd703`) change Kafka TLS property translation and
the capture-compromise paths only; neither is exercised by the steady-state performance profiles.

Size: 48.2K lines of Rust under `crates/*/src`, 7.3K under `crates/*/tests`. Workspace: **895+ tests
passing, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo fmt --check` clean**
(`unsafe_code` forbidden workspace-wide).

Host honesty: an arm64 macOS Docker VM is not the production Linux/EKS topology. It cannot
reproduce the 128-pod, 152.5K req/s fleet, a 48-node source, a replicated multi-broker Kafka, or
production cAdvisor. It **does** reproduce production's per-pod behaviour closely (§5.1), and every
ratio below is a paired Java/Rust comparison on identical topology.

## 2. Summary

All ratios are Rust ÷ Java within interleaved pairs (order alternated), every run validated; CI95 in
brackets. Proxy pinned to 5 cores / 5,000 MiB, same topology for both arms.

| Gate | Requirement | Result | Pairs clearing | Verdict |
|---|---|---|---|---|
| 1 | Zero exercised WIRE or PROTOCOL divergence | 3 Rust passes, 0 differences vs calibrated Java (§3) | — | **PASS** |
| 2 | Plaintext scan-and-forward req/s per used core ≥ 1.20 | **2.93** [2.80–3.05]; confirmations 3.04 and 2.83 on later images | 9/9 | **PASS** |
| 3 | Double TLS: req/s per core ≥ 0.95; proxy-added p99 ≤ 1.05 | **1.54** [1.45–1.62]; p99 **0.54** [0.49–0.60] | 10/10, 10/10 | **PASS** |
| 3 | 23% churn + TLS: same | **1.27** [1.21–1.34]; p99 **0.56** [0.52–0.60] | 10/10, 10/10 | **PASS** |
| 4 | Capture slice req/s per core ≥ 1.15; steady RSS ≤ 0.75 | **1.46** [1.40–1.51]; anon **0.097** [0.090–0.103] | 10/10, 10/10 | **PASS** |
| 5 | Kafka records/core and MiB/core ≥ 0.90 | **1.38** [1.33–1.42] / **1.38** [1.34–1.43] | 10/10 | **PASS** |
| 5 | Kafka ack p99 ≤ 1.10 | As first measured: 1.47 [1.38–1.55] — **invalid**, see §5.4. Corrected re-measurement: PENDING | 0/10 | **PENDING** |
| 6 | Delayed Kafka: bounded memory, lossless backpressure, recovery | Peak 15.4 MiB (Java 347), 0 errors, recovers to 12.5 MiB | — | **PASS**, with a latency finding (§5.5) |

## 3. WIRE and PROTOCOL evidence

### 3.1 Java replayer differential (Gate 1)

Method (`/tmp/rust-proxy-qual/specs/wire-proof.md`): a fixed corpus (seed `20261115`, digest
`5d9597e4…4cf5`; keep-alive GETs, POST/PUT/DELETE/PATCH critical mutations, chunked + trailers,
`Expect: 100-continue`, a policy-suppressed request between two captured ones, rich headers, seeded
1–6-segment TCP fragmentation, driven over raw sockets) is sent through each proxy into its own
topic, and each topic is read back by the **unchanged Java replayer** in `dump-both` mode. Transcripts
and an independent decoder's canonical observation stream are compared per connection.

Calibration: six Java passes (including a second, independent Kafka cluster) are mutually identical
after normalisation. A third and fourth Java pass were needed to find a TCP-read-count
nondeterminism that two passes did not reveal. **Negative control:** a single added corpus byte
leaves all per-run assertions passing but is caught by the differential comparison in three
independent views — so the differential, not the per-run assertions, decides Gate 1.

| Comparison | Image | Total differences |
|---|---|---:|
| Java j1 vs Rust r1 | `rust-ba76d762e` | **0** |
| Java j1 vs Rust r2 | `rust-748379d05` (F flush live) | **0** |
| Rust r1 vs Rust r2 | — | **0** |
| Java j1 vs Rust r3 | `rust-2ff6080df` (measurement image) | **0** |

Per-run assertions, all 12 PASS on every Rust pass: every value decodes as `CaptureRecord` with
exactly one payload; **zero Kafka headers**; key = raw `connectionId`; every timestamp **type** is
`LogAppendTime`; one partition per connection; index order agrees with offset order;
`connectionObservationSequence` present and exactly +1; one `CloseObservation` per connection, last in
stream and in the final record; request (11 / 4,549 B), response (4,279 B) and informational (25 B)
bytes **byte-exact**; one `requestForwarded` per end-of-message in order; the suppressed request yields
a `requestDropped` after its captured prefix and no `requestForwarded`.

Replayer reconstruction, Java and Rust identical: **11 REQ, 11 RSP, 1 informational, 5 CLOSED**, zero
replayer exceptions or protocol violations. Record packing happened to be identical too
(c0=1, c1=5, c2=2, c3=2, c4=1, probe=1), although the J-1 envelope deviation permits it to differ.

Segmentation through the Java replayer: a 3 MiB response (forcing size-triggered detaches and
segmented write observations) captured by Rust reconstructs as `RSP[3145863]` (3,145,728 body + 135
header bytes) plus its `REQ` and `CLOSED`.

### 3.2 Protocol behaviour against real Kafka

| Obligation | Evidence | Result |
|---|---|---|
| Java Murmur2 routing over sorted assignment | 68 ids × 8 partition sets generated by kafka-clients 4.2.0 `Utils`: 68 hashes, **544 routing decisions, 0 mismatches** | PASS |
| Broker-assigned time | Probe accepts only `Timestamp::LogAppendTime(t>0)`; a CreateTime topic returned a **positive** `CreateTime(1794802821323)` on Kafka 4.0.0 — the documented "positive timestamp" check would have passed it (OD-4) | PASS |
| CreateTime topic refuses startup (OD-5) | Real broker; listener never binds | PASS |
| Listener opens only after readiness | Dead broker: port polled throughout startup never binds; exit 78 | PASS |
| Capture-before-forward | Mutation test: removing the publication wait fails exactly the test asserting "the gate forwarded a mutation that Kafka had not acknowledged" | PASS |
| Multi-replica rebalance (§3.4/§3.6) | Two proxies, 8 partitions: A gets all 8 (A:1); B joins → A:2 = [0–3], B:1 = [4–7]; A's old connections keep A:1 and their original partitions (incl. 4 and 6, now B's) — verified in logs **and** decoded Kafka records; A:1 heartbeats every 2 s while draining and each partition retires within 12 ms of its last connection's close, never before; both exit 0 `completed`; **Java replayer: 23 REQ / 23 RSP / 10 CLOSED**, matching 23 requests on 10 connections | PASS |
| fail-closed under broker failure (§7.2) | Broker paused, H=2 E=6: exit **78** at 4.89 s; the waiting PUT **never reached the origin** (origin logs every request before responding) | PASS after fix (§4) |
| fail-open under broker failure (§7.3) | Process stays up; waiting PUT, new-connection PUT/GET during the outage and after recovery all forwarded; one-way transition logged once; SIGTERM 0.17 s, `outcome=incomplete`, retirement not claimed | PASS after fix (§4) |
| F periodic flush (§4.2) | Idle partial request appended **2003 ms** after send with F = 2000 ms, connection still open | PASS after fix (§4) |
| Terminal record + deregistration (§5.1) | 3 MiB response then source close: 3/3 `outcome=completed`, writer retired (was 3/3 exit 70) | PASS after fix (§4) |
| Effective producer config | Read back from librdkafka: `enable.idempotence=true`, `acks=-1`, `max.in.flight=5` | PASS |
| No admin operations (OD-7) | Layout from producer metadata only; zero admin calls on a real broker | PASS |

### 3.3 Authentication

| Mode | Evidence | Result |
|---|---|---|
| none | All real-broker tests above | PASS |
| TLS (`ssl`) | Real SSL listener, orchestration's exact `client.properties` shape: capture + PUT/GET 200, exit 0; missing CA → refuses, exit 78 | PASS after fix (§4) |
| SCRAM-SHA-512 | Real SASL_SSL listener: capture + PUT/GET 200, exit 0; wrong password → refuses, exit 78 | PASS after fix (§4) |
| MSK IAM | **Proxy release code, run on an in-VPC EC2 client against the live MSK Serverless cluster**, credentials from the instance profile via IMDSv2 through `aws-config`'s default chain (no static keys in the environment): capture-ready in 4.4 s, assignment `[0, 1, 2]`, PUT and GET 200 through the capture gate, SIGTERM exit 0 `outcome=completed`, 3 writer lanes retired. The CreateTime topic on the same cluster refused startup in 1.6 s with exit 78 and never bound: *"the capability probe for partition 2 was acknowledged with CreateTime(1794840361641), so broker time cannot be proven and capture would be unsound"* (OD-5). Built from commit `6d9dac9ed` shipped as a git bundle over SSM; no AWS resources created for this step | PASS |
| MSK IAM from EKS IRSA | Same code path as IMDS (`aws-config` default chain), but the web-identity provider, SA→role trust, pod reachability and token-rotation soak are unproved; EKS changes were not authorised | NOT RUN |

## 4. Defects found and fixed during Phase 1

Each was found by a real-component test, root-caused in source, fixed, and re-proved by the same
test. None required a WIRE or PROTOCOL deviation.

| # | Defect | Impact | Commit |
|---|---|---|---|
| 1 | F flush deadline had no driver (prior attempt's blocker #3, recurring) | idle records waited for the next observation | `3bcf18e81`, `748379d05` |
| 2 | Terminal record stranded behind an in-flight record when the connection task exited | lost `CloseObservation`, connection never deregistered, shutdown exit 70 after 72 s | `9568cb3b0` |
| 3 | Capture gate polled with a "200 µs" sleep that tokio's ~1 ms timer resolution turned into ~1 ms | critical-mutation latency **1.47 → 0.73 ms** median after fix; would have roughly halved capture throughput | `2ff6080df` |
| 4 | Java `ssl.truststore.*` keys passed to librdkafka | **every SCRAM deployment refused to start** (`No such configuration property: ssl.truststore.type`) | `6d9dac9ed` |
| 5 | fail-closed never terminated | process alive, listener open, refusing traffic | `6a2edd703` |
| 6 | fail-open refused new connections | violated unmanaged pass-through | `6a2edd703` |
| 7 | No bridge from Kafka terminal failure to the capture gate | an idle proxy's heartbeat expiry left it silently refusing traffic | `6a2edd703` |
| 8 | librdkafka `linger.ms` default 5 ms vs Java 0 | 4–9× throughput regression on the measured ack-latency curve | `b00c8670d` |
| 9 | librdkafka `message.max.bytes` default 1,000,000 < 1,048,076 default payload limit | maximum-size record = terminal capture failure out of the box | `b00c8670d` |

Java defects found and deliberately not reproduced (owner rulings OD-1…OD-10): malformed framing
tunnelled uncaptured past the capture gate (incl. any >4096-byte request line); header removal leaked
the removed header when a 100-continue arrived mid-head; the `CaptureRecord` envelope was never
budgeted; backward broker time regressed the heartbeat baseline; whitespace before a header colon
was folded into the header name (desync shape).

## 5. Performance

### 5.1 Java baseline and its fidelity to production

PENDING final table. Fidelity, from the Phase 0 Java scorecard: at 23.01% new-connection churn the
Java proxy used **3.512 cores** (production: 3.54/pod, 1%), Kafka byte amplification **1.045×**
(production: ~1.05×).

### 5.2 Method

Interleaved paired A/B (`/tmp/rust-proxy-qual/harness/run-paired.sh`, `paired_ratio.py`), order
alternated within pairs, every run through `validate_run.py`, ratio computed within each pair. A gate
passes only when the paired median clears it, ≥80% of pairs clear it, and the CI95 lies entirely on
the passing side; otherwise INCONCLUSIVE. Pairing did **not** collapse variance as first assumed —
within-pair noise is independent — so resolution is bought with pair count, priced per gate.

### 5.3 Throughput, CPU, memory (decided gates)

| Profile | Java req/s/core | Rust req/s/core | Ratio | Cores used J → R | Proxy-added p99 J → R | Anon MiB J → R |
|---|---:|---:|---:|---|---|---|
| Plaintext no-capture (G2) | 21,102 | 61,561 | 2.93 | 1.75 → 1.88 | 4.52 → 0.66 ms | 233 → 6.2 |
| Plaintext capture (G4) | 12,189 | 17,673 | 1.46 | 2.38 → 2.40 | 6.19 → 4.13 ms | 484 → 46.6 |
| Double TLS (G3) | 10,906 | 16,420 | 1.54 | 2.31 → 2.65 | 6.67 → 3.64 ms | 473 → 48.7 |
| 23% churn + TLS (G3) | 1,823 | 2,369 | 1.27 | 3.65 → 4.23 | 23.76 → 13.32 ms | 487 → 30.7 |
| Kafka healthy (G5) | 7,848 | 10,647 | 1.37 | 2.49 → 2.67 | 8.71 → 5.63 ms | 482 → 40.3 |

Proxy-added p99 here is the load generator's exact per-request timing, minus the direct-to-source
baseline measured under the same conditions. On the TLS and churn profiles Rust used 14–16% more
cores than Java, so part of its absolute gain is parallelism; the per-core ratio is what the gate
measures. On the capture profile cores used were equal (0.996), so the gain there is efficiency.

Gate 4 is measured with Java at its default heap, which is the harder comparison for Rust. A
pinned-heap Java arm (`-Xms536m -Xmx536m`, working set 622 MiB = 93.5% of production's 0.65 GiB)
corroborates the memory result (0.074); its throughput result is inconclusive at 4 valid pairs.

### 5.4 Gate 5 acknowledgement latency — first measurement invalid

The first measurement (`rust-b60dc220e`) gave Rust ack p99 7.31 ms against Java 4.91 ms (1.47), ack
mean 2.05 against 1.31 ms. It is reported here as measured, and it is **not** a valid verdict, for
two reasons established in source and data:

1. **Instrument defect (fixed in `03ddf2ab2`).** The Rust image sampled delivery latency twice per
   record: on librdkafka's delivery-callback thread (Java `KafkaRecordContext`'s send→callback span)
   and again on the publisher actor three thread hops later. `kafkaCommitCount` was double-counted
   and the histogram mixed both samples, with the late ones dominating the tail.
2. **Unequal offered load.** The generator is closed-loop, so the faster proxy drove the shared
   single broker harder: 28,432 vs 19,430 req/s (1.48×). Broker acknowledgement latency rises with
   load. On the churn profile, where Rust offered only 1.26× Java's records/s, the ack-p99 ratio was
   1.017 with a CI including 1.0.

The corrected re-measurement on `rust-03ddf2ab2` runs both saturating pairs and pairs rate-matched
at 15,000 req/s on both arms: PENDING.

### 5.5 Gate 6 and a latency finding

Under a 750 ms broker→client delay (toxiproxy), 5 pairs:

| | Java | Rust |
|---|---:|---:|
| Generator errors | 0 | 0 |
| Steady / peak / anon memory | 336 / 347 / 330 MiB | 14.0 / 15.4 / 9.4 MiB |
| After removing the delay | 345.5 MiB | 12.5 MiB |
| Ack mean | 811 ms | **1,509 ms** |
| Client p50 | 760 ms | 1,510 ms |
| Throughput | 95.7 req/s | 54.7 req/s (0.57×) |

Memory is bounded far below the 64 MiB publication budget, delivery is lossless, and memory recovers:
the gate's clauses hold. But Rust waits about **two** delayed broker round trips per acknowledgement
where Java waits one. That halves throughput whenever acknowledgements are slow, and it may also
contribute to the healthy-broker gap above.

Not the cause: records per request are identical (0.80 on both), and the publisher actor holds
traffic only when its budget is exhausted, which it was not (one record awaiting acknowledgement).
Leading hypothesis, under test: librdkafka builds one ProduceRequest per partition while the Java
client packs every partition led by a broker into one request, so with the forced
`max.in.flight.requests.per.connection=5` and an 8-partition topic on one broker, partitions beyond the
fifth wait a full round trip. Diagnostic runs at 4 vs 8 partitions and with librdkafka's protocol
debug log: PENDING.

### 5.6 CPU profiles

**Java** (JFR, Phase 0): 49.9% of Java-thread CPU in telemetry, 23.1% in `ActiveContextTracker`
sorted-map work, publisher plus producer network threads 44.5% on two threads; 45% of allocation is
protobuf `byte[]`.

**Rust** (`perf` 6.6.31, profiling build of `rust-b60dc220e`, plaintext capture, 101,379 samples, 0
lost; attribution only): 60.2% kernel, 21.5% proxy binary, 9.4% libc, 8.8% clock reads (vDSO).

| Hot spot | Share | Note |
|---|---:|---|
| Cross-thread wake-ups (`try_to_wake_up` 13.96%, `__wake_up_sync_key` 6.38%) | ~20% | Per-record hand-offs between tokio workers, the publisher actor, and librdkafka threads — the largest optimisation target |
| `crc32c_sw` | 3.1% | librdkafka uses software CRC32C on arm64; on x86_64 it uses SSE4.2, so this is architecture-specific |
| HTTP parsing | 3.2% | |
| Tokio runtime | 3.1% | |
| Kafka publishing | 2.5% | |
| OpenTelemetry | 1.4% | vs ~50% in Java |
| Protobuf encoding | 0.8% | |
| Docker bridge connection tracking | ~5% | Same cost for Java |

### 5.7 Measurement-integrity notes

- 14 harness defects found and fixed, including a run where a dead proxy's 761,216 failed dials was
  recorded `OK`, an interrupted run that exited 0 without a summary, and a script edited while it ran.
  Every reported run passed `validate_run.py`, re-checked per arm by the pair runner.
- Pairing did not collapse per-run variance (it is independent noise, not common-mode drift); the
  design's value is that drift cannot bias the ratio. Resolution came from pair count.
- The first Rust images recorded fewer instruments than Java; results on them are reported only with
  confirmation pairs on instrument-parity images.
- Some of my own e2e tests and builds ran on the host during early Gate 2/4 pairs; alternation guards
  against bias, not against added variance.

## 6. Open items and risks

PENDING.

## 7. Recommendation

PENDING.
