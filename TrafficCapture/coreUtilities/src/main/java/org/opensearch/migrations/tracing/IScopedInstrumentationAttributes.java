package org.opensearch.migrations.tracing;

import io.opentelemetry.api.metrics.LongCounter;
import io.opentelemetry.api.metrics.DoubleHistogram;
import io.opentelemetry.api.trace.Span;
import lombok.NonNull;

public interface IScopedInstrumentationAttributes<S extends IInstrumentConstructor<S>>
        extends IWithStartTimeAndAttributes<S>, AutoCloseable {

    String getActivityName();

    @Override
    @NonNull Span getCurrentSpan();
    CommonScopedMetricInstruments getMetrics();
    default LongCounter getEndOfScopeCountMetric() {
        return getMetrics().contextCounter;
    }
    default DoubleHistogram getEndOfScopeDurationMetric() {
        return getMetrics().contextDuration;
    }
    default void endSpan() {
        getCurrentSpan().end();
    }

    default void sendMeterEventsForEnd() {
        meterIncrementEvent(getEndOfScopeCountMetric());
        meterHistogramMicros(getEndOfScopeDurationMetric());
    }

    default void close() {
        endSpan();
        sendMeterEventsForEnd();
    }

    default void addException(Exception e) {
        getCurrentSpan().recordException(e);
    }
}
