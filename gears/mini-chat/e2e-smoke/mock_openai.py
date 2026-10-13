#!/usr/bin/env python3
"""Mock OpenAI-compatible provider for the mini-chat black-box smoke test.

Stdlib only (``http.server``, threaded). Serves both URL flavours the gear uses
through OAGW: OpenAI (``/v1/...``) and Azure (``/openai/...?api-version=...``);
routing is by path suffix, so the prefix and query string do not matter.

Endpoints:
- ``POST .../responses``: ``stream: true`` -> Responses SSE (``response.created``,
  two ``response.output_text.delta`` "Hello" / " world", ``response.completed`` with
  usage 12/2); without ``stream`` -> a JSON completion (thread summary).
  When the last user input contains ``MOCK_PROVIDER_ERROR`` the SSE stream ends with
  ``response.failed`` whose message carries a provider id, a URL and a key, which
  the gear must sanitize.
- ``POST .../files`` -> ``{"id": "file-…"}``; ``POST .../vector_stores`` -> ``{"id": "vs_…"}``;
  ``POST .../vector_stores/{vs}/files`` and ``GET .../vector_stores/{vs}/files/{id}``
  -> ``completed``; any ``DELETE`` -> 200.

Every request is recorded (method, path, lower-cased headers, JSON body or the
head of a raw body). Use ``start_mock()`` in-process, or run this file standalone:
``python3 mock_openai.py --port 18080 --record /tmp/mock_requests.jsonl``.
"""
import argparse
import itertools
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ERROR_MARKER = "MOCK_PROVIDER_ERROR"
# Raw provider error text: the gear must strip the response id, URL and key.
RAW_PROVIDER_ERROR = (
    "The server had an error processing resp_mockfailure0001 "
    "(see https://mock-provider.invalid/status, key sk-mockleakedkey1234567)"
)
USAGE = {
    "input_tokens": 12,
    "input_tokens_details": {"cached_tokens": 0},
    "output_tokens": 2,
    "output_tokens_details": {"reasoning_tokens": 0},
    "total_tokens": 14,
}


class Recorder:
    """Thread-safe request log, optionally mirrored to a JSONL file."""

    def __init__(self, path=None):
        self._lock = threading.Lock()
        self._entries = []
        self._path = path

    def add(self, entry):
        with self._lock:
            self._entries.append(entry)
            if self._path:
                with open(self._path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(entry) + "\n")

    def entries(self):
        with self._lock:
            return list(self._entries)


def _last_user_text(req):
    items = req.get("input")
    if not isinstance(items, list):
        return json.dumps(items)
    for item in reversed(items):
        if isinstance(item, dict) and item.get("role") == "user":
            return json.dumps(item.get("content"))
    return ""


def make_handler(recorder, ids):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):  # keep test output quiet
            pass

        # -- plumbing -------------------------------------------------------
        def _body(self):
            n = int(self.headers.get("content-length") or 0)
            if n:
                return self.rfile.read(n)
            if self.headers.get("transfer-encoding", "").lower() == "chunked":
                data = b""
                while True:
                    size = int(self.rfile.readline().strip(), 16)
                    if size == 0:
                        self.rfile.readline()
                        break
                    data += self.rfile.read(size)
                    self.rfile.readline()
                return data
            return b""

        def _record(self, body):
            entry = {
                "method": self.command,
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
            }
            if "json" in self.headers.get("content-type", ""):
                try:
                    entry["json"] = json.loads(body or b"null")
                except ValueError:
                    entry["raw_head"] = body[:300].decode("utf-8", "replace")
            else:
                entry["raw_len"] = len(body)
                entry["raw_head"] = body[:300].decode("utf-8", "replace")
            recorder.add(entry)
            return entry

        def _json(self, status, obj):
            data = json.dumps(obj).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        # -- verbs ----------------------------------------------------------
        def do_POST(self):
            entry = self._record(self._body())
            path = self.path.split("?")[0]
            if path.endswith("/responses"):
                req = entry.get("json") or {}
                if req.get("stream"):
                    return self._sse(req)
                return self._json(200, {
                    "id": "resp_mocksummary0001", "object": "response", "status": "completed",
                    "model": req.get("model", ""),
                    "output": [{"type": "message", "id": "msg_mocksummary0001", "role": "assistant",
                                "status": "completed",
                                "content": [{"type": "output_text", "annotations": [],
                                             "text": "<summary>mock summary</summary>"}]}],
                    "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
                })
            if "/vector_stores/" in path and path.endswith("/files"):
                return self._json(200, {"id": "vsf_mock", "object": "vector_store.file",
                                        "status": "completed"})
            if path.endswith("/files"):
                return self._json(200, {"id": f"file-mock{next(ids):012d}", "object": "file"})
            if path.endswith("/vector_stores"):
                return self._json(200, {"id": f"vs_mock{next(ids):012d}", "object": "vector_store"})
            return self._json(404, {"error": {"message": "not found"}})

        def do_GET(self):
            self._record(b"")
            if "/vector_stores/" in self.path and "/files/" in self.path:
                return self._json(200, {"id": "vsf_mock", "status": "completed"})
            return self._json(404, {"error": {"message": "not found"}})

        def do_DELETE(self):
            self._record(b"")
            return self._json(200, {"deleted": True})

        def _sse(self, req):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("connection", "close")
            self.end_headers()
            resp = {"id": "resp_mock0000000001", "object": "response",
                    "model": req.get("model", ""), "status": "in_progress"}
            events = [("response.created",
                       {"type": "response.created", "sequence_number": 0, "response": resp})]
            if ERROR_MARKER in _last_user_text(req):
                events.append(("response.failed", {
                    "type": "response.failed", "sequence_number": 1,
                    "response": {**resp, "status": "failed",
                                 "error": {"code": "server_error", "message": RAW_PROVIDER_ERROR}},
                }))
            else:
                for seq, delta in ((1, "Hello"), (2, " world")):
                    events.append(("response.output_text.delta", {
                        "type": "response.output_text.delta", "sequence_number": seq,
                        "item_id": "msg_mock0000000001", "output_index": 0,
                        "content_index": 0, "delta": delta}))
                events.append(("response.completed", {
                    "type": "response.completed", "sequence_number": 3, "response": {
                        **resp, "status": "completed", "usage": USAGE,
                        "output": [{"type": "message", "id": "msg_mock0000000001",
                                    "role": "assistant", "status": "completed",
                                    "content": [{"type": "output_text", "text": "Hello world",
                                                 "annotations": []}]}]}}))
            for name, data in events:
                self.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
                self.wfile.flush()
            self.close_connection = True

    return Handler


def start_mock(host="127.0.0.1", port=0, record_path=None):
    """Start the mock in a daemon thread; returns ``(server, recorder)``.

    ``server.server_address[1]`` is the bound port; call ``server.shutdown()`` to stop.
    """
    recorder = Recorder(record_path)
    server = ThreadingHTTPServer((host, port), make_handler(recorder, itertools.count(1)))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, name="mock-openai", daemon=True).start()
    return server, recorder


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--record", default=None, help="append every request as JSONL to this file")
    args = ap.parse_args()
    server, _ = start_mock(args.host, args.port, args.record)
    print(f"mock listening on {args.host}:{server.server_address[1]}", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
