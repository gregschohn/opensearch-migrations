"""Gap B end-to-end: an idle connection's partial request must reach Kafka after F, while open."""
import socket, subprocess, sys, time, signal, os, threading, http.server, socketserver

topic = open('/tmp/rust-proxy-qual/gapb-topic.txt').read().strip()
proxy_bin = __import__('os').environ.get('CAPTURE_PROXY_BIN', str(__import__('pathlib').Path(__file__).resolve().parents[2] / 'target/release/capture-proxy'))
marker = b'X-GapB-Marker: idle-flush-%d' % int(time.time())

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Length', '0'); self.end_headers()
    def log_message(self, *a): pass
origin = socketserver.TCPServer(('127.0.0.1', 18081), H)
threading.Thread(target=origin.serve_forever, daemon=True).start()

def topic_values():
    out = subprocess.run(['docker', 'exec', 'capture-kafka-rust', '/opt/kafka/bin/kafka-console-consumer.sh',
                          '--bootstrap-server', 'localhost:9092', '--topic', topic, '--from-beginning',
                          '--timeout-ms', '4000', '--property', 'print.timestamp=true'],
                         capture_output=True)
    return out.stdout

log = open('/tmp/rust-proxy-qual/gapb/proxy.log', 'w')
p = subprocess.Popen([proxy_bin, '--bind-address', '127.0.0.1', '--listen-port', '18091',
                      '--destination-uri', 'http://127.0.0.1:18081', '--kafkaBrokers', 'localhost:19092',
                      '--kafkaTopic', topic, '--traffic-stream-flush-interval-seconds', '2',
                      '--heartbeat-interval-seconds', '30', '--heartbeat-expiration-interval-seconds', '60'],
                     stdout=log, stderr=subprocess.STDOUT)
try:
    for _ in range(300):
        try:
            s = socket.create_connection(('127.0.0.1', 18091), timeout=1); break
        except OSError:
            time.sleep(0.1)
    else:
        print('FAIL: proxy never became ready'); sys.exit(1)
    # Partial head: no terminating blank line, so no EOM and no size/mutation/close trigger.
    t_sent_ms = time.time() * 1000
    s.sendall(b'GET /gapb HTTP/1.1\r\nHost: x\r\n' + marker + b'\r\n')
    time.sleep(4.0)   # > F = 2 s; the connection stays open throughout
    try:
        s.setblocking(False); s.recv(1); still_open = False
    except BlockingIOError:
        still_open = True
    except OSError:
        still_open = False
    values = topic_values()
    s.close()   # close only after Kafka has been read, so the record cannot be a close-triggered one
    import re
    headers = list(re.finditer(rb'(?:^|\n)(LogAppendTime|CreateTime):(\d+)\t', values))
    found = None
    for i, h in enumerate(headers):
        end = headers[i + 1].start() if i + 1 < len(headers) else len(values)
        if marker in values[h.end():end]:
            found = (h.group(1).decode(), int(h.group(2))); break
    if found is None:
        print('FAIL: the idle record never reached Kafka while the connection was open'); verdict = False
    else:
        kind, ts = found
        delay_ms = ts - t_sent_ms
        print(f'records on topic: {len(headers)}; marker record timestamp type: {kind}')
        print(f'broker append time - send time: {delay_ms:.0f} ms (F = 2000 ms)')
        print(f'connection still open after the record was appended: {still_open}')
        verdict = kind == 'LogAppendTime' and 1800 <= delay_ms <= 3500 and still_open
    print('VERDICT:', 'PASS' if verdict else 'FAIL')
finally:
    p.send_signal(signal.SIGTERM)
    try:
        print('proxy exit:', p.wait(timeout=20))
    except subprocess.TimeoutExpired:
        p.kill(); print('proxy exit: killed after timeout')
    origin.shutdown()
