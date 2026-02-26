# Replayer Logging Improvement Plan

Based on analysis of a 2.7.0 run (see `/Users/schohn/Downloads/ANALYSIS_SUMMARY.md`) and the mainline `replayerArchitecture.md`.

## Problem Statement

From a real run with 20 log files over ~2.5 hours, we could not determine:
- Whether the consumer was alive during 10+ minute gaps (empty polls are TRACE-only)
- Why zero commits happened (OffsetLifecycleTracker state is DEBUG-only)
- That invisible rebalances occurred (generation 13→15, skipping 14)
- Which connection was blocking the commit head
- How long the main thread was blocked between poll submissions
- Whether backpressure was engaged or not
- Source vs target response status codes (both log identically)

OTel metrics capture much of this data for dashboards, but are useless for post-mortem — customers can't easily export metric captures. Everything must be reconstructable from a single log dump.

## Design Principles

1. **Periodic heartbeats per subsystem** — Each subsystem logs its own 30s summary with min/max/current gauge values
2. **Gauges tied to OTel contexts** — UpDownCounters on scope open/close, snapshotted into heartbeat logs
3. **INFO tells the complete story** — A user reading only INFO logs can diagnose commit stalls, rebalances, backpressure, and scheduling issues
4. **No PII** — No request/response bodies. URIs are acceptable.
5. **ActiveContextMonitor compact summary in main log** — Detailed tree stays in separate file; compact counts go to main stream
6. **Progress log dual-written** — Transaction summaries go to both the main log and the separate progress file

---

## Subsystem Heartbeats (every 30s)

### 1. Kafka Subsystem (`TrackingKafkaConsumer`)

```
[INFO] KafkaHeartbeat - generation=13 partitions=[0] inflight=74434 
  commitHead={offset=15774, age=49m12s, connection=00000f88} 
  commitTail={offset=90202} queueSize=74434 
  pollsSinceLastHB=435 emptyPollsSinceLastHB=0 
  commitsSinceLastHB=0 readyToCommit=false
```

**Values to track:**
| Value | Source | Why |
|-------|--------|-----|
| `generation` | `consumerConnectionGeneration` | Detect invisible rebalances |
| `partitions` | `kafkaConsumer.assignment()` | Confirm partition ownership |
| `inflight` | `kafkaRecordsLeftToCommitEventually` | Already an AtomicInteger |
| `commitHead.offset` | `OffsetLifecycleTracker.pQueue.peek()` | **NEW** — expose min of priority queue |
| `commitHead.age` | Wall clock minus time when head offset was consumed | **NEW** — detect stuck head |
| `commitHead.connection` | Connection ID associated with head offset | **NEW** — identify the blocker |
| `commitTail.offset` | `OffsetLifecycleTracker.cursorHighWatermark` | Already tracked |
| `queueSize` | `OffsetLifecycleTracker.size()` | Already exists |
| `pollsSinceLastHB` | Counter, reset each heartbeat | **NEW** — detect poll starvation |
| `emptyPollsSinceLastHB` | Counter, reset each heartbeat | **NEW** — distinguish exhausted topic from blocked main thread |
| `commitsSinceLastHB` | Counter, reset each heartbeat | **NEW** — detect commit stalls |
| `readyToCommit` | `kafkaRecordsReadyToCommit` | Already an AtomicBoolean |

**Changes to `OffsetLifecycleTracker`:**
- Add `peekHead()` → returns min offset (already `pQueue.peek()`)
- Add `getHighWatermark()` → returns `cursorHighWatermark`
- Track the connection ID associated with each offset (currently only stores `long offset` in the pQueue — need to associate the `ITrafficStreamKey` or at least the connection ID string)

### 2. Accumulator Subsystem (`CapturedTrafficToHttpTransactionAccumulator`)

```
[INFO] AccumulatorHeartbeat - liveConnections=580 
  byState={WAITING=12, READS=45, WRITES=523} 
  oldest={conn=00000f88, state=WRITES, age=49m, offset=15774} 
  connectionsCreated=580 connectionsClosed=359 connectionsExpired=0
```

**Values to track:**
| Value | Source | Why |
|-------|--------|-----|
| `liveConnections` | `liveStreams.size()` (ExpiringTrafficStreamMap) | Active connection count |
| `byState` | Count Accumulations by `accum.state` | Distribution of states |
| `oldest.conn` | Oldest entry in ExpiringTrafficStreamMap | Identify stuck connections |
| `oldest.state` | Its `Accumulation.state` | What it's waiting for |
| `oldest.age` | Time since creation | How long it's been stuck |
| `oldest.offset` | First Kafka offset held | Correlate to commit head |
| `connectionsCreated/Closed/Expired` | Existing counters | Throughput |

**New OTel gauges (UpDownCounters):**
- `accumulator.liveConnections` — increment in `createInitialAccumulation`, decrement on close/expire
- `accumulator.connectionsInWriteState` — increment on state transition to ACCUMULATING_WRITES, decrement on `handleEndOfResponse`

### 3. Replay Engine / Target Subsystem (`ReplayEngine` + `ClientConnectionPool`)

```
[INFO] ReplayHeartbeat - tasksOutstanding=774 
  activeTargetConnections=32 cachedSessions=580 
  schedulingLag={min=48m50s, max=49m12s, current=49m02s} 
  sourceTimeRange={first=19:42:34, last=19:43:38} 
  wallClock=20:32:30 timeShifterOffset=-49m02s
  responsesSinceLastHB={200=257, 429=91, 400=0}
```

**Values to track:**
| Value | Source | Why |
|-------|--------|-----|
| `tasksOutstanding` | `totalCountOfScheduledTasksOutstanding` | Already an AtomicLong |
| `activeTargetConnections` | Existing `activeSocketConnectionsCounter` UpDownCounter | Already tracked via OTel |
| `cachedSessions` | `connectionId2ChannelCache.size()` | **NEW** — pool size |
| `schedulingLag.current` | `wallClock - timeShifter.transformSourceTimeToRealTime(lastSourceTime)` | **NEW** — detect "all scheduled now" |
| `sourceTimeRange` | Min/max source timestamps seen | **NEW** — understand traffic window |
| `timeShifterOffset` | `timeShifter` state | **NEW** — understand time mapping |
| `responsesSinceLastHB` | Counters by status code, reset each heartbeat | **NEW** — replaces per-response INFO logs |

**New OTel gauges:**
- `replay.cachedSessions` — increment on cache put, decrement on cache remove/invalidate
- `replay.schedulingLagSeconds` — observable gauge, sampled each heartbeat

### 4. Backpressure Subsystem (`BlockingTrafficSource`)

```
[INFO] BackpressureHeartbeat - engaged=false 
  stopReadingAt=19:47:35 lastTimestamp=19:43:38 
  bufferWindow=300s headroom=238s 
  readGatePermits=1
```

**Values to track:**
| Value | Source | Why |
|-------|--------|-----|
| `engaged` | `stopReadingAtRef.get().isBefore(lastTimestampSecondsRef.get())` | Is backpressure active? |
| `stopReadingAt` | `stopReadingAtRef` | Current barrier |
| `lastTimestamp` | `lastTimestampSecondsRef` | Latest source timestamp consumed |
| `bufferWindow` | `bufferTimeWindow` | Config value |
| `headroom` | `stopReadingAt - lastTimestamp` | How close to engaging |
| `readGatePermits` | `readGate.availablePermits()` | Is the gate open? |

### 5. ActiveContextMonitor Compact Summary (in main log)

Currently the `AllActiveWorkMonitor` logger goes only to `longRunningActivity/longRunningActivity.log`. The detailed tree output stays there. But a compact summary should also go to the main log.

**Current output format** (verbose, to separate file):
```
Oldest of 42 GLOBAL scopes that are past thresholds...
  age=PT2M3.456S, start=2026-02-25T19:43:26Z id=<<12345678>> httpTransaction: attribs={connectionId: 00000f88, ...}
    age=PT2M3.400S, start=... requestAccumulation: attribs={...}
      age=PT2M2.100S, start=... responseAccumulation: attribs={...}
```

**Proposed compact summary** (to main log, every 30s):
```
[INFO] ActiveContextSummary - totalActiveScopes=42 
  byType={HttpTransaction=12, KafkaRecord=8, RequestAccum=6, ResponseAccum=10, TargetRequest=6} 
  oldest3=[
    {type=ResponseAccum, conn=00000f88, age=49m, offset=15774},
    {type=HttpTransaction, conn=000010b7, age=48m, offset=15778},
    {type=TargetRequest, conn=00001495, age=47m, offset=15783}
  ]
  ageDistribution={<30s=5, 30s-60s=3, 1m-5m=8, 5m-30m=12, >30m=14}
```

**Implementation:** Add a `logCompactSummary(Logger mainLogger)` method to `ActiveContextMonitor` that:
1. Gets counts per activity type from `perActivityContextTracker`
2. Gets the 3 oldest scopes from `globalContextTracker.getActiveScopesByAge()`
3. Builds an age histogram by bucketing `getAge()` values
4. Logs one line to the main logger

The existing `run()` method continues to log the detailed tree to the separate file. The compact summary is called from the same scheduled executor, right after `run()`.

**Log config change:** Add the main log appender to the `AllActiveWorkMonitor` logger as a second appender, OR have the compact summary use a different logger name that routes to the main appender.

---

## New OTel Gauges (UpDownCounters)

These follow the existing pattern: `meterDeltaEvent(counter, +1)` on scope open, `meterDeltaEvent(counter, -1)` on scope close.

| Gauge | MetricInstruments class | Scope open | Scope close |
|-------|------------------------|------------|-------------|
| `accumulator.liveConnections` | New in Accumulator | `createInitialAccumulation` | `fireAccumulationsCallbacksAndClose` / expire |
| `accumulator.connectionsInWriteState` | New in Accumulator | State → ACCUMULATING_WRITES | `handleEndOfResponse` |
| `replay.cachedSessions` | `ChannelKeyContext.MetricInstruments` | Cache put in `getCachedSession` | Cache remove/invalidate |
| `kafka.pendingCommits` | `CommitScopeContext.MetricInstruments` | `nextSetOfCommitsMap.put` | `safeCommit` success |

These serve double duty: OTel dashboards get real-time gauges, and the heartbeat logs snapshot them for post-mortem.

---

## OffsetLifecycleTracker Enhancements

The tracker currently stores only `long offset` in its priority queue. To support the heartbeat's `commitHead.connection` field, we need to associate metadata with each offset.

**Option A (minimal):** Keep the pQueue as `PriorityQueue<Long>`, add a parallel `Map<Long, String> offsetToConnectionId`. Populated in `add()`, removed in `removeAndReturnNewHead()`.

**Option B (cleaner):** Change pQueue to `PriorityQueue<OffsetEntry>` where `OffsetEntry` has `long offset` + `String connectionId`. Comparator uses offset only.

Either way, expose:
- `long peekHeadOffset()` — `pQueue.peek()`
- `String peekHeadConnectionId()` — connection ID of the head
- `long getHighWatermark()` — `cursorHighWatermark`
- `Duration getHeadAge(Clock clock)` — time since head was added (needs timestamp tracking)

---

## Progress Log Dual-Write

**Log4j2 config change only — no code change:**

```properties
logger.TransactionSummaryLogger.appenderRef.ReplayerLogFile.ref = ReplayerLogFile
```

Add the main `ReplayerLogFile` appender as a second target for `TransactionSummaryLogger`. Each transaction summary line appears in both `progress/progress.log` and the main `replayer.log`.

The progress log format is already compact (one line per transaction from `ResultsToLogsConsumer`). Adding it to the main log gives the complete story in one file without code changes.

---

## Existing INFO Logs to Demote to DEBUG

To make room for heartbeats and reduce noise:

| Current log | Count in analysis run | Proposed level |
|-------------|----------------------|----------------|
| `NettyDecodedHttpResponseConvertHandler - parsed response: ...` | 3,156 | DEBUG (replaced by heartbeat response counters) |
| `ReplayEngine - Scheduling 'processing' ...` | ~1,700 | DEBUG (replaced by heartbeat tasksOutstanding) |
| `ReplayEngine - Scheduled task '...' finished ...` | ~1,700 | DEBUG |
| `TrackingKafkaConsumer - Kafka consumer poll has fetched 1 records` | ~8,800 | Keep at INFO but add empty poll throttled logging |
| `RequestSenderOrchestrator - IndexedChannelInteraction ... scheduling TRANSMIT` | ~1,057 | DEBUG |
| `CapturedTrafficToHttpTransactionAccumulator - Connection terminated` | ~1,146 | DEBUG (replaced by heartbeat liveConnections gauge) |

This removes ~9,500 per-event INFO lines and replaces them with ~10 heartbeat lines/minute (300 over the same 2.5 hour run). Net reduction: ~97% fewer lines, dramatically better signal-to-noise.

---

## Logs to Keep/Add at INFO

| Log | When | Purpose |
|-----|------|---------|
| Kafka heartbeat | Every 30s | Consumer liveness, commit progress, inflight |
| Accumulator heartbeat | Every 30s | Connection state distribution, stuck connections |
| Replay heartbeat | Every 30s | Task count, scheduling lag, target response rates |
| Backpressure heartbeat | Every 30s | Engaged/not, headroom |
| ActiveContext compact summary | Every 30s | Oldest activities, age distribution |
| Progress one-liner (dual-write) | Per transaction | Transaction outcome |
| Rebalance events | On occurrence | Generation, committed offset, partition changes |
| Commit flushed | On occurrence | New committed offset |
| Commit head stuck warning | After 60s stuck | Blocking offset and connection |
| Main thread batch >5s | On occurrence | Batch duration, remaining poll interval budget |
| Empty poll throttled | Every 30s while empty | Consumer alive but no data |
| Startup config summary | Once | All key config values in one line |

---

## Implementation Order

| Phase | Items | Files |
|-------|-------|-------|
| 1 | Kafka heartbeat + OffsetLifecycleTracker enhancements + empty poll throttled logging | `TrackingKafkaConsumer`, `OffsetLifecycleTracker` |
| 2 | Accumulator heartbeat + new gauges | `CapturedTrafficToHttpTransactionAccumulator`, `ReplayContexts` |
| 3 | Replay/Target heartbeat + scheduling lag | `ReplayEngine`, `ClientConnectionPool` |
| 4 | Backpressure heartbeat | `BlockingTrafficSource` |
| 5 | ActiveContextMonitor compact summary | `ActiveContextMonitor`, `TrafficReplayer` |
| 6 | Progress log dual-write + demote noisy INFO logs | `log4j2.properties` |
| 7 | Main thread batch timing | `TrafficReplayerCore` |
| 8 | Startup config summary | `TrafficReplayer` |

---

## WARN/ERROR Log Correlation Audit

Every WARN/ERROR log must include enough context to correlate back to the periodic heartbeat logs. The minimum correlatable identifier is **connection ID + Kafka offset** (or partition + generation for Kafka-level events).

### ✅ Already Correlatable

| File:Line | Level | What it logs | Identifier |
|-----------|-------|-------------|------------|
| `TrackingKafkaConsumer:429` | WARN | Stale generation commit drop | Full traffic stream key + offset + generation |
| `TrackingKafkaConsumer:171` | WARN | Partitions lost | Partition list |
| `TrackingKafkaConsumer:202` | WARN | Partitions revoked | Partition list + `this` (includes generation, inflight) |
| `Accumulator:244` | ERROR | Stale accumulation (gen mismatch) | partitionId + connectionId + stored/incoming gen |
| `Accumulator:327` | WARN | Unaccounted observation | observation + `accum.trafficChannelKey` |
| `Accumulator:289` | WARN | Inconsistent TrafficStream values | Full stream object |
| `Accumulator:593` | WARN | Terminating reconstruction before data accumulated | `accumulation.trafficChannelKey` |
| `ParsedHttpMessagesAsDicts:160` | WARN | Bogus value (NPE in JSON transform) | `context` (= traffic stream key + offset) |
| `TrafficReplayerCore:203` | ERROR | Throwable in handle() | `context` (= UniqueReplayerRequestKey) |
| `TrafficReplayerCore:213` | ERROR | Unexpected exception in callback | `context` (= UniqueReplayerRequestKey) |
| `TrafficReplayerCore:324` | ERROR | Exception in CompletableFuture callback | `tupleHandlingContext` |
| `ClientConnectionPool:195` | WARN | Work remaining on closing session | `session.getChannelKeyContext()` + work count |
| `NettyPacketToHttpConsumer:381` | WARN | Channel not set up, not writing | `httpContext().getReplayerRequestKey()` |
| `TrackingKafkaConsumer:510` | WARN | Error while committing | Pending commit map contents |
| `BehavioralPolicy:31` | ERROR | Timestamp before expiration window | `trafficStreamKey` (via `formatPartitionAndConnectionIds`) |
| `BehavioralPolicy:55` | ERROR | Data arrived outside expiration window | `trafficStreamKey` + timestamps |
| `BehavioralPolicy:82` | ERROR | Race condition in timestamp update | `trafficStreamKey` + timestamp |
| `BehavioralPolicy:102` | ERROR | Data arriving after accumulation removed | `trafficStreamKey` |
| `KafkaTrafficCaptureSource:384` | ERROR | Terminating Kafka stream due to exception | (exception only, but fatal — acceptable) |

### ❌ Missing Correlatable Identifiers — Need Fix

| File:Line | Level | What it logs | Missing | Fix |
|-----------|-------|-------------|---------|-----|
| `NettyPacketToHttpConsumer:127` | WARN | Error creating channel, not retrying | No connection ID, no offset | Add `channelCtx` to message (it's in scope as `channelCtx`) |
| `NettyPacketToHttpConsumer:142` | WARN | Channel wasn't active, retrying | No connection ID, no offset | Add `channelCtx` to message |
| `NettyPacketToHttpConsumer:216` | WARN | Exception getting active channel | No connection ID, no offset | Add `httpContext()` to message |
| `NettyPacketToHttpConsumer:344` | WARN | Ignoring missing handler exception | No connection ID — just handler name | Add `httpContext()` or channel key. Low priority (benign cleanup) |
| `HttpJsonTransformingConsumer:127` | WARN | IncompleteJsonBodyException | No connection ID, no offset | Add `transformationContext` to message (it's `this.transformationContext`) |
| `NettyJsonContentAuthSigner:58` | WARN | Failed to sign — handler removed early | No connection ID, no offset | Thread the `IReplayContexts.IRequestTransformationContext` through or add channel info from pipeline |
| `NettyJsonContentAuthSigner:68` | WARN | Failed to sign — channel unregistered | No connection ID, no offset | Same as above |
| `NettyJsonToByteBufHandler:134` | WARN | Writing headers directly failed | Has `httpJson` but no connection/offset | Add request context from pipeline |
| `TrafficStreamLimiter:64` | ERROR | consumeFromQueue interrupted | No connection — has queue size only | Add `workItem.context` if non-null (it's already in scope as `finalWorkItem`) |
| `BlockingTrafficSource:185` | WARN | Interrupted while waiting to read | No context at all | Add `stopReadingAtRef`, `lastTimestampSecondsRef` — these are the backpressure state |
| `TrafficReplayerCore:208` | ERROR | Caught Error, initiating shutdown | No specific connection | Add the error context — which request/connection triggered it (from the enclosing `context` variable) |
| `TrafficReplayerTopLevel:183` | WARN | Terminating runReplay due to exception | No context | Add accumulator stats (`getStatsString()`) and inflight count |
| `TrafficReplayerTopLevel:341` | ERROR | Not waiting for work, shutting down | No context | Add `requestWorkTracker.size()` and oldest entry |
| `TrackingKafkaConsumer:295` | WARN | Unable to poll topic (in touch) | Has topic name only | Add `generation`, `inflight`, `commitHead` |
| `TrackingKafkaConsumer:414` | WARN | Unable to poll topic (in safePoll) | Has topic name only | Add `generation`, `inflight`, `commitHead` |
| `KafkaTrafficCaptureSource:264` | ERROR | Unable to load kafka properties file | N/A (startup config error) | Acceptable as-is |

### Summary

- **19 log sites** are already correlatable (have connection ID, offset, or traffic stream key)
- **15 log sites** are missing correlatable identifiers and need fixes
- Most fixes are trivial — the context object is already in scope, just not included in the log message
- The Netty pipeline handlers (`NettyPacketToHttpConsumer`, `NettyJsonContentAuthSigner`) are the hardest — they don't always have direct access to the replay context. May need to thread it through the pipeline or attach it as a channel attribute.

---

## Additional Items

### Startup Config Summary

Log all key parameters in a single INFO line at startup so post-mortem analysis doesn't require guessing config values:

```
[INFO] ReplayerConfig - lookahead=300s speedup=1.0 maxConcurrent=1024 
  pollInterval=60s sessionTimeout=45s heartbeatInterval=3s 
  observedPacketConnectionTimeout=70s targetUri=https://target:9200 
  kafkaTopic=logging-traffic-topic groupId=logging-group-default 
  mskAuth=true transformerConfig=null
```

**File:** `TrafficReplayer.java` — after parsing params, before entering the main loop.

### Graceful Shutdown Summary

When the replayer shuts down (SIGTERM, exception, or EOF), log a final snapshot of incomplete state:

```
[INFO] ShutdownSummary - inflight=74434 commitHead=15774 commitTail=90202 
  uncommittedRange=74428 activeConnections=580 tasksOutstanding=774 
  requestWorkTrackerSize=12 generation=13 shutdownCause=coordinatorDisconnect
```

This tells you whether the shutdown was clean or left a mess. Currently shutdown logs are generic ("Shutting down", "Not waiting for work") with no state.

**File:** `TrafficReplayerTopLevel.java` — in the `finally` block of `setupRunAndWaitForReplayWithShutdownChecks`, and in the shutdown hook.

### Kafka Consumer Lag

Add the consumer lag (high watermark minus current position) to the Kafka heartbeat. This immediately distinguishes "topic exhausted" (lag=0) from "consumer blocked" (lag>0):

```
[INFO] KafkaHeartbeat - ... lag=0 highWatermark=90204 currentPosition=90204
```

The Kafka consumer API provides `endOffsets()` for the high watermark. Call it once per heartbeat (every 30s), not per poll.

**File:** `TrackingKafkaConsumer.java` — in the heartbeat method. Note: `endOffsets()` is a network call to the broker, so it should be called on the `kafkaExecutor` thread and only at heartbeat cadence.

### Target Response Latency in Replay Heartbeat

Track min/max/count of target response times since last heartbeat. This flags when the target is getting slow (which causes the 429 cascade):

```
[INFO] ReplayHeartbeat - ... targetLatency={min=12ms, max=1847ms, p50=45ms, count=257}
```

A full percentile tracker is overkill — min/max/count (and optionally a simple running average) is sufficient. Reset each heartbeat.

**File:** `ReplayEngine.java` or `RequestSenderOrchestrator.java` — instrument the time between request send and response received.

### Log Expiry Configuration Values

The `observedPacketConnectionTimeout` and `minimumGuaranteedLifetime` are the expiry timeouts that should have caught connection 00000f88. Log them at startup (in the config summary) and reference them in the accumulator heartbeat so you can immediately tell whether expiry is configured correctly:

```
[INFO] AccumulatorHeartbeat - ... expiryConfig={connectionTimeout=70s, guaranteedLifetime=70s}
  oldestInWrites={conn=00000f88, age=49m, lastPacketAge=48m, SHOULD_HAVE_EXPIRED}
```

The `SHOULD_HAVE_EXPIRED` flag is computed by comparing `lastPacketAge > minimumGuaranteedLifetime`. This is the direct diagnostic for "why didn't expiry fire?"

**File:** `CapturedTrafficToHttpTransactionAccumulator.java` — in the heartbeat method. The config values are already available as constructor parameters.

Every stateful component that can silently stall, leak, or block progress. Each must participate in heartbeat monitoring.

### Tier 1 — Commit Pipeline (stall here = zero progress)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **OffsetLifecycleTracker** | Min-heap of consumed offsets per partition | `headOffset`, `headConnectionId`, `headAge`, `tailOffset`, `queueSize` | Head age > 60s |
| **TrackingKafkaConsumer** | Generation, commit maps, ready-to-commit flag | `generation`, `inflight`, `readyToCommit`, `pollCount`, `emptyPollCount`, `commitCount` (all since last HB) | `commitCount=0` for >60s while `inflight>0` |
| **nextSetOfCommitsMap** | Pending commits waiting for next `safeCommit()` | `pendingCommitPartitions`, `pendingCommitOffsets` | Non-empty for >30s (should flush every poll cycle) |

### Tier 2 — Accumulation Pipeline (stall here = commit pipeline starved)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **ExpiringTrafficStreamMap** | Per-connection Accumulation objects with time-bucketed expiry | `totalEntries`, `oldestEntryAge`, `oldestEntryConnectionId`, `expiryConfig` (minimumGuaranteedLifetime), `nodesTracked`, `bucketsPerNode` | Oldest entry age > `minimumGuaranteedLifetime` × 2 |
| **Accumulation state machines** | Per-connection: state, last TSK, newestPacketTimestamp | `byState={WAITING, READS, WRITES, IGNORING}`, `oldestInWrites={conn, age, lastTskOffset, lastPacketAge}` | Connection in WRITES with `lastPacketAge` > `minimumGuaranteedLifetime` (should have expired) |
| **CapturedTrafficToHttpTransactionAccumulator** | Connection lifecycle counters | `created`, `closed`, `expired`, `exceptions` (cumulative) | `expired` increasing while `closed` stagnant |

### Tier 3 — Replay Pipeline (stall here = target overwhelmed or scheduling broken)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **TrafficStreamLimiter** | Semaphore (maxConcurrentCost), work queue | `availablePermits`, `queueDepth`, `maxPermits` | `availablePermits=0` for >30s (all slots consumed) |
| **ReplayEngine** | tasksOutstanding, time shifter, idle updater | `tasksOutstanding`, `schedulingLag` (wall minus source time), `lastCompletedSourceTime`, `lastIdleUpdate` | `schedulingLag` > `bufferTimeWindow` (pacing broken) |
| **TimeShifter** | Source-to-real time mapping | `firstSourceTimestamp`, `wallClockAtFirstTimestamp`, `speedupFactor`, `currentSourceTime` | N/A (config, not stall-prone) |
| **requestWorkTracker** (OrderedWorkerTracker) | In-flight request futures keyed by time | `size`, `oldestAge`, `oldestKey` | `oldestAge` > 5 minutes |

### Tier 4 — Target Connection Pool (stall here = connection leak or target down)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **ClientConnectionPool** | Channel cache, Netty event loop | `cachedSessions`, `activeSocketConnections` (existing gauge), `activeChannels` (existing gauge) | `cachedSessions` growing without bound |
| **ConnectionReplaySession** | Per-connection: OnlineRadixSorter, channel, generation | (reported via pool aggregates) | N/A individually |
| **OnlineRadixSorter** | Per-connection request ordering slots | (reported via requestWorkTracker) | N/A individually |

### Tier 5 — Backpressure & Source (stall here = reads blocked or consumer kicked)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **BlockingTrafficSource** | Time gate semaphore, stopReadingAt, lastTimestamp | `engaged`, `stopReadingAt`, `lastTimestamp`, `headroom`, `readGatePermits` | `engaged=true` for >5 minutes (replay not advancing) |
| **KafkaTrafficCaptureSource** | kafkaExecutor, partitionToActiveConnections | `activeConnectionsByPartition` | N/A (derived from accumulator) |

### Tier 6 — Observability (meta — monitors the monitors)

| System | Key State | Heartbeat Values | Stall Signal |
|--------|-----------|-----------------|--------------|
| **ActiveContextTracker** | Global ordered set of all open scopes | `totalActiveScopes` | Monotonically growing (scopes not closing) |
| **ActiveContextTrackerByActivityType** | Per-type ordered sets | `countByType`, `oldestByType` | See below |

---

## ActiveContextMonitor: Breach-Based Reporting

### Per-type expected lifetimes

| Activity Type | Expected Max | Category |
|---------------|-------------|----------|
| `channel` | unbounded | long-lived — report as gauge count only |
| `tcpConnection` | unbounded | long-lived — report as gauge count only |
| `recordLifetime` | 5 min | medium — flag if breached |
| `trafficStreamLifetime` | 5 min | medium — flag if breached |
| `httpTransaction` | 2 min | medium — flag if breached |
| `accumulatingRequest` | 30s | ephemeral — flag if breached |
| `accumulatingResponse` | 60s | ephemeral — flag if breached |
| `transformation` | 10s | ephemeral — flag if breached |
| `scheduled` | depends on scheduling lag | ephemeral |
| `targetTransaction` | 30s | ephemeral — flag if breached |
| `requestConnecting` | 10s | ephemeral — flag if breached |
| `requestSending` | 10s | ephemeral — flag if breached |
| `waitingForResponse` | 30s | ephemeral — flag if breached |
| `receivingResponse` | 30s | ephemeral — flag if breached |
| `tupleComparison` | 5s | ephemeral — flag if breached |
| `touch` | 10s | ephemeral — flag if breached |
| `kafkaPoll` | 10s | ephemeral — flag if breached |
| `commit` | 10s | ephemeral — flag if breached |
| `kafkaCommit` | 10s | ephemeral — flag if breached |
| `readNextTrafficChunk` | 10s | ephemeral — flag if breached |
| `backPressureBlock` | unbounded | long-lived when engaged — report as gauge |
| `waitForNextBackPressureCheck` | 30s | ephemeral |

(These thresholds belong in a `DIAGNOSTICS.md` reference doc, not in the log output.)

### Compact summary format (main log, every 30s)

Normal case — one line:
```
[INFO] ActiveContextSummary - scopes=42 longLived={channel=580, tcpConn=32} 
  ephemeral={httpTx=12, respAccum=10, targetReq=6, ...} breached=0
```

Breached case — one line per breached item with parent chain walk-up:
```
[WARN] ActiveContextSummary - scopes=42 longLived={channel=580, tcpConn=32} 
  ephemeral={httpTx=12, respAccum=10} breached=2:
    respAccum age=49m conn=00000f88 lastTsk=.6|offset=15970 lastPacketAge=48m → httpTx conn=00000f88 offset=15774 → channel gen=13
    httpTx age=48m conn=000010b7 offset=15778 → channel gen=13
```

Key design choices:
- **Long-lived types** (`channel`, `tcpConnection`, `backPressureBlock`) → gauge counts only, never flagged as breached
- **Breached items** get a parent chain walk-up to the nearest long-lived ancestor (1-3 hops, not full depth)
- **`lastTsk` and `lastPacketAge`** on the breached `responseAccum` tell you whether data stopped flowing (expiry should have fired) or data is still flowing (accumulator hasn't seen end-of-response)
- **No expected-max in the log** — that's reference material for `DIAGNOSTICS.md`
- The detailed tree dump continues to go to `longRunningActivity.log` unchanged
