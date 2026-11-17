# Java/Rust capture-proxy performance qualification harness

The harness behind `../reports/phase1-owner-review.md` §5. It is committed so that every number in the
report can be reproduced. It is **qualification tooling, not product code**.

## Layout

| Path | What it is |
|---|---|
| `run-profile.sh` | One measured run of one proxy on one profile: private Docker network, source, Kafka, optional toxiproxy, OTel collector, proxy pinned to 5 cores / 5,000 MiB, cgroup v2 sampler, generator, summary. |
| `run-paired.sh` | Interleaved Java/Rust pairs with order alternated; every run must pass `validate_run.py` or the pair is retried/abandoned. |
| `paired_ratio.py` | Per-pair Rust ÷ Java ratios, medians, CI95, pairs-clearing, and the PASS/FAIL/INCONCLUSIVE rule. |
| `profiles.sh` | The profile definitions (corpus mix, connections, churn, TLS, capture, Kafka delay). |
| `gen/` | The Go load generator (`qualgen`), closed-loop by default, open-loop with `-rate`. Build with `Dockerfile.qualgen`. |
| `measurement-patch/` | **Measurement-only** Java overlay that changes only the ack-latency histogram bucket boundaries in `RootOtelContext`, so Java and Rust histograms share `GATE_LATENCY_BUCKETS_MS`. Never valid for a correctness claim. |
| `run-g5v4.sh`, `run-prodshape*.sh`, `run-gates-*.sh`, `run-chain-*.sh`, `run-matrix.sh` | The exact drivers used for each batch in the report. |
| `run-diag-*.sh`, `run-profile-diag.sh` | Diagnostics (not gate numbers): librdkafka protocol debug and statistics. |
| `specs/perf-baseline-spec.md` | Full method: topology, core budget, latency definition, working-set definitions, the Java Phase 0 scorecard, and the production baseline it is checked against. |
| `specs/wire-proof.md` | Gate 1 method: corpus, Java calibration, negative control, replayer differential. |

## Assumptions

- Paths are rooted at `/tmp/rust-proxy-qual` (`ROOT` in `lib.sh`; `run-paired.sh` hard-codes it). Copy
  this directory to `/tmp/rust-proxy-qual/harness` before running.
- Images: `JAVA_PROXY_IMAGE` and `RUST_PROXY_IMAGE` (built from `captureProxy/Dockerfile`, context
  `TrafficCapture/`), in a local registry at `localhost:5001`.
- **The Java comparison image is pinned by digest, not rebuilt.** The Java proxy and its Jib task
  `:TrafficCapture:trafficCaptureProxyServer:jib_arm64` have been deleted from the repository, so
  `JAVA_PROXY_IMAGE` defaults (in `lib.sh` and `measurement-patch/build.sh`) to the image every
  Phase 1 comparison used:
  `localhost:5001/migrations/capture_proxy@sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357`
  (linux/arm64, built at the baseline `059d7b6483f2`; provenance in
  `specs/perf-baseline-spec.md` §3 and `../reports/phase1-owner-review.md`). Phase 6 comparisons must
  use the same digest. Do not resolve it through a tag: `capture_proxy:latest` and `latest_arm64` are
  now Rust builds or may become them. If the digest is missing from a registry, rebuild it from the
  baseline commit, where the Jib task still exists, and check that the digest matches before using it.
- All containers run on per-run private Docker networks with unique names derived from the run id;
  nothing publishes a host port.
- Host: an 18-core arm64 machine; core sets are assigned in `lib.sh`. Absolute numbers do not transfer
  to other hosts. Paired ratios are what the gates use.

## Example

```bash
./run-paired.sh g4 plaintext-capture 10 java rust 30 15
python3 -I paired_ratio.py /tmp/rust-proxy-qual/results/paired-g4-plaintext-capture.manifest \
  --gate rps_per_core:ge:1.15 --gate anon_p50_mib:le:0.75
```
