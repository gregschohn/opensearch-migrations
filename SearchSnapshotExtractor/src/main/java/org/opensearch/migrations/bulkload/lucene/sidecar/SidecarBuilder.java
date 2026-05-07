package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.lang.invoke.VarHandle;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Comparator;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene10.org.apache.lucene.codecs.CodecUtil;
import shadow.lucene10.org.apache.lucene.codecs.lucene90.IndexedDISI;
import shadow.lucene10.org.apache.lucene.store.ByteBuffersDataOutput;
import shadow.lucene10.org.apache.lucene.store.ByteBuffersIndexOutput;
import shadow.lucene10.org.apache.lucene.store.Directory;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.IndexOutput;
import shadow.lucene10.org.apache.lucene.store.NIOFSDirectory;
import shadow.lucene10.org.apache.lucene.util.BitSetIterator;
import shadow.lucene10.org.apache.lucene.util.BitUtil;
import shadow.lucene10.org.apache.lucene.util.BytesRef;
import shadow.lucene10.org.apache.lucene.util.FixedBitSet;
import shadow.lucene10.org.apache.lucene.util.IOUtils;
import shadow.lucene10.org.apache.lucene.util.OfflineSorter;
import shadow.lucene10.org.apache.lucene.util.OfflineSorter.BufferSize;
import shadow.lucene10.org.apache.lucene.util.packed.DirectMonotonicWriter;

/**
 * Builds a per-(segment, field) sidecar that indexes {@code docId -> (pos, termId, startOff, endOff)+}.
 *
 * <p>On-disk layout is a single container file {@value #SIDECAR_FILE}, fronted by
 * {@link CodecUtil#writeIndexHeader} and closed by {@link CodecUtil#writeFooter}. Each section's
 * byte range is captured in a trailing fixed-size meta block. Encoding leans on Lucene:
 *
 * <ul>
 *   <li><b>terms</b> — {@code vInt(len) + UTF-8 bytes} per distinct term, in registration
 *       (ascending sorted-bytes) order.
 *   <li><b>payloads</b> — per-doc {@code vInt(pairCount) + (vInt Δpos, vInt termId, zInt
 *       startOff, zInt endOff) × pairCount}. Written only for docs with at least one token.
 *   <li><b>term offsets</b> — {@link DirectMonotonicWriter} data + inline-serialized meta over
 *       byte offsets into the terms section. Monotonic, so compression is ~a few bits/entry.
 *   <li><b>doc offsets</b> — same, over byte offsets into the payloads section, indexed by
 *       DISI ordinal (not raw docId).
 *   <li><b>has-values DISI</b> — {@link IndexedDISI} bitmap of docs with at least one token.
 * </ul>
 *
 * <p>The sort still uses Lucene's {@link OfflineSorter} on 20-byte records packed big-endian;
 * unsigned lexicographic comparison gives the correct {@code (docId, pos, termId) ASC} order.
 */
@Slf4j
public final class SidecarBuilder implements PostingsSink, AutoCloseable {

    static final String CODEC_NAME = "RfsSidecar";
    static final int VERSION_CURRENT = 0;
    static final String SIDECAR_FILE = "sidecar.bin";
    /** Fixed 16-byte ID used in the {@link CodecUtil} index header. Sidecars live in per-segment
     *  spill dirs so we don't need to distinguish them; the fixed ID keeps the reader strict. */
    static final byte[] SIDECAR_HEADER_ID = new byte[16];

    /** 20 bytes: docId(4) + pos(4) + termId(4) + startOff(4) + endOff(4), big-endian. */
    public static final int RECORD_BYTES = 20;

    private static final String SORT_INPUT_FILE = "sort-input.bin";
    private static final String TERMS_STAGE_FILE = "terms-stage.bin";
    private static final int DEFAULT_SORT_BUFFER_MB = 256;
    private static final VarHandle VH_BE_INT = BitUtil.VH_BE_INT;

    /** Block shift for DirectMonotonicWriter: 16 ⇒ 65 536-entry blocks. Matches Lucene defaults. */
    static final int DIRECT_MONOTONIC_BLOCK_SHIFT = 16;
    /** Dense rank power for IndexedDISI: 9 ⇒ 512-bit rank sub-blocks. Matches Lucene defaults. */
    static final byte DISI_DENSE_RANK_POWER = 9;

    private final Path spillDir;
    private final Directory dir;
    private final int maxDoc;
    private final BufferSize sortBufferSize;

    private final IndexOutput termsStage;
    private final IndexOutput sortInputOut;
    private final OfflineSorter.ByteSequencesWriter sortInputWriter;
    private final byte[] recordScratch = new byte[RECORD_BYTES];

    private int nextTermId = 0;
    private long[] termOffsetsBuf = new long[64];
    private boolean built = false;
    private boolean closed = false;

    public SidecarBuilder(Path spillDir, long sortBufferBytes, int maxDoc) throws IOException {
        this.spillDir = spillDir;
        this.maxDoc = Math.max(1, maxDoc);
        Files.createDirectories(spillDir);
        this.dir = new NIOFSDirectory(spillDir);

        long mb = sortBufferBytes > 0 ? (sortBufferBytes >>> 20) : DEFAULT_SORT_BUFFER_MB;
        mb = Math.max(OfflineSorter.MIN_BUFFER_SIZE_MB, Math.min(Integer.MAX_VALUE, mb));
        this.sortBufferSize = BufferSize.megabytes((int) mb);

        this.termsStage      = dir.createOutput(TERMS_STAGE_FILE, IOContext.DEFAULT);
        this.sortInputOut    = dir.createOutput(SORT_INPUT_FILE, IOContext.DEFAULT);
        this.sortInputWriter = new OfflineSorter.ByteSequencesWriter(sortInputOut);
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
    public void accept(int termId, int docId, int[] positions, int[] startOffsets, int[] endOffsets, int positionCount) throws IOException {
        if (positionCount <= 0 || docId < 0 || docId >= maxDoc) return;
        byte[] rec = recordScratch;
        VH_BE_INT.set(rec, 0, docId);
        VH_BE_INT.set(rec, 8, termId);
        BytesRef br = new BytesRef(rec, 0, RECORD_BYTES);
        for (int i = 0; i < positionCount; i++) {
            VH_BE_INT.set(rec, 4, positions[i]);
            VH_BE_INT.set(rec, 12, startOffsets[i]);
            VH_BE_INT.set(rec, 16, endOffsets[i]);
            sortInputWriter.write(br);
        }
    }

    public SidecarReader buildAndOpenReader() throws IOException {
        if (built) throw new IllegalStateException("buildAndOpenReader already called");
        built = true;

        termsStage.close();
        CodecUtil.writeFooter(sortInputOut);
        sortInputWriter.close();

        OfflineSorter sorter = new OfflineSorter(
                dir, "sort", Comparator.naturalOrder(), sortBufferSize,
                OfflineSorter.MAX_TEMPFILES, RECORD_BYTES, null, 0);
        String sortedName = sorter.sort(SORT_INPUT_FILE);

        try (IndexOutput container = dir.createOutput(SIDECAR_FILE, IOContext.DEFAULT)) {
            CodecUtil.writeIndexHeader(container, CODEC_NAME, VERSION_CURRENT, SIDECAR_HEADER_ID, "");

            long termsStart = container.getFilePointer();
            copyFileInto(container, TERMS_STAGE_FILE);
            long termsEnd = container.getFilePointer();

            long payloadsStart = container.getFilePointer();
            FixedBitSet docsWithValues = new FixedBitSet(maxDoc);
            PayloadResult pr = writePayloads(sortedName, container, docsWithValues);
            long payloadsEnd = container.getFilePointer();

            // Rebase term offsets to absolute container offsets: registerTerm recorded them
            // relative to the terms-stage file, which landed at termsStart inside the container.
            for (int i = 0; i < nextTermId; i++) {
                termOffsetsBuf[i] += termsStart;
            }

            // Term offsets: data comes first so the reader can slice [dataStart, dataEnd);
            // meta comes after, its start captured for loadMeta.
            Section termOffsets = writeMonotonic(container, termOffsetsBuf, nextTermId);
            Section docOffsets  = writeMonotonic(container, pr.docOffsets,  pr.numDocsWithValues);

            long disiStart = container.getFilePointer();
            short disiJumpCount = writeDisi(container, docsWithValues, pr.numDocsWithValues);
            long disiEnd = container.getFilePointer();

            // Trailing meta block — fixed layout, read by SidecarReader.open.
            container.writeLong(termsStart);
            container.writeLong(termsEnd);
            container.writeLong(payloadsStart);
            container.writeLong(payloadsEnd);
            container.writeLong(termOffsets.dataStart);
            container.writeLong(termOffsets.dataEnd);
            container.writeLong(termOffsets.metaStart);
            container.writeLong(termOffsets.metaEnd);
            container.writeLong(docOffsets.dataStart);
            container.writeLong(docOffsets.dataEnd);
            container.writeLong(docOffsets.metaStart);
            container.writeLong(docOffsets.metaEnd);
            container.writeLong(disiStart);
            container.writeLong(disiEnd);
            container.writeInt(nextTermId);
            container.writeInt(maxDoc);
            container.writeInt(pr.numDocsWithValues);
            container.writeByte(DISI_DENSE_RANK_POWER);
            container.writeShort(disiJumpCount);
            container.writeByte((byte) DIRECT_MONOTONIC_BLOCK_SHIFT);

            CodecUtil.writeFooter(container);
        } finally {
            dir.deleteFile(SORT_INPUT_FILE);
            dir.deleteFile(sortedName);
            dir.deleteFile(TERMS_STAGE_FILE);
        }

        closed = true;
        return SidecarReader.open(spillDir);
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

    private PayloadResult writePayloads(String sortedName, IndexOutput container,
                                        FixedBitSet docsWithValues) throws IOException {
        long[] docOffsets = new long[Math.min(Math.max(1, maxDoc), 64)];
        int numDocsWithValues = 0;

        try (OfflineSorter.ByteSequencesReader reader = new OfflineSorter.ByteSequencesReader(
                dir.openChecksumInput(sortedName), sortedName)) {

            ByteBuffersDataOutput staging = ByteBuffersDataOutput.newResettableInstance();
            int currentDoc = -1;
            int pairCount = 0;
            int prevPos = 0;

            BytesRef rec;
            while ((rec = reader.next()) != null) {
                if (rec.length != RECORD_BYTES) throw new IOException("bad record length " + rec.length);
                int docId    = (int) VH_BE_INT.get(rec.bytes, rec.offset);
                int pos      = (int) VH_BE_INT.get(rec.bytes, rec.offset + 4);
                int termId   = (int) VH_BE_INT.get(rec.bytes, rec.offset + 8);
                int startOff = (int) VH_BE_INT.get(rec.bytes, rec.offset + 12);
                int endOff   = (int) VH_BE_INT.get(rec.bytes, rec.offset + 16);

                if (docId != currentDoc) {
                    if (currentDoc >= 0) {
                        docOffsets = appendOffset(docOffsets, numDocsWithValues, container.getFilePointer());
                        container.writeVInt(pairCount);
                        staging.copyTo(container);
                        numDocsWithValues++;
                    }
                    currentDoc = docId;
                    pairCount = 0;
                    prevPos = 0;
                    staging.reset();
                    docsWithValues.set(docId);
                }
                staging.writeVInt(pos - prevPos);
                staging.writeVInt(termId);
                staging.writeZInt(startOff);
                staging.writeZInt(endOff);
                prevPos = pos;
                pairCount++;
            }
            if (currentDoc >= 0) {
                docOffsets = appendOffset(docOffsets, numDocsWithValues, container.getFilePointer());
                container.writeVInt(pairCount);
                staging.copyTo(container);
                numDocsWithValues++;
            }
        }
        return new PayloadResult(docOffsets, numDocsWithValues);
    }

    private static long[] appendOffset(long[] buf, int index, long value) {
        if (index == buf.length) {
            long[] grown = new long[buf.length * 2];
            System.arraycopy(buf, 0, grown, 0, buf.length);
            buf = grown;
        }
        buf[index] = value;
        return buf;
    }

    /**
     * Writes {@code count} monotonic longs using {@link DirectMonotonicWriter}. The writer
     * requires separate {@code meta} and {@code data} IndexOutputs; we buffer meta in memory
     * and append it to {@code container} right after the data so everything stays in one file.
     * The returned {@link Section} captures both byte ranges so the reader can slice them apart.
     */
    private static Section writeMonotonic(IndexOutput container, long[] values, int count) throws IOException {
        long dataStart = container.getFilePointer();
        if (count == 0) {
            // Empty section — data is empty, and meta is a fixed (tiny) "no values" blob we
            // still emit so the reader always parses a meta stream uniformly.
            // DirectMonotonicWriter doesn't support count=0, so hand-craft an empty body.
            long metaStart = container.getFilePointer();
            long metaEnd = metaStart;
            return new Section(dataStart, dataStart, metaStart, metaEnd);
        }

        ByteBuffersDataOutput metaBuf = ByteBuffersDataOutput.newResettableInstance();
        try (ByteBuffersIndexOutput metaOut = new ByteBuffersIndexOutput(metaBuf, "meta", "meta")) {
            DirectMonotonicWriter writer = DirectMonotonicWriter.getInstance(
                    metaOut, container, count, DIRECT_MONOTONIC_BLOCK_SHIFT);
            for (int i = 0; i < count; i++) {
                writer.add(values[i]);
            }
            writer.finish();
        }
        long dataEnd = container.getFilePointer();
        long metaStart = container.getFilePointer();
        metaBuf.copyTo(container);
        long metaEnd = container.getFilePointer();
        return new Section(dataStart, dataEnd, metaStart, metaEnd);
    }

    /** @return the jump-table entry count produced by {@link IndexedDISI#writeBitSet}. */
    private static short writeDisi(IndexOutput container, FixedBitSet bits, int cardinality) throws IOException {
        if (cardinality == 0) return 0;
        BitSetIterator it = new BitSetIterator(bits, cardinality);
        return IndexedDISI.writeBitSet(it, container, DISI_DENSE_RANK_POWER);
    }

    /** Byte-range of a DirectMonotonic section within the container. */
    private record Section(long dataStart, long dataEnd, long metaStart, long metaEnd) {}

    private record PayloadResult(long[] docOffsets, int numDocsWithValues) {}

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(termsStage, sortInputWriter, dir);
        try {
            Files.deleteIfExists(spillDir.resolve(SORT_INPUT_FILE));
        } catch (IOException e) {
            log.debug("Ignored sort-input delete error: {}", e.toString());
        }
        try {
            Files.deleteIfExists(spillDir.resolve(TERMS_STAGE_FILE));
        } catch (IOException e) {
            log.debug("Ignored terms-stage delete error: {}", e.toString());
        }
    }
}
