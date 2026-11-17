# Patched `rdkafka-sys` (librdkafka 2.12.1 + two upstream fixes)

The capture proxy builds librdkafka from source through `rdkafka-sys`. Two librdkafka defects in
the idempotent producer limit capture publication, and both have open, unmerged upstream fixes.
Until a release contains them, the workspace builds a patched copy:

| Patch | Upstream | Effect without it |
|---|---|---|
| `0001-idempotent-cap-inflight-requests-not-messages.patch` | PR [#5636](https://github.com/confluentinc/librdkafka/pull/5636) for issue [#5632](https://github.com/confluentinc/librdkafka/issues/5632) | A partition with five *records* in flight cannot start another request, so under load each partition gets about one request per round trip and records wait two round trips (Phase 1 review §5.5) |
| `0002-idempotent-no-spin-while-partition-cannot-send.patch` | PR [#5622](https://github.com/confluentinc/librdkafka/pull/5622) (draft) for issue [#5617](https://github.com/confluentinc/librdkafka/issues/5617) | The broker thread busy-spins while a partition is at the cap (measured: 2.4 proxy cores at 70 req/s with four producers under a slow broker) |

Neither changes the idempotence contract: at most five requests per partition stay within the
broker's five-batch de-duplication window, which is exactly what kafka-clients allows.

## Use

```bash
third_party/rdkafka-sys/prepare.sh   # once, before any cargo command
cargo build
```

`prepare.sh` downloads `rdkafka-sys 4.10.0+2.12.1`, verifies its SHA-256 against the value
`Cargo.lock` pinned for the unpatched crate, applies `patches/` in order, and writes the result to
`crate/` (git-ignored). The workspace `Cargo.toml` redirects `rdkafka-sys` there with
`[patch.crates-io]`.

## Removing

When a librdkafka release contains both fixes and `rdkafka-sys` bundles it: delete this directory
and the `[patch.crates-io]` entry, bump `rdkafka`, and re-run the producer diagnostics in
`qualification/perf/` to confirm the behaviour.
