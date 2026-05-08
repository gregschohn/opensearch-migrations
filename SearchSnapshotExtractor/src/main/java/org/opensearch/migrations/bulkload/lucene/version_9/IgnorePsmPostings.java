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
import shadow.lucene9.org.apache.lucene.store.FilterDirectory;
import shadow.lucene9.org.apache.lucene.store.FilterIndexInput;
import shadow.lucene9.org.apache.lucene.store.IOContext;
import shadow.lucene9.org.apache.lucene.store.IndexInput;

/**
 * PostingsFormat SPI adapter for the {@code ES812Postings} format used by Elasticsearch 8.12+.
 *
 * <p>ES 8.12+ stores per-field postings with a custom codec whose files are named with the
 * full per-field suffix (e.g. {@code _0_ES812Postings_0.tim}) and whose file headers embed
 * ES-specific codec names ({@code ES812PostingsWriterDoc/Pos/Pay/Terms}) instead of the stock
 * Lucene names {@link Lucene99PostingsFormat} expects. The binary layout is otherwise
 * identical to {@link Lucene99PostingsFormat}, so we wrap the directory with a thin filter
 * that rewrites those codec name strings on read and delegate to the stock reader.
 *
 * <p>The substitution is applied in {@code DataInput.readString()} only — raw bytes are
 * never modified, so {@code BufferedChecksumIndexInput}'s footer CRC equality holds without
 * any custom checksum input. Only this file 9 path needs the rewrite: ES 5/6/7 predate
 * ES812; ES 9.x's version_10 path simply returns {@link FallbackLuceneComponents#EMPTY_FIELDS_PRODUCER}
 * because Lucene 10 readers never need to decode postings during snapshot reconstruction
 * (stored fields suffice).
 */
@Slf4j
public class IgnorePsmPostings extends PostingsFormat {

    static final String FORMAT_NAME = "ES812Postings";

    /**
     * Maps ES812 codec name strings to the Lucene names that {@code Lucene99PostingsReader}
     * (Doc/Pos/Pay) and {@code Lucene40BlockTreeTermsReader} (Terms) check at header time.
     */
    private static final Map<String, String> CODEC_RENAMES = Map.of(
        "ES812PostingsWriterDoc",   "Lucene99PostingsWriterDoc",
        "ES812PostingsWriterPos",   "Lucene99PostingsWriterPos",
        "ES812PostingsWriterPay",   "Lucene99PostingsWriterPay",
        "ES812PostingsWriterTerms", "Lucene90PostingsWriterTerms"
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
        // state.segmentInfo.name = "_0" — IndexFileNames.segmentFileName("_0", "ES812Postings_0", "tim")
        // resolves to "_0_ES812Postings_0.tim", which is what ES wrote. We only need to substitute
        // the codec name strings inside those files for the stock reader to accept them.
        SegmentReadState reScoped = new SegmentReadState(
            new RewritingDirectory(state.directory),
            state.segmentInfo, state.fieldInfos, state.context, state.segmentSuffix);

        try {
            return new Lucene99PostingsFormat().fieldsProducer(reScoped);
        } catch (IOException e) {
            log.warn("ES812Postings: Lucene99PostingsFormat fallback failed for segment {} suffix {}, returning empty. Cause: {}",
                state.segmentInfo.name, state.segmentSuffix, e.getMessage());
            return FallbackLuceneComponents.EMPTY_FIELDS_PRODUCER;
        }
    }

    /** Wraps every opened input with a {@link RewritingInput}; everything else delegates. */
    private static final class RewritingDirectory extends FilterDirectory {
        RewritingDirectory(Directory in) {
            super(in);
        }

        @Override
        public IndexInput openInput(String name, IOContext context) throws IOException {
            return new RewritingInput(super.openInput(name, context));
        }
    }

    /**
     * Substitutes ES812 codec names with their Lucene equivalents inside {@link #readString()}.
     * {@code readByte}/{@code readBytes} pass through untouched, so the wrapping
     * {@code BufferedChecksumIndexInput} CRCs the raw byte stream and footer equality holds
     * naturally — no custom checksum input needed.
     */
    private static final class RewritingInput extends FilterIndexInput {
        RewritingInput(IndexInput in) {
            super("Es812CodecRewriting(" + in + ")", in);
        }

        @Override
        public String readString() throws IOException {
            String raw = super.readString();
            return CODEC_RENAMES.getOrDefault(raw, raw);
        }

        @Override
        public IndexInput slice(String sliceDescription, long offset, long length) throws IOException {
            return new RewritingInput(in.slice(sliceDescription, offset, length));
        }

        // FilterIndexInput.in is protected final, so super.clone()'s shallow copy would share
        // the delegate with the original — violating IndexInput's contract that clones have
        // independent position state. Copy-factory style (mirrors Lucene's own
        // EndiannessReverserIndexInput) clones the delegate explicitly.
        @SuppressWarnings("java:S1182")
        @Override
        public IndexInput clone() {
            return new RewritingInput(in.clone());
        }
    }
}
