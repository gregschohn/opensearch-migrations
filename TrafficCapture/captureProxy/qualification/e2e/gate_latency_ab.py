"""Indicative A/B: client-observed latency of sequential keep-alive PUTs (critical mutations) through
the proxy, poll-based gate vs notify-based gate. Same broker, same origin, alternating runs."""
import socket, subprocess, sys, time, signal, threading, http.server, socketserver, statistics, os
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_PUT(self):
        n = int(self.headers.get('Content-Length') or 0); self.rfile.read(n)
        self.send_response(200); self.send_header('Content-Length', '2'); self.end_headers(); self.wfile.write(b'ok')
    def log_message(self, *a): pass
class TS(socketserver.ThreadingMixIn, socketserver.TCPServer): daemon_threads = True; allow_reuse_address = True
origin = TS(('127.0.0.1', 18083), H); threading.Thread(target=origin.serve_forever, daemon=True).start()
def topic():
    t = f'gateab-{time.time_ns()}'
    subprocess.run(['docker','exec','capture-kafka-rust','/opt/kafka/bin/kafka-topics.sh','--bootstrap-server','localhost:9092',
                    '--create','--topic',t,'--partitions','1','--config','message.timestamp.type=LogAppendTime'],capture_output=True,check=True)
    return t
def run(binary, n=2000):
    p = subprocess.Popen([binary,'--bind-address','127.0.0.1','--listen-port','18093','--destination-uri','http://127.0.0.1:18083',
                          '--kafkaBrokers','localhost:19092','--kafkaTopic',topic()], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(300):
            try: s = socket.create_connection(('127.0.0.1',18093), timeout=2); break
            except OSError: time.sleep(0.1)
        req = b'PUT /i/_doc/1 HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}'
        lat = []
        for i in range(n + 200):
            t = time.perf_counter(); s.sendall(req)
            buf = b''
            while not buf.endswith(b'ok'): buf += s.recv(4096)
            if i >= 200: lat.append((time.perf_counter() - t) * 1e3)
        s.close(); lat.sort()
        return statistics.median(lat), lat[int(len(lat)*0.99)]
    finally:
        p.send_signal(signal.SIGTERM); p.wait(timeout=30)
res = {'poll200us': [], 'notify': []}
for rep in range(3):
    for name in (['poll200us','notify'] if rep % 2 == 0 else ['notify','poll200us']):
        res[name].append(run(f'/tmp/rust-proxy-qual/capture-proxy-{name}'))
for name, v in res.items():
    print(f"{name:10s} median ms per run: {[round(x[0],3) for x in v]}  p99 ms per run: {[round(x[1],3) for x in v]}")
origin.shutdown()
