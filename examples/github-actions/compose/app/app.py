# A visit counter: an HTTP server that keeps its count in Redis (plain RESP
# over a socket, so the image needs no packages).
import os
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REDIS = (os.environ.get("REDIS_HOST", "redis"), 6379)


def redis(*args):
    with socket.create_connection(REDIS, timeout=5) as s:
        cmd = f"*{len(args)}\r\n" + "".join(f"${len(str(a))}\r\n{a}\r\n" for a in args)
        s.sendall(cmd.encode())
        reply = s.recv(1024).decode()
    if reply[0] in "+:":
        return reply[1:].strip()
    raise RuntimeError(reply.strip())


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            body = redis("PING")
        else:
            body = f"visits: {redis('INCR', 'visits')}"
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body.encode() + b"\n")


ThreadingHTTPServer(("", 8000), Handler).serve_forever()
