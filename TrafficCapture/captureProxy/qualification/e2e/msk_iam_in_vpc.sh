set -uo pipefail
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_PROFILE
export AWS_REGION=us-east-2
cd /opt/rp
cat > /opt/rp/msk_proof.py <<'PY'
import os, socket, subprocess, sys, time, signal, threading, http.server, socketserver
BIN = '/opt/rp/src/TrafficCapture/captureProxy/target/debug/capture-proxy'
BOOT = '<msk-iam-bootstrap-host>:9098'
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def _ok(self):
        n = int(self.headers.get('Content-Length') or 0)
        if n: self.rfile.read(n)
        self.send_response(200); self.send_header('Content-Length', '2'); self.end_headers(); self.wfile.write(b'ok')
    do_GET = do_PUT = _ok
    def log_message(self, *a): pass
class TS(socketserver.ThreadingMixIn, socketserver.TCPServer): daemon_threads = True; allow_reuse_address = True
origin = TS(('127.0.0.1', 18085), H); threading.Thread(target=origin.serve_forever, daemon=True).start()
def read_response(s):
    buf = b''
    while b'\r\n\r\n' not in buf: buf += s.recv(4096)
    head, _, body = buf.partition(b'\r\n\r\n')
    n = next(int(l.split(b':')[1]) for l in head.split(b'\r\n') if l.lower().startswith(b'content-length'))
    while len(body) < n: body += s.recv(4096)
    return head
def case(name, topic, expect_ready):
    log = open(f'/opt/rp/proxy-{name}.log', 'w')
    t0 = time.time()
    p = subprocess.Popen([BIN, '--bind-address', '127.0.0.1', '--listen-port', '18095', '--destination-uri', 'http://127.0.0.1:18085',
                          '--kafkaBrokers', BOOT, '--kafkaTopic', topic, '--kafkaAuthType', 'msk-iam'], stdout=log, stderr=subprocess.STDOUT)
    ready = False
    while time.time() - t0 < 180 and p.poll() is None:
        try: s = socket.create_connection(('127.0.0.1', 18095), timeout=1); ready = True; break
        except OSError: time.sleep(0.2)
    t_ready = time.time() - t0
    r1 = r2 = b''
    if ready:
        s.sendall(b'PUT /i/_doc/1 HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}'); r1 = read_response(s)
        s.sendall(b'GET /i/_doc/1 HTTP/1.1\r\nHost: x\r\n\r\n'); r2 = read_response(s); s.close()
        time.sleep(2); p.send_signal(signal.SIGTERM)
    try: rc = p.wait(timeout=120)
    except subprocess.TimeoutExpired: p.kill(); rc = 'killed'
    ok200 = r1.startswith(b'HTTP/1.1 200') and r2.startswith(b'HTTP/1.1 200')
    verdict = (ready and ok200 and rc == 0) if expect_ready else (not ready and rc not in (0, 'killed'))
    print(f'{name}: ready={ready} after {t_ready:.1f}s PUT={r1[:15]} GET={r2[:15]} exit={rc} -> {"PASS" if verdict else "FAIL"}')
case('logappendtime', 'rustproxy-probe-topic-logappendtime', True)
case('createtime', 'rustproxy-probe-topic', False)
origin.shutdown()
PY
python3 -I /opt/rp/msk_proof.py
for f in /opt/rp/proxy-logappendtime.log /opt/rp/proxy-createtime.log; do
  echo "=== $f events ==="
  grep -oE 'event[="]+[a-z_]+|outcome=[a-z]+|timestamp=[A-Za-z]+\([0-9]+\)|refus[a-z]*[^"]{0,140}' "$f" | sort | uniq -c | sort -rn | head -14
done
grep -iE "token|oauth" /opt/rp/proxy-logappendtime.log | grep -viE "secret|signature" | head -3 | cut -c1-200
