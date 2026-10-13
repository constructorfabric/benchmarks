"""Mock OpenAI-compatible provider used by the mini-chat smoke tests.

Serves the OpenAI Responses API (streaming and non-streaming), the Files API and the
Vector Stores API on both the OpenAI (`/v1/...`) and Azure (`/openai/...`) paths.
Every request is recorded and can be read back through `GET /_mock/requests` and
cleared through `POST /_mock/reset`.

The streamed answer is controlled by markers in the last user message:

* `[[error]]`        -> `response.failed` with a provider error that contains a file id
* `[[429]]`          -> HTTP 429 with `Retry-After: 7`
* `[[500]]`          -> HTTP 500 with a JSON error body
* `[[web_search]]`   -> one web_search call + a url_citation annotation
* `[[web_search3]]`  -> three web_search calls (exceeds the default per-turn limit)
* `[[file_search]]`  -> file_search call + a file_citation for the first file in the request
* `[[slow]]`         -> 10 deltas with 0.5 s pauses
* `[[incomplete]]`   -> `response.incomplete`
* `[[empty]]`        -> completed without text
* `[[stall]]`        -> waits `CONFIG["stall_secs"]` (default 6 s) before the first delta

`[[file_search]]` cites the first file of the first vector store named in the request's
`file_search` tool (falls back to any known file).

Usage: `python3 mock_provider.py <port>`.
"""

import json
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
REQUESTS = []
FILES = {}
VECTOR_STORES = {}
CONFIG = {"index_status": "completed", "file_upload_status": 200, "stall_secs": 6}


def record(method, path, headers, body):
    with LOCK:
        REQUESTS.append(
            {
                "method": method,
                "path": path,
                "headers": {k.lower(): v for k, v in headers.items()},
                "body": body,
            }
        )


def last_user_text(body):
    items = body.get("input") or []
    for item in reversed(items):
        if isinstance(item, dict) and item.get("role") == "user":
            content = item.get("content")
            if isinstance(content, str):
                return content
            if isinstance(content, list):
                return " ".join(
                    part.get("text", "") for part in content if isinstance(part, dict)
                )
    return ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: N802
        pass

    def _read_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            return self.rfile.read(length)
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                size_line = self.rfile.readline().strip()
                size = int(size_line, 16)
                if size == 0:
                    self.rfile.readline()
                    break
                data += self.rfile.read(size)
                self.rfile.readline()
            return data
        return b""

    def _json(self, status, obj, extra_headers=None):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def _route(self):
        path = self.path.split("?", 1)[0]
        for prefix in ("/openai/v1", "/openai", "/v1"):
            if path.startswith(prefix + "/"):
                return path[len(prefix):]
        return path

    # ---------------------------------------------------------------- GET
    def do_GET(self):  # noqa: N802
        if self.path.startswith("/_mock/requests"):
            with LOCK:
                return self._json(200, REQUESTS)
        record("GET", self.path, dict(self.headers), None)
        route = self._route()
        parts = route.strip("/").split("/")
        if len(parts) == 4 and parts[0] == "vector_stores" and parts[2] == "files":
            return self._json(
                200, {"id": parts[3], "object": "vector_store.file", "status": CONFIG["index_status"]}
            )
        return self._json(404, {"error": {"message": "not found"}})

    # ------------------------------------------------------------- DELETE
    def do_DELETE(self):  # noqa: N802
        record("DELETE", self.path, dict(self.headers), None)
        route = self._route()
        parts = route.strip("/").split("/")
        if parts[0] == "files" and len(parts) == 2:
            existed = FILES.pop(parts[1], None)
            if existed is None:
                return self._json(404, {"error": {"message": "No such file"}})
            return self._json(200, {"id": parts[1], "deleted": True})
        if parts[0] == "vector_stores" and len(parts) == 2:
            VECTOR_STORES.pop(parts[1], None)
            return self._json(200, {"id": parts[1], "deleted": True})
        return self._json(404, {"error": {"message": "not found"}})

    # --------------------------------------------------------------- POST
    def do_POST(self):  # noqa: N802
        raw = self._read_body()
        if self.path.startswith("/_mock/reset"):
            with LOCK:
                REQUESTS.clear()
            return self._json(200, {"ok": True})
        if self.path.startswith("/_mock/config"):
            CONFIG.update(json.loads(raw or b"{}"))
            return self._json(200, CONFIG)
        route = self._route()
        ctype = self.headers.get("Content-Type", "")
        body = None
        if "application/json" in ctype and raw:
            body = json.loads(raw)
        record("POST", self.path, dict(self.headers), body if body is not None else {"_bytes": len(raw), "_ctype": ctype})
        parts = route.strip("/").split("/")
        if route == "/files" or parts == ["files"]:
            if CONFIG["file_upload_status"] != 200:
                return self._json(CONFIG["file_upload_status"], {"error": {"message": "upload failed"}})
            fid = "file-" + uuid.uuid4().hex[:24]
            FILES[fid] = len(raw)
            return self._json(200, {"id": fid, "object": "file", "bytes": len(raw), "purpose": "assistants"})
        if parts == ["vector_stores"]:
            vid = "vs_" + uuid.uuid4().hex[:24]
            VECTOR_STORES[vid] = []
            return self._json(200, {"id": vid, "object": "vector_store"})
        if len(parts) == 3 and parts[0] == "vector_stores" and parts[2] == "files":
            VECTOR_STORES.setdefault(parts[1], []).append(body.get("file_id"))
            return self._json(
                200, {"id": body.get("file_id"), "object": "vector_store.file", "status": CONFIG["index_status"]}
            )
        if parts[-1] == "responses":
            return self._responses(body or {})
        return self._json(404, {"error": {"message": "not found"}})

    def _responses(self, body):
        text = last_user_text(body)
        if "[[429]]" in text:
            return self._json(429, {"error": {"message": "Rate limit"}}, {"Retry-After": "7"})
        if "[[500]]" in text:
            return self._json(500, {"error": {"message": "boom file-abcdef0123456789abcd"}})
        if not body.get("stream"):
            answer = "<analysis>a</analysis><summary>Summary of the conversation.</summary>"
            return self._json(
                200,
                {
                    "id": "resp_" + uuid.uuid4().hex,
                    "object": "response",
                    "status": "completed",
                    "output": [
                        {"type": "message", "role": "assistant",
                         "content": [{"type": "output_text", "text": answer, "annotations": []}]}
                    ],
                    "usage": {"input_tokens": 50, "output_tokens": 20},
                },
            )
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def send(event, data):
            payload = f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()
            self.wfile.write(f"{len(payload):x}\r\n".encode() + payload + b"\r\n")
            self.wfile.flush()

        resp_id = "resp_" + uuid.uuid4().hex
        try:
            send("response.created", {"type": "response.created", "response": {"id": resp_id}})
            if "[[error]]" in text:
                send("response.failed", {"type": "response.failed", "response": {"id": resp_id, "error": {
                    "code": "server_error", "message": "Upstream failed for file-abcdef0123456789abcd"}}})
                return self._end()
            annotations = []
            answer = "Hello from mock."
            if "[[web_search3]]" in text:
                for i in range(3):
                    send("response.web_search_call.searching", {"type": "response.web_search_call.searching", "item_id": f"ws_{i}"})
                    send("response.web_search_call.completed", {"type": "response.web_search_call.completed", "item_id": f"ws_{i}"})
            if "[[web_search]]" in text:
                send("response.web_search_call.searching", {"type": "response.web_search_call.searching", "item_id": "ws_1"})
                send("response.web_search_call.completed", {"type": "response.web_search_call.completed", "item_id": "ws_1"})
                annotations.append({"type": "url_citation", "url": "https://example.com/a", "title": "Example",
                                    "start_index": 0, "end_index": 5})
            if "[[file_search]]" in text:
                send("response.file_search_call.searching", {"type": "response.file_search_call.searching", "item_id": "fs_1"})
                send("response.file_search_call.completed", {"type": "response.file_search_call.completed", "item_id": "fs_1"})
                fid = None
                for tool in body.get("tools") or []:
                    if isinstance(tool, dict) and tool.get("type") == "file_search":
                        for vid in tool.get("vector_store_ids") or []:
                            if VECTOR_STORES.get(vid):
                                fid = VECTOR_STORES[vid][0]
                                break
                    if fid:
                        break
                fid = fid or next(iter(FILES), "file-unknown000000000000")
                annotations.append({"type": "file_citation", "file_id": fid, "filename": "x", "index": 0})
            if "[[stall]]" in text:
                time.sleep(float(CONFIG.get("stall_secs", 6)))
            if "[[slow]]" in text:
                for i in range(10):
                    send("response.output_text.delta", {"type": "response.output_text.delta", "delta": f"tok{i} "})
                    time.sleep(0.5)
            elif "[[empty]]" not in text:
                for piece in ("Hello", " from", " mock."):
                    send("response.output_text.delta", {"type": "response.output_text.delta", "delta": piece})
            final = {
                "id": resp_id,
                "status": "completed",
                "output": [{"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": answer, "annotations": annotations}]}],
                "usage": {"input_tokens": 120, "output_tokens": 30,
                          "input_tokens_details": {"cached_tokens": 10},
                          "output_tokens_details": {"reasoning_tokens": 0}},
            }
            if "[[incomplete]]" in text:
                final["status"] = "incomplete"
                final["incomplete_details"] = {"reason": "max_output_tokens"}
                send("response.incomplete", {"type": "response.incomplete", "response": final})
            else:
                send("response.completed", {"type": "response.completed", "response": final})
            self._end()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _end(self):
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18999
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
