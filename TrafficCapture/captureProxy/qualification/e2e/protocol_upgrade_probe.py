"""Does the proxy keep forwarding after a 101 Switching Protocols response, as Java does?"""
import socket, subprocess, threading, time, signal
BIN = '/Users/schohn/dev/rustifyProxy/TrafficCapture/captureProxy/target/release/capture-proxy'
REQ = b'GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n'
RESP = b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n'
FRAME = b'\x81\x85\x37\xfa\x21\x3d\x7f\x9f\x4d\x51\x58'  # masked text frame "Hello"
got = bytearray()
def origin():
    srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('127.0.0.1', 18087)); srv.listen(1); c, _ = srv.accept(); c.settimeout(3)
    buf = b''
    while b'\r\n\r\n' not in buf: buf += c.recv(4096)
    c.sendall(RESP)
    try:
        while True:
            b = c.recv(4096)
            if not b: break
            got.extend(b)
    except socket.timeout: pass
    c.close(); srv.close()
threading.Thread(target=origin, daemon=True).start(); time.sleep(0.3)
p = subprocess.Popen([BIN, '--bind-address', '127.0.0.1', '--listen-port', '18097', '--destination-uri', 'http://127.0.0.1:18087', '--no-capture'],
                     stdout=open('/tmp/rust-proxy-qual/upgrade_proxy.log', 'w'), stderr=subprocess.STDOUT)
for _ in range(100):
    try: s = socket.create_connection(('127.0.0.1', 18097), timeout=1); break
    except OSError: time.sleep(0.1)
s.sendall(REQ); s.settimeout(3)
resp = s.recv(4096)
s.sendall(FRAME); time.sleep(1); s.close(); p.send_signal(signal.SIGTERM); p.wait(timeout=20)
print('client got 101:', resp.startswith(b'HTTP/1.1 101'))
print('frame bytes after 101 reaching the origin:', len(got), 'of', len(FRAME), '; identical:', bytes(got) == FRAME)
