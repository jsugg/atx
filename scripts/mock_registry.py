#!/usr/bin/env python3
"""Minimal crates.io-shaped sparse-index registry mock for publish tests.

Serves everything cargo needs to publish against a fully local registry:
the sparse-index config, an empty index until a crate lands, the crate
metadata API, and the `/api/v1/crates/new` upload endpoint. After an
upload succeeds, the sparse index serves the new entry so cargo's
post-publish visibility poll finds it.

Every request is appended to `requests.log` (one JSON object per line) so
tests can assert exactly what was uploaded. Set `FAIL_UPLOADS=1` to make
uploads return a server error, which proves failure propagation end to end.

Usage: python3 mock_registry.py PORT [WORKDIR]

Dummy credentials only: this server never talks to crates.io.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8192
WORKDIR = sys.argv[2] if len(sys.argv) > 2 else "."
STATE_FILE = os.path.join(WORKDIR, "state.json")
LOG_FILE = os.path.join(WORKDIR, "requests.log")


def read_state() -> dict[str, object]:
    try:
        with open(STATE_FILE, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return {"entries": []}


def write_state(state: dict[str, object]) -> None:
    with open(STATE_FILE, "w", encoding="utf-8") as f:
        json.dump(state, f)


def log(event: dict[str, object]) -> None:
    with open(LOG_FILE, "a", encoding="utf-8") as f:
        f.write(json.dumps(event) + "\n")


def index_entry(name: str, vers: str, cksum: str) -> dict[str, object]:
    return {"name": name, "vers": vers, "deps": [], "cksum": cksum, "features": {}, "yanked": False}


class Handler(BaseHTTPRequestHandler):
    def _json(self, code: int, obj: object) -> None:
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler naming
        log({"method": "GET", "path": self.path})
        state = read_state()
        if self.path.endswith("/config.json"):
            # Point downloads and API calls back at this mock.
            return self._json(
                200,
                {"dl": f"http://127.0.0.1:{PORT}/api/v1/crates", "api": f"http://127.0.0.1:{PORT}"},
            )
        if "/index/" in self.path and self.path != "/index/config.json":
            # Only serve entries whose crate file can be downloaded; cargo
            # re-checks the entry against the downloaded .crate after publish.
            wanted = self.path.rsplit("/", 1)[-1]
            lines = "".join(
                json.dumps(index_entry(e["name"], e["vers"], e["cksum"])) + "\n"
                for e in state["entries"]
                if e["name"] == wanted
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(lines)))
            self.end_headers()
            self.wfile.write(lines)
            return
        if self.path == "/api/v1/crates/atx":
            if not state["entries"]:
                return self._json(404, {"errors": [{"detail": "not found"}]})
            latest = state["entries"][-1]
            return self._json(
                200,
                {
                    "crate": {"name": "atx", "max_version": latest["vers"]},
                    "versions": [{"num": e["vers"], "yanked": False} for e in state["entries"]],
                },
            )
        return self._json(404, {"errors": [{"detail": "not found"}]})

    def do_PUT(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler naming
        if self.path != "/api/v1/crates/new":
            log({"method": "PUT", "path": self.path})
            return self._json(404, {"errors": [{"detail": "not found"}]})
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        # Upload format: u32 little-endian JSON length, JSON metadata, .crate blob.
        try:
            meta_len = int.from_bytes(body[0:4], "little")
            meta = json.loads(body[4 : 4 + meta_len])
        except (IndexError, ValueError):
            log({"method": "PUT", "path": self.path, "error": "unparsable body", "content_length": length})
            return self._json(400, {"errors": [{"detail": "unparsable upload"}]})
        event = {
            "method": "PUT",
            "path": self.path,
            "name": meta.get("name"),
            "vers": meta.get("vers"),
            "content_length": length,
        }
        log(event)
        if os.environ.get("FAIL_UPLOADS") == "1":
            return self._json(500, {"errors": [{"detail": "mock upload failure"}]})
        state = read_state()
        if any(e["vers"] == meta.get("vers") for e in state["entries"]):
            # Real crates.io rejects duplicates outright; surface it as an
            # error so failure propagation can be asserted.
            return self._json(409, {"errors": [{"detail": "already exists"}]})
        # The .crate blob follows the metadata block; its sha256 must match
        # what the index advertises so cargo's visibility poll accepts it.
        crate_blob = body[4 + meta_len :]
        cksum = hashlib.sha256(crate_blob).hexdigest()
        state["entries"].append({"name": meta["name"], "vers": meta["vers"], "cksum": cksum})
        write_state(state)
        return self._json(200, {"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}})

    def do_HEAD(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler naming
        self.do_GET()

    def log_message(self, *args: object) -> None:
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
