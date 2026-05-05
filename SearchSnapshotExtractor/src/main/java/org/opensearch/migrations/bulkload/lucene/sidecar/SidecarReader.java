package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

import lombok.extern.slf4j.Slf4j;

/**
 * Reads a sidecar built by {@link SidecarBuilder}. Answers {@code get(docId) -> List<String>}
 * using mmap'd lookups with zero heap allocation beyond the returned {@code ArrayList}.
 *
 * <p>File layout (all files live in a per-(segment, field) spill directory):
 * <pre>
 *   terms.dat          - sequence of int32 length | bytes[length] records, one per termId
 *   term-offsets.dat   - little-endian int[numTerms] of byte offsets into terms.dat
 *   sidecar.dat        - for each doc that has tokens, a uvint(numEntries) header followed by
 *                        numEntries pairs of (uvint positionDelta, uvint termId)
 *   doc-index.dat      - little-endian long[maxDoc] of absolute offsets into sidecar.dat,
 *                        or SidecarBuilder.DOC_INDEX_NO_TOKENS for docs with no tokens
 * </pre>
 *
 * <p>The four mmap views are wrapped in {@link ChunkedByteBuffer} so that files larger
 * than {@link Integer#MAX_VALUE} bytes (which {@code FileChannel.map} rejects in one call)
 * are transparently mapped as a series of chunks. Files ≤1 GiB still take a single-mmap
 * fast path.
 *
 * <p>Heap footprint per open reader:
 *   <ul>
 *     <li>A mmap'd view over each of the four files. The OS pages in what the access
 *         pattern touches; heap is a handful of direct-buffer wrappers.
 *     <li>No long[] docOffsets in heap - doc offsets live in doc-index.dat. For a 600 GB
 *         shard with 20M docs that's 160 MB of OS page cache, not 160 MB heap.
 *     <li>No int[] termOffsets in heap - term offsets live in term-offsets.dat.
 *     <li>Term bytes read on demand; one small reusable byte[] per {@link #get} call is
 *         the only transient heap.
 *   </ul>
 *
 * <p>Thread-safety: {@link #get(int)} allocates a fresh {@link ChunkedByteBuffer.Cursor}
 * on each call so concurrent readers each get their own position state. Positional
 * {@code getInt}/{@code getLong} on the shared {@link ChunkedByteBuffer} do not mutate
 * any shared state. Opening and closing are not concurrent-safe.
 */
@Slf4j
public final class SidecarReader implements AutoCloseable {

    private final Path spillDir;
    private final int maxDoc;
    private final int numTerms;

    private final FileChannel termsChannel;
    private final FileChannel termOffsetsChannel;
    private final FileChannel sidecarChannel;
    private final FileChannel docIndexChannel;

    private final ChunkedByteBuffer termsBuf;
    private final ChunkedByteBuffer termOffsetsBuf;  // little-endian int[numTerms]
    private final ChunkedByteBuffer sidecarBuf;
    private final ChunkedByteBuffer docIndexBuf;     // little-endian long[maxDoc]

    private volatile boolean closed;

    private SidecarReader(Path spillDir,
                          int maxDoc,
                          int numTerms,
                          FileChannel termsChannel,
                          FileChannel termOffsetsChannel,
                          FileChannel sidecarChannel,
                          FileChannel docIndexChannel,
                          ChunkedByteBuffer termsBuf,
                          ChunkedByteBuffer termOffsetsBuf,
                          ChunkedByteBuffer sidecarBuf,
                          ChunkedByteBuffer docIndexBuf) {
        this.spillDir = spillDir;
        this.maxDoc = maxDoc;
        this.numTerms = numTerms;
        this.termsChannel = termsChannel;
        this.termOffsetsChannel = termOffsetsChannel;
        this.sidecarChannel = sidecarChannel;
        this.docIndexChannel = docIndexChannel;
        this.termsBuf = termsBuf;
        this.termOffsetsBuf = termOffsetsBuf;
        this.sidecarBuf = sidecarBuf;
        this.docIndexBuf = docIndexBuf;
    }

    /**
     * Opens a sidecar previously produced by {@link SidecarBuilder#buildAndOpenReader()}
     * in {@code spillDir}. mmaps all four files (chunked if any exceeds the 2 GiB single-mmap
     * limit) and caches their endian-configured views.
     */
    static SidecarReader open(Path spillDir, int maxDoc, int numTerms) throws IOException {
        return open(spillDir, maxDoc, numTerms, ChunkedByteBuffer.DEFAULT_CHUNK_SIZE);
    }

    /**
     * Package-private variant that accepts a custom chunk size. Exposed so chunk-boundary
     * tests can force small chunks without fabricating multi-GiB fixtures.
     */
    static SidecarReader open(Path spillDir, int maxDoc, int numTerms, long chunkSize) throws IOException {
        Path termsFile        = spillDir.resolve(SidecarBuilder.TERMS_FILE);
        Path termOffsetsFile  = spillDir.resolve(SidecarBuilder.TERM_OFFSETS_FILE);
        Path sidecarFile      = spillDir.resolve(SidecarBuilder.SIDECAR_FILE);
        Path docIndexFile     = spillDir.resolve(SidecarBuilder.DOC_INDEX_FILE);

        FileChannel termsCh = FileChannel.open(termsFile, StandardOpenOption.READ);
        FileChannel termOffsetsCh = FileChannel.open(termOffsetsFile, StandardOpenOption.READ);
        FileChannel sidecarCh = FileChannel.open(sidecarFile, StandardOpenOption.READ);
        FileChannel docIndexCh = FileChannel.open(docIndexFile, StandardOpenOption.READ);

        // terms.dat is length-prefixed with big-endian int32 (DataOutputStream.writeInt).
        ChunkedByteBuffer termsBuf = ChunkedByteBuffer.open(termsCh, chunkSize);
        termsBuf.order(ByteOrder.BIG_ENDIAN);

        ChunkedByteBuffer termOffsetsBuf = ChunkedByteBuffer.open(termOffsetsCh, chunkSize);
        termOffsetsBuf.order(ByteOrder.LITTLE_ENDIAN);

        ChunkedByteBuffer sidecarBuf = ChunkedByteBuffer.open(sidecarCh, chunkSize);
        sidecarBuf.order(ByteOrder.LITTLE_ENDIAN);

        ChunkedByteBuffer docIndexBuf = ChunkedByteBuffer.open(docIndexCh, chunkSize);
        docIndexBuf.order(ByteOrder.LITTLE_ENDIAN);

        return new SidecarReader(spillDir, maxDoc, numTerms,
                termsCh, termOffsetsCh, sidecarCh, docIndexCh,
                termsBuf, termOffsetsBuf, sidecarBuf, docIndexBuf);
    }

    /**
     * Returns the position-ordered list of terms for {@code docId}, or an empty list
     * if the doc has no tokens in this field (or {@code docId} is out of range).
     */
    public List<String> get(int docId) throws IOException {
        if (closed) throw new IOException("SidecarReader has been closed");
        if (docId < 0 || docId >= maxDoc) return Collections.emptyList();
        long offset = readDocOffset(docId);
        if (offset == SidecarBuilder.DOC_INDEX_NO_TOKENS) return Collections.emptyList();
        if (offset < 0) {
            throw new IOException("Sidecar offset out of range: " + offset);
        }
        // Fresh cursor per call gives concurrent readers their own position state.
        ChunkedByteBuffer.Cursor cur = sidecarBuf.cursor(offset);
        int numEntries = VarintCoder.readUVInt(cur);
        List<String> result = new ArrayList<>(numEntries);
        for (int i = 0; i < numEntries; i++) {
            // Positions are delta-encoded in the on-disk format, but this reader returns
            // only the term strings for each token — callers use the list's iteration
            // order to reconstruct positional order. Skip the position delta.
            VarintCoder.readUVInt(cur);
            int termId = VarintCoder.readUVInt(cur);
            result.add(readTerm(termId));
        }
        return result;
    }

    private long readDocOffset(int docId) {
        // doc-index.dat is long[maxDoc] little-endian.
        return docIndexBuf.getLong((long) docId * 8);
    }

    private String readTerm(int termId) throws IOException {
        if (termId < 0 || termId >= numTerms) {
            throw new IOException("termId out of range: " + termId + " (numTerms=" + numTerms + ")");
        }
        int termOffset = termOffsetsBuf.getInt((long) termId * 4);
        ChunkedByteBuffer.Cursor cur = termsBuf.cursor(termOffset);
        int len = cur.readInt();
        byte[] bytes = new byte[len];
        cur.readBytes(bytes);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        closeQuietly(termsChannel);
        closeQuietly(termOffsetsChannel);
        closeQuietly(sidecarChannel);
        closeQuietly(docIndexChannel);
    }

    private static void closeQuietly(java.io.Closeable c) {
        if (c == null) return;
        try {
            c.close();
        } catch (IOException e) {
            // Best-effort close on a mmap channel — the backing file is owned by this
            // reader and is about to be unmapped regardless.
            log.debug("Ignored close error in SidecarReader: {}", e.toString());
        }
    }

    Path spillDir() { return spillDir; }
}
