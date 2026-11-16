#!/usr/bin/env python3
"""Decide whether one run is a valid measurement. Exit 0 = valid, 1 = reject.

   validate_run.py <run-dir>

This exists because a run in which EVERY request failed was recorded as "OK":
the generator exited 0 (it had honestly counted 761,216 dial errors and 0
successes) and nothing downstream checked. A measurement harness that can
report a dead proxy as a result is worse than no harness.

Rejection criteria, each with the failure it is there to catch:

  no_successes        the proxy died or never served (the case above)
  error_rate          > 1% of requests failed -> the run is a failure study,
                      not a throughput measurement
  timebase_skew       wall-clock span of the generator phase deviates from
                      warmup+duration by more than 25%, which means a Docker
                      container-create stall shifted t_start/t_end and the
                      cgroup window no longer lines up with the load
  sampler_coverage    fewer cgroup samples inside the measured window than 60%
                      of the expected count -> CPU and memory figures are built
                      on too few points
  no_cpu              cores_used could not be computed at all
  accounting          the generator's own lifetime invariant failed
  proxy_exited        the proxy container is not running at the end of the run
  kafka_silent        capture was enabled but the broker accepted no records
"""
import json
import os
import sys


def main():
    d = sys.argv[1]
    try:
        s = json.load(open(os.path.join(d, "summary.json")))
    except Exception as e:
        print(f"REJECT {os.path.basename(d)}: cannot read summary.json: {e}")
        return 1

    reasons = []
    warn = []

    ok = s.get("requests_per_s") or 0
    att = s.get("attempted_per_s") or 0
    errs = s.get("errors_total") or 0
    if ok <= 0:
        reasons.append(f"no_successes (requests_per_s={ok}, errors={errs}, "
                       f"error_breakdown={s.get('errors')})")
    else:
        total = ok + (errs / max(s.get("measured_s") or 1, 1e-9))
        rate = (errs / max(att * (s.get("measured_s") or 1), 1)) if att else 0.0
        if rate > 0.01:
            reasons.append(f"error_rate {rate*100:.2f}% > 1% (errors={errs})")
        elif errs > 0:
            warn.append(f"nonzero errors: {errs} ({rate*100:.3f}%)")

    expected = (s.get("warmup_s") or 0) + (s.get("duration_s") or 0)
    span = ((s.get("t_end_ms") or 0) - (s.get("t_start_ms") or 0)) / 1000.0
    if expected > 0:
        if span > expected * 1.25 or span < expected * 0.75:
            reasons.append(f"timebase_skew: generator phase spanned {span:.0f}s "
                           f"but warmup+duration is {expected:.0f}s")

    # A none-direct run has no proxy, so there is no proxy cgroup to sample and
    # no capture path to produce Kafka records. Applying the proxy-side checks
    # to it would reject the harness-ceiling reference run (the one that proves
    # the generator is not the bottleneck) for lacking something it cannot have.
    is_direct = s.get("proxy_kind") == "none-direct"

    if not is_direct:
        if s.get("cores_used") in (None, 0):
            reasons.append("no_cpu: cores_used could not be computed")
        nwin = s.get("cgroup_samples_in_window") or 0
        interval = s.get("sample_interval_s") or 1
        expect_samples = (s.get("duration_s") or 0) / max(interval, 1e-9)
        if expect_samples and nwin < 0.6 * expect_samples:
            reasons.append(f"sampler_coverage: {nwin} cgroup samples in window, "
                           f"expected about {expect_samples:.0f}")

    if s.get("accounting_consistent") is False:
        reasons.append("accounting: generator lifetime invariant failed")

    if s.get("capture") == 1 and not is_direct and not (s.get("kafka_records_window") or 0):
        reasons.append("kafka_silent: capture enabled but broker accepted no "
                       "records inside the measured window")

    # Profile PRECONDITION check, not an outcome check.
    #
    # A healthy-Kafka profile asserts that Kafka is healthy. Because the proxy
    # runs --capture-failure-policy fail-closed, 82% of requests block on the
    # Kafka acknowledgement, so proxy throughput is governed almost entirely by
    # ack latency: measured on this host, ack mean 0.79 ms gave 32,245 req/s
    # while ack mean 6.59 ms gave 3,595 req/s on the identical profile. A run
    # whose broker was starved is a valid measurement of a DEGRADED system but
    # not of the profile it claims to be, so it is rejected here and re-run.
    #
    # This is deliberately a test on an input (broker health), never on the
    # output (throughput): rejecting runs for having low throughput would be
    # cherry-picking. The threshold is 2.0 ms, about 2.5x the observed healthy
    # ack mean of 0.79-0.81 ms.
    # Threshold calibration note: an earlier value of 2.0 ms was calibrated on
    # plaintext-capture (healthy ack mean 0.70-0.75 ms) and systematically
    # rejected churn23-tls runs that were merely at that profile's own healthy
    # level (2.02-2.59 ms — more connections means more TrafficStreams and more
    # Kafka records per request, so a higher healthy baseline). A single
    # absolute threshold cannot separate "healthy churn profile" from "degraded
    # plaintext profile" because the two ranges overlap. So this per-run check
    # is now only a coarse pathology guard, and the real outlier rejection is
    # profile-RELATIVE and happens in aggregate.py (> 2x the profile median).
    # Raised from 4.0 to 20.0 ms: the production-shape profile (both-side TLS +
    # 23% churn) has a healthy Java ack mean of ~4.7 ms, so 4.0 ms rejected
    # every Java arm -- the same miscalibration as defect #9. This per-run test
    # is now only a gross-pathology guard; the real broker-health test is the
    # profile/arm-RELATIVE pair exclusion in paired_ratio.py (> 2x arm median).
    HEALTHY_ACK_MEAN_MS = float(os.environ.get("HEALTHY_ACK_MEAN_MS", "20.0"))
    if s.get("capture") == 1 and not is_direct and not (s.get("kafka_delay_ms") or 0):
        # The WINDOWED mean, not the lifetime mean: the lifetime figure is
        # diluted by warm-up and read 1.605 ms on a run whose measured window
        # was really 2.731 ms, which would have let a degraded run through.
        ack = s.get("kafka_ack_mean_ms")
        if ack is None:
            ack = ((s.get("otel") or {}).get("kafkaCommitDuration") or {}).get("mean")
        if ack is None:
            warn.append("kafka ack latency unknown (no OTel histogram)")
        elif ack > HEALTHY_ACK_MEAN_MS:
            reasons.append(
                f"kafka_unhealthy_precondition: kafkaCommitDuration mean {ack:.3f} ms "
                f"> {HEALTHY_ACK_MEAN_MS} ms, so this profile's healthy-Kafka premise "
                f"did not hold (throughput here is ack-latency-bound)")

    name = os.path.basename(d)
    if reasons:
        print(f"REJECT {name}:")
        for r in reasons:
            print(f"    - {r}")
        return 1
    if is_direct:
        print(f"ACCEPT {name}: rps={ok:.0f} (none-direct reference; no proxy cgroup)"
              + (f"  [warn: {'; '.join(warn)}]" if warn else ""))
        return 0
    print(f"ACCEPT {name}: rps={ok:.0f} cores={s.get('cores_used'):.3f} "
          f"rps/core={s.get('rps_per_core'):.0f} ws={s.get('steady_ws_mib'):.0f}MiB"
          + (f"  [warn: {'; '.join(warn)}]" if warn else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
