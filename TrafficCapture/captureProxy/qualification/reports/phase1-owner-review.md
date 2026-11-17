# Phase 1 owner review — Rust capture proxy

Date: 2026-11-16
Status: **FOR OWNER REVIEW. Phase 2 is not started and will not start without explicit approval.**

## 1. Revisions, images, environment

| Item | Value |
|---|---|
| Baseline | `opensearch-project/opensearch-migrations` `main` @ `059d7b6483f29759b1ee108b87b87c61b0ce3432` (matches the plan's analyzed baseline) |
| Branch | `rust-capture-proxy` |
| Phase 1 code head | `03ddf2ab2` (later commits are reports and harness only) |
| Measurement images | Gates 2–4, 6: `rust-b60dc220e` (`sha256:3e3cd935…3b62`), the first image recording the same instrument set as Java. Gate 5, production shape, fixed-rate runs and confirmations: `rust-03ddf2ab2` (`sha256:90f39c70…1ddb`). Earlier primary results on `rust-748379d05` (Gate 2) and `rust-2ff6080df` (Gate 4) are reported with their confirmations. linux/arm64, AL2023, 62.6 MB image, 13.7 MB stripped binary, user 10001 |
| Java comparison image | `localhost:5001/migrations/capture_proxy:latest_arm64`, `sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357`, Jib `:TrafficCapture:trafficCaptureProxyServer:jib_arm64` at the baseline |
| Host | Apple M5 Pro, 18 cores, 64 GiB, macOS (Darwin 25.6.0), Docker Desktop 29.7.2 Linux/arm64 VM |
| Toolchains | Rust 1.97.1, Corretto 21.0.12, kafka-clients 4.2.0 (Java), librdkafka 2.12.1 via rdkafka 0.39 |
| Kafka | Apache Kafka 4.0.0 KRaft single broker per test, `message.timestamp.type=LogAppendTime`; plus a live AWS MSK Serverless cluster for IAM (torn down) |
| Harness | `qualification/perf/` (committed with this report): generator, runners, validator, paired-ratio analysis, and the full measurement specification |

The commits after `2ff6080df` (`6d9dac9ed`, `6a2edd703`) change Kafka TLS property translation and
the capture-compromise paths only; neither is exercised by the steady-state performance profiles.
`03ddf2ab2` adds the HTTP/2 tunnel (OD-11), the remaining connection metrics, and removes a
duplicate ack-latency sample; Gate 2 and Gate 4 were re-confirmed on it (§5.3).

Size: 48.2K lines of Rust under `crates/*/src`, 7.3K under `crates/*/tests`. Workspace at the code head: **906 tests
passing, 0 failing, 10 ignored** (9 need a real broker or MSK and were run as the real-component
proofs in §3; 1 prints a snapshot on demand). `cargo clippy --workspace --all-targets --locked -- -D
warnings` and `cargo fmt --check` are clean. `unsafe_code` is forbidden workspace-wide.

Host honesty: an arm64 macOS Docker VM is not the production Linux/EKS topology. It cannot
reproduce the 128-pod, 152.5K req/s fleet, a 48-node source, a replicated multi-broker Kafka, or
production cAdvisor. It **does** reproduce production's per-pod behaviour closely (§5.1), and every
ratio below is a paired Java/Rust comparison on identical topology.

## 2. Summary

All ratios are Rust ÷ Java within interleaved pairs (order alternated), every run validated; CI95 in
brackets. Proxy pinned to 5 cores / 5,000 MiB, same topology for both arms.

### 2.1 The six Phase 1 gates

| Gate | Requirement | Result | Pairs clearing | Verdict |
|---|---|---|---|---|
| 1 | Zero exercised WIRE or PROTOCOL divergence | 3 Rust passes, 0 differences vs calibrated Java (§3) | — | **PASS** |
| 2 | Plaintext scan-and-forward req/s per used core ≥ 1.20 | **2.93** [2.80–3.05]; confirmations 3.04, 2.83, and **2.81** [2.56–3.06] on `03ddf2ab2` | 9/9, 3/3 | **PASS** |
| 3 | Double TLS: req/s per core ≥ 0.95; proxy-added p99 ≤ 1.05 | **1.54** [1.45–1.62]; p99 **0.54** [0.49–0.60] | 10/10, 10/10 | **PASS** |
| 3 | 23% churn + TLS: same | **1.27** [1.21–1.34]; p99 **0.56** [0.52–0.60] | 10/10, 10/10 | **PASS** |
| 4 | Capture slice req/s per core ≥ 1.15; steady RSS ≤ 0.75 | **1.46** [1.40–1.51]; anon **0.097** [0.090–0.103]; confirmation on `03ddf2ab2` 1.36 [1.18–1.54], anon 0.082 | 10/10, 3/3 | **PASS** as specified (flat out); 0.93 at 70% load (§5.8) |
| 5 | Kafka records/core and MiB/core ≥ 0.90 | **1.37** [1.31–1.43] / **1.37** [1.31–1.43] saturating; **1.09** [1.02–1.16] / **1.09** [1.02–1.16] rate-matched | 10/10, 10/10 | **PASS** |
| 5 | Kafka ack p99 ≤ 1.10 | **1.36** [1.20–1.53] saturating; **1.45** [1.21–1.68] rate-matched (§5.4) | 1/10, 3/10 | **FAIL** |
| 6 | Delayed Kafka: bounded memory, lossless backpressure, recovery | Peak 15.4 MiB (Java 347), 0 errors, recovers to 12.5 MiB | — | **PASS** on its clauses, with a **2× latency defect** (§5.5) |

### 2.2 Toward the final release gates (not Phase 1 gates; reported because they change the picture)

| Measure | Result | Reading |
|---|---|---|
| Production shape at saturation (TLS both sides, 23% churn, capture) | req/s per core **1.85** [1.79–1.92]; proxy-added p99 **0.43** [0.40–0.47]; anon **0.056** | 5/5 pairs; clears F2's ≥1.30 and F4 at saturation |
| Production shape at a fixed 2,848 req/s (70% of Java's saturation, as production runs) | req/s per core **2.03** [1.73–2.34]; cores **0.49×** (2.67 → 1.49); proxy-added p99 **0.43** [0.35–0.50]; ack p99 **0.53** [0.38–0.69]; anon **0.046** | 5/5 pairs on every metric; the closest predictor of F2–F4 this host can produce |
| Plaintext capture at a fixed 11,371 req/s (70% of Java's saturation) | req/s per core **0.93** [0.80–1.06]; cores **1.07×**; ack p99 **1.52** [1.06–1.97]; anon 0.054 | 1/5 pairs ≥ 1.15. **The Gate 4 comparison, at production-like utilisation, fails** (§5.8) |
| Kafka healthy at a fixed 15K req/s (~81% of Java's saturation) | req/s per core **1.09** [1.02–1.16] | §5.8 |

**Plainly stated failures.**

1. Gate 5's acknowledgement-latency clause fails at both saturating and equal load.
2. Gate 6 passes its clauses but exposes a defect: under a slow broker Rust waits two broker round
   trips per acknowledgement where Java waits one. (1) and (2) share one confirmed root cause, the
   librdkafka in-flight window (§5.5).
3. Gates 2–5 are specified flat out, and on the **capture path** that hides a load sensitivity. At
   70–81% of Java's saturation, Rust's requests per core fall to **0.93–1.09×** Java's. Below
   saturation, Rust's capture path is no more CPU-efficient than Java's (§5.8).

At the production shape (TLS both sides, 23% churn) and a production-like fixed rate, the same build
beats Java on every measured axis, including 2.03× per core and 0.53× ack p99. That advantage comes
from TLS and connection handling, not from the capture path.

## 3. WIRE and PROTOCOL evidence

### 3.1 Java replayer differential (Gate 1)

Method (`qualification/perf/specs/wire-proof.md`): a fixed corpus (seed `20261115`, digest
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

`03ddf2ab2` was not re-run through the differential. Its changes are outside the recorded stream for
HTTP/1 (metrics only, and one fewer histogram sample); the HTTP/2 tunnel it adds records only the
connection's terminal `CloseObservation`, as Java does. A Phase 2 re-run of the differential on the
final image is planned regardless.

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
| HTTP/2 prior knowledge (OD-11) | Exact 24-byte preface tunnelled byte-exact both ways, nothing scanned or captured (unit tests, capture mode); live binary (`e2e/h2c_prior_knowledge_probe.py`): *"client sent 50 bytes; origin received 50 bytes; identical: True"*, server SETTINGS returned; malformed HTTP/1 still fails closed | PASS |
| Effective producer config | Read back from librdkafka: `enable.idempotence=true`, `acks=-1`, `max.in.flight=5`; `linger.ms=0` and `message.max.bytes` forced by `client_config` (unit-tested) | PASS |
| No admin operations (OD-7) | Layout from producer metadata only; zero admin calls on a real broker | PASS |

Not run in Phase 1: a 3-broker cluster with a partition **leadership change** under load. A single
broker cannot exercise it. Phase 3 owns it.

### 3.3 Authentication

| Mode | Evidence | Result |
|---|---|---|
| none | All real-broker tests above | PASS |
| TLS (`ssl`) | Real SSL listener, orchestration's exact `client.properties` shape: capture + PUT/GET 200, exit 0; missing CA → refuses, exit 78 | PASS after fix (§4) |
| SCRAM-SHA-512 | Real SASL_SSL listener: capture + PUT/GET 200, exit 0; wrong password → refuses, exit 78 | PASS after fix (§4) |
| MSK IAM | **Proxy release code, run on an in-VPC EC2 client against the live MSK Serverless cluster**, credentials from the instance profile via IMDSv2 through `aws-config`'s default chain (no static keys in the environment): capture-ready in 4.4 s, assignment `[0, 1, 2]`, PUT and GET 200 through the capture gate, SIGTERM exit 0 `outcome=completed`, 3 writer lanes retired. The CreateTime topic on the same cluster refused startup in 1.6 s with exit 78 and never bound: *"the capability probe for partition 2 was acknowledged with CreateTime(1794840361641), so broker time cannot be proven and capture would be unsound"* (OD-5). Built from commit `6d9dac9ed` shipped as a git bundle over SSM; no AWS resources created for this step. All MSK and EC2 test resources have since been deleted and verified gone | PASS |
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
| 10 | h2c prior-knowledge connections closed as malformed (origin got 0 bytes) where Java forwards them | production surprise for any HTTP/2 client | `03ddf2ab2` (OD-11) |
| 11 | Ack latency sampled twice per record (callback thread and actor) | double-counted `kafkaCommitCount`, inflated tail | `03ddf2ab2` |

Java defects found and deliberately not reproduced (owner rulings OD-1…OD-10): malformed framing
tunnelled uncaptured past the capture gate (incl. any >4096-byte request line); header removal leaked
the removed header when a 100-continue arrived mid-head; the `CaptureRecord` envelope was never
budgeted; backward broker time regressed the heartbeat baseline; whitespace before a header colon
was folded into the header name (desync shape).

## 5. Performance

### 5.1 Java baseline and its fidelity to production

The Java columns of §5.3 are the Java baseline, re-measured inside every pair rather than taken
from the Phase 0 scorecard (`qualification/perf/specs/perf-baseline-spec.md` §4, 38 runs, 8 profiles,
≥5 reps). Fidelity, from that scorecard: at 23.01% new-connection churn the Java proxy used
**3.512 cores** (production: 3.54/pod, within 1%), Kafka byte amplification **1.045×** (production:
~1.05×). Production pods run at 3.54 of a 5-core limit (~71%); the fixed-rate production-shape
pairs (§5.8) are set at 70% of the Java arm's measured saturation to match.

### 5.2 Method

Interleaved paired A/B (`qualification/perf/run-paired.sh`, `paired_ratio.py`), order alternated
within pairs, every run through `validate_run.py`, ratio computed within each pair. A gate passes only
when the paired median clears it, ≥80% of pairs clear it, and the CI95 lies entirely on the passing
side; it fails when the median is on the failing side with the CI95 excluding the threshold; otherwise
INCONCLUSIVE. Pairing did **not** collapse variance as first assumed — within-pair noise is
independent — so resolution is bought with pair count, priced per gate.

Topology: disjoint core sets for proxy (5 cores, 5,000 MiB), generator, source, Kafka and
infrastructure; the purpose-built Go generator is closed-loop by default and open-loop at a fixed rate
for "rate-matched" runs; CPU and memory from cgroup v2 via a sidecar sampler at 1 s; proxy-added
latency is the generator's exact per-request timing minus a concurrent direct-to-source probe;
ack latency is each proxy's own send→delivery-callback histogram with identical buckets
(`GATE_LATENCY_BUCKETS_MS`). Java's stock buckets are too coarse to resolve a 10% p99 difference, so
Java arms that report ack latency run with a **measurement-only** classpath overlay
(`MEASUREMENT_PATCH=1`, `qualification/perf/measurement-patch/`) that replaces only the histogram
bucket boundaries in `RootOtelContext`; no other Java code changes.

Reproduce one gate, e.g. Gate 5 rate-matched:

```bash
cd TrafficCapture/captureProxy/qualification/perf
ARM_A_ENV=$'MEASUREMENT_PATCH=1\nRATE_OVERRIDE=15000' ARM_B_ENV=$'RATE_OVERRIDE=15000' \
  ./run-paired.sh g5v4rate kafka-healthy 10 java rust 30 15
python3 -I paired_ratio.py /tmp/rust-proxy-qual/results/paired-g5v4rate-kafka-healthy.manifest \
  --gate kafka_ack_p99_ms:le:1.10 --gate kafka_records_per_s_per_core:ge:0.90
```

### 5.3 Throughput, CPU, memory

Medians of the paired arms. Ratio is the paired median, not the ratio of medians.

| Profile | Image | Java req/s/core | Rust req/s/core | Ratio [CI95] | Cores used J → R | Proxy-added p99 J → R | Anon MiB J → R |
|---|---|---:|---:|---|---|---|---|
| Plaintext no-capture (G2) | b60dc220e | 21,102 | 61,561 | 2.93 [2.80–3.05] | 1.75 → 1.88 | 4.52 → 0.66 ms | 233 → 6.2 |
| Plaintext no-capture (G2 confirm) | 03ddf2ab2 | 10,988 | 30,831 | 2.81 [2.56–3.06] | 1.79 → 2.29 | — | — |
| Plaintext capture (G4) | b60dc220e | 12,189 | 17,673 | 1.46 [1.40–1.51] | 2.38 → 2.40 | 6.19 → 4.13 ms | 484 → 46.6 |
| Plaintext capture (G4 confirm) | 03ddf2ab2 | 6,766 | 9,580 | 1.36 [1.18–1.54] | 2.41 → 2.45 | — | 482 → 39.7 |
| Double TLS (G3) | b60dc220e | 10,906 | 16,420 | 1.54 [1.45–1.62] | 2.31 → 2.65 | 6.67 → 3.64 ms | 473 → 48.7 |
| 23% churn + TLS (G3) | b60dc220e | 1,823 | 2,369 | 1.27 [1.21–1.34] | 3.65 → 4.23 | 23.76 → 13.32 ms | 487 → 30.7 |
| Kafka healthy, saturating (G5) | 03ddf2ab2 | 7,680 | 10,547 | 1.37 [1.31–1.43] | 2.42 → 2.60 | 10.25 → 6.98 ms | 491 → 40.1 |
| Kafka healthy, 15K req/s (G5) | 03ddf2ab2 | 6,226 | 7,144 | **1.09** [1.02–1.16] | 2.27 → 2.10 | 11.60 → 8.12 ms | 487 → 27.3 |
| Production shape, saturating | 03ddf2ab2 | 1,092 | 1,959 | 1.85 [1.79–1.92] | 3.79 → 3.97 | 34.50 → 15.48 ms | 541 → 30.4 |
| Production shape, 2,848 req/s | 03ddf2ab2 | 1,066 | 1,915 | **2.03** [1.73–2.34] | 2.67 → 1.49 | 29.70 → 13.56 ms | 540 → 25.1 |
| Plaintext capture, 11,371 req/s | 03ddf2ab2 | 7,241 | 6,549 | **0.93** [0.80–1.06] | 1.57 → 1.74 | 8.01 → 7.86 ms | 482 → 25.9 |

The `03ddf2ab2` G2/G4 confirmations (3 pairs each) ran with a host load average of 8–16 from other
work, and their absolute req/s/core is about half the primaries'; their **ratios** agree with the
primaries, which is what pairing protects.

Proxy-added p99 is the load generator's exact per-request timing, minus the direct-to-source
baseline measured under the same conditions. On the TLS and churn profiles Rust used 14–16% more
cores than Java at saturation, so part of its absolute gain is parallelism; the per-core ratio is
what the gate measures. On the capture profile cores used were equal (0.996), so the gain there is
efficiency.

Gate 4 is measured with Java at its default heap, which is the harder comparison for Rust. A
pinned-heap Java arm (`-Xms536m -Xmx536m`, working set 622 MiB = 93.5% of production's 0.65 GiB)
corroborates the memory result (0.074); its throughput result is inconclusive at 4 valid pairs.

### 5.4 Gate 5 acknowledgement latency — FAIL

The first measurement (`rust-b60dc220e`, 1.47 [1.38–1.55]) was invalid: Rust sampled each record's
latency twice, once on librdkafka's callback thread and again on the publisher actor three thread
hops later, and the generator, being closed-loop, drove the faster Rust proxy's broker 1.48× harder.
`03ddf2ab2` samples once, on the delivery callback, exactly where Java's `KafkaRecordContext` span
ends, and the re-measurement adds pairs rate-matched on both arms.

| 10 pairs each, `rust-03ddf2ab2` | Offered req/s J → R | Ack mean J → R | Ack p99 J → R | Ack p99 ratio [CI95] | Pairs ≤ 1.10 |
|---|---|---|---|---|---|
| Saturating | 18,554 → 27,232 | 1.356 → 2.063 ms (1.50 [1.43–1.58]) | 5.99 → 7.87 ms | **1.36** [1.20–1.53] | 1/10 |
| Rate-matched, 15,000 target | 14,347 → 15,002 | 1.605 → 1.939 ms (1.26 [1.12–1.40]) | 6.74 → 9.16 ms | **1.45** [1.21–1.68] | 3/10 |

Equal load does not remove the gap, so it is not an artefact of Rust offering the broker more work.
It is a real 26–50% increase in send→acknowledgement time at these record rates. Its client-visible
effect is bounded on a healthy broker — proxy-added p99 is still 0.70–0.74× Java's in the same runs —
because only capture-gated mutations wait for an acknowledgement. Under a slow broker it is fully
visible (§5.5).

It is rate-dependent. At the production shape and a fixed 2,848 req/s (5 pairs), Rust's ack p99 is
**0.53×** Java's [0.38–0.69] (20.64 → 9.59 ms) and its ack mean 0.30× (4.35 → 1.23 ms). On plaintext
capture at 11,371 req/s it is 1.52× [1.06–1.97]. The mechanism (§5.5) bites when one broker's
records per round trip exceed what five single-partition requests carry. A healthy multi-broker MSK
cluster at production's ≈1,190 req/s per pod is less exposed; any rise in broker latency brings it
back.

### 5.5 Gate 6, and the two-round-trip acknowledgement defect

Under a 750 ms broker→client delay (toxiproxy), 5 pairs on `rust-b60dc220e`:

| | Java | Rust |
|---|---:|---:|
| Generator errors | 0 | 0 |
| Steady / peak / anon memory | 336 / 347 / 330 MiB | 14.0 / 15.4 / 9.4 MiB |
| After removing the delay | 345.5 MiB | 12.5 MiB |
| Ack mean | 811 ms | **1,509 ms** |
| Client p50 | 760 ms | 1,510 ms |
| Throughput | 95.7 req/s | 54.7 req/s (0.57×) |

Memory is bounded far below the 64 MiB publication budget, delivery is lossless, and memory
recovers, so the gate's clauses hold. But Rust waits about **two** delayed broker round trips per
acknowledgement where Java waits one, which halves throughput whenever acknowledgements are slow.

Diagnosis so far, all on `rust-03ddf2ab2`, single runs (diagnostic, not gate numbers):

| Hypothesis | Test | Result |
|---|---|---|
| Extra records per request | Records per request, both arms | 0.80 on both — **not the cause** |
| Publisher actor holding traffic | Hold happens only when the byte/record budget is exhausted; one record awaiting ack | **not the cause** |
| One ProduceRequest per partition exhausting `max.in.flight=5` on an 8-partition topic | 8 partitions: ack mean 1,535 / 1,464 ms; **4 partitions: 1,507 / 1,514 ms**; Java at 4 partitions: 1,016 ms | **falsified** — partition count does not matter |
| Where in the path | `submitted_at` is taken at librdkafka enqueue and sampled in librdkafka's delivery callback | the extra round trip is **inside librdkafka**, between `produce()` and the delivery report |
| librdkafka in-flight window (`debug=broker,protocol,msg`, 4 partitions) | 31 s steady state, 204 ProduceRequests: **4.95 requests per round trip** against a cap of 5; 168/204 sent with 4 already outstanding; **202/204 sent within 1 ms of a ProduceResponse arriving**; every request carries one partition (request = MessageSet + 89 B); mean 6.7 records per request | **confirmed: window-limited** |

(A first protocol-debug run captured nothing: librdkafka's debug lines reached the proxy's log
facade and were dropped by its default INFO filter. The re-run raised exactly that component with
`MIGRATIONS_COMPONENT_LOG_LEVELS=librdkafka=debug`, PROXY-15. Log:
`results/diag2-rtt-rust-p4-debug/px-diag2-rtt-rust-p4-debug.log`.)

**Mechanism.** librdkafka builds one ProduceRequest **per partition**; kafka-clients drains every
ready partition led by a broker into **one** ProduceRequest. Both clients are held to
`max.in.flight.requests.per.connection=5` per broker connection by idempotence. Closed-loop clients
acknowledged together send their next requests together, so records arrive in waves; at
`linger.ms=0` each partition's first record leaves alone and the rest of its wave need another
request. Once a wave needs more than five requests, the window is full, sending becomes paced by
responses, and every record queues for about one round trip before its own round trip. Java carries
a whole wave in one multi-partition request and stays inside the window (inferred from Java's measured
one-round-trip acknowledgement and kafka-clients' per-broker drain; the Java side was not traced). Partition count barely
matters because two requests per partition per wave already exceed five at four partitions.

**The same window explains Gate 5 on a healthy broker.** One Rust run at Gate 5's saturating load
(24,879 req/s, 20,022 records/s) recorded librdkafka's per-broker statistics every second (log:
`results/diag3-stats-rust-healthy/`). Across 46 busy one-second samples:

- **5 of 5 requests awaiting a response** (median `waitresp_cnt`), with one more request queued
  behind them (`outbuf_cnt` 1).
- **4.96 records per ProduceRequest.**
- A record waits **0.94 ms** on average in librdkafka's partition queue before it is packed into a
  request (`int_latency`; p99 4.9 ms), plus 0.25 ms in the request queue (`outbuf_latency`), against
  a 1.31 ms broker round trip (`rtt`; p99 4.2 ms).
- Those three sum to the run's measured 2.39 ms ack mean, so about **half of every acknowledgement
  is waiting for a window slot**.

This run briefly overlapped a stray diagnostic on the same cores. The window counts are robust to
that; the latencies may be slightly inflated.

**Fix options.** None changes a WIRE or PROTOCOL obligation.

| Option | Effect | Assessment |
|---|---|---|
| A. Several librdkafka producers per proxy with a **fixed partition → producer mapping**, each with its own broker connections and window | Window capacity scales with producers; per-partition order and idempotence are preserved because each partition always uses one producer | **Recommended.** It shards the Kafka client, though the single publisher actor can stay; D4's deferral of publisher sharding covers it in spirit, so it needs your ruling |
| B. A small `linger.ms` to coalesce each wave | One request per partition per wave | Rejected: adds the linger to every gated mutation on a healthy broker (defect #8 measured 5 ms costing 4–9×) |
| C. Disable idempotence to lift the cap of 5 | Larger window | Rejected: weakens per-partition ordering and duplicate suppression that capture depends on |

### 5.6 CPU profiles

**Java** (JFR, Phase 0): 49.9% of Java-thread CPU in telemetry, 23.1% in `ActiveContextTracker`
sorted-map work, publisher plus producer network threads 44.5% on two threads; 45% of allocation is
protobuf `byte[]`.

**Rust** (`perf` 6.6.31, profiling build of `rust-b60dc220e`, plaintext capture, 101,379 samples, 0
lost; attribution only): 60.2% kernel, 21.5% proxy binary, 9.4% libc, 8.8% clock reads (vDSO).

| Hot spot | Share | Note |
|---|---:|---|
| Cross-thread wake-ups (`try_to_wake_up` 13.96%, `__wake_up_sync_key` 6.38%) | ~20% | Per-record hand-offs between tokio workers, the publisher actor, librdkafka's threads and the delivery forwarder — the largest optimisation target |
| Clock reads (vDSO) | 8.8% | `Instant::now()`/`SystemTime::now()` per observation and per record; candidates for coarser or shared clocks |
| `crc32c_sw` | 3.1% | librdkafka uses software CRC32C on arm64; on x86_64 it uses SSE4.2, so this is architecture-specific |
| HTTP parsing | 3.2% | |
| Tokio runtime | 3.1% | |
| Kafka publishing | 2.5% | |
| OpenTelemetry | 1.4% | vs ~50% in Java |
| Protobuf encoding | 0.8% | |
| Docker bridge connection tracking | ~5% | Same cost for Java |

Allocation: Rust steady anonymous memory is 5–10% of Java's on every capture profile (§5.3) with
bounded publication budgets; no allocation profiler was run on Rust, and none is needed to decide
any gate, but the per-record hand-off fix below should be checked for allocation regressions.

### 5.7 Measurement-integrity notes

- 14 harness defects found and fixed, including a run where a dead proxy's 761,216 failed dials was
  recorded `OK`, an interrupted run that exited 0 without a summary, and a script edited while it ran.
  Every reported run passed `validate_run.py`, re-checked per arm by the pair runner.
- Pairing did not collapse per-run variance (it is independent noise, not common-mode drift); the
  design's value is that drift cannot bias the ratio. Resolution came from pair count.
- The first Rust images recorded fewer instruments than Java; results on them are reported only with
  confirmation pairs on instrument-parity images.
- Some of my own e2e tests and builds ran on the host during early Gate 2/4 pairs, and concurrent
  diagnostics ran during the 3-pair confirmations; alternation guards against bias, not against added
  variance.
- Rate-matched Java arms reached 14,347 of the 15,000 req/s target (Rust 15,002); the offered-load
  ratio was 1.04, which slightly favours Java on ack latency and slightly favours Rust on per-core.
- Host speed drifted during the session: the `03ddf2ab2` G2/G4 confirmations ran at about half the
  earlier per-core throughput on **both** arms, with VM CPU unchanged (cause unconfirmed). Ratios
  held. As a result the 15,000 req/s Gate 5 rate was ~81% of Java's saturation in that window rather
  than the intended 75%. Later fixed-rate targets were derived from the immediately preceding Java
  saturating arms (70%), never chosen by hand.
- `validate_run.py`'s per-run ack-mean guard (a broker-health check) was raised from 4 ms to 20 ms
  during the production-shape batch, because healthy production-shape Java arms run at ~4.7 ms. The
  per-pair check (arm more than 2× its batch median) is unchanged and remains the real guard.
- Two diagnostics overlapped measured runs: the librdkafka debug run overlapped g4rate pair 1 (both
  arms; excluding it changes the median from 0.93 to 0.90), and a stray queued diagnostic briefly
  overlapped the statistics run (diagnostic only). No other gate pair overlapped other work.

### 5.8 Fixed-rate CPU efficiency

Every Phase 1 gate ratio is specified **flat out**: each proxy is driven to its own saturation by a
closed-loop generator. Production instead offers a fixed rate (≈1,190 req/s per pod at 3.54 of 5
cores), and F2 divides that rate by the cores it costs. If either proxy's CPU per request changes
with load, the two readings differ. So three profiles were also run open-loop at the same fixed rate
on both arms (every pair held its target to within 0.1% on both arms):

| Profile, `rust-03ddf2ab2` | Flat-out ratio | Fixed rate | Fixed-rate ratio | Cores J → R | Pairs |
|---|---:|---|---:|---|---|
| Production shape (TLS both sides, 23% churn, capture) | 1.85 [1.79–1.92] | 2,848 req/s (70%) | **2.03** [1.73–2.34] | 2.67 → 1.49 | 5/5 ≥ 1.30 |
| Kafka healthy (plaintext, capture) | 1.37 [1.31–1.43] | 15,000 req/s (~81%) | **1.09** [1.02–1.16] | 2.27 → 2.10 | 9/10 ≥ 1.00 |
| Plaintext capture (Gate 4 profile) | 1.36 [1.18–1.54] | 11,371 req/s (70%) | **0.93** [0.80–1.06] | 1.57 → 1.74 | 1/5 ≥ 1.15 |

Each ratio is within-pair, so host-speed drift between batches cancels. The flat-out and fixed-rate
batches ran at different times, so their absolute per-core values are not comparable to each other.

**Production shape: the advantage holds at production utilisation.** TLS on both sides and
connection churn dominate this profile's CPU, and that is where Rust saves most (Gate 3).

**Capture path below saturation: no CPU advantage.** On both plaintext capture profiles Rust's
advantage shrinks from ~1.37× flat out to 0.93–1.09× at 70–81% load. Gate 4's own comparison would
fail at production-like utilisation. Excluding pair 1, which overlapped a low-rate diagnostic, the
11,371 req/s median is 0.90.

The direction matches the profile in §5.6. About 20% of Rust's CPU is cross-thread wake-ups on the
per-record path: connection task → publisher actor → librdkafka → delivery forwarder → actor →
connection. A saturated runtime finds work waiting and amortises each wake across many records; a
partly idle one parks and re-wakes threads for nearly every record. The runtime also starts 10
worker threads on a 5-core cgroup, which multiplies parking. The profile was taken at saturation, so
this attribution at partial load is inferred, not measured.

## 6. Open items and risks

| # | Item | Severity | Next step |
|---|---|---|---|
| R1 | **Producer in-flight window saturation** (§5.5, mechanism confirmed): Gate 5 ack p99 FAIL at 15K–27K req/s; two broker round trips per acknowledgement under a slow broker, halving throughput. At production shape and rate it does not bite on a healthy broker (ack p99 0.53×), but any slow-MSK episode triggers it. | **High** — a failed continuation gate and a production-reachable 2× degradation | Fix option A; needs your D4 ruling |
| R2 | **Capture-path CPU at partial load**: 0.93× [0.80–1.06] and 1.09× [1.02–1.16] per core on the two plaintext capture profiles at 70–81% load (§5.8). Gate 4 passes as specified but would fail at production-like utilisation. The production shape (2.03×) is carried by TLS and connection handling. | **High** — the capture path is the part being rewritten, and its CPU case is unproved below saturation | Profile at a fixed rate; cut per-record hand-offs, wake-ups and clock reads; size the runtime to the cgroup |
| R3 | Owner decisions **D2–D6** open (`OWNER-DECISIONS.md`). D4 now gates R1. D6 (30 s `terminationGracePeriodSeconds` vs the 60/240/270/300 s shutdown sequence) is a production correctness issue for Java and Rust alike. | Medium–High | Owner |
| R4 | MSK IAM from **EKS IRSA** not run; the IMDS path is proved through the same `aws-config` chain | Medium | Phase 3, in an authorised EKS environment |
| R5 | **3-broker leadership change** under load not run (single broker on this host) | Medium | Phase 3 |
| R6 | `crc32c_sw` is 3.1% of CPU on arm64 only; x86_64 uses SSE4.2 | Low | Note for Graviton deployments |
| R7 | Host fidelity: arm64 macOS Docker VM, single broker; absolute numbers do not transfer, ratios do | Accepted | — |
| R8 | Phase 5 deployment work (image wiring, orchestration args, dashboards, Java module deletion) not started, by design | — | Phase 5 |
| R9 | Source comments cite Java file:line heavily; many should become behaviour statements | Low | Phase 2 cleanup |
| R10 | Gate 1 differential last run on `2ff6080df`; `03ddf2ab2` changes are outside the HTTP/1 stream (§3.1) | Low | Re-run on the reworked image |
| R11 | The proxy binds `0.0.0.0` by default, compliant only because the Service is an internal LB by default; `internetFacing=true` without client auth is OD-10's recorded, unactioned remediation | Recorded | OD-10 |

## 7. Recommendation

**REWORK the capture delivery path before Phase 2.** The scope is narrow and in-crate. Do not stop the
rewrite. Do not start Phase 2 on these numbers.

**Why not STOP.** WIRE/PROTOCOL compatibility, the contract that matters most, is proved with zero
differences through the unchanged Java replayer. Every real-Kafka protocol obligation exercised so
far passes, and all four auth modes work against real brokers, including MSK IAM. Memory is 5–10% of
Java's on every capture profile, with bounded, lossless backpressure. At the production shape and a
production-like fixed rate, Rust serves the same traffic on **0.49× the cores**, at **0.43×
proxy-added p99** and **0.53× ack p99**. Eleven defects were found by real-component tests and fixed
without any WIRE or PROTOCOL deviation. Both remaining problems are localised, one with a confirmed
root cause.

**Why not PROCEED.** One continuation gate fails (Gate 5 ack p99). Gate 6 passes while hiding a 2×
slow-broker degradation. And the capture path, the component this rewrite exists to replace, shows no
CPU advantage over Java below saturation (§5.8). Phase 2 builds goldens and replayer checks on top of
that path. Fixing the path first means Phase 2 qualifies the code that will ship. The fix itself
should not touch the record format, so Phase 2's work would not be invalidated either way.

**Rework scope.** No WIRE or PROTOCOL change is expected. If one became necessary, it would come back
to you first.

1. **R1, producer window.** Use several librdkafka producers per proxy with a fixed
   partition → producer mapping (option A, §5.5). Keep the single publisher actor, idempotence,
   `acks=all`, `max.in.flight=5` and `linger.ms=0`. Exit criteria:
   - Gate 5 ack p99 ≤ 1.10 at saturating and rate-matched load.
   - Gate 6 ack mean within 10% of Java's under the 750 ms toxic.
2. **R2, capture-path CPU at partial load.** First profile at a fixed rate to confirm the
   attribution. Then cut per-record cross-thread hand-offs and wake-ups, size the tokio runtime to
   the cgroup's CPU quota, and coarsen per-observation clock reads. Exit criteria:
   - Gate 4's comparison at 70% of Java's saturation ≥ 1.15 per core, CI95 above.
   - No regression on Gates 2–4 flat out, the production shape, or memory.
3. Re-run Gates 4–6, all three fixed-rate profiles and the Gate 1 differential on the reworked image,
   with the same harness and pair counts. Then a second, short Phase 1 review.

**Alternative, if you prefer overlap:** start Phase 2 now with R1 and R2 as blocking items for Phase 2
exit. This is technically sound, because Phase 2 tests record bytes, which the rework does not
change. The cost is that Phase 2 qualifies a delivery path that is about to change.

**Decisions requested from you:**

1. **REWORK first** (recommended), **PROCEED with R1 and R2 blocking**, or STOP.
2. **D4**: may the proxy run several librdkafka producers with a fixed partition mapping (option A)?
   This is the recommended R1 fix, and D4 currently defers publisher sharding.
3. Rulings on **D2, D3, D5, D6**. D6 (30 s `terminationGracePeriodSeconds` vs the 60/240/270/300 s
   shutdown sequence) affects the Java proxy in production today as well.

**Housekeeping:**

- The 22 user containers stopped at the start of the session are listed in
  `/tmp/rust-proxy-qual/stopped-containers.txt` for restart.
- My test containers `capture-kafka-rust` (paused), `wpkafka-wp1` and `wpsrc-wp1` are still present.
- `auth-kafka` and `kreal-kafka` are stopped.
- All AWS resources created for the MSK proof are deleted.

---

## 8. Rework addendum — producer window fixes (after the owner review)

Owner direction: use multiple producers (OD-12). Investigation then found two librdkafka defects in
the idempotent producer, both reported upstream with open, unmerged fixes; the proxy now builds
librdkafka 2.12.1 with those two PRs applied (`third_party/rdkafka-sys/`, commit `428ef064c`).

| Defect | Upstream | Effect before the patch |
|---|---|---|
| Per-partition in-flight cap counted **records**, not requests (`RD_KAFKA_IDEMP_MAX_INFLIGHT - rktp_msgs_inflight`) | issue #5632, PR #5636 | a partition with five records in flight could not start another request; about one request per partition per round trip under load |
| Broker thread armed an immediate wake-up before the cap refused to send | issue #5617, PR #5622 (draft) | busy spin: 2.4 proxy cores at 70 req/s with four producers under a slow broker (`rust-cf1eb57ff`) |

Raising `max.in.flight` above 5 was rejected: librdkafka and kafka-clients refuse it with
idempotence on, and the broker de-duplicates only five batches per producer and partition.

### 8.1 Measurements on `rust-428ef064c` (patched), 4 partitions, both arms

Four partitions so that four producers is exactly one partition per producer; this also matches
production's four partitions per proxy. Ratios are Rust ÷ Java paired medians unless noted.

| Profile (pairs per setting) | Metric | 1 producer | 4 producers |
|---|---|---|---|
| Gate 5 saturating (10) | ack p99 ≤ 1.10 | 1.18 [1.00–1.35] FAIL | **0.74 [0.63–0.84] PASS** |
| | records/core ≥ 0.90 | 1.48 PASS | 1.38 PASS |
| Gate 5 rate-matched 15K (9 / 10) | ack p99 ≤ 1.10 | **0.98 [0.89–1.06] PASS** | **0.54 [0.45–0.63] PASS** |
| | records/core ≥ 0.90 | 1.01 INCONCLUSIVE | 1.04 PASS |
| Gate 6, 750 ms broker delay (5) | client p50 | 1.94 (two round trips) | **0.99 (one)** |
| | throughput | 0.83 | **1.23** |
| | peak memory | 0.045 | 0.049 |
| Gate 4 saturating (5) | req/s per core ≥ 1.15 | 1.56 PASS | 1.43 PASS |
| | anon memory ≤ 0.75 | 0.108 PASS | 0.141 PASS |
| Plaintext capture at 11,371 req/s (5) | req/s per core | 1.18 [1.10–1.25] | 1.03 [0.93–1.12] |
| Production shape at 2,848 req/s (5) | req/s per core | 1.47 [1.38–1.56] | 1.38 [1.32–1.45] |
| | ack p99 | 0.42 | 0.33 |

Gate 1 on the patched image: Java j1 vs Rust r4 (1 producer) and vs r5 (4 producers, 8 partitions):
**0 differences** each, 12/12 per-run assertions.

**Direct 1 vs 4 producers** (10 alternating pairs, Gate 5 saturating, same conditions; 4 ÷ 1):
total req/s **1.22×**, cores 1.23×, req/s per core **0.975** [0.94–1.01], ack p99 **0.63×**
[0.57–0.70], proxy-added p99 0.77×, anon memory 1.48× (47 → 70 MiB, about 8 MiB per extra
producer). The Java-paired batches above suggested a 6–13% per-core cost for four producers; the
direct pairs show about 2.5%. The difference is host drift: the four-producer Java-paired batches
ran in a slower host period that affected Java's acknowledgement latency more than Rust's.

Integrity: one rate-matched Java arm was rejected three times for a host stall (a 45 s run spanning
130–357 s of wall clock with connection errors) and that pair was abandoned. The production-shape
ratio here (1.47) is not comparable with §5.8's 2.03: that batch used 8 partitions, and Java's own CPU
at the same rate differs between the two layouts (2.67 vs 1.42 cores); this is being checked.

### 8.2 Reading

- The record-count cap was the main cause of Gate 5's and Gate 6's latency failures. With it fixed,
  the remaining limit is the five-request window **per producer per broker**, because librdkafka sends
  one partition per request (upstream issue #1700, long open).
- More producers only help when several of a proxy's busy partitions share a leader broker. In the
  production layout (4 partitions per proxy, 12 brokers) that is rare, so one producer should be
  enough; on this single-broker bench, four producers is clearly better.
- Proposed: compute the producer count when partitions are assigned, as
  `min(cap, most assigned partitions led by one broker)`, with a starting cap of 4 to be confirmed by a
  scaling study, and spread each broker's partitions across producers (the current `p mod N` mapping
  collapses onto one producer when N divides the broker count). Awaiting owner approval.
