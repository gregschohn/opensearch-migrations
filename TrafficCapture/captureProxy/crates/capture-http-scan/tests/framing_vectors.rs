//! Request-framing vectors ported from the Java suites named in transport-http spec §15.
//!
//! Every byte vector here is copied from the Java test that pins the behaviour, and the Java test
//! is named in a comment. Where a Java assertion was weaker than the behaviour it pins
//! (`firstLineByteLength > 0`, for instance), the ported assertion states the exact value, because
//! the exclusion rules for CR LF and the terminating blank line are the easiest thing in this crate
//! to get wrong.

use std::time::Duration;

use capture_config::{CaptureDirective, HttpStrictness, RequestLimits};
use capture_http_scan::request_scanner::{FramingError, LimitViolation, ScanError};
use capture_http_scan::{BodyFraming, RequestScanner, ScanEvent, SuppressionPolicy};

// ---------------------------------------------------------------- vectors

/// `ChunkedTransferEncodingCaptureTest:40-66`.
const REQUEST_HEADERS: &str = concat!(
    "POST /test-index/_search HTTP/1.1\r\n",
    "Host: localhost\r\n",
    "Transfer-Encoding: chunked\r\n",
    "User-Agent: test-client\r\n",
    "Accept: application/json\r\n",
    "Content-Type: application/json\r\n",
    "\r\n",
);
const JSON_BODY: &str = "{\"query\":{\"match_all\":{}},\"size\":10}";
const CHUNKED_BODY_PREFIX: &str = "24\r\n";
const CHUNK_TERMINATOR: &str = "0\r\n\r\n";
const SIMPLE_GET: &str = "GET / HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n";

/// `RequestForwardedCaptureTest:28-42`.
const GET: &str = "GET /thing HTTP/1.1\r\nHost: source\r\n\r\n";
const POST: &str = "POST /thing HTTP/1.1\r\nContent-Length: 2\r\n\r\nab";

/// `SimpleRequests.java:4-16`.
const SMALL_POST: &str = concat!(
    "POST / HTTP/1.1\r\n",
    "Host: localhost\r\n",
    "Content-Type: application/x-www-form-urlencoded\r\n",
    "Content-Length: 16\r\n",
    "\r\n",
    "FAKE_UPLOAD_DATA",
);
const HEALTH_CHECK: &str = concat!(
    "POST / HTTP/1.1\r\n",
    "Host: localhost\r\n",
    "User-Agent: uploader\r\n",
    "Content-Length: 27\r\n",
    "\r\n",
    "field1=value1&field2=value2",
);

fn chunked_request() -> String {
    format!("{REQUEST_HEADERS}{CHUNKED_BODY_PREFIX}{JSON_BODY}\r\n{CHUNK_TERMINATOR}")
}

// ---------------------------------------------------------------- harness

fn limits() -> RequestLimits {
    RequestLimits::default()
}

fn small_limits(header_bytes: u64, total_bytes: u64, request_line: Option<u64>) -> RequestLimits {
    RequestLimits::new(
        Duration::from_mins(1),
        header_bytes,
        total_bytes,
        Duration::from_mins(1),
        request_line,
    )
    .expect("valid limits")
}

/// Feeds `bytes` in fragments of `fragment` and returns every event, in order.
fn scan_all<P: capture_http_scan::CapturePredicate>(
    scanner: &mut RequestScanner<P>,
    bytes: &[u8],
    fragment: usize,
) -> Result<Vec<ScanEvent>, ScanError> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = offset.saturating_add(fragment).min(bytes.len());
        events.extend(scanner.scan(&bytes[offset..end])?);
        offset = end;
    }
    Ok(events)
}

fn message_ends(events: &[ScanEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::MessageComplete(end) => {
                Some((end.first_line_byte_length, end.headers_byte_length))
            }
            ScanEvent::HeadComplete(_) => None,
        })
        .collect()
}

// ---------------------------------------------------------------- T1: chunked framing

#[test]
fn t1_chunked_request_in_a_single_read() {
    // ChunkedTransferEncodingCaptureTest.testChunkedRequestInSingleRead (:174-222).
    let request = chunked_request();
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), usize::MAX).expect("well formed");

    assert_eq!(
        events.len(),
        2,
        "exactly one head and one end of message: {events:?}"
    );
    let ScanEvent::HeadComplete(head) = &events[0] else {
        panic!("the head completes first: {events:?}");
    };
    assert_eq!(head.body, BodyFraming::Chunked);
    assert_eq!(head.offset, REQUEST_HEADERS.len());
    assert!(head.critical_mutation, "POST is Critical Mutation Traffic");

    // Java asserts only `> 0` and `!= -1`; the exact values are the arithmetic of spec §3.8.
    assert_eq!(message_ends(&events), vec![(33, 118)]);
    let ScanEvent::MessageComplete(end) = &events[1] else {
        panic!("the message ends last: {events:?}");
    };
    assert_eq!(
        end.offset,
        request.len(),
        "the chunk terminator must not leave trailing bytes in the request"
    );
}

#[test]
fn t1_chunked_request_split_at_the_terminator() {
    // ChunkedTransferEncodingCaptureTest.testChunkedRequestSplitAtTerminator (:231-282): the
    // terminator arrives in its own read and must still end the message there, with nothing after.
    let head_and_body = format!("{REQUEST_HEADERS}{CHUNKED_BODY_PREFIX}{JSON_BODY}\r\n");
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let first = scanner.scan(head_and_body.as_bytes()).expect("well formed");
    assert_eq!(first.len(), 1, "no end of message yet: {first:?}");
    assert!(scanner.is_request_in_progress());

    let second = scanner
        .scan(CHUNK_TERMINATOR.as_bytes())
        .expect("well formed");
    assert_eq!(message_ends(&second), vec![(33, 118)]);
    assert_eq!(
        second,
        vec![ScanEvent::MessageComplete(
            capture_http_scan::MessageComplete {
                offset: CHUNK_TERMINATOR.len(),
                first_line_byte_length: 33,
                headers_byte_length: 118,
            }
        )]
    );
    assert!(!scanner.is_request_in_progress());
    assert_eq!(scanner.requests_completed(), 1);
}

#[test]
fn t1_chunked_request_followed_by_a_normal_request() {
    // ChunkedTransferEncodingCaptureTest.testChunkedRequestFollowedByNormalRequest (:288-327):
    // exactly two end-of-message indicators, both with positive lengths.
    let stream = format!("{}{SIMPLE_GET}", chunked_request());
    for fragment in [1usize, 7, 64, usize::MAX] {
        let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
        let events = scan_all(&mut scanner, stream.as_bytes(), fragment).expect("well formed");
        assert_eq!(
            message_ends(&events),
            vec![(33, 118), (14, 26)],
            "fragment {fragment}"
        );
        assert_eq!(scanner.requests_completed(), 2);
    }
}

// ---------------------------------------------------------------- T2: end-of-message arithmetic

#[test]
fn t2_end_of_message_lengths_exclude_crlf_and_the_blank_line() {
    // NettyScanningHttpProxyTest:152-154 asserts firstLineByteLength == 14 and
    // headersByteLength == 711 for the request at :50-74. That request also carries the test HTTP
    // client's own headers, which the Java assertion's `.*?` allows for and which account for
    // exactly 711 - 595 = 116 bytes. Both halves are pinned here: the 595 of the headers the Java
    // test spells out, and the 711 total once a 116-byte filler stands in for the client's.
    let mut request = String::from("GET / HTTP/1.1\r\nHost: localhost\r\nUser-Agent: UnitTest\r\n");
    for index in 0..20 {
        use std::fmt::Write;
        write!(request, "DumbAndLongHeaderValue-{index}: {index}\r\n")
            .expect("writing to a String cannot fail");
    }
    let spelled_out = format!("{request}\r\n");

    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, spelled_out.as_bytes(), usize::MAX).expect("well formed");
    assert_eq!(
        message_ends(&events),
        vec![(14, 595)],
        "GET / HTTP/1.1 is 14 bytes; the 23 spelled-out header lines sum to 595 excluding CR LF"
    );

    let filler = format!("X-Filler: {}", "p".repeat(106));
    assert_eq!(filler.len(), 116);
    let with_client_headers = format!("{request}{filler}\r\n\r\n");
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events =
        scan_all(&mut scanner, with_client_headers.as_bytes(), usize::MAX).expect("well formed");
    assert_eq!(message_ends(&events), vec![(14, 711)]);
}

#[test]
fn t2_lengths_are_identical_however_the_head_is_fragmented() {
    let request = format!("{}{SIMPLE_GET}", chunked_request());
    for fragment in [1usize, 2, 3, 5, 13, 97] {
        let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
        let events = scan_all(&mut scanner, request.as_bytes(), fragment).expect("well formed");
        assert_eq!(
            message_ends(&events),
            vec![(33, 118), (14, 26)],
            "fragment {fragment}"
        );
    }
}

#[test]
fn an_obs_folded_header_counts_each_physical_line() {
    // Netty accepts a folded header and joins the parts with one space (HOD:755-761); each
    // physical line is a separate HeaderParser.parse call, so each contributes its own length.
    let request = "GET / HTTP/1.1\r\nHost: a\r\n continued\r\n\r\n";
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), usize::MAX).expect("folding accepted");
    // "Host: a" is 7, " continued" is 10.
    assert_eq!(message_ends(&events), vec![(14, 17)]);
}

#[test]
fn an_obs_folded_framing_header_is_unfolded_before_it_is_read() {
    let request = "POST / HTTP/1.1\r\nContent-Length:\r\n 4\r\n\r\nabcd";
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), 1).expect("folding accepted");
    let ScanEvent::HeadComplete(head) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        head.body,
        BodyFraming::Fixed(4),
        "the folded Content-Length must still frame the body"
    );
    assert_eq!(message_ends(&events).len(), 1);
}

// ---------------------------------------------------------------- framing of ordinary requests

#[test]
fn a_request_with_no_framing_headers_has_an_empty_body_whatever_the_method() {
    // HOD:404-412: for a request, no Content-Length and no Transfer-Encoding means a zero-length
    // body. There is no method-based body suppression anywhere in the proxy.
    for (method, expected_critical) in [("GET", false), ("DELETE", true), ("TRACE", false)] {
        let request = format!("{method} / HTTP/1.1\r\nHost: a\r\n\r\n");
        let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
        let events = scan_all(&mut scanner, request.as_bytes(), usize::MAX).expect("well formed");
        let ScanEvent::HeadComplete(head) = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(head.body, BodyFraming::Empty, "{method}");
        assert_eq!(head.critical_mutation, expected_critical, "{method}");
        assert_eq!(
            events.len(),
            2,
            "the message ends with its head: {events:?}"
        );
    }
}

#[test]
fn a_get_with_a_content_length_does_get_a_body() {
    let request = "GET / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc";
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), usize::MAX).expect("well formed");
    let ScanEvent::HeadComplete(head) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(head.body, BodyFraming::Fixed(3));
    assert_eq!(events.len(), 2);
}

#[test]
fn pipelined_requests_each_report_their_own_boundary() {
    // The protocol document excludes pipelining, but owner decision OD-3 requires header rewriting
    // to be correct for pipelined requests, which is only possible if framing reports every
    // boundary. So pipelining is framed, not refused.
    let stream = format!("{GET}{POST}");
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, stream.as_bytes(), usize::MAX).expect("well formed");
    let offsets: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            ScanEvent::MessageComplete(end) => Some(end.offset),
            ScanEvent::HeadComplete(_) => None,
        })
        .collect();
    assert_eq!(offsets, vec![GET.len(), GET.len() + POST.len()]);
    assert_eq!(message_ends(&events), vec![(19, 12), (20, 17)]);
}

#[test]
fn leading_crlf_left_over_from_a_previous_message_is_tolerated() {
    // HOD:1242-1264 skips control and whitespace bytes before a request line.
    let stream = format!("\r\n\r\n{SIMPLE_GET}");
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, stream.as_bytes(), 1).expect("well formed");
    assert_eq!(message_ends(&events), vec![(14, 26)]);
}

#[test]
fn trailers_are_framed_and_end_the_message() {
    let request = format!(
        "{REQUEST_HEADERS}{CHUNKED_BODY_PREFIX}{JSON_BODY}\r\n0\r\nX-Checksum: abc\r\n Continued\r\n\r\n"
    );
    for fragment in [1usize, 4, usize::MAX] {
        let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
        let events = scan_all(&mut scanner, request.as_bytes(), fragment).expect("well formed");
        assert_eq!(
            message_ends(&events),
            vec![(33, 118)],
            "fragment {fragment}"
        );
    }
}

#[test]
fn chunk_extensions_are_parsed_and_ignored() {
    let request = format!("{REQUEST_HEADERS}24;name=value\r\n{JSON_BODY}\r\n0;final\r\n\r\n");
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), usize::MAX).expect("well formed");
    assert_eq!(message_ends(&events), vec![(33, 118)]);
}

// ---------------------------------------------------------------- T5: limits and OD-1 framing

#[test]
fn t5_the_header_byte_limit_closes_the_connection() {
    // ConditionallyReliableLoggingHttpHandlerTest:267-298: limits (1 min, 32, 1024) and a request
    // with a 49-byte header line.
    let request = "GET / HTTP/1.1\r\nX-Large: 1234567890123456789012345678901234567890\r\n\r\n";
    for fragment in [1usize, usize::MAX] {
        let mut scanner = RequestScanner::capture_everything(
            small_limits(32, 1024, None),
            HttpStrictness::Strict,
        );
        let error = scan_all(&mut scanner, request.as_bytes(), fragment)
            .expect_err("the header budget is exceeded");
        assert!(
            matches!(error, ScanError::Limit(LimitViolation::HeaderBytes { .. })),
            "fragment {fragment}: {error:?}"
        );
        assert!(scanner.is_faulted());
        assert!(matches!(
            scanner.scan(b"more"),
            Err(ScanError::AlreadyFaulted)
        ));
    }
}

#[test]
fn t5_a_complete_request_over_the_total_byte_limit_is_refused_before_its_end_of_message() {
    // ConditionallyReliableLoggingHttpHandlerTest:334-366: limits (1 min, 128, 150), a POST with
    // Content-Length 200 and a 200-byte body. The violation must be reported instead of the end of
    // message, which is what stops the request being forwarded (spec §4.3).
    let request = format!(
        "POST / HTTP/1.1\r\nContent-Length: 200\r\n\r\n{}",
        "x".repeat(200)
    );
    for fragment in [1usize, usize::MAX] {
        let mut scanner = RequestScanner::capture_everything(
            small_limits(128, 150, None),
            HttpStrictness::Strict,
        );
        let error = scan_all(&mut scanner, request.as_bytes(), fragment)
            .expect_err("the total budget is exceeded");
        assert!(
            matches!(error, ScanError::Limit(LimitViolation::TotalBytes { .. })),
            "fragment {fragment}: {error:?}"
        );
        assert_eq!(
            scanner.requests_completed(),
            0,
            "no end of message may be reported for a request that was refused"
        );
    }
}

#[test]
fn t5_the_byte_counter_resets_between_keep_alive_requests() {
    // ConditionallyReliableLoggingHttpHandlerTest:402-431: limits (1 min, 32, 32) and two 21-byte
    // requests; both are accepted and the connection survives.
    let mut scanner =
        RequestScanner::capture_everything(small_limits(32, 32, None), HttpStrictness::Strict);
    let events = scan_all(
        &mut scanner,
        b"GET /one HTTP/1.1\r\n\r\nGET /two HTTP/1.1\r\n\r\n",
        1,
    )
    .expect("each request is within budget on its own");
    assert_eq!(message_ends(&events), vec![(17, 0), (17, 0)]);
    assert_eq!(scanner.request_bytes(), 0);
}

#[test]
fn od1_a_request_line_over_its_budget_is_terminal() {
    // Java leaves Netty's maxInitialLineLength at 4096 and never recognises
    // TooLongHttpLineException, so this request is tunnelled uncaptured instead (spec §3.7).
    let request = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(4096));
    for fragment in [1usize, usize::MAX] {
        let mut scanner = RequestScanner::capture_everything(
            small_limits(8192, 1 << 20, Some(4096)),
            HttpStrictness::Strict,
        );
        let error = scan_all(&mut scanner, request.as_bytes(), fragment)
            .expect_err("the request line is over budget");
        assert!(
            matches!(
                error,
                ScanError::Framing(FramingError::RequestLineTooLong { limit: 4096 })
            ),
            "fragment {fragment}: {error:?}"
        );
    }
}

#[test]
fn od1_a_long_request_line_inside_the_raised_default_budget_still_works() {
    // The point of raising the limit: a 5000-byte _search URI is routine and must keep working.
    let request = format!(
        "GET /_search?q={} HTTP/1.1\r\nHost: a\r\n\r\n",
        "a".repeat(5000)
    );
    let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    let events = scan_all(&mut scanner, request.as_bytes(), 128).expect("well within the budget");
    assert_eq!(message_ends(&events).len(), 1);
}

#[test]
fn od1_a_bare_lf_is_terminal_in_strict_mode_and_accepted_in_lenient_mode() {
    let request = "GET / HTTP/1.1\nHost: a\n\n";
    let mut strict = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
    assert!(matches!(
        scan_all(&mut strict, request.as_bytes(), 1),
        Err(ScanError::Framing(FramingError::BareLineFeed))
    ));

    let mut lenient =
        RequestScanner::capture_everything(limits(), HttpStrictness::LenientLineEndings);
    let events = scan_all(&mut lenient, request.as_bytes(), 1).expect("lenient accepts bare LF");
    assert_eq!(message_ends(&events), vec![(14, 7)]);
}

#[test]
fn od2_lenient_mode_relaxes_line_endings_and_nothing_else() {
    // OD-2: LenientLineEndings accepts a bare LF and nothing else. A Transfer-Encoding/
    // Content-Length conflict and a duplicate Content-Length stay terminal.
    let conflict = "POST / HTTP/1.1\nTransfer-Encoding: chunked\nContent-Length: 4\n\n";
    let mut lenient =
        RequestScanner::capture_everything(limits(), HttpStrictness::LenientLineEndings);
    assert!(matches!(
        scan_all(&mut lenient, conflict.as_bytes(), usize::MAX),
        Err(ScanError::Framing(
            FramingError::TransferEncodingWithContentLength
        ))
    ));

    let duplicate = "POST / HTTP/1.1\nContent-Length: 1\nContent-Length: 2\n\n";
    let mut lenient =
        RequestScanner::capture_everything(limits(), HttpStrictness::LenientLineEndings);
    assert!(matches!(
        scan_all(&mut lenient, duplicate.as_bytes(), usize::MAX),
        Err(ScanError::Framing(FramingError::DuplicateContentLength))
    ));
}

/// A refused request: a label, the bytes, and a predicate on the error it must produce.
type RefusalCase = (&'static str, &'static str, fn(&ScanError) -> bool);

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table of refusal vectors reads better than several"
)]
fn od1_ambiguous_and_malformed_framing_is_terminal() {
    let cases: Vec<RefusalCase> = vec![
        (
            "duplicate Content-Length",
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nxx",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::DuplicateContentLength)
                )
            },
        ),
        (
            "comma-separated Content-Length",
            "POST / HTTP/1.1\r\nContent-Length: 1,1\r\n\r\nx",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::DuplicateContentLength)
                )
            },
        ),
        (
            "signed Content-Length",
            "POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\nx",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidContentLength { .. })
                )
            },
        ),
        (
            "negative Content-Length",
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\nx",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidContentLength { .. })
                )
            },
        ),
        (
            "non-numeric Content-Length",
            "POST / HTTP/1.1\r\nContent-Length: 0x10\r\n\r\nx",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidContentLength { .. })
                )
            },
        ),
        (
            "Transfer-Encoding and Content-Length",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\nabcd",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::TransferEncodingWithContentLength)
                )
            },
        ),
        (
            "unframeable Transfer-Encoding",
            "POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\nabcd",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::UnsupportedTransferEncoding { .. })
                )
            },
        ),
        (
            "non-hex chunk size",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidChunkSize { .. })
                )
            },
        ),
        (
            "empty chunk size",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidChunkSize { .. })
                )
            },
        ),
        (
            "bad chunk terminator",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXX",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidChunkTermination)
                )
            },
        ),
        (
            "bare-LF chunk terminator in strict mode",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::InvalidChunkTermination)
                )
            },
        ),
        (
            "chunk size over i32::MAX",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n80000000\r\n",
            |error| matches!(error, ScanError::Framing(FramingError::ChunkSizeOverflow)),
        ),
        (
            "Content-Length in a trailer",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 4\r\n\r\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::MalformedTrailer { .. })
                )
            },
        ),
        (
            "no colon in a header",
            "GET / HTTP/1.1\r\nHostx\r\n\r\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::MalformedHeaders { .. })
                )
            },
        ),
        (
            "whitespace before a header colon",
            "GET / HTTP/1.1\r\nHost : a\r\n\r\n",
            |error| {
                matches!(
                    error,
                    ScanError::Framing(FramingError::MalformedHeaders { .. })
                )
            },
        ),
        ("two-field request line", "GET /\r\n\r\n", |error| {
            matches!(
                error,
                ScanError::Framing(FramingError::MalformedRequestLine { .. })
            )
        }),
        ("unparseable version", "GET / HTTP/x.y\r\n\r\n", |error| {
            matches!(
                error,
                ScanError::Framing(FramingError::MalformedRequestLine { .. })
            )
        }),
    ];

    for (label, request, expected) in cases {
        for fragment in [1usize, usize::MAX] {
            let mut scanner = RequestScanner::capture_everything(limits(), HttpStrictness::Strict);
            let error = scan_all(&mut scanner, request.as_bytes(), fragment)
                .expect_err(&format!("{label} must be refused (fragment {fragment})"));
            assert!(
                expected(&error),
                "{label} (fragment {fragment}) produced {error:?}"
            );
            assert!(scanner.is_faulted(), "{label}");
        }
    }
}

#[test]
fn a_bare_lf_chunk_terminator_is_accepted_only_in_lenient_mode() {
    // DEVIATION: Netty's chunk-size line and chunk terminator are strict regardless of
    // `strictLineParsing` (HOD:476, HOD:513-524). Applying the configured strictness uniformly to
    // every HTTP/1 line is simpler to explain and leaves the strict default — which is what
    // production runs — identical to Netty.
    let request = "POST / HTTP/1.1\nTransfer-Encoding: chunked\n\n2\nab\n0\n\n";
    let mut lenient =
        RequestScanner::capture_everything(limits(), HttpStrictness::LenientLineEndings);
    let events = scan_all(&mut lenient, request.as_bytes(), 1).expect("lenient accepts bare LF");
    assert_eq!(message_ends(&events), vec![(15, 26)]);
}

// ---------------------------------------------------------------- T6: suppression through the scan

#[test]
fn t6_a_health_check_is_dropped_and_a_real_post_is_captured() {
    // ConditionallyReliableLoggingHttpHandlerTest.testThatHealthCheckCaptureCanBeSuppressed
    // (:777-866) with predicate suppressCaptureHeaderPairs(Map.of("user-Agent", ".*uploader.*")).
    let policy = SuppressionPolicy::builder()
        .header("user-Agent", ".*uploader.*")
        .expect("valid pattern")
        .build();
    let stream = format!("{HEALTH_CHECK}{SMALL_POST}");
    for fragment in [1usize, 11, usize::MAX] {
        let mut scanner = RequestScanner::new(limits(), HttpStrictness::Strict, policy.clone());
        let events = scan_all(&mut scanner, stream.as_bytes(), fragment).expect("well formed");
        let directives: Vec<CaptureDirective> = events
            .iter()
            .filter_map(|event| match event {
                ScanEvent::HeadComplete(head) => Some(head.directive),
                ScanEvent::MessageComplete(_) => None,
            })
            .collect();
        assert_eq!(
            directives,
            vec![CaptureDirective::Drop, CaptureDirective::Capture],
            "fragment {fragment}"
        );
        assert_eq!(scanner.requests_completed(), 2, "fragment {fragment}");
    }
}

#[test]
fn t6_the_h2c_prior_knowledge_preface_is_suppressed_and_then_refused() {
    // Spec §5.3: Java suppresses the preface's capture (hardcoded protocolPattern "HTTP/2.*") and
    // forwards it, then parses the following HTTP/2 frames as nonsense requests and degrades into
    // the tunnel of §3.7. Suppression is preserved here; the degradation is not — the frame bytes
    // are refused under OD-1.
    let policy = SuppressionPolicy::builder().build();
    let mut scanner = RequestScanner::new(limits(), HttpStrictness::Strict, policy);
    let events = scanner
        .scan(b"PRI * HTTP/2.0\r\n\r\n")
        .expect("the preface parses as a request");
    let ScanEvent::HeadComplete(head) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(head.directive, CaptureDirective::Drop);
    assert_eq!(head.body, BodyFraming::Empty);
    assert_eq!(message_ends(&events).len(), 1);

    let error = scanner
        .scan(b"SM\r\n\r\n")
        .expect_err("the rest of the preface is not an HTTP/1 request");
    assert!(matches!(
        error,
        ScanError::Framing(FramingError::MalformedRequestLine { .. })
    ));
}

#[test]
fn t6_a_suppression_header_that_was_not_configured_cannot_be_matched() {
    // The scanner retains only the configured names, so a request that would match a pattern on an
    // unretained header is captured.
    let policy = SuppressionPolicy::builder()
        .header("x-configured", ".*drop.*")
        .expect("valid pattern")
        .build();
    let mut scanner = RequestScanner::new(limits(), HttpStrictness::Strict, policy);
    let events = scanner
        .scan(b"GET / HTTP/1.1\r\nX-Other: please-drop-me\r\n\r\n")
        .expect("well formed");
    let ScanEvent::HeadComplete(head) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(head.directive, CaptureDirective::Capture);
}
