#!/usr/bin/env python3
"""OpenAI-compatible mock provider for mini-chat end-to-end tests.

Endpoints: Responses API (streaming and buffered), Chat Completions
(streaming), Files, Vector Stores. Behaviour of a chat request is selected by
directives in the request's last user text:

  MOCK_SLOW        20 deltas, 0.25 s apart
  MOCK_HANG        stream starts, then nothing for 300 s
  MOCK_HTTP_429    HTTP 429 with Retry-After: 7
  MOCK_HTTP_500    HTTP 500
  MOCK_HTTP_400    HTTP 400
  MOCK_FAILED      response.failed event (MOCK_FAILED_IDS: message with provider ids,
                   MOCK_FAILED_USAGE: carries usage)
  MOCK_DELAY_FIRST=n  wait n seconds before the first event after response.created
  MOCK_INCOMPLETE  response.incomplete (max_output_tokens)
  MOCK_CLOSE       connection closed mid-stream without a terminal event
  MOCK_WEB[=n]     n (default 1) web_search_call events + url_citation annotation
  MOCK_FILE        file_search_call events + file_citation annotation
  MOCK_CODE        code interpreter call with logs
  MOCK_ECHO        answer is the request's last user text
  MOCK_LONG        ~4000-character answer
  MOCK_USAGE=a,b   usage input_tokens=a, output_tokens=b

Test control: GET /_mock/requests (recorded requests), POST /_mock/reset,
POST /_mock/config (JSON: {"index_status": "completed|in_progress|failed",
"index_delay_secs": n, "upload_status": 200, "summary_text": "...",
"delete_fail_count": n, "summary_fail_count": n}).
"""

import json
import os
import re
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
REQUESTS = []
FILES = {}
VECTOR_STORES = {}
DEFAULT_CONFIG = {
    "index_status": "completed",
    "index_delay_secs": 0,
    "upload_status": 200,
    "vs_create_status": 200,
    "summary_text": "Mock summary of the conversation.",
    "delete_fail_count": 0,
    "summary_fail_count": 0,
}


def take_failure(key):
    with LOCK:
        n = CONFIG.get(key, 0)
        if n > 0:
            CONFIG[key] = n - 1
            return True
    return False
CONFIG = dict(DEFAULT_CONFIG)


def record(entry):
    with LOCK:
        REQUESTS.append(entry)


def last_user_text(body):
    inp = body.get("input")
    msgs = body.get("messages")
    items = inp if isinstance(inp, list) else (msgs if isinstance(msgs, list) else [])
    if isinstance(inp, str):
        return inp
    for item in reversed(items):
        if not isinstance(item, dict) or item.get("role") != "user":
            continue
        content = item.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            texts = [
                p.get("text", "")
                for p in content
                if isinstance(p, dict) and p.get("type") in ("input_text", "text")
            ]
            return "\n".join(texts)
    return ""


def usage_for(text):
    m = re.search(r"MOCK_USAGE=(\d+),(\d+)", text)
    if m:
        return int(m.group(1)), int(m.group(2))
    return 20, 10


def answer_for(text):
    if "MOCK_ECHO" in text:
        return text
    if "MOCK_LONG" in text:
        return ("This is a long mock answer. " * 150).strip()
    if "MOCK_SLOW" in text:
        return "".join(f"chunk{i} " for i in range(20))
    return "Hello from the mock provider."


def chunks_of(s, n=8):
    return [s[i : i + n] for i in range(0, len(s), n)] or [""]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if os.getenv("MOCK_DEBUG"):
            sys.stderr.write(fmt % args + "\n")

    # ── helpers ────────────────────────────────────────────────────────────
    def read_body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n:
            return self.rfile.read(n)
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                line = self.rfile.readline().strip()
                size = int(line, 16)
                if size == 0:
                    self.rfile.readline()
                    break
                data += self.rfile.read(size)
                self.rfile.readline()
            return data
        return b""

    def send_json(self, status, obj, extra_headers=None):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def start_sse(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    def sse(self, event, data):
        payload = f"event: {event}\ndata: {json.dumps(data)}\n\n" if event else f"data: {data if isinstance(data, str) else json.dumps(data)}\n\n"
        self.wfile.write(payload.encode())
        self.wfile.flush()

    def record_req(self, raw):
        entry = {
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "ts": time.time(),
        }
        ctype = self.headers.get("Content-Type", "")
        if ctype.startswith("application/json") and raw:
            try:
                entry["json"] = json.loads(raw)
            except ValueError:
                entry["raw"] = raw.decode(errors="replace")
        elif raw:
            entry["size"] = len(raw)
            entry["content_type"] = ctype
        record(entry)
        return entry

    # ── routing ────────────────────────────────────────────────────────────
    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/_mock/requests":
            with LOCK:
                return self.send_json(200, list(REQUESTS))
        if path == "/health":
            return self.send_json(200, {"ok": True})
        self.record_req(b"")
        m = re.fullmatch(r".*/vector_stores/([^/]+)/files/([^/]+)", path)
        if m:
            return self.vs_file_status(m.group(1), m.group(2))
        self.send_json(404, {"error": {"message": "not found"}})

    def do_DELETE(self):
        path = self.path.split("?")[0]
        self.record_req(b"")
        if take_failure("delete_fail_count"):
            return self.send_json(500, {"error": {"message": "delete failed"}})
        m = re.fullmatch(r".*/files/([^/]+)", path)
        if m and "/vector_stores/" not in path:
            fid = m.group(1)
            with LOCK:
                existed = FILES.pop(fid, None) is not None
            if not existed:
                return self.send_json(404, {"error": {"message": "No such file"}})
            return self.send_json(200, {"id": fid, "object": "file", "deleted": True})
        m = re.fullmatch(r".*/vector_stores/([^/]+)", path)
        if m:
            with LOCK:
                existed = VECTOR_STORES.pop(m.group(1), None) is not None
            if not existed:
                return self.send_json(404, {"error": {"message": "No such vector store"}})
            return self.send_json(200, {"id": m.group(1), "deleted": True})
        self.send_json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        raw = self.read_body()
        path = self.path.split("?")[0]
        if path == "/_mock/reset":
            with LOCK:
                REQUESTS.clear()
                CONFIG.clear()
                CONFIG.update(DEFAULT_CONFIG)
            return self.send_json(200, {"ok": True})
        if path == "/_mock/config":
            with LOCK:
                CONFIG.update(json.loads(raw or b"{}"))
            return self.send_json(200, dict(CONFIG))
        entry = self.record_req(raw)
        if path.endswith("/responses"):
            return self.handle_responses(entry.get("json") or {})
        if path.endswith("/chat/completions"):
            return self.chat_completions(entry.get("json") or {})
        if path.endswith("/files") and "/vector_stores/" not in path:
            return self.upload_file(raw)
        if re.fullmatch(r".*/vector_stores/([^/]+)/files", path):
            vs = path.split("/vector_stores/")[1].split("/")[0]
            return self.vs_add_file(vs, entry.get("json") or {})
        if re.fullmatch(r".*/vector_stores/([^/]+)/search", path):
            return self.send_json(200, {"object": "vector_store.search_results.page", "data": []})
        if path.endswith("/vector_stores"):
            status = CONFIG.get("vs_create_status", 200)
            if status != 200:
                return self.send_json(status, {"error": {"message": "vector store create failed"}})
            vid = "vs_" + uuid.uuid4().hex[:20]
            with LOCK:
                VECTOR_STORES[vid] = {"files": {}}
            return self.send_json(200, {"id": vid, "object": "vector_store"})
        self.send_json(404, {"error": {"message": "not found"}})

    # ── files / vector stores ──────────────────────────────────────────────
    def upload_file(self, raw):
        status = CONFIG.get("upload_status", 200)
        if status != 200:
            return self.send_json(status, {"error": {"message": "upload failed"}})
        m = re.search(rb'filename="([^"]*)"', raw)
        name = m.group(1).decode(errors="replace") if m else "file"
        fid = "file-" + uuid.uuid4().hex[:24]
        with LOCK:
            FILES[fid] = {"filename": name, "bytes": len(raw)}
        self.send_json(
            200,
            {"id": fid, "object": "file", "bytes": len(raw), "filename": name, "purpose": "assistants"},
        )

    def vs_add_file(self, vs, body):
        fid = body.get("file_id", "")
        with LOCK:
            store = VECTOR_STORES.get(vs)
            if store is None:
                pass
            else:
                store["files"][fid] = {"added": time.time(), "attributes": body.get("attributes")}
        if store is None:
            return self.send_json(404, {"error": {"message": "No such vector store"}})
        return self.send_json(200, {"id": fid, "object": "vector_store.file", "status": self.index_status(vs, fid)})

    def index_status(self, vs, fid):
        status = CONFIG.get("index_status", "completed")
        delay = CONFIG.get("index_delay_secs", 0)
        with LOCK:
            info = VECTOR_STORES.get(vs, {}).get("files", {}).get(fid)
        if info is None:
            return None
        if status == "completed" and delay and time.time() - info["added"] < delay:
            return "in_progress"
        return status

    def vs_file_status(self, vs, fid):
        st = self.index_status(vs, fid)
        if st is None:
            return self.send_json(404, {"error": {"message": "No such file"}})
        return self.send_json(200, {"id": fid, "object": "vector_store.file", "status": st})

    # ── Responses API ──────────────────────────────────────────────────────
    def handle_responses(self, body):
        text = last_user_text(body)
        for code in (429, 500, 400):
            if f"MOCK_HTTP_{code}" in text:
                hdr = {"Retry-After": "7"} if code == 429 else None
                return self.send_json(code, {"error": {"message": f"mock error {code}", "type": "mock"}}, hdr)
        inp, out = usage_for(text)
        rid = "resp_" + uuid.uuid4().hex[:20]
        usage = {
            "input_tokens": inp,
            "output_tokens": out,
            "total_tokens": inp + out,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        }
        if not body.get("stream"):
            if (body.get("metadata") or {}).get("request_type") == "summary" and take_failure("summary_fail_count"):
                return self.send_json(500, {"error": {"message": "summary failed"}})
            answer = CONFIG.get("summary_text") if "summar" in json.dumps(body).lower() else answer_for(text)
            if "<summary>" not in answer and "summar" in json.dumps(body).lower():
                answer = f"<analysis>mock</analysis><summary>{answer}</summary>"
            return self.send_json(
                200,
                {
                    "id": rid,
                    "object": "response",
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": answer, "annotations": []}],
                        }
                    ],
                    "usage": usage,
                },
            )
        self.start_sse()
        try:
            self.stream_responses(body, text, rid, usage)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def stream_responses(self, body, text, rid, usage):
        self.sse("response.created", {"type": "response.created", "response": {"id": rid, "status": "in_progress"}})
        if "MOCK_HANG" in text:
            time.sleep(300)
            return
        m = re.search(r"MOCK_DELAY_FIRST=(\d+(?:\.\d+)?)", text)
        if m:
            time.sleep(float(m.group(1)))
        if "MOCK_FAILED" in text:
            msg = "mock failure"
            if "MOCK_FAILED_IDS" in text:
                msg = "bad file-abcdef1234567890 in vs_abcdef1234567890xyz see https://internal.example.com/x key sk-abcdefghij12345 resp_abc123"
            self.sse(
                "response.failed",
                {"type": "response.failed", "response": {"id": rid, "status": "failed", "error": {"code": "server_error", "message": msg}, "usage": usage if "MOCK_FAILED_USAGE" in text else None}},
            )
            return
        annotations = []
        if "MOCK_WEB" in text:
            m = re.search(r"MOCK_WEB=(\d+)", text)
            for i in range(int(m.group(1)) if m else 1):
                self.sse("response.web_search_call.in_progress", {"type": "response.web_search_call.in_progress", "output_index": i})
                self.sse("response.web_search_call.searching", {"type": "response.web_search_call.searching", "output_index": i})
                self.sse("response.web_search_call.completed", {"type": "response.web_search_call.completed", "output_index": i})
        if "MOCK_FILE" in text:
            self.sse("response.file_search_call.in_progress", {"type": "response.file_search_call.in_progress", "output_index": 0})
            self.sse("response.file_search_call.searching", {"type": "response.file_search_call.searching", "output_index": 0})
            self.sse("response.file_search_call.completed", {"type": "response.file_search_call.completed", "output_index": 0})
        if "MOCK_CODE" in text:
            self.sse("response.code_interpreter_call.in_progress", {"type": "response.code_interpreter_call.in_progress", "output_index": 0})
            self.sse(
                "response.output_item.done",
                {"type": "response.output_item.done", "output_index": 0, "item": {"type": "code_interpreter_call", "id": "ci_1", "outputs": [{"type": "logs", "logs": "42"}]}},
            )
        answer = answer_for(text)
        delay = 0.25 if "MOCK_SLOW" in text else 0.0
        for i, c in enumerate(chunks_of(answer)):
            self.sse("response.output_text.delta", {"type": "response.output_text.delta", "output_index": 1, "content_index": 0, "delta": c})
            if delay:
                time.sleep(delay)
            if "MOCK_CLOSE" in text and i == 1:
                self.wfile.flush()
                self.connection.shutdown(2)
                return
        if "MOCK_WEB" in text:
            annotations.append({"type": "url_citation", "url": "https://example.com/page", "title": "Example Page", "start_index": 0, "end_index": min(5, len(answer))})
        if "MOCK_FILE" in text:
            fid = self.any_file_id()
            annotations.append({"type": "file_citation", "file_id": fid, "filename": "doc.pdf", "index": 0})
        output = [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": answer, "annotations": annotations}],
            }
        ]
        if "MOCK_INCOMPLETE" in text:
            self.sse(
                "response.incomplete",
                {"type": "response.incomplete", "response": {"id": rid, "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": output, "usage": usage}},
            )
            return
        self.sse(
            "response.completed",
            {"type": "response.completed", "response": {"id": rid, "status": "completed", "output": output, "usage": usage}},
        )

    def any_file_id(self):
        with LOCK:
            for store in VECTOR_STORES.values():
                for fid in store["files"]:
                    return fid
            for fid in FILES:
                return fid
        return "file-unknown"

    # ── Chat Completions ───────────────────────────────────────────────────
    def chat_completions(self, body):
        text = last_user_text(body)
        for code in (429, 500, 400):
            if f"MOCK_HTTP_{code}" in text:
                return self.send_json(code, {"error": {"message": f"mock error {code}"}})
        inp, out = usage_for(text)
        answer = answer_for(text)
        if not body.get("stream"):
            return self.send_json(
                200,
                {
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": inp, "completion_tokens": out, "total_tokens": inp + out},
                },
            )
        self.start_sse()
        try:
            for c in chunks_of(answer):
                self.sse(None, {"id": "chatcmpl-1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": c}, "finish_reason": None}]})
            self.sse(None, {"id": "chatcmpl-1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            self.sse(None, {"id": "chatcmpl-1", "object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": inp, "completion_tokens": out, "total_tokens": inp + out}})
            self.sse(None, "[DONE]")
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18090
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    srv.daemon_threads = True
    print(f"mock llm listening on {port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
