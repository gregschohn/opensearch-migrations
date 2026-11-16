package main

import (
	"bufio"
	"crypto/tls"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

// ---------------------------------------------------------------------------
// HTTP source.
//
// This stands in for the Elasticsearch source cluster. It is deliberately a
// hand-rolled HTTP/1.1 origin rather than nginx or a real search engine: the
// measurement target is the proxy, so the source must (a) never be the
// bottleneck, (b) cost a predictable and small number of cycles per request on
// its own pinned cores, and (c) correctly drain fixed-length AND chunked
// bodies with trailers, which is what the corpus sends. A real engine would add
// its own indexing variance to every latency sample.
//
// The response is a fixed ~0.5 KB JSON body, matching the production report's
// "responses about 0.5 KB".
// ---------------------------------------------------------------------------

var srcRequests atomic.Uint64
var srcErrors atomic.Uint64

func fixedResponseBody() []byte {
	b := []byte(`{"took":4,"errors":false,"items":[{"index":{"_index":"nyc_taxis","_id":"0","_version":1,"result":"created","_shards":{"total":2,"successful":2,"failed":0},"_seq_no":1,"_primary_term":1,"status":201}}],"_pad":"`)
	for len(b) < 500 {
		b = append(b, byte('x'))
	}
	b = append(b, '"', '}')
	return b
}

func runSource(addr string, useTLS bool, certFile, keyFile string) error {
	body := fixedResponseBody()
	hdr := fmt.Sprintf("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\nX-Source: qualgen\r\n\r\n", len(body))
	respKeep := append([]byte(hdr), body...)
	hdrClose := fmt.Sprintf("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\nX-Source: qualgen\r\nConnection: close\r\n\r\n", len(body))
	respClose := append([]byte(hdrClose), body...)

	var ln net.Listener
	var err error
	ln, err = net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	if useTLS {
		cert, err := tls.LoadX509KeyPair(certFile, keyFile)
		if err != nil {
			return fmt.Errorf("source tls keypair: %w", err)
		}
		ln = tls.NewListener(ln, &tls.Config{Certificates: []tls.Certificate{cert}, MinVersion: tls.VersionTLS12})
	}
	log.Printf("source listening on %s tls=%v", addr, useTLS)

	go func() {
		for {
			time.Sleep(10 * time.Second)
			log.Printf("source: requests=%d errors=%d", srcRequests.Load(), srcErrors.Load())
		}
	}()

	for {
		c, err := ln.Accept()
		if err != nil {
			srcErrors.Add(1)
			continue
		}
		go serveSourceConn(c, respKeep, respClose)
	}
}

func serveSourceConn(c net.Conn, respKeep, respClose []byte) {
	defer c.Close()
	if tc, ok := c.(*net.TCPConn); ok {
		_ = tc.SetNoDelay(true)
	}
	br := bufio.NewReaderSize(c, 16*1024)
	bw := bufio.NewWriterSize(c, 16*1024)
	for {
		_ = c.SetReadDeadline(time.Now().Add(120 * time.Second))
		closeAfter, err := readOneRequest(br)
		if err != nil {
			if err != io.EOF {
				srcErrors.Add(1)
			}
			_ = bw.Flush()
			return
		}
		srcRequests.Add(1)
		if closeAfter {
			_, _ = bw.Write(respClose)
			_ = bw.Flush()
			return
		}
		if _, err := bw.Write(respKeep); err != nil {
			srcErrors.Add(1)
			return
		}
		// Flush when no further pipelined request is already buffered, so a
		// burst of pipelined requests still gets one write syscall.
		if br.Buffered() == 0 {
			if err := bw.Flush(); err != nil {
				srcErrors.Add(1)
				return
			}
		}
	}
}

// readOneRequest consumes exactly one HTTP/1.1 request, including a
// fixed-length or chunked body with trailers. Returns whether the client asked
// to close.
func readOneRequest(br *bufio.Reader) (bool, error) {
	line, err := readLine(br)
	if err != nil {
		return false, err
	}
	if line == "" {
		return false, io.EOF
	}
	contentLen := -1
	chunked := false
	closeAfter := false
	for {
		h, err := readLine(br)
		if err != nil {
			return false, err
		}
		if h == "" {
			break
		}
		i := strings.IndexByte(h, ':')
		if i < 0 {
			continue
		}
		name := strings.ToLower(strings.TrimSpace(h[:i]))
		val := strings.TrimSpace(h[i+1:])
		switch name {
		case "content-length":
			n, err := strconv.Atoi(val)
			if err != nil {
				return false, fmt.Errorf("bad content-length %q", val)
			}
			contentLen = n
		case "transfer-encoding":
			if strings.Contains(strings.ToLower(val), "chunked") {
				chunked = true
			}
		case "connection":
			if strings.EqualFold(val, "close") {
				closeAfter = true
			}
		}
	}
	if chunked {
		if err := drainChunked(br); err != nil {
			return false, err
		}
	} else if contentLen > 0 {
		if _, err := io.CopyN(io.Discard, br, int64(contentLen)); err != nil {
			return false, err
		}
	}
	return closeAfter, nil
}

func drainChunked(br *bufio.Reader) error {
	for {
		l, err := readLine(br)
		if err != nil {
			return err
		}
		if semi := strings.IndexByte(l, ';'); semi >= 0 {
			l = l[:semi]
		}
		n, err := strconv.ParseInt(strings.TrimSpace(l), 16, 64)
		if err != nil {
			return fmt.Errorf("bad chunk size %q", l)
		}
		if n == 0 {
			// Trailer section: read until blank line.
			for {
				t, err := readLine(br)
				if err != nil {
					return err
				}
				if t == "" {
					return nil
				}
			}
		}
		if _, err := io.CopyN(io.Discard, br, n); err != nil {
			return err
		}
		// CRLF after chunk data
		if _, err := readLine(br); err != nil {
			return err
		}
	}
}

func readLine(br *bufio.Reader) (string, error) {
	s, err := br.ReadString('\n')
	if err != nil {
		if err == io.EOF && s == "" {
			return "", io.EOF
		}
		return "", err
	}
	return strings.TrimRight(s, "\r\n"), nil
}

func mustEnvInt(name string, def int) int {
	v := os.Getenv(name)
	if v == "" {
		return def
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		log.Fatalf("env %s: %v", name, err)
	}
	return n
}
