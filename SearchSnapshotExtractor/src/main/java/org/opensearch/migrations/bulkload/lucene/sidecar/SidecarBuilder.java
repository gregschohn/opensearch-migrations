package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene10.org.apache.lucene.codecs.CodecUtil;
import shadow.lucene10.org.apache.lucene.store.ByteBuffersDataOutput;
import shadow.lucene10.org.apache.lucene.store.Directory;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexOutput;
import shadow.lucene10.org.apache.lucene.store.NIOFSDirectory;
import shadow.lucene10.org.apache.lucene.util.BytesRef;
import shadow.lucene10.org.apache.lucene.util.OfflineSorter;
import shadow.lucene10.org.apache.lucene.util.OfflineSorter.BufferSize;

/**
 * Builds the per-(segment, field) sidecar files by delegating the heavy lifting to
 * Lucene 10's {@link OfflineSorter} (external merge sort over a {@link Directory})
 * and emitting the final sidecar / doc-index / terms / term-offsets files through
 * Lucene {@link IndexOutput}s on the same {@link Directory}. This replaces the
 * previous hand-rolled spill/merge and raw {@code FileChannel}+{@code ByteBuffer}
 * plumbing with the canonical, battle-tested Lucene primitives.
 *
 * <p>Algorithm:
 * <ol>
 *   <li>{@link #registerTerm} streams term bytes into {@code terms.dat} as a
 *       VInt-length-prefixed record and writes the (LE long) byte-offset into
 *       {@code term-offsets.dat}. Term registration is term-major, matching
 *       {@code TermsEnum.next()} order.</li>
 *   <li>{@link #accept} writes each {@code (docId, pos, termId)} triple as a
 *       fixed 12-byte big-endian record into an OfflineSorter input file via
 *       {@link OfflineSorter.ByteSequencesWriter}. Big-endian packing makes the
 *       bytewise comparator sort naturally into {@code (docId ASC, pos ASC,
 *       termId ASC)}. Heap footprint during accept: one {@code byte[12]} scratch.</li>
 *   <li>{@link #buildAndOpenReader} closes the input writer (adding the codec
 *       footer OfflineSorter requires), runs {@link OfflineSorter#sort} which
 *       handles spilling, k-way merge, temp-file cleanup, and all corner cases
 *       for us, then streams the sorted records through
 *       {@link #streamSortedPairsToSidecar} to produce {@code sidecar.dat} and
 *       {@code doc-index.dat}.</li>
 * </ol>
 *
 * <p>Heap footprint is bounded by {@link BufferSize} passed to OfflineSorter
 * (default 256 MiB caller-side hint, clamped to Lucene's {@code MIN_BUFFER_SIZE_MB}
 * floor) regardless of total record count.
 */
@Slf4j
public final class SidecarBuilder implements PostingsSink, AutoCloseable {
    /** Fixed record width: docId(4) + pos(4) + termId(4), big-endian so bytewise sort matches. */
    public static final int RECORD_BYTES = 12;

    /** Sentinel written into doc-index.dat for docs that have no tokens in this field. */
    static final long DOC_INDEX_NO_TOKENS = -1L;

    /** File names — package-private so {@link SidecarReader} can find them. */
    static final String TERMS_FILE        = "terms.dat";
    static final String TERM_OFFSETS_FILE = "term-offsets.dat";
    static final String SIDECAR_FILE      = "sidecar.dat";
    static final String DOC_INDEX_FILE    = "doc-index.dat";

    /** Intermediate sort input name inside {@link #spillDir}. */
    private static final String SORT_INPUT_FILE = "sort-input.bin";

    /** Default caller-side hint for sort buffer size. OfflineSorter clamps to its own floor. */
    private static final int DEFAULT_SORT_BUFFER_MB = 256;

    private final Path spillDir;
    private final Directory dir;
    private final int maxDoc;
    private final BufferSize sortBufferSize;

    private final IndexOutput termsOut;
    private final IndexOutput termOffsetsOut;
    private final IndexOutput sortInputOut;

    /** Sequencer that appends length-prefixed byte records; here records are fixed-width. */
    private final OfflineSorter.ByteSequencesWriter sortInputWriter;

    /** Reusable 12-byte scratch for record emission. */
    private final byte[] recordScratch = new byte[RECORD_BYTES];

    private int nextTermId = 0;
    private long currentTermOffset = 0L;
    private boolean built = false;
    private boolean closed = false;
    private boolean sortInputClosed = false;

    public SidecarBuilder(Path spillDir, int sortBufferBytes, int maxDoc) throws IOException {
        this.spillDir = spillDir;
        this.maxDoc = Math.max(1, maxDoc);
        Files.createDirectories(spillDir);

        // NIOFSDirectory for temp + builder writes: we don't need mmap semantics here, just a
        // Directory that OfflineSorter can use for its spills. SidecarReader will reopen with
        // MMapDirectory for the read side.
        this.dir = new NIOFSDirectory(spillDir);

        int mb = sortBufferBytes > 0 ? (sortBufferBytes >>> 20) : DEFAULT_SORT_BUFFER_MB;
        // Clamp to OfflineSorter's MIN_BUFFER_SIZE_MB floor; automatic() also works but this
        // gives deterministic behavior for tests.
        mb = (int) Math.max((long) OfflineSorter.MIN_BUFFER_SIZE_MB, (long) mb);
        this.sortBufferSize = BufferSize.megabytes(mb);

        this.termsOut        = dir.createOutput(TERMS_FILE, IOContext.DEFAULT);
        this.termOffsetsOut  = dir.createOutput(TERM_OFFSETS_FILE, IOContext.DEFAULT);
        this.sortInputOut    = dir.createOutput(SORT_INPUT_FILE, IOContext.DEFAULT);
        this.sortInputWriter = new OfflineSorter.ByteSequencesWriter(sortInputOut);
    }

    @Override
    public int registerTerm(BytesRefLike term) throws IOException {
        int id = nextTermId++;
        termOffsetsOut.writeLong(currentTermOffset);
        int len = term.length();
        termsOut.writeVInt(len);
        termsOut.writeBytes(term.bytes(), term.offset(), len);
        // VInt length cost + payload cost.
        currentTermOffset += vintLen(len) + (long) len;
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

        BytesRef br = new BytesRef(rec, 0, RECORD_BYTES);
        for (int i = 0; i < positionCount; i++) {
            writeBE32(rec, 4, positions[i]);
            sortInputWriter.write(br);
        }
    }

    public SidecarReader buildAndOpenReader() throws IOException {
        if (built) throw new IllegalStateException("SidecarBuilder.buildAndOpenReader already called");
        built = true;

        termsOut.close();
        termOffsetsOut.close();

        // OfflineSorter validates a codec footer on the input file. ByteSequencesWriter does
        // NOT write one; we call CodecUtil.writeFooter before closing the input.
        CodecUtil.writeFooter(sortInputOut);
        sortInputWriter.close();
        sortInputClosed = true;

        // Bytewise comparator on our BE-packed (docId, pos, termId) records sorts naturally
        // into (docId ASC, pos ASC, termId ASC) under unsigned-byte comparison (all three
        // fields are non-negative ints, so sign-bit interference is moot).
        OfflineSorter sorter = new OfflineSorter(
            dir,
            "sort",
            Comparator.naturalOrder(), // BytesRef natural order = unsigned bytewise
            sortBufferSize,
            OfflineSorter.MAX_TEMPFILES,
            RECORD_BYTES, // fixed valueLength → OfflineSorter drops the 2-byte length prefix on spills
            null,
            0
        );
        String sortedName = sorter.sort(SORT_INPUT_FILE);

        try {
            streamSortedPairsToSidecar(sortedName);
        } finally {
            // OfflineSorter deletes its own temp files but leaves the named input+output.
            dir.deleteFile(SORT_INPUT_FILE);
            dir.deleteFile(sortedName);
        }

        closed = true;
        return SidecarReader.open(spillDir, maxDoc, nextTermId);
    }

    /**
     * Streams the sorted record file through {@link DocPayloadEmitter}, emitting one doc's
     * payload at a time into {@code sidecar.dat} and the parallel {@code doc-index.dat}.
     */
    private void streamSortedPairsToSidecar(String sortedName) throws IOException {
        try (IndexOutput sideOut     = dir.createOutput(SIDECAR_FILE, IOContext.DEFAULT);
             IndexOutput docIndexOut = dir.createOutput(DOC_INDEX_FILE, IOContext.DEFAULT);
             OfflineSorter.ByteSequencesReader reader = new OfflineSorter.ByteSequencesReader(
                 dir.openChecksumInput(sortedName),
                 sortedName)) {

            DocPayloadEmitter emitter = new DocPayloadEmitter(sideOut, docIndexOut);
            BytesRef rec;
            while ((rec = reader.next()) != null) {
                if (rec.length != RECORD_BYTES) {
                    throw new IOException("Sorted record has unexpected length " + rec.length);
                }
                int docId  = readBE32(rec.bytes, rec.offset);
                int pos    = readBE32(rec.bytes, rec.offset + 4);
                int termId = readBE32(rec.bytes, rec.offset + 8);
                emitter.accept(docId, pos, termId);
            }
            emitter.finish(maxDoc);
        }
    }

    /**
     * Accumulates one doc's (pos-delta, termId) VInt pairs in a {@link ByteBuffersDataOutput}
     * so we can write the {@code pairCount} header before the payload — Lucene's native
     * in-memory staging buffer grows lazily and exposes {@link ByteBuffersDataOutput#copyTo}
     * to stream into the backing {@link IndexOutput} without a byte[] detour.
     *
     * <p>Also emits {@code doc-index.dat} inline: one LE long per doc in {@code [0, maxDoc)},
     * either the absolute offset of the doc's payload in {@code sidecar.dat} or
     * {@link #DOC_INDEX_NO_TOKENS} for docs with no tokens in this field.
     */
    private static final class DocPayloadEmitter {
        private final IndexOutput sideOut;
        private final IndexOutput docIndexOut;
        private final ByteBuffersDataOutput staging = ByteBuffersDataOutput.newResettableInstance();
        private int currentDoc = -1;
        private int pairCount = 0;
        private int prevPos = 0;
        private int nextDoc = 0;

        DocPayloadEmitter(IndexOutput sideOut, IndexOutput docIndexOut) {
            this.sideOut = sideOut;
            this.docIndexOut = docIndexOut;
        }

        void accept(int docId, int pos, int termId) throws IOException {
            if (docId != currentDoc) {
                if (currentDoc >= 0) flushDoc();
                currentDoc = docId;
                pairCount = 0;
                prevPos = 0;
                staging.reset();
            }
            staging.writeVInt(pos - prevPos);
            staging.writeVInt(termId);
            prevPos = pos;
            pairCount++;
        }

        void finish(int maxDoc) throws IOException {
            if (currentDoc >= 0) flushDoc();
            // Pad the tail of doc-index with sentinels so SidecarReader can index it
            // directly as a long[maxDoc] randomAccessSlice.
            while (nextDoc < maxDoc) {
                docIndexOut.writeLong(DOC_INDEX_NO_TOKENS);
                nextDoc++;
            }
        }

        private void flushDoc() throws IOException {
            // Pad gaps (docs with no tokens) with sentinels up to currentDoc.
            while (nextDoc < currentDoc) {
                docIndexOut.writeLong(DOC_INDEX_NO_TOKENS);
                nextDoc++;
            }
            if (currentDoc < nextDoc) {
                throw new IllegalStateException(
                    "doc-index emit out of order: got " + currentDoc + ", expected >= " + nextDoc);
            }
            long off = sideOut.getFilePointer();
            docIndexOut.writeLong(off);
            nextDoc = currentDoc + 1;

            sideOut.writeVInt(pairCount);
            staging.copyTo(sideOut);
        }
    }

    /** Returns the number of bytes {@link IndexOutput#writeVInt} uses for {@code v ≥ 0}. */
    private static int vintLen(int v) {
        int n = 1;
        while ((v & ~0x7F) != 0) { n++; v >>>= 7; }
        return n;
    }

    private static void writeBE32(byte[] b, int off, int v) {
        b[off]     = (byte) (v >>> 24);
        b[off + 1] = (byte) (v >>> 16);
        b[off + 2] = (byte) (v >>> 8);
        b[off + 3] = (byte) v;
    }

    private static int readBE32(byte[] b, int off) {
        return ((b[off] & 0xFF) << 24)
             | ((b[off + 1] & 0xFF) << 16)
             | ((b[off + 2] & 0xFF) << 8)
             |  (b[off + 3] & 0xFF);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        closeQuietly(termsOut);
        closeQuietly(termOffsetsOut);
        if (!sortInputClosed) {
            closeQuietly(sortInputWriter);
            sortInputClosed = true;
        }
        try {
            dir.close();
        } catch (IOException e) {
            log.debug("Ignored Directory close error during SidecarBuilder abort: {}", e.toString());
        }
        // Best-effort temp cleanup: the reader has already re-opened the 4 output files via
        // its own MMapDirectory, so removing the sort-input scratch is safe whether or not
        // buildAndOpenReader() completed.
        try {
            Files.deleteIfExists(spillDir.resolve(SORT_INPUT_FILE));
        } catch (IOException e) {
            log.debug("Ignored sort-input delete error: {}", e.toString());
        }
    }

    private static void closeQuietly(AutoCloseable c) {
        if (c == null) return;
        try {
            c.close();
        } catch (Exception e) {
            log.debug("Ignored close error during SidecarBuilder abort: {}", e.toString());
        }
    }
}
