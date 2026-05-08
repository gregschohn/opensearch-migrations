package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

import shadow.lucene10.org.apache.lucene.codecs.CodecUtil;
import shadow.lucene10.org.apache.lucene.codecs.lucene90.IndexedDISI;
import shadow.lucene10.org.apache.lucene.store.IOContext;
import shadow.lucene10.org.apache.lucene.store.IndexInput;
import shadow.lucene10.org.apache.lucene.store.MMapDirectory;
import shadow.lucene10.org.apache.lucene.store.RandomAccessInput;
import shadow.lucene10.org.apache.lucene.util.IOUtils;
import shadow.lucene10.org.apache.lucene.util.packed.DirectMonotonicReader;

/**
 * Reader for the single-container sidecar produced by {@link SidecarBuilder}.
 * MMap-backed; handles files &gt; 2 GiB via Lucene's {@link MMapDirectory}.
 *
 * <p>{@link #get(int)} clones IndexInputs per call so concurrent readers don't share
 * position state, matching the thread-safety contract {@link IndexInput} provides.
 */
public final class SidecarReader implements AutoCloseable {

    /** Size of the trailing meta block in bytes. Kept in sync with {@link SidecarBuilder}. */
    private static final int TRAILER_BYTES =
            14 * Long.BYTES   // 14 longs: 7 section start/end pairs
          + 3 * Integer.BYTES // numTerms, maxDoc, numDocsWithValues
          + 1                 // DISI dense rank power
          + Short.BYTES       // DISI jump-table entry count
          + 1;                // DirectMonotonic block shift

    private final Path spillDir;
    private final int maxDoc;
    private final int numTerms;
    private final int numDocsWithValues;

    private final MMapDirectory dir;
    private final IndexInput container;

    private final DirectMonotonicReader termOffsets;
    private final DirectMonotonicReader docOffsets;

    // IndexedDISI — reconstructed per-get with the raw container input + offset/length.
    private final long disiStart;
    private final long disiLength;
    private final int disiJumpTableEntryCount;
    private final byte disiDenseRankPower;

    private volatile boolean closed;

    private SidecarReader(Builder b) {
        this.spillDir = b.spillDir;
        this.maxDoc = b.maxDoc;
        this.numTerms = b.numTerms;
        this.numDocsWithValues = b.numDocsWithValues;
        this.dir = b.dir;
        this.container = b.container;
        this.termOffsets = b.termOffsets;
        this.docOffsets = b.docOffsets;
        this.disiStart = b.disiStart;
        this.disiLength = b.disiLength;
        this.disiJumpTableEntryCount = b.disiJumpTableEntryCount;
        this.disiDenseRankPower = b.disiDenseRankPower;
    }

    static SidecarReader open(Path spillDir) throws IOException {
        MMapDirectory dir = new MMapDirectory(spillDir);
        IndexInput container = null;
        try {
            container = dir.openInput(SidecarBuilder.SIDECAR_FILE, IOContext.DEFAULT);
            CodecUtil.checkIndexHeader(container, SidecarBuilder.CODEC_NAME,
                    SidecarBuilder.VERSION_CURRENT, SidecarBuilder.VERSION_CURRENT,
                    SidecarBuilder.SIDECAR_HEADER_ID, "");

            long fileLen = container.length();
            long footerLen = CodecUtil.footerLength();
            long trailerPos = fileLen - footerLen - TRAILER_BYTES;
            if (trailerPos < 0) {
                throw new IOException("Sidecar container shorter than expected trailer: " + fileLen);
            }
            container.seek(trailerPos);
            long termsStart        = container.readLong();
            long termsEnd          = container.readLong();
            long payloadsStart     = container.readLong();
            long payloadsEnd       = container.readLong();
            long termOffDataStart  = container.readLong();
            long termOffDataEnd    = container.readLong();
            long termOffMetaStart  = container.readLong();
            long termOffMetaEnd    = container.readLong();
            long docOffDataStart   = container.readLong();
            long docOffDataEnd     = container.readLong();
            long docOffMetaStart   = container.readLong();
            long docOffMetaEnd     = container.readLong();
            long disiStart         = container.readLong();
            long disiEnd           = container.readLong();
            int numTerms           = container.readInt();
            int maxDoc             = container.readInt();
            int numDocsWithValues  = container.readInt();
            byte denseRankPower    = container.readByte();
            short disiJumpCount    = container.readShort();
            byte blockShift        = container.readByte();

            // Final checksum verify (does not shift the file pointer from our POV; we re-seek anyway).
            container.seek(0);
            CodecUtil.checksumEntireFile(container);

            Builder b = new Builder();
            b.spillDir = spillDir;
            b.dir = dir;
            b.container = container;
            b.maxDoc = maxDoc;
            b.numTerms = numTerms;
            b.numDocsWithValues = numDocsWithValues;

            // termsEnd / payloadsEnd are currently only used for bounds validation of the
            // container layout; slicing them would be redundant since term/payload reads go
            // through cloned container inputs using absolute offsets.
            if (termsStart > termsEnd || payloadsStart > payloadsEnd) {
                throw new IOException("Sidecar container has malformed section offsets");
            }

            b.termOffsets = numTerms == 0 ? null
                    : loadMonotonic(container,
                            termOffDataStart, termOffDataEnd,
                            termOffMetaStart, termOffMetaEnd,
                            numTerms, blockShift, "termOffsets");

            b.docOffsets = numDocsWithValues == 0 ? null
                    : loadMonotonic(container,
                            docOffDataStart, docOffDataEnd,
                            docOffMetaStart, docOffMetaEnd,
                            numDocsWithValues, blockShift, "docOffsets");

            b.disiStart = disiStart;
            b.disiLength = disiEnd - disiStart;
            b.disiJumpTableEntryCount = disiJumpCount;
            b.disiDenseRankPower = denseRankPower;

            return new SidecarReader(b);
        } catch (Throwable t) {
            IOUtils.closeWhileHandlingException(container, dir);
            throw t;
        }
    }

    private static DirectMonotonicReader loadMonotonic(
            IndexInput container,
            long dataStart, long dataEnd, long metaStart, long metaEnd,
            long numValues, int blockShift, String name) throws IOException {
        IndexInput metaSlice = container.slice(name + "-meta", metaStart, metaEnd - metaStart);
        DirectMonotonicReader.Meta meta = DirectMonotonicReader.loadMeta(metaSlice, numValues, blockShift);
        RandomAccessInput dataSlice = container.randomAccessSlice(dataStart, dataEnd - dataStart);
        return DirectMonotonicReader.getInstance(meta, dataSlice);
    }

    /** Returns the tokens for {@code docId}, position-ordered, deduplicated by longest-at-same-position. */
    public List<TermEntry> get(int docId) throws IOException {
        if (closed) throw new IOException("SidecarReader closed");
        if (docId < 0 || docId >= maxDoc) return Collections.emptyList();
        if (numDocsWithValues == 0) return Collections.emptyList();

        // Ordinal lookup via DISI: does this doc have a payload, and if so at which ordinal?
        // The public IndexedDISI constructor takes the raw container input with offset+length;
        // clone so concurrent get() calls don't share position state.
        IndexedDISI disi = new IndexedDISI(
                container.clone(), disiStart, disiLength, disiJumpTableEntryCount,
                disiDenseRankPower, numDocsWithValues);
        int found = disi.advance(docId);
        if (found != docId) return Collections.emptyList();
        long ordinal = disi.index();

        // docOffsets stores absolute container byte offsets to each doc's payload start.
        long payloadOffset = docOffsets.get(ordinal);

        IndexInput in = container.clone();
        in.seek(payloadOffset);
        int numEntries = in.readVInt();

        int[] rawPos      = new int[numEntries];
        int[] rawTermId   = new int[numEntries];
        int[] rawStartOff = new int[numEntries];
        int[] rawEndOff   = new int[numEntries];
        int prevPos = 0;
        for (int i = 0; i < numEntries; i++) {
            prevPos        += in.readVInt();
            rawPos[i]       = prevPos;
            rawTermId[i]    = in.readVInt();
            rawStartOff[i]  = in.readZInt();
            rawEndOff[i]    = in.readZInt();
        }

        // Within each position group, pick the winner by term length, then materialize
        // the winner's bytes. Singleton groups (the common case) read one term body.
        // Multi-entry groups read a cheap vInt length per candidate + one body for the winner.
        List<TermEntry> result = new ArrayList<>(numEntries);
        int i = 0;
        while (i < numEntries) {
            int j = i + 1;
            while (j < numEntries && rawPos[j] == rawPos[i]) j++;
            int best = i;
            if (j - i > 1) {
                int bestLen = readTermLength(rawTermId[i]);
                for (int k = i + 1; k < j; k++) {
                    int lk = readTermLength(rawTermId[k]);
                    if (lk > bestLen) { best = k; bestLen = lk; }
                }
            }
            result.add(new TermEntry(readTerm(rawTermId[best]), rawStartOff[best], rawEndOff[best]));
            i = j;
        }
        return result;
    }

    private int readTermLength(int termId) throws IOException {
        checkTermId(termId);
        IndexInput in = container.clone();
        in.seek(termOffsets.get(termId));
        return in.readVInt();
    }

    /** Convenience — strings only. */
    public List<String> getTermStrings(int docId) throws IOException {
        List<TermEntry> entries = get(docId);
        List<String> strings = new ArrayList<>(entries.size());
        for (TermEntry e : entries) strings.add(e.term());
        return strings;
    }

    private String readTerm(int termId) throws IOException {
        checkTermId(termId);
        IndexInput in = container.clone();
        in.seek(termOffsets.get(termId));
        int len = in.readVInt();
        byte[] bytes = new byte[len];
        in.readBytes(bytes, 0, len);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    private void checkTermId(int termId) throws IOException {
        if (termId < 0 || termId >= numTerms) {
            throw new IOException("termId out of range: " + termId + " (numTerms=" + numTerms + ")");
        }
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        IOUtils.closeWhileHandlingException(container, dir);
    }

    Path spillDir() { return spillDir; }

    private static final class Builder {
        Path spillDir;
        MMapDirectory dir;
        IndexInput container;
        DirectMonotonicReader termOffsets;
        DirectMonotonicReader docOffsets;
        long disiStart;
        long disiLength;
        int disiJumpTableEntryCount;
        byte disiDenseRankPower;
        int maxDoc;
        int numTerms;
        int numDocsWithValues;
    }
}
