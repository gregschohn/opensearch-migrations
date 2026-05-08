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
    private static final String TERMS_STAGE_FILE = "terms-stage.bin";

    /** Fresh zero-filled 16-byte ID for the {@link CodecUtil} header. See {@link SidecarBuilder#headerId}. */
    static byte[] headerId() {
        return new byte[16];
    }

    static final byte DISI_DENSE_RANK_POWER = 9;

    private final Path spillDir;
    private final Directory dir;
    private final int maxDoc;

    private final IndexOutput termsStage;

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

        this.termsStage = dir.createOutput(TERMS_STAGE_FILE, IOContext.DEFAULT);

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
            CodecUtil.writeIndexHeader(container, CODEC_NAME, VERSION_CURRENT, headerId(), "");

            long termsStart = container.getFilePointer();
            copyFileInto(container, TERMS_STAGE_FILE);
            long termsEnd = container.getFilePointer();

            long termOffsetsStart = container.getFilePointer();
            writeRawTermOffsets(container, termOffsetsBuf, nextTermId);
            long termOffsetsEnd = container.getFilePointer();

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

            // Trailer: 8 longs (64) + 4 ints (16) + 1 byte + 1 short = 83 bytes. Read by
            // SingleTermSidecarReader.open by seeking back from the footer.
            container.writeLong(termsStart);
            container.writeLong(termsEnd);
            container.writeLong(termOffsetsStart);
            container.writeLong(termOffsetsEnd);
            container.writeLong(disiStart);
            container.writeLong(disiEnd);
            container.writeLong(valuesStart);
            container.writeLong(valuesEnd);
            container.writeInt(nextTermId);
            container.writeInt(maxDoc);
            container.writeInt(numPresent);
            container.writeInt(bitsPerValue);
            container.writeByte(DISI_DENSE_RANK_POWER);
            container.writeShort(disiJumpCount);

            CodecUtil.writeFooter(container);
        } finally {
            try {
                dir.deleteFile(TERMS_STAGE_FILE);
            } catch (IOException e) {
                // Best-effort cleanup; leaving the stage file behind is harmless because the
                // spill dir is per-(segment, field) and removed wholesale by SegmentTermIndex.close().
                log.debug("Ignored terms-stage delete error: {}", e.toString());
            }
        }

        closed = true;
        return SingleTermSidecarReader.open(spillDir);
    }

    private void copyFileInto(IndexOutput dst, String srcName) throws IOException {
        try (IndexInput in = dir.openInput(srcName, IOContext.DEFAULT)) {
            dst.copyBytes(in, in.length());
        }
    }

    /**
     * Writes {@code count} raw 8-byte term offsets. Single-valued keyword cardinalities are
     * low enough that DirectMonotonic compression isn't worth the dual-stream complexity
     * used on the positional sidecar.
     */
    private static void writeRawTermOffsets(IndexOutput container, long[] values, int count) throws IOException {
        for (int i = 0; i < count; i++) {
            container.writeLong(values[i]);
        }
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(termsStage, dir);
        try {
            Files.deleteIfExists(spillDir.resolve(TERMS_STAGE_FILE));
        } catch (IOException e) {
            log.debug("Ignored single-term stage delete error: {}", e.toString());
        }
    }
}
