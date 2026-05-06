package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;
import java.util.Map;

import shadow.lucene9.org.apache.lucene.store.Directory;
import shadow.lucene9.org.apache.lucene.store.FilterDirectory;
import shadow.lucene9.org.apache.lucene.store.IOContext;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * A {@link FilterDirectory} that wraps every opened {@link IndexInput} with
 * {@link Es812CodecSubstitutingInput} so that
 * {@link shadow.lucene9.org.apache.lucene.codecs.CodecUtil#checkHeader} reads
 * see Lucene codec names instead of ES812 codec names.
 *
 * <p>The substitution happens only inside {@link IndexInput#readString()} — no
 * bytes in the underlying file are changed, so file length and CRC32 footer
 * remain valid.
 */
public class CodecHeaderRewritingDirectory extends FilterDirectory {

    private final Map<String, String> codecReplacements;

    public CodecHeaderRewritingDirectory(Directory in, Map<String, String> codecReplacements) {
        super(in);
        this.codecReplacements = codecReplacements;
    }

    @Override
    public IndexInput openInput(String name, IOContext context) throws IOException {
        IndexInput raw = super.openInput(name, context);
        return new Es812CodecSubstitutingInput(raw, codecReplacements);
    }

    @Override
    public shadow.lucene9.org.apache.lucene.store.ChecksumIndexInput openChecksumInput(String name, IOContext context) throws IOException {
        IndexInput raw = in.openInput(name, context);
        Es812CodecSubstitutingInput substituted = new Es812CodecSubstitutingInput(raw, codecReplacements);
        // Pass a second raw clone so Es812ChecksumIndexInput can peek at the footer CRC.
        IndexInput rawForPeek = in.openInput(name, context);
        return new Es812ChecksumIndexInput(
            "Es812Checksum(" + name + ")", substituted, rawForPeek);
    }
}
