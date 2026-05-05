package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;

/**
 * Mmap abstraction that transparently spans files larger than
 * {@link Integer#MAX_VALUE} bytes (2 GiB), which {@link FileChannel#map} rejects
 * in a single call.
 *
 * <p>Each chunk is an independent mmap region of at most {@code chunkSize} bytes.
 * Positional reads ({@link #getInt(long)} / {@link #getLong(long)}) that straddle
 * a chunk boundary are reassembled byte-by-byte; reads that fit within one chunk
 * take the fast path on a duplicated {@link ByteBuffer}.
 *
 * <p>Empty files are represented as a single zero-length chunk so callers do not
 * need to special-case them.
 *
 * <p>Thread-safety: {@link #getInt(long)} and {@link #getLong(long)} duplicate
 * the underlying chunk on each call and never mutate shared state. {@link Cursor}
 * holds per-cursor position and is not safe to share across threads; get a fresh
 * cursor per reader via {@link #cursor(long)}.
 */
public final class ChunkedByteBuffer {

    /**
     * Default chunk size. 1 GiB leaves ample headroom under the 2 GiB
     * {@code FileChannel.map} limit, keeps the chunk count small for terabyte
     * files, and avoids fragmenting the virtual address space for the common
     * sub-2-GiB case (single chunk, single mmap).
     */
    public static final long DEFAULT_CHUNK_SIZE = 1L << 30; // 1 GiB

    private final ByteBuffer[] chunks;
    private final long chunkSize;
    private final long totalSize;
    private ByteOrder order = ByteOrder.BIG_ENDIAN;

    private ChunkedByteBuffer(ByteBuffer[] chunks, long chunkSize, long totalSize) {
        this.chunks = chunks;
        this.chunkSize = chunkSize;
        this.totalSize = totalSize;
    }

    /**
     * Mmaps {@code ch} as 1..N chunks of at most {@code chunkSize} bytes.
     * An empty channel yields a single zero-length heap buffer so downstream
     * code doesn't need to branch on size == 0.
     */
    public static ChunkedByteBuffer open(FileChannel ch, long chunkSize) throws IOException {
        if (chunkSize <= 0 || chunkSize > Integer.MAX_VALUE) {
            throw new IllegalArgumentException("chunkSize must be in (0, Integer.MAX_VALUE]: " + chunkSize);
        }
        long size = ch.size();
        if (size == 0) {
            return new ChunkedByteBuffer(new ByteBuffer[] { ByteBuffer.allocate(0) }, chunkSize, 0);
        }
        int numChunks = (int) ((size + chunkSize - 1) / chunkSize);
        ByteBuffer[] chunks = new ByteBuffer[numChunks];
        for (int i = 0; i < numChunks; i++) {
            long off = i * chunkSize;
            long len = Math.min(chunkSize, size - off);
            chunks[i] = ch.map(FileChannel.MapMode.READ_ONLY, off, len);
            chunks[i].order(ByteOrder.BIG_ENDIAN);
        }
        return new ChunkedByteBuffer(chunks, chunkSize, size);
    }

    /** Sets the byte order applied to multi-byte positional reads and new cursors. */
    public void order(ByteOrder o) {
        this.order = o;
        for (ByteBuffer chunk : chunks) {
            chunk.order(o);
        }
    }

    public long size() {
        return totalSize;
    }

    /**
     * Reads a 32-bit int at absolute byte offset {@code pos} using the configured
     * {@link ByteOrder}. Straddling a chunk boundary falls back to a byte-wise assembly.
     */
    public int getInt(long pos) {
        if (pos < 0 || pos + 4 > totalSize) {
            throw new IndexOutOfBoundsException("getInt pos=" + pos + " totalSize=" + totalSize);
        }
        int chunkIdx = (int) (pos / chunkSize);
        int inChunk = (int) (pos - chunkIdx * chunkSize);
        if (inChunk + 4 <= chunks[chunkIdx].limit()) {
            return chunks[chunkIdx].getInt(inChunk);
        }
        // Straddles a chunk boundary.
        byte b0 = readByte(pos);
        byte b1 = readByte(pos + 1);
        byte b2 = readByte(pos + 2);
        byte b3 = readByte(pos + 3);
        if (order == ByteOrder.LITTLE_ENDIAN) {
            return (b0 & 0xFF) | ((b1 & 0xFF) << 8) | ((b2 & 0xFF) << 16) | ((b3 & 0xFF) << 24);
        }
        return ((b0 & 0xFF) << 24) | ((b1 & 0xFF) << 16) | ((b2 & 0xFF) << 8) | (b3 & 0xFF);
    }

    /**
     * Reads a 64-bit long at absolute byte offset {@code pos} using the configured
     * {@link ByteOrder}. Straddling a chunk boundary falls back to a byte-wise assembly.
     */
    public long getLong(long pos) {
        if (pos < 0 || pos + 8 > totalSize) {
            throw new IndexOutOfBoundsException("getLong pos=" + pos + " totalSize=" + totalSize);
        }
        int chunkIdx = (int) (pos / chunkSize);
        int inChunk = (int) (pos - chunkIdx * chunkSize);
        if (inChunk + 8 <= chunks[chunkIdx].limit()) {
            return chunks[chunkIdx].getLong(inChunk);
        }
        // Straddles a chunk boundary.
        long result = 0;
        if (order == ByteOrder.LITTLE_ENDIAN) {
            for (int i = 0; i < 8; i++) {
                result |= ((long) (readByte(pos + i) & 0xFF)) << (i * 8);
            }
        } else {
            for (int i = 0; i < 8; i++) {
                result = (result << 8) | (readByte(pos + i) & 0xFF);
            }
        }
        return result;
    }

    private byte readByte(long pos) {
        int chunkIdx = (int) (pos / chunkSize);
        int inChunk = (int) (pos - chunkIdx * chunkSize);
        return chunks[chunkIdx].get(inChunk);
    }

    /** Returns a fresh {@link Cursor} starting at absolute byte offset {@code pos}. */
    public Cursor cursor(long pos) {
        return new Cursor(pos);
    }

    /**
     * Sequential byte reader that advances across chunk boundaries. Fresh cursors
     * are cheap; use one per reader thread.
     */
    public final class Cursor implements VarintCoder.ByteSource {
        private long pos;

        Cursor(long pos) {
            this.pos = pos;
        }

        @Override
        public byte readByte() {
            byte b = ChunkedByteBuffer.this.readByte(pos);
            pos++;
            return b;
        }

        /** Reads a 32-bit big-endian int (matches {@link java.io.DataOutputStream#writeInt}). */
        public int readInt() {
            byte b0 = readByte();
            byte b1 = readByte();
            byte b2 = readByte();
            byte b3 = readByte();
            return ((b0 & 0xFF) << 24) | ((b1 & 0xFF) << 16) | ((b2 & 0xFF) << 8) | (b3 & 0xFF);
        }

        public void readBytes(byte[] dst) {
            for (int i = 0; i < dst.length; i++) {
                dst[i] = readByte();
            }
        }

        public long position() {
            return pos;
        }
    }
}
