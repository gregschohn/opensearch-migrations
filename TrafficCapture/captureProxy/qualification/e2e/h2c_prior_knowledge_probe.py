"""Does the proxy forward an HTTP/2 prior-knowledge (h2c) connection, as Java does?"""
import socket, subprocess, sys, threading, time, signal
BIN = __import__('os').environ.get('CAPTURE_PROXY_BIN', str(__import__('pathlib').Path(__file__).resolve().parents[2] / 'target/release/capture-proxy'))
PREFACE = b'PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n'
SETTINGS = b'\x00\x00\x00\x04\x00\x00\x00\x00\x00'
PING = b'\x00\x00\x08\x06\x00\x00\x00\x00\x00' + b'12345678'
received = bytearray()
def origin():
    srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('127.0.0.1', 18086)); srv.listen(1)
    c, _ = srv.accept(); c.settimeout(3)
    try:
        while True:
            b = c.recv(4096)
            if not b: break
            received.extend(b)
            if len(received) >= len(PREFACE + SETTINGS + PING):
                c.sendall(SETTINGS)  # server SETTINGS back to the client
    except socket.timeout: pass
    c.close(); srv.close()
threading.Thread(target=origin, daemon=True).start(); time.sleep(0.3)
p = subprocess.Popen([BIN, '--bind-address', '127.0.0.1', '--listen-port', '18096', '--destination-uri', 'http://127.0.0.1:18086', '--no-capture'],
                     stdout=open('/tmp/rust-proxy-qual/h2c_proxy.log', 'w'), stderr=subprocess.STDOUT)
for _ in range(100):
    try: s = socket.create_connection(('127.0.0.1', 18096), timeout=1); break
    except OSError: time.sleep(0.1)
s.sendall(PREFACE + SETTINGS + PING); s.settimeout(3)
try: back = s.recv(4096)
except Exception as e: back = repr(e).encode()
time.sleep(0.5); s.close(); p.send_signal(signal.SIGTERM); p.wait(timeout=20)
sent = PREFACE + SETTINGS + PING
print('client sent', len(sent), 'bytes; origin received', len(received), 'bytes; identical:', bytes(received) == sent)
print('client got back:', back[:40])
