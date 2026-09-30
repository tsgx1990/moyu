#!/usr/bin/env python3
"""Minimal local Blossom blob server for moyu's attachment E2E.

A Blossom server is HTTP blob storage, SEPARATE from a Nostr relay -- the E2E's
local `nostr-rs-relay` is NOT one, and no Blossom server ships on the machine or
in either repo. This is the smallest stand-in that MDK's Blossom client
(`crates/marmot-app/src/media/blossom.rs`) actually talks to:

  * PUT /upload   -- body = the ENCRYPTED blob; header `X-SHA-256` = hex sha256
                     of that body. We verify the hash (integrity, exactly what a
                     real server does), store the blob keyed by hash, and return
                     a BUD-02 descriptor whose `sha256` == the hash (MDK rejects
                     the upload if it doesn't match). `Authorization: Nostr ...`
                     is accepted as-is -- we don't re-verify the signed auth
                     event; this is a loopback test fixture, not a real server.
  * GET /<hash>.bin -- serve the stored (still-encrypted) bytes, or 404.

moyu never decrypts here: the ChaCha20Poly1305 key is derived inside the MLS
group and never leaves it, so this server only ever sees ciphertext -- exactly
the third-party-blob-store trust model documented in the design (§7/§8). NEVER
point moyu at a public Blossom server for tests; this stays on loopback.

Usage: blossom-mock.py <port> [store_dir]   (store_dir defaults to a tempdir)
Prints `BLOSSOM_READY <port> <store_dir>` on stdout once listening.
"""
import hashlib
import http.server
import json
import os
import socketserver
import sys
import tempfile

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3000
STORE = sys.argv[2] if len(sys.argv) > 2 else tempfile.mkdtemp(prefix="blossom-mock-")
os.makedirs(STORE, exist_ok=True)

# A blob hash is a 64-char hex sha256; reject anything else to avoid path games.
def _hash_from(path):
    name = path.lstrip("/")
    if name.endswith(".bin"):
        name = name[:-4]
    if len(name) == 64 and all(c in "0123456789abcdef" for c in name.lower()):
        return name.lower()
    return None


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):  # keep the E2E output clean
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self):
        if self.path.rstrip("/") != "/upload":
            self._json(404, {"message": "not found"})
            return
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        digest = hashlib.sha256(body).hexdigest()
        claimed = (self.headers.get("X-SHA-256") or "").lower()
        if claimed and claimed != digest:
            # Integrity failure -- exactly what a real Blossom server rejects.
            self._json(400, {"message": "X-SHA-256 does not match body"})
            return
        with open(os.path.join(STORE, digest), "wb") as f:
            f.write(body)
        host = self.headers.get("Host", "127.0.0.1:%d" % PORT)
        self._json(
            201,
            {
                "sha256": digest,  # MDK checks this == the encrypted hash
                "url": "http://%s/%s.bin" % (host, digest),
                "size": len(body),
                "type": self.headers.get("Content-Type", "application/octet-stream"),
                "uploaded": 0,
            },
        )

    def do_GET(self):
        digest = _hash_from(self.path)
        blob = os.path.join(STORE, digest) if digest else None
        if not blob or not os.path.exists(blob):
            self._json(404, {"message": "blob not found"})
            return
        data = open(blob, "rb").read()
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    httpd = Server(("127.0.0.1", PORT), Handler)
    print("BLOSSOM_READY %d %s" % (PORT, STORE), flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
