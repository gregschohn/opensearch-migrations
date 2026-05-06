package org.opensearch.migrations.bulkload.lucene.version_9;

import java.io.IOException;
import java.util.Map;

import lombok.extern.slf4j.Slf4j;
import shadow.lucene9.org.apache.lucene.backward_codecs.lucene99.Lucene99PostingsFormat;
import shadow.lucene9.org.apache.lucene.codecs.FieldsConsumer;
import shadow.lucene9.org.apache.lucene.codecs.FieldsProducer;
import shadow.lucene9.org.apache.lucene.codecs.PostingsFormat;
import shadow.lucene9.org.apache.lucene.index.SegmentReadState;
import shadow.lucene9.org.apache.lucene.index.SegmentWriteState;
import shadow.lucene9.org.apache.lucene.store.Directory;

/**
 * PostingsFormat SPI adapter for the {@code ES812Postings} format used by Elasticsearch 8.12+.
 *
 * <p>ES 8.12+ stores per-field postings with a custom codec whose files are named using the
 * full per-field suffix (e.g. {@code _0_ES812Postings_0.tim}) and whose file headers embed
 * ES-specific codec names ({@code ES812PostingsWriterDoc}, {@code ES812PostingsWriterTerms},
 * etc.) instead of the stock Lucene names expected by {@link Lucene99PostingsFormat}.
 *
 * <p>The binary layout is otherwise identical to {@link Lucene99PostingsFormat}.  We wrap the
 * directory in a {@link CodecHeaderRewritingDirectory} that substitutes the codec names on the
 * fly, then delegate to the stock {@link Lucene99PostingsFormat} reader.
 */
@Slf4j
public class IgnorePsmPostings extends PostingsFormat {

    static final String FORMAT_NAME = "ES812Postings";

    /** Maps ES812 header codec names to Lucene99 equivalents expected by the stock reader. */
    private static final Map<String, String> CODEC_RENAMES = Map.of(
        "ES812PostingsWriterDoc",      "Lucene99PostingsWriterDoc",
        "ES812PostingsWriterPos",      "Lucene99PostingsWriterPos",
        "ES812PostingsWriterPay",      "Lucene99PostingsWriterPay",
        "ES812PostingsWriterTim",      "Lucene99PostingsWriterTim",
        "ES812PostingsWriterTip",      "Lucene99PostingsWriterTip",
        "ES812PostingsWriterTermMeta", "Lucene99PostingsWriterTermMeta",
        "ES812PostingsWriterTerms",    "Lucene90PostingsWriterTerms"
    );

    public IgnorePsmPostings() {
        super(FORMAT_NAME);
    }

    @Override
    public FieldsConsumer fieldsConsumer(SegmentWriteState state) throws IOException {
        throw new UnsupportedOperationException("ES812Postings is read-only fallback");
    }

    @Override
    public FieldsProducer fieldsProducer(SegmentReadState state) throws IOException {
        // PerFieldPostingsFormat sets state.segmentSuffix = "ES812Postings_<n>" and
        // state.segmentInfo.name = "_0" (the original segment name).
        // IndexFileNames.segmentFileName("_0", "ES812Postings_0", "tim") = "_0_ES812Postings_0.tim"
        // which is exactly what ES wrote.  We only need to wrap the directory to substitute
        // ES-branded codec header names with the Lucene99 names the stock reader expects.
        Directory rewritingDir = new CodecHeaderRewritingDirectory(state.directory, CODEC_RENAMES);
        SegmentReadState reScoped = new SegmentReadState(
            rewritingDir,
            state.segmentInfo,
            state.fieldInfos,
            state.context,
            state.segmentSuffix
        );

        try {
            return new Lucene99PostingsFormat().fieldsProducer(reScoped);
        } catch (IOException e) {
            log.warn("ES812Postings: Lucene99PostingsFormat fallback failed for segment {} suffix {}, returning empty. Cause: {}",
                state.segmentInfo.name, state.segmentSuffix, e.getMessage());
            return FallbackLuceneComponents.EMPTY_FIELDS_PRODUCER;
        }
    }
}
