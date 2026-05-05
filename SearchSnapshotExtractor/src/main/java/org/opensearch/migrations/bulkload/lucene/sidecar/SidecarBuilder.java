package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.BufferedOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.List;
import java.util.PriorityQueue;

import lombok.extern.slf4j.Slf4j;

/**
 * {@code SidecarBuilder} using a JDK-native external merge sort over fixed-width 12-byte
 * {@code (docId, pos, termId)} records. Retained heap is bounded by a configurable per-run
 * sort buffer (default 256 MiB), independent of total positions, so the builder handles
 * segments whose position count exceeds JVM heap by orders of magnitude.
 *
 * <p>Algorithm:
 * <ol>
 *   <li>{@link #registerTerm} streams term bytes to {@code terms.dat} and writes the
 *       LE int offset to {@code term-offsets.dat}.</li>
 *   <li>{@link #accept} appends each {@code (docId, pos, termId)} triple as a fixed
 *       12-byte big-endian record into an in-memory sort buffer. Big-endian non-negative
 *       ints sort correctly under signed-int comparison, giving us {@code (docId ASC,
 *       pos ASC, termId ASC)} for free on the primitive int keys we pack into a
 *       {@code long[]} index-sort array. When the buffer fills, it is sorted and spilled
 *       to a run file, and the buffer is reused.</li>
 *   <li>{@link #buildAndOpenReader} flushes the last run, performs a k-way merge of all
 *       runs via a min-heap keyed by the first 8 bytes of each record (docId hi, pos lo),
 *       and streams output directly into {@code sidecar.dat} one doc at a time, emitting
 *       the uvint header + pair payload on each docId boundary.</li>
 * </ol>
 *
 * <p>On-disk output is identical to earlier {@code SidecarBuilder} revisions — the shared
 * {@link SidecarReader} consumes this builder's output without modification.
 *
 * <p>Performance characteristics:
 * <ul>
 *   <li>Retained heap ≤ {@code sortBufferBytes} + {@code (64 KiB read buf) × numRuns} +
 *       {@code 8 KiB} (doc-index write buffer). For a 256 MiB sort buffer and a 4 GiB raw tuple
 *       stream that's ~128 runs → ~40 MiB steady-state retained. <b>Independent of both tuple
 *       count and {@code maxDoc}</b> — the doc-index is streamed forward during merge, padding
 *       sentinel entries for docs with no tokens. Mergesort is O(N log (N/B)) I/O-optimal.</li>
 *   <li>Accept phase: each tuple is one {@code byte[12]} copy into the sort buffer plus
 *       amortized 12 bytes to disk. No per-doc Java objects, no array doubling.</li>
 *   <li>Build phase: streams sorted records with per-doc payloads accumulated in a
 *       {@code ByteBuffer} that grows to the largest single-doc payload (bounded by
 *       positions-per-doc, not total positions).</li>
 * </ul>
 */
@Slf4j
public final class SidecarBuilder implements PostingsSink, AutoCloseable {
    /** Fixed record width: docId(4) + pos(4) + termId(4). */
    public static final int RECORD_BYTES = 12;

    /** Sentinel written into doc-index.dat for docs that have no tokens in this field. */
    static final long DOC_INDEX_NO_TOKENS = -1L;

    /** File names — package-private so {@link SidecarReader} can find them. */
    static final String TERMS_FILE        = "terms.dat";
    static final String TERM_OFFSETS_FILE = "term-offsets.dat";
    static final String SIDECAR_FILE      = "sidecar.dat";
    static final String DOC_INDEX_FILE    = "doc-index.dat";

    /** Run file prefix. */
    private static final String RUN_PREFIX = "run-";

    /** Default in-memory sort buffer when caller supplies no hint. */
    private static final int DEFAULT_SORT_BUFFER_BYTES = 256 * 1024 * 1024;
    /** Absolute floor — must hold at least a few hundred records to amortize spill. */
    private static final int MIN_SORT_BUFFER_BYTES = 1 * 1024 * 1024;

    /** Per-run read buffer size during merge. Small to keep retained heap low. */
    private static final int MERGE_READ_BUFFER_BYTES = 64 * 1024;

    private final Path spillDir;
    private final int maxDoc;
    private final int sortBufferBytes;
    /** Number of fixed records that fit in the sort buffer. */
    private final int sortBufferRecords;

    private final DataOutputStream termsOut;
    private final DataOutputStream termOffsetsOut;

    private final Path termsFile;
    private final Path termOffsetsFile;

    /** Sort buffer: packed records, each RECORD_BYTES wide, positions 0..inBuffer × RECORD_BYTES. */
    private final byte[] sortBuffer;
    private int inBuffer = 0; // number of records currently in sortBuffer

    /** Absolute paths of spilled run files, in creation order. */
    private final List<Path> runs = new ArrayList<>();

    /** Reusable 12-byte scratch for record emission. */
    private final byte[] recordScratch = new byte[RECORD_BYTES];

    private int nextTermId = 0;
    private int currentTermOffset = 0;
    private boolean built = false;
    private boolean closed = false;

    /**
     * @param spillDir        per-(segment,field) directory owned by this builder.
     * @param sortBufferBytes in-memory sort buffer. Pass 0 to use the default (256 MiB).
     *                        This bounds steady-state retained heap; larger = fewer runs = faster merge.
     * @param maxDoc          the segment's {@code maxDoc}.
     */
    public SidecarBuilder(Path spillDir, int sortBufferBytes, int maxDoc) throws IOException {
        this.spillDir = spillDir;
        this.maxDoc = Math.max(1, maxDoc);
        int effectiveBuf = sortBufferBytes > 0 ? sortBufferBytes : DEFAULT_SORT_BUFFER_BYTES;
        effectiveBuf = Math.max(MIN_SORT_BUFFER_BYTES, effectiveBuf);
        // Round down to a multiple of RECORD_BYTES.
        effectiveBuf -= (effectiveBuf % RECORD_BYTES);
        this.sortBufferBytes = effectiveBuf;
        this.sortBufferRecords = effectiveBuf / RECORD_BYTES;

        Files.createDirectories(spillDir);
        this.termsFile = spillDir.resolve(TERMS_FILE);
        this.termOffsetsFile = spillDir.resolve(TERM_OFFSETS_FILE);
        this.termsOut = new DataOutputStream(new BufferedOutputStream(
            Files.newOutputStream(termsFile, StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING)));
        this.termOffsetsOut = new DataOutputStream(new BufferedOutputStream(
            Files.newOutputStream(termOffsetsFile, StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING)));

        this.sortBuffer = new byte[this.sortBufferBytes];
    }

    @Override
    public int registerTerm(BytesRefLike term) throws IOException {
        int id = nextTermId++;
        termOffsetsOut.writeInt(Integer.reverseBytes(currentTermOffset));
        termsOut.writeInt(term.length());
        termsOut.write(term.bytes(), term.offset(), term.length());
        currentTermOffset += 4 + term.length();
        return id;
    }

    @Override
    public void accept(int termId, int docId, int[] positions, int positionCount) throws IOException {
        if (positionCount <= 0) return;
        if (docId < 0 || docId >= maxDoc) return;

        byte[] rec = recordScratch;
        // docId and termId stay fixed across every position in this accept call; the
        // pos slot (bytes 4..7) is rewritten per iteration.
        writeBE32(rec, 0, docId);
        writeBE32(rec, 8, termId);

        for (int i = 0; i < positionCount; i++) {
            writeBE32(rec, 4, positions[i]);

            if (inBuffer == sortBufferRecords) {
                spillRun();
            }
            System.arraycopy(rec, 0, sortBuffer, inBuffer * RECORD_BYTES, RECORD_BYTES);
            inBuffer++;
        }
    }

    /**
     * Sort {@link #sortBuffer}[0..inBuffer] by (docId, pos, termId) ASC and append as a new run file.
     */
    private void spillRun() throws IOException {
        if (inBuffer == 0) return;
        sortSortBuffer();
        Path runPath = spillDir.resolve(RUN_PREFIX + runs.size() + ".dat");
        try (OutputStream os = new BufferedOutputStream(Files.newOutputStream(runPath,
                StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING, StandardOpenOption.WRITE))) {
            os.write(sortBuffer, 0, inBuffer * RECORD_BYTES);
        }
        runs.add(runPath);
        inBuffer = 0;
    }

    /**
     * Sort the in-memory sort buffer in place by (docId, pos, termId) ASC.
     *
     * <p>Strategy: extract records into parallel primitive arrays
     * {@code long[] keys = (docId << 32) | pos} and {@code int[] termIds}, sort the pair
     * with a primitive dual-pivot quicksort (no boxing, no virtual comparator dispatch),
     * then scatter the sorted records back into the byte buffer.
     *
     * <p>Transient allocation per spill: {@code 12 × n} bytes for the parallel arrays
     * plus {@code 12 × n} bytes for the scatter scratch — same order of magnitude as the
     * sort buffer itself, one-shot, released immediately after.
     *
     * <p>Correctness note: the producer guarantees {@code (docId, pos, termId)} triples
     * are globally unique, so a stable sort is not required. Ties on the primary key
     * {@code (docId, pos)} fall back to {@code termId}.
     */
    private void sortSortBuffer() {
        final int n = inBuffer;
        final byte[] src = sortBuffer;

        long[] keys = new long[n];
        int[] termIds = new int[n];
        for (int i = 0; i < n; i++) {
            int off = i * RECORD_BYTES;
            int docId = readBE32(src, off);
            int pos = readBE32(src, off + 4);
            int termId = readBE32(src, off + 8);
            keys[i] = ((long) docId << 32) | (pos & 0xFFFFFFFFL);
            termIds[i] = termId;
        }

        quicksortPair(keys, termIds, 0, n - 1);

        // Scatter sorted records back to src in order.
        for (int i = 0; i < n; i++) {
            long k = keys[i];
            int docId = (int) (k >>> 32);
            int pos = (int) k;
            int termId = termIds[i];
            int off = i * RECORD_BYTES;
            writeBE32(src, off, docId);
            writeBE32(src, off + 4, pos);
            writeBE32(src, off + 8, termId);
        }
    }

    /**
     * Recursive dual-pivot-esque quicksort over two parallel arrays, sorting by
     * {@code keys[]} ASC (unsigned), with {@code termIds[]} as a tiebreaker.
     *
     * <p>Uses {@link Long#compareUnsigned} on the primary key so that {@code (docId << 32)
     * | pos} sorts naturally even when {@code docId}'s sign bit is clear (which it always
     * is for non-negative {@code int}s — so in practice plain {@code <}/{@code >} would
     * suffice, but the unsigned compare is a single AMD64 {@code cmp} instruction and
     * future-proofs against any accidental 32-bit overflow).
     *
     * <p>Falls back to insertion sort for {@code n ≤ 16} partitions.
     */
    private static void quicksortPair(long[] keys, int[] termIds, int loIn, int hiIn) {
        int lo = loIn;
        int hi = hiIn;
        while (lo < hi) {
            int span = hi - lo + 1;
            if (span <= 16) {
                insertionSortPair(keys, termIds, lo, hi);
                return;
            }
            int pivot = hoarePartition(keys, termIds, lo, hi);
            // Recurse into the smaller partition, iterate on the larger (bounded stack depth).
            if ((pivot - lo) < (hi - (pivot + 1))) {
                quicksortPair(keys, termIds, lo, pivot);
                lo = pivot + 1;
            } else {
                quicksortPair(keys, termIds, pivot + 1, hi);
                hi = pivot;
            }
        }
    }

    /**
     * Hoare partition over [{@code lo}, {@code hi}] using a median-of-three pivot. Returns the
     * index {@code j} such that every element in [{@code lo}, {@code j}] sorts ≤ pivot and every
     * element in [{@code j+1}, {@code hi}] sorts ≥ pivot (standard Hoare post-condition).
     */
    private static int hoarePartition(long[] keys, int[] termIds, int lo, int hi) {
        int mid = lo + ((hi - lo + 1) >>> 1);
        sortThree(keys, termIds, lo, mid, hi);
        long pivotKey = keys[mid];
        int pivotTerm = termIds[mid];
        int i = lo - 1;
        int j = hi + 1;
        while (true) {
            do {
                i++;
            } while (cmpPair(keys[i], termIds[i], pivotKey, pivotTerm) < 0);
            do {
                j--;
            } while (cmpPair(keys[j], termIds[j], pivotKey, pivotTerm) > 0);
            if (i >= j) return j;
            swap(keys, termIds, i, j);
        }
    }

    /** Sort the three positions {@code a < b < c} in place so keys[a] ≤ keys[b] ≤ keys[c]. */
    private static void sortThree(long[] keys, int[] termIds, int a, int b, int c) {
        if (cmpPair(keys[a], termIds[a], keys[b], termIds[b]) > 0) {
            swap(keys, termIds, a, b);
        }
        if (cmpPair(keys[a], termIds[a], keys[c], termIds[c]) > 0) {
            swap(keys, termIds, a, c);
        }
        if (cmpPair(keys[b], termIds[b], keys[c], termIds[c]) > 0) {
            swap(keys, termIds, b, c);
        }
    }

    private static void swap(long[] keys, int[] termIds, int x, int y) {
        long tk = keys[x];
        keys[x] = keys[y];
        keys[y] = tk;
        int tt = termIds[x];
        termIds[x] = termIds[y];
        termIds[y] = tt;
    }

    private static void insertionSortPair(long[] keys, int[] termIds, int lo, int hi) {
        for (int i = lo + 1; i <= hi; i++) {
            long k = keys[i];
            int t = termIds[i];
            int j = i - 1;
            while (j >= lo && cmpPair(keys[j], termIds[j], k, t) > 0) {
                keys[j + 1] = keys[j];
                termIds[j + 1] = termIds[j];
                j--;
            }
            keys[j + 1] = k;
            termIds[j + 1] = t;
        }
    }

    /** Unsigned key compare, ties broken by unsigned termId compare. */
    private static int cmpPair(long k1, int t1, long k2, int t2) {
        int c = Long.compareUnsigned(k1, k2);
        if (c != 0) return c;
        return Integer.compareUnsigned(t1, t2);
    }

    private static void writeBE32(byte[] b, int off, int v) {
        b[off]     = (byte) (v >>> 24);
        b[off + 1] = (byte) (v >>> 16);
        b[off + 2] = (byte) (v >>> 8);
        b[off + 3] = (byte) v;
    }

    private static int readBE32(byte[] b, int off) {
        return ((b[off] & 0xFF) << 24)
             | ((b[off+1] & 0xFF) << 16)
             | ((b[off+2] & 0xFF) << 8)
             |  (b[off+3] & 0xFF);
    }

    public SidecarReader buildAndOpenReader() throws IOException {
        if (built) throw new IllegalStateException("SidecarBuilder.buildAndOpenReader already called");
        built = true;

        termsOut.flush();
        termsOut.close();
        termOffsetsOut.flush();
        termOffsetsOut.close();

        Path sidecarFile = spillDir.resolve(SIDECAR_FILE);
        Path docIndexFile = spillDir.resolve(DOC_INDEX_FILE);

        // Stream doc-index.dat alongside sidecar.dat — no retained long[maxDoc] in heap.
        // Merge emits docIds in ascending order, so the writer just pads gaps with the
        // no-tokens sentinel as it advances. Retained heap for the offset table becomes O(1).
        try (DocIndexWriter docIndex = new DocIndexWriter(docIndexFile)) {
            if (runs.isEmpty()) {
                // Fast path: no spill happened yet — the whole dataset fits in the in-memory
                // sort buffer. Sort it, then stream directly to sidecar.dat without round-tripping
                // through a run file. This is the common case for small segments and is meaningfully
                // faster than the general k-way merge path.
                sortSortBuffer();
                streamSortedPairsToSidecar(new InMemoryPairCursor(sortBuffer, inBuffer), sidecarFile, docIndex);
            } else {
                // General path: flush the in-memory tail as a final run, then k-way merge.
                if (inBuffer > 0) {
                    spillRun();
                }
                try (RunReaderBundle bundle = RunReaderBundle.open(runs)) {
                    streamSortedPairsToSidecar(bundle.cursor(), sidecarFile, docIndex);
                }
            }
            docIndex.finish(maxDoc);
        }

        for (Path run : runs) {
            tryDelete(run);
        }
        runs.clear();
        closed = true;
        return SidecarReader.open(spillDir, maxDoc, nextTermId);
    }

    /**
     * Shared driver for both the fast in-memory path and the k-way merge path: pulls
     * sorted {@code (docId, pos, termId)} triples from the cursor and emits one doc's
     * payload at a time into {@code sidecar.dat}.
     */
    private static void streamSortedPairsToSidecar(PairCursor cursor, Path sidecarFile, DocIndexWriter docIndex)
            throws IOException {
        try (FileChannel sideCh = FileChannel.open(sidecarFile,
                StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING, StandardOpenOption.WRITE)) {
            if (!cursor.advance()) {
                // Empty stream — sidecar.dat is just touched (0 bytes).
                return;
            }
            DocPayloadWriter writer = new DocPayloadWriter(sideCh, docIndex);
            do {
                writer.accept(cursor.docId(), cursor.pos(), cursor.termId());
            } while (cursor.advance());
            writer.finish();
        }
    }

    /** Source of sorted {@code (docId, pos, termId)} triples consumed by {@link #streamSortedPairsToSidecar}. */
    private interface PairCursor {
        /** @return true if a triple was loaded and {@link #docId()}, {@link #pos()}, {@link #termId()} are valid. */
        boolean advance() throws IOException;
        int docId();
        int pos();
        int termId();
    }

    /** Pulls triples from the packed in-memory {@link #sortBuffer}. */
    private static final class InMemoryPairCursor implements PairCursor {
        private final byte[] buffer;
        private final int count;
        private int idx = -1;
        private int curDoc;
        private int curPos;
        private int curTermId;

        InMemoryPairCursor(byte[] buffer, int count) {
            this.buffer = buffer;
            this.count = count;
        }

        @Override
        public boolean advance() {
            idx++;
            if (idx >= count) return false;
            int off = idx * RECORD_BYTES;
            curDoc = readBE32(buffer, off);
            curPos = readBE32(buffer, off + 4);
            curTermId = readBE32(buffer, off + 8);
            return true;
        }

        @Override public int docId()  { return curDoc; }
        @Override public int pos()    { return curPos; }
        @Override public int termId() { return curTermId; }
    }

    /**
     * Manages the lifetime of {@link RunReader}s for the k-way merge so the caller can
     * own them via try-with-resources. Closes all readers on {@link #close()} — swallowed
     * close errors are logged at debug.
     */
    private static final class RunReaderBundle implements AutoCloseable {
        private final List<RunReader> readers;
        private final PriorityQueue<RunReader> heap;

        private RunReaderBundle(List<RunReader> readers, PriorityQueue<RunReader> heap) {
            this.readers = readers;
            this.heap = heap;
        }

        static RunReaderBundle open(List<Path> runs) throws IOException {
            List<RunReader> readers = new ArrayList<>(runs.size());
            PriorityQueue<RunReader> heap = new PriorityQueue<>(Math.max(1, runs.size()),
                (a, b) -> cmpPair(a.currentKey, a.currentTermId, b.currentKey, b.currentTermId));
            try {
                for (Path runPath : runs) {
                    RunReader rr = new RunReader(runPath);
                    readers.add(rr);
                    if (rr.advance()) heap.offer(rr);
                }
            } catch (IOException | RuntimeException e) {
                for (RunReader rr : readers) closeQuietly(rr);
                throw e;
            }
            return new RunReaderBundle(readers, heap);
        }

        PairCursor cursor() {
            return new HeapPairCursor(heap);
        }

        @Override
        public void close() {
            for (RunReader rr : readers) closeQuietly(rr);
        }
    }

    /** Pulls the next-smallest triple off the merge heap. */
    private static final class HeapPairCursor implements PairCursor {
        private final PriorityQueue<RunReader> heap;
        private int curDoc;
        private int curPos;
        private int curTermId;

        HeapPairCursor(PriorityQueue<RunReader> heap) {
            this.heap = heap;
        }

        @Override
        public boolean advance() throws IOException {
            if (heap.isEmpty()) return false;
            RunReader rr = heap.poll();
            curDoc = (int) (rr.currentKey >>> 32);
            curPos = (int) rr.currentKey;
            curTermId = rr.currentTermId;
            if (rr.advance()) heap.offer(rr);
            return true;
        }

        @Override public int docId()  { return curDoc; }
        @Override public int pos()    { return curPos; }
        @Override public int termId() { return curTermId; }
    }

    /**
     * Per-doc payload accumulator used by {@link #streamSortedPairsToSidecar}. Encapsulates
     * the buffer / doc-boundary / flushWriteBuffer state that would otherwise bloat the
     * caller's cognitive complexity.
     */
    private static final class DocPayloadWriter {
        private static final int INITIAL_PAYLOAD_BYTES = 64 * 1024;
        private static final int HEADER_BUF_BYTES = 16;
        private static final int OUT_BUF_BYTES = 64 * 1024;
        private static final int PAIR_WORST_CASE_BYTES = 10;

        private final FileChannel sideCh;
        private final DocIndexWriter docIndex;
        private final ByteBuffer headerBuf = ByteBuffer.allocate(HEADER_BUF_BYTES).order(ByteOrder.LITTLE_ENDIAN);
        private final ByteBuffer outBuf = ByteBuffer.allocate(OUT_BUF_BYTES).order(ByteOrder.LITTLE_ENDIAN);
        private ByteBuffer pairPayload = ByteBuffer.allocate(INITIAL_PAYLOAD_BYTES).order(ByteOrder.LITTLE_ENDIAN);
        private long sidecarOffset = 0;
        private int currentDoc = -1;
        private int pairCount = 0;
        private int prevPos = 0;

        DocPayloadWriter(FileChannel sideCh, DocIndexWriter docIndex) {
            this.sideCh = sideCh;
            this.docIndex = docIndex;
        }

        void accept(int docId, int pos, int termId) throws IOException {
            if (docId != currentDoc) {
                if (currentDoc >= 0) flushDoc();
                currentDoc = docId;
                pairCount = 0;
                prevPos = 0;
                pairPayload.clear();
            }
            if (pairPayload.remaining() < PAIR_WORST_CASE_BYTES) {
                pairPayload = growBuffer(pairPayload, PAIR_WORST_CASE_BYTES);
            }
            VarintCoder.writeUVInt(pairPayload, pos - prevPos);
            VarintCoder.writeUVInt(pairPayload, termId);
            prevPos = pos;
            pairCount++;
        }

        void finish() throws IOException {
            if (currentDoc >= 0) flushDoc();
            flushOutBuf();
        }

        /**
         * Flushes the accumulated pair payload for {@code currentDoc}: emits a doc-index entry,
         * writes the pair-count varint header, then streams the payload into {@code sidecar.dat}.
         * Small writes are staged through {@code outBuf}; writes larger than {@code outBuf}'s
         * remaining capacity flush it and go straight to the channel.
         * Advances {@code sidecarOffset} by the bytes written.
         */
        private void flushDoc() throws IOException {
            docIndex.emit(currentDoc, sidecarOffset);
            headerBuf.clear();
            VarintCoder.writeUVInt(headerBuf, pairCount);
            headerBuf.flip();
            int headerBytes = headerBuf.remaining();

            pairPayload.flip();
            int payloadBytes = pairPayload.remaining();
            putOrFlushAndWrite(headerBuf);
            putOrFlushAndWrite(pairPayload);
            sidecarOffset += headerBytes + payloadBytes;
        }

        /**
         * Puts {@code src} into {@code outBuf} if it fits; otherwise flushes {@code outBuf}
         * and, if {@code src} still exceeds {@code outBuf}'s capacity, writes {@code src}
         * directly to the channel. {@code src} is fully drained on return.
         */
        private void putOrFlushAndWrite(ByteBuffer src) throws IOException {
            if (src.remaining() <= outBuf.remaining()) {
                outBuf.put(src);
                return;
            }
            flushOutBuf();
            if (src.remaining() <= outBuf.capacity()) {
                outBuf.put(src);
            } else {
                while (src.hasRemaining()) sideCh.write(src);
            }
        }

        private void flushOutBuf() throws IOException {
            outBuf.flip();
            while (outBuf.hasRemaining()) sideCh.write(outBuf);
            outBuf.clear();
        }

        private static ByteBuffer growBuffer(ByteBuffer cur, int neededExtra) {
            int newCap = cur.capacity();
            int needed = cur.position() + neededExtra;
            while (newCap < needed) newCap <<= 1;
            ByteBuffer grown = ByteBuffer.allocate(newCap).order(cur.order());
            cur.flip();
            grown.put(cur);
            return grown;
        }
    }

    /**
     * Streams {@code doc-index.dat} as the merge produces docId boundaries, filling gaps
     * between emitted docIds (and the tail out to {@code maxDoc}) with
     * {@link #DOC_INDEX_NO_TOKENS}. Replaces the previous {@code long[maxDoc]} staging array,
     * so retained heap for the offset table becomes O(1) regardless of segment size.
     *
     * <p>Requires {@link #emit} to be called with strictly ascending {@code docId}. The k-way
     * merge and in-memory sort paths both honor that invariant because records are globally
     * sorted by {@code (docId, pos, termId)} before any doc boundary fires.
     */
    private static final class DocIndexWriter implements AutoCloseable {
        private final FileChannel channel;
        private final ByteBuffer buf;
        /** Next docId expected to be written — everything below is already flushed. */
        private int nextDoc = 0;

        DocIndexWriter(Path docIndexDst) throws IOException {
            this.channel = FileChannel.open(docIndexDst,
                StandardOpenOption.CREATE, StandardOpenOption.TRUNCATE_EXISTING, StandardOpenOption.WRITE);
            this.buf = ByteBuffer.allocate(8192).order(ByteOrder.LITTLE_ENDIAN);
        }

        /**
         * Record that {@code docId}'s payload starts at {@code sidecarOffset} in sidecar.dat.
         * Pads doc-index.dat with no-tokens sentinels for any intermediate docIds.
         */
        void emit(int docId, long sidecarOffset) throws IOException {
            if (docId < nextDoc) {
                throw new IllegalStateException(
                    "doc-index emit out of order: got " + docId + ", expected >= " + nextDoc);
            }
            // Fill sentinel entries for docs that had no tokens.
            while (nextDoc < docId) {
                putLong(DOC_INDEX_NO_TOKENS);
                nextDoc++;
            }
            putLong(sidecarOffset);
            nextDoc++;
        }

        /**
         * Pad out to {@code maxDoc} sentinels and flush. Must be called before {@link #close()}.
         */
        void finish(int maxDoc) throws IOException {
            while (nextDoc < maxDoc) {
                putLong(DOC_INDEX_NO_TOKENS);
                nextDoc++;
            }
            flush();
        }

        private void putLong(long v) throws IOException {
            if (!buf.hasRemaining()) flush();
            buf.putLong(v);
        }

        private void flush() throws IOException {
            buf.flip();
            while (buf.hasRemaining()) channel.write(buf);
            buf.clear();
        }

        @Override
        public void close() throws IOException {
            channel.close();
        }
    }

    @Override
    public void close() {
        if (closed) return;
        closeQuietly(termsOut);
        closeQuietly(termOffsetsOut);
        tryDelete(termsFile);
        tryDelete(termOffsetsFile);
        for (Path run : runs) tryDelete(run);
        runs.clear();
        closed = true;
    }

    private static void closeQuietly(AutoCloseable c) {
        if (c == null) return;
        try {
            c.close();
        } catch (Exception e) {
            log.debug("Ignored close error during SidecarBuilder abort: {}", e.toString());
        }
    }

    private static void tryDelete(Path p) {
        if (p == null) return;
        try {
            Files.deleteIfExists(p);
        } catch (IOException e) {
            log.warn("Failed to delete spill file {}: {}", p, e.toString());
        }
    }

    /**
     * Streaming reader over one sorted run file. Buffered reads; exposes current record's
     * packed key ({@code (docId << 32) | pos}) and termId for the merge heap.
     */
    private static final class RunReader implements AutoCloseable {
        private final FileChannel channel;
        private final ByteBuffer buf;
        private boolean eof;
        long currentKey;
        int currentTermId;

        RunReader(Path path) throws IOException {
            this.channel = FileChannel.open(path, StandardOpenOption.READ);
            // Buffer size chosen so total merge retained heap = numRuns × MERGE_READ_BUFFER_BYTES.
            this.buf = ByteBuffer.allocate(MERGE_READ_BUFFER_BYTES).order(ByteOrder.BIG_ENDIAN);
            this.buf.flip(); // start empty so first fill() triggers a read
        }

        /** Advance to the next record; returns false at end of file. */
        boolean advance() throws IOException {
            while (buf.remaining() < RECORD_BYTES) {
                if (eof) return false;
                buf.compact();
                int n = channel.read(buf);
                buf.flip();
                if (n < 0) eof = true;
            }
            int docId = buf.getInt();
            int pos = buf.getInt();
            int termId = buf.getInt();
            // Pack as unsigned 32-bit halves.
            currentKey = ((long) docId << 32) | (pos & 0xFFFFFFFFL);
            currentTermId = termId;
            return true;
        }

        @Override
        public void close() throws IOException {
            channel.close();
        }
    }
}
