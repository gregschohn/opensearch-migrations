//! Randomised TCP-fragmentation properties.
//!
//! The Java suites pin fragmentation with a handful of hand-picked size lists plus 1000 seeded
//! random splits (`HeaderRemoverHandlerTest:147-176`). The property these tests state is the one
//! that actually matters and that no fixed list can cover: **the same byte stream split at
//! arbitrary boundaries must produce identical framing decisions and identical forwarded bytes.**
//!
//! The driver below is also a worked example of how the transport layer is meant to use this
//! crate: scan a read, split it at the end-of-message offsets the scan reports, and call
//! `begin_request` between messages — which is what makes rewriting stateless per request
//! (owner decision OD-3) instead of being driven by the response direction.

use bytes::Bytes;
use capture_config::{CaptureDirective, HttpStrictness, RequestLimits};
use capture_http_scan::{
    BodyFraming, HeaderRewriter, RequestScanner, ResponseClassifier, RewriteConfig, ScanEvent,
    SuppressionPolicy,
};
use proptest::prelude::*;

/// What a framing decision is, with the read-relative offsets removed: those necessarily differ
/// between fragmentations, everything else must not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Head {
        directive: CaptureDirective,
        critical_mutation: bool,
        body: BodyFraming,
    },
    MessageEnd {
        first_line_byte_length: u32,
        headers_byte_length: u32,
    },
}

fn normalise(event: &ScanEvent) -> Decision {
    match event {
        ScanEvent::HeadComplete(head) => Decision::Head {
            directive: head.directive,
            critical_mutation: head.critical_mutation,
            body: head.body,
        },
        ScanEvent::MessageComplete(end) => Decision::MessageEnd {
            first_line_byte_length: end.first_line_byte_length,
            headers_byte_length: end.headers_byte_length,
        },
    }
}

/// Four requests exercising every framing path: an empty body, a fixed body, a chunked body with
/// an extension and trailers, a suppressed health check, and an obs-folded header.
fn request_stream() -> Vec<u8> {
    let mut stream = String::new();
    stream.push_str("GET /thing HTTP/1.1\r\nHost: source\r\nAccept: */*\r\n\r\n");
    stream.push_str(concat!(
        "POST /test-index/_search HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Transfer-Encoding: chunked\r\n",
        "User-Agent: test-client\r\n",
        "\r\n",
        "24;ext=1\r\n",
        "{\"query\":{\"match_all\":{}},\"size\":10}\r\n",
        "0\r\n",
        "X-Checksum: abc\r\n",
        " folded\r\n",
        "\r\n",
    ));
    stream.push_str(concat!(
        "POST / HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "User-Agent: uploader\r\n",
        "Content-Length: 27\r\n",
        "\r\n",
        "field1=value1&field2=value2",
    ));
    stream.push_str(concat!(
        "PUT /doc/1 HTTP/1.1\r\n",
        "Host: a\r\n",
        "X-Long: first\r\n",
        "  second\r\n",
        "Content-Length: 4\r\n",
        "\r\n",
        "abcd",
    ));
    stream.into_bytes()
}

fn policy() -> SuppressionPolicy {
    SuppressionPolicy::builder()
        .header("user-Agent", ".*uploader.*")
        .expect("valid pattern")
        .build()
}

fn rewrite_config() -> RewriteConfig {
    RewriteConfig::new()
        .set_header("host", "my.host")
        .expect("valid")
        .remove_header("x-long")
        .expect("valid")
}

/// Drives the scanner and the rewriter over `stream`, cutting reads at `sizes` (cycled).
///
/// Returns the framing decisions and the bytes that would be forwarded to the source.
fn drive(stream: &[u8], sizes: &[usize]) -> (Vec<Decision>, Vec<u8>) {
    let mut scanner =
        RequestScanner::new(RequestLimits::default(), HttpStrictness::Strict, policy());
    let config = rewrite_config();
    let mut rewriter = HeaderRewriter::new(&config);
    rewriter.begin_request();

    let bytes = Bytes::copy_from_slice(stream);
    let mut decisions = Vec::new();
    let mut forwarded: Vec<Bytes> = Vec::new();
    let mut offset = 0usize;
    let mut index = 0usize;
    while offset < bytes.len() {
        let size = sizes[index % sizes.len()];
        index += 1;
        let end = offset.saturating_add(size).min(bytes.len());
        let chunk = bytes.slice(offset..end);
        let events = scanner.scan(&chunk).expect("the stream is well formed");

        let mut rewritten_upto = 0usize;
        for event in &events {
            decisions.push(normalise(event));
            if let ScanEvent::MessageComplete(message) = event {
                rewriter.rewrite(&chunk.slice(rewritten_upto..message.offset), &mut forwarded);
                rewritten_upto = message.offset;
                rewriter.begin_request();
            }
        }
        if rewritten_upto < chunk.len() {
            rewriter.rewrite(&chunk.slice(rewritten_upto..), &mut forwarded);
        }
        offset = end;
    }
    (decisions, forwarded.concat())
}

fn baseline() -> (Vec<Decision>, Vec<u8>) {
    drive(&request_stream(), &[usize::MAX])
}

#[test]
fn the_baseline_is_what_it_should_be() {
    // Stated explicitly so the property tests below cannot pass by comparing two identical bugs.
    let (decisions, forwarded) = baseline();
    assert_eq!(
        decisions,
        vec![
            Decision::Head {
                directive: CaptureDirective::Capture,
                critical_mutation: false,
                body: BodyFraming::Empty,
            },
            Decision::MessageEnd {
                first_line_byte_length: 19,
                headers_byte_length: 23,
            },
            Decision::Head {
                directive: CaptureDirective::Capture,
                critical_mutation: true,
                body: BodyFraming::Chunked,
            },
            Decision::MessageEnd {
                first_line_byte_length: 33,
                headers_byte_length: 64,
            },
            Decision::Head {
                directive: CaptureDirective::Drop,
                critical_mutation: true,
                body: BodyFraming::Fixed(27),
            },
            Decision::MessageEnd {
                first_line_byte_length: 15,
                headers_byte_length: 53,
            },
            Decision::Head {
                directive: CaptureDirective::Capture,
                critical_mutation: true,
                body: BodyFraming::Fixed(4),
            },
            Decision::MessageEnd {
                first_line_byte_length: 19,
                headers_byte_length: 45,
            },
        ]
    );

    let forwarded = String::from_utf8(forwarded).expect("ascii");
    // Every request got the rewrite, including the pipelined ones, and no body was touched.
    assert_eq!(forwarded.matches("host: my.host\r\n").count(), 4);
    assert!(!forwarded.contains("Host: source"));
    assert!(!forwarded.contains("Host: localhost"));
    assert!(!forwarded.contains("X-Long"));
    assert!(
        !forwarded.contains("  second"),
        "the folded continuation of a removed header must go with it"
    );
    assert!(forwarded.contains("{\"query\":{\"match_all\":{}},\"size\":10}"));
    assert!(forwarded.contains("field1=value1&field2=value2"));
    assert!(forwarded.ends_with("\r\n\r\nabcd"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// The property the Java suites approximate with 1000 seeded random splits.
    #[test]
    fn framing_and_forwarded_bytes_are_independent_of_fragmentation(
        sizes in prop::collection::vec(1usize..41, 1..24)
    ) {
        let (expected_decisions, expected_forwarded) = baseline();
        let (decisions, forwarded) = drive(&request_stream(), &sizes);
        prop_assert_eq!(decisions, expected_decisions);
        prop_assert_eq!(forwarded, expected_forwarded);
    }

    /// A rewriter configured with nothing must be byte-exact, at every fragmentation.
    #[test]
    fn an_unconfigured_rewriter_is_byte_exact(
        sizes in prop::collection::vec(1usize..41, 1..24)
    ) {
        let stream = request_stream();
        let config = RewriteConfig::new();
        let mut rewriter = HeaderRewriter::new(&config);
        rewriter.begin_request();
        let bytes = Bytes::copy_from_slice(&stream);
        let mut out = Vec::new();
        let mut offset = 0usize;
        let mut index = 0usize;
        while offset < bytes.len() {
            let end = offset.saturating_add(sizes[index % sizes.len()]).min(bytes.len());
            index += 1;
            rewriter.rewrite(&bytes.slice(offset..end), &mut out);
            offset = end;
        }
        prop_assert_eq!(out.concat(), stream);
    }

    /// Response classification: the pieces, their kinds, and their bytes must not depend on how
    /// the source's writes were cut up, and concatenating them must reproduce the stream exactly.
    #[test]
    fn response_classification_is_independent_of_write_boundaries(
        sizes in prop::collection::vec(1usize..29, 1..16)
    ) {
        let stream = concat!(
            "HTTP/1.1 100 Continue\r\n\r\n",
            "HTTP/1.1 103 Early Hints\r\nLink: </next>\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ).as_bytes();

        let mut expected = ResponseClassifier::new();
        let expected: Vec<(bool, Vec<u8>)> = expected
            .classify(stream)
            .pieces
            .iter()
            .map(|piece| (piece.is_interim(), piece.bytes().to_vec()))
            .collect();

        let mut classifier = ResponseClassifier::new();
        let mut observed: Vec<(bool, Vec<u8>)> = Vec::new();
        let mut offset = 0usize;
        let mut index = 0usize;
        while offset < stream.len() {
            let end = offset.saturating_add(sizes[index % sizes.len()]).min(stream.len());
            index += 1;
            let classification = classifier.classify(&stream[offset..end]);
            prop_assert!(!classification.hold_overflow);
            for piece in &classification.pieces {
                observed.push((piece.is_interim(), piece.bytes().to_vec()));
            }
            offset = end;
        }
        if let Some(residue) = classifier.flush_held() {
            observed.push((false, residue));
        }

        // Interim boundaries are identical however the writes were cut.
        prop_assert_eq!(
            observed.iter().filter(|(interim, _)| *interim).cloned().collect::<Vec<_>>(),
            expected.iter().filter(|(interim, _)| *interim).cloned().collect::<Vec<_>>()
        );
        // And no byte is lost, duplicated, or reordered.
        let flattened: Vec<u8> = observed.iter().flat_map(|(_, bytes)| bytes.clone()).collect();
        prop_assert_eq!(flattened, stream.to_vec());
    }
}
