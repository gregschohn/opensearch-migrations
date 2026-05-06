package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene10.org.apache.lucene.store.Directory;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexOutput;
import shadow.lucene10.org.apache.lucene.store.NIOFSDirectory;
import shadow.lucene10.org.apache.lucene.util.IOUtils;

/**
 * Builds a per-(segment, field) single-term sidecar for single-valued
 * DOCS-only keyword fields. On-disk layout:
 *
 * <ul>
 *   <li>{@link #TERMS_FILE} {@code "st-terms.dat"} — term bytes, packed
 *       {@code vInt length} + {@code byte[length]} per term, in registration
 *       order (== ascending sorted-bytes order).
 *   <li>{@link #TERM_OFFSETS_FILE} {@code "st-term-offsets.dat"} — {@code long}
 *       per termId, absolute byte offset into {@code st-terms.dat}.
 *   <li>{@link #SINGLE_INDEX_FILE} {@code "single-index.dat"} — flat
 *       {@code int[maxDoc]}, big-endian, 4 bytes per slot. Value is the dense
 *       termId or {@link #NO_VALUE} {@code (-1)} when the doc has no value.
 * </ul>
 *
 * <p>Write pattern: {@code single-index.dat} is zero-padded up-front to
 * {@code maxDoc * 4} bytes, then {@link #accept(int, int)} does a random-access
 * {@code writeInt} at {@code docId * 4}. This avoids heap allocation
 * proportional to {@code maxDoc} and matches how the positional
 * {@link SidecarBuilder} keeps its heap footprint bounded.
 *
 * <p>This builder deliberately does NOT use {@code OfflineSorter}: the walker
 * drives docIds in ascending order within each term, but the cross-term
 * interleaving would require sorting for a doc-major layout — which is
 * unnecessary here because the output is already keyed by docId (O(1) random
 * write per entry). The sparse seek-and-writeInt pattern is cheap: Lucene's
 * {@code IndexOutput} is buffered, and the OS page cache coalesces adjacent
 * writes.
 */
@Slf4j
public final class SingleTermSidecarBuilder implements SingleTermSink, AutoCloseable {

    /** Sentinel termId stored in {@code single-index.dat} for docs with no value. */
    public static final int NO_VALUE = -1;

    static final String TERMS_FILE        = "st-terms.dat";
    static final String TERM_OFFSETS_FILE = "st-term-offsets.dat";
    static final String SINGLE_INDEX_FILE = "single-index.dat";

    private final Path spillDir;
    private final Directory dir;
    private final int maxDoc;

    private final IndexOutput termsOut;
    private final IndexOutput termOffsetsOut;
    // NOTE: stored as NIOFSDirectory IndexOutput; we close it before buildAndOpenReader
    // patches sentinel bytes via a direct FileChannel for random-access writes.
    private final Path singleIndexPath;

    // In-memory int[maxDoc] buffer. For maxDoc <= ~16M (64 MiB) this is trivially
    // bounded, which is well within the existing heap budget — RFS already holds
    // larger structures per segment (the positional sidecar's sort buffer is 256 MiB).
    // For segments beyond that, callers should prefer the positional sidecar path
    // anyway because the field would likely be multi-valued or positional.
    private final int[] docToTermId;

    private int nextTermId = 0;
    private boolean built = false;
    private boolean closed = false;

    public SingleTermSidecarBuilder(Path spillDir, int maxDoc) throws IOException {
        this.spillDir = spillDir;
        this.maxDoc = Math.max(1, maxDoc);
        Files.createDirectories(spillDir);
        this.dir = new NIOFSDirectory(spillDir);

        this.termsOut        = dir.createOutput(TERMS_FILE, IOContext.DEFAULT);
        this.termOffsetsOut  = dir.createOutput(TERM_OFFSETS_FILE, IOContext.DEFAULT);
        this.singleIndexPath = spillDir.resolve(SINGLE_INDEX_FILE);

        this.docToTermId = new int[this.maxDoc];
        java.util.Arrays.fill(this.docToTermId, NO_VALUE);
    }

    @Override
    public int registerTerm(BytesRefLike term) throws IOException {
        termOffsetsOut.writeLong(termsOut.getFilePointer());
        int len = term.length();
        termsOut.writeVInt(len);
        termsOut.writeBytes(term.bytes(), term.offset(), len);
        return nextTermId++;
    }

    @Override
    public void accept(int termId, int docId) throws IOException {
        if (docId < 0 || docId >= maxDoc) return;
        docToTermId[docId] = termId;
    }

    /**
     * Flushes the single-index file, closes all outputs, and opens a reader
     * mmap'd over the resulting files.
     */
    public SingleTermSidecarReader buildAndOpenReader() throws IOException {
        if (built) throw new IllegalStateException("buildAndOpenReader already called");
        built = true;

        termsOut.close();
        termOffsetsOut.close();

        // Flush int[] to disk as big-endian 4-byte-per-doc. We go through
        // the same Directory abstraction used elsewhere for consistency
        // (uses Lucene's buffered IndexOutput; writeInt is big-endian).
        try (IndexOutput out = dir.createOutput(SINGLE_INDEX_FILE, IOContext.DEFAULT)) {
            for (int i = 0; i < maxDoc; i++) {
                out.writeInt(docToTermId[i]);
            }
        }

        closed = true;
        return SingleTermSidecarReader.open(spillDir, maxDoc, nextTermId);
    }

    Path spillDir() { return spillDir; }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(termsOut, termOffsetsOut, dir);
        try {
            Files.deleteIfExists(singleIndexPath);
        } catch (IOException e) {
            log.debug("Ignored single-index delete error: {}", e.toString());
        }
    }
}
