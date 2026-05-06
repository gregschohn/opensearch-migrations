package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;

import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.MMapDirectory;
import shadow.lucene10.org.apache.lucene.store.RandomAccessInput;
import shadow.lucene10.org.apache.lucene.util.IOUtils;

/**
 * Reads a sidecar built by {@link SingleTermSidecarBuilder}. MMap-backed; {@link #get(int)}
 * is a single {@code readInt} over the flat {@code int[maxDoc]} plus a term-dict
 * lookup on the first access per term (no per-term caching — we rely on the
 * OS page cache).
 *
 * <p>Not thread-safe for concurrent {@link #get(int)} calls on the same instance:
 * {@code termsInput} is cloned per lookup, but the resolved-term allocation is
 * per-call anyway so the only hot path cost is two random-access reads
 * ({@code docToTermRA} and {@code termOffsetsRA}).
 */
public final class SingleTermSidecarReader implements AutoCloseable {

    private final Path spillDir;
    private final int maxDoc;
    private final int numTerms;

    private final MMapDirectory dir;
    private final IndexInput termsInput;
    private final IndexInput termOffsetsInput;
    private final IndexInput singleIndexInput;
    private final RandomAccessInput termOffsetsRA;
    private final RandomAccessInput singleIndexRA;

    private volatile boolean closed;

    private SingleTermSidecarReader(Path spillDir, int maxDoc, int numTerms, MMapDirectory dir,
                                    IndexInput termsInput, IndexInput termOffsetsInput,
                                    IndexInput singleIndexInput,
                                    RandomAccessInput termOffsetsRA,
                                    RandomAccessInput singleIndexRA) {
        this.spillDir = spillDir;
        this.maxDoc = maxDoc;
        this.numTerms = numTerms;
        this.dir = dir;
        this.termsInput = termsInput;
        this.termOffsetsInput = termOffsetsInput;
        this.singleIndexInput = singleIndexInput;
        this.termOffsetsRA = termOffsetsRA;
        this.singleIndexRA = singleIndexRA;
    }

    static SingleTermSidecarReader open(Path spillDir, int maxDoc, int numTerms) throws IOException {
        MMapDirectory dir = new MMapDirectory(spillDir);
        IndexInput terms = null;
        IndexInput termOffsets = null;
        IndexInput singleIndex = null;
        try {
            terms       = dir.openInput(SingleTermSidecarBuilder.TERMS_FILE,        IOContext.DEFAULT);
            termOffsets = dir.openInput(SingleTermSidecarBuilder.TERM_OFFSETS_FILE, IOContext.DEFAULT);
            singleIndex = dir.openInput(SingleTermSidecarBuilder.SINGLE_INDEX_FILE, IOContext.DEFAULT);
            return new SingleTermSidecarReader(spillDir, maxDoc, numTerms, dir, terms, termOffsets, singleIndex,
                    termOffsets.randomAccessSlice(0L, termOffsets.length()),
                    singleIndex.randomAccessSlice(0L, singleIndex.length()));
        } catch (Throwable t) {
            IOUtils.closeWhileHandlingException(terms, termOffsets, singleIndex, dir);
            throw t;
        }
    }

    /**
     * Returns the single term string for {@code docId}, or {@code null} when
     * the doc has no value (sentinel {@link SingleTermSidecarBuilder#NO_VALUE}).
     */
    public String get(int docId) throws IOException {
        if (closed) throw new IOException("SingleTermSidecarReader closed");
        if (docId < 0 || docId >= maxDoc) return null;
        int termId = singleIndexRA.readInt(docId * 4L);
        if (termId == SingleTermSidecarBuilder.NO_VALUE) return null;
        if (termId < 0 || termId >= numTerms) {
            throw new IOException("termId out of range: " + termId + " (numTerms=" + numTerms + ")");
        }
        IndexInput in = termsInput.clone();
        in.seek(termOffsetsRA.readLong(termId * 8L));
        byte[] bytes = new byte[in.readVInt()];
        in.readBytes(bytes, 0, bytes.length);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(termsInput, termOffsetsInput, singleIndexInput, dir);
    }

    Path spillDir() { return spillDir; }
    public int maxDoc() { return maxDoc; }
    public int numTerms() { return numTerms; }
}
