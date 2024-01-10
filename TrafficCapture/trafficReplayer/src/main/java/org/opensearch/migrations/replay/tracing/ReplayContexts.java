package org.opensearch.migrations.replay.tracing;

import io.opentelemetry.api.metrics.LongCounter;
import io.opentelemetry.api.metrics.DoubleHistogram;
import lombok.Getter;
import org.opensearch.migrations.replay.datatypes.ISourceTrafficChannelKey;
import org.opensearch.migrations.replay.datatypes.ITrafficStreamKey;
import org.opensearch.migrations.replay.datatypes.UniqueReplayerRequestKey;
import org.opensearch.migrations.tracing.AbstractNestedSpanContext;
import org.opensearch.migrations.tracing.DirectNestedSpanContext;
import org.opensearch.migrations.tracing.IInstrumentationAttributes;
import org.opensearch.migrations.tracing.IndirectNestedSpanContext;

import java.time.Duration;
import java.time.Instant;

public class ReplayContexts {

    private ReplayContexts() {}

    public static class ChannelKeyContext
            extends AbstractNestedSpanContext<IRootReplayerContext, IInstrumentationAttributes<IRootReplayerContext>>
            implements IReplayContexts.IChannelKeyContext {

        @Getter
        final ISourceTrafficChannelKey channelKey;
        
        @Override
        public DoubleHistogram getEndOfScopeDurationMetric() {
            return getRootInstrumentationScope().getChannelDuration();
        }

        @Override
        public LongCounter getEndOfScopeCountMetric() {
            return getRootInstrumentationScope().getChannelCounter();
        }

        public ChannelKeyContext(IInstrumentationAttributes<IRootReplayerContext> enclosingScope, ISourceTrafficChannelKey channelKey) {
            super(enclosingScope);
            this.channelKey = channelKey;
            initializeSpan();
        }

        @Override
        public String toString() {
            return channelKey.toString();
        }

        @Override
        public void onTargetConnectionCreated() {
            meterDeltaEvent(getRootInstrumentationScope().getActiveChannelsCounter(), 1);
        }
        @Override
        public void onTargetConnectionClosed() {
            meterDeltaEvent(getRootInstrumentationScope().getActiveChannelsCounter(), -1);
        }
    }

    public static class KafkaRecordContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IChannelKeyContext>
            implements IReplayContexts.IKafkaRecordContext {

        final String recordId;

        public KafkaRecordContext(IReplayContexts.IChannelKeyContext enclosingScope, String recordId,
                                  int recordSize) {
            super(enclosingScope);
            this.recordId = recordId;
            initializeSpan();
            this.meterIncrementEvent(getRootInstrumentationScope().getKafkaRecordCounter());
            this.meterIncrementEvent(getRootInstrumentationScope().getKafkaRecordBytesCounter(), recordSize);
        }

        @Override
        public String getRecordId() {
            return recordId;
        }

        @Override
        public DoubleHistogram getEndOfScopeDurationMetric() {
            return getRootInstrumentationScope().getKafkaRecordDuration();
        }

        @Override
        public LongCounter getEndOfScopeCountMetric() {
            return getRootInstrumentationScope().getKafkaRecordCounter();
        }
    }

    public static class TrafficStreamsLifecycleContext
            extends IndirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IKafkaRecordContext, IReplayContexts.IChannelKeyContext>
            implements IReplayContexts.ITrafficStreamsLifecycleContext {
        private final ITrafficStreamKey trafficStreamKey;

        public TrafficStreamsLifecycleContext(IReplayContexts.IKafkaRecordContext enclosingScope,
                                              ITrafficStreamKey trafficStreamKey) {
            super(enclosingScope);
            this.trafficStreamKey = trafficStreamKey;
            initializeSpan();
            this.meterIncrementEvent(IReplayContexts.MetricNames.TRAFFIC_STREAMS_READ);
        }

        @Override
        public IReplayContexts.IChannelKeyContext getChannelKeyContext() {
            return getLogicalEnclosingScope();
        }

        @Override
        public ITrafficStreamKey getTrafficStreamKey() {
            return trafficStreamKey;
        }

        @Override
        public IReplayContexts.IChannelKeyContext getLogicalEnclosingScope() {
            return getImmediateEnclosingScope().getLogicalEnclosingScope();
        }

        @Override
        public DoubleHistogram getEndOfScopeDurationMetric() {
            return getRootInstrumentationScope().getTrafficStreamLifecycleDuration();
        }

        @Override
        public LongCounter getEndOfScopeCountMetric() {
            return getRootInstrumentationScope().getTrafficStreamLifecycleCounter();
        }
    }

    public static class HttpTransactionContext
            extends IndirectNestedSpanContext<IRootReplayerContext, IReplayContexts.ITrafficStreamsLifecycleContext, IReplayContexts.IChannelKeyContext>
            implements IReplayContexts.IReplayerHttpTransactionContext {
        final UniqueReplayerRequestKey replayerRequestKey;
        @Getter final Instant timeOfOriginalRequest;

        public HttpTransactionContext(IReplayContexts.ITrafficStreamsLifecycleContext enclosingScope,
                                      UniqueReplayerRequestKey replayerRequestKey,
                                      Instant timeOfOriginalRequest) {
            super(enclosingScope);
            this.replayerRequestKey = replayerRequestKey;
            this.timeOfOriginalRequest = timeOfOriginalRequest;
            initializeSpan();
        }

        public IReplayContexts.IChannelKeyContext getChannelKeyContext() {
            return getLogicalEnclosingScope();
        }

        @Override
        public UniqueReplayerRequestKey getReplayerRequestKey() {
            return replayerRequestKey;
        }

        @Override
        public String toString() {
            return replayerRequestKey.toString();
        }

        @Override
        public IReplayContexts.IChannelKeyContext getLogicalEnclosingScope() {
            return getImmediateEnclosingScope().getLogicalEnclosingScope();
        }
    }

    public static class RequestAccumulationContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.IRequestAccumulationContext {
        public RequestAccumulationContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }

    public static class ResponseAccumulationContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.IResponseAccumulationContext {
        public ResponseAccumulationContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }

    public static class RequestTransformationContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.IRequestTransformationContext {
        public RequestTransformationContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }

        @Override
        public void onHeaderParse() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_HEADER_PARSE);
        }
        @Override
        public void onPayloadParse() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_PAYLOAD_PARSE_REQUIRED);
        }
        @Override
        public void onPayloadParseSuccess() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_PAYLOAD_PARSE_SUCCESS);
        }
        @Override
        public void onJsonPayloadParseRequired() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_JSON_REQUIRED);
        }
        @Override
        public void onJsonPayloadParseSucceeded() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_JSON_SUCCEEDED);
        }
        @Override
        public void onPayloadBytesIn(int inputSize) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_PAYLOAD_BYTES_IN, inputSize);
        }
        @Override
        public void onUncompressedBytesIn(int inputSize) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_UNCOMPRESSED_BYTES_IN, inputSize);
        }
        @Override
        public void onUncompressedBytesOut(int inputSize) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_UNCOMPRESSED_BYTES_OUT, inputSize);
        }
        @Override
        public void onFinalBytesOut(int inputSize) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_FINAL_PAYLOAD_BYTES_OUT, inputSize);
        }
        @Override
        public void onTransformSuccess() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_SUCCESS);
        }
        @Override
        public void onTransformSkip() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_SKIPPED);
        }
        @Override
        public void onTransformFailure() {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_ERROR);
        }
        @Override
        public void aggregateInputChunk(int sizeInBytes) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_BYTES_IN, sizeInBytes);
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_CHUNKS_IN);
        }
        @Override
        public void aggregateOutputChunk(int sizeInBytes) {
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_BYTES_OUT, sizeInBytes);
            meterIncrementEvent(IReplayContexts.MetricNames.TRANSFORM_CHUNKS_OUT);
        }
    }

    public static class ScheduledContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.IScheduledContext {
        private final Instant scheduledFor;

        public ScheduledContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope,
                                Instant scheduledFor) {
            super(enclosingScope);
            this.scheduledFor = scheduledFor;
            initializeSpan();
        }

        @Override
        public void sendMeterEventsForEnd() {
            super.sendMeterEventsForEnd();
            meterHistogramMillis(IReplayContexts.MetricNames.NETTY_SCHEDULE_LAG,
                    Duration.between(scheduledFor, Instant.now()));

        }
    }

    public static class TargetRequestContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.ITargetRequestContext {
        public TargetRequestContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
            meterHistogramMillis(IReplayContexts.MetricNames.SOURCE_TO_TARGET_REQUEST_LAG,
                    Duration.between(enclosingScope.getTimeOfOriginalRequest(), Instant.now()));
        }
        @Override
        public void onBytesSent(int size) {
            meterIncrementEvent(IReplayContexts.MetricNames.BYTES_WRITTEN_TO_TARGET, size);
        }

        @Override
        public void onBytesReceived(int size) {
            meterIncrementEvent(IReplayContexts.MetricNames.BYTES_READ_FROM_TARGET, size);
        }
    }

    public static class RequestSendingContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.ITargetRequestContext>
            implements IReplayContexts.IRequestSendingContext {
        public RequestSendingContext(IReplayContexts.ITargetRequestContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }

    public static class WaitingForHttpResponseContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.ITargetRequestContext>
            implements IReplayContexts.IWaitingForHttpResponseContext {
        public WaitingForHttpResponseContext(IReplayContexts.ITargetRequestContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }

    public static class ReceivingHttpResponseContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.ITargetRequestContext>
            implements IReplayContexts.IReceivingHttpResponseContext {
        public ReceivingHttpResponseContext(IReplayContexts.ITargetRequestContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }

    public static class TupleHandlingContext
            extends DirectNestedSpanContext<IRootReplayerContext, IReplayContexts.IReplayerHttpTransactionContext>
            implements IReplayContexts.ITupleHandlingContext {
        public TupleHandlingContext(IReplayContexts.IReplayerHttpTransactionContext enclosingScope) {
            super(enclosingScope);
            initializeSpan();
        }
    }
}
