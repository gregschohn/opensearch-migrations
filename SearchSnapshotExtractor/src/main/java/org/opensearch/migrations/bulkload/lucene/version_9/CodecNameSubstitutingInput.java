package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;
import java.util.Map;

import shadow.lucene9.org.apache.lucene.store.FilterIndexInput;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * A {@link FilterIndexInput} that substitutes codec name strings on the fly via
 * {@link #readString()}. Used to make stock {@code Lucene99PostingsReader} accept
 * files written by Elasticsearch's ES812 codec, which embeds {@code ES812Postings*}
 * codec names instead of the {@code Lucene99Postings*} names the reader checks for.
 *
 * <p>Unlike a byte-stream rewriter, this approach leaves the underlying byte sequence
 * (and therefore the CRC computed over it) untouched. {@link #readByte()} and
 * {@link #readBytes(byte[], int, int)} pass through verbatim, so a wrapping
 * {@code BufferedChecksumIndexInput} sees raw bytes and the footer CRC equality
 * holds at end-of-stream without any special handling.
 *
 * <p>{@code DataInput.readString} is the only path through which Lucene reads
 * variable-length UTF-8 strings, so overriding it once catches every codec-name
 * check ({@code CodecUtil.checkIndexHeader}, {@code checkHeader},
 * {@code checkIndexHeaderSuffix}). All other binary fields read past it
 * (versions, segment IDs, byte offsets, etc.) are unaffected.
 */
public class CodecNameSubstitutingInput extends FilterIndexInput {

    private final Map<String, String> codecReplacements;

    public CodecNameSubstitutingInput(IndexInput in, Map<String, String> codecReplacements) {
        super("CodecNameSubstituting(" + in + ")", in);
        this.codecReplacements = codecReplacements;
    }

    @Override
    public String readString() throws IOException {
        String raw = super.readString();
        String replacement = codecReplacements.get(raw);
        return replacement != null ? replacement : raw;
    }

    @Override
    public IndexInput slice(String sliceDescription, long offset, long length) throws IOException {
        return new CodecNameSubstitutingInput(in.slice(sliceDescription, offset, length), codecReplacements);
    }

    @Override
    public CodecNameSubstitutingInput clone() {
        // Inherits FilterIndexInput's super.clone(); codecReplacements is shared (immutable map).
        return (CodecNameSubstitutingInput) super.clone();
    }
}
