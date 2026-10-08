"""In-process mock of an OpenAI-compatible provider (Responses API, Files API,
Vector Stores API) used by the mini-chat E2E suite.

The mock runs a ``ThreadingHTTPServer`` in a daemon thread. The gear reaches it
through OAGW (``providers.openai.host/port`` in the server config). Every request
is recorded in memory (method, path, query, lower-cased headers, JSON body or
multipart metadata) and can be inspected from the tests either directly through
the Python API (``MockLLM.requests()``) or over HTTP (``GET /__mock/requests``).

Behaviour of ``POST /v1/responses`` is selected by *directives* embedded in the
text of the LAST user message of the request ``input``:

  (none)              deltas "Hello", " world"; usage 10/5
  [[usage:I:O]]       custom usage on the terminal event
  [[long:N]]          N text deltas ("tok0 ", "tok1 ", ...)
  [[slow]]            first delta at once, then one delta every 3 s (20 deltas)
  [[slow:N:S]]        N deltas, S seconds between them
  [[delay:S]]         sleep S seconds before the first delta (ping tests)
  [[hang]]            one delta, then keep the stream open (SSE comments every
                      second) until the client disconnects or the mock is reset
  [[fail]]            ``response.failed`` whose message contains a provider file
                      id, a response id, a URL and an ``sk-`` key
  [[error_event]]     top-level SSE ``error`` event
  [[http500]]         plain HTTP 500 JSON error (no SSE)
  [[http429]]         HTTP 429 with ``Retry-After: 7`` (no SSE)
  [[websearch:N]]     N web_search searching/completed pairs + one url_citation
  [[filecite]]        file_search searching/completed + a file_citation for the
                      newest file of the vector store named in the request tools
                      (fallback: the newest uploaded file)
  [[filecite:unknown]] file_citation with a file id the gear does not know
  [[codeint:N]]       N code_interpreter in_progress + output_item.done (logs)
  [[incomplete]]      deltas then ``response.incomplete``
  [[empty]]           ``response.completed`` without any delta
  [[ksearch:N]]       ``search_knowledge`` function_call (ends the response) until
                      the input carries N ``function_call_output`` items (default 1),
                      then a normal answer
  [[badtool]]         function_call for a tool the gear never offered

Non-streaming ``POST /v1/responses`` (``"stream"`` false or absent; used by the
thread-summary worker) returns a JSON response whose output text is
``<analysis>...</analysis><summary>MOCK-SUMMARY-MARKER ...</summary>``.

Control endpoints:  ``GET /__mock/requests``, ``POST /__mock/reset``,
``POST /__mock/config`` (merge JSON into the config), ``GET /__mock/state``,
``GET /__mock/health``.

Config keys (``MockLLM.configure(**kw)`` or ``POST /__mock/config``):

  files_fail (bool)              POST /files answers 500
  files_delay (float)            sleep before answering POST /files
  file_delete_fail_count (int)   the next N DELETE /files/{id} answer 500
  vs_create_fail (bool)          POST /vector_stores answers 500
  vs_file_fail (bool)            POST /vector_stores/{id}/files answers 500
  index_status (str)             status returned when a file is added to a
                                 vector store: completed | in_progress | failed
  poll_status (str|None)         if set, overrides the status returned by
                                 GET /vector_stores/{id}/files/{file_id}
  summary_fail ({match, count})  the next ``count`` non-streaming (summary)
                                 calls whose body contains ``match`` answer 500
"""

from __future__ import annotations

import copy
import json
import random
import re
import socket
import string
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Optional
from urllib.parse import parse_qs, urlsplit

SUMMARY_MARKER = "MOCK-SUMMARY-MARKER"
FAKE_FILE_ID_IN_ERROR = "file-abcdefghijklmnopqrstu"
FAKE_RESP_ID_IN_ERROR = "resp_ABCDEF1234567890abcd"
FAKE_URL_IN_ERROR = "https://status.example.com/incident/42"
FAKE_SK_IN_ERROR = "sk-abcdefghijklmnop123456"
FAIL_MESSAGE = (
    f"Upstream failure for {FAKE_FILE_ID_IN_ERROR} in {FAKE_RESP_ID_IN_ERROR}, "
    f"see {FAKE_URL_IN_ERROR} key {FAKE_SK_IN_ERROR}"
)
ERROR_EVENT_MESSAGE = "boom in vs_abcdefghijklmnop1234 see http://internal.example.net/x"
HTTP500_MESSAGE = f"internal failure for {FAKE_FILE_ID_IN_ERROR}"

DEFAULT_DELTAS = ["Hello", " world"]
DEFAULT_USAGE = (10, 5)

DIRECTIVE_RE = re.compile(r"\[\[([a-z_0-9]+)(?::([^\]]*))?\]\]")

_ALNUM = string.ascii_letters + string.digits


def rand_id(prefix: str, n: int = 24) -> str:
    return prefix + "".join(random.choice(_ALNUM) for _ in range(n))


def free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ---------------------------------------------------------------------------
# Request helpers (also used by the tests)
# ---------------------------------------------------------------------------


def item_text(item: Any) -> str:
    """Concatenate the text of one Responses API ``input`` item."""
    if isinstance(item, str):
        return item
    if not isinstance(item, dict):
        return ""
    content = item.get("content")
    if isinstance(content, str):
        return content
    parts = []
    if isinstance(content, list):
        for part in content:
            if isinstance(part, dict):
                if isinstance(part.get("text"), str):
                    parts.append(part["text"])
            elif isinstance(part, str):
                parts.append(part)
    if not parts and isinstance(item.get("text"), str):
        parts.append(item["text"])
    return "".join(parts)


def input_items(body: Optional[dict]) -> list:
    if not isinstance(body, dict):
        return []
    inp = body.get("input")
    if isinstance(inp, str):
        return [{"role": "user", "content": inp}]
    if isinstance(inp, list):
        return inp
    return []


def last_user_text(body: Optional[dict]) -> str:
    for item in reversed(input_items(body)):
        if isinstance(item, dict) and item.get("role") == "user":
            return item_text(item)
    return ""


def parse_directives(text: str) -> dict:
    out: dict = {}
    for name, arg in DIRECTIVE_RE.findall(text or ""):
        out[name] = arg
    return out


def tool_types(body: Optional[dict]) -> list:
    if not isinstance(body, dict):
        return []
    return [t.get("type") for t in (body.get("tools") or []) if isinstance(t, dict)]


# ---------------------------------------------------------------------------
# Mock server
# ---------------------------------------------------------------------------


class _Disconnected(Exception):
    pass


class MockLLM:
    def __init__(self, host: str = "127.0.0.1", port: Optional[int] = None):
        self.host = host
        self.port = port or free_port()
        self._lock = threading.RLock()
        self._requests: list = []
        self._seq = 0
        self._config: dict = {}
        self._files: dict = {}  # file_id -> meta
        self._file_order: list = []
        self._stores: dict = {}  # vs_id -> {"files": {file_id: status}, "order": [...], "deleted": bool}
        self._release = threading.Event()
        self._server: Optional[ThreadingHTTPServer] = None
        self._thread: Optional[threading.Thread] = None

    # -- lifecycle -----------------------------------------------------------

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def start(self) -> "MockLLM":
        mock = self

        class Handler(_Handler):
            pass

        Handler.mock = mock
        self._server = ThreadingHTTPServer((self.host, self.port), Handler)
        self._server.daemon_threads = True
        self._thread = threading.Thread(target=self._server.serve_forever, name="mock-llm", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        self._release.set()
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None

    # -- control API ---------------------------------------------------------

    def reset(self) -> None:
        """Clear recorded requests and config; release hanging streams."""
        with self._lock:
            self._requests.clear()
            self._config.clear()
        self._release.set()
        time.sleep(0.05)
        self._release = threading.Event()

    def release_hangs(self) -> None:
        old = self._release
        self._release = threading.Event()
        old.set()

    def configure(self, **kw) -> None:
        with self._lock:
            for k, v in kw.items():
                if v is None:
                    self._config.pop(k, None)
                else:
                    self._config[k] = v

    def config(self) -> dict:
        with self._lock:
            return copy.deepcopy(self._config)

    def requests(
        self,
        path_contains: Optional[str] = None,
        method: Optional[str] = None,
        contains: Optional[str] = None,
        stream: Optional[bool] = None,
        since_seq: int = 0,
    ) -> list:
        """Return recorded requests (deep copies) matching all filters.

        ``contains`` matches against the raw request body text; ``stream``
        filters ``/responses`` calls by their ``stream`` flag.
        """
        with self._lock:
            reqs = [copy.deepcopy(r) for r in self._requests if r["seq"] > since_seq]
        out = []
        for r in reqs:
            if path_contains and path_contains not in r["path"]:
                continue
            if method and r["method"] != method.upper():
                continue
            if contains and contains not in (r.get("body_text") or ""):
                continue
            if stream is not None:
                body = r.get("json") if isinstance(r.get("json"), dict) else {}
                if bool(body.get("stream")) != stream:
                    continue
            out.append(r)
        return out

    def chat_requests(self, contains: Optional[str] = None, since_seq: int = 0) -> list:
        """Streaming ``/responses`` calls (one per user turn sent to the provider)."""
        return self.requests(path_contains="/responses", method="POST", contains=contains, stream=True, since_seq=since_seq)

    def summary_requests(self, contains: Optional[str] = None, since_seq: int = 0) -> list:
        return self.requests(path_contains="/responses", method="POST", contains=contains, stream=False, since_seq=since_seq)

    def last_seq(self) -> int:
        with self._lock:
            return self._seq

    def state(self) -> dict:
        with self._lock:
            return {
                "files": copy.deepcopy(self._files),
                "file_order": list(self._file_order),
                "vector_stores": copy.deepcopy(self._stores),
            }

    def issued_file_ids(self) -> list:
        with self._lock:
            return list(self._file_order)

    def issued_vector_store_ids(self) -> list:
        with self._lock:
            return list(self._stores.keys())

    # -- internal ------------------------------------------------------------

    def _record(self, rec: dict) -> dict:
        with self._lock:
            self._seq += 1
            rec["seq"] = self._seq
            rec["ts"] = time.time()
            self._requests.append(rec)
            return rec

    def _cfg(self, key: str, default: Any = None) -> Any:
        with self._lock:
            return self._config.get(key, default)

    def _take_counter(self, key: str) -> bool:
        """Decrement an integer config counter; True if it was positive."""
        with self._lock:
            n = int(self._config.get(key) or 0)
            if n > 0:
                self._config[key] = n - 1
                return True
            return False

    def _take_summary_fail(self, body_text: str) -> bool:
        with self._lock:
            sf = self._config.get("summary_fail")
            if not isinstance(sf, dict):
                return False
            match = sf.get("match")
            if match and match not in body_text:
                return False
            n = int(sf.get("count") or 0)
            if n <= 0:
                return False
            sf["count"] = n - 1
            return True


# ---------------------------------------------------------------------------
# HTTP handler
# ---------------------------------------------------------------------------


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    mock: MockLLM = None  # type: ignore[assignment]
    server_version = "MockLLM/1.0"

    def log_message(self, fmt, *args):  # silence default logging
        pass

    # -- body ---------------------------------------------------------------

    def _read_body(self) -> bytes:
        te = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in te:
            chunks = []
            while True:
                line = self.rfile.readline()
                if not line:
                    break
                size_s = line.split(b";", 1)[0].strip()
                if not size_s:
                    continue
                size = int(size_s, 16)
                if size == 0:
                    # trailers
                    while True:
                        tl = self.rfile.readline()
                        if not tl or tl in (b"\r\n", b"\n"):
                            break
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()  # CRLF after chunk
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length > 0 else b""

    # -- response helpers ---------------------------------------------------

    def _send_json(self, status: int, obj: Any, extra_headers: Optional[dict] = None) -> None:
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def _start_sse(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        self.wfile.flush()

    def _chunk(self, data: bytes) -> None:
        try:
            self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, OSError) as e:
            raise _Disconnected() from e

    def _event(self, etype: str, data: dict) -> None:
        payload = dict(data)
        payload.setdefault("type", etype)
        self._chunk(f"event: {etype}\ndata: {json.dumps(payload)}\n\n".encode())

    def _comment(self) -> None:
        self._chunk(b": keepalive\n\n")

    def _end_sse(self) -> None:
        try:
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except OSError:
            pass

    # -- dispatch -----------------------------------------------------------

    def _handle(self, method: str) -> None:
        url = urlsplit(self.path)
        path = url.path
        body = self._read_body() if method in ("POST", "PUT", "PATCH", "DELETE") else b""
        headers = {k.lower(): v for k, v in self.headers.items()}
        rec: dict = {
            "method": method,
            "path": path,
            "query": parse_qs(url.query),
            "headers": headers,
            "body_text": None,
            "json": None,
            "form": None,
        }
        ctype = headers.get("content-type", "")
        if body:
            if "multipart/form-data" in ctype:
                rec["form"] = _parse_multipart(body, ctype)
                rec["body_text"] = json.dumps(
                    {k: v for k, v in (rec["form"] or {}).items() if k != "_files"}
                )
            else:
                try:
                    rec["body_text"] = body.decode("utf-8", "replace")
                except Exception:  # pragma: no cover
                    rec["body_text"] = ""
                try:
                    rec["json"] = json.loads(body)
                except Exception:
                    rec["json"] = None

        if path.startswith("/__mock"):
            return self._control(method, path, rec)

        rec = self.mock._record(rec)
        # Route on the part after "/v1" (tolerates "/openai/v1/..", "/openai/..", or an alias prefix).
        idx = path.find("/v1/")
        norm = path[idx + 3 :] if idx >= 0 else re.sub(r"^/openai", "", path)
        try:
            if norm == "/responses" and method == "POST":
                return self._responses(rec)
            if norm == "/files" and method == "POST":
                return self._files_create(rec)
            m = re.fullmatch(r"/files/([^/]+)", norm)
            if m and method == "DELETE":
                return self._files_delete(m.group(1))
            if m and method == "GET":
                return self._files_get(m.group(1))
            if norm == "/vector_stores" and method == "POST":
                return self._vs_create(rec)
            m = re.fullmatch(r"/vector_stores/([^/]+)", norm)
            if m and method == "DELETE":
                return self._vs_delete(m.group(1))
            if m and method == "GET":
                return self._vs_get(m.group(1))
            m = re.fullmatch(r"/vector_stores/([^/]+)/search", norm)
            if m and method == "POST":
                return self._vs_search(m.group(1), rec)
            m = re.fullmatch(r"/vector_stores/([^/]+)/files", norm)
            if m and method == "POST":
                return self._vs_add_file(m.group(1), rec)
            m = re.fullmatch(r"/vector_stores/([^/]+)/files/([^/]+)", norm)
            if m and method == "GET":
                return self._vs_file_status(m.group(1), m.group(2))
            if m and method == "DELETE":
                return self._send_json(200, {"id": m.group(2), "deleted": True})
            return self._send_json(404, {"error": {"message": f"mock: no route {method} {path}", "type": "not_found"}})
        except _Disconnected:
            rec["client_disconnected"] = True
            self.close_connection = True

    def do_GET(self):  # noqa: N802
        self._handle("GET")

    def do_POST(self):  # noqa: N802
        self._handle("POST")

    def do_DELETE(self):  # noqa: N802
        self._handle("DELETE")

    def do_PUT(self):  # noqa: N802
        self._handle("PUT")

    def do_PATCH(self):  # noqa: N802
        self._handle("PATCH")

    # -- control ------------------------------------------------------------

    def _control(self, method: str, path: str, rec: dict) -> None:
        m = self.mock
        if path == "/__mock/health":
            return self._send_json(200, {"ok": True})
        if path == "/__mock/requests" and method == "GET":
            return self._send_json(200, m.requests())
        if path == "/__mock/reset" and method == "POST":
            m.reset()
            return self._send_json(200, {"ok": True})
        if path == "/__mock/config" and method == "POST":
            m.configure(**(rec.get("json") or {}))
            return self._send_json(200, m.config())
        if path == "/__mock/config" and method == "GET":
            return self._send_json(200, m.config())
        if path == "/__mock/state":
            return self._send_json(200, m.state())
        if path == "/__mock/release" and method == "POST":
            m.release_hangs()
            return self._send_json(200, {"ok": True})
        return self._send_json(404, {"error": "unknown control endpoint"})

    # -- Responses API ------------------------------------------------------

    def _responses(self, rec: dict) -> None:
        body = rec.get("json") if isinstance(rec.get("json"), dict) else {}
        stream = bool(body.get("stream"))
        model = body.get("model") or "mock-model"
        rid = rand_id("resp_")
        rec["response_id"] = rid
        if not stream:
            return self._responses_non_stream(rec, body, rid, model)

        text = last_user_text(body)
        d = parse_directives(text)
        rec["directives"] = d

        if "http500" in d:
            return self._send_json(500, {"error": {"message": HTTP500_MESSAGE, "type": "server_error", "code": "server_error"}})
        if "http429" in d:
            return self._send_json(
                429,
                {"error": {"message": "Rate limit reached", "type": "rate_limit_exceeded", "code": "rate_limit_exceeded"}},
                {"Retry-After": "7"},
            )

        in_t, out_t = DEFAULT_USAGE
        if "usage" in d:
            parts = (d["usage"] or "").split(":")
            in_t = int(parts[0])
            out_t = int(parts[1]) if len(parts) > 1 else 0

        deltas = list(DEFAULT_DELTAS)
        interval = 0.0
        if "long" in d:
            n = int(d["long"] or 50)
            deltas = [f"tok{i} " for i in range(n)]
        if "slow" in d:
            arg = (d["slow"] or "").split(":")
            n = int(arg[0]) if arg and arg[0] else 20
            interval = float(arg[1]) if len(arg) > 1 and arg[1] else 3.0
            deltas = [f"Partial-{i} " for i in range(n)]
        if "hang" in d:
            deltas = ["Hanging-0 "]
        if "empty" in d:
            deltas = []

        msg_id = rand_id("msg_")
        self._start_sse()
        created = {"id": rid, "object": "response", "status": "in_progress", "model": model, "output": []}
        self._event("response.created", {"response": created, "sequence_number": 0})
        self._event("response.in_progress", {"response": created, "sequence_number": 1})

        if "delay" in d:
            self._sleep_checked(float(d["delay"] or 7))

        output_items: list = []
        annotations: list = []

        # -- function calls (knowledge search loop) --
        fn_call = None
        if "badtool" in d:
            fn_call = "delete_everything"
        elif "ksearch" in d:
            want = int(d["ksearch"] or 1)
            done = sum(1 for it in input_items(body) if isinstance(it, dict) and it.get("type") == "function_call_output")
            if done < want:
                fn_call = "search_knowledge"
        if fn_call is not None:
            item = {
                "type": "function_call",
                "id": rand_id("fc_"),
                "call_id": rand_id("call_"),
                "name": fn_call,
                "arguments": json.dumps({"query": "mock knowledge query"}),
                "status": "completed",
            }
            self._event("response.output_item.added", {"item": dict(item, status="in_progress", arguments=""), "output_index": 0})
            self._event("response.output_item.done", {"item": item, "output_index": 0})
            usage = {"input_tokens": in_t, "output_tokens": 5, "total_tokens": in_t + 5}
            self._event(
                "response.completed",
                {"response": {"id": rid, "object": "response", "status": "completed", "model": model, "usage": usage, "output": [item]}},
            )
            rec["function_call"] = fn_call
            self._end_sse()
            return

        # -- tools before text --
        if "websearch" in d:
            n = int(d["websearch"] or 1)
            for i in range(n):
                ws_id = rand_id("ws_")
                self._event("response.web_search_call.searching", {"item_id": ws_id, "output_index": len(output_items)})
                self._event("response.web_search_call.completed", {"item_id": ws_id, "output_index": len(output_items)})
                output_items.append({"type": "web_search_call", "id": ws_id, "status": "completed"})
        if "filecite" in d:
            fs_id = rand_id("fs_")
            self._event("response.file_search_call.in_progress", {"item_id": fs_id, "output_index": len(output_items)})
            self._event("response.file_search_call.searching", {"item_id": fs_id, "output_index": len(output_items)})
            self._event("response.file_search_call.completed", {"item_id": fs_id, "output_index": len(output_items)})
            output_items.append({"type": "file_search_call", "id": fs_id, "status": "completed", "queries": ["q"]})
        if "codeint" in d:
            n = int(d["codeint"] or 1)
            for i in range(n):
                ci_id = rand_id("ci_")
                self._event("response.code_interpreter_call.in_progress", {"item_id": ci_id, "output_index": len(output_items)})
                self._event("response.code_interpreter_call.interpreting", {"item_id": ci_id, "output_index": len(output_items)})
                self._event("response.code_interpreter_call.completed", {"item_id": ci_id, "output_index": len(output_items)})
                item = {
                    "type": "code_interpreter_call",
                    "id": ci_id,
                    "status": "completed",
                    "code": "print(42)",
                    "outputs": [{"type": "logs", "logs": f"ci-output-{i}"}],
                }
                self._event("response.output_item.done", {"item": item, "output_index": len(output_items)})
                output_items.append(item)

        # -- text --
        full = ""
        msg_index = len(output_items)
        if deltas:
            self._event(
                "response.output_item.added",
                {"item": {"type": "message", "id": msg_id, "role": "assistant", "status": "in_progress", "content": []}, "output_index": msg_index},
            )
        for i, dt in enumerate(deltas):
            if i > 0 and interval > 0:
                self._sleep_checked(interval)
            full += dt
            self._event(
                "response.output_text.delta",
                {"item_id": msg_id, "output_index": msg_index, "content_index": 0, "delta": dt},
            )

        if "hang" in d:
            self._hang()
            return

        if "websearch" in d:
            end = max(1, min(len(full), 5))
            annotations.append(
                {
                    "type": "url_citation",
                    "url": "https://example.com/mock-article",
                    "title": "Mock Article",
                    "start_index": 0,
                    "end_index": end,
                }
            )
        if "filecite" in d:
            fid = self._cited_file_id(body, d.get("filecite"))
            annotations.append({"type": "file_citation", "file_id": fid, "filename": "provider-name.bin", "index": len(full)})
        for ai, ann in enumerate(annotations):
            self._event(
                "response.output_text.annotation.added",
                {"item_id": msg_id, "output_index": msg_index, "content_index": 0, "annotation_index": ai, "annotation": ann},
            )

        if deltas:
            self._event("response.output_text.done", {"item_id": msg_id, "output_index": msg_index, "content_index": 0, "text": full})
        message_item = {
            "type": "message",
            "id": msg_id,
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": full, "annotations": annotations}],
        }
        if deltas or annotations:
            output_items.append(message_item)

        usage = {
            "input_tokens": in_t,
            "output_tokens": out_t,
            "total_tokens": in_t + out_t,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        }

        if "fail" in d:
            self._event(
                "response.failed",
                {
                    "response": {
                        "id": rid,
                        "object": "response",
                        "status": "failed",
                        "model": model,
                        "error": {"code": "server_error", "message": FAIL_MESSAGE},
                        "output": [],
                    }
                },
            )
        elif "error_event" in d:
            self._event("error", {"code": "server_error", "message": ERROR_EVENT_MESSAGE, "param": None})
        elif "incomplete" in d:
            self._event(
                "response.incomplete",
                {
                    "response": {
                        "id": rid,
                        "object": "response",
                        "status": "incomplete",
                        "model": model,
                        "incomplete_details": {"reason": "max_output_tokens"},
                        "usage": usage,
                        "output": output_items,
                    }
                },
            )
        else:
            self._event(
                "response.completed",
                {"response": {"id": rid, "object": "response", "status": "completed", "model": model, "usage": usage, "output": output_items}},
            )
        rec["completed_text"] = full
        self._end_sse()

    def _sleep_checked(self, seconds: float) -> None:
        """Sleep while probing the connection with SSE comments."""
        deadline = time.time() + seconds
        while time.time() < deadline:
            if self.mock._release.wait(min(1.0, max(0.0, deadline - time.time()))):
                return
            if time.time() < deadline:
                self._comment()

    def _hang(self) -> None:
        deadline = time.time() + 600
        release = self.mock._release
        while time.time() < deadline:
            if release.wait(1.0):
                break
            self._comment()  # raises _Disconnected when the gear dropped us
        # Released by the test (or timed out): end the body without a terminal event.
        self.close_connection = True
        self._end_sse()

    def _cited_file_id(self, body: dict, arg: Optional[str]) -> str:
        if arg == "unknown":
            return rand_id("file-")
        m = self.mock
        vs_ids: list = []
        for t in body.get("tools") or []:
            if isinstance(t, dict) and t.get("type") == "file_search":
                vs_ids.extend(t.get("vector_store_ids") or [])
        with m._lock:
            for vs in vs_ids:
                st = m._stores.get(vs)
                if st and st["order"]:
                    return st["order"][-1]
            if m._file_order:
                return m._file_order[-1]
        return rand_id("file-")

    def _responses_non_stream(self, rec: dict, body: dict, rid: str, model: str) -> None:
        if self.mock._take_summary_fail(rec.get("body_text") or ""):
            rec["failed_on_purpose"] = True
            return self._send_json(500, {"error": {"message": "summary failure (mock)", "type": "server_error"}})
        text = (
            "<analysis>mock analysis of the conversation</analysis>"
            f"<summary>{SUMMARY_MARKER} The user discussed several topics with the assistant.</summary>"
        )
        resp = {
            "id": rid,
            "object": "response",
            "status": "completed",
            "model": model,
            "output": [
                {
                    "type": "message",
                    "id": rand_id("msg_"),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                }
            ],
            "usage": {
                "input_tokens": 200,
                "output_tokens": 40,
                "total_tokens": 240,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            },
        }
        return self._send_json(200, resp)

    # -- Files API ----------------------------------------------------------

    def _files_create(self, rec: dict) -> None:
        m = self.mock
        delay = float(m._cfg("files_delay") or 0)
        if delay:
            time.sleep(delay)
        if m._cfg("files_fail"):
            return self._send_json(500, {"error": {"message": "files backend failure (mock)", "type": "server_error"}})
        form = rec.get("form") or {}
        f = (form.get("_files") or {}).get("file") or {}
        fid = rand_id("file-")
        meta = {
            "id": fid,
            "object": "file",
            "bytes": f.get("size", 0),
            "created_at": int(time.time()),
            "filename": f.get("filename") or "upload",
            "purpose": form.get("purpose") or "assistants",
            "status": "processed",
            "content_type": f.get("content_type"),
            "deleted": False,
        }
        with m._lock:
            m._files[fid] = meta
            m._file_order.append(fid)
        rec["file_id"] = fid
        out = {k: v for k, v in meta.items() if k not in ("deleted", "content_type")}
        return self._send_json(200, out)

    def _files_delete(self, fid: str) -> None:
        m = self.mock
        if m._take_counter("file_delete_fail_count"):
            return self._send_json(500, {"error": {"message": "delete failure (mock)", "type": "server_error"}})
        with m._lock:
            meta = m._files.get(fid)
            if meta is None or meta.get("deleted"):
                found = False
            else:
                meta["deleted"] = True
                found = True
        if not found:
            return self._send_json(404, {"error": {"message": "No such File object", "type": "invalid_request_error"}})
        return self._send_json(200, {"id": fid, "object": "file", "deleted": True})

    def _files_get(self, fid: str) -> None:
        with self.mock._lock:
            meta = self.mock._files.get(fid)
        if meta is None or meta.get("deleted"):
            return self._send_json(404, {"error": {"message": "No such File object"}})
        return self._send_json(200, {k: v for k, v in meta.items() if k not in ("deleted", "content_type")})

    # -- Vector stores ------------------------------------------------------

    def _vs_create(self, rec: dict) -> None:
        m = self.mock
        if m._cfg("vs_create_fail"):
            return self._send_json(500, {"error": {"message": "vector store failure (mock)", "type": "server_error"}})
        vs = rand_id("vs_")
        name = (rec.get("json") or {}).get("name") if isinstance(rec.get("json"), dict) else None
        with m._lock:
            m._stores[vs] = {"files": {}, "order": [], "deleted": False, "name": name}
        rec["vector_store_id"] = vs
        return self._send_json(
            200,
            {"id": vs, "object": "vector_store", "name": name, "status": "completed", "created_at": int(time.time()), "file_counts": {}},
        )

    def _vs_search(self, vs: str, rec: dict) -> None:
        q = (rec.get("json") or {}).get("query") or ""
        if self.mock._cfg("kb_search_fail"):
            return self._send_json(500, {"error": {"message": "search failed (mock)", "type": "server_error"}})
        data = [
            {"file_id": f"file-kb{i}", "filename": f"kb{i}.md", "score": 0.9 - i / 10, "content": [{"type": "text", "text": f"KB-CHUNK-{i} about {q}"}]}
            for i in range(5)
        ]
        return self._send_json(200, {"object": "vector_store.search_results.page", "search_query": [q], "data": data, "has_more": False})

    def _vs_get(self, vs: str) -> None:
        with self.mock._lock:
            st = self.mock._stores.get(vs)
        if st is None or st["deleted"]:
            return self._send_json(404, {"error": {"message": "No such vector store"}})
        return self._send_json(200, {"id": vs, "object": "vector_store", "status": "completed"})

    def _vs_delete(self, vs: str) -> None:
        m = self.mock
        with m._lock:
            st = m._stores.get(vs)
            if st is None or st["deleted"]:
                found = False
            else:
                st["deleted"] = True
                found = True
        if not found:
            return self._send_json(404, {"error": {"message": "No such vector store"}})
        return self._send_json(200, {"id": vs, "object": "vector_store.deleted", "deleted": True})

    def _vs_add_file(self, vs: str, rec: dict) -> None:
        m = self.mock
        if m._cfg("vs_file_fail"):
            return self._send_json(500, {"error": {"message": "vector store file failure (mock)", "type": "server_error"}})
        body = rec.get("json") if isinstance(rec.get("json"), dict) else {}
        fid = body.get("file_id")
        status = m._cfg("index_status") or "completed"
        with m._lock:
            st = m._stores.get(vs)
            if st is None or st["deleted"]:
                st = None
            else:
                st["files"][fid] = status
                st["order"].append(fid)
        if st is None:
            return self._send_json(404, {"error": {"message": "No such vector store"}})
        return self._send_json(
            200,
            {"id": fid, "object": "vector_store.file", "vector_store_id": vs, "status": status, "attributes": body.get("attributes"), "last_error": None},
        )

    def _vs_file_status(self, vs: str, fid: str) -> None:
        m = self.mock
        with m._lock:
            st = m._stores.get(vs)
            status = None if st is None else st["files"].get(fid)
        if status is None:
            return self._send_json(404, {"error": {"message": "No such vector store file"}})
        override = m._cfg("poll_status")
        if override:
            status = override
            with m._lock:
                m._stores[vs]["files"][fid] = override
        return self._send_json(200, {"id": fid, "object": "vector_store.file", "vector_store_id": vs, "status": status, "last_error": None})


# ---------------------------------------------------------------------------
# Multipart parsing
# ---------------------------------------------------------------------------


def _parse_multipart(body: bytes, content_type: str) -> dict:
    m = re.search(r'boundary="?([^";]+)"?', content_type)
    if not m:
        return {}
    boundary = ("--" + m.group(1)).encode()
    out: dict = {"_files": {}}
    for part in body.split(boundary):
        part = part.strip(b"\r\n")
        if not part or part == b"--":
            continue
        if b"\r\n\r\n" in part:
            head, data = part.split(b"\r\n\r\n", 1)
        elif b"\n\n" in part:
            head, data = part.split(b"\n\n", 1)
        else:
            continue
        if data.endswith(b"\r\n"):
            data = data[:-2]
        hdrs = {}
        for line in head.decode("utf-8", "replace").splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                hdrs[k.strip().lower()] = v.strip()
        disp = hdrs.get("content-disposition", "")
        nm = re.search(r'name="([^"]*)"', disp)
        fn = re.search(r'filename="([^"]*)"', disp)
        name = nm.group(1) if nm else ""
        if fn is not None or "content-type" in hdrs and name == "file":
            out["_files"][name] = {
                "filename": fn.group(1) if fn else None,
                "content_type": hdrs.get("content-type"),
                "size": len(data),
            }
        else:
            out[name] = data.decode("utf-8", "replace")
    return out


if __name__ == "__main__":  # manual run: python mock_llm.py [port]
    import sys

    mk = MockLLM(port=int(sys.argv[1]) if len(sys.argv) > 1 else 18999).start()
    print(f"mock LLM listening on {mk.base_url}")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        mk.stop()
