package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.Map;
import java.util.Random;
import java.util.TreeSet;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.opensearch.migrations.bulkload.lucene.sidecar.SidecarTestSupport.bytesRef;
import static org.opensearch.migrations.bulkload.lucene.sidecar.SidecarTestSupport.rm;

/**
 * Round-trip tests for {@link SingleTermSidecarBuilder} / {@link SingleTermSidecarReader},
 * mirroring the coverage that {@link SidecarBuilderRoundTripTest} gives the positional path.
 *
 * <p>Pins: empty segment, single doc, duplicate accept (last write wins), out-of-range docIds,
 * termId at the numTerms boundary, and a fuzz run that covers bit-packing across several
 * cardinalities so {@link shadow.lucene10.org.apache.lucene.util.packed.DirectWriter}'s
 * bits-per-value selection is exercised end-to-end.
 */
class SingleTermSidecarRoundTripTest {

    private Path spillDir;

    @BeforeEach
    void setUp() throws IOException {
        spillDir = Files.createTempDirectory("single-term-test-");
    }

    @AfterEach
    void tearDown() throws IOException {
        rm(spillDir);
    }

    @Test
    void maxDocZero_clampsToOne_allDocsAbsent() throws IOException {
        // Defensive: Lucene segments always have maxDoc >= 1 in practice, but the builder
        // silently clamps to avoid zero-length on-disk structures. Pin that behavior.
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 0);
             SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
            assertEquals(1, reader.maxDoc(), "maxDoc=0 must clamp to 1");
            assertNull(reader.get(0));
        }
    }

    @Test
    void emptySegment_allDocsReturnNull() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 5);
             SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
            for (int d = 0; d < 5; d++) {
                assertNull(reader.get(d));
            }
        }
    }

    @Test
    void singleDoc_singleTerm_roundTrip() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 1)) {
            int t = builder.registerTerm(bytesRef("only"));
            builder.accept(t, 0);
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("only", reader.get(0));
            }
        }
    }

    @Test
    void duplicateAccept_lastWriteWins() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 1)) {
            int a = builder.registerTerm(bytesRef("alpha"));
            int b = builder.registerTerm(bytesRef("bravo"));
            builder.accept(a, 0);
            builder.accept(b, 0); // overwrites
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("bravo", reader.get(0));
            }
        }
    }

    @Test
    void outOfRangeDocId_returnsNull() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 3)) {
            int t = builder.registerTerm(bytesRef("x"));
            builder.accept(t, 0);
            builder.accept(t, 2);
            // accept with out-of-range docId is silently ignored
            builder.accept(t, 3);
            builder.accept(t, -1);
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("x",  reader.get(0));
                assertNull(reader.get(1));
                assertEquals("x",  reader.get(2));
                // get() also treats out-of-range as null.
                assertNull(reader.get(3));
                assertNull(reader.get(-1));
                assertNull(reader.get(Integer.MAX_VALUE));
            }
        }
    }

    @Test
    void termIdAtBoundary_roundsTrip() throws IOException {
        // Register 256 terms; last termId = 255, which needs exactly 8 bits via DirectWriter.
        int numTerms = 256;
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, numTerms)) {
            int[] ids = new int[numTerms];
            for (int i = 0; i < numTerms; i++) {
                ids[i] = builder.registerTerm(bytesRef("t" + i));
            }
            assertEquals(255, ids[numTerms - 1]);
            for (int d = 0; d < numTerms; d++) {
                builder.accept(ids[d], d);
            }
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals(numTerms, reader.numTerms());
                assertEquals(numTerms, reader.maxDoc());
                for (int d = 0; d < numTerms; d++) {
                    assertEquals("t" + d, reader.get(d),
                            "boundary-termId doc " + d + " must round-trip");
                }
            }
        }
    }

    @Test
    void corruptHeader_rejected() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 1)) {
            int t = builder.registerTerm(bytesRef("y"));
            builder.accept(t, 0);
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("y", reader.get(0));
            }
        }
        Path file = spillDir.resolve(SingleTermSidecarBuilder.SIDECAR_FILE);
        byte[] bytes = Files.readAllBytes(file);
        bytes[3] ^= (byte) 0xFF;
        Files.write(file, bytes);
        assertThrows(IOException.class, () -> SingleTermSidecarReader.open(spillDir));
    }

    @Test
    void fuzz_cardinalities_matchReference() throws IOException {
        Random rng = new Random(0xCAFEBABEL);
        // Three cardinalities exercise DirectWriter's rounding up to 1, 8, and 16 bits/value.
        // At N=2 → bitsRequired(1)=1; N=200 → 8; N=40000 → 16.
        int[] cardinalities = { 2, 200, 40_000 };
        for (int numTerms : cardinalities) {
            Path sub = Files.createDirectory(spillDir.resolve("cardinality-" + numTerms));
            int numDocs = Math.min(numTerms * 2, 500);

            try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(sub, numDocs)) {
                int[] ids = new int[numTerms];
                for (int i = 0; i < numTerms; i++) {
                    ids[i] = builder.registerTerm(bytesRef(String.format("%s-%06d", "t", i)));
                }
                Map<Integer, String> reference = new HashMap<>();
                // Sparse: only half of docs get a value.
                for (int d = 0; d < numDocs; d++) {
                    if (rng.nextBoolean()) {
                        int termIdx = rng.nextInt(numTerms);
                        builder.accept(ids[termIdx], d);
                        reference.put(d, "t-" + String.format("%06d", termIdx));
                    }
                }
                try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                    for (int d = 0; d < numDocs; d++) {
                        String got = reader.get(d);
                        String want = reference.get(d);
                        assertEquals(want, got,
                                "Cardinality " + numTerms + " doc " + d + " mismatch");
                    }
                }
            }
        }
    }

    @Test
    void unicodeTermsRoundTrip() throws IOException {
        // Sorted-bytes order matters — register in byte-ascending order (TreeSet handles it).
        TreeSet<String> terms = new TreeSet<>();
        terms.add("ascii");
        terms.add("café");
        terms.add("日本語");
        terms.add("😀");
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, terms.size())) {
            int idx = 0;
            Map<Integer, String> byDoc = new HashMap<>();
            for (String t : terms) {
                int id = builder.registerTerm(bytesRef(t));
                builder.accept(id, idx);
                byDoc.put(idx, t);
                idx++;
            }
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                for (Map.Entry<Integer, String> e : byDoc.entrySet()) {
                    assertEquals(e.getValue(), reader.get(e.getKey()));
                }
            }
        }
    }

}
