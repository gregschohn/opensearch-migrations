package org.opensearch.migrations.replay.traffic.source;

import java.io.EOFException;
import java.util.List;
import java.util.Optional;
import java.util.PriorityQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Supplier;

import org.opensearch.migrations.replay.datatypes.ITrafficStreamKey;
import org.opensearch.migrations.replay.datatypes.PojoTrafficStreamAndKey;
import org.opensearch.migrations.replay.tracing.ITrafficSourceContexts;
import org.opensearch.migrations.tracing.TestContext;

import lombok.extern.slf4j.Slf4j;

@Slf4j
public class ArrayCursorTrafficCaptureSource implements ISimpleTrafficCaptureSource {
    final AtomicInteger readCursor;
    final PriorityQueue<TrafficStreamCursorKey> pQueue = new PriorityQueue<>();
    Integer cursorHighWatermark;
    ArrayCursorTrafficSourceContext arrayCursorTrafficSourceContext;
    TestContext rootContext;

    public ArrayCursorTrafficCaptureSource(
        TestContext rootContext,
        ArrayCursorTrafficSourceContext arrayCursorTrafficSourceContext
    ) {
        var startingCursor = arrayCursorTrafficSourceContext.nextReadCursor.get();
        log.atDebug().setMessage(()->"startingCursor = " + startingCursor).log();
        this.readCursor = new AtomicInteger(startingCursor);
        this.arrayCursorTrafficSourceContext = arrayCursorTrafficSourceContext;
        cursorHighWatermark = startingCursor;
        this.rootContext = rootContext;
    }

    @Override
    public CompletableFuture<List<ITrafficStreamWithKey>> readNextTrafficStreamChunk(
        Supplier<ITrafficSourceContexts.IReadChunkContext> contextSupplier
    ) {
        var idx = readCursor.getAndIncrement();
        log.atDebug().setMessage(()->"reading chunk from index=" + idx).log();
        if (arrayCursorTrafficSourceContext.trafficStreamsList.size() <= idx) {
            return CompletableFuture.failedFuture(new EOFException());
        }
        var stream = arrayCursorTrafficSourceContext.trafficStreamsList.get(idx);
        var key = new TrafficStreamCursorKey(rootContext, stream, idx);
        synchronized (pQueue) {
            pQueue.add(key);
            cursorHighWatermark = idx;
        }
        return CompletableFuture.supplyAsync(() -> List.of(new PojoTrafficStreamAndKey(stream, key)));
    }

    @Override
    public CommitResult commitTrafficStream(ITrafficStreamKey trafficStreamKey) {
        synchronized (pQueue) { // figure out if I need to do something more efficient later
            log.atDebug()
                .setMessage(()->"Commit called for " + trafficStreamKey + " with pQueue.size=" + pQueue.size()).log();
            var incomingCursor = ((TrafficStreamCursorKey) trafficStreamKey).arrayIndex;
            int topCursor = pQueue.peek().arrayIndex;
            var didRemove = pQueue.remove(trafficStreamKey);
            if (!didRemove) {
                log.error("no item " + incomingCursor + " to remove from " + pQueue);
            }
            assert didRemove;
            if (topCursor == incomingCursor) {
                topCursor = Optional.ofNullable(pQueue.peek())
                    .map(k -> k.getArrayIndex())
                    .orElse(cursorHighWatermark + 1); // most recent cursor was previously popped
                log.atDebug().setMessage("Commit called for {}, and new topCursor={}")
                    .addArgument(trafficStreamKey)
                    .addArgument(topCursor)
                    .log();
                arrayCursorTrafficSourceContext.nextReadCursor.set(topCursor);
            } else {
                log.atDebug().setMessage("Commit called for {}, but new topCursor={}")
                    .addArgument(trafficStreamKey)
                    .addArgument(topCursor)
                    .log();
            }
        }
        rootContext.channelContextManager.releaseContextFor(
            ((TrafficStreamCursorKey) trafficStreamKey).trafficStreamsContext.getChannelKeyContext()
        );
        return CommitResult.IMMEDIATE;
    }
}
