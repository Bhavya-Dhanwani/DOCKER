#!/usr/bin/env python3
"""A tiny in-memory Docker Registry v2 for tests (stdlib only). NOT a real registry.

usage: mock-registry.py <port> <auth: none|basic|bearer> [user:password]
Verifies the sha256 of every uploaded blob, checks credentials, and logs each request to stderr.
"""
import base64, hashlib, json, re, sys, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
MODE = sys.argv[2]
USER, PASSWORD = (sys.argv[3] if len(sys.argv) > 3 else "u:p").split(":", 1)
TOKEN = "tok-" + uuid.uuid4().hex
BLOBS, MANIFESTS, UPLOADS = {}, {}, {}  # digest -> bytes, (name, ref) -> (type, bytes), uuid -> name

BASIC = "Basic " + base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s -> %s\n" % (self.command, self.path.split("?")[0], args[1] if len(args) > 1 else ""))

    def send(self, code, body=b"", headers=None):
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def authorized(self):
        a = self.headers.get("Authorization", "")
        return MODE == "none" or (MODE == "basic" and a == BASIC) or (MODE == "bearer" and a == f"Bearer {TOKEN}")

    def challenge(self):
        if MODE == "basic":
            return {"WWW-Authenticate": 'Basic realm="mock"'}
        return {"WWW-Authenticate": f'Bearer realm="http://127.0.0.1:{PORT}/token",service="mock"'}

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def route(self):
        path, _, query = self.path.partition("?")
        if path == "/token":
            if self.headers.get("Authorization") != BASIC:
                return self.send(401, b'{"error":"bad credentials"}')
            return self.send(200, json.dumps({"token": TOKEN}).encode(), {"Content-Type": "application/json"})
        if path == "/v2/":
            return self.send(200, b"{}") if self.authorized() else self.send(401, b"", self.challenge())
        if not self.authorized():
            return self.send(401, b"", self.challenge())

        m = re.fullmatch(r"/v2/(?P<name>.+)/blobs/uploads/(?P<id>[0-9a-f]+)?", path)
        if m:
            if self.command == "POST":
                self.body()
                uid = uuid.uuid4().hex
                UPLOADS[uid] = m["name"]
                return self.send(202, b"", {"Location": f"/v2/{m['name']}/blobs/uploads/{uid}"})
            if self.command == "PUT" and m["id"] in UPLOADS:
                digest = dict(p.split("=", 1) for p in query.split("&")).get("digest", "")
                data = self.body()
                actual = "sha256:" + hashlib.sha256(data).hexdigest()
                if actual != digest:
                    return self.send(400, f"digest mismatch: {actual} != {digest}".encode())
                BLOBS[digest] = data
                return self.send(201, b"", {"Docker-Content-Digest": digest})
        m = re.fullmatch(r"/v2/(?P<name>.+)/blobs/(?P<d>sha256:[0-9a-f]{64})", path)
        if m and self.command in ("GET", "HEAD"):
            data = BLOBS.get(m["d"])
            if data is None:
                return self.send(404, b"")
            return self.send(200, data, {"Content-Type": "application/octet-stream", "Docker-Content-Digest": m["d"]})
        m = re.fullmatch(r"/v2/(?P<name>.+)/manifests/(?P<ref>.+)", path)
        if m:
            key = (m["name"], m["ref"])
            if self.command == "PUT":
                data = self.body()
                digest = "sha256:" + hashlib.sha256(data).hexdigest()
                ctype = self.headers.get("Content-Type", "")
                MANIFESTS[key] = MANIFESTS[(m["name"], digest)] = (ctype, data)
                return self.send(201, b"", {"Docker-Content-Digest": digest})
            if self.command in ("GET", "HEAD") and key in MANIFESTS:
                ctype, data = MANIFESTS[key]
                return self.send(200, data, {"Content-Type": ctype, "Docker-Content-Digest": "sha256:" + hashlib.sha256(data).hexdigest()})
            return self.send(404, b"")
        return self.send(404, b"")

    do_GET = do_HEAD = do_POST = do_PUT = route


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
