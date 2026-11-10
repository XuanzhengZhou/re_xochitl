#!/usr/bin/env python3
"""mock_server.py —— SERVER.md 协议的一个最小参考实现（联调用，不是生产）。

只够验证设备端客户端：配对 / 清单 / blob / commit / 健康检查。
生产实现要加：TLS、多设备、版本历史、并发锁、原子写。

用法：
    python3 tools/mock_server.py --port 8099 --root /tmp/rmsrv
"""
import argparse
import hashlib
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
STATE = {
    "rev": 0,
    "entries": {},   # path -> {"sha256","size","mtime"} 或 {"deleted":True,"mtime"}
    "blobs": {},     # sha -> 文件路径
    "tokens": {},    # token -> device_id
    "codes": {"12345678"},   # 预置配对码
}
ROOT = "/tmp/rmsrv"


def blob_path(sha):
    return os.path.join(ROOT, "blobs", sha[:2], sha)


def save_blob(sha, data):
    p = blob_path(sha)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    if not os.path.exists(p):
        tmp = p + ".tmp"
        with open(tmp, "wb") as f:
            f.write(data)
        os.replace(tmp, p)
    STATE["blobs"][sha] = p


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, code, body=b"", ctype="application/json"):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _auth(self):
        h = self.headers.get("Authorization", "")
        tok = h[7:] if h.startswith("Bearer ") else ""
        return tok if tok in STATE["tokens"] else None

    def _read(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        if self.path == "/api/v1/health":
            return self._send(200, json.dumps({"ok": True, "rev": STATE["rev"]}))
        if not self._auth():
            return self._send(401, json.dumps({"error": "bad_token"}))
        if self.path == "/api/v1/manifest":
            with LOCK:
                ents = [dict(path=p, **v) for p, v in STATE["entries"].items()]
                rev = STATE["rev"]
            return self._send(200, json.dumps({"rev": rev, "entries": ents}))
        if self.path.startswith("/api/v1/blob/"):
            sha = self.path.rsplit("/", 1)[-1]
            p = STATE["blobs"].get(sha)
            if not p or not os.path.exists(p):
                return self._send(404, json.dumps({"error": "no_blob"}))
            with open(p, "rb") as f:
                return self._send(200, f.read(), "application/octet-stream")
        return self._send(404, json.dumps({"error": "not_found"}))

    def do_POST(self):
        body = self._read()
        if self.path == "/api/v1/pair":
            d = json.loads(body or b"{}")
            code = str(d.get("code", "")).strip()
            with LOCK:
                if code not in STATE["codes"]:
                    return self._send(401, json.dumps({"error": "bad_code"}))
                STATE["codes"].discard(code)
                tok = hashlib.sha256(f"{code}{time.time()}".encode()).hexdigest()
                did = "d-" + tok[:8]
                STATE["tokens"][tok] = did
            return self._send(200, json.dumps({"device_id": did, "token": tok,
                                               "server_time": int(time.time() * 1000)}))
        if not self._auth():
            return self._send(401, json.dumps({"error": "bad_token"}))
        if self.path == "/api/v1/blob":
            sha = self.headers.get("X-Sha256", "")
            got = hashlib.sha256(body).hexdigest()
            if sha != got:
                return self._send(400, json.dumps({"error": "sha_mismatch"}))
            with LOCK:
                save_blob(sha, body)
            return self._send(204)
        if self.path == "/api/v1/commit":
            d = json.loads(body or b"{}")
            with LOCK:
                if d.get("base_rev", 0) != STATE["rev"]:
                    return self._send(409, json.dumps({"error": "rev_changed", "rev": STATE["rev"]}))
                conflicts = []
                for p in d.get("puts", []):
                    old = STATE["entries"].get(p["path"], {})
                    if old.get("mtime", 0) > p["mtime"]:
                        conflicts.append(p["path"])
                        continue
                    STATE["entries"][p["path"]] = {
                        "sha256": p["sha256"], "size": 0, "mtime": p["mtime"]}
                for p in d.get("dels", []):
                    STATE["entries"][p["path"]] = {"deleted": True, "mtime": p["mtime"]}
                STATE["rev"] += 1
                rev = STATE["rev"]
            return self._send(200, json.dumps({"rev": rev, "conflicts": conflicts}))
        if self.path == "/api/v1/ocr":
            return self._send(200, json.dumps({
                "lines": [{"text": "mock line", "bbox": [0, 0, 10, 10], "conf": 1.0}],
                "text": "mock line", "engine": "mock"}))
        if self.path == "/api/v1/mail":
            d = json.loads(body or b"{}")
            print("[mail]", json.dumps(d)[:200], flush=True)
            return self._send(200, json.dumps({"ok": True}))
        return self._send(404, json.dumps({"error": "not_found"}))


def main():
    global ROOT
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--root", default="/tmp/rmsrv")
    ap.add_argument("--host", default="127.0.0.1")
    a = ap.parse_args()
    ROOT = a.root
    os.makedirs(os.path.join(ROOT, "blobs"), exist_ok=True)
    srv = ThreadingHTTPServer((a.host, a.port), H)
    print(f"mock server on http://{a.host}:{a.port} root={ROOT} code=12345678", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
