#!/usr/bin/env python3
"""Aggregate run summaries into per-profile medians and min/max ranges.

   aggregate.py <results-dir> <tag> [--kind java]

Reports the median plus the min and max across reps for every metric, and
flags any profile where the spread of requests_per_s, cores_used,
rps_per_core or steady working set exceeds 10% of the median. Emits JSON on
stdout and a markdown table per profile to <results-dir>/../specs/tables-<tag>.md.
"""
import glob
import json
import os
import statistics
import sys

METRICS = [
    ("requests_per_s", "requests/s", 1),
    ("cores_used", "cores used", 3),
    ("rps_per_core", "requests/s per used core", 1),
    ("p50_ms", "p50 latency (ms)", 3),
    ("p99_ms", "p99 latency (ms)", 3),
    ("probe_p50_ms", "direct probe p50 (ms)", 3),
    ("probe_p99_ms", "direct probe p99 (ms)", 3),
    ("proxy_added_p50_ms", "proxy-added p50 (ms)", 3),
    ("proxy_added_p99_ms", "proxy-added p99 (ms)", 3),
    ("tls_handshakes_per_s", "TLS handshakes/s", 1),
    ("new_conn_fraction", "new-connection fraction", 3),
    ("steady_ws_mib", "steady working set (MiB)", 1),
    ("peak_ws_mib", "peak working set (MiB)", 1),
    ("burst_ws_mib", "burst working set (MiB)", 1),
    ("idle_ws_mib", "idle working set (MiB)", 1),
    ("anon_p50_mib", "anon memory p50 (MiB)", 1),
    ("memory_peak_mib", "cgroup memory.peak (MiB)", 1),
    ("kafka_ack_mean_ms", "Kafka ack latency mean (ms)", 3),
    ("blocked_mean_ms", "capture-blocked mean (ms)", 3),
    ("gathering_request_mean_ms", "HTTP request gather mean (ms)", 3),
    ("kafka_records_per_s", "Kafka records/s", 1),
    ("kafka_mib_per_s", "Kafka MiB/s", 2),
    ("kafka_records_per_s_per_core", "Kafka records/s per core", 1),
    ("kafka_mib_per_s_per_core", "Kafka MiB/s per core", 3),
    ("kafka_records_per_request", "Kafka records per request", 3),
    ("kafka_bytes_per_record", "Kafka bytes per record", 0),
    ("capture_publish_mib_per_s_per_core", "capture-publish MiB/s per core", 3),
    ("client_mib_s", "client->proxy MiB/s", 2),
    ("vm_busy_cores", "VM-wide busy cores (all workloads)", 2),
    ("vm_softirq_cores", "VM softirq cores (network stack)", 2),
    ("proxy_share_of_vm_cpu", "proxy cgroup share of VM CPU", 3),
    ("errors_total", "generator errors", 0),
    ("nr_throttled_delta", "cgroup CPU throttle events", 0),
]

VARIANCE_KEYS = ["requests_per_s", "cores_used", "rps_per_core", "steady_ws_mib"]


def fmt(v, nd):
    if v is None:
        return "—"
    if nd == 0:
        return str(int(round(v)))
    return f"{v:.{nd}f}"


def main():
    results = sys.argv[1]
    tag = sys.argv[2]
    kind = "java"
    if "--kind" in sys.argv:
        kind = sys.argv[sys.argv.index("--kind") + 1]

    runs = []
    rejected = []
    for p in sorted(glob.glob(os.path.join(results, f"{tag}-{kind}-*", "summary.json"))):
        # A run that validate_run.py rejected leaves a REJECTED marker. Such a
        # run is never aggregated: averaging in a run whose proxy died or whose
        # time base was corrupted is exactly the failure this harness exists to
        # prevent.
        if os.path.exists(os.path.join(os.path.dirname(p), "REJECTED")):
            rejected.append(os.path.basename(os.path.dirname(p)))
            continue
        try:
            runs.append(json.load(open(p)))
        except Exception as e:
            print(f"WARNING: could not read {p}: {e}", file=sys.stderr)
    if rejected:
        print(f"skipping {len(rejected)} rejected run(s): {', '.join(rejected)}", file=sys.stderr)

    by_profile = {}
    for r in runs:
        by_profile.setdefault(r["profile"], []).append(r)

    # Profile-RELATIVE Kafka-health outlier exclusion.
    #
    # This proxy blocks 82% of requests on the Kafka acknowledgement, so its
    # throughput is governed by ack latency. A run whose broker was starved is
    # a valid measurement of a degraded system but not of the profile it claims
    # to be. An absolute ack threshold cannot be used because each profile has
    # its own healthy baseline (plaintext-capture 0.7 ms, churn23-tls 2.0 ms:
    # more connections means more TrafficStreams and more records per request).
    # So the test is relative to the profile's own median, which needs no
    # hand-tuned constant per profile.
    #
    # This is a test on an INPUT (broker health), never on the output
    # (throughput), so it cannot be used to cherry-pick fast runs.
    excluded = {}
    for prof, rs in list(by_profile.items()):
        acks = [r.get("kafka_ack_mean_ms") for r in rs]
        acks = [a for a in acks if isinstance(a, (int, float)) and a > 0]
        if len(acks) < 3:
            continue
        med_ack = statistics.median(acks)
        keep, drop = [], []
        for r in rs:
            a = r.get("kafka_ack_mean_ms")
            if isinstance(a, (int, float)) and a > 2.0 * med_ack:
                drop.append((r["run_id"], a, med_ack))
            else:
                keep.append(r)
        if drop and len(keep) >= 3:
            by_profile[prof] = keep
            excluded[prof] = drop
            for rid, a, m in drop:
                print(f"excluding {rid}: kafka ack mean {a:.3f} ms > 2x profile "
                      f"median {m:.3f} ms", file=sys.stderr)

    agg = {}
    md = [f"# Java capture-proxy baseline — tag `{tag}`, proxy `{kind}`", ""]
    for prof in sorted(by_profile):
        rs = by_profile[prof]
        m0 = rs[0]
        agg[prof] = {
            "reps": len(rs),
            "run_ids": [r["run_id"] for r in rs],
            "config": {
                k: m0.get(k)
                for k in ("front_tls", "back_tls", "capture", "churn", "frag_prob",
                          "conns", "rate", "kafka_path", "kafka_delay_ms",
                          "proxy_cores", "proxy_mem_limit")
            },
            "metrics": {},
            "high_variance": [],
            "excluded_for_kafka_health": [
                {"run_id": rid, "ack_mean_ms": a, "profile_median_ack_ms": m}
                for rid, a, m in excluded.get(prof, [])
            ],
        }
        md.append(f"## Profile `{prof}`  ({len(rs)} reps)")
        md.append("")
        cfg = agg[prof]["config"]
        md.append(
            "Config: front_tls=%s back_tls=%s capture=%s churn=%s frag=%s conns=%s "
            "rate=%s kafka=%s delay=%sms cpuset=%s mem=%s"
            % (cfg["front_tls"], cfg["back_tls"], cfg["capture"], cfg["churn"],
               cfg["frag_prob"], cfg["conns"], cfg["rate"], cfg["kafka_path"],
               cfg["kafka_delay_ms"], cfg["proxy_cores"], cfg["proxy_mem_limit"])
        )
        md.append("")
        md.append("| Metric | Median | Min | Max | Spread % of median |")
        md.append("|---|---|---|---|---|")
        for key, label, nd in METRICS:
            vals = [r.get(key) for r in rs]
            vals = [v for v in vals if isinstance(v, (int, float))]
            if not vals:
                agg[prof]["metrics"][key] = None
                md.append(f"| {label} | NOT MEASURED | — | — | — |")
                continue
            med = statistics.median(vals)
            lo, hi = min(vals), max(vals)
            spread = (100.0 * (hi - lo) / med) if med else 0.0
            agg[prof]["metrics"][key] = {
                "median": med, "min": lo, "max": hi, "n": len(vals),
                "spread_pct_of_median": spread,
            }
            flag = ""
            if key in VARIANCE_KEYS and spread > 10.0:
                flag = " **>10%**"
                agg[prof]["high_variance"].append(key)
            md.append(
                f"| {label} | {fmt(med, nd)} | {fmt(lo, nd)} | {fmt(hi, nd)} | {spread:.1f}%{flag} |"
            )
        md.append("")
        bad = [r["run_id"] for r in rs if r.get("accounting_consistent") is False]
        if bad:
            md.append(f"**Request accounting inconsistent in: {', '.join(bad)}**")
            md.append("")
        notes = sorted({r[k] for r in rs for k in r if k.startswith("note_")})
        for n in notes:
            md.append(f"- note: {n}")
        if notes:
            md.append("")

    outdir = os.path.join(os.path.dirname(results.rstrip("/")), "specs")
    os.makedirs(outdir, exist_ok=True)
    mdpath = os.path.join(outdir, f"tables-{tag}-{kind}.md")
    with open(mdpath, "w") as f:
        f.write("\n".join(md) + "\n")
    print(json.dumps(agg, indent=2, default=str))
    print(f"\n# markdown tables -> {mdpath}", file=sys.stderr)


if __name__ == "__main__":
    main()
