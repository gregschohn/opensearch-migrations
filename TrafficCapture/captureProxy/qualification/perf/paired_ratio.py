#!/usr/bin/env python3
"""Paired-ratio analysis for interleaved A/B runs.

   paired_ratio.py <manifest> [--gate METRIC:OP:THRESHOLD ...]

   e.g. --gate rps_per_core:ge:1.20   (B/A must be at least 1.20)
        --gate proxy_added_p99_ms:le:1.05

For each metric this reports:

  * the DISTRIBUTION of per-pair ratios B/A: median, min, max, and the fraction
    of pairs clearing each requested gate threshold;
  * alongside it, the ratio you would have got from comparing the two arms'
    independent medians, and the spread of each arm measured on its own.

The point of printing both is that the second is the method that the host's
drift makes meaningless, and the first is the method that survives it. The
"variance reduction" column is the ratio of the two coefficients of variation:
how much narrower the paired-ratio distribution is than the per-arm spread it
came from. A value well above 1 means pairing bought real resolution.

A gate is reported as PASS only when the median paired ratio clears it AND the
fraction of pairs clearing it is at least --min-fraction (default 0.80). A
median that clears while half the pairs do not is a coin flip, not a result.
"""
import json
import os
import statistics
import sys

METRICS = [
    # (key, "higher is better"?)
    ("rps_per_core", True),
    ("cores_used", False),
    ("kafka_records_per_s_per_core", True),
    ("kafka_mib_per_s_per_core", True),
    ("requests_per_s", True),
    ("proxy_added_p50_ms", False),
    ("proxy_added_p99_ms", False),
    ("p99_ms", False),
    ("steady_ws_mib", False),
    ("anon_p50_mib", False),
    ("kafka_ack_mean_ms", False),
    ("kafka_ack_p99_ms", False),
    ("blocked_mean_ms", False),
]

RESULTS = "/tmp/rust-proxy-qual/results"


def load(rid):
    p = os.path.join(RESULTS, rid, "summary.json")
    try:
        return json.load(open(p))
    except Exception:
        return None


def cv(xs):
    """Coefficient of variation, as a percentage."""
    xs = [x for x in xs if isinstance(x, (int, float))]
    if len(xs) < 2:
        return None
    m = statistics.mean(xs)
    if m == 0:
        return None
    return 100.0 * statistics.stdev(xs) / abs(m)


def main():
    manifest = sys.argv[1]
    gates = []
    minfrac = 0.80
    args = sys.argv[2:]
    i = 0
    while i < len(args):
        if args[i] == "--gate":
            k, op, thr = args[i + 1].split(":")
            gates.append((k, op, float(thr)))
            i += 2
        elif args[i] == "--min-fraction":
            minfrac = float(args[i + 1])
            i += 2
        else:
            i += 1

    pairs = []
    for line in open(manifest):
        parts = line.rstrip("\n").split("\t")
        if len(parts) != 4:
            continue
        n, order, a_rid, b_rid = parts
        a, b = load(a_rid), load(b_rid)
        if a and b:
            pairs.append({"n": int(n), "order": order, "a_rid": a_rid,
                          "b_rid": b_rid, "a": a, "b": b})

    if not pairs:
        print("no complete pairs found", file=sys.stderr)
        return 1

    # Pair-level health exclusion -- a test on INPUTS, never on the outcome,
    # applied to the whole pair so the design stays balanced:
    #  * an arm whose windowed Kafka ack mean exceeds 2x that arm's median
    #    across the pairs: the broker was starved and the run does not measure
    #    the profile it claims to (Rust exports no ack histogram, so only the
    #    Java arm can trip this; it still excludes the whole pair);
    #  * either arm with negative proxy-added p99: the direct-to-source probe's
    #    tail exceeded the proxied tail, i.e. the source or host stalled during
    #    the window, which invalidates the subtraction for that run.
    excluded = []
    for arm in ("a", "b"):
        acks = [p[arm].get("kafka_ack_mean_ms") for p in pairs]
        acks = [x for x in acks if isinstance(x, (int, float)) and x > 0]
        med = statistics.median(acks) if len(acks) >= 3 else None
        for p in pairs:
            x = p[arm].get("kafka_ack_mean_ms")
            if med and isinstance(x, (int, float)) and x > 2.0 * med and not p.get("_ex"):
                p["_ex"] = f"arm {arm.upper()} kafka ack mean {x:.3f} ms > 2x arm median {med:.3f} ms"
    for p in pairs:
        for arm in ("a", "b"):
            v = p[arm].get("proxy_added_p99_ms")
            if isinstance(v, (int, float)) and v < 0 and not p.get("_ex"):
                p["_ex"] = f"arm {arm.upper()} proxy-added p99 {v:.3f} ms < 0 (source/host stall)"
    if "--no-exclude" not in sys.argv:
        excluded = [(p["n"], p["_ex"]) for p in pairs if p.get("_ex")]
        pairs = [p for p in pairs if not p.get("_ex")]
    for n, why in excluded:
        print(f"# EXCLUDED pair {n}: {why}")

    out = {"manifest": manifest, "pairs": len(pairs), "excluded_pairs": excluded,
           "orders": {}, "metrics": {}, "gates": []}
    for p in pairs:
        out["orders"][p["order"]] = out["orders"].get(p["order"], 0) + 1

    print(f"# paired-ratio analysis: {len(pairs)} complete pairs")
    print(f"# order balance: {out['orders']}")
    print(f"# profile: {pairs[0]['a'].get('profile')}   "
          f"A={pairs[0]['a_rid'].rsplit('-',2)[-2]}  B={pairs[0]['b_rid'].rsplit('-',2)[-2]}")
    print()
    hdr = (f"{'metric':<32} {'pairedB/A med':>13} {'min':>8} {'max':>8} "
           f"{'pairCV%':>8} | {'medA':>11} {'medB':>11} {'indepB/A':>9} "
           f"{'cvA%':>6} {'cvB%':>6} {'varRed':>7}")
    print(hdr)
    print("-" * len(hdr))

    for key, higher_better in METRICS:
        av = [p["a"].get(key) for p in pairs]
        bv = [p["b"].get(key) for p in pairs]
        both = [(x, y) for x, y in zip(av, bv)
                if isinstance(x, (int, float)) and isinstance(y, (int, float)) and x]
        if len(both) < 2:
            continue
        ratios = [y / x for x, y in both]
        med_r = statistics.median(ratios)
        cva, cvb, cvr = cv([x for x, _ in both]), cv([y for _, y in both]), cv(ratios)
        med_a = statistics.median([x for x, _ in both])
        med_b = statistics.median([y for _, y in both])
        indep = med_b / med_a if med_a else None
        # Variance reduction: how much narrower the paired ratio is than the
        # looser of the two per-arm spreads it was built from.
        varred = (max(cva, cvb) / cvr) if (cva and cvb and cvr) else None

        # Resolution of the paired estimate. The dominant uncertainty in a
        # gate verdict is the standard error of the ratio estimate, which falls
        # as 1/sqrt(n_pairs) -- so the actionable output is not just "is the
        # variance smaller" but "how many pairs does THIS gate margin need".
        n = len(ratios)
        sem_pct = (cvr / (n ** 0.5)) if cvr else None          # % of the ratio
        ci95_pct = (1.96 * sem_pct) if sem_pct else None
        ci_lo = med_r * (1 - ci95_pct / 100.0) if ci95_pct else None
        ci_hi = med_r * (1 + ci95_pct / 100.0) if ci95_pct else None
        # Pairs needed for the 95% CI half-width to fit inside a given margin.
        def pairs_for(margin_pct):
            if not cvr or margin_pct <= 0:
                return None
            return int(((1.96 * cvr) / margin_pct) ** 2 + 0.999)

        out["metrics"][key] = {
            "n": n,
            "paired_cv_pct": cvr,
            "sem_pct_of_ratio": sem_pct,
            "ci95_half_width_pct": ci95_pct,
            "ci95_lo": ci_lo, "ci95_hi": ci_hi,
            "pairs_needed_for_margin": {
                "20pct": pairs_for(20.0), "15pct": pairs_for(15.0),
                "10pct": pairs_for(10.0), "5pct": pairs_for(5.0),
            },
            "higher_is_better": higher_better,
            "n_pairs": len(both),
            "paired_ratio_median": med_r,
            "paired_ratio_min": min(ratios),
            "paired_ratio_max": max(ratios),
            "paired_ratio_cv_pct": cvr,
            "arm_a_median": med_a, "arm_b_median": med_b,
            "independent_median_ratio": indep,
            "arm_a_cv_pct": cva, "arm_b_cv_pct": cvb,
            "variance_reduction_factor": varred,
            "per_pair_ratios": ratios,
        }
        print(f"{key:<32} {med_r:>13.4f} {min(ratios):>8.4f} {max(ratios):>8.4f} "
              f"{(cvr if cvr is not None else float('nan')):>8.2f} | "
              f"{med_a:>11.3f} {med_b:>11.3f} "
              f"{(indep if indep else float('nan')):>9.4f} "
              f"{(cva if cva else float('nan')):>6.2f} {(cvb if cvb else float('nan')):>6.2f} "
              f"{(varred if varred else float('nan')):>7.2f}")

    print()
    print("# resolution: 95% CI half-width on the paired ratio at this n, and the number")
    print("# of pairs each gate margin would need (n scales as 1/margin^2)")
    rh = f"{'metric':<32} {'n':>3} {'pairCV%':>8} {'CI95+-%':>8} {'20%':>5} {'15%':>5} {'10%':>5} {'5%':>5}"
    print(rh); print("-" * len(rh))
    for key, _ in METRICS:
        m = out["metrics"].get(key)
        if not m or not m.get("ci95_half_width_pct"):
            continue
        pn = m["pairs_needed_for_margin"]
        print(f"{key:<32} {m['n']:>3} {m['paired_cv_pct']:>8.2f} "
              f"{m['ci95_half_width_pct']:>8.2f} {str(pn['20pct']):>5} {str(pn['15pct']):>5} "
              f"{str(pn['10pct']):>5} {str(pn['5pct']):>5}")

    if gates:
        print()
        print("# gate evaluation on the paired-ratio distribution")
        print(f"# a gate PASSES only if the median clears it AND >= {minfrac:.0%} of pairs clear it")
        for key, op, thr in gates:
            m = out["metrics"].get(key)
            if not m:
                print(f"  {key} {op} {thr}: NOT MEASURED")
                out["gates"].append({"metric": key, "op": op, "threshold": thr,
                                     "verdict": "NOT MEASURED"})
                continue
            rs = m["per_pair_ratios"]
            clears = [r >= thr for r in rs] if op == "ge" else [r <= thr for r in rs]
            frac = sum(clears) / len(clears)
            med_ok = (m["paired_ratio_median"] >= thr) if op == "ge" \
                else (m["paired_ratio_median"] <= thr)
            # The CI must also sit entirely on the passing side, otherwise the
            # verdict is within measurement resolution and is not a result.
            lo, hi = m.get("ci95_lo"), m.get("ci95_hi")
            ci_ok = None
            if lo is not None and hi is not None:
                ci_ok = (lo >= thr) if op == "ge" else (hi <= thr)
            if med_ok and frac >= minfrac and ci_ok:
                verdict = "PASS"
            elif (not med_ok) and frac <= (1 - minfrac) and ci_ok is False and (
                    (hi < thr) if op == "ge" else (lo > thr)):
                verdict = "FAIL"
            elif med_ok and frac >= minfrac and ci_ok is False:
                verdict = "INCONCLUSIVE (median clears but 95% CI straddles the threshold)"
            elif med_ok and frac < minfrac:
                verdict = "INCONCLUSIVE (median clears but too few pairs do)"
            else:
                verdict = "FAIL"
            ci_s = f"CI95=[{lo:.4f}..{hi:.4f}]" if lo is not None else "CI95=n/a"
            print(f"  {key} {op} {thr}: median={m['paired_ratio_median']:.4f} {ci_s} "
                  f"pairs_clearing={sum(clears)}/{len(clears)} ({frac:.0%}) -> {verdict}")
            out["gates"].append({"metric": key, "op": op, "threshold": thr,
                                 "median_ratio": m["paired_ratio_median"],
                                 "ci95_lo": lo, "ci95_hi": hi,
                                 "pairs_clearing": sum(clears), "pairs": len(clears),
                                 "fraction": frac, "verdict": verdict})

    jpath = manifest.replace(".manifest", ".paired.json")
    json.dump(out, open(jpath, "w"), indent=2, default=str)
    print(f"\n# json -> {jpath}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
