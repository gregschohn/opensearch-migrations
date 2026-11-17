# WIRE/PROTOCOL differential proof — Phase 1 Gate 1 evidence

Status: **PHASE A COMPLETE AND PASSING. PHASE B NOT RUN** (no runnable Rust
image yet — see [Phase B](#9-phase-b--rust-not-run)).

What this proves today: the unchanged Java replayer, in its real production
Kafka intake and reconstruction path (`--mode dump-both`), reconstructs the
fixed corpus **identically across six independent runs of the Java capture
proxy spanning two separate Kafka clusters**, every one of 12 record-level
assertions passes on every run, and the comparison that establishes that is
sensitive enough to catch a single added byte. That is the calibrated
instrument Gate 1 needs. **It has not yet been pointed at Rust, so this
document decides nothing about Gate 1 yet.**

Nothing in this document was inferred. Every number is from a run under
`/tmp/rust-proxy-qual/wire-proof/out/wp1/`.

---

## 1. Design

```
fixed corpus --> Java proxy  --> topic wp-wp1-j1 --> unchanged Java replayer (dump-both) --> transcript J1
fixed corpus --> Java proxy  --> topic wp-wp1-j2 --> unchanged Java replayer (dump-both) --> transcript J2
fixed corpus --> Rust proxy  --> topic wp-wp1-r1 --> unchanged Java replayer (dump-both) --> transcript R1   [NOT RUN]

assert transcript J == transcript R, modulo documented nondeterminism
```

The replayer is used **unmodified**, invoked through Gradle from the repository
working tree. Its `dump-both` mode runs the real `ReplayIntakeOwner` /
`SourceConnectionState` reconstruction (`KafkaTopicDumper.runHttpFromKafka`),
so a record the production path would reject is rejected here too.

An independent decoder (`bin/analyze.py`, hand-written protobuf wire parser,
no shared code with the Java generated classes) is used as a **second** check
for the record-level assertions the replayer's text output cannot express
(timestamp type, Kafka headers, record key, observation sequence).

### Three views are compared, not one

| View | Source | Compared? |
|---|---|---|
| `reconstruction` | replayer `msg ` lines (`HttpTransactionDumper`) | **yes — primary evidence** |
| `observations` | replayer raw per-record lines (`TrafficStreamDumper`), concatenated per connection across records and coalesced | **yes** |
| `canonical_stream` | independent decoder, per-connection byte hashes | **yes** |
| record count / record boundaries | both | **no — deviation J-1** |

### Record packing is deliberately excluded (deviation J-1)

Per `TrafficCapture/captureProxy/qualification/OWNER-DECISIONS.md`, the Rust
encoder budgets the `CaptureRecord` envelope that Java leaves unbudgeted, so a
connection's observations may split across `CaptureRecord` envelopes at
different byte offsets. The comparison therefore operates on the
**concatenated observation stream per connection**, never record-for-record.
This is implemented in two places and commented as such in both:

* `bin/normalize.py` → `normalise()` sorts a connection's raw records by
  `TrafficStream` index, concatenates their token streams, then coalesces
  adjacent same-kind payload tokens and sums their byte counts. The per-
  connection record count is emitted as `record_count_NOT_COMPARED`.
* `bin/analyze.py` → `canonical()` emits per-connection byte hashes over the
  concatenated read / write / interim streams, with no record identity at all.

`bin/compare.py` skips every key whose name ends in `_NOT_COMPARED`.

**Java's own record packing (the J-1 baseline Rust is allowed to differ from),
identical in all four Java runs:**

| connection | `TrafficStream` records |
|---|---|
| c0 | 1 |
| c1 | 5 |
| c2 | 2 |
| c3 | 2 |
| c4 | 1 |
| readiness probe | 1 |

---

## 2. Exact commands

All paths absolute. Runnable verbatim. Working dir `/tmp/rust-proxy-qual/wire-proof`.

```bash
cd /tmp/rust-proxy-qual/wire-proof

# 1. Verify the pinned Java proxy image digest is present locally.
docker image inspect \
  'localhost:5001/migrations/capture_proxy@sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357' \
  >/dev/null && echo "pinned digest present"

# 2. Print the corpus and its digest (must be 5d9597e4...4cf5).
python3 -I bin/corpus.py | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["digest"])'

# 3. Bring up the private network, Kafka (KRaft, LogAppendTime, 8 partitions)
#    and the deterministic HTTP origin.
RUN_ID=wp1 ./bin/wireproof.sh up

# 4. One full pass. Creates the topic, starts the proxy, drives the corpus at
#    the socket level, SIGTERMs the proxy, dumps raw records, runs the
#    UNCHANGED Java replayer in dump-both mode.
RUN_ID=wp1 ./bin/wireproof.sh run j1 java
RUN_ID=wp1 ./bin/wireproof.sh run j2 java

# 5. Per-run assertions (independent decoder) + transcript normalisation.
for t in j1 j2; do
  python3 -I bin/analyze.py   --records out/wp1/$t/records.jsonl \
                              --manifest out/wp1/$t/driver.json \
                              --label $t --out out/wp1/$t/analysis.json
  python3 -I bin/normalize.py --transcript out/wp1/$t/replayer.stdout \
                              --out out/wp1/$t/normal.json
done

# 6. The differential comparison. Exit 0 == zero divergence.
python3 -I bin/compare.py --root out/wp1 --a j1 --b j2 \
                          --out out/wp1/compare-j1-j2.json

# 7. Tear down.
RUN_ID=wp1 ./bin/wireproof.sh down
```

The replayer command issued inside step 4, with the flag names confirmed
against `TrafficReplayer.java:141,364,371` rather than assumed:

```bash
cd <repo-root> && ./gradlew --console=plain -q \
  :TrafficCapture:trafficReplayer:run \
  --args="--mode dump-both --kafka-traffic-brokers 127.0.0.1:29599 \
          --kafka-traffic-topic wp-wp1-j1 \
          --preview-bytes-read 0 --preview-bytes-write 0"
```

**Correction to the brief:** the task specified a unique *consumer group* per
run. Dump modes take none. `TrafficReplayer.validateDumpModeParams()` (line
725-731) *rejects* `--kafka-traffic-group-id` in dump modes outright, and
`KafkaTopicDumper.runDumpFromKafka()` removes `GROUP_ID_CONFIG` and uses
`assign()` with an explicit seek and no commit. Uniqueness is carried by the
topic name instead (`wp-$RUN_ID-$tag`). `--preview-bytes-read/write 0` suppress
the hex payload previews, which are redundant once bytes are compared by hash.

Negative control (reproduces the sensitivity result in §5):

```bash
cd /tmp/rust-proxy-qual/wire-proof
rm -rf bin-neg && cp -r bin bin-neg
sed -i '' 's|_body_bulk(128)),|_body_bulk(129)),|' bin-neg/corpus.py
WP_BIN=$PWD/bin-neg RUN_ID=wp1 ./bin/wireproof.sh run neg java
python3 -I bin/analyze.py   --records out/wp1/neg/records.jsonl \
                            --manifest out/wp1/neg/driver.json \
                            --label neg --out out/wp1/neg/analysis.json
python3 -I bin/normalize.py --transcript out/wp1/neg/replayer.stdout \
                            --out out/wp1/neg/normal.json
python3 -I bin/compare.py --root out/wp1 --a j1 --b neg \
                          --out out/wp1/compare-j1-neg.json   # expect exit 1
```

### Environment actually used

| Component | Value |
|---|---|
| Kafka | `apache/kafka:3.9.1`, single broker, KRaft, `KAFKA_LOG_MESSAGE_TIMESTAMP_TYPE=LogAppendTime`, `KAFKA_NUM_PARTITIONS=8`; topic also created with `--config message.timestamp.type=LogAppendTime --partitions 8` |
| Java proxy | `localhost:5001/migrations/capture_proxy@sha256:8abd1f4d91e9db138da3909f1092ec8428b695491fc17da5c9b81ebca6008357` — digest verified present locally before use; no rebuild needed |
| Origin + driver | `python:3.12-alpine`, scripts bind-mounted read-only, run under `python -I` |
| Record dumper | `java -cp <kafka-clients-4.2.0>:<slf4j-api-2.0.18> bin/RecordDump.java` (single-file source mode) |
| Replayer | repository working tree, unmodified, via `./gradlew` |
| Proxy flags (identical for both proxies) | `--listenPort 9201 --destinationUri http://wp-origin:9200 --insecureDestination --kafkaBrokers <kafka>:9092 --kafkaTopic <topic> --kafkaClientId wireproof-wp1-<tag> --suppressCaptureForUriPath '/_suppressed/.*' --traffic-stream-flush-interval-seconds 2` |

Isolation from the five concurrent agents: network `wpnet-wp1`, containers
`wpkafka-wp1` / `wpsrc-wp1` / `wppx-wp1-<tag>`, topics `wp-wp1-<tag>`, host port
`29599`, outputs under `/tmp/rust-proxy-qual/wire-proof/`. Nothing was written
inside the repository; `./gradlew` populates its usual `build/` output
directories, which is unavoidable when running the real replayer.

Exposure: every container is on a private docker network and publishes nothing,
with one exception — Kafka's `EXTERNAL` listener is published to **`127.0.0.1`
only** (`-p 127.0.0.1:29599:29092`), because the unchanged replayer runs on the
host. Verified after startup: `lsof -nP -iTCP -sTCP:LISTEN | grep 29599` →
`TCP 127.0.0.1:29599 (LISTEN)`. The origin's in-container `0.0.0.0` bind
publishes no port and is reachable only from `wpnet-wp1`.

---

## 3. The corpus

Defined once in `bin/corpus.py` and imported by both the driver (which sends
it) and the assertion engine (which compares against it).

* **Seed: `FRAG_SEED = 20261115`**, version string `wire-proof-corpus-1`.
* **Digest: `5d9597e470b50edbb89f4633f7e41ffd99bc4eed85d58c41fbc9aa12c4634cf5`**
  — verified byte-identical on host CPython 3.9.6 and container CPython 3.12,
  and on every one of the four runs (the driver re-reports it each time).
* 5 connections, 12 requests, 11 captured, 4789 request bytes, 4668 response
  bytes received.
* Driven over raw TCP sockets. No HTTP client library is used anywhere, so
  fragmentation is the harness's and not a library's.
* Fragmentation is `random.Random(f"{seed}:{version}:{ordinal}:{part}")` —
  a pure function of the seed and the request's ordinal, so a failure
  reproduces exactly.
* Requests are never pipelined (the driver reads each response before sending
  the next request), so no captured read straddles two requests.

| req | method | URI | wire bytes | head TCP segments | body TCP segments | captured |
|---|---|---|---|---|---|---|
| c0/0 | GET | `/c0/doc0?pretty=true` | 607 | 28, 391, 188 | — | yes |
| c0/1 | GET | `/c0/doc1?pretty=true` | 241 | 89, 48, 13, 40, 25, 26 | — | yes |
| c0/2 | GET | `/c0/doc2?pretty=true` | 241 | 23, 91, 27, 83, 17 | — | yes |
| c1/0 | POST | `/c1/_bulk` | 777 | 192, 30, 43 | 292, 220 | yes |
| c1/1 | PUT | `/c1/idx/_doc/1` | 397 | 45, 107, 35, 18, 2, 62 | 110, 18 | yes |
| c1/2 | DELETE | `/c1/idx/_doc/1` | 238 | 105, 133 | — | yes |
| c1/3 | PATCH | `/c1/idx/_mapping` | 336 | 272 | 14, 2, 6, 3, 19, 20 | yes |
| c2/0 | POST | `/c2/_bulk` (chunked + 2 trailers) | 704 | 70, 30, 34, 178 | 11, 381 | yes |
| c3/0 | PUT | `/c3/idx/_doc/9` (`Expect: 100-continue`) | 547 | 290, 1 | 256 | yes |
| c4/0 | GET | `/c4/before` | 231 | 231 | — | yes |
| c4/1 | GET | `/_suppressed/health` | 240 | 124, 45, 71 | — | **no (policy)** |
| c4/2 | GET | `/c4/after` | 230 | 4, 16, 130, 43, 5, 32 | — | yes |

Coverage against the brief:

* keep-alive GETs on a reused connection — **c0** (3 requests, one socket).
* the four critical fixed-length methods — **c1**: POST, PUT, DELETE, PATCH.
  These are exactly the methods named by
  `ProxyChannelInitializer.shouldGuaranteeMessageOffloading()` (lines 138-144).
* chunked with trailers — **c2**: 3 chunks (96/32/200 bytes), terminal `0`
  chunk, `Trailer: X-Checksum, X-Trailer-Two` announced, both trailers sent.
* `100 Continue` then the final response — **c3**: driver sends headers, blocks
  until the interim response arrives, then sends the 256-byte body. Captured as
  `InterimResponseObservation` (25 bytes) distinct from the final
  `WriteObservation`.
* capture suppressed by configured policy — **c4/1**, matched by
  `--suppressCaptureForUriPath '/_suppressed/.*'`. The pattern is a *full*
  match because `HeaderValueFilteringCapturePredicate` uses `Pattern.matches()`.
  Placed *between* two captured requests on the same connection, so suppression
  is proven selective rather than connection-wide.
* representative header sets — c0/0 additionally carries `Authorization`,
  `x-ODD-CaSiNg`, a duplicated `X-Repeated`, and a 240-byte `X-Long` value.
* randomized TCP fragmentation from a recorded seed — table above; 1-6 segments
  per buffer, 6 ms apart, `TCP_NODELAY` set.

The origin (`bin/source.py`) is hand-written HTTP/1.1 rather than a library so
its responses carry **no `Date` header and no clock**: each response is a pure
function of the request's `X-Wire-Conn`/`X-Wire-Req`/path, 389 bytes, identical
in every run. `http.server` would have made the response bytes nondeterministic.

---

## 4. J1-vs-J2 calibration result

**Result: J1 == J2 == J3 == J4, zero differences in all six compared sections.**

Four passes of the *same* Java proxy over the *same* bytes into four different
topics:

| comparison | reconstruction | observation stream | independent decoder | assertion verdicts | counts | labels | total |
|---|---|---|---|---|---|---|---|
| j1 vs j2 | identical | identical | identical | identical | identical | identical | **0** |
| j1 vs j3 | identical | identical | identical | identical | identical | identical | **0** |
| j2 vs j3 | identical | identical | identical | identical | identical | identical | **0** |
| j1 vs j4 | identical | identical | identical | identical | identical | identical | **0** |

And the whole published command sequence was then re-run from a clean state in
a **second, independent session** (`RUN_ID=wp2 KPORT=29600` — a different Kafka
cluster, different broker, different containers), to confirm the commands in §2
work verbatim and that the result is not a property of one long-lived broker:

| comparison | result |
|---|---|
| wp2 j1 vs wp2 j2 | **0 differences**, 12/12 assertions PASS on each |
| **wp1/j1 vs wp2/j1** (cross-session, cross-cluster) | **0 differences** |

Six Java passes in total, across two Kafka clusters, all mutually identical
after normalisation. Java's per-connection record packing was also identical in
both sessions (c0=1, c1=5, c2=2, c3=2, c4=1, probe=1), which is what makes it a
usable J-1 baseline to report Rust against.

### The normalisation is heavily load-bearing

Of the 50 transcript lines, **exactly 1 is byte-identical between J1 and J2**
before normalisation. Measured:

```
$ comm -12 <(sort j1/replayer.stdout) <(sort j2/replayer.stdout) | wc -l
       1
```

### Every field canonicalised, and the observed nondeterminism that justifies it

Each row was *measured* to differ between J1 and J2. None is guesswork.

| # | Canonicalised field | Observed J1 | Observed J2 | Why it varies |
|---|---|---|---|---|
| 1 | `nodeId` (writer node UUID) | `bb54cf3b-eb83-4ae1-bf8c-3454ea0ee2a7` | `ce3c7f4a-76db-4725-ad1b-201bcc8ad8d9` | fresh UUID per proxy process |
| 2 | `connectionId` | `2a5999fffe253c39-00000001-…` | `e6fbacfffe94e4fa-00000001-…` | Netty `Channel.id().asLongText()` — MAC + PID + sequence + timestamp + random |
| 3 | Kafka partition `p:N` | connections on p:0,3,4,7 | connections on p:0,1,2,5,6 | partition = murmur2 of the record key, which is the connectionId (row 2) |
| 4 | Kafka offset `o:N` | max offset 7 | max offset 5 | per-partition, follows from row 3 |
| 5 | Absolute observation timestamps `[ts-ts]` | 1794801516-1794801518 | 1794801724-1794801725 | wall clock |
| 6 | Relative-time columns (`0.0s 0.0s`) | span 3 s | span 2 s | derived from row 5 and the base epoch, which is whichever record the dumper saw first |
| 7 | Heartbeat `emittedAtMillis` | 1794801515800/802/803 | 1794801723950/952/953 | wall clock |
| 8 | `CaptureCapabilityProbe` probe id | `d26bf935-9595-4629-80ea-722254e95066` | `49714003-271d-4348-bf9a-39b695a4956f` | fresh UUID per process |
| 9 | Heartbeat / probe record counts | 8 heartbeats, 1 probe | 8 heartbeats, 1 probe | *stable at this run length*, but depends on wall-clock overlap with the heartbeat interval; counted and reported as `metadata_record_counts_NOT_COMPARED`, kinds-present compared |
| 10 | `TrafficStream` record count and boundaries per connection | c0=1 c1=5 c2=2 c3=2 c4=1 | same | stable for Java; excluded because **deviation J-1** permits Rust to differ. Reported as `record_count_NOT_COMPARED` |
| 11 | Read / write / interim **observation counts** | 50 read observations | 50 | **found by this calibration** — see below |

Row 11 is the one the calibration earned. The first comparison (J1 vs J2)
passed with zero differences, but J3 produced **51** read observations where
J1/J2 produced 50, and J4 produced **49**: on connection c2 the same 704 request
bytes arrived in 6 TCP reads instead of 5. Measured:

| run | read observations | c2 observation count |
|---|---|---|
| j1 | 50 | 9 |
| j2 | 50 | 9 |
| j3 | **51** | **10** |
| j4 | **49** | 9 |

How many observations carry a payload is a TCP read-boundary artifact, not a
protocol fact, so `observation_count`, `read_observations`,
`write_observations` and `interim_observations` were moved behind
`_NOT_COMPARED` and the raw-view token stream is coalesced across adjacent
same-kind tokens with byte counts summed. **The bytes themselves remain fully
compared** — by SHA-256 over the concatenated per-kind stream, by total length,
and byte-for-byte against the driver's manifest in assertion A7. Had the
comparison been run once and declared sound, this would have surfaced later as
a phantom Rust "divergence".

### Fields deliberately NOT canonicalised (they were stable, and must stay compared)

* Per-connection read / write / interim **byte counts and SHA-256 hashes**.
* The coalesced observation **kind stream** per connection, in order
  (`READ EOM REQUEST_FORWARDED WRITE … DROPPED … CLOSE`).
* The replayer's reconstruction lines: kind (`REQ`/`RSP`/`INT`/`CLOSED`), byte
  count, `UNPROVEN` flag, captured-request ordinal, and the HTTP first line.
* `EndOfMessageIndication.firstLineByteLength` / `headersByteLength` — stable
  across all four runs, so they are compared:

| connection | (firstLine, headers) per captured request |
|---|---|
| c0 | (33,544) (33,188) (33,188) |
| c1 | (23,220) (27,220) (30,188) (31,219) |
| c2 | (23,265) |
| c3 | (27,240) |
| c4 | (23,188) (22,188) |

* The number of connections, and the corpus label each connection maps to.
  Connections are identified **by the `X-Wire-Conn` tag the corpus stamps into
  every request URI/headers**, never by connection id — which is why rows 1-4
  can be dropped without losing the ability to say *which* connection diverged.

---

## 5. Comparison sensitivity (negative control)

Never weakening an assertion is only half the job; the comparison also has to be
able to fail. One corpus byte was added — the c1 PUT body went from 128 to 129
bytes, changing the corpus digest to `00ec7f82…bb85` — and the whole pipeline
was re-run as pass `neg`.

**All 12 per-run assertions still PASS on `neg`** (it is internally consistent:
the proxy faithfully captured the different bytes it was given). The
differential comparison caught it in all three views:

```
replayer_reconstruction                    1 DIFFERENCE(S)
    /c1[2]/bytes                     j1: 397      neg: 398
replayer_observation_stream                1 DIFFERENCE(S)
    /c1/stream[4]/bytes              j1: 397      neg: 398
independent_decoder_canonical_stream       2 DIFFERENCE(S)
    /c1/read_bytes_len               j1: 1748     neg: 1749
    /c1/read_bytes_sha256            j1: 3256ca75…57d7   neg: 051d9f03…7802
independent_decoder_counts                 1 DIFFERENCE(S)
    /captured_request_bytes          j1: 4549     neg: 4550
TOTAL DIFFERENCES: 5
```

This is the load-bearing result for the harness itself: per-run assertions alone
cannot decide Gate 1, the *differential* comparison is what detects a one-byte
wire difference, and it does.

---

## 6. Per-assertion results

Run on all four Java passes. `records.jsonl` metadata from the independent
`RecordDump`; protobuf decoded by the hand-written parser in `bin/analyze.py`.
Counts below are J1; J2/J3/J4 identical except where §4 row 11 notes.

Topic totals: **21 Kafka records** — 12 `trafficStream`, 8
`writerPartitionHeartbeat`, 1 `captureCapabilityProbe`.

| # | Assertion | Result | Real counts |
|---|---|---|---|
| A1 | every record value decodes as `CaptureRecord` with exactly one `payload` oneof set | **PASS** | 21/21 decoded, 0 decode errors, 0 records with unknown top-level fields; payload kinds {trafficStream: 12, writerPartitionHeartbeat: 8, captureCapabilityProbe: 1} |
| A2 | **zero Kafka headers** on every record | **PASS** | 21 records inspected, 0 with any header |
| A3 | the record key is the raw `connectionId` | **PASS** | 12/12 `trafficStream` records: key bytes == `connectionId` UTF-8 exactly, 0 mismatches. Metadata-record keys are recorded but not asserted (the brief's rule is about capture records): the 8 heartbeats are keyed `<nodeId>:1:heartbeat:<partition>` for partitions 0-7, the probe `<nodeId>:PROBE:<probeId>` |
| A4 | every timestamp is `LogAppendTime` and positive | **PASS** | timestamp **type** counts `{LogAppendTime: 21}`, 0 non-positive. The TYPE is the evidence; a positive value alone would also be produced by librdkafka substituting the client clock for a zero timestamp |
| A4b | all records of a connection share one partition | **PASS** | 6 connections, 0 spanning >1 partition (consistent with the key being the connectionId) |
| A4c | `TrafficStream` index order agrees with Kafka offset order | **PASS** | 6 connections, 0 disagreements, no absent index |
| A5 | `connectionObservationSequence` present and **+1 exactly** within a connection | **PASS** | **91 observations** across 6 connections; 0 absent, 0 gaps, 0 regressions. (`SourceConnectionState.requireSequenceContiguous`, lines 727-746, rejects both an absent field and anything other than +1) |
| A6 | `CloseObservation` is final in the stream and final in the last record; nothing follows | **PASS** | 6/6 connections: exactly one `close`, last observation of the stream and last observation of the connection's final record |
| A7a | every captured connection is a corpus connection (one each) or a readiness-probe connect/close | **PASS** | 5/5 corpus connections mapped 1:1 via `X-Wire-Conn`; 1 readiness-probe connection; 0 unmapped, 0 labels with two connections |
| A7 | captured request / response / informational bytes are **byte-exact** | **PASS** | 11 captured requests, **4549 bytes** byte-for-byte equal to what the driver sent (4789 total minus the 240-byte suppressed request); **4279 response bytes** (11 × 389) byte-for-byte equal to what the origin returned; **25 informational bytes** equal to `HTTP/1.1 100 Continue\r\n\r\n`; 0 mismatches |
| A8 | `requestForwarded` once per captured end-of-message, in end-of-message order | **PASS** | 11 `requestForwarded`, 11 `endOfMessageIndicator`; every `requestForwarded` directly followed its own EOM with no intervening read; 0 emitted for the suppressed request |
| A9 | a suppressed request yields no observation or a `requestDropped` after a captured prefix, and never a `requestForwarded` | **PASS** | 1 dropped group; captured prefix **169 of 240 bytes** (segments 124+45; the third 71-byte segment completed the headers and was not captured), verified to be a true prefix of the suppressed request; 0 `requestForwarded` for it |

### Why the suppressed-request prefix is 169 bytes and not 0

`LoggingHttpHandler.channelRead` calls `decodeRequestBytes()` *before*
`captureReadIfRequired()`, so the read that completes the request headers is
already subject to the predicate's `DROP` and is not captured. Earlier
fragments of the same request were captured while `captureState` still held the
previous request's verdict, and
`StreamChannelConnectionCaptureSerializer.cancelCaptureForCurrentRequest()`
(line 543) leaves them in the stream and appends `RequestIntentionallyDropped`.
Both outcomes the brief allows are therefore reachable; which one occurs depends
on whether the request arrived in one read or several. With this corpus the
seed fixes it at three segments, and the prefix was **169 bytes in all four
runs**. The assertion accepts either outcome and reports which occurred.

---

## 7. Replayer reconstruction counts vs what the driver sent

The replayer's own `dump-both` output, counted from the `msg ` lines, against
the driver's manifest. Identical in all four Java runs.

| | driver sent / origin returned | replayer reconstructed | match |
|---|---|---|---|
| requests (captured) | 11 | **11 `REQ`** | yes |
| requests suppressed by policy | 1 | 0 `REQ` (1 `DROPPED` in the raw view) | yes — correct by design |
| responses | 11 captured (12 returned) | **11 `RSP`**, 0 `UNPROVEN`, 0 `RSP INCOMPLETE` | yes |
| informational responses | 1 | **1 `INT[25]` — `HTTP/1.1 100 Continue`** | yes |
| closes | 5 corpus connections closed | **5 `CLOSED`** | yes |
| readiness-probe connection closed | 1 | 0 `CLOSED` | **expected** — see below |

Per-connection reconstruction, J1 (`out/wp1/j1/normal.json`):

* **c0** `REQ[607] #0 GET /c0/doc0?pretty=true` · `RSP[389]` · `REQ[241] #1` ·
  `RSP[389]` · `REQ[241] #2` · `RSP[389]` · `CLOSED`
* **c1** `REQ[777] #0 POST /c1/_bulk` · `RSP[389]` · `REQ[397] #1 PUT …` ·
  `RSP[389]` · `REQ[238] #2 DELETE …` · `RSP[389]` · `REQ[336] #3 PATCH …` ·
  `RSP[389]` · `CLOSED`
* **c2** `REQ[704] #0 POST /c2/_bulk` · `RSP[389]` · `CLOSED`
* **c3** `REQ[547] #0 PUT /c3/idx/_doc/9` · `INT[25] HTTP/1.1 100 Continue` ·
  `RSP[389]` · `CLOSED`
* **c4** `REQ[231] #0 GET /c4/before` · `RSP[389]` ·
  `REQ[230] #2 GET /c4/after` · `RSP[389]` · `CLOSED`

Note c4's captured-request ordinals are **#0 and #2**: the suppressed request
consumed ordinal #1. The replayer's own accounting agrees that a request
happened there and was dropped, which is stronger than it simply not appearing.

Every response is `RSP` with no `UNPROVEN` marker — i.e. each was proven
complete by HTTP framing or by the following request, never only by the close.

---

## 8. Failures, flakes and anomalies — stated plainly

1. **FLAKE, real, now canonicalised.** Read-observation counts varied across
   four identical runs: 50, 50, 51, 49. On run j3 connection c2's 704 request
   bytes arrived in 6 TCP reads rather than 5. Bytes identical, reconstruction
   identical. Found by running the calibration more than twice; fixed by
   coalescing payload observations and excluding observation counts. **If I had
   stopped at J1 vs J2 this would have looked like a Rust divergence later.**

2. **HARNESS ARTIFACT, accounted for not hidden.** The TCP readiness probe
   (`busybox nc -z` against the proxy port) opens and closes a connection
   before the driver runs. The proxy captures it as a connection whose entire
   stream is one `CloseObservation` (1 record). It is classified explicitly in
   assertion A7a, counted (`readiness_probe_connections: 1`, identical in all
   runs) and compared — never filtered silently. It incidentally proves a bare
   connect/close is captured and that the replayer tolerates it.

3. **REPLAYER BEHAVIOUR, by design, documented in its own source.** That probe
   connection produces **no `CLOSED` line** in the reconstruction view.
   `SourceConnectionState.applyCapturedClose()` (line 611-628) only calls
   `sink.onCapturedClose(...)` `if (requestEverReconstituted)`. Not a defect;
   recorded so it is not mistaken for one when Rust runs.

4. **FIRST ATTEMPT FAILED (harness bug, fixed).** The driver's first run died
   with `ModuleNotFoundError: No module named 'corpus'` because `python -I` is
   isolated and drops `PYTHONPATH`. Fixed by having `driver.py` insert its own
   directory into `sys.path` explicitly, keeping `-I`. The failed attempt
   produced no records and its topic was deleted before the real j1.

5. **NO REPLAYER REJECTIONS.** Zero `CaptureRecordProtocolViolationException`,
   zero `CaptureProtocolViolation`, zero exceptions, across four passes. The
   only stderr line from the replayer is the benign
   `RootOtelContext - Collector endpoints are not configured …`; the proxy log
   has exactly one `WARN`, the same OTel notice. Had a record been rejected the
   error and the offending record bytes would be reported here instead of
   worked around.

6. **Not an anomaly, worth stating:** Gradle writes to the repository's `build/`
   directories when the replayer runs. No repository *source* file was created,
   modified or deleted by this work; everything else lives under
   `/tmp/rust-proxy-qual/`.

7. **Weak spot I am not claiming to have closed.** The reference for response
   bytes in assertion A7 is the bytes the *driver received*, not the bytes the
   origin wrote. A proxy that corrupted responses identically on both the
   capture path and the client path would not be caught by A7 alone — though
   the J-vs-R comparison would still catch it if only one proxy did so. The
   origin does keep its own per-connection record of every byte it wrote
   (`out/wp1/source.jsonl`, appended one JSON line per closed connection; 32
   lines, of which 3 are the empty records left by the origin's own TCP
   readiness probes), but **cross-checking driver-received against
   origin-written is NOT RUN** — it needs a connection-level join between the
   two logs that I did not implement.

---

## 9. Phase B — Rust: NOT RUN

**NOT RUN. Reason: no runnable Rust capture-proxy image exists yet.**

Attempted so as to report something concrete rather than nothing. The only Rust
image present locally is
`localhost:5001/migrations/capture_proxy:rust-phase1-059d7b6483f2`, built from
commit `059d7b6483f2`; the branch is now at `bb630783c`. It starts and exits
immediately:

```
Error: loading capture proxy configuration
Caused by:
    only ---INLINE-JSON configuration is supported by this loader
```

so it never binds `9201` and no traffic can be driven through it. That pass was
abandoned, its container removed and its topic deleted. No Rust numbers are
reported anywhere in this document.

### Stack state right now

The `wp1` session is **left running** so Phase B can start the moment an image
exists: `wpkafka-wp1` (external listener on `127.0.0.1:29599`) and `wpsrc-wp1`
(the origin, no published port). The `wp2` verification session was torn down.
To reclaim the resources: `RUN_ID=wp1 /tmp/rust-proxy-qual/wire-proof/bin/wireproof.sh down`
— the evidence under `out/` and `logs/` survives teardown, and
`./bin/wireproof.sh up` recreates the stack.

### Phase B is turn-key

I checked the Rust CLI at HEAD: `crates/capture-config/src/cli.rs` defines
Java-compatible `alias` names for every flag the harness passes —
`listenPort`, `destinationUri`, `insecureDestination`, `kafkaBrokers`,
`kafkaTopic`, `kafkaClientId`, `suppressCaptureForUriPath` — and
`traffic-stream-flush-interval-seconds` exists in its kebab form. So the
*identical* argument vector should work against a HEAD-built image with no
harness change:

```bash
cd /tmp/rust-proxy-qual/wire-proof
RUN_ID=wp1 ./bin/wireproof.sh up          # if not already up
RUST_PROXY_IMAGE=<image-or-digest> RUN_ID=wp1 ./bin/wireproof.sh run r1 rust
python3 -I bin/analyze.py   --records out/wp1/r1/records.jsonl \
                            --manifest out/wp1/r1/driver.json \
                            --label r1 --out out/wp1/r1/analysis.json
python3 -I bin/normalize.py --transcript out/wp1/r1/replayer.stdout \
                            --out out/wp1/r1/normal.json
python3 -I bin/compare.py --root out/wp1 --a j1 --b r1 \
                          --out out/wp1/compare-j1-r1.json
```

If a build still needs the JSON loader, `RUST_PROXY_ARGS='---INLINE-JSON {…}'`
replaces the argument vector verbatim without touching the rest of the harness.

**Gate 1 verdict when Phase B runs:** zero differences in
`replayer_reconstruction`, `replayer_observation_stream`,
`independent_decoder_canonical_stream`, `assertion_verdicts` and
`independent_decoder_counts` — with `record_count_NOT_COMPARED` expected to
differ under deviation J-1 and reported, not asserted. Any other difference is
a divergence and must be reported as one, not normalised away.

---

## 10. Files

Harness (`/tmp/rust-proxy-qual/wire-proof/bin/`):

| file | role |
|---|---|
| `corpus.py` | the fixed corpus and its seeded fragmentation plan; prints the digest |
| `source.py` | deterministic HTTP/1.1 origin, `100-continue` and chunked+trailers capable, no `Date`/clock |
| `driver.py` | socket-level corpus driver; writes the `driver.json` manifest used as the byte-exactness reference |
| `RecordDump.java` | independent Kafka consumer; emits record metadata (timestamp **type**, key, header count) + base64 value, no protobuf decoding |
| `analyze.py` | hand-written protobuf parser + the 12 assertions + the packing-independent canonical stream |
| `normalize.py` | canonicalises one `dump-both` transcript into `reconstruction` + `observations` |
| `compare.py` | the differential comparison; skips only `*_NOT_COMPARED` |
| `wireproof.sh` | orchestrator: `up` / `run <tag> <java\|rust>` / `down` |
| `bin-neg/` | the perturbed-corpus copy used for the negative control |

Evidence (`/tmp/rust-proxy-qual/wire-proof/out/wp1/`): per pass `j1 j2 j3 j4 neg`
— `driver.json`, `records.jsonl`, `offsets.txt`, `replayer.stdout`,
`replayer.stderr`, `analysis.json`, `analysis.txt`, `normal.json`; plus
`compare-j1-j2.json`, `compare-j1-j3.json`, `compare-j2-j3.json`,
`compare-j1-j4.json`, `compare-j1-neg.json`, `source.jsonl`. Proxy and topic
logs under `/tmp/rust-proxy-qual/wire-proof/logs/wp1/`.
