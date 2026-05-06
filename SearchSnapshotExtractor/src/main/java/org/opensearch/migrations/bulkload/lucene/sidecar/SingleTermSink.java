package org.opensearch.migrations.bulkload.lucene.sidecar;

import java.io.IOException;

/**
 * Sink variant for fields that are guaranteed single-valued and DOCS-only
 * (no positions, no offsets). The walker calls {@link #registerTerm} once per
 * distinct term in {@code TermsEnum.next()} order and then {@link #accept}
 * once per (termId, docId). Since the field is single-valued, each docId is
 * written at most once across the whole walk.
 *
 * <p>Kept deliberately separate from {@link PostingsSink} because the positional
 * sink carries the {@code positions}/{@code startOffsets}/{@code endOffsets}
 * contract, and we don't want to force the single-term fast-path through the
 * 20-byte-record {@link SidecarBuilder} pipeline just to drop 16 bytes per
 * entry. The single-term on-disk representation is a flat {@code int[maxDoc]}
 * of termIds, sentinel {@code -1} for docs with no value.
 */
public interface SingleTermSink {

    /**
     * Registers a distinct term. Called exactly once per term, in ascending
     * sorted-bytes order. The returned non-negative int is the dense termId
     * the sink will accept on subsequent {@link #accept} calls. The sink MAY
     * write term-dictionary bytes eagerly, or buffer them — callers must not
     * assume either.
     */
    int registerTerm(BytesRefLike term) throws IOException;

    /**
     * Records that {@code termId} occurs in {@code docId}. Single-valued
     * contract: each docId should be called at most once across the whole
     * walk. Out-of-range docIds are silently ignored (mirrors
     * {@link SidecarBuilder#accept}).
     */
    void accept(int termId, int docId) throws IOException;
}
