#!/usr/bin/env python3
"""Minimal OpenAI-compatible mock (Responses SSE, Files, Vector Stores) for smoke runs.

GET /__requests returns every recorded request; POST /__script sets the next chat behaviour:
{"mode": "ok"|"error_status"|"failed_event"|"slow", "text": "...", "status": 500, "usage": {...}}
"""
import json
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REQUESTS = []
SCRIPT = {"mode": "ok"}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def _json(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _record(self, body):
        entry = {"method": self.command, "path": self.path, "headers": dict(self.headers)}
        try:
            entry["json"] = json.loads(body) if body else None
        except Exception:
            entry["raw_len"] = len(body)
        with LOCK:
            REQUESTS.append(entry)

    def do_GET(self):
        if self.path == "/__requests":
            with LOCK:
                return self._json(200, REQUESTS)
        self._record(b"")
        if "/vector_stores/" in self.path and "/files/" in self.path:
            return self._json(200, {"status": "completed"})
        self._json(404, {"error": {"message": "not found"}})

    def do_DELETE(self):
        self._record(b"")
        self._json(200, {"deleted": True})

    def do_POST(self):
        body = self._body()
        if self.path == "/__script":
            global SCRIPT
            SCRIPT = json.loads(body)
            return self._json(200, {})
        if self.path == "/__reset":
            with LOCK:
                REQUESTS.clear()
            return self._json(200, {})
        self._record(body)
        p = self.path.split("?")[0]
        if p.endswith("/files") and "vector_stores" not in p:
            return self._json(200, {"id": "file-" + uuid.uuid4().hex, "object": "file"})
        if p.endswith("/vector_stores"):
            return self._json(200, {"id": "vs_" + uuid.uuid4().hex})
        if "/vector_stores/" in p and p.endswith("/files"):
            return self._json(200, {"id": "x", "status": "completed"})
        if p.endswith("/responses"):
            req = json.loads(body)
            return self._responses(req)
        self._json(404, {"error": {"message": "unknown path"}})

    def _responses(self, req):
        script = SCRIPT
        text = script.get("text", "Hello from the mock provider.")
        usage = script.get("usage", {"input_tokens": 42, "output_tokens": 7})
        if script.get("mode") == "error_status":
            return self._json(script.get("status", 500), {"error": {"message": "upstream failure for file-abcdefghijklmnop"}})
        if not req.get("stream"):
            return self._json(200, {"id": "resp_x", "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "<analysis>a</analysis><summary>" + text + "</summary>"}]}], "usage": usage})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

        def send(event, data):
            chunk = ("event: %s\ndata: %s\n\n" % (event, json.dumps(data))).encode()
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()

        send("response.created", {"type": "response.created", "response": {"id": "resp_1"}})
        if script.get("mode") == "slow":
            time.sleep(script.get("delay", 3))
        for word in text.split(" "):
            send("response.output_text.delta", {"type": "response.output_text.delta", "delta": word + " "})
        if script.get("mode") == "failed_event":
            send("response.failed", {"type": "response.failed", "response": {"error": {"message": "boom"}}})
        else:
            send("response.completed", {"type": "response.completed", "response": {"id": "resp_1", "usage": usage, "output": []}})
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
