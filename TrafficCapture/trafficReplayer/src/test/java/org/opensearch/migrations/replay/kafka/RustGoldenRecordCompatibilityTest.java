/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * The OpenSearch Contributors require contributions made to
 * this file be licensed under the Apache-2.0 license or a
 * compatible open source license.
 */

package org.opensearch.migrations.replay.kafka;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.stream.Stream;

import org.opensearch.migrations.replay.HttpMessageAndTimestamp;
import org.opensearch.migrations.replay.SourceResponseCompletionEvidence;
import org.opensearch.migrations.replay.SourceResponseFramingFallbackReason;
import org.opensearch.migrations.replay.identity.CancellationGrace;
import org.opensearch.migrations.replay.identity.ConnectionProcessingId;
import org.opensearch.migrations.replay.identity.KafkaRecordId;
import org.opensearch.migrations.replay.identity.PartitionBatchRequestId;
import org.opensearch.migrations.replay.identity.PartitionGenerationId;
import org.opensearch.migrations.replay.identity.ReplayRequestId;
import org.opensearch.migrations.replay.intake.ReplayIntakeInput;
import org.opensearch.migrations.replay.intake.ReplayIntakeInputQueue;
import org.opensearch.migrations.replay.intake.ReplayIntakeOwner;
import org.opensearch.migrations.replay.intake.SourceAssemblySink;
import org.opensearch.migrations.replay.kafkasource.ApplicationKafkaRecord;
import org.opensearch.migrations.replay.kafkasource.KafkaSourceInput;
import org.opensearch.migrations.replay.kafkasource.KafkaSourceInputQueue;
import org.opensearch.migrations.replay.kafkasource.PolledKafkaRecord;
import org.opensearch.migrations.replay.kafkasource.WakeupController;
import org.opensearch.migrations.replay.tracing.ReplayIntakeMetrics;
import org.opensearch.migrations.replay.tracing.RootReplayerContext;
import org.opensearch.migrations.replay.util.TrafficChannelKeyFormatter;
import org.opensearch.migrations.tracing.InMemoryInstrumentationBundle;
import org.opensearch.migrations.trafficcapture.protos.CaptureRecord;
import org.opensearch.migrations.trafficcapture.protos.TrafficObservation;
import org.opensearch.migrations.trafficcapture.protos.TrafficStream;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.google.protobuf.ByteString;
import com.google.protobuf.Descriptors;
import com.google.protobuf.Message;
import org.apache.kafka.common.TopicPartition;
import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.MethodSource;

/**
 * Phase 2 exit gate of the Rust capture-proxy rewrite: <em>Java replayer utilities decode Rust-generated golden
 * records with equivalent fields and exact embedded traffic bytes.</em>
 *
 * <p>The goldens are produced by {@code capture-test-support/tests/golden_records.rs}, which drives the Rust
 * proxy's real record encoder and control-record builders with fixed inputs, and are read here from the
 * repository by path. Nothing here decodes protobuf or reassembles HTTP by hand: every record goes through
 * {@link PolledKafkaRecord} and {@link ApplicationKafkaRecord} exactly as {@code KafkaConsumerSourcePort} and
 * {@code KafkaSourceOwner} construct them, and through the real {@link ReplayIntakeOwner} with its
 * {@code SourceConnectionState} reconstruction, which is the path {@code --mode dump-both} runs. The one difference
 * from production is that inputs are applied on the calling thread, which the owner provides for deterministic
 * fixtures.
 *
 * <p>Each golden's {@code expected} block is the oracle. Its decoded fields come from an independent protobuf
 * decoder on the Rust side; its reconstructed bytes come from the scenario inputs, never from decoding. A
 * disagreement is a WIRE or PROTOCOL divergence to report, not an expectation to adjust.
 *
 * <p>The goldens live outside this project; {@code build.gradle} declares them as test-task inputs, so
 * regenerating them re-runs this test.
 */
class RustGoldenRecordCompatibilityTest {

    private static final String GOLDENS_FROM_REPOSITORY_ROOT =
        "TrafficCapture/captureProxy/crates/capture-test-support/fixtures/goldens";
    private static final String FORMAT = "capture-golden-v1";
    private static final List<String> REQUIRED_SCENARIOS = List.of(
        "keep_alive_gets",
        "critical_mutations",
        "chunked_with_trailers",
        "expect_continue_interim",
        "suppressed_request_dropped",
        "segmented_large_response",
        "segmented_large_request",
        "connection_exception",
        "close_terminal_record",
        "periodic_flush",
        "writer_partition_heartbeat",
        "capture_capability_probe"
    );
    private static final ObjectMapper JSON = new ObjectMapper();

    // ---------------------------------------------------------------- the four obligations

    @Test
    void everyRequiredScenarioHasAGolden() throws IOException {
        var present = goldenFiles().map(name -> name.replaceFirst("\\.json$", "")).toList();
        for (var scenario : REQUIRED_SCENARIOS) {
            Assertions.assertTrue(present.contains(scenario), "missing golden " + scenario + " in " + goldensDir());
        }
    }

    /**
     * Every record decodes through the replayer's own Kafka record type into fields equivalent to the expected
     * decoding, is canonical protobuf (Java re-serializes it to the identical bytes), carries no field the
     * schema does not define, renders in the raw dump view, and is keyed as its record kind requires.
     */
    @ParameterizedTest(name = "{0}")
    @MethodSource("goldenFiles")
    void javaDecodesEveryRecordWithEquivalentFields(String fileName) throws Exception {
        var golden = Golden.load(fileName);
        var expectedRecords = golden.root.get("expected").get("records");
        Assertions.assertEquals(golden.records.size(), expectedRecords.size(), "one expectation per record");
        for (var i = 0; i < golden.records.size(); i++) {
            var record = golden.records.get(i);
            var where = fileName + " record " + i + " (p" + record.partition + "@" + record.polled.offset() + ")";
            var envelope = record.polled.decodeEnvelope();

            Assertions.assertArrayEquals(
                record.polled.encodedEnvelope().toByteArray(),
                envelope.toByteArray(),
                where + ": Java re-serializes the decoded envelope to different bytes, so the Rust bytes are not"
                    + " canonical protobuf"
            );
            assertNoUnknownFields(envelope, where);
            assertDecodedRecord(expectedRecords.get(i), envelope, record, where);

            var rawLine = TrafficStreamDumper.format(envelope, record.partition, record.polled.offset(), 0, 0, -1);
            Assertions.assertFalse(rawLine.contains(" UNKNOWN"), where + ": raw dump view " + rawLine);
        }
    }

    /**
     * Replay intake applies every record without a protocol violation and reconstructs, per connection, exactly
     * the requests, interim responses, responses and closes the golden expects: same ordinals, same bytes, same
     * packet structure, same source and broker times, same completion evidence.
     */
    @ParameterizedTest(name = "{0}")
    @MethodSource("goldenFiles")
    void replayIntakeReconstructsTheExactCapturedBytes(String fileName) throws Exception {
        var golden = Golden.load(fileName);
        var sink = new RecordingSink();
        try (var replay = new Replay(sink, ReplayIntakeOwner.RecordObserver.NONE)) {
            replay.apply(golden.topic(), golden.records);

            Assertions.assertEquals(
                golden.records.size(),
                replay.metric(ReplayIntakeMetrics.MetricNames.RECORDS_APPLIED),
                "every record applied"
            );
            Assertions.assertEquals(0, replay.metric(ReplayIntakeMetrics.MetricNames.CAPTURE_PROTOCOL_VIOLATIONS));
            var transitions = golden.root.get("expected").get("writerTimeTransitions");
            if (transitions != null) {
                var expected = new TreeMap<String, Long>();
                transitions.properties().forEach(entry -> expected.put(entry.getKey(), entry.getValue().asLong()));
                Assertions.assertEquals(expected, replay.writerTimeTransitions(), fileName + ": writer time transitions");
            }
        }

        var expectedConnections = golden.root.get("expected").get("connections");
        var expectedIds = new ArrayList<String>();
        for (var connection : expectedConnections) {
            var connectionId = connection.get("connectionId").asText();
            expectedIds.add(connectionId);
            Assertions.assertEquals(
                expectedEvents(connection.get("events")),
                sink.eventsFor(connectionId),
                fileName + ": reconstruction of " + connectionId
            );
        }
        Assertions.assertTrue(
            expectedIds.containsAll(sink.eventsByConnection.keySet()),
            () -> fileName + ": events for an unexpected connection " + sink.eventsByConnection.keySet()
        );
    }

    /**
     * {@code --mode dump-both}'s two views, built exactly as {@code KafkaTopicDumper} builds them, agree with
     * the expectation: one raw line per record, and one transaction line per reconstructed event.
     */
    @ParameterizedTest(name = "{0}")
    @MethodSource("goldenFiles")
    void dumpBothRendersTheReconstruction(String fileName) throws Exception {
        var golden = Golden.load(fileName);
        var captured = new ByteArrayOutputStream();
        try (var out = new PrintStream(captured, true, StandardCharsets.UTF_8)) {
            var dumper = new HttpTransactionDumper(out, "msg ");
            var baseEpoch = golden.baseEpochSeconds();
            dumper.setBaseEpochSeconds(baseEpoch);
            ReplayIntakeOwner.RecordObserver rawView = record -> out.println(TrafficStreamDumper.format(
                record.envelope(),
                record.recordId().generation().topicPartition().partition(),
                record.recordId().offset(),
                0,
                0,
                baseEpoch
            ));
            try (var replay = new Replay(dumper, rawView)) {
                replay.apply(golden.topic(), golden.records);
            }
        }
        var lines = captured.toString(StandardCharsets.UTF_8).lines().toList();
        Assertions.assertEquals(
            golden.records.size(),
            lines.stream().filter(line -> !line.startsWith("msg ")).count(),
            () -> fileName + ": one raw line per record\n" + String.join("\n", lines)
        );

        for (var connection : golden.root.get("expected").get("connections")) {
            var key = "nc:" + TrafficChannelKeyFormatter.format(
                connection.get("nodeId").asText(),
                connection.get("connectionId").asText()
            ) + ":";
            var rendered = lines.stream()
                .filter(line -> line.startsWith("msg ") && line.contains(key))
                .map(line -> line.substring(line.indexOf(key) + key.length()))
                .toList();
            var expected = expectedEvents(connection.get("events")).stream()
                .map(RustGoldenRecordCompatibilityTest::transcriptSuffix)
                .toList();
            Assertions.assertEquals(expected, rendered, () -> fileName + ": dump-both transcript\n"
                + String.join("\n", lines));
        }
    }

    /**
     * Replay that starts at a later record of a connection, as after a consumer restart, reconstructs exactly
     * the later requests with the same ordinals. This is the observable meaning of {@code priorRequestsReceived}
     * and {@code lastObservationWasUnterminatedRead} as the Rust writer stamped them.
     */
    @ParameterizedTest(name = "{0}")
    @MethodSource("goldenFilesWithResume")
    void replayResumedMidConnectionHonoursTheContinuityFields(String fileName) throws Exception {
        var golden = Golden.load(fileName);
        for (var connection : golden.root.get("expected").get("connections")) {
            var resume = connection.get("resume");
            if (resume == null) {
                continue;
            }
            var partition = connection.get("partition").asInt();
            var fromOffset = resume.get("fromOffset").asLong();
            var suffix = golden.records.stream()
                .filter(record -> record.partition == partition && record.polled.offset() >= fromOffset)
                .toList();
            Assertions.assertFalse(suffix.isEmpty(), "the resume point names a record");
            var sink = new RecordingSink();
            try (var replay = new Replay(sink, ReplayIntakeOwner.RecordObserver.NONE)) {
                replay.apply(golden.topic(), suffix);
                Assertions.assertEquals(0, replay.metric(ReplayIntakeMetrics.MetricNames.CAPTURE_PROTOCOL_VIOLATIONS));
            }
            var connectionId = connection.get("connectionId").asText();
            Assertions.assertEquals(
                expectedEvents(resume.get("events")),
                sink.eventsFor(connectionId),
                fileName + ": reconstruction of " + connectionId + " resumed at offset " + fromOffset
            );
        }
    }

    // ---------------------------------------------------------------- golden files

    static Stream<String> goldenFiles() throws IOException {
        try (var files = Files.list(goldensDir())) {
            return files.map(path -> path.getFileName().toString())
                .filter(name -> name.endsWith(".json"))
                .sorted()
                .toList()
                .stream();
        }
    }

    static Stream<String> goldenFilesWithResume() throws IOException {
        return goldenFiles().filter(name -> {
            try {
                var connections = Golden.load(name).root.get("expected").get("connections");
                for (var connection : connections) {
                    if (connection.has("resume")) {
                        return true;
                    }
                }
                return false;
            } catch (IOException e) {
                throw new IllegalStateException(e);
            }
        });
    }

    /** Walks up from the working directory, which Gradle sets to the project directory, to the repository root. */
    private static Path goldensDir() {
        for (var dir = Path.of("").toAbsolutePath(); dir != null; dir = dir.getParent()) {
            var candidate = dir.resolve(GOLDENS_FROM_REPOSITORY_ROOT);
            if (Files.isDirectory(candidate)) {
                return candidate;
            }
        }
        throw new IllegalStateException(
            "cannot find " + GOLDENS_FROM_REPOSITORY_ROOT + " above " + Path.of("").toAbsolutePath()
        );
    }

    private record GoldenRecord(int partition, String key, PolledKafkaRecord polled) {}

    private record Golden(JsonNode root, List<GoldenRecord> records) {
        static Golden load(String fileName) throws IOException {
            var root = JSON.readTree(goldensDir().resolve(fileName).toFile());
            Assertions.assertEquals(FORMAT, root.get("format").asText(), fileName + ": golden format");
            var records = new ArrayList<GoldenRecord>();
            for (var record : root.get("records")) {
                var value = Base64.getDecoder().decode(record.get("valueBase64").asText());
                // Exactly as KafkaConsumerSourcePort.poll builds it from a ConsumerRecord.
                records.add(new GoldenRecord(
                    record.get("partition").asInt(),
                    record.get("key").asText(),
                    new PolledKafkaRecord(
                        record.get("offset").asLong(),
                        record.get("logAppendTimeMillis").asLong(),
                        value.length,
                        ByteString.copyFrom(value)
                    )
                ));
            }
            return new Golden(root, List.copyOf(records));
        }

        String topic() {
            return root.get("topic").asText();
        }

        /** As {@code KafkaTopicDumper.getBaseEpoch}: the first traffic record's first observation, else unset. */
        long baseEpochSeconds() {
            for (var record : records) {
                var envelope = record.polled.envelope();
                if (envelope.hasTrafficStream() && envelope.getTrafficStream().getSubStreamCount() > 0) {
                    return envelope.getTrafficStream().getSubStream(0).getTs().getSeconds();
                }
            }
            return -1;
        }
    }

    // ---------------------------------------------------------------- decoded-field equivalence

    private static void assertDecodedRecord(
        JsonNode expected,
        CaptureRecord envelope,
        GoldenRecord record,
        String where
    ) {
        var payload = CaptureRecord.getDescriptor()
            .findFieldByNumber(envelope.getPayloadCase().getNumber());
        Assertions.assertNotNull(payload, where + ": no payload set");
        Assertions.assertEquals(expected.get("payload").asText(), payload.getName(), where + ": payload");
        switch (envelope.getPayloadCase()) {
            case TRAFFICSTREAM -> {
                var stream = envelope.getTrafficStream();
                assertTrafficStream(expected, stream, where);
                Assertions.assertEquals(stream.getConnectionId(), record.key, where + ": key is the connection id");
            }
            case WRITERPARTITIONHEARTBEAT -> {
                var heartbeat = envelope.getWriterPartitionHeartbeat();
                Assertions.assertEquals(expected.get("writerNodeId").asText(), heartbeat.getWriterNodeId(), where);
                Assertions.assertEquals(
                    expected.get("heartbeatIntervalMillis").asLong(),
                    heartbeat.getHeartbeatIntervalMillis(),
                    where
                );
                assertOptionalLong(
                    expected.get("emittedAtMillis"),
                    heartbeat.hasEmittedAtMillis(),
                    heartbeat.getEmittedAtMillis(),
                    where + " emittedAtMillis"
                );
                Assertions.assertEquals(
                    heartbeat.getWriterNodeId() + ":heartbeat:" + record.partition,
                    record.key,
                    where + ": heartbeat key"
                );
                Assertions.assertTrue(
                    TrafficStreamDumper.format(envelope, record.partition, record.polled.offset(), 0, 0, -1)
                        .endsWith("HEARTBEAT writer:" + heartbeat.getWriterNodeId()
                            + " intervalMillis:" + heartbeat.getHeartbeatIntervalMillis()
                            + (heartbeat.hasEmittedAtMillis() ? " emittedAtMillis:" + heartbeat.getEmittedAtMillis() : "")),
                    where + ": heartbeat raw view"
                );
            }
            case CAPTURECAPABILITYPROBE -> {
                var probe = envelope.getCaptureCapabilityProbe();
                Assertions.assertEquals(expected.get("writerNodeId").asText(), probe.getWriterNodeId(), where);
                Assertions.assertEquals(expected.get("probeId").asText(), probe.getProbeId(), where);
                Assertions.assertEquals(
                    probe.getWriterNodeId() + ":" + probe.getProbeId(),
                    record.key,
                    where + ": probe key"
                );
                Assertions.assertTrue(
                    TrafficStreamDumper.format(envelope, record.partition, record.polled.offset(), 0, 0, -1)
                        .endsWith("PROBE writer:" + probe.getWriterNodeId() + " id:" + probe.getProbeId()),
                    where + ": probe raw view"
                );
            }
            case PAYLOAD_NOT_SET -> Assertions.fail(where + ": payload not set");
        }
    }

    private static void assertTrafficStream(JsonNode expected, TrafficStream stream, String where) {
        Assertions.assertEquals(expected.get("connectionId").asText(), stream.getConnectionId(), where);
        Assertions.assertEquals(expected.get("nodeId").asText(), stream.getNodeId(), where);
        var index = expected.get("index");
        var indexField = TrafficStream.getDescriptor().findFieldByNumber(stream.getIndexCase().getNumber());
        Assertions.assertNotNull(indexField, where + ": index oneof not set");
        Assertions.assertEquals(index.get("field").asText(), indexField.getName(), where + ": index arm");
        Assertions.assertEquals(
            index.get("value").asInt(),
            stream.hasNumber() ? stream.getNumber() : stream.getNumberOfThisLastChunk(),
            where + ": index value"
        );
        assertOptionalLong(
            expected.get("priorRequestsReceived"),
            stream.hasPriorRequestsReceived(),
            stream.getPriorRequestsReceived(),
            where + " priorRequestsReceived"
        );
        var unterminated = expected.get("lastObservationWasUnterminatedRead");
        Assertions.assertEquals(
            !unterminated.isNull(),
            stream.hasLastObservationWasUnterminatedRead(),
            where + ": lastObservationWasUnterminatedRead presence"
        );
        if (!unterminated.isNull()) {
            Assertions.assertEquals(unterminated.asBoolean(), stream.getLastObservationWasUnterminatedRead(), where);
        }

        var observations = expected.get("observations");
        Assertions.assertEquals(observations.size(), stream.getSubStreamCount(), where + ": observation count");
        for (var i = 0; i < stream.getSubStreamCount(); i++) {
            assertObservation(observations.get(i), stream.getSubStream(i), where + " observation " + i);
        }
    }

    private static void assertObservation(JsonNode expected, TrafficObservation observation, String where) {
        Assertions.assertTrue(observation.hasConnectionObservationSequence(), where + ": sequence present");
        Assertions.assertEquals(
            expected.get("sequence").asLong(),
            observation.getConnectionObservationSequence(),
            where + ": sequence"
        );
        Assertions.assertTrue(observation.hasTs(), where + ": ts present");
        Assertions.assertEquals(expected.get("ts").get("seconds").asLong(), observation.getTs().getSeconds(), where);
        Assertions.assertEquals(expected.get("ts").get("nanos").asInt(), observation.getTs().getNanos(), where);

        var capture = TrafficObservation.getDescriptor().findFieldByNumber(observation.getCaptureCase().getNumber());
        Assertions.assertNotNull(capture, where + ": capture oneof not set");
        Assertions.assertEquals(expected.get("capture").asText(), capture.getName(), where + ": capture arm");

        var data = dataOf(observation);
        Assertions.assertEquals(expected.has("dataLength"), data != null, where + ": carries data");
        if (data != null) {
            Assertions.assertEquals(expected.get("dataLength").asInt(), data.size(), where + ": data length");
        }
        if (observation.hasEndOfMessageIndicator()) {
            var eom = observation.getEndOfMessageIndicator();
            assertOptionalLong(
                expected.get("firstLineByteLength"),
                eom.hasFirstLineByteLength(),
                eom.getFirstLineByteLength(),
                where + " firstLineByteLength"
            );
            assertOptionalLong(
                expected.get("headersByteLength"),
                eom.hasHeadersByteLength(),
                eom.getHeadersByteLength(),
                where + " headersByteLength"
            );
        }
        if (observation.hasConnectionException()) {
            Assertions.assertEquals(
                expected.get("message").asText(),
                observation.getConnectionException().getMessage(),
                where + ": exception message"
            );
        }
    }

    private static ByteString dataOf(TrafficObservation observation) {
        return switch (observation.getCaptureCase()) {
            case READ -> observation.getRead().getData();
            case READSEGMENT -> observation.getReadSegment().getData();
            case WRITE -> observation.getWrite().getData();
            case WRITESEGMENT -> observation.getWriteSegment().getData();
            case INTERIMRESPONSE -> observation.getInterimResponse().getData();
            case INTERIMRESPONSESEGMENT -> observation.getInterimResponseSegment().getData();
            default -> null;
        };
    }

    /** A {@code null} or omitted expectation means the field is absent on the wire. */
    private static void assertOptionalLong(JsonNode expected, boolean present, long actual, String where) {
        var expectedPresent = expected != null && !expected.isNull();
        Assertions.assertEquals(expectedPresent, present, where + " presence");
        if (expectedPresent) {
            Assertions.assertEquals(expected.asLong(), actual, where);
        }
    }

    private static void assertNoUnknownFields(Message message, String where) {
        Assertions.assertTrue(
            message.getUnknownFields().asMap().isEmpty(),
            () -> where + ": unknown fields in " + message.getDescriptorForType().getName() + ": "
                + message.getUnknownFields()
        );
        for (var entry : message.getAllFields().entrySet()) {
            if (entry.getKey().getJavaType() != Descriptors.FieldDescriptor.JavaType.MESSAGE) {
                continue;
            }
            if (entry.getKey().isRepeated()) {
                for (var element : (List<?>) entry.getValue()) {
                    assertNoUnknownFields((Message) element, where);
                }
            } else {
                assertNoUnknownFields((Message) entry.getValue(), where);
            }
        }
    }

    // ---------------------------------------------------------------- replay intake

    /** One replay-intake owner over fresh telemetry, applying one generation per partition. */
    private static final class Replay implements AutoCloseable {
        private final InMemoryInstrumentationBundle telemetry = new InMemoryInstrumentationBundle(false, true);
        private final RootReplayerContext rootContext = new RootReplayerContext(telemetry.openTelemetrySdk);
        private final KafkaSourceInputQueue sourceInputs =
            new KafkaSourceInputQueue(new WakeupController(() -> {}, rootContext));
        private final ReplayIntakeOwner owner;

        Replay(SourceAssemblySink sink, ReplayIntakeOwner.RecordObserver observer) {
            // The constructor KafkaTopicDumper uses for dump-both.
            owner = new ReplayIntakeOwner(
                new ReplayIntakeInputQueue(),
                sourceInputs,
                sink,
                failure -> Assertions.fail("replay intake failed: " + failure.getMessage(), failure),
                rootContext.replayIntakeMetrics,
                observer
            );
        }

        void apply(String topic, List<GoldenRecord> records) {
            var byPartition = new TreeMap<Integer, List<GoldenRecord>>();
            records.forEach(record -> byPartition.computeIfAbsent(record.partition, p -> new ArrayList<>()).add(record));
            for (var entry : byPartition.entrySet()) {
                var generation = new PartitionGenerationId(new TopicPartition(topic, entry.getKey()), 0);
                owner.applyOnCallingThread(new ReplayIntakeInput.PartitionGenerationAssigned(generation));
                // Stamped exactly as KafkaSourceOwner stamps a polled batch, minus the tracing context.
                var stamped = entry.getValue().stream()
                    .map(record -> new ApplicationKafkaRecord(
                        new KafkaRecordId(generation, record.polled.offset()),
                        record.polled.logAppendTimeMillis(),
                        record.polled.serializedSizeBytes(),
                        record.polled.encodedEnvelope(),
                        null
                    ))
                    .toList();
                owner.applyOnCallingThread(new ReplayIntakeInput.PartitionRecordBatch(
                    new PartitionBatchRequestId(generation, 0),
                    stamped
                ));
            }
            var violations = sourceInputs.drain().stream()
                .filter(KafkaSourceInput.CaptureProtocolViolationDetected.class::isInstance)
                .toList();
            Assertions.assertEquals(List.of(), violations, "replay intake reported a capture protocol violation");
        }

        long metric(String name) {
            return InMemoryInstrumentationBundle.getMetricValueOrZero(telemetry.getFinishedMetrics(), name);
        }

        Map<String, Long> writerTimeTransitions() {
            var transitions = new TreeMap<String, Long>();
            telemetry.getFinishedMetrics().stream()
                .filter(metric -> metric.getName().equals(ReplayIntakeMetrics.MetricNames.WRITER_TIME_TRANSITIONS))
                .flatMap(metric -> metric.getLongSumData().getPoints().stream())
                .forEach(point -> transitions.merge(
                    point.getAttributes().get(ReplayIntakeMetrics.WRITER_TIME_TRANSITION_ATTRIBUTE),
                    point.getValue(),
                    Long::sum
                ));
            return transitions;
        }

        @Override
        public void close() {
            telemetry.close();
        }
    }

    // ---------------------------------------------------------------- reconstruction events

    /** An HTTP message as the replayer holds it: its packets, joined. */
    private record HttpBytes(String firstLine, int byteLength, int packetCount, String bytesBase64) {
        static HttpBytes of(HttpMessageAndTimestamp message) {
            List<byte[]> packets;
            try (var stream = message.stream()) {
                packets = stream.toList();
            }
            var joined = new ByteArrayOutputStream();
            packets.forEach(joined::writeBytes);
            var bytes = joined.toByteArray();
            var text = new String(bytes, StandardCharsets.ISO_8859_1);
            var lineEnd = text.indexOf("\r\n");
            return new HttpBytes(
                lineEnd < 0 ? text : text.substring(0, lineEnd),
                bytes.length,
                packets.size(),
                Base64.getEncoder().encodeToString(bytes)
            );
        }

        static HttpBytes of(JsonNode message) {
            return new HttpBytes(
                message.get("firstLine").asText(),
                message.get("byteLength").asInt(),
                message.get("packetCount").asInt(),
                message.get("bytesBase64").asText()
            );
        }
    }

    /** One source-assembly callback; fields a kind does not carry are null. */
    private record Event(
        String kind,
        long ordinal,
        HttpBytes message,
        Instant firstByteTs,
        Instant endOfMessageTs,
        Long completingLogAppendTimeMillis,
        String evidence,
        Instant requestForwardedTs,
        Instant ts
    ) {
        static Event of(String kind, long ordinal) {
            return new Event(kind, ordinal, null, null, null, null, null, null, null);
        }
    }

    private static List<Event> expectedEvents(JsonNode events) {
        var expected = new ArrayList<Event>();
        for (var event : events) {
            var kind = event.get("kind").asText();
            var ordinal = event.get("ordinal").asLong();
            expected.add(switch (kind) {
                case "REQ" -> new Event(kind, ordinal, HttpBytes.of(event.get("message")),
                    instant(event.get("firstByteTs")), instant(event.get("endOfMessageTs")),
                    event.get("completingLogAppendTimeMillis").asLong(), null, null, null);
                case "INT" -> new Event(kind, ordinal, HttpBytes.of(event.get("message")),
                    null, null, null, null, null, null);
                case "RSP" -> new Event(kind, ordinal, HttpBytes.of(event.get("message")),
                    null, null, null, event.get("evidence").asText(), instant(event.get("requestForwardedTs")), null);
                case "CLOSED" -> new Event(kind, ordinal, null, null, null, null, null, null, instant(event.get("ts")));
                default -> throw new IllegalArgumentException("unknown expected event kind " + kind);
            });
        }
        return expected;
    }

    private static Instant instant(JsonNode ts) {
        if (ts == null || ts.isNull()) {
            return null;
        }
        return Instant.ofEpochSecond(ts.get("seconds").asLong(), ts.get("nanos").asLong());
    }

    /** What {@link HttpTransactionDumper} prints after the connection column for an expected event. */
    private static String transcriptSuffix(Event event) {
        return switch (event.kind) {
            case "REQ" -> " REQ[" + event.message.byteLength() + "] #" + event.ordinal + " " + event.message.firstLine();
            case "INT" -> " INT[" + event.message.byteLength() + "] " + event.message.firstLine();
            case "RSP" -> " RSP[" + event.message.byteLength() + "]"
                + (SourceResponseCompletionEvidence.valueOf(event.evidence).provenComplete() ? "" : " UNPROVEN")
                + " " + event.message.firstLine();
            case "CLOSED" -> " CLOSED";
            default -> throw new IllegalArgumentException(event.kind);
        };
    }

    /**
     * Records every source-assembly callback as an {@link Event}. Callbacks no golden expects are recorded too,
     * under their own kind, so their appearance fails the comparison instead of passing unseen.
     */
    private static final class RecordingSink implements SourceAssemblySink {
        private final Map<String, List<Event>> eventsByConnection = new LinkedHashMap<>();

        List<Event> eventsFor(String connectionId) {
            return eventsByConnection.getOrDefault(connectionId, List.of());
        }

        private void add(ConnectionProcessingId connection, Event event) {
            eventsByConnection
                .computeIfAbsent(connection.capturedConnectionId().connectionId(), id -> new ArrayList<>())
                .add(event);
        }

        @Override
        public void onRequestReconstituted(
            ReplayRequestId replayRequestId,
            long capturedRequestOrdinal,
            HttpMessageAndTimestamp.Request request,
            Instant requestFirstByteSourceTime,
            Instant requestEndOfMessageSourceTime,
            long requestCompletingLogAppendTime
        ) {
            Assertions.assertEquals(capturedRequestOrdinal, replayRequestId.capturedRequestOrdinal());
            add(replayRequestId.connectionProcessingId(), new Event(
                "REQ",
                capturedRequestOrdinal,
                HttpBytes.of(request),
                requestFirstByteSourceTime,
                requestEndOfMessageSourceTime,
                requestCompletingLogAppendTime,
                null,
                null,
                null
            ));
        }

        @Override
        public void onSourceInterimResponse(
            ReplayRequestId replayRequestId,
            HttpMessageAndTimestamp.InterimResponse interimResponse
        ) {
            add(replayRequestId.connectionProcessingId(), new Event(
                "INT",
                replayRequestId.capturedRequestOrdinal(),
                HttpBytes.of(interimResponse),
                null, null, null, null, null, null
            ));
        }

        @Override
        public void onSourceResponseComplete(
            ReplayRequestId replayRequestId,
            HttpMessageAndTimestamp.Response response,
            SourceResponseCompletionEvidence completionEvidence
        ) {
            add(replayRequestId.connectionProcessingId(), new Event(
                "RSP",
                replayRequestId.capturedRequestOrdinal(),
                HttpBytes.of(response),
                null,
                null,
                null,
                completionEvidence.name(),
                response.getRequestForwardedTimestamp(),
                null
            ));
        }

        @Override
        public void onSourceResponseFramingFallback(
            ReplayRequestId replayRequestId,
            SourceResponseFramingFallbackReason reason,
            Throwable failure
        ) {
            add(replayRequestId.connectionProcessingId(),
                Event.of("FRAMING_FALLBACK_" + reason, replayRequestId.capturedRequestOrdinal()));
        }

        @Override
        public void onPostFramingResponseObservation(
            ReplayRequestId replayRequestId,
            PostFramingObservationType observationType,
            int byteCount
        ) {
            add(replayRequestId.connectionProcessingId(),
                Event.of("POST_FRAMING_" + observationType, replayRequestId.capturedRequestOrdinal()));
        }

        @Override
        public void onSourceResponseIncomplete(ReplayRequestId replayRequestId, IncompleteReason reason) {
            add(replayRequestId.connectionProcessingId(),
                Event.of("INCOMPLETE_" + reason, replayRequestId.capturedRequestOrdinal()));
        }

        @Override
        public void onCapturedClose(
            ConnectionProcessingId connectionProcessingId,
            long capturedOrdinal,
            Instant closeTime
        ) {
            add(connectionProcessingId, new Event(
                "CLOSED",
                capturedOrdinal,
                null, null, null, null, null, null,
                closeTime
            ));
        }

        @Override
        public void onCapturedConnectionExpired(ConnectionProcessingId connectionProcessingId) {
            add(connectionProcessingId, Event.of("EXPIRED", -1));
        }

        @Override
        public void onGracefulGenerationCancellation(
            ConnectionProcessingId connectionProcessingId,
            CancellationGrace grace
        ) {
            add(connectionProcessingId, Event.of("GRACEFUL_CANCELLATION", -1));
        }

        @Override
        public void onForceGenerationCancellation(ConnectionProcessingId connectionProcessingId) {
            add(connectionProcessingId, Event.of("FORCE_CANCELLATION", -1));
        }

        @Override
        public void onConnectionOwnerFinished(ConnectionProcessingId connectionProcessingId) {
            // No connection owner exists in this fixture, as in dump modes.
        }
    }
}
