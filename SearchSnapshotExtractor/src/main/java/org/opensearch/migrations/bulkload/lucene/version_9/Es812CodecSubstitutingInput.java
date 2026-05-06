package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Map;

import shadow.lucene9.org.apache.lucene.store.FilterIndexInput;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * A {@link FilterIndexInput} that substitutes ES812 codec names with their
 * Lucene equivalents at the byte-stream level.
 *
 * <p>The state machine detects Lucene header magic ({@code 0x3FD76C17}) in the byte
 * stream and, when found, reads the following VInt-encoded codec name directly from
 * the underlying input.  If the name matches a known ES812 codec name it is replaced
 * with the Lucene equivalent (which may be a different length).  All subsequent
 * {@link #readByte()} calls drain the substituted bytes before resuming normal
 * passthrough.
 *
 * <p>Because substituted names may be longer than the originals, the virtual stream
 * length and file-pointer differ from the raw (underlying) values.  This class
 * tracks a {@link #virtualFP} that counts every byte actually returned by
 * {@link #readByte()} and overrides {@link #getFilePointer()} and {@link #length()}
 * accordingly, so that callers computing {@code length() - getFilePointer()} for
 * footer-region detection see the correct remaining byte count.
 */
public class Es812CodecSubstitutingInput extends FilterIndexInput {

    /** Lucene header magic: big-endian 0x3FD76C17. */
    private static final byte[] LUCENE_MAGIC = {0x3F, (byte)0xD7, 0x6C, 0x17};

    private final Map<String, String> codecReplacements;

    // ---- magic detection ----
    private int magicMatch = 0;

    // ---- pending substitution state ----
    /** Non-null while draining a substituted (or pass-through) codec name. */
    private byte[] pendingBytes;
    private int pendingIdx;

    // ---- virtual file pointer ----
    /**
     * Counts every byte returned by {@link #readByte()}.  This may exceed
     * {@link #in#getFilePointer()} when substituted names are longer than originals.
     */
    private long virtualFP = 0;
    /**
     * Net extra bytes injected by all substitutions so far.
     * {@code virtualLength = in.length() + virtualExtra}.
     */
    private long virtualExtra = 0;

    public Es812CodecSubstitutingInput(IndexInput in, Map<String, String> codecReplacements) {
        super("Es812CodecSubstituting(" + in + ")", in);
        this.codecReplacements = codecReplacements;
    }

    @Override
    public byte readByte() throws IOException {
        // ---- drain pending substitution ----
        if (pendingBytes != null) {
            byte b = pendingBytes[pendingIdx++];
            virtualFP++;
            if (pendingIdx >= pendingBytes.length) {
                pendingBytes = null;
                magicMatch = 0;
            }
            return b;
        }

        byte b = in.readByte();
        virtualFP++;

        // ---- detect Lucene magic ----
        if (b == LUCENE_MAGIC[magicMatch]) {
            if (++magicMatch == 4) {
                consumeCodecName();
            }
        } else {
            magicMatch = (b == LUCENE_MAGIC[0]) ? 1 : 0;
        }

        return b;
    }

    /**
     * Called immediately after the 4-byte Lucene magic is consumed from {@code in}.
     * Reads VInt + name bytes, checks for ES812 substitution, and queues replacement bytes.
     */
    private void consumeCodecName() throws IOException {
        int nameLen = readRawVInt();
        byte[] nameBytes = new byte[nameLen];
        for (int i = 0; i < nameLen; i++) nameBytes[i] = in.readByte();
        String name = new String(nameBytes, StandardCharsets.UTF_8);

        String replacement = codecReplacements.get(name);
        byte[] replBytes = (replacement != null)
            ? replacement.getBytes(StandardCharsets.UTF_8)
            : nameBytes;
        byte[] vint = encodeVInt(replBytes.length);

        // vint byte(s) + name bytes consumed from `in`
        int origTotal = vIntLen(nameLen) + nameLen;

        pendingBytes = new byte[vint.length + replBytes.length];
        System.arraycopy(vint, 0, pendingBytes, 0, vint.length);
        System.arraycopy(replBytes, 0, pendingBytes, vint.length, replBytes.length);
        pendingIdx = 0;

        // Track extra bytes added by substitution
        int delta = pendingBytes.length - origTotal;
        virtualExtra += delta;
    }

    @Override
    public void readBytes(byte[] dst, int offset, int len) throws IOException {
        for (int i = 0; i < len; i++) dst[offset + i] = readByte();
    }

    /** Virtual file pointer — counts every byte served by {@link #readByte()}. */
    @Override
    public long getFilePointer() {
        return virtualFP;
    }

    /** Virtual file length — raw length plus any extra bytes from name substitutions. */
    @Override
    public long length() {
        return in.length() + virtualExtra;
    }

    @Override
    public void seek(long pos) throws IOException {
        // Translate virtual position to raw position.
        // Approximation: assumes substitutions happen at the very start (header region).
        // For .tmd sequential reads this is correct since we never seek mid-stream.
        in.seek(pos - virtualExtra >= 0 ? pos - virtualExtra : pos);
        virtualFP = pos;
    }

    @Override
    public IndexInput slice(String sliceDescription, long offset, long length) throws IOException {
        return new Es812CodecSubstitutingInput(in.slice(sliceDescription, offset, length), codecReplacements);
    }

    @Override
    public Es812CodecSubstitutingInput clone() {
        Es812CodecSubstitutingInput c = (Es812CodecSubstitutingInput) super.clone();
        if (this.pendingBytes != null) {
            c.pendingBytes = this.pendingBytes.clone();
        }
        return c;
    }

    // ---- helpers ----

    private int readRawVInt() throws IOException {
        byte b = in.readByte();
        if ((b & 0x80) == 0) return b & 0x7F;
        int i = b & 0x7F;
        b = in.readByte(); i |= (b & 0x7F) << 7; if ((b & 0x80) == 0) return i;
        b = in.readByte(); i |= (b & 0x7F) << 14; if ((b & 0x80) == 0) return i;
        b = in.readByte(); i |= (b & 0x7F) << 21; if ((b & 0x80) == 0) return i;
        b = in.readByte(); return i | ((b & 0x0F) << 28);
    }

    private static int vIntLen(int v) {
        if (v < 0x80) return 1;
        if (v < 0x4000) return 2;
        if (v < 0x200000) return 3;
        if (v < 0x10000000) return 4;
        return 5;
    }

    private static byte[] encodeVInt(int v) {
        if (v < 0x80) return new byte[]{ (byte) v };
        if (v < 0x4000) return new byte[]{ (byte)(v | 0x80), (byte)(v >>> 7) };
        if (v < 0x200000) return new byte[]{ (byte)(v|0x80),(byte)((v>>>7)|0x80),(byte)(v>>>14) };
        if (v < 0x10000000) return new byte[]{ (byte)(v|0x80),(byte)((v>>>7)|0x80),(byte)((v>>>14)|0x80),(byte)(v>>>21) };
        return new byte[]{ (byte)(v|0x80),(byte)((v>>>7)|0x80),(byte)((v>>>14)|0x80),(byte)((v>>>21)|0x80),(byte)(v>>>28) };
    }
}
