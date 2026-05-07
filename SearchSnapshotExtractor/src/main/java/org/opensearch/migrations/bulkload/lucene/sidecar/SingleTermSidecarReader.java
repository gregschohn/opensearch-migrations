package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;

import shadow.lucene10.org.apache.lucene.codecs.CodecUtil;
import shadow.lucene10.org.apache.lucene.codecs.lucene90.IndexedDISI;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.MMapDirectory;
import shadow.lucene10.org.apache.lucene.store.RandomAccessInput;
import shadow.lucene10.org.apache.lucene.util.IOUtils;
import shadow.lucene10.org.apache.lucene.util.LongValues;
import shadow.lucene10.org.apache.lucene.util.packed.DirectReader;

/**
 * Reader for the single-container single-term sidecar produced by
 * {@link SingleTermSidecarBuilder}. MMap-backed.
 */
public final class SingleTermSidecarReader implements AutoCloseable {

    /** Size of the trailing meta block in bytes; kept in sync with {@link SingleTermSidecarBuilder}. */
    private static final int TRAILER_BYTES =
            10 * Long.BYTES   // 10 longs: 4 section pairs + 2 reserved
          + 4 * Integer.BYTES // numTerms, maxDoc, numPresent, bitsPerValue
          + 1                 // DISI dense rank power
          + Short.BYTES       // DISI jump-table entry count
          + 1;                // DirectMonotonic block shift (unused today, reserved)

    private final Path spillDir;
    private final int maxDoc;
    private final int numTerms;
    private final int numPresent;
    private final int bitsPerValue;

    private final MMapDirectory dir;
    private final IndexInput container;
    private final long termsStart;
    private final long termOffsetsStart;

    private final long disiStart;
    private final long disiLength;
    private final int disiJumpTableEntryCount;
    private final byte disiDenseRankPower;

    private final LongValues values; // DirectReader over the bit-packed termIds

    private volatile boolean closed;

    private SingleTermSidecarReader(Builder b) {
        this.spillDir = b.spillDir;
        this.maxDoc = b.maxDoc;
        this.numTerms = b.numTerms;
        this.numPresent = b.numPresent;
        this.bitsPerValue = b.bitsPerValue;
        this.dir = b.dir;
        this.container = b.container;
        this.termsStart = b.termsStart;
        this.termOffsetsStart = b.termOffsetsStart;
        this.disiStart = b.disiStart;
        this.disiLength = b.disiLength;
        this.disiJumpTableEntryCount = b.disiJumpTableEntryCount;
        this.disiDenseRankPower = b.disiDenseRankPower;
        this.values = b.values;
    }

    static SingleTermSidecarReader open(Path spillDir) throws IOException {
        MMapDirectory dir = new MMapDirectory(spillDir);
        IndexInput container = null;
        try {
            container = dir.openInput(SingleTermSidecarBuilder.SIDECAR_FILE, IOContext.DEFAULT);
            CodecUtil.checkIndexHeader(container, SingleTermSidecarBuilder.CODEC_NAME,
                    SingleTermSidecarBuilder.VERSION_CURRENT, SingleTermSidecarBuilder.VERSION_CURRENT,
                    SingleTermSidecarBuilder.HEADER_ID, "");

            long fileLen = container.length();
            long footerLen = CodecUtil.footerLength();
            long trailerPos = fileLen - footerLen - TRAILER_BYTES;
            if (trailerPos < 0) {
                throw new IOException("Single-term sidecar too short for trailer: " + fileLen);
            }
            container.seek(trailerPos);
            long termsStart        = container.readLong();
            long termsEnd          = container.readLong();
            long termOffDataStart  = container.readLong();
            long termOffDataEnd    = container.readLong();
            long disiStart         = container.readLong();
            long disiEnd           = container.readLong();
            long valuesStart       = container.readLong();
            long valuesEnd         = container.readLong();
            container.readLong(); // reserved
            container.readLong(); // reserved
            int numTerms           = container.readInt();
            int maxDoc             = container.readInt();
            int numPresent         = container.readInt();
            int bitsPerValue       = container.readInt();
            byte denseRankPower    = container.readByte();
            short disiJumpCount    = container.readShort();
            container.readByte();  // block shift — unused; single-term writes raw longs

            if (termsStart > termsEnd || termOffDataStart > termOffDataEnd
                    || disiStart > disiEnd || valuesStart > valuesEnd) {
                throw new IOException("Malformed single-term sidecar section offsets");
            }

            container.seek(0);
            CodecUtil.checksumEntireFile(container);

            Builder b = new Builder();
            b.spillDir = spillDir;
            b.dir = dir;
            b.container = container;
            b.maxDoc = maxDoc;
            b.numTerms = numTerms;
            b.numPresent = numPresent;
            b.bitsPerValue = bitsPerValue;
            b.termsStart = termsStart;
            b.termOffsetsStart = termOffDataStart;
            b.disiStart = disiStart;
            b.disiLength = disiEnd - disiStart;
            b.disiJumpTableEntryCount = disiJumpCount;
            b.disiDenseRankPower = denseRankPower;

            if (numPresent > 0 && bitsPerValue > 0) {
                RandomAccessInput valuesSlice = container.randomAccessSlice(
                        valuesStart, valuesEnd - valuesStart);
                b.values = DirectReader.getInstance(valuesSlice, bitsPerValue);
            } else {
                b.values = null;
            }

            return new SingleTermSidecarReader(b);
        } catch (Throwable t) {
            IOUtils.closeWhileHandlingException(container, dir);
            throw t;
        }
    }

    /**
     * Returns the single term string for {@code docId}, or {@code null} when the doc has no value.
     */
    public String get(int docId) throws IOException {
        if (closed) throw new IOException("SingleTermSidecarReader closed");
        if (docId < 0 || docId >= maxDoc) return null;
        if (numPresent == 0 || values == null) return null;

        IndexedDISI disi = new IndexedDISI(
                container.clone(), disiStart, disiLength, disiJumpTableEntryCount,
                disiDenseRankPower, numPresent);
        int found = disi.advance(docId);
        if (found != docId) return null;
        long ordinal = disi.index();

        int termId = (int) values.get(ordinal);
        if (termId < 0 || termId >= numTerms) {
            throw new IOException("termId out of range: " + termId + " (numTerms=" + numTerms + ")");
        }

        // term offset: 8B per term from termOffsetsStart.
        IndexInput in = container.clone();
        in.seek(termOffsetsStart + (long) termId * 8L);
        long termOffset = in.readLong();
        in.seek(termsStart + termOffset);
        int len = in.readVInt();
        byte[] bytes = new byte[len];
        in.readBytes(bytes, 0, len);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(container, dir);
    }

    Path spillDir() { return spillDir; }
    public int maxDoc() { return maxDoc; }
    public int numTerms() { return numTerms; }

    private static final class Builder {
        Path spillDir;
        MMapDirectory dir;
        IndexInput container;
        long termsStart;
        long termOffsetsStart;
        long disiStart;
        long disiLength;
        int disiJumpTableEntryCount;
        byte disiDenseRankPower;
        LongValues values;
        int maxDoc;
        int numTerms;
        int numPresent;
        int bitsPerValue;
    }
}
