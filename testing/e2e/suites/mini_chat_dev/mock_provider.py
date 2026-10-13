"""Mock OpenAI / Azure OpenAI compatible provider for the mini-chat e2e suite.

Pure stdlib (``http.server``).  Started in-process on a free port; the server
under test reaches it through OAGW (upstream aliases ``mock-openai`` /
``mock-azure`` both point at this one server).  See README.md for the full
script reference; the short version:

Data plane (both OpenAI and Azure path shapes are accepted, the optional
``/openai`` and ``/v1`` prefixes are stripped before routing)::

    POST   /v1/responses            /openai/v1/responses
    POST   /v1/files                /openai/files
    DELETE /v1/files/{id}
    POST   /v1/vector_stores
    POST   /v1/vector_stores/{id}/files
    GET    /v1/vector_stores/{id}/files/{fid}
    DELETE /v1/vector_stores/{id}

Control plane::

    POST /__control/reset      clear queues, recorded requests, file state
    POST /__control/enqueue    {"route": <key>, "script": {...}, "repeat": 1}
    GET  /__control/requests   {"requests": [...]}  (?method=&path=&route=)

Route keys: ``responses``, ``files``, ``vector_store_file_status``,
``delete_file``, ``delete_vector_store``, ``summary`` (and ``vector_stores``
for ``POST /vector_stores`` creation).
"""

from __future__ import annotations

import json
import random
import re
import select
import socket
import string
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlsplit

ROUTE_KEYS = (
    "responses",
    "files",
    "vector_stores",
    "vector_store_file_status",
    "delete_file",
    "delete_vector_store",
    "summary",
)

#: Requests whose JSON body has ``model == SUMMARY_MODEL`` are served from the
#: ``summary`` queue (thread-summary worker) instead of ``responses``.
SUMMARY_MODEL = "gpt-4.1-mini"

DEFAULT_USAGE = {"input_tokens": 12, "output_tokens": 5}
DEFAULT_DELTAS = ("Hello", " world")
DEFAULT_SUMMARY_TEXT = (
    "<analysis>Reviewed the conversation.</analysis>"
    "<summary>Summary of the conversation so far.</summary>"
)
DEFAULT_SUMMARY_USAGE = {"input_tokens": 20, "output_tokens": 8}

#: Upper bound for a ``hang`` script before the mock gives up and closes.
HANG_TIMEOUT_S = 300.0

_ALNUM = string.ascii_letters + string.digits
_PATH_RE = re.compile(r"^(?:/openai)?(?:/v1)?/(responses|files|vector_stores)(/.*)?$")


def rand_id(prefix: str, n: int = 24) -> str:
    return prefix + "".join(random.choices(_ALNUM, k=n))


# ── Event / script builders (JSON-serialisable; used with ``enqueue``) ────────


def ev(type_: str, data: dict[str, Any] | None = None, *, event: str | None = None,
       delay_ms: int | None = None) -> dict[str, Any]:
    """One scripted SSE event.  Wire frame: ``event: <event or type>\\ndata: {"type": type, ...data}``."""
    out: dict[str, Any] = {"type": type_, "data": data or {}}
    if event is not None:
        out["event"] = event
    if delay_ms is not None:
        out["delay_ms"] = delay_ms
    return out


def ev_created() -> dict[str, Any]:
    return ev("response.created", {"response": {"status": "in_progress"}})


def ev_delta(text: str) -> dict[str, Any]:
    return ev("response.output_text.delta",
              {"item_id": "msg_mock", "output_index": 0, "content_index": 0, "delta": text})


def ev_completed(usage: dict[str, int] | None = None, text: str | None = None) -> dict[str, Any]:
    resp: dict[str, Any] = {"status": "completed", "usage": usage or dict(DEFAULT_USAGE)}
    if text is not None:
        resp["output"] = [_message_item(text)]
    return ev("response.completed", {"response": resp})


def ev_incomplete(reason: str = "max_output_tokens", usage: dict[str, int] | None = None) -> dict[str, Any]:
    return ev("response.incomplete", {"response": {
        "status": "incomplete", "incomplete_details": {"reason": reason},
        "usage": usage or dict(DEFAULT_USAGE)}})


def ev_failed(code: str = "server_error", message: str = "provider failure",
              usage: dict[str, int] | None = None) -> dict[str, Any]:
    resp: dict[str, Any] = {"status": "failed", "error": {"code": code, "message": message}}
    if usage is not None:
        resp["usage"] = usage
    return ev("response.failed", {"response": resp})


def ev_file_search(done: bool = False) -> dict[str, Any]:
    return ev("response.file_search_call.completed" if done else "response.file_search_call.searching",
              {"item_id": "fs_mock", "output_index": 0})


def ev_web_search(done: bool = False) -> dict[str, Any]:
    return ev("response.web_search_call.completed" if done else "response.web_search_call.searching",
              {"item_id": "ws_mock", "output_index": 0})


def ev_code_interpreter_start() -> dict[str, Any]:
    return ev("response.code_interpreter_call.in_progress", {"item_id": "ci_mock", "output_index": 0})


def ev_code_interpreter_done(logs: str = "ok") -> dict[str, Any]:
    return ev("response.output_item.done", {"output_index": 0, "item": {
        "type": "code_interpreter_call", "id": "ci_mock", "status": "completed",
        "code": "print('ok')", "outputs": [{"type": "logs", "logs": logs}]}})


def ev_annotation(annotation: dict[str, Any], index: int = 0) -> dict[str, Any]:
    return ev("response.output_text.annotation.added", {
        "item_id": "msg_mock", "output_index": 0, "content_index": 0,
        "annotation_index": index, "annotation": annotation})


def ev_url_citation(url: str, title: str = "Example", start: int = 0, end: int = 5) -> dict[str, Any]:
    return ev_annotation({"type": "url_citation", "url": url, "title": title,
                          "start_index": start, "end_index": end})


def ev_file_citation(file_id: str, filename: str = "doc.txt", index: int = 0) -> dict[str, Any]:
    return ev_annotation({"type": "file_citation", "file_id": file_id,
                          "filename": filename, "index": index})


def text_events(*chunks: str, usage: dict[str, int] | None = None) -> list[dict[str, Any]]:
    """created + one delta per chunk + completed."""
    return [ev_created(), *[ev_delta(c) for c in chunks], ev_completed(usage)]


def _message_item(text: str) -> dict[str, Any]:
    return {"type": "message", "id": "msg_mock", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}


# ── Server ───────────────────────────────────────────────────────────────────


class MockProvider:
    """Threaded mock provider.  Thread-safe; usable directly from tests."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        self._lock = threading.Lock()
        self._stop = threading.Event()   # set on stop(); releases hangs
        self._gen = threading.Event()    # replaced on reset(); releases hangs of the old generation
        self._queues: dict[str, list[dict[str, Any]]] = {k: [] for k in ROUTE_KEYS}
        self._requests: list[dict[str, Any]] = []
        self._vs_files: dict[tuple[str, str], str] = {}
        self._seq = 0
        provider = self

        class Handler(_Handler):
            mock = provider

        self._httpd = ThreadingHTTPServer((host, port), Handler)
        self._httpd.daemon_threads = True
        self.host = host
        self.port = self._httpd.server_address[1]
        self._thread: threading.Thread | None = None

    # lifecycle
    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def start(self) -> "MockProvider":
        self._thread = threading.Thread(target=self._httpd.serve_forever, kwargs={"poll_interval": 0.1},
                                        daemon=True, name="mock-provider")
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        self._gen.set()
        self._httpd.shutdown()
        self._httpd.server_close()
        if self._thread:
            self._thread.join(timeout=5)

    # control
    def reset(self) -> None:
        with self._lock:
            for q in self._queues.values():
                q.clear()
            self._requests.clear()
            self._vs_files.clear()
            old, self._gen = self._gen, threading.Event()
        old.set()

    def enqueue(self, route: str, script: dict[str, Any] | None = None, repeat: int = 1) -> None:
        """Queue ``script`` for ``route``; consumed FIFO, ``repeat`` times (<= 0: forever)."""
        if route not in ROUTE_KEYS:
            raise ValueError(f"unknown route key {route!r}; expected one of {ROUTE_KEYS}")
        with self._lock:
            self._queues[route].append({"script": script or {}, "left": repeat if repeat > 0 else -1})

    def requests(self, method: str | None = None, path: str | None = None,
                 route: str | None = None) -> list[dict[str, Any]]:
        """Recorded requests (oldest first); ``path`` is a substring match."""
        with self._lock:
            out = [dict(r) for r in self._requests]
        return [r for r in out
                if (method is None or r["method"] == method.upper())
                and (path is None or path in r["path"])
                and (route is None or r.get("route") == route)]

    # internals used by the handler
    def _pop(self, route: str) -> dict[str, Any] | None:
        with self._lock:
            q = self._queues[route]
            if not q:
                return None
            item = q[0]
            if item["left"] > 0:
                item["left"] -= 1
                if item["left"] == 0:
                    q.pop(0)
            return item["script"]

    def _record(self, rec: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            self._seq += 1
            rec["seq"] = self._seq
            self._requests.append(rec)
            return rec

    def _vs_get(self, vs: str, fid: str) -> str | None:
        with self._lock:
            return self._vs_files.get((vs, fid))

    def _vs_set(self, vs: str, fid: str, status: str) -> None:
        with self._lock:
            self._vs_files[(vs, fid)] = status

    def _generation(self) -> threading.Event:
        with self._lock:
            return self._gen


def _parse_multipart(content_type: str, body: bytes) -> list[dict[str, Any]]:
    m = re.search(r'boundary="?([^";]+)"?', content_type or "")
    if not m:
        return []
    delim = b"--" + m.group(1).encode()
    parts = []
    for chunk in body.split(delim)[1:]:
        if chunk.startswith(b"--"):
            break
        chunk = chunk.lstrip(b"\r\n") if chunk.startswith(b"\r\n") else chunk
        head, _, payload = chunk.partition(b"\r\n\r\n")
        if payload.endswith(b"\r\n"):
            payload = payload[:-2]
        headers = {}
        for line in head.decode("utf-8", "replace").split("\r\n"):
            k, _, v = line.partition(":")
            headers[k.strip().lower()] = v.strip()
        disp = headers.get("content-disposition", "")
        name = re.search(r'\bname="([^"]*)"', disp)
        fname = re.search(r'\bfilename="([^"]*)"', disp)
        parts.append({
            "name": name.group(1) if name else None,
            "filename": fname.group(1) if fname else None,
            "content_type": headers.get("content-type"),
            "size": len(payload),
            "value": None if fname else payload.decode("utf-8", "replace"),
        })
    return parts


class _Handler(BaseHTTPRequestHandler):
    mock: MockProvider
    protocol_version = "HTTP/1.1"
    server_version = "mock-provider/1.0"

    def log_message(self, *args: Any) -> None:  # silence
        pass

    do_GET = do_POST = do_DELETE = do_PUT = do_PATCH = lambda self: self._dispatch()  # noqa: E731

    # ── plumbing ────────────────────────────────────────────────────────────
    def _read_body(self) -> bytes:
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            out = b""
            while True:
                size = int(self.rfile.readline().strip().split(b";")[0] or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    return out
                out += self.rfile.read(size)
                self.rfile.readline()
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def _send_json(self, status: int, body: Any, headers: dict[str, str] | None = None) -> None:
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(raw)
        self.rec["response_status"] = status

    def _client_gone(self) -> bool:
        try:
            r, _, _ = select.select([self.connection], [], [], 0)
            return bool(r) and self.connection.recv(1, socket.MSG_PEEK) == b""
        except (OSError, ValueError):
            return True

    def _sleep(self, seconds: float) -> bool:
        """Interruptible sleep; False when the client went away or the mock was reset/stopped."""
        end = time.monotonic() + seconds
        gen = self.mock._generation()
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return True
            if gen.is_set() or self.mock._stop.is_set():
                return False
            if self._client_gone():
                self.rec["client_disconnected"] = True
                return False
            time.sleep(min(0.02, left))

    # ── dispatch ────────────────────────────────────────────────────────────
    def _dispatch(self) -> None:
        parts = urlsplit(self.path)
        body = self._read_body()
        self.rec: dict[str, Any] = {
            "method": self.command,
            "path": parts.path,
            "query": parse_qs(parts.query),
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "json": None,
            "multipart_fields": None,
            "multipart": None,
            "route": None,
            "response_status": None,
            "client_disconnected": False,
        }
        ctype = self.headers.get("Content-Type", "")
        if body and "multipart/" in ctype:
            mp = _parse_multipart(ctype, body)
            self.rec["multipart"] = mp
            self.rec["multipart_fields"] = [p["name"] for p in mp]
        elif body:
            try:
                self.rec["json"] = json.loads(body)
            except ValueError:
                pass
        if parts.path.startswith("/__control/"):
            return self._control(parts.path, parse_qs(parts.query), body)
        self.mock._record(self.rec)
        try:
            self._route(parts.path)
        except (BrokenPipeError, ConnectionResetError):
            self.rec["client_disconnected"] = True
            self.close_connection = True

    def _control(self, path: str, query: dict[str, list[str]], body: bytes) -> None:
        self.rec = {}
        if path == "/__control/reset" and self.command == "POST":
            self.mock.reset()
            return self._send_json(200, {"ok": True})
        if path == "/__control/enqueue" and self.command == "POST":
            try:
                req = json.loads(body or b"{}")
                self.mock.enqueue(req["route"], req.get("script"), int(req.get("repeat", 1)))
            except (ValueError, KeyError, TypeError) as e:
                return self._send_json(400, {"error": str(e)})
            return self._send_json(200, {"ok": True})
        if path == "/__control/requests" and self.command == "GET":
            reqs = self.mock.requests(
                method=(query.get("method") or [None])[0],
                path=(query.get("path") or [None])[0],
                route=(query.get("route") or [None])[0])
            return self._send_json(200, {"requests": reqs})
        self._send_json(404, {"error": "unknown control endpoint"})

    def _route(self, path: str) -> None:
        m = _PATH_RE.match(path)
        if not m:
            return self._send_json(404, {"error": {"message": f"no route {path}", "type": "not_found"}})
        kind, rest = m.group(1), (m.group(2) or "")
        segs = [s for s in rest.split("/") if s]
        cmd = self.command
        if kind == "responses" and not segs and cmd == "POST":
            return self._responses()
        if kind == "files":
            if not segs and cmd == "POST":
                return self._files_create()
            if len(segs) == 1 and cmd == "DELETE":
                return self._simple("delete_file", {"id": segs[0], "object": "file", "deleted": True})
        if kind == "vector_stores":
            if not segs and cmd == "POST":
                return self._vs_create()
            if len(segs) == 1 and cmd == "DELETE":
                return self._simple("delete_vector_store",
                                    {"id": segs[0], "object": "vector_store.deleted", "deleted": True})
            if len(segs) == 2 and segs[1] == "files" and cmd == "POST":
                return self._vs_file(segs[0], None)
            if len(segs) == 3 and segs[1] == "files" and cmd == "GET":
                return self._vs_file(segs[0], segs[2])
        self._send_json(404, {"error": {"message": f"no route {cmd} {path}", "type": "not_found"}})

    # ── generic scripted JSON endpoints ─────────────────────────────────────
    def _scripted(self, route: str, delay_key: str = "delay_ms") -> tuple[dict[str, Any], bool]:
        """Pop the script, apply the pre-response delay; return (script, still_connected)."""
        self.rec["route"] = route
        script = self.mock._pop(route) or {}
        delay = script.get(delay_key, 0)
        ok = self._sleep(delay / 1000.0) if delay else True
        return script, ok

    def _finish_json(self, script: dict[str, Any], default_body: Any) -> None:
        if script.get("hang"):
            self._sleep(HANG_TIMEOUT_S)
            self.close_connection = True
            return
        status = int(script.get("status", 200))
        body = script["body"] if "body" in script else (
            default_body if status < 400 else {"error": {"message": "mock error", "type": "server_error"}})
        self._send_json(status, body, script.get("headers"))

    def _simple(self, route: str, default_body: Any) -> None:
        script, ok = self._scripted(route)
        if ok:
            self._finish_json(script, default_body)

    def _files_create(self) -> None:
        script, ok = self._scripted("files")
        if not ok:
            return
        fname = next((p["filename"] for p in self.rec.get("multipart") or [] if p["filename"]), None)
        size = next((p["size"] for p in self.rec.get("multipart") or [] if p["filename"]), 0)
        fid = script.get("id") or rand_id("file-")
        self._finish_json(script, {"id": fid, "object": "file", "bytes": size,
                                   "filename": fname or "upload", "purpose": "assistants",
                                   "status": "processed"})

    def _vs_create(self) -> None:
        script, ok = self._scripted("vector_stores")
        if not ok:
            return
        self._finish_json(script, {"id": script.get("id") or rand_id("vs_"), "object": "vector_store",
                                   "status": "completed"})

    def _vs_file(self, vs: str, fid: str | None) -> None:
        script, ok = self._scripted("vector_store_file_status")
        if not ok:
            return
        if fid is None:  # POST: attach file
            fid = (self.rec.get("json") or {}).get("file_id") or rand_id("file-")
            status = script.get("file_status") or "completed"
        else:
            status = script.get("file_status") or self.mock._vs_get(vs, fid) or "completed"
        self.mock._vs_set(vs, fid, status)
        body: dict[str, Any] = {"id": fid, "object": "vector_store.file", "vector_store_id": vs,
                                "status": status, "last_error": None}
        if status == "failed":
            body["last_error"] = {"code": "server_error", "message": "mock indexing failure"}
        self._finish_json(script, body)

    # ── /responses ──────────────────────────────────────────────────────────
    def _responses(self) -> None:
        req = self.rec.get("json")
        if not isinstance(req, dict):
            return self._send_json(400, {"error": {"message": "invalid JSON body", "type": "invalid_request_error"}})
        route = "summary" if req.get("model") == SUMMARY_MODEL else "responses"
        stream = bool(req.get("stream"))
        script, ok = self._scripted(route, "start_delay_ms")
        if not ok:
            return
        status = int(script.get("status", 200))
        if status >= 400:
            return self._send_json(status, script.get("body") or {
                "error": {"message": "mock provider error", "type": "server_error", "code": "mock_error"}},
                script.get("headers"))
        text = DEFAULT_SUMMARY_TEXT if route == "summary" else "".join(DEFAULT_DELTAS)
        usage = DEFAULT_SUMMARY_USAGE if route == "summary" else DEFAULT_USAGE
        if not stream:
            return self._finish_json(script, {
                "id": rand_id("resp_"), "object": "response", "status": "completed",
                "model": req.get("model"), "output": [_message_item(text)], "output_text": text,
                "usage": dict(usage)})
        self._stream(script, req, route, text, usage)

    def _stream(self, script: dict[str, Any], req: dict[str, Any], route: str, text: str,
                usage: dict[str, int]) -> None:
        events = script.get("events")
        if events is None:
            if route == "summary":
                events = [ev_created(), ev_delta(text), ev_completed(usage)]
            else:
                events = text_events(*DEFAULT_DELTAS)
            if script.get("failed"):
                events = events[:1]
        events = list(events)
        failed = script.get("failed")
        if failed:
            f = ev_failed(**failed) if isinstance(failed, dict) else ev_failed()
            events.append(f)
        gap = float(script.get("delay_ms", 0)) / 1000.0  # between consecutive events
        resp_id = rand_id("resp_")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        for k, v in (script.get("headers") or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.flush()
        self.rec["response_status"] = 200

        for i, e in enumerate(events):
            d = float(e["delay_ms"]) / 1000.0 if "delay_ms" in e else (gap if i else 0.0)
            if d and not self._sleep(d):
                self.close_connection = True
                return
            etype = e["type"]
            data = {"type": etype, "sequence_number": i, **(e.get("data") or {})}
            if isinstance(data.get("response"), dict):
                r = data["response"]
                r.setdefault("id", resp_id)
                r.setdefault("object", "response")
                r.setdefault("model", req.get("model"))
            frame = f"event: {e.get('event') or etype}\ndata: {json.dumps(data)}\n\n".encode()
            self._chunk(frame)
            self.rec["events_sent"] = i + 1
        if script.get("hang"):
            self._sleep(HANG_TIMEOUT_S)
            self.close_connection = True
            return
        if script.get("disconnect"):
            self.close_connection = True
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _chunk(self, data: bytes) -> None:
        self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
        self.wfile.flush()


def start_mock(host: str = "127.0.0.1", port: int = 0) -> MockProvider:
    return MockProvider(host, port).start()


if __name__ == "__main__":  # manual run: python3 mock_provider.py [port]
    import sys

    m = start_mock(port=int(sys.argv[1]) if len(sys.argv) > 1 else 0)
    print(f"mock provider on {m.base_url}", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        m.stop()
