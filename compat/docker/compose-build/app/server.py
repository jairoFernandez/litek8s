import os, socket
from http.server import BaseHTTPRequestHandler, HTTPServer

def redis_incr(key):
    s = socket.create_connection((os.environ.get("REDIS_HOST", "localhost"), 6379), timeout=3)
    s.sendall(f"*2\r\n$4\r\nINCR\r\n${len(key)}\r\n{key}\r\n".encode())
    reply = s.recv(64).decode()
    s.close()
    return int(reply.strip().lstrip(":"))

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        hits = redis_incr("hits")
        marker = open("/srv/app/vendor/marker").read().strip()
        body = f"version={os.environ['APP_VERSION']} hits={hits} vendor={marker}\n".encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

HTTPServer(("0.0.0.0", 8000), H).serve_forever()
