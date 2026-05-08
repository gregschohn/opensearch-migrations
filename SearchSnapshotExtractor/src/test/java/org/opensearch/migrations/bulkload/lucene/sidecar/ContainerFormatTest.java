package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.stream.Stream;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.opensearch.migrations.bulkload.lucene.sidecar.SidecarTestSupport.bytesRef;
import static org.opensearch.migrations.bulkload.lucene.sidecar.SidecarTestSupport.rm;

/**
 * Pins the new unified-container sidecar format on disk:
 *
 * <ul>
 *   <li>Positional sidecar: one file {@code sidecar.bin} with {@link
 *       shadow.lucene10.org.apache.lucene.codecs.CodecUtil} header + footer.
 *   <li>Single-term sidecar: one file {@code single-sidecar.bin}, same envelope.
 *   <li>DirectMonotonicWriter meta+data for term/doc offsets, IndexedDISI for
 *       has-values bitmaps. Reader fails cleanly if the header is tampered with.
 * </ul>
 */
class ContainerFormatTest {

    private Path spillDir;

    @BeforeEach
    void setUp() throws IOException {
        spillDir = Files.createTempDirectory("sidecar-container-test-");
    }

    @AfterEach
    void tearDown() throws IOException {
        rm(spillDir);
    }

    @Test
    void positionalSidecar_producesSingleContainerFile() throws IOException {
        try (SidecarBuilder builder = new SidecarBuilder(spillDir, 1024, 3)) {
            int a = builder.registerTerm(bytesRef("apple"));
            int b = builder.registerTerm(bytesRef("banana"));
            builder.accept(a, 0, new int[]{0}, 1);
            builder.accept(b, 2, new int[]{1}, 1);
            try (SidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("apple",  reader.getTermStrings(0).get(0));
                assertEquals("banana", reader.getTermStrings(2).get(0));
            }
        }

        // Post-build: exactly one container file in the spill dir.
        long binFiles;
        try (Stream<Path> s = Files.list(spillDir)) {
            binFiles = s.filter(p -> p.getFileName().toString().equals("sidecar.bin")).count();
        }
        assertEquals(1L, binFiles, "Expected a single sidecar.bin container; spill dir contents: "
                + listing(spillDir));
    }

    @Test
    void singleTermSidecar_producesSingleContainerFile() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 5)) {
            int t = builder.registerTerm(bytesRef("alpha"));
            builder.accept(t, 0);
            builder.accept(t, 3);
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("alpha", reader.get(0));
                assertNull(reader.get(1));
                assertEquals("alpha", reader.get(3));
                assertNull(reader.get(4));
            }
        }
        long binFiles;
        try (Stream<Path> s = Files.list(spillDir)) {
            binFiles = s.filter(p -> p.getFileName().toString().equals("single-sidecar.bin")).count();
        }
        assertEquals(1L, binFiles, "Expected a single single-sidecar.bin container; spill dir contents: "
                + listing(spillDir));
    }

    /**
     * Sparse docs force the IndexedDISI bitmap to store gaps — proves the reader's DISI
     * ordinal mapping lines up with the DirectMonotonic doc-offsets table.
     */
    @Test
    void sparseDocs_roundTripViaDisiAndMonotonicOffsets() throws IOException {
        // maxDoc = 10_000; only docs 7, 5_000, and 9_999 have values.
        try (SidecarBuilder builder = new SidecarBuilder(spillDir, 1024, 10_000)) {
            int t = builder.registerTerm(bytesRef("tok"));
            builder.accept(t, 7,      new int[]{0}, 1);
            builder.accept(t, 5_000,  new int[]{0}, 1);
            builder.accept(t, 9_999,  new int[]{0}, 1);
            try (SidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("tok", reader.getTermStrings(7).get(0));
                assertEquals("tok", reader.getTermStrings(5_000).get(0));
                assertEquals("tok", reader.getTermStrings(9_999).get(0));
                // Spot-check a few explicitly-absent docs.
                assertTrue(reader.get(0).isEmpty());
                assertTrue(reader.get(100).isEmpty());
                assertTrue(reader.get(9_998).isEmpty());
            }
        }
    }

    /**
     * DirectWriter bit-packing kicks in: many distinct termIds need ceil(log2(N)) bits/value,
     * which is strictly less than 32 for anything under 2^32 distinct terms. Proves the
     * round-trip of the bit-packed value stream.
     */
    @Test
    void singleTermSidecar_bitPacksValuesAtFewBitsPerDoc() throws IOException {
        int numTerms = 200;   // bitsRequired(199) = 8 — well below 32.
        int numDocs  = 1_000;
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, numDocs)) {
            int[] termIds = new int[numTerms];
            for (int i = 0; i < numTerms; i++) {
                termIds[i] = builder.registerTerm(bytesRef("t" + i));
            }
            for (int d = 0; d < numDocs; d++) {
                // Assign each doc a deterministic termId — a hash-like pattern covers
                // the whole termId range.
                builder.accept(termIds[(d * 37) % numTerms], d);
            }
            try (SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
                for (int d = 0; d < numDocs; d++) {
                    int expectedId = (d * 37) % numTerms;
                    assertEquals("t" + expectedId, reader.get(d),
                            "Mismatch for doc " + d);
                }
            }
        }
    }

    @Test
    void corruptedHeader_failsCleanlyOnOpen() throws IOException {
        try (SidecarBuilder builder = new SidecarBuilder(spillDir, 1024, 2)) {
            int t = builder.registerTerm(bytesRef("x"));
            builder.accept(t, 0, new int[]{0}, 1);
            try (SidecarReader reader = builder.buildAndOpenReader()) {
                assertEquals("x", reader.getTermStrings(0).get(0));
            }
        }

        // Flip one byte near the top of the file (inside CodecUtil index header magic).
        Path file = spillDir.resolve(SidecarBuilder.SIDECAR_FILE);
        byte[] bytes = Files.readAllBytes(file);
        bytes[3] ^= (byte) 0xFF;
        Files.write(file, bytes);

        // Reader should refuse to open — whether via corrupted-header or checksum mismatch.
        assertThrows(IOException.class, () -> SidecarReader.open(spillDir));
    }

    @Test
    void emptySegment_positional_roundTrip() throws IOException {
        try (SidecarBuilder builder = new SidecarBuilder(spillDir, 1024, 5);
             SidecarReader reader = builder.buildAndOpenReader()) {
            for (int d = 0; d < 5; d++) {
                assertTrue(reader.get(d).isEmpty());
            }
        }
    }

    @Test
    void emptySegment_singleTerm_roundTrip() throws IOException {
        try (SingleTermSidecarBuilder builder = new SingleTermSidecarBuilder(spillDir, 5);
             SingleTermSidecarReader reader = builder.buildAndOpenReader()) {
            for (int d = 0; d < 5; d++) {
                assertNull(reader.get(d));
            }
        }
    }

    @Test
    void tokenBytesSurviveContainerCopy() throws IOException {
        // Many distinct terms — stresses the stage→container rebase of the DirectMonotonic
        // term-offsets table.
        int N = 5_000;
        try (SidecarBuilder builder = new SidecarBuilder(spillDir, 1024, N)) {
            int[] ids = new int[N];
            for (int i = 0; i < N; i++) {
                ids[i] = builder.registerTerm(bytesRef("term-" + String.format("%06d", i)));
            }
            // One position per doc, termId picked deterministically.
            for (int d = 0; d < N; d++) {
                builder.accept(ids[d], d, new int[]{0}, 1);
            }
            try (SidecarReader reader = builder.buildAndOpenReader()) {
                for (int d = 0; d < N; d++) {
                    assertEquals("term-" + String.format("%06d", d),
                            reader.getTermStrings(d).get(0),
                            "Doc " + d + " term should decode from the rebased monotonic offsets");
                }
            }
        }
    }

    private static String listing(Path dir) {
        try (Stream<Path> s = Files.list(dir)) {
            StringBuilder sb = new StringBuilder("[");
            s.forEach(p -> sb.append(p.getFileName()).append(','));
            if (sb.length() > 1) sb.setLength(sb.length() - 1);
            sb.append(']');
            return sb.toString();
        } catch (IOException e) {
            return "(listing error: " + e.toString() + ")";
        }
    }
}
