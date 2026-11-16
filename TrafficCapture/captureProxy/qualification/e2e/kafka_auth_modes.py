"""Auth-mode proof: TLS and SCRAM-SHA-512 against a real broker, using the orchestration's exact
client.properties shape (ssl.truststore.type=PEM / ssl.truststore.location)."""
import os, socket, subprocess, sys, time, signal, threading, http.server, socketserver
D = '/tmp/rust-proxy-qual/auth'
BIN = __import__('os').environ.get('CAPTURE_PROXY_BIN', str(__import__('pathlib').Path(__file__).resolve().parents[2] / 'target/release/capture-proxy'))
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def _ok(self):
        n = int(self.headers.get('Content-Length') or 0)
        if n: self.rfile.read(n)
        self.send_response(200); self.send_header('Content-Length', '2'); self.end_headers(); self.wfile.write(b'ok')
    do_GET = do_PUT = _ok
    def log_message(self, *a): pass
class TS(socketserver.ThreadingMixIn, socketserver.TCPServer): daemon_threads = True; allow_reuse_address = True
origin = TS(('127.0.0.1', 18084), H); threading.Thread(target=origin.serve_forever, daemon=True).start()

def read_response(sock):
    buf = b''
    while b'\r\n\r\n' not in buf:
        buf += sock.recv(4096)
    head, _, body = buf.partition(b'\r\n\r\n')
    length = next(int(l.split(b':')[1]) for l in head.split(b'\r\n') if l.lower().startswith(b'content-length'))
    while len(body) < length:
        body += sock.recv(4096)
    return head + b'\r\n\r\n' + body

def offsets(topic):
    out = subprocess.run(['docker','exec','auth-kafka','/opt/kafka/bin/kafka-get-offsets.sh','--bootstrap-server','localhost:9092','--topic',topic], capture_output=True, text=True).stdout
    return sum(int(l.rsplit(':',1)[1]) for l in out.split() if l.count(':') == 2)

def case(name, args, env_extra, expect_ready, topic):
    env = dict(os.environ, **env_extra)
    log = open(f'{D}/proxy-{name}.log', 'w')
    before = offsets(topic)
    p = subprocess.Popen([BIN, '--bind-address','127.0.0.1','--listen-port','18094','--destination-uri','http://127.0.0.1:18084','--kafkaTopic',topic] + args,
                         stdout=log, stderr=subprocess.STDOUT, env=env)
    ready = False
    for _ in range(200):
        if p.poll() is not None: break
        try:
            s = socket.create_connection(('127.0.0.1', 18094), timeout=1); ready = True; break
        except OSError: time.sleep(0.1)
    if ready:
        s.sendall(b'PUT /i/_doc/1 HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}')
        r1 = read_response(s)
        s.sendall(b'GET /i/_doc/1 HTTP/1.1\r\nHost: x\r\n\r\n')
        r2 = read_response(s); s.close()
        time.sleep(1)
        p.send_signal(signal.SIGTERM)
    try: rc = p.wait(timeout=60)
    except subprocess.TimeoutExpired: p.kill(); rc = 'killed'
    grew = offsets(topic) - before
    ok = (ready == expect_ready) and ((rc == 0 and grew > 0 and r1.startswith(b'HTTP/1.1 200') and r2.startswith(b'HTTP/1.1 200')) if expect_ready else rc != 0)
    if ready and not (r1.startswith(b'HTTP/1.1 200') and r2.startswith(b'HTTP/1.1 200')):
        print('   r1=', r1[:60], ' r2=', r2[:60])
    print(f"{name:28s} ready={ready!s:5s} exit={rc!s:4s} topic_records_added={grew:3d}  {'PASS' if ok else 'FAIL'}")
    return ok

props = ['--kafkaPropertyFile', f'{D}/client.properties']
results = [
    case('tls (ssl)',            ['--kafkaBrokers','localhost:29801','--kafkaAuthType','ssl'] + props, {}, True, 'auth-tls'),
    case('scram-sha-512',        ['--kafkaBrokers','localhost:29802','--kafkaAuthType','scram-sha-512','--kafkaUserName','capture'] + props,
                                 {'CAPTURE_PROXY_KAFKA_PASSWORD':'capture-secret'}, True, 'auth-scram'),
    case('scram wrong password', ['--kafkaBrokers','localhost:29802','--kafkaAuthType','scram-sha-512','--kafkaUserName','capture'] + props,
                                 {'CAPTURE_PROXY_KAFKA_PASSWORD':'not-the-password'}, False, 'auth-scram'),
    case('tls without CA trust', ['--kafkaBrokers','localhost:29801','--kafkaAuthType','ssl'], {}, False, 'auth-tls'),
]
print('ALL PASS' if all(results) else 'SOME FAILED')
origin.shutdown()
