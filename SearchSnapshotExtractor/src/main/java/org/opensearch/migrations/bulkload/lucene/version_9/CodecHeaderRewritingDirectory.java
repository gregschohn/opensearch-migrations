package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;
import java.util.Map;

import shadow.lucene9.org.apache.lucene.store.Directory;
import shadow.lucene9.org.apache.lucene.store.FilterDirectory;
import shadow.lucene9.org.apache.lucene.store.IOContext;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * A {@link FilterDirectory} that rewrites codec name strings as they are read by
 * {@code CodecUtil.checkIndexHeader} so the stock Lucene 9.9 reader accepts files
 * written by Elasticsearch's ES812 codec.
 *
 * <p>The substitution is implemented as an override of {@link IndexInput#readString()}
 * — the running CRC computed by {@code BufferedChecksumIndexInput} hashes raw
 * {@code readByte()} traffic, so the footer CRC equality holds without any
 * special handling. {@code openChecksumInput} therefore inherits the default
 * {@link Directory} implementation (it wraps {@code openInput} with a
 * {@code BufferedChecksumIndexInput}), and we do not need a custom checksum input.
 */
public class CodecHeaderRewritingDirectory extends FilterDirectory {

    private final Map<String, String> codecReplacements;

    public CodecHeaderRewritingDirectory(Directory in, Map<String, String> codecReplacements) {
        super(in);
        this.codecReplacements = codecReplacements;
    }

    @Override
    public IndexInput openInput(String name, IOContext context) throws IOException {
        return new CodecNameSubstitutingInput(super.openInput(name, context), codecReplacements);
    }
}
