"""Gap A repro: a connection still open at SIGTERM is force-closed at the 60 s natural drain."""
import socket, subprocess, sys, time, signal, os, threading, http.server, socketserver
topic = open('/tmp/rust-proxy-qual/gapa/topic.txt').read().strip()
binp = __import__('os').environ.get('CAPTURE_PROXY_BIN', str(__import__('pathlib').Path(__file__).resolve().parents[2] / 'target/release/capture-proxy'))
mode = sys.argv[1] if len(sys.argv) > 1 else 'get'
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def _ok(self):
        n = int(self.headers.get('Content-Length') or 0)
        if n: self.rfile.read(n)
        body = b'z' * (3 * 1024 * 1024) if self.path == '/big' else b'ok'
        self.send_response(200); self.send_header('Content-Length', str(len(body)))
        if self.path == '/big': self.send_header('Connection', 'close')
        self.end_headers(); self.wfile.write(body)
        if self.path == '/big': self.close_connection = True
    do_GET = do_POST = do_PUT = _ok
    def log_message(self, *a): pass
class TS(socketserver.ThreadingMixIn, socketserver.TCPServer): daemon_threads = True; allow_reuse_address = True
origin = TS(('127.0.0.1', 18082), H); threading.Thread(target=origin.serve_forever, daemon=True).start()
env = dict(os.environ, MIGRATIONS_ROOT_LOG_LEVEL='debug')
log = open(f'/tmp/rust-proxy-qual/gapa/proxy-{mode}.log', 'w')
p = subprocess.Popen([binp, '--bind-address', '127.0.0.1', '--listen-port', '18092',
                      '--destination-uri', 'http://127.0.0.1:18082', '--kafkaBrokers', 'localhost:19092',
                      '--kafkaTopic', topic], stdout=log, stderr=subprocess.STDOUT, env=env)
t0 = time.time()
for _ in range(300):
    try: s = socket.create_connection(('127.0.0.1', 18092), timeout=2); break
    except OSError: time.sleep(0.1)
else: print('never ready'); sys.exit(1)
if mode == 'big':
    # >1 MiB response forces size-triggered detaches; the source then closes immediately.
    s.sendall(b'GET /big HTTP/1.1\r\nHost: x\r\n\r\n')
    got = 0
    while True:
        b = s.recv(1 << 20)
        if not b: break
        got += len(b)
    print('response bytes:', got)
    time.sleep(2)
    print(f'SIGTERM at +{time.time()-t0:.1f}s; the connection already closed')
    p.send_signal(signal.SIGTERM)
    try: rc = p.wait(timeout=150)
    except subprocess.TimeoutExpired: p.kill(); rc = 'killed'
    print(f'exit={rc} at +{time.time()-t0:.1f}s'); origin.shutdown(); sys.exit(0)
elif mode == 'get':
    s.sendall(b'GET /held HTTP/1.1\r\nHost: x\r\n\r\n')
else:  # a critical mutation, which publishes synchronously, then stays open
    s.sendall(b'PUT /held HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}')
print('response:', s.recv(4096)[:20])
time.sleep(1)
print(f'SIGTERM at +{time.time()-t0:.1f}s with the connection still open')
p.send_signal(signal.SIGTERM)
try: rc = p.wait(timeout=150)
except subprocess.TimeoutExpired: p.kill(); rc = 'killed'
print(f'exit={rc} at +{time.time()-t0:.1f}s'); s.close(); origin.shutdown()
