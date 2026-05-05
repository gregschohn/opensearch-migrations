package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * Exercises the chunk-boundary-crossing paths in {@link ChunkedByteBuffer} and its
 * {@code Cursor} through a real {@link SidecarReader} opened with an intentionally
 * tiny chunk size.
 *
 * <p>The chunk size is chosen prime (17) and small so every spill file spans several
 * chunks and reads of length 4 or 8 — plus varint decode streaming via {@code Cursor}
 * — are statistically certain to straddle chunk boundaries. With the chunk size the
 * reader uses in production (1 GiB), all files in this test would fit in a single chunk
 * and the straddle path would never fire.
 */
class SidecarReaderChunkBoundaryTest {

    /** Prime, small: forces every file to span several chunks at the fabrication scale used here. */
    private static final long TEST_CHUNK_SIZE = 17L;

    @Test
    void straddlesChunkBoundaries_onAllFourMmaps(@TempDir Path tempDir) throws IOException {
        Path spillDir = tempDir.resolve("spill");

        // 5 terms, each 6-byte UTF-8: terms.dat ends up at 5*(4+6) = 50 bytes (>> 17).
        // 5 docs with multiple positions each: sidecar.dat spans multiple chunks too.
        // doc-index.dat = 5*8 = 40 bytes, term-offsets.dat = 5*4 = 20 bytes — both cross 17.
        List<Emission> stream = Arrays.asList(
            e("term00", 0, 0),
            e("term00", 0, 5),
            e("term01", 0, 1),
            e("term02", 1, 0),
            e("term02", 1, 3),
            e("term03", 2, 2),
            e("term00", 3, 0),
            e("term04", 4, 1),
            e("term03", 4, 2)
        );
        int maxDoc = 5;

        int numTerms = buildFixture(stream, spillDir, maxDoc);

        // Reference: compute the position-ordered term lists with a simple in-heap map.
        Map<Integer, List<String>> reference = inHeapReference(stream);

        try (SidecarReader reader = SidecarReader.open(spillDir, maxDoc, numTerms, TEST_CHUNK_SIZE)) {
            for (int doc = 0; doc < maxDoc; doc++) {
                assertEquals(
                    reference.getOrDefault(doc, List.of()),
                    reader.get(doc),
                    "doc " + doc + " must round-trip across chunk boundaries");
            }
        }
    }

    /**
     * Builds the on-disk fixture via the real {@link SidecarBuilder} so every on-disk
     * invariant (byte order, file layout, varint encoding) is exercised end-to-end.
     *
     * @return the number of distinct terms registered (= {@code numTerms} for the reader).
     */
    private static int buildFixture(List<Emission> stream, Path spillDir, int maxDoc) throws IOException {
        SidecarBuilder builder = new SidecarBuilder(spillDir, /*sortBufferBytes=*/1024, maxDoc);
        List<String> sortedTerms = new ArrayList<>(new TreeSet<>(stream.stream().map(e -> e.term).toList()));
        Map<String, Integer> termIds = new HashMap<>();
        for (String t : sortedTerms) {
            byte[] bytes = t.getBytes(StandardCharsets.UTF_8);
            termIds.put(t, builder.registerTerm(new BytesRefLike(bytes, 0, bytes.length)));
        }
        List<Emission> sorted = new ArrayList<>(stream);
        sorted.sort(Comparator.<Emission>comparingInt(x -> termIds.get(x.term))
                              .thenComparingInt(x -> x.docId)
                              .thenComparingInt(x -> x.position));
        int i = 0;
        while (i < sorted.size()) {
            Emission head = sorted.get(i);
            int termId = termIds.get(head.term);
            int docId = head.docId;
            int[] positions = new int[8];
            int n = 0;
            while (i < sorted.size()
                   && termIds.get(sorted.get(i).term) == termId
                   && sorted.get(i).docId == docId) {
                if (n == positions.length) positions = Arrays.copyOf(positions, positions.length * 2);
                positions[n++] = sorted.get(i).position;
                i++;
            }
            builder.accept(termId, docId, positions, n);
        }
        // Build, but close the returned reader immediately — the test opens its own with a
        // custom chunk size.
        builder.buildAndOpenReader().close();
        return sortedTerms.size();
    }

    private static Map<Integer, List<String>> inHeapReference(List<Emission> stream) {
        List<String> sortedTerms = new ArrayList<>(new TreeSet<>(stream.stream().map(e -> e.term).toList()));
        Map<String, Integer> termIds = new HashMap<>();
        for (int i = 0; i < sortedTerms.size(); i++) termIds.put(sortedTerms.get(i), i);
        Map<Integer, TreeMap<Long, String>> byDoc = new HashMap<>();
        for (Emission em : stream) {
            long key = ((long) em.position << 32) | (termIds.get(em.term) & 0xFFFFFFFFL);
            byDoc.computeIfAbsent(em.docId, k -> new TreeMap<>()).put(key, em.term);
        }
        Map<Integer, List<String>> out = new HashMap<>();
        byDoc.forEach((d, map) -> out.put(d, new ArrayList<>(map.values())));
        return out;
    }

    private static Emission e(String term, int docId, int pos) {
        return new Emission(term, docId, pos);
    }

    private static final class Emission {
        final String term;
        final int docId;
        final int position;
        Emission(String term, int docId, int position) {
            this.term = term;
            this.docId = docId;
            this.position = position;
        }
    }
}
