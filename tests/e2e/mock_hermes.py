#!/usr/bin/env python3
"""Mock Hermes Gateway — accepts forwarded messages from bridge and records them."""
import json, sys, os
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse

HOST = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 9999
LOG_FILE = sys.argv[3] if len(sys.argv) > 3 else "/tmp/bridge-e2e-hermes.log"

received = []

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0))
        body = self.rfile.read(length).decode('utf-8')
        entry = {
            "path": self.path,
            "headers": dict(self.headers),
            "body": body,
        }
        received.append(entry)
        with open(LOG_FILE, 'a') as f:
            json.dump(entry, f)
            f.write('\n')
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')
    
    def do_GET(self):
        if self.path == '/health':
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'ok')
        elif self.path == '/dump':
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps(received, indent=2).encode())
        else:
            self.send_response(404)
            self.end_headers()
    
    def log_message(self, fmt, *args):
        pass  # suppress server logs

srv = HTTPServer((HOST, PORT), Handler)
print(f"mock-hermes listening on {HOST}:{PORT}", flush=True)
srv.serve_forever()
