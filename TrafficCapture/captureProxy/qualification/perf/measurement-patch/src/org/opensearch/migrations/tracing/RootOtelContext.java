package org.opensearch.migrations.tracing;

import java.net.URI;
import java.net.URISyntaxException;
import java.time.Duration;
import java.util.Optional;
import java.util.concurrent.TimeUnit;
import java.util.function.Function;
import java.util.stream.Stream;

import org.opensearch.migrations.Utils;

import io.opentelemetry.api.OpenTelemetry;
import io.opentelemetry.api.metrics.MeterProvider;
import io.opentelemetry.api.trace.Span;
import io.opentelemetry.api.trace.SpanBuilder;
import io.opentelemetry.context.Context;
import io.opentelemetry.exporter.otlp.metrics.OtlpGrpcMetricExporter;
import io.opentelemetry.exporter.otlp.trace.OtlpGrpcSpanExporter;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.metrics.Aggregation;
import io.opentelemetry.sdk.metrics.InstrumentSelector;
import io.opentelemetry.sdk.metrics.SdkMeterProvider;
import io.opentelemetry.sdk.metrics.SdkMeterProviderBuilder;
import io.opentelemetry.sdk.metrics.View;
import io.opentelemetry.sdk.metrics.export.MetricReader;
import io.opentelemetry.sdk.metrics.export.PeriodicMetricReader;
import io.opentelemetry.sdk.resources.Resource;
import io.opentelemetry.sdk.trace.SdkTracerProvider;
import io.opentelemetry.sdk.trace.export.BatchSpanProcessor;
import io.opentelemetry.semconv.ServiceAttributes;
import lombok.Getter;
import lombok.NonNull;
import lombok.extern.slf4j.Slf4j;

@Slf4j
public class RootOtelContext implements IRootOtelContext {
    private final OpenTelemetry openTelemetryImpl;
    private final String scopeName;
    @Getter
    private final IContextTracker contextTracker;

    /**
     * MEASUREMENT-ONLY. Histogram bucket boundaries in MILLISECONDS, byte-for-byte the same list
     * the Rust implementation exposes as {@code contract::GATE_LATENCY_BUCKETS_MS}, so that
     * latency percentiles are directly comparable between the two implementations.
     *
     * 17 bounds -- deliberately the same count as the OpenTelemetry Java default -- so applying
     * this adds no new Prometheus series, it only moves where the boundaries sit. Six of them are
     * at or below 5 ms, which is what continuation gate 5 needs: the measured Java acknowledgement
     * latency mean is 1.249 ms and the default set puts 99.045% of observations inside a single
     * (0, 5] ms bucket, making p50 and p99 unresolvable.
     */
    public static final java.util.List<Double> GATE_LATENCY_BUCKETS_MS = java.util.List.of(
        0.1d, 0.25d, 0.5d, 1.0d, 2.5d, 5.0d, 10.0d, 25.0d, 50.0d,
        100.0d, 250.0d, 500.0d, 1000.0d, 2500.0d, 5000.0d, 10000.0d, 30000.0d);

    /**
     * MEASUREMENT-ONLY. Exactly the instruments a continuation gate is stated on.
     *
     * Deliberately EXCLUDED: captureConnectionDuration and waitingForResponseDuration. Connection
     * lifetime and source response time are not proxy-added latency, no gate is stated on them,
     * and both legitimately run to minutes, so they keep the long default boundary set. A
     * difference there would be noise in the comparison rather than signal.
     */
    public static final java.util.List<String> GATE_LATENCY_INSTRUMENTS = java.util.List.of(
        "kafkaCommitDuration",        // gate 5: Kafka acknowledgement p99
        "gatheringRequestDuration",   // gate 3: proxy-added p99
        "blockedDuration",            // gate 3: the capture gate's share of proxy-added p99
        "gatheringResponseDuration"); // gate 3: proxy-added p99

    /** MEASUREMENT-ONLY. Registers one explicit-bucket view per gate-bearing instrument. */
    static SdkMeterProviderBuilder registerGateLatencyViews(SdkMeterProviderBuilder b) {
        for (String instrument : GATE_LATENCY_INSTRUMENTS) {
            b = b.registerView(
                InstrumentSelector.builder().setName(instrument).build(),
                View.builder()
                    .setAggregation(Aggregation.explicitBucketHistogram(GATE_LATENCY_BUCKETS_MS))
                    .build());
        }
        return b;
    }

    public static OpenTelemetry initializeOpenTelemetryForCollectors(
        @NonNull OtelCollectorEndpoints collectorEndpoints,
        @NonNull String serviceName,
        @NonNull String instanceName
    ) {
        var openTelemetrySdk = buildOpenTelemetryForCollectors(
            collectorEndpoints,
            serviceName,
            instanceName,
            endpoint -> PeriodicMetricReader.builder(
                OtlpGrpcMetricExporter.builder()
                    .setEndpoint(endpoint)
                    // Keep cumulative temporality; the collector converts only the AWSEMF branch to delta.
                    .build()
            ).setInterval(Duration.ofMillis(1000)).build()
        );

        // Add hook to close SDK, which flushes logs
        Runtime.getRuntime().addShutdownHook(new Thread(openTelemetrySdk::close));
        return openTelemetrySdk;
    }

    static OpenTelemetrySdk buildOpenTelemetryForCollectors(
        @NonNull OtelCollectorEndpoints collectorEndpoints,
        @NonNull String serviceName,
        @NonNull String instanceName,
        @NonNull Function<String, MetricReader> metricReaderFactory
    ) {
        var openTelemetryBuilder = OpenTelemetrySdk.builder();
        var serviceResource = buildServiceResource(serviceName, instanceName);

        var normalizedTraceEndpoint = Optional.ofNullable(collectorEndpoints.getTraceEndpoint())
            .map(endpoint -> normalizeOtlpEndpoint(endpoint, "trace"));
        if (normalizedTraceEndpoint.isPresent()) {
            final var spanProcessor = BatchSpanProcessor.builder(
                OtlpGrpcSpanExporter.builder()
                    .setEndpoint(normalizedTraceEndpoint.get())
                    .setTimeout(2, TimeUnit.SECONDS)
                    .build()
            ).build();

            openTelemetryBuilder = openTelemetryBuilder.setTracerProvider(
                SdkTracerProvider.builder()
                    .setResource(serviceResource)
                    .addSpanProcessor(spanProcessor)
                    .build()
            );
        }

        var normalizedMetricsEndpoint = Optional.ofNullable(collectorEndpoints.getMetricsEndpoint())
            .map(endpoint -> normalizeOtlpEndpoint(endpoint, "metrics"));
        if (normalizedMetricsEndpoint.isPresent()) {
            final var metricReader = metricReaderFactory.apply(normalizedMetricsEndpoint.get());

            // MEASUREMENT-ONLY PATCH. Re-buckets only the four gate-bearing latency
            // instruments. Changes nothing about proxy behaviour, recorded values, or any
            // wire format -- only how measurements are bucketed for export.
            openTelemetryBuilder = openTelemetryBuilder.setMeterProvider(
                registerGateLatencyViews(
                    SdkMeterProvider.builder().setResource(serviceResource))
                    .registerMetricReader(metricReader).build()
            );
        }

        return openTelemetryBuilder.build();
    }

    static Resource buildServiceResource(@NonNull String serviceName, @NonNull String instanceName) {
        return Resource.getDefault()
            .toBuilder()
            .put(ServiceAttributes.SERVICE_NAME, serviceName)
            .put(ServiceAttributes.SERVICE_INSTANCE_ID, instanceName)
            .build();
    }

    public static OpenTelemetry initializeNoopOpenTelemetry() {
        return OpenTelemetrySdk.builder().build();
    }

    static String normalizeOtlpEndpoint(@NonNull String endpoint, @NonNull String signalName) {
        var trimmedEndpoint = endpoint.trim();
        if (trimmedEndpoint.isEmpty()) {
            throw new IllegalArgumentException("OpenTelemetry " + signalName +
                " endpoint cannot be blank; omit the endpoint option to disable " + signalName + " export");
        }
        var normalizedEndpoint = trimmedEndpoint.startsWith("http://") || trimmedEndpoint.startsWith("https://")
            ? trimmedEndpoint
            : "http://" + trimmedEndpoint;
        try {
            var uri = new URI(normalizedEndpoint);
            if (!"http".equals(uri.getScheme()) && !"https".equals(uri.getScheme())) {
                throw new IllegalArgumentException("OpenTelemetry " + signalName +
                    " endpoint must use http or https: " + endpoint);
            }
            if (uri.getHost() == null || uri.getHost().isEmpty()) {
                throw new IllegalArgumentException("OpenTelemetry " + signalName +
                    " endpoint must include a host: " + endpoint);
            }
            return normalizedEndpoint;
        } catch (URISyntaxException e) {
            throw new IllegalArgumentException("OpenTelemetry " + signalName +
                " endpoint is not a valid URI: " + endpoint, e);
        }
    }

    /**
     * Initialize the Otel SDK for collectors when at least one signal endpoint is configured, or setup an empty,
     * do-nothing SDK when all endpoints are null.
     * @param collectorEndpoints - URLs of the otel-collectors by signal
     * @param serviceName - name of this service that is sending data to the collector
     * @return a fully initialize OpenTelemetry object capable of producing MeterProviders and TraceProviders
     */
    public static OpenTelemetry initializeOpenTelemetryWithCollectorsOrAsNoop(
        OtelCollectorEndpoints collectorEndpoints,
        @NonNull String serviceName,
        @NonNull String instanceName
    ) {
        return Optional.ofNullable(collectorEndpoints)
            .filter(endpoints -> endpoints.getTraceEndpoint() != null || endpoints.getMetricsEndpoint() != null)
            .map(endpoints -> initializeOpenTelemetryForCollectors(endpoints, serviceName, instanceName))
            .orElseGet(() -> {
                if (serviceName != null) {
                    log.atWarn().setMessage("Collector endpoints are not configured, so serviceName parameter '{}'" +
                            " is being ignored since a no-op OpenTelemetry object is being created")
                        .addArgument(serviceName).log();
                }
                return initializeNoopOpenTelemetry();
            });
    }

    @Override
    public void onContextCreated(IScopedInstrumentationAttributes newScopedContext) {
        contextTracker.onContextCreated(newScopedContext);
    }

    @Override
    public void onContextClosed(IScopedInstrumentationAttributes newScopedContext) {
        contextTracker.onContextClosed(newScopedContext);
    }

    @Override
    public CommonMetricInstruments getMetrics() {
        return null;
    }

    public RootOtelContext(@NonNull String scopeName,
                           IContextTracker contextTracker,
                           @NonNull String serviceName,
                           @NonNull String instanceName) {
        this(scopeName, contextTracker,
            initializeOpenTelemetryWithCollectorsOrAsNoop(OtelCollectorEndpoints.empty(), serviceName, instanceName));
    }

    public RootOtelContext(String scopeName, IContextTracker contextTracker, @NonNull OpenTelemetry sdk) {
        openTelemetryImpl = sdk;
        this.scopeName = scopeName;
        this.contextTracker = contextTracker;
    }

    @Override
    public Exception getObservedExceptionToIncludeInMetrics() {
        return null;
    }

    @Override
    public RootOtelContext getEnclosingScope() {
        return null;
    }

    private OpenTelemetry getOpenTelemetry() {
        return openTelemetryImpl;
    }

    @Override
    public MeterProvider getMeterProvider() {
        return getOpenTelemetry().getMeterProvider();
    }

    private static SpanBuilder addLinkedToBuilder(Stream<Span> linkedSpanContexts, SpanBuilder spanBuilder) {
        return Optional.ofNullable(linkedSpanContexts)
            .map(ss -> ss.collect(Utils.foldLeft(spanBuilder, (b, s) -> b.addLink(s.getSpanContext()))))
            .orElse(spanBuilder);
    }

    private static Span buildSpanWithParent(SpanBuilder builder, Span parentSpan, Stream<Span> linkedSpanContexts) {
        return addLinkedToBuilder(
            linkedSpanContexts,
            Optional.ofNullable(parentSpan)
                .map(p -> builder.setParent(Context.current().with(p)))
                .orElseGet(builder::setNoParent)
        ).startSpan();
    }

    @Override
    public @NonNull Span buildSpan(
        IScopedInstrumentationAttributes forScope,
        String spanName,
        Stream<Span> linkedSpans
    ) {
        var forEnclosingScope = forScope.getEnclosingScope();
        var parentSpan = forEnclosingScope == null ? null : forEnclosingScope.getCurrentSpan();
        var spanBuilder = getOpenTelemetry().getTracer(scopeName).spanBuilder(spanName);
        return buildSpanWithParent(spanBuilder, parentSpan, linkedSpans);
    }
}
