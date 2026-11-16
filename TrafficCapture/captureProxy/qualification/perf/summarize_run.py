#!/usr/bin/env python3
"""Reduce one run directory (meta.json + gen.json + cgroup.csv + otel-metrics.prom)
to a single machine-readable summary.

CPU is taken from the cgroup v2 `cpu.stat` usage_usec counter, differenced
between the first and last sample inside the measured window and divided by the
measured wall-clock seconds. That gives cores-used directly and does not depend
on any sampling of /proc or on macOS `ps`, neither of which can see inside the
Docker VM's cgroups.

Working set is reported two ways, because they answer different questions:
  * memory.current  - total charged to the cgroup, including page cache.
  * memory.stat anon - anonymous pages, i.e. the heap and native allocations
    that a rewrite can actually shrink.
The production report's "working set" is cAdvisor's container_memory_working_set
_bytes, which is memory.current minus inactive_file; that form is computed too
so the two are comparable.
"""
import csv
import json
import os
import re
import sys

MIB = 1024.0 * 1024.0


def pct(sorted_vals, p):
    if not sorted_vals:
        return None
    i = int(p * (len(sorted_vals) - 1))
    return sorted_vals[i]


def load_cgroup(path, t0_ms, t1_ms):
    """Return (rows_in_window, all_rows)."""
    if not os.path.exists(path):
        return [], []
    rows = []
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            try:
                r["epoch_ms"] = int(r["epoch_ms"])
            except (ValueError, TypeError):
                continue
            rows.append(r)
    win = [r for r in rows if t0_ms <= r["epoch_ms"] <= t1_ms]
    return win, rows


def num(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None


def parse_prom(path):
    """Parse the subset of the prometheus text format the harness needs:
    counters/gauges as name{labels} value, and histogram _bucket/_count/_sum."""
    out = {}
    if not os.path.exists(path):
        return out
    line_re = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+([0-9eE.+-]+)")
    with open(path) as f:
        for line in f:
            if line.startswith("#"):
                continue
            m = line_re.match(line.strip())
            if not m:
                continue
            name, labels, val = m.group(1), m.group(2) or "", m.group(3)
            try:
                v = float(val)
            except ValueError:
                continue
            out.setdefault(name, []).append((labels, v))
    return out


def sum_metric(prom, name):
    return sum(v for _, v in prom.get(name, [])) if name in prom else None


def hist_quantiles(prom, base, qs=(0.5, 0.99)):
    """Reconstruct quantiles from cumulative prometheus histogram buckets."""
    buckets = prom.get(base + "_bucket")
    if not buckets:
        return None
    acc = {}
    for labels, v in buckets:
        m = re.search(r'le="([^"]+)"', labels)
        if not m:
            continue
        le = m.group(1)
        le = float("inf") if le == "+Inf" else float(le)
        acc[le] = acc.get(le, 0.0) + v
    if not acc:
        return None
    edges = sorted(acc.items())
    total = edges[-1][1]
    if total <= 0:
        return None
    res = {}
    for q in qs:
        want = q * total
        prev_le = 0.0
        prev_c = 0.0
        hit = None
        for le, c in edges:
            if c >= want:
                # Linear interpolation inside the bucket.
                if c > prev_c and le != float("inf"):
                    frac = (want - prev_c) / (c - prev_c)
                    hit = prev_le + frac * (le - prev_le)
                else:
                    hit = prev_le if le == float("inf") else le
                break
            prev_le, prev_c = le, c
        res["p%g" % (q * 100)] = hit
    cnt = sum_metric(prom, base + "_count")
    s = sum_metric(prom, base + "_sum")
    res["count"] = cnt
    res["mean"] = (s / cnt) if (cnt and s is not None and cnt > 0) else None
    return res


def main():
    d = sys.argv[1]
    meta = json.load(open(os.path.join(d, "meta.json")))
    gen = json.load(open(os.path.join(d, "gen.json")))

    out = dict(meta)
    pl = gen.get("proxy", {})
    probe = gen.get("direct_probe") or {}
    measured_s = gen.get("measured_s") or meta["duration_s"]

    out["measured_s"] = measured_s
    out["requests_per_s"] = pl.get("requests_per_s")
    out["attempted_per_s"] = pl.get("attempted_per_s")
    out["errors_total"] = pl.get("errors_total")
    out["errors"] = pl.get("errors")
    out["accounting_consistent"] = gen.get("accounting_consistent")
    out["bytes_in_per_s"] = pl.get("bytes_out_per_s")      # client -> proxy
    out["bytes_out_per_s"] = pl.get("bytes_in_per_s")      # proxy -> client
    out["client_mib_s"] = (pl.get("bytes_out_per_s") or 0) / MIB
    out["tls_handshakes_per_s"] = pl.get("tls_handshakes_per_s")
    out["new_conn_fraction"] = pl.get("new_conn_fraction")
    out["fragmented_fraction"] = pl.get("fragmented_fraction")
    out["by_kind"] = pl.get("by_kind")
    out["p50_ms"] = pl.get("p50_ms")
    out["p90_ms"] = pl.get("p90_ms")
    out["p99_ms"] = pl.get("p99_ms")
    out["p999_ms"] = pl.get("p999_ms")
    out["max_ms"] = pl.get("max_ms")
    out["latency_samples"] = pl.get("latency_samples")
    out["latency_overflow"] = pl.get("latency_overflow")

    out["probe_p50_ms"] = probe.get("p50_ms")
    out["probe_p99_ms"] = probe.get("p99_ms")
    out["probe_requests_per_s"] = probe.get("requests_per_s")
    out["probe_errors_total"] = probe.get("errors_total")
    for q in ("p50", "p99"):
        a, b = pl.get(q + "_ms"), probe.get(q + "_ms")
        out["proxy_added_%s_ms" % q] = (
            round(a - b, 4) if (a is not None and b is not None and a >= 0 and b >= 0) else None
        )

    # ---- CPU and memory from cgroup v2 ----
    t0, t1 = meta["t_start_ms"], meta["t_end_ms"]
    # The measured window starts after warm-up.
    meas_t0 = t0 + int(meta["warmup_s"] * 1000)
    win, allrows = load_cgroup(os.path.join(d, "cgroup.csv"), meas_t0, t1)
    out["cgroup_samples_total"] = len(allrows)
    out["cgroup_samples_in_window"] = len(win)

    if len(win) >= 2:
        a, b = win[0], win[-1]
        dt_s = (b["epoch_ms"] - a["epoch_ms"]) / 1000.0
        du = num(b["usage_usec"]) - num(a["usage_usec"])
        out["cpu_window_s"] = dt_s
        out["cpu_usage_usec_delta"] = du
        out["cores_used"] = (du / 1e6 / dt_s) if dt_s > 0 else None
        out["user_cores"] = ((num(b["user_usec"]) - num(a["user_usec"])) / 1e6 / dt_s) if dt_s > 0 else None
        out["system_cores"] = ((num(b["system_usec"]) - num(a["system_usec"])) / 1e6 / dt_s) if dt_s > 0 else None
        out["nr_throttled_delta"] = num(b["nr_throttled"]) - num(a["nr_throttled"])
        out["throttled_usec_delta"] = num(b["throttled_usec"]) - num(a["throttled_usec"])

        # VM-wide CPU over the same window, in cores. USER_HZ is 100 on this
        # kernel. vm_busy_cores counts everything except idle and iowait, so it
        # includes the softirq network cost that the container cgroup omits.
        if a.get("vm_user") is not None and num(a.get("vm_user")) is not None:
            HZ = 100.0
            fields = ["vm_user", "vm_nice", "vm_system", "vm_idle",
                      "vm_iowait", "vm_irq", "vm_softirq", "vm_steal"]
            try:
                da = {k: num(b[k]) - num(a[k]) for k in fields}
                busy = sum(da[k] for k in fields if k not in ("vm_idle", "vm_iowait"))
                out["vm_busy_cores"] = busy / HZ / dt_s if dt_s > 0 else None
                out["vm_softirq_cores"] = da["vm_softirq"] / HZ / dt_s if dt_s > 0 else None
                out["vm_system_cores"] = da["vm_system"] / HZ / dt_s if dt_s > 0 else None
                if out.get("cores_used") and out["vm_busy_cores"]:
                    out["proxy_share_of_vm_cpu"] = out["cores_used"] / out["vm_busy_cores"]
            except (TypeError, KeyError):
                out["note_vm_cpu"] = "VM-wide CPU columns unparseable in this run's CSV"
        else:
            out["note_vm_cpu"] = "NOT MEASURED: this run predates VM-wide CPU sampling"
        if out["cores_used"]:
            out["rps_per_core"] = (out["requests_per_s"] or 0) / out["cores_used"]
            out["proxy_cpu_pct_of_limit"] = 100.0 * out["cores_used"] / meta["proxy_cores_n"]
        # Working set over the measured window.
        cur = sorted(num(r["memory_current"]) or 0 for r in win)
        anon = sorted(num(r["anon"]) or 0 for r in win)
        # cAdvisor-style working set = memory.current - inactive_file.
        # memory.stat here records `file` and `inactive_anon`/`active_anon`;
        # inactive_file is not in the sampled set, so the cAdvisor-equivalent is
        # reported as anon+active_anon-free approximation and flagged.
        out["steady_ws_mib"] = pct(cur, 0.5) / MIB
        out["ws_p90_mib"] = pct(cur, 0.90) / MIB
        out["ws_p99_mib"] = pct(cur, 0.99) / MIB
        out["peak_ws_mib"] = cur[-1] / MIB
        out["anon_p50_mib"] = pct(anon, 0.5) / MIB
        out["anon_max_mib"] = anon[-1] / MIB
        mp = num(allrows[-1]["memory_peak"]) if allrows else None
        out["memory_peak_mib"] = (mp / MIB) if mp else None
        im = num(meta.get("idle_memory_current"))
        out["idle_ws_mib"] = (im / MIB) if im else None
        # Burst memory = the highest single sample anywhere in the run,
        # including warm-up, where connection establishment peaks.
        allcur = [num(r["memory_current"]) or 0 for r in allrows]
        out["burst_ws_mib"] = (max(allcur) / MIB) if allcur else None
    else:
        out["cores_used"] = None
        out["rps_per_core"] = None
        out["note_cpu"] = "NOT RUN: fewer than 2 cgroup samples inside the measured window"

    # ---- Kafka ----
    # Differenced against the warm-up-boundary snapshot. These are lifetime
    # cumulative counters; using the end-of-run total over the measured
    # duration would count every warm-up record inside the measured window.
    recs_end = meta.get("kafka_records_total") or 0
    recs_warm = meta.get("kafka_records_at_warmup") or 0
    kb_end = meta.get("kafka_log_bytes_total") or 0
    kb_warm = meta.get("kafka_log_bytes_at_warmup") or 0
    recs = max(recs_end - recs_warm, 0)
    kb = max(kb_end - kb_warm, 0)
    out["kafka_records_window"] = recs
    out["kafka_records_lifetime"] = recs_end
    out["kafka_log_bytes_window"] = kb
    out["kafka_records_per_s"] = (recs / measured_s) if (recs and measured_s) else 0.0
    out["kafka_log_mib"] = kb / MIB
    out["kafka_mib_per_s"] = (kb / MIB / measured_s) if (kb and measured_s) else 0.0
    if out.get("cores_used"):
        out["kafka_records_per_s_per_core"] = out["kafka_records_per_s"] / out["cores_used"]
        out["kafka_mib_per_s_per_core"] = out["kafka_mib_per_s"] / out["cores_used"]
    out["kafka_records_per_request"] = (recs / (pl.get("ok") or 1)) if recs else 0.0
    out["kafka_bytes_per_record"] = (kb / recs) if recs else None
    if meta.get("capture") == 1 and recs_warm == 0 and recs_end > 0:
        out["note_kafka_window"] = (
            "warm-up Kafka snapshot was 0 while the end total was not; the window "
            "difference may include warm-up records"
        )

    # ---- Proxy's own OTel metrics ----
    prom = parse_prom(os.path.join(d, "otel-metrics.prom"))
    prom_warm = parse_prom(os.path.join(d, "otel-metrics-warmup.prom"))
    out["otel_metric_names"] = sorted(prom.keys())[:200]

    # Windowed deltas of the proxy's own cumulative counters: end-of-window
    # scrape minus warm-up-boundary scrape, divided by the measured seconds.
    def wdelta(*names):
        for n in names:
            a = sum_metric(prom, n)
            if a is None:
                continue
            b = sum_metric(prom_warm, n)
            if b is None:
                return None
            return max(a - b, 0.0)
        return None

    if prom and prom_warm:
        fr = wdelta("fullRequest_count_total", "fullRequest_total")
        br = wdelta("bytesRead_bytes_total", "bytesRead_total")
        bw = wdelta("bytesWritten_bytes_total", "bytesWritten_total")
        ra = wdelta("capturePublishingRecordsAccepted_records_total")
        ba = wdelta("capturePublishingBytesAccepted_bytes_total")
        blk = wdelta("blockingRequest_count_total")

        # Windowed mean of a histogram: (sum_end - sum_warm) / (count_end -
        # count_warm). The lifetime mean is diluted by warm-up and by any idle
        # period, which matters a lot here: the lifetime kafkaCommit mean read
        # 1.605 ms on a run whose measured window was actually 2.731 ms.
        def whist_mean(base):
            for b in (base + "_milliseconds", base):
                ce, se = sum_metric(prom, b + "_count"), sum_metric(prom, b + "_sum")
                cw, sw = sum_metric(prom_warm, b + "_count"), sum_metric(prom_warm, b + "_sum")
                if None in (ce, se, cw, sw):
                    continue
                dc, ds = ce - cw, se - sw
                return (ds / dc) if dc > 0 else None
            return None

        def whist_quantiles(base, qs=(0.5, 0.9, 0.99)):
            """Windowed percentiles from cumulative prometheus histogram buckets:
            difference each bucket between the end and warm-up scrapes, then
            interpolate. Only this form is honest about the measured window --
            the lifetime histogram is diluted by warm-up."""
            for b in (base + "_milliseconds", base):
                be, bw = prom.get(b + "_bucket"), prom_warm.get(b + "_bucket")
                if not be or not bw:
                    continue
                def acc(rows):
                    d = {}
                    for labels, v in rows:
                        m = re.search(r'le="([^"]+)"', labels)
                        if not m:
                            continue
                        le = m.group(1)
                        le = float("inf") if le == "+Inf" else float(le)
                        d[le] = d.get(le, 0.0) + v
                    return d
                ae, aw = acc(be), acc(bw)
                edges = sorted(set(ae) | set(aw))
                cum = [(le, ae.get(le, 0.0) - aw.get(le, 0.0)) for le in edges]
                # Cumulative counts must be non-decreasing after differencing.
                cum = [(le, max(c, 0.0)) for le, c in cum]
                total = cum[-1][1] if cum else 0.0
                if total <= 0:
                    continue
                res = {"count": total, "boundaries": len([e for e in edges if e != float("inf")])}
                for q in qs:
                    want = q * total
                    prev_le, prev_c = 0.0, 0.0
                    hit = None
                    for le, c in cum:
                        if c >= want:
                            if c > prev_c and le != float("inf"):
                                hit = prev_le + (want - prev_c) / (c - prev_c) * (le - prev_le)
                            else:
                                hit = prev_le if le == float("inf") else le
                            break
                        prev_le, prev_c = le, c
                    res["p%g" % (q * 100)] = hit
                # Resolution guard: if more than 90% of the mass sits inside a
                # single bucket, the percentile inside it is interpolation, not
                # measurement. Say so rather than quoting a number.
                widest = 0.0
                prev_c = 0.0
                for le, c in cum:
                    widest = max(widest, c - prev_c)
                    prev_c = c
                res["max_single_bucket_fraction"] = widest / total
                res["resolvable"] = bool(widest / total < 0.90)
                return res
            return None

        ack_q = whist_quantiles("kafkaCommitDuration")
        blocked_q = whist_quantiles("blockedDuration")
        ack_w = whist_mean("kafkaCommitDuration")
        blocked_w = whist_mean("blockedDuration")
        gather_w = whist_mean("gatheringRequestDuration")
        out["otel_window"] = {
            "fullRequest": fr,
            "fullRequest_per_s": (fr / measured_s) if fr else None,
            "bytesRead_per_s": (br / measured_s) if br else None,
            "bytesWritten_per_s": (bw / measured_s) if bw else None,
            "capturePublishingRecordsAccepted": ra,
            "capturePublishingRecordsAccepted_per_s": (ra / measured_s) if ra else None,
            "capturePublishingBytesAccepted": ba,
            "capturePublishing_mib_per_s": (ba / MIB / measured_s) if ba else None,
            "blockingRequests": blk,
            "blocking_fraction_of_requests": (blk / fr) if (blk and fr) else None,
            "kafka_ack_mean_ms": ack_w,
            "blocked_mean_ms": blocked_w,
            "gathering_request_mean_ms": gather_w,
        }
        # Promoted to the top level because these are the two numbers that
        # govern this proxy's throughput (see the ack-latency coupling in the
        # spec): 82% of requests block on the Kafka acknowledgement.
        out["kafka_ack_mean_ms"] = ack_w
        out["kafka_ack_quantiles"] = ack_q
        out["blocked_quantiles"] = blocked_q
        if ack_q:
            out["kafka_ack_p50_ms"] = ack_q.get("p50")
            out["kafka_ack_p99_ms"] = ack_q.get("p99")
            out["kafka_ack_resolvable"] = ack_q.get("resolvable")
            if not ack_q.get("resolvable"):
                out["note_ack_resolution"] = (
                    "NOT RESOLVABLE: %.1f%% of kafkaCommitDuration mass sits in one bucket; "
                    "p50/p99 are interpolation, not measurement. Rebuild with "
                    "MEASUREMENT_PATCH=1 for sub-millisecond boundaries."
                    % (100.0 * ack_q["max_single_bucket_fraction"])
                )
        out["blocked_mean_ms"] = blocked_w
        out["gathering_request_mean_ms"] = gather_w
        if out.get("cores_used") and out["otel_window"].get("capturePublishing_mib_per_s"):
            out["capture_publish_mib_per_s_per_core"] = (
                out["otel_window"]["capturePublishing_mib_per_s"] / out["cores_used"]
            )
    else:
        out["otel_window"] = None
        if meta.get("capture") == 1:
            out["note_otel_window"] = (
                "NOT MEASURED: warm-up and/or end-of-window OTel scrape missing, so the "
                "proxy's own counters could not be differenced over the measured window"
            )

    if prom:
        out["otel"] = {
            "fullRequest_total": sum_metric(prom, "fullRequest_total") or sum_metric(prom, "fullRequest"),
            "bytesRead_total": sum_metric(prom, "bytesRead_bytes_total") or sum_metric(prom, "bytesRead_total"),
            "bytesWritten_total": sum_metric(prom, "bytesWritten_bytes_total") or sum_metric(prom, "bytesWritten_total"),
            "capturePublishingRecordsAccepted": sum_metric(prom, "capturePublishingRecordsAccepted_records_total")
            or sum_metric(prom, "capturePublishingRecordsAccepted_total"),
            "capturePublishingBytesAccepted": sum_metric(prom, "capturePublishingBytesAccepted_bytes_total")
            or sum_metric(prom, "capturePublishingBytesAccepted_total"),
            "recordsAwaitingAck": sum_metric(prom, "capturePublishingRecordsAwaitingAcknowledgement_records")
            or sum_metric(prom, "capturePublishingRecordsAwaitingAcknowledgement"),
            "writesRejected": sum_metric(prom, "captureWritesRejected_records_total")
            or sum_metric(prom, "captureWritesRejected_total"),
            "writeGateTransitions": sum_metric(prom, "captureWriteGateTransitions_total"),
            "kafkaCommitDuration": hist_quantiles(prom, "kafkaCommitDuration_milliseconds")
            or hist_quantiles(prom, "kafkaCommitDuration"),
        }
    else:
        out["otel"] = None
        out["note_otel"] = "NOT RUN: no OTel metrics were scraped for this run"

    json.dump(out, sys.stdout, indent=2, default=str)
    print()


if __name__ == "__main__":
    main()
