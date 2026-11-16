package main

import (
	"bytes"
	"fmt"
	"math/rand"
	"strings"
)

// ---------------------------------------------------------------------------
// Request corpus.
//
// Shapes are modelled on the production report
// (docs/captureAndReplay/loadTestRun-2026-10-13-160k.md): a single-document
// `_bulk` of about 2 KiB of body and about 3.9 KB per request including
// headers, signed with SigV4 including a session token. The header block below
// is sized to reproduce that ~1.9 KB of headers so that the proxy's HTTP
// decode and capture path sees a production-shaped byte count, not a toy one.
// ---------------------------------------------------------------------------

type ReqKind int

const (
	KindGetKeepAlive ReqKind = iota
	KindBulkPost             // critical fixed-length mutation
	KindPut                  // critical fixed-length mutation
	KindDelete               // critical mutation, no body
	KindPatch                // critical fixed-length mutation
	KindChunkedTrailers      // chunked request with trailers
)

func (k ReqKind) String() string {
	switch k {
	case KindGetKeepAlive:
		return "get"
	case KindBulkPost:
		return "bulk_post"
	case KindPut:
		return "put"
	case KindDelete:
		return "delete"
	case KindPatch:
		return "patch"
	case KindChunkedTrailers:
		return "chunked_trailers"
	}
	return "unknown"
}

// A mix entry: kind plus relative weight.
type MixEntry struct {
	Kind   ReqKind
	Weight int
}

// parseMix reads "bulk_post=70,get=20,put=4,delete=2,patch=2,chunked_trailers=2".
func parseMix(s string) ([]MixEntry, error) {
	byName := map[string]ReqKind{
		"get": KindGetKeepAlive, "bulk_post": KindBulkPost, "put": KindPut,
		"delete": KindDelete, "patch": KindPatch, "chunked_trailers": KindChunkedTrailers,
	}
	var out []MixEntry
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		kv := strings.SplitN(part, "=", 2)
		if len(kv) != 2 {
			return nil, fmt.Errorf("bad mix entry %q", part)
		}
		k, ok := byName[kv[0]]
		if !ok {
			return nil, fmt.Errorf("unknown request kind %q", kv[0])
		}
		var w int
		if _, err := fmt.Sscanf(kv[1], "%d", &w); err != nil || w < 0 {
			return nil, fmt.Errorf("bad weight in %q", part)
		}
		if w > 0 {
			out = append(out, MixEntry{Kind: k, Weight: w})
		}
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("empty mix")
	}
	return out, nil
}

// Representative SigV4-shaped header block. The security token is deliberately
// long because the production run signed with a session token, and that token
// is a large share of the 1.9 KB header budget the proxy must parse and copy.
const fakeSecurityToken = "IQoJb3JpZ2luX2VjEJr__________wEaCXVzLWVhc3QtMiJHMEUCIQD" +
	"0kXqNq1mD3nF2vY8pQ7lZ4cB6tR9wW1sJ5hG8dA3xLgIgSvM7nK2oP9rT4uE6yH1bC8fX3dN5" +
	"wQ0jL7mZ9pR2aKsq9gMI2BEAAaDDg3NjkwODAxMjUxMCIM0tVqXp7nR4mLb9cZKrcDQ1pYvH3" +
	"kT8uN6wE2sJ4dR7fG9bM1xA5oC0iL8hP3nK6tY9wZ2vB7qD4jS1mF8gX5rT0uN3cE6yH9bW1s" +
	"J7hG4dA8xL2gSvM5nK9oP3rT6uE1yH4bC2fX8dN0wQ5jL3mZ7pR9aKsq2gMI5BEAAaDDg3Njk" +
	"wODAxMjUxMCIMtVqXp0nR4mLb9cZKrcDQ1pYvH3kT8uN6wE2sJ4dR7fG9bM1xA5oC0iL8hP3n" +
	"K6tY9wZ2vB7qD4jS1mF8gX5rT0uN3cE6yH9bW1sJ7hG4dA8xL2gSvM5nK9oP3rT6uE1yH4bC2" +
	"fX8dN0wQ5jL3mZ7pR9aKsq2gMI5BEAAaDDg3NjkwODAxMjUxMCIMtVqXp0nR4mLb9cZKrcDQ1" +
	"pYvH3kT8uN6wE2sJ4dR7fG9bM1xA5oC0iL8hP3nK6tY9wZ2vB7qD4jS1mF8gX5rT0uN3cE6yH"

const fakeAuthorization = "AWS4-HMAC-SHA256 Credential=ASIAV3ZUEFP6EXAMPLE/20261013/" +
	"us-east-2/es/aws4_request, SignedHeaders=accept;content-length;content-type;host;" +
	"user-agent;x-amz-content-sha256;x-amz-date;x-amz-security-token, " +
	"Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"

// buildHeaders renders the common header block for one request.
func buildHeaders(sb *bytes.Buffer, host string, closeConn bool) {
	sb.WriteString("Host: ")
	sb.WriteString(host)
	sb.WriteString("\r\n")
	sb.WriteString("User-Agent: aws-sdk-java/2.25.11 Linux/6.1 OpenJDK_64-Bit_Server_VM/21.0.12 Java/21.0.12 vendor/Amazon.com_Inc. io/sync http/Apache cfg/retry-mode/legacy\r\n")
	sb.WriteString("Accept: application/json\r\n")
	sb.WriteString("x-amz-date: 20261013T112600Z\r\n")
	sb.WriteString("x-amz-content-sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\r\n")
	sb.WriteString("Authorization: ")
	sb.WriteString(fakeAuthorization)
	sb.WriteString("\r\n")
	sb.WriteString("x-amz-security-token: ")
	sb.WriteString(fakeSecurityToken)
	sb.WriteString("\r\n")
	if closeConn {
		sb.WriteString("Connection: close\r\n")
	}
}

// bulkBody builds a single-document _bulk payload of about 2 KiB: one
// NYC-taxi-shaped document plus padding, matching the production request shape.
func bulkBody(r *rand.Rand, id int64) []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, "{\"index\":{\"_index\":\"nyc_taxis\",\"_id\":\"%d\"}}\n", id)
	b.WriteString(`{"trip_type":"1","passenger_count":1,"tolls_amount":0.0,`)
	b.WriteString(`"tip_amount":1.76,"store_and_fwd_flag":"N","mta_tax":0.5,`)
	b.WriteString(`"ehail_fee":0.0,"total_amount":10.56,"rate_code_id":"1",`)
	fmt.Fprintf(&b, `"pickup_datetime":"2026-10-13 11:%02d:%02d",`, r.Intn(60), r.Intn(60))
	fmt.Fprintf(&b, `"dropoff_datetime":"2026-10-13 11:%02d:%02d",`, r.Intn(60), r.Intn(60))
	b.WriteString(`"payment_type":"1","fare_amount":8.0,"extra":0.5,`)
	b.WriteString(`"improvement_surcharge":0.3,"vendor_id":"2",`)
	fmt.Fprintf(&b, `"trip_distance":%.2f,`, r.Float64()*20)
	fmt.Fprintf(&b, `"pickup_location":[-73.%06d,40.%06d],`, r.Intn(1000000), r.Intn(1000000))
	fmt.Fprintf(&b, `"dropoff_location":[-73.%06d,40.%06d],`, r.Intn(1000000), r.Intn(1000000))
	// Padding to reach the ~2 KiB body the production run used (1,300 bytes of
	// padding on top of the document in that run).
	b.WriteString(`"pad":"`)
	pad := 1300 - 8
	for i := 0; i < pad; i++ {
		b.WriteByte(byte('a' + (i % 26)))
	}
	b.WriteString("\"}\n")
	return b.Bytes()
}

func smallDocBody(r *rand.Rand, id int64) []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, `{"doc_id":"%d","vendor_id":"2","passenger_count":%d,`, id, 1+r.Intn(4))
	fmt.Fprintf(&b, `"total_amount":%.2f,"pad":"`, r.Float64()*100)
	for i := 0; i < 900; i++ {
		b.WriteByte(byte('A' + (i % 26)))
	}
	b.WriteString("\"}")
	return b.Bytes()
}

// buildRequest serialises one request of the given kind into wire bytes.
func buildRequest(kind ReqKind, host string, r *rand.Rand, id int64, closeConn bool) []byte {
	var b bytes.Buffer
	switch kind {
	case KindGetKeepAlive:
		b.WriteString("GET /nyc_taxis/_search?size=1 HTTP/1.1\r\n")
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Length: 0\r\n\r\n")

	case KindBulkPost:
		body := bulkBody(r, id)
		b.WriteString("POST /nyc_taxis/_bulk HTTP/1.1\r\n")
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Type: application/x-ndjson\r\n")
		fmt.Fprintf(&b, "Content-Length: %d\r\n\r\n", len(body))
		b.Write(body)

	case KindPut:
		body := smallDocBody(r, id)
		fmt.Fprintf(&b, "PUT /nyc_taxis/_doc/%d HTTP/1.1\r\n", id)
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Type: application/json\r\n")
		fmt.Fprintf(&b, "Content-Length: %d\r\n\r\n", len(body))
		b.Write(body)

	case KindDelete:
		fmt.Fprintf(&b, "DELETE /nyc_taxis/_doc/%d HTTP/1.1\r\n", id)
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Length: 0\r\n\r\n")

	case KindPatch:
		body := smallDocBody(r, id)
		fmt.Fprintf(&b, "PATCH /nyc_taxis/_update/%d HTTP/1.1\r\n", id)
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Type: application/json\r\n")
		fmt.Fprintf(&b, "Content-Length: %d\r\n\r\n", len(body))
		b.Write(body)

	case KindChunkedTrailers:
		body := bulkBody(r, id)
		b.WriteString("POST /nyc_taxis/_bulk HTTP/1.1\r\n")
		buildHeaders(&b, host, closeConn)
		b.WriteString("Content-Type: application/x-ndjson\r\n")
		b.WriteString("Transfer-Encoding: chunked\r\n")
		b.WriteString("Trailer: x-checksum-crc32,x-record-count\r\n\r\n")
		// Three chunks of uneven size, then a trailer section. Uneven chunk
		// sizes are deliberate: a decoder that only ever sees one chunk per
		// body does not exercise the chunk-boundary path.
		cuts := []int{len(body) / 5, len(body) / 2, len(body)}
		prev := 0
		for _, c := range cuts {
			if c <= prev {
				continue
			}
			fmt.Fprintf(&b, "%x\r\n", c-prev)
			b.Write(body[prev:c])
			b.WriteString("\r\n")
			prev = c
		}
		b.WriteString("0\r\n")
		b.WriteString("x-checksum-crc32: 9f2b4c71\r\n")
		fmt.Fprintf(&b, "x-record-count: %d\r\n", 1)
		b.WriteString("\r\n")
	}
	return b.Bytes()
}
