package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene10.org.apache.lucene.codecs.CodecUtil;
import shadow.lucene10.org.apache.lucene.codecs.lucene90.IndexedDISI;
import shadow.lucene10.org.apache.lucene.store.Directory;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.IndexOutput;
import shadow.lucene10.org.apache.lucene.store.NIOFSDirectory;
import shadow.lucene10.org.apache.lucene.util.BitSetIterator;
import shadow.lucene10.org.apache.lucene.util.FixedBitSet;
import shadow.lucene10.org.apache.lucene.util.IOUtils;
import shadow.lucene10.org.apache.lucene.util.packed.DirectMonotonicWriter;
import shadow.lucene10.org.apache.lucene.util.packed.DirectWriter;

/**
 * Builds a single-container sidecar for single-valued DOCS-only fields. Layout:
 *
 * <ul>
 *   <li>{@link CodecUtil} index header
 *   <li><b>terms</b> — {@code vInt(len) + UTF-8 bytes} per distinct term
 *   <li><b>term offsets</b> — {@link DirectMonotonicWriter} over term byte offsets
 *   <li><b>has-value DISI</b> — {@link IndexedDISI} bitmap of docs that carry a term
 *   <li><b>term-id values</b> — {@link DirectWriter} bit-packed termIds, one per present
 *       doc, indexed by DISI ordinal. At {@code numTerms} distinct terms, we use
 *       {@code ceil(log2(numTerms))} bits per value instead of 32.
 *   <li>Trailing fixed meta + {@link CodecUtil} footer
 * </ul>
 *
 * <p>The flat {@code int[maxDoc]} buffer is retained during build for O(1) random-access
 * writes (the walker emits (termId, docId) in term-major order, not docId order), but gets
 * converted to DirectWriter+DISI on {@link #buildAndOpenReader()}. No heap growth beyond
 * that 4-B/doc buffer during the walk.
 */
@Slf4j
public final class SingleTermSidecarBuilder implements SingleTermSink, AutoCloseable {

    public static final int NO_VALUE = -1;

    static final String CODEC_NAME = "RfsSingleTerm";
    static final int VERSION_CURRENT = 0;
    static final String SIDECAR_FILE = "single-sidecar.bin";
    static final byte[] HEADER_ID = new byte[16];

    /** Block shift for DirectMonotonicWriter — matches {@link SidecarBuilder#DIRECT_MONOTONIC_BLOCK_SHIFT}. */
    static final int DIRECT_MONOTONIC_BLOCK_SHIFT = 16;
    static final byte DISI_DENSE_RANK_POWER = 9;

    private final Path spillDir;
    private final Directory dir;
    private final int maxDoc;

    private final IndexOutput termsStage;
    private final Path termsStagePath;

    private final int[] docToTermId;

    private int nextTermId = 0;
    private long[] termOffsetsBuf = new long[64];
    private boolean built = false;
    private boolean closed = false;

    public SingleTermSidecarBuilder(Path spillDir, int maxDoc) throws IOException {
        this.spillDir = spillDir;
        this.maxDoc = Math.max(1, maxDoc);
        Files.createDirectories(spillDir);
        this.dir = new NIOFSDirectory(spillDir);

        this.termsStage = dir.createOutput("terms-stage.bin", IOContext.DEFAULT);
        this.termsStagePath = spillDir.resolve("terms-stage.bin");

        this.docToTermId = new int[this.maxDoc];
        java.util.Arrays.fill(this.docToTermId, NO_VALUE);
    }

    @Override
    public int registerTerm(BytesRefLike term) throws IOException {
        if (nextTermId == termOffsetsBuf.length) {
            long[] grown = new long[termOffsetsBuf.length * 2];
            System.arraycopy(termOffsetsBuf, 0, grown, 0, termOffsetsBuf.length);
            termOffsetsBuf = grown;
        }
        termOffsetsBuf[nextTermId] = termsStage.getFilePointer();
        int len = term.length();
        termsStage.writeVInt(len);
        termsStage.writeBytes(term.bytes(), term.offset(), len);
        return nextTermId++;
    }

    @Override
    public void accept(int termId, int docId) throws IOException {
        if (docId < 0 || docId >= maxDoc) return;
        docToTermId[docId] = termId;
    }

    public SingleTermSidecarReader buildAndOpenReader() throws IOException {
        if (built) throw new IllegalStateException("buildAndOpenReader already called");
        built = true;
        termsStage.close();

        try (IndexOutput container = dir.createOutput(SIDECAR_FILE, IOContext.DEFAULT)) {
            CodecUtil.writeIndexHeader(container, CODEC_NAME, VERSION_CURRENT, HEADER_ID, "");

            long termsStart = container.getFilePointer();
            copyFileInto(container, "terms-stage.bin");
            long termsEnd = container.getFilePointer();

            long termOffDataStart = container.getFilePointer();
            writeMonotonic(container, termOffsetsBuf, nextTermId);
            long termOffDataEnd = container.getFilePointer();
            // Meta for DirectMonotonic goes after data; for simplicity here we inline it the
            // same way SidecarBuilder does. But for this single-term side the meta is also
            // small enough that we buffer and append it in one pass. Use the helper in
            // SidecarBuilder-style: data then meta.
            // (Actually we need meta written in the same stream for simplicity — rewrite below.)

            // Build the has-value DISI + DirectWriter values.
            FixedBitSet present = new FixedBitSet(maxDoc);
            int numPresent = 0;
            int maxTermIdSeen = 0;
            for (int d = 0; d < maxDoc; d++) {
                int tid = docToTermId[d];
                if (tid != NO_VALUE) {
                    present.set(d);
                    numPresent++;
                    if (tid > maxTermIdSeen) maxTermIdSeen = tid;
                }
            }

            long disiStart = container.getFilePointer();
            short disiJumpCount = 0;
            if (numPresent > 0) {
                BitSetIterator it = new BitSetIterator(present, numPresent);
                disiJumpCount = IndexedDISI.writeBitSet(it, container, DISI_DENSE_RANK_POWER);
            }
            long disiEnd = container.getFilePointer();

            long valuesStart = container.getFilePointer();
            int bitsPerValue = 0;
            if (numPresent > 0) {
                bitsPerValue = DirectWriter.unsignedBitsRequired(maxTermIdSeen);
                DirectWriter dw = DirectWriter.getInstance(container, numPresent, bitsPerValue);
                BitSetIterator it = new BitSetIterator(present, numPresent);
                int d;
                while ((d = it.nextDoc()) != shadow.lucene10.org.apache.lucene.search.DocIdSetIterator.NO_MORE_DOCS) {
                    dw.add(docToTermId[d]);
                }
                dw.finish();
            }
            long valuesEnd = container.getFilePointer();

            // Trailing meta. Fixed size: 10 longs (80) + 4 ints (16) + 1 byte + 1 short + 1 byte = 100.
            container.writeLong(termsStart);
            container.writeLong(termsEnd);
            container.writeLong(termOffDataStart);
            container.writeLong(termOffDataEnd);
            container.writeLong(disiStart);
            container.writeLong(disiEnd);
            container.writeLong(valuesStart);
            container.writeLong(valuesEnd);
            // Reserve two more longs for potential extensions / alignment (keeps trailer layout stable).
            container.writeLong(0L);
            container.writeLong(0L);
            container.writeInt(nextTermId);
            container.writeInt(maxDoc);
            container.writeInt(numPresent);
            container.writeInt(bitsPerValue);
            container.writeByte(DISI_DENSE_RANK_POWER);
            container.writeShort(disiJumpCount);
            container.writeByte((byte) DIRECT_MONOTONIC_BLOCK_SHIFT);

            CodecUtil.writeFooter(container);
        } finally {
            try { dir.deleteFile("terms-stage.bin"); } catch (IOException ignored) {}
        }

        closed = true;
        return SingleTermSidecarReader.open(spillDir);
    }

    private void copyFileInto(IndexOutput dst, String srcName) throws IOException {
        try (IndexInput in = dir.openInput(srcName, IOContext.DEFAULT)) {
            long len = in.length();
            byte[] buf = new byte[8192];
            long remaining = len;
            while (remaining > 0) {
                int chunk = (int) Math.min(remaining, buf.length);
                in.readBytes(buf, 0, chunk);
                dst.writeBytes(buf, 0, chunk);
                remaining -= chunk;
            }
        }
    }

    /**
     * Writes {@code count} monotonic longs via {@link DirectMonotonicWriter}. The writer
     * requires separate meta + data streams; for the single-term layout we keep it simple
     * by buffering term offsets in memory and writing them via DirectMonotonicWriter into
     * a pair of byte arrays, then splicing both into {@code container} back-to-back. The
     * caller records start/end of the combined blob; the reader parses the meta and data
     * independently using the recorded total length.
     *
     * <p>Design note: we don't need a separate meta-end pointer because
     * {@link SingleTermSidecarReader} reconstructs the DirectMonotonic reader by capturing
     * the writer's own byte output. To keep this simple and avoid the dual-stream complexity
     * of the positional sidecar, the single-term path keeps term-offsets as a plain
     * {@code long[]} written as raw {@code writeLong} entries into the container. With
     * typical cardinalities (a single-valued keyword rarely has more than a few million
     * distinct values), 8 B/term is fine — the meaningful win is the DirectWriter for the
     * {@code numPresent × bitsPerValue} values section.
     */
    private static void writeMonotonic(IndexOutput container, long[] values, int count) throws IOException {
        // Raw long[] in-place. Simpler than DirectMonotonic for this table and avoids the
        // dual-stream mechanics; matches what SingleTermSidecarReader reads back.
        for (int i = 0; i < count; i++) {
            container.writeLong(values[i]);
        }
    }

    Path spillDir() { return spillDir; }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(termsStage, dir);
        try {
            Files.deleteIfExists(termsStagePath);
        } catch (IOException e) {
            log.debug("Ignored single-term stage delete error: {}", e.toString());
        }
    }
}
