package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.MMapDirectory;
import shadow.lucene10.org.apache.lucene.store.RandomAccessInput;

/**
 * Reads a sidecar built by {@link SidecarBuilder}. Answers {@code get(docId) -> List<String>}
 * using {@link MMapDirectory}-backed {@link IndexInput}s with zero heap allocation beyond
 * the returned {@link ArrayList}.
 *
 * <p>File layout (all files live in a per-(segment, field) spill directory):
 * <pre>
 *   terms.dat          - sequence of VInt(length) | bytes[length] records, one per termId
 *   term-offsets.dat   - little-endian long[numTerms] of byte offsets into terms.dat
 *   sidecar.dat        - for each doc that has tokens, a VInt(numEntries) header followed by
 *                        numEntries pairs of (VInt positionDelta, VInt termId)
 *   doc-index.dat      - little-endian long[maxDoc] of absolute offsets into sidecar.dat,
 *                        or {@link SidecarBuilder#DOC_INDEX_NO_TOKENS} for docs with no tokens
 * </pre>
 *
 * <p>The four backing files are opened via {@link MMapDirectory}, which natively handles
 * files exceeding the 2 GiB single-mmap limit (chunked mmap under the hood). The two long[]
 * lookup tables ({@code term-offsets.dat}, {@code doc-index.dat}) are accessed through
 * {@link RandomAccessInput#readLong(long)} for positional reads without a sequential seek.
 *
 * <p>Heap footprint per open reader: one {@link MMapDirectory}, four {@link IndexInput}
 * handles, and two {@link RandomAccessInput} views. The OS pages in what {@link #get}
 * touches; no heap-resident offset tables.
 *
 * <p>Thread-safety: {@link #get(int)} clones the per-doc {@link IndexInput}s so concurrent
 * readers each get their own position state. {@link RandomAccessInput} positional reads do
 * not mutate shared state. Opening and closing are not concurrent-safe.
 */
@Slf4j
public final class SidecarReader implements AutoCloseable {

    private final Path spillDir;
    private final int maxDoc;
    private final int numTerms;

    private final MMapDirectory dir;
    private final IndexInput termsInput;
    private final IndexInput termOffsetsInput;
    private final IndexInput sidecarInput;
    private final IndexInput docIndexInput;
    private final RandomAccessInput termOffsetsRA;
    private final RandomAccessInput docIndexRA;

    private volatile boolean closed;

    private SidecarReader(Path spillDir,
                          int maxDoc,
                          int numTerms,
                          MMapDirectory dir,
                          IndexInput termsInput,
                          IndexInput termOffsetsInput,
                          IndexInput sidecarInput,
                          IndexInput docIndexInput,
                          RandomAccessInput termOffsetsRA,
                          RandomAccessInput docIndexRA) {
        this.spillDir = spillDir;
        this.maxDoc = maxDoc;
        this.numTerms = numTerms;
        this.dir = dir;
        this.termsInput = termsInput;
        this.termOffsetsInput = termOffsetsInput;
        this.sidecarInput = sidecarInput;
        this.docIndexInput = docIndexInput;
        this.termOffsetsRA = termOffsetsRA;
        this.docIndexRA = docIndexRA;
    }

    /**
     * Opens a sidecar previously produced by {@link SidecarBuilder#buildAndOpenReader()}
     * in {@code spillDir}. Uses {@link MMapDirectory} for transparent >2GiB file support.
     */
    static SidecarReader open(Path spillDir, int maxDoc, int numTerms) throws IOException {
        MMapDirectory dir = new MMapDirectory(spillDir);
        IndexInput termsInput = null;
        IndexInput termOffsetsInput = null;
        IndexInput sidecarInput = null;
        IndexInput docIndexInput = null;
        try {
            termsInput       = dir.openInput(SidecarBuilder.TERMS_FILE,        IOContext.DEFAULT);
            termOffsetsInput = dir.openInput(SidecarBuilder.TERM_OFFSETS_FILE, IOContext.DEFAULT);
            sidecarInput     = dir.openInput(SidecarBuilder.SIDECAR_FILE,      IOContext.DEFAULT);
            docIndexInput    = dir.openInput(SidecarBuilder.DOC_INDEX_FILE,    IOContext.DEFAULT);

            RandomAccessInput termOffsetsRA = termOffsetsInput.randomAccessSlice(0L, termOffsetsInput.length());
            RandomAccessInput docIndexRA    = docIndexInput.randomAccessSlice(0L, docIndexInput.length());

            return new SidecarReader(spillDir, maxDoc, numTerms, dir,
                    termsInput, termOffsetsInput, sidecarInput, docIndexInput,
                    termOffsetsRA, docIndexRA);
        } catch (Throwable t) {
            closeQuietly(termsInput);
            closeQuietly(termOffsetsInput);
            closeQuietly(sidecarInput);
            closeQuietly(docIndexInput);
            closeQuietly(dir);
            throw t;
        }
    }

    /**
     * Returns the position-ordered list of terms for {@code docId}, or an empty list
     * if the doc has no tokens in this field (or {@code docId} is out of range).
     */
    public List<String> get(int docId) throws IOException {
        if (closed) throw new IOException("SidecarReader has been closed");
        if (docId < 0 || docId >= maxDoc) return Collections.emptyList();
        long offset = docIndexRA.readLong((long) docId * 8L);
        if (offset == SidecarBuilder.DOC_INDEX_NO_TOKENS) return Collections.emptyList();
        if (offset < 0) {
            throw new IOException("Sidecar offset out of range: " + offset);
        }
        // Fresh clone per call gives concurrent readers their own position state.
        IndexInput in = sidecarInput.clone();
        in.seek(offset);
        int numEntries = in.readVInt();
        List<String> result = new ArrayList<>(numEntries);
        for (int i = 0; i < numEntries; i++) {
            // Positions are delta-encoded in the on-disk format; readers only return
            // term strings, relying on list iteration order to reconstruct positional order.
            in.readVInt();
            int termId = in.readVInt();
            result.add(readTerm(termId));
        }
        return result;
    }

    private String readTerm(int termId) throws IOException {
        if (termId < 0 || termId >= numTerms) {
            throw new IOException("termId out of range: " + termId + " (numTerms=" + numTerms + ")");
        }
        long termOffset = termOffsetsRA.readLong((long) termId * 8L);
        IndexInput in = termsInput.clone();
        in.seek(termOffset);
        int len = in.readVInt();
        byte[] bytes = new byte[len];
        in.readBytes(bytes, 0, len);
        return new String(bytes, java.nio.charset.StandardCharsets.UTF_8);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        // RandomAccessInput views don't own channels; the backing IndexInputs do.
        closeQuietly(termsInput);
        closeQuietly(termOffsetsInput);
        closeQuietly(sidecarInput);
        closeQuietly(docIndexInput);
        closeQuietly(dir);
    }

    private static void closeQuietly(AutoCloseable c) {
        if (c == null) return;
        try {
            c.close();
        } catch (Exception e) {
            log.debug("Ignored close error in SidecarReader: {}", e.toString());
        }
    }

    Path spillDir() { return spillDir; }
}
