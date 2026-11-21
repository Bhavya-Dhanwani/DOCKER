#!/usr/bin/env python3
"""A tiny in-memory Docker Registry v2 for tests (stdlib only). NOT a real registry.

usage: mock-registry.py <port> <auth: none|basic|bearer> [user:password] [flaky]
Verifies the sha256 of every uploaded blob, checks credentials, and logs each request to stderr.
Blobs are per repository (like a real registry), so HEAD only finds what was pushed/mounted into that repo.

Supports: monolithic and chunked (PATCH + Content-Range, GET status) uploads, cross-repo mount
(POST ?mount=&from=), and with `flaky`: a 503 (Retry-After: 0) on the first try of every blob PUT,
manifest PUT/GET and on the first non-first PATCH of every upload (the chunk is dropped, so the
client must resume from the offset reported by the status GET).

Test hooks (no auth; POST):
  /__recompress/<repo>/<tag>/<newtag>            re-store the image's gzip layers as zstd layers (hand-made zstd frames)
  /__index/<repo>/<tag>/<newtag>/<amd64|arm64>   publish an OCI index: that arch -> the real manifest, the other arch ->
                                                  a decoy copy (different digest), plus an attestation entry; replies JSON
"""
import base64, gzip, hashlib, json, re, struct, sys, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
MODE = sys.argv[2]
USER, PASSWORD = (sys.argv[3] if len(sys.argv) > 3 else "u:p").split(":", 1)
FLAKY = "flaky" in sys.argv[4:]
TOKEN = "tok-" + uuid.uuid4().hex
BLOBS = {}      # digest -> bytes (content addressed)
REPO_BLOBS = {}  # repo -> set of digests visible in that repo
MANIFESTS = {}  # (name, ref) -> (type, bytes)
UPLOADS = {}    # uuid -> {"name": repo, "data": bytearray, "patched": bool}
SEEN = set()    # for flaky: requests that already got their 503

BASIC = "Basic " + base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()
OCI_ZSTD = "application/vnd.oci.image.layer.v1.tar+zstd"


def sha(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def zstd_raw(data):
    """A valid zstd frame made only of raw (stored) blocks: magic, header (window descriptor 1 MiB), blocks."""
    out = b"\x28\xb5\x2f\xfd\x00\x50"
    chunks = [data[i:i + 65536] for i in range(0, len(data), 65536)] or [b""]
    for i, c in enumerate(chunks):
        out += struct.pack("<I", (1 if i == len(chunks) - 1 else 0) | (len(c) << 3))[:3] + c
    return out


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

    def flake(self, key):
        """With `flaky`: fail the first try of `key` with a 503 (the body must be drained first)."""
        if FLAKY and key not in SEEN:
            SEEN.add(key)
            self.body()
            self.send(503, b"try again", {"Retry-After": "0"})
            return True
        return False

    def hook(self, path):
        m = re.fullmatch(r"/__recompress/(?P<n>.+)/(?P<t>[^/]+)/(?P<nt>[^/]+)", path)
        if m:
            ctype, data = MANIFESTS[(m["n"], m["t"])]
            mf = json.loads(data)
            for l in mf["layers"]:
                raw = gzip.decompress(BLOBS[l["digest"]])
                z = zstd_raw(raw)
                BLOBS[sha(z)] = z
                REPO_BLOBS[m["n"]].add(sha(z))
                l.update(digest=sha(z), size=len(z), mediaType=OCI_ZSTD)
            out = json.dumps(mf).encode()
            MANIFESTS[(m["n"], m["nt"])] = MANIFESTS[(m["n"], sha(out))] = (ctype, out)
            return self.send(200, json.dumps({"digest": sha(out)}).encode())
        m = re.fullmatch(r"/__index/(?P<n>.+)/(?P<t>[^/]+)/(?P<nt>[^/]+)/(?P<arch>amd64|arm64)", path)
        if m:
            ctype, data = MANIFESTS[(m["n"], m["t"])]
            decoy = json.dumps({**json.loads(data), "annotations": {"decoy": "1"}}).encode()
            MANIFESTS[(m["n"], sha(decoy))] = (ctype, decoy)
            other = "amd64" if m["arch"] == "arm64" else "arm64"
            plat = lambda a: {"architecture": a, "os": "linux"}
            idx = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
                {"mediaType": ctype, "digest": sha(decoy), "size": len(decoy), "platform": plat(other)},
                {"mediaType": ctype, "digest": sha(data), "size": len(data), "platform": plat(m["arch"])},
                {"mediaType": ctype, "digest": "sha256:" + "0" * 64, "size": 1, "platform": {"architecture": "unknown", "os": "unknown"}},
            ]}).encode()
            MANIFESTS[(m["n"], m["nt"])] = MANIFESTS[(m["n"], sha(idx))] = ("application/vnd.oci.image.index.v1+json", idx)
            return self.send(200, json.dumps({"real": sha(data), "decoy": sha(decoy), "index": sha(idx)}).encode())
        return self.send(404, b"")

    def route(self):
        path, _, query = self.path.partition("?")
        if path.startswith("/__"):
            return self.hook(path)
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
            name, uid = m["name"], m["id"]
            q = dict(p.split("=", 1) for p in query.split("&") if "=" in p)
            if self.command == "POST":
                if self.flake(("POST", self.path)):
                    return
                self.body()
                digest, src = q.get("mount"), q.get("from")
                if digest and src and digest in REPO_BLOBS.get(src, ()):
                    REPO_BLOBS.setdefault(name, set()).add(digest)
                    sys.stderr.write(f"MOUNT {name} <- {src} {digest[:19]}\n")
                    return self.send(201, b"", {"Docker-Content-Digest": digest})
                uid = uuid.uuid4().hex
                UPLOADS[uid] = {"name": name, "data": bytearray(), "patched": False}
                return self.send(202, b"", {"Location": f"/v2/{name}/blobs/uploads/{uid}"})
            up = UPLOADS.get(uid)
            if up is None:
                return self.send(404, b"")
            loc = {"Location": f"/v2/{name}/blobs/uploads/{uid}"}
            if self.command == "GET":  # upload status: how many bytes do you have?
                rng = {"Range": f"0-{len(up['data']) - 1}"} if up["data"] else {}
                return self.send(204, b"", {**loc, **rng})
            if self.command == "PATCH":
                data = self.body()
                start = int(self.headers["Content-Range"].split("-")[0]) if "Content-Range" in self.headers else len(up["data"])
                if FLAKY and start > 0 and (uid, start) not in SEEN:
                    SEEN.add((uid, start))  # drop this chunk: the client has to ask where we are and resume
                    return self.send(503, b"try again", {"Retry-After": "0"})
                if start != len(up["data"]):
                    return self.send(416, b"", {**loc, "Range": f"0-{len(up['data']) - 1}"})
                up["data"] += data
                up["patched"] = True
                return self.send(202, b"", {**loc, "Range": f"0-{len(up['data']) - 1}"})
            if self.command == "PUT":
                if self.flake(("PUT", path)):
                    return
                digest = q.get("digest", "")
                up["data"] += self.body()
                actual = sha(bytes(up["data"]))
                if actual != digest:
                    return self.send(400, f"digest mismatch: {actual} != {digest}".encode())
                BLOBS[digest] = bytes(up["data"])
                REPO_BLOBS.setdefault(name, set()).add(digest)
                del UPLOADS[uid]
                kind = "chunked" if up["patched"] else "monolithic"
                sys.stderr.write(f"BLOB {name} {digest[:19]} {kind} {len(BLOBS[digest])}\n")
                return self.send(201, b"", {"Docker-Content-Digest": digest})
        m = re.fullmatch(r"/v2/(?P<name>.+)/blobs/(?P<d>sha256:[0-9a-f]{64})", path)
        if m and self.command in ("GET", "HEAD"):
            data = BLOBS.get(m["d"])
            if data is None or m["d"] not in REPO_BLOBS.get(m["name"], ()):
                return self.send(404, b"")
            return self.send(200, data, {"Content-Type": "application/octet-stream", "Docker-Content-Digest": m["d"]})
        m = re.fullmatch(r"/v2/(?P<name>.+)/manifests/(?P<ref>.+)", path)
        if m:
            key = (m["name"], m["ref"])
            if self.command == "PUT":
                if self.flake(("MPUT", path)):
                    return
                data = self.body()
                digest = sha(data)
                ctype = self.headers.get("Content-Type", "")
                MANIFESTS[key] = MANIFESTS[(m["name"], digest)] = (ctype, data)
                return self.send(201, b"", {"Docker-Content-Digest": digest})
            if self.command in ("GET", "HEAD") and key in MANIFESTS:
                if self.command == "GET" and self.flake(("MGET", path)):
                    return
                ctype, data = MANIFESTS[key]
                return self.send(200, data, {"Content-Type": ctype, "Docker-Content-Digest": sha(data)})
            return self.send(404, b"")
        return self.send(404, b"")

    do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = route


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
