#!/usr/bin/env python3
"""Flat CPU / allocation attribution from `jfr print` text output.

   jfr_buckets.py <exec-top1.txt> <exec-deep.txt> <alloc.txt>

Buckets are decided by matching frame patterns anywhere in the stack, in
precedence order. Precedence matters: protobuf encoding is always reached from
the capture path, and Kafka's producer always ends in a socket write, so a
"first match wins on the most specific subsystem" rule is the only way to get a
flat split rather than everything landing in "socket I/O".

ExecutionSample counts Java-state samples; NativeMethodSample counts samples
whose top frame is a native method (socket reads/writes, SSL, epoll). They are
reported separately AND combined, because the ratio between them is itself the
headline result for a proxy: it says how much of the cost is syscall/transport
versus interpreted/compiled work a rewrite could change.
"""
import collections
import json
import re
import sys

# (bucket, [regexes]) in precedence order.
BUCKETS = [
    ("tls", [
        r"\bsun\.security\.ssl\b", r"io\.netty\.handler\.ssl", r"javax\.crypto",
        r"sun\.security\.provider", r"com\.sun\.crypto\.provider",
        r"java\.security\.MessageDigest", r"\bSSLEngine", r"\bGaloisCounterMode",
        r"\bAESCrypt", r"\bGHASH",
    ]),
    ("protobuf", [
        r"\bcom\.google\.protobuf\b", r"CodedOutputStream", r"CodedInputStream",
        r"\borg\.opensearch\.migrations\.trafficcapture\.protos\b",
    ]),
    ("kafka", [
        r"\borg\.apache\.kafka\b", r"kafkaoffloader", r"\bKafkaCaptureFactory\b",
        r"\bRecordAccumulator\b", r"\bSender\b",
    ]),
    ("capture_offload", [
        r"trafficcapture\.(?!netty)\w*offloader", r"\bStreamChannelConnectionCaptureSerializer\b",
        r"trafficcapture\.captureoffloader",
    ]),
    ("telemetry", [
        r"\bio\.opentelemetry\b", r"\.tracing\.", r"\bRootOtelContext\b",
        r"\bMeter\b", r"\bLongCounter\b", r"\bDoubleHistogram\b",
    ]),
    ("logging", [
        r"\borg\.apache\.logging\b", r"\borg\.slf4j\b", r"\bLog4j", r"\bLogger\b",
    ]),
    ("http_decode", [
        r"io\.netty\.handler\.codec\.http", r"\bHttpObjectDecoder\b",
        r"\bHttpObjectAggregator\b", r"\bHttpRequestDecoder\b",
        r"\bHeaderRemoverHandler\b", r"\bHeaderAdderHandler\b",
        r"io\.netty\.handler\.codec\b",
    ]),
    ("proxy_logic", [
        r"trafficcapture\.proxyserver", r"trafficcapture\.netty",
    ]),
    ("netty_transport", [
        r"io\.netty\.channel", r"io\.netty\.buffer", r"io\.netty\.util",
        r"\bNioEventLoop\b", r"\bEpollEventLoop\b", r"io\.netty\.bootstrap",
    ]),
    ("jdk_socket_nio", [
        r"\bsun\.nio\.ch\b", r"\bjava\.net\b", r"\bSocketDispatcher\b",
        r"\bNativeThread\b", r"\bIOUtil\b",
    ]),
    ("jdk_other", [r"^\s*java\.", r"^\s*jdk\.", r"^\s*sun\."]),
]
COMPILED = [(name, [re.compile(p) for p in pats]) for name, pats in BUCKETS]

FRAME_RE = re.compile(r"^\s+([\w$.]+)\.([\w$<>]+)\(")
SAMPLE_SPLIT = re.compile(r"^jdk\.(ExecutionSample|NativeMethodSample|ObjectAllocationSample)\s*\{",
                          re.M)


def parse_samples(path):
    """Yield (event_type, raw_block) for each event in a `jfr print` dump."""
    try:
        text = open(path, errors="replace").read()
    except OSError:
        return
    marks = [(m.start(), m.group(1)) for m in SAMPLE_SPLIT.finditer(text)]
    for i, (pos, typ) in enumerate(marks):
        end = marks[i + 1][0] if i + 1 < len(marks) else len(text)
        yield typ, text[pos:end]


def bucket_of(block):
    """Bucket by 'a frame anywhere in the stack', in precedence order.

    This OVER-attributes to whichever subsystem sits highest in the precedence
    list, because a tracing or Kafka frame appears in almost every capture
    stack. Kept only for comparison against bucket_nearest().
    """
    for name, pats in COMPILED:
        for p in pats:
            if p.search(block):
                return name
    return "unattributed"


def frames_of(block):
    out = []
    for line in block.splitlines():
        m = FRAME_RE.match(line)
        if m:
            out.append(m.group(1) + "." + m.group(2))
    return out


def bucket_nearest(block):
    """Attribute a sample's self time to the NEAREST owning subsystem.

    Walk the stack from the executing frame downwards and take the first frame
    that matches a non-generic bucket. This is the honest flat attribution: a
    sample caught inside ConcurrentSkipListMap.findPredecessor is charged to
    whatever called it (here, the telemetry context tracker) rather than to
    'jdk_other', and a sample inside protobuf's CodedOutputStream is charged to
    protobuf rather than to the Kafka frame further down the stack.
    """
    frames = frames_of(block)
    generic = {"jdk_other"}
    for f in frames:
        for name, pats in COMPILED:
            if name in generic:
                continue
            for p in pats:
                if p.search(f):
                    return name
    # Nothing specific anywhere: fall back to the generic classification of the
    # executing frame.
    if frames:
        for name, pats in COMPILED:
            for p in pats:
                if p.search(frames[0]):
                    return name
    return "unattributed"


def top_frame(block):
    for line in block.splitlines():
        m = FRAME_RE.match(line)
        if m:
            return m.group(1) + "." + m.group(2)
    return "?"


def thread_of(block):
    m = re.search(r'sampledThread = "([^"]*)"', block)
    if m:
        return m.group(1)
    m = re.search(r'eventThread = "([^"]*)"', block)
    return m.group(1) if m else "?"


def pctmap(counter, total):
    return {
        k: {"samples": v, "pct": round(100.0 * v / total, 2)}
        for k, v in sorted(counter.items(), key=lambda kv: -kv[1])
    }


def main():
    top1_path, deep_path, alloc_path = sys.argv[1], sys.argv[2], sys.argv[3]

    out = {}

    # Flat top-frame hot methods (what the CPU was literally executing).
    hot = collections.Counter()
    by_thread = collections.Counter()
    n_top = 0
    for typ, blk in parse_samples(top1_path):
        hot[top_frame(blk)] += 1
        by_thread[thread_of(blk)] += 1
        n_top += 1
    out["exec_top_frame_samples"] = n_top
    out["exec_top_frames"] = dict(list(pctmap(hot, max(n_top, 1)).items())[:35])
    out["exec_by_thread"] = dict(list(pctmap(by_thread, max(n_top, 1)).items())[:25])

    # Subsystem buckets from deep stacks, split by sample kind.
    buckets = {"ExecutionSample": collections.Counter(),
               "NativeMethodSample": collections.Counter()}
    nearest = {"ExecutionSample": collections.Counter(),
               "NativeMethodSample": collections.Counter()}
    by_thread_deep = collections.Counter()
    counts = collections.Counter()
    for typ, blk in parse_samples(deep_path):
        if typ not in buckets:
            continue
        buckets[typ][bucket_of(blk)] += 1
        nearest[typ][bucket_nearest(blk)] += 1
        by_thread_deep[thread_of(blk)] += 1
        counts[typ] += 1
    nall = nearest["ExecutionSample"] + nearest["NativeMethodSample"]
    ntot = sum(nall.values())
    out["cpu_buckets_nearest_owner"] = pctmap(nall, max(ntot, 1))
    out["cpu_buckets_nearest_java_only"] = pctmap(nearest["ExecutionSample"],
                                                  max(counts["ExecutionSample"], 1))
    out["cpu_by_thread"] = dict(list(pctmap(by_thread_deep, max(ntot, 1)).items())[:25])
    combined = buckets["ExecutionSample"] + buckets["NativeMethodSample"]
    total = sum(combined.values())
    out["sample_counts"] = dict(counts)
    out["cpu_buckets_java_only"] = pctmap(buckets["ExecutionSample"],
                                          max(counts["ExecutionSample"], 1))
    out["cpu_buckets_native_only"] = pctmap(buckets["NativeMethodSample"],
                                            max(counts["NativeMethodSample"], 1))
    out["cpu_buckets_combined"] = pctmap(combined, max(total, 1))
    out["java_vs_native_sample_split"] = {
        "java_samples": counts["ExecutionSample"],
        "native_samples": counts["NativeMethodSample"],
        "native_pct": round(100.0 * counts["NativeMethodSample"] / max(total, 1), 2),
    }

    # Allocation attribution: by allocated type and by subsystem bucket.
    alloc_type = collections.Counter()
    alloc_bucket = collections.Counter()
    alloc_weight = collections.Counter()
    n_alloc = 0
    for typ, blk in parse_samples(alloc_path):
        if typ != "ObjectAllocationSample":
            continue
        n_alloc += 1
        m = re.search(r"objectClass = ([\w$.\[\]]+)", blk)
        t = m.group(1) if m else "?"
        alloc_type[t] += 1
        b = bucket_of(blk)
        alloc_bucket[b] += 1
        w = re.search(r"weight = ([\d.]+)", blk)
        if w:
            alloc_weight[b] += float(w.group(1))
    out["alloc_samples"] = n_alloc
    out["alloc_by_type"] = dict(list(pctmap(alloc_type, max(n_alloc, 1)).items())[:30])
    out["alloc_by_bucket"] = pctmap(alloc_bucket, max(n_alloc, 1))
    tw = sum(alloc_weight.values())
    out["alloc_weight_bytes_by_bucket"] = {
        k: {"bytes": int(v), "pct": round(100.0 * v / tw, 2)}
        for k, v in sorted(alloc_weight.items(), key=lambda kv: -kv[1])
    } if tw else None
    out["alloc_weight_total_bytes"] = int(tw) if tw else None

    json.dump(out, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
