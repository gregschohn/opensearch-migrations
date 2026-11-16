#!/usr/bin/env python3
"""Tiny keep-alive HTTP/1.1 origin. Logs every request it receives (method path body) to a file."""
import sys, time, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

PORT = int(sys.argv[1]); LOG = sys.argv[2]
lock = threading.Lock()

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def _handle(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        with lock, open(LOG, "a") as f:
            f.write(f"{time.time():.3f} {self.command} {self.path} {body.decode(errors='replace')}\n")
        out = f'{{"ok":true,"path":"{self.path}"}}'.encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers(); self.wfile.write(out)
    do_GET = do_PUT = do_POST = do_DELETE = _handle

ThreadingHTTPServer.daemon_threads = True
ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
