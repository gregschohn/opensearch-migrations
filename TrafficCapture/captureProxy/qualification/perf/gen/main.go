package main

import (
	"bufio"
	"crypto/tls"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// ---------------------------------------------------------------------------
// qualgen: the load generator and HTTP source for the Java-vs-Rust capture
// proxy comparison.
//
// WHY A PURPOSE-BUILT GENERATOR INSTEAD OF k6
//
// The repository's k6 workloads
// (TrafficCapture/trafficLoadTest/{scenarios,lib,k6-config}) are not runnable
// standalone: k6-run.sh submits an Argo Workflow against k6-operator
// WorkflowTemplates in a Kubernetes namespace, and lib/valkey.js + lib/sigv4.js
// bind the scenarios to an in-cluster Valkey id-registry and to SigV4 signing
// against a real AWS domain. There is no Kubernetes cluster in this
// measurement topology.
//
// More importantly, two of the required profile dimensions cannot be expressed
// in k6 at all:
//   - Randomized TCP fragmentation. k6's HTTP API hands a whole request to its
//     Go HTTP client; there is no hook to split one request across several TCP
//     segments. Fragmentation is exactly what stresses a scanning proxy's
//     incremental HTTP decoder, so a generator that cannot produce it cannot
//     qualify the decode path.
//   - Per-request new-connection churn. k6 controls connection reuse per
//     scenario (noConnectionReuse), which is why the production run had to
//     split its offered load across three runner groups to land at ~23% new
//     connections. A per-request Bernoulli choice gives the same aggregate at
//     one knob and keeps the keep-alive and churn traffic interleaved on the
//     same arrival process rather than segregated into different runner pods.
//
// Trailers on chunked requests are likewise outside k6's HTTP API.
//
// Error accounting: every request is counted in exactly one terminal bucket
// (ok / status_err / dial_err / tls_err / write_err / read_err / parse_err /
// timeout). The invariant is asserted on the LIFETIME counters:
//
//	0 <= attempted - (ok + errors) <= workers
//
// The lower bound catches double-counting and the upper bound allows exactly
// the requests that were still in flight when the stop flag was observed — at
// most one per worker, since a worker is serial. The invariant cannot be
// asserted on the warm-up-to-end delta, because requests that were started
// before the warm-up boundary and finished after it legitimately land in `ok`
// without having landed in the windowed `attempted`; that skew is reported as
// `window_inflight_skew` rather than treated as a dropped request. If the
// lifetime invariant fails the program exits non-zero, so a run can never
// silently drop requests.
//
// Latency samples are recorded exactly (not bucketed) up to a cap; if the cap
// is reached the overflow count is reported.
// ---------------------------------------------------------------------------

type errCounters struct {
	ok        atomic.Uint64
	statusErr atomic.Uint64
	dialErr   atomic.Uint64
	tlsErr    atomic.Uint64
	writeErr  atomic.Uint64
	readErr   atomic.Uint64
	parseErr  atomic.Uint64
	timeout   atomic.Uint64
	attempted atomic.Uint64
}

func (e *errCounters) totalErrors() uint64 {
	return e.statusErr.Load() + e.dialErr.Load() + e.tlsErr.Load() + e.writeErr.Load() +
		e.readErr.Load() + e.parseErr.Load() + e.timeout.Load()
}

type lane struct {
	name     string
	counters errCounters
	// Per-worker latency slices, merged at the end. Avoids a shared lock on
	// the hot path, which at tens of thousands of requests/s would itself
	// become the generator's bottleneck and distort the measurement.
	lat         [][]int64
	latOverflow atomic.Uint64

	bytesOut atomic.Uint64
	bytesIn  atomic.Uint64

	newConns   atomic.Uint64 // fresh TCP connections opened
	handshakes atomic.Uint64 // TLS handshakes completed
	fragged    atomic.Uint64 // requests written in >1 TCP segment
	byKind     [6]atomic.Uint64
}

type config struct {
	mode string

	target   string
	host     string
	frontTLS bool
	tlsSNI   string

	// Direct probe lane: a low fixed-rate lane straight to the source,
	// running concurrently with the main proxy load, used as the latency
	// baseline to subtract.
	probeTarget string
	probeTLS    bool
	probeRate   int

	conns    int
	rate     int // 0 = unthrottled (saturating)
	duration time.Duration
	warmup   time.Duration

	churn    float64
	fragProb float64
	fragMax  int

	mix     string
	timeout time.Duration
	out     string
	runID   string
	profile string
	proxy   string

	latCap int
}

func main() {
	var c config
	flag.StringVar(&c.mode, "mode", "load", "load | source")

	// source mode
	srcAddr := flag.String("source-addr", ":9200", "source listen address")
	srcTLS := flag.Bool("source-tls", false, "serve TLS from the source")
	srcCert := flag.String("source-cert", "", "source TLS cert PEM")
	srcKey := flag.String("source-key", "", "source TLS key PEM")

	// load mode
	flag.StringVar(&c.target, "target", "", "host:port under test")
	flag.StringVar(&c.host, "host", "", "Host header value (defaults to -target)")
	flag.BoolVar(&c.frontTLS, "tls", false, "speak TLS to -target")
	flag.StringVar(&c.tlsSNI, "sni", "localhost", "TLS SNI/servername")
	flag.StringVar(&c.probeTarget, "probe-target", "", "host:port for the concurrent direct-to-source latency probe")
	flag.BoolVar(&c.probeTLS, "probe-tls", false, "speak TLS on the probe lane")
	flag.IntVar(&c.probeRate, "probe-rate", 50, "probe lane requests/s")
	flag.IntVar(&c.conns, "conns", 64, "keep-alive worker count (concurrency)")
	flag.IntVar(&c.rate, "rate", 0, "target requests/s; 0 = unthrottled/saturating")
	flag.DurationVar(&c.duration, "duration", 60*time.Second, "measured duration")
	flag.DurationVar(&c.warmup, "warmup", 20*time.Second, "warm-up duration, excluded from all stats")
	flag.Float64Var(&c.churn, "churn", 0.0, "fraction of requests on a brand-new connection (0.23 matches production)")
	flag.Float64Var(&c.fragProb, "frag-prob", 0.0, "fraction of requests written in multiple TCP segments")
	flag.IntVar(&c.fragMax, "frag-max", 6, "max TCP segments per fragmented request")
	flag.StringVar(&c.mix, "mix", "bulk_post=70,get=18,put=4,delete=2,patch=2,chunked_trailers=4", "request mix")
	flag.DurationVar(&c.timeout, "timeout", 30*time.Second, "per-request timeout")
	flag.StringVar(&c.out, "out", "", "write JSON summary here")
	flag.StringVar(&c.runID, "run-id", "", "RUN_ID")
	flag.StringVar(&c.profile, "profile", "", "profile name")
	flag.StringVar(&c.proxy, "proxy", "", "proxy target label: java|rust|none-direct")
	flag.IntVar(&c.latCap, "lat-cap", 6_000_000, "max latency samples per lane")
	flag.Parse()

	if c.mode == "source" {
		if err := runSource(*srcAddr, *srcTLS, *srcCert, *srcKey); err != nil {
			log.Fatalf("source: %v", err)
		}
		return
	}
	if c.target == "" {
		log.Fatal("-target is required in load mode")
	}
	if c.host == "" {
		c.host = c.target
	}
	if err := runLoad(&c); err != nil {
		log.Fatalf("load: %v", err)
	}
}

// ---------------------------------------------------------------------------

type conn struct {
	nc net.Conn
	br *bufio.Reader
}

func dial(target string, useTLS bool, sni string, l *lane, timeout time.Duration) (*conn, error) {
	nc, err := net.DialTimeout("tcp", target, timeout)
	if err != nil {
		l.counters.dialErr.Add(1)
		return nil, err
	}
	l.newConns.Add(1)
	if tc, ok := nc.(*net.TCPConn); ok {
		_ = tc.SetNoDelay(true)
	}
	if useTLS {
		tc := tls.Client(nc, &tls.Config{
			ServerName: sni,
			// The proxy and source use self-signed certs generated by the
			// harness. Certificate chain validation is not under measurement;
			// the handshake cost is, and that is unaffected.
			InsecureSkipVerify: true,
			MinVersion:         tls.VersionTLS12,
		})
		_ = tc.SetDeadline(time.Now().Add(timeout))
		if err := tc.Handshake(); err != nil {
			l.counters.tlsErr.Add(1)
			_ = nc.Close()
			return nil, err
		}
		l.handshakes.Add(1)
		nc = tc
	}
	return &conn{nc: nc, br: bufio.NewReaderSize(nc, 32*1024)}, nil
}

// writeFragmented writes buf either in one call or split into n random pieces.
func writeFragmented(w net.Conn, buf []byte, r *rand.Rand, frag bool, fragMax int) error {
	if !frag || len(buf) < 8 {
		_, err := w.Write(buf)
		return err
	}
	n := 2 + r.Intn(fragMax-1)
	if n > len(buf) {
		n = len(buf)
	}
	cuts := make([]int, 0, n)
	for i := 0; i < n-1; i++ {
		cuts = append(cuts, 1+r.Intn(len(buf)-1))
	}
	sort.Ints(cuts)
	cuts = append(cuts, len(buf))
	prev := 0
	for _, c := range cuts {
		if c <= prev {
			continue
		}
		if _, err := w.Write(buf[prev:c]); err != nil {
			return err
		}
		prev = c
	}
	return nil
}

// readResponse consumes one HTTP response and returns its status code, total
// bytes consumed, and whether the server signalled close.
func readResponse(br *bufio.Reader) (int, int, bool, error) {
	total := 0
	line, n, err := readLineCounted(br)
	total += n
	if err != nil {
		return 0, total, false, err
	}
	parts := strings.SplitN(line, " ", 3)
	if len(parts) < 2 || !strings.HasPrefix(parts[0], "HTTP/1.") {
		return 0, total, false, fmt.Errorf("bad status line %q", line)
	}
	code, err := strconv.Atoi(parts[1])
	if err != nil {
		return 0, total, false, fmt.Errorf("bad status code %q", parts[1])
	}
	contentLen := -1
	chunked := false
	closeAfter := false
	for {
		h, n, err := readLineCounted(br)
		total += n
		if err != nil {
			return code, total, false, err
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
			v, err := strconv.Atoi(val)
			if err != nil {
				return code, total, false, fmt.Errorf("bad response content-length %q", val)
			}
			contentLen = v
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
	switch {
	case chunked:
		for {
			l, n, err := readLineCounted(br)
			total += n
			if err != nil {
				return code, total, false, err
			}
			if semi := strings.IndexByte(l, ';'); semi >= 0 {
				l = l[:semi]
			}
			sz, err := strconv.ParseInt(strings.TrimSpace(l), 16, 64)
			if err != nil {
				return code, total, false, fmt.Errorf("bad response chunk size %q", l)
			}
			if sz == 0 {
				for {
					t, n, err := readLineCounted(br)
					total += n
					if err != nil {
						return code, total, false, err
					}
					if t == "" {
						break
					}
				}
				break
			}
			m, err := io.CopyN(io.Discard, br, sz)
			total += int(m)
			if err != nil {
				return code, total, false, err
			}
			_, n, err = readLineCounted(br)
			total += n
			if err != nil {
				return code, total, false, err
			}
		}
	case contentLen > 0:
		m, err := io.CopyN(io.Discard, br, int64(contentLen))
		total += int(m)
		if err != nil {
			return code, total, false, err
		}
	case contentLen < 0 && !chunked:
		// No framing: the body runs to EOF. Must close after.
		m, _ := io.Copy(io.Discard, br)
		total += int(m)
		closeAfter = true
	}
	return code, total, closeAfter, nil
}

func readLineCounted(br *bufio.Reader) (string, int, error) {
	s, err := br.ReadString('\n')
	if err != nil {
		return "", len(s), err
	}
	return strings.TrimRight(s, "\r\n"), len(s), nil
}

// ---------------------------------------------------------------------------

type laneSpec struct {
	l        *lane
	target   string
	useTLS   bool
	workers  int
	rate     int
	churn    float64
	fragProb float64
}

func runLoad(c *config) error {
	mix, err := parseMix(c.mix)
	if err != nil {
		return err
	}
	// Expand the mix into a 1000-slot lookup table so kind selection is one
	// RNG call and one index, not a loop over weights.
	totalW := 0
	for _, m := range mix {
		totalW += m.Weight
	}
	table := make([]ReqKind, 0, 1000)
	for _, m := range mix {
		n := m.Weight * 1000 / totalW
		for i := 0; i < n; i++ {
			table = append(table, m.Kind)
		}
	}
	for len(table) < 1000 {
		table = append(table, mix[0].Kind)
	}

	main := &lane{name: "proxy"}
	specs := []laneSpec{{l: main, target: c.target, useTLS: c.frontTLS, workers: c.conns,
		rate: c.rate, churn: c.churn, fragProb: c.fragProb}}

	var probe *lane
	if c.probeTarget != "" && c.probeRate > 0 {
		probe = &lane{name: "direct_probe"}
		// The probe lane is deliberately keep-alive only, unfragmented and
		// rate-limited: its job is to report what the source costs under this
		// run's source load, not to add load of its own.
		pw := 8
		specs = append(specs, laneSpec{l: probe, target: c.probeTarget, useTLS: c.probeTLS,
			workers: pw, rate: c.probeRate, churn: 0, fragProb: 0})
	}

	for _, s := range specs {
		s.l.lat = make([][]int64, s.workers)
	}

	// measuring flips from false to true when warm-up ends. Workers check it
	// per request; samples taken while false are thrown away.
	var measuring atomic.Bool
	var stop atomic.Bool
	var wg sync.WaitGroup

	loadAvgBefore := readLoadAvg()

	for _, s := range specs {
		for w := 0; w < s.workers; w++ {
			wg.Add(1)
			cap := c.latCap / s.workers
			if cap < 1024 {
				cap = 1024
			}
			s.l.lat[w] = make([]int64, 0, cap)
			go func(s laneSpec, w int, capN int) {
				defer wg.Done()
				worker(c, s, w, capN, table, &measuring, &stop)
			}(s, w, cap)
		}
	}

	log.Printf("warm-up %s ...", c.warmup)
	time.Sleep(c.warmup)
	// Snapshot the counters at the warm-up boundary so the measured interval's
	// byte and connection totals exclude warm-up.
	snap := map[string]map[string]uint64{}
	for _, s := range specs {
		snap[s.l.name] = laneSnapshot(s.l)
	}
	measuring.Store(true)
	t0 := time.Now()
	log.Printf("measuring %s ...", c.duration)
	time.Sleep(c.duration)
	measuring.Store(false)
	elapsed := time.Since(t0).Seconds()
	stop.Store(true)
	wg.Wait()
	loadAvgAfter := readLoadAvg()

	out := map[string]interface{}{
		"run_id":            c.runID,
		"profile":           c.profile,
		"proxy":             c.proxy,
		"target":            c.target,
		"front_tls":         c.frontTLS,
		"conns":             c.conns,
		"rate_requested":    c.rate,
		"churn_requested":   c.churn,
		"frag_prob":         c.fragProb,
		"mix":               c.mix,
		"warmup_s":          c.warmup.Seconds(),
		"measured_s":        elapsed,
		"load_avg_before":   loadAvgBefore,
		"load_avg_after":    loadAvgAfter,
		"generator_version": "qualgen/1",
	}
	consistent := true
	for _, s := range specs {
		m, ok := laneReport(s.l, snap[s.l.name], elapsed, s.workers)
		if !ok {
			consistent = false
		}
		out[s.l.name] = m
	}
	out["accounting_consistent"] = consistent

	b, _ := json.MarshalIndent(out, "", "  ")
	if c.out != "" {
		if err := os.WriteFile(c.out, b, 0o644); err != nil {
			return err
		}
	}
	fmt.Println(string(b))
	if !consistent {
		return fmt.Errorf("request accounting inconsistent: attempted != ok+errors; refusing to report this run")
	}
	return nil
}

func laneSnapshot(l *lane) map[string]uint64 {
	m := map[string]uint64{
		"attempted": l.counters.attempted.Load(), "ok": l.counters.ok.Load(),
		"status_err": l.counters.statusErr.Load(), "dial_err": l.counters.dialErr.Load(),
		"tls_err": l.counters.tlsErr.Load(), "write_err": l.counters.writeErr.Load(),
		"read_err": l.counters.readErr.Load(), "parse_err": l.counters.parseErr.Load(),
		"timeout": l.counters.timeout.Load(),
		"bytes_out": l.bytesOut.Load(), "bytes_in": l.bytesIn.Load(),
		"new_conns": l.newConns.Load(), "handshakes": l.handshakes.Load(),
		"fragged": l.fragged.Load(),
	}
	for i := range l.byKind {
		m["kind_"+ReqKind(i).String()] = l.byKind[i].Load()
	}
	return m
}

func laneReport(l *lane, before map[string]uint64, elapsed float64, workers int) (map[string]interface{}, bool) {
	now := laneSnapshot(l)
	d := func(k string) uint64 {
		if now[k] < before[k] {
			return 0
		}
		return now[k] - before[k]
	}
	var all []int64
	for _, s := range l.lat {
		all = append(all, s...)
	}
	sort.Slice(all, func(i, j int) bool { return all[i] < all[j] })
	q := func(p float64) float64 {
		if len(all) == 0 {
			return -1
		}
		i := int(p * float64(len(all)-1))
		return float64(all[i]) / 1000.0 // us -> ms
	}
	attempted := d("attempted")
	ok := d("ok")
	errs := d("status_err") + d("dial_err") + d("tls_err") + d("write_err") +
		d("read_err") + d("parse_err") + d("timeout")

	// Lifetime invariant: 0 <= attempted - (ok+errors) <= workers.
	lifeAtt := now["attempted"]
	lifeTerm := now["ok"] + now["status_err"] + now["dial_err"] + now["tls_err"] +
		now["write_err"] + now["read_err"] + now["parse_err"] + now["timeout"]
	consistent := lifeAtt >= lifeTerm && lifeAtt-lifeTerm <= uint64(workers)
	// Windowed skew: requests straddling the warm-up boundary. Signed.
	skew := int64(attempted) - int64(ok+errs)

	kinds := map[string]uint64{}
	for i := range l.byKind {
		kinds[ReqKind(i).String()] = d("kind_" + ReqKind(i).String())
	}

	m := map[string]interface{}{
		"attempted":            attempted,
		"ok":                   ok,
		"errors_total":         errs,
		"errors": map[string]uint64{
			"status": d("status_err"), "dial": d("dial_err"), "tls": d("tls_err"),
			"write": d("write_err"), "read": d("read_err"), "parse": d("parse_err"),
			"timeout": d("timeout"),
		},
		"requests_per_s":       float64(ok) / elapsed,
		"attempted_per_s":      float64(attempted) / elapsed,
		"bytes_out":            d("bytes_out"),
		"bytes_in":             d("bytes_in"),
		"bytes_out_per_s":      float64(d("bytes_out")) / elapsed,
		"bytes_in_per_s":       float64(d("bytes_in")) / elapsed,
		"new_conns":            d("new_conns"),
		"new_conns_per_s":      float64(d("new_conns")) / elapsed,
		"tls_handshakes":       d("handshakes"),
		"tls_handshakes_per_s": float64(d("handshakes")) / elapsed,
		"new_conn_fraction":    frac(d("new_conns"), attempted),
		"fragmented_requests":  d("fragged"),
		"fragmented_fraction":  frac(d("fragged"), attempted),
		"by_kind":              kinds,
		"latency_samples":      len(all),
		"latency_overflow":     l.latOverflow.Load(),
		"lifetime_attempted":   lifeAtt,
		"lifetime_terminal":    lifeTerm,
		"lifetime_inflight":    lifeAtt - lifeTerm,
		"window_inflight_skew": skew,
		"workers":              workers,
		"p50_ms":               q(0.50),
		"p90_ms":               q(0.90),
		"p99_ms":               q(0.99),
		"p999_ms":              q(0.999),
		"max_ms":               q(1.0),
		"accounting_consistent": consistent,
	}
	return m, consistent
}

func frac(a, b uint64) float64 {
	if b == 0 {
		return 0
	}
	return float64(a) / float64(b)
}

func worker(c *config, s laneSpec, w int, capN int, table []ReqKind,
	measuring *atomic.Bool, stop *atomic.Bool) {

	r := rand.New(rand.NewSource(time.Now().UnixNano() ^ int64(w)*0x9E3779B9))
	l := s.l
	var kc *conn // persistent keep-alive connection
	var id int64 = int64(w) * 1_000_000_000

	// Per-worker pacing when a rate is requested. Each worker gets rate/workers
	// and keeps an absolute schedule, so a slow period is visible as lateness
	// rather than silently lowering the offered rate.
	var interval time.Duration
	if s.rate > 0 {
		per := float64(s.rate) / float64(s.workers)
		if per <= 0 {
			per = 1
		}
		interval = time.Duration(float64(time.Second) / per)
	}
	next := time.Now()

	for !stop.Load() {
		if interval > 0 {
			now := time.Now()
			if next.After(now) {
				time.Sleep(next.Sub(now))
			}
			next = next.Add(interval)
			// If we have fallen more than one second behind, resynchronise and
			// report it rather than trying to make up an unbounded burst.
			if time.Since(next) > time.Second {
				next = time.Now()
			}
		}

		id++
		kind := table[r.Intn(len(table))]
		fresh := s.churn > 0 && r.Float64() < s.churn
		frag := s.fragProb > 0 && r.Float64() < s.fragProb

		var cn *conn
		var closeAfter bool
		if fresh {
			closeAfter = true
		}
		buf := buildRequest(kind, c.host, r, id, closeAfter)

		meas := measuring.Load()
		l.counters.attempted.Add(1)
		l.byKind[kind].Add(1)

		if fresh {
			var err error
			cn, err = dial(s.target, s.useTLS, c.tlsSNI, l, c.timeout)
			if err != nil {
				continue // dial/tls counters already incremented
			}
		} else {
			if kc == nil {
				var err error
				kc, err = dial(s.target, s.useTLS, c.tlsSNI, l, c.timeout)
				if err != nil {
					continue
				}
			}
			cn = kc
		}

		_ = cn.nc.SetDeadline(time.Now().Add(c.timeout))
		start := time.Now()
		if err := writeFragmented(cn.nc, buf, r, frag, c.fragMax); err != nil {
			if isTimeout(err) {
				l.counters.timeout.Add(1)
			} else {
				l.counters.writeErr.Add(1)
			}
			closeConn(cn, &kc)
			continue
		}
		if frag {
			l.fragged.Add(1)
		}
		l.bytesOut.Add(uint64(len(buf)))

		code, n, srvClose, err := readResponse(cn.br)
		lat := time.Since(start)
		l.bytesIn.Add(uint64(n))
		if err != nil {
			if isTimeout(err) {
				l.counters.timeout.Add(1)
			} else if code == 0 {
				l.counters.readErr.Add(1)
			} else {
				l.counters.parseErr.Add(1)
			}
			closeConn(cn, &kc)
			continue
		}
		if code < 200 || code >= 400 {
			l.counters.statusErr.Add(1)
		} else {
			l.counters.ok.Add(1)
			if meas {
				if len(l.lat[w]) < capN {
					l.lat[w] = append(l.lat[w], lat.Microseconds())
				} else {
					l.latOverflow.Add(1)
				}
			}
		}
		if fresh || closeAfter || srvClose {
			_ = cn.nc.Close()
			if cn == kc {
				kc = nil
			}
		}
	}
	if kc != nil {
		_ = kc.nc.Close()
	}
}

func closeConn(cn *conn, kc **conn) {
	_ = cn.nc.Close()
	if *kc == cn {
		*kc = nil
	}
}

func isTimeout(err error) bool {
	ne, ok := err.(net.Error)
	return ok && ne.Timeout()
}

func readLoadAvg() []float64 {
	b, err := os.ReadFile("/proc/loadavg")
	if err != nil {
		return nil
	}
	f := strings.Fields(string(b))
	out := []float64{}
	for i := 0; i < 3 && i < len(f); i++ {
		v, err := strconv.ParseFloat(f[i], 64)
		if err != nil {
			return out
		}
		out = append(out, v)
	}
	return out
}
