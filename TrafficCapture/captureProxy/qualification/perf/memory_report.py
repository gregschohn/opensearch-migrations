#!/usr/bin/env python3
"""Java memory behaviour across idle / steady / burst / degraded-Kafka.

   memory_report.py <results-dir> <tag> [kind]

Reads every run's summary.json plus its raw cgroup.csv so that the burst and
recovery phases can be read off the time series rather than inferred from
end-of-run aggregates.

Phases:
  idle      - memory.current with the proxy listening but before any load
              (recorded by run-profile.sh immediately after the sampler starts)
  steady    - median memory.current inside the measured window
  burst     - maximum memory.current anywhere in the run, which for this
              workload is connection establishment during warm-up
  degraded  - steady and peak for the kafka-delayed profile
  recovered - median memory.current over the post-toxic recovery window of the
              kafka-delayed runs, which is the "returns near baseline" half of
              gate 6
"""
import csv
import glob
import json
import os
import statistics
import sys

MIB = 1024.0 * 1024.0


def med(xs):
    xs = [x for x in xs if isinstance(x, (int, float))]
    return statistics.median(xs) if xs else None


def rng(xs):
    xs = [x for x in xs if isinstance(x, (int, float))]
    return (min(xs), max(xs)) if xs else (None, None)


def load_csv(p):
    if not os.path.exists(p):
        return []
    rows = []
    with open(p, newline="") as f:
        for r in csv.DictReader(f):
            try:
                r["epoch_ms"] = int(r["epoch_ms"])
                r["memory_current"] = int(r["memory_current"])
                r["anon"] = int(r["anon"])
            except (ValueError, TypeError, KeyError):
                continue
            rows.append(r)
    return rows


def main():
    results, tag = sys.argv[1], sys.argv[2]
    kind = sys.argv[3] if len(sys.argv) > 3 else "java"

    by_profile = {}
    for p in sorted(glob.glob(os.path.join(results, f"{tag}-{kind}-*", "summary.json"))):
        if os.path.exists(os.path.join(os.path.dirname(p), "REJECTED")):
            continue
        d = json.load(open(p))
        d["_dir"] = os.path.dirname(p)
        by_profile.setdefault(d["profile"], []).append(d)

    out = {}
    for prof, runs in sorted(by_profile.items()):
        rec = {"reps": len(runs)}
        for key, field in (("idle_mib", "idle_ws_mib"), ("steady_mib", "steady_ws_mib"),
                           ("burst_mib", "burst_ws_mib"), ("peak_mib", "peak_ws_mib"),
                           ("cgroup_peak_mib", "memory_peak_mib"),
                           ("anon_p50_mib", "anon_p50_mib"), ("anon_max_mib", "anon_max_mib")):
            vals = [r.get(field) for r in runs]
            lo, hi = rng(vals)
            rec[key] = {"median": med(vals), "min": lo, "max": hi}

        # Recovery window, kafka-delayed only.
        recs = []
        for r in runs:
            recov = r.get("recovery")
            if not isinstance(recov, dict):
                continue
            rows = load_csv(os.path.join(r["_dir"], "cgroup.csv"))
            a, b = recov.get("rec_start_ms"), recov.get("rec_end_ms")
            if not (a and b and rows):
                continue
            win = [x["memory_current"] for x in rows if a <= x["epoch_ms"] <= b]
            # Last third of the recovery window: after the backlog has drained.
            tail = win[len(win) * 2 // 3:] if win else []
            if win:
                recs.append({
                    "run_id": r["run_id"],
                    "recovery_samples": len(win),
                    "recovery_median_mib": statistics.median(win) / MIB,
                    "recovery_min_mib": min(win) / MIB,
                    "recovery_tail_median_mib": (statistics.median(tail) / MIB) if tail else None,
                    "steady_mib": r.get("steady_ws_mib"),
                    "records_after_recovery": recov.get("records_after_recovery"),
                })
        if recs:
            rec["recovery"] = recs
            rec["recovery_tail_median_mib_median"] = med(
                [x["recovery_tail_median_mib"] for x in recs])

        # Memory limit headroom.
        lim = runs[0].get("proxy_mem_limit")
        rec["mem_limit"] = lim
        if rec["peak_mib"]["max"] and lim and lim.endswith("m"):
            limit_mib = float(lim[:-1])
            rec["peak_pct_of_limit"] = 100.0 * rec["peak_mib"]["max"] / limit_mib
        # Backpressure evidence.
        ow = [r.get("otel_window") or {} for r in runs]
        rec["blocking_fraction"] = med([o.get("blocking_fraction_of_requests") for o in ow])
        rec["kafka_records_per_s"] = med([r.get("kafka_records_per_s") for r in runs])
        rec["requests_per_s"] = med([r.get("requests_per_s") for r in runs])
        rec["p99_ms"] = med([r.get("p99_ms") for r in runs])
        rec["errors_total"] = med([r.get("errors_total") for r in runs])
        ack = [((r.get("otel") or {}).get("kafkaCommitDuration") or {}).get("mean") for r in runs]
        rec["kafka_ack_mean_ms"] = med(ack)
        out[prof] = rec

    json.dump(out, sys.stdout, indent=2, default=str)
    print()


if __name__ == "__main__":
    main()
