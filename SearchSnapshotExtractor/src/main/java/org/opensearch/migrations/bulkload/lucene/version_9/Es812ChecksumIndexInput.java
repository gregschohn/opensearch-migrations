package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;

import shadow.lucene9.org.apache.lucene.store.ChecksumIndexInput;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * A {@link ChecksumIndexInput} that wraps an {@link Es812CodecSubstitutingInput}
 * and makes {@link shadow.lucene9.org.apache.lucene.codecs.CodecUtil#checkFooter}
 * pass even though the in-stream codec names have been patched (which would
 * otherwise invalidate the running CRC).
 *
 * <p>{@code checkFooter} does:
 * <ol>
 *   <li>Read footer magic + algorithmId from the input stream.</li>
 *   <li>Call {@code in.getChecksum()} to get the running CRC.</li>
 *   <li>Call {@code readCRC(in)} which reads the next 8 bytes from the stream
 *       as the stored CRC.</li>
 *   <li>Assert that running CRC == stored CRC.</li>
 * </ol>
 *
 * <p>We make step 4 pass by overriding {@link #getChecksum()} to return the
 * value of the stored CRC from the original raw file footer.  Since
 * {@code readCRC} reads those same 8 bytes from our stream (which at that
 * point passes through the original unpatched bytes), both sides of the
 * comparison see the same original CRC value.
 *
 * <p>The original CRC is read by peeking at the raw underlying file: we
 * {@code seek} a clone to {@code length - 8} and read the 8-byte long.
 * This is safe because the footer CRC bytes are at a fixed position that
 * is the same in both the original and the substituted streams (the codec
 * header substitution shifts only early bytes; the footer is at the end
 * and its position in the raw file is unchanged).
 */
public class Es812ChecksumIndexInput extends ChecksumIndexInput {

    private final Es812CodecSubstitutingInput delegate;
    /** Clone of the raw underlying input used to peek at the footer CRC. */
    private final IndexInput rawPeek;

    public Es812ChecksumIndexInput(String resourceDescription,
                                    Es812CodecSubstitutingInput delegate,
                                    IndexInput rawInput) {
        super(resourceDescription);
        this.delegate = delegate;
        this.rawPeek = rawInput.clone();
    }

    /**
     * Returns the stored CRC from the raw file footer so that
     * {@link shadow.lucene9.org.apache.lucene.codecs.CodecUtil#checkFooter}
     * will see matching values on both sides of the equality check.
     */
    @Override
    public long getChecksum() throws IOException {
        // Footer layout: ...content...[footerMagic:4][algorithmId:4][storedCRC:8]
        // Read the stored CRC the same way readCRC() does: 8 readByte() calls, big-endian.
        long crcOffset = rawPeek.length() - 8L;
        rawPeek.seek(crcOffset);
        // Build long big-endian from individual bytes (MemorySegmentIndexInput.readLong()
        // may be native-endian; readByte() is always correct).
        long value = 0;
        for (int i = 0; i < 8; i++) {
            value = (value << 8) | (rawPeek.readByte() & 0xFFL);
        }
        return value;
    }

    @Override public byte readByte() throws IOException { return delegate.readByte(); }
    @Override public void readBytes(byte[] b, int off, int len) throws IOException { delegate.readBytes(b, off, len); }
    @Override public long length() { return delegate.length(); }
    @Override public long getFilePointer() { return delegate.getFilePointer(); }
    @Override public void seek(long pos) throws IOException { delegate.seek(pos); }

    @Override
    public void close() throws IOException {
        try { delegate.close(); } finally { rawPeek.close(); }
    }

    @Override
    public IndexInput slice(String sliceDescription, long offset, long length) throws IOException {
        return delegate.slice(sliceDescription, offset, length);
    }
}
