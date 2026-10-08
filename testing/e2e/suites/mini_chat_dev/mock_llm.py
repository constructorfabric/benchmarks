"""OpenAI-compatible mock LLM provider for the mini-chat black-box suite.

Implements the subset of the OpenAI API the gear calls through OAGW:

- ``POST /v1/responses``: SSE when the body has ``"stream": true`` (chat turns),
  JSON otherwise (thread summary);
- ``POST /v1/files`` (multipart) -> ``{"id": "file-..."}``;
- ``POST /v1/vector_stores`` -> ``{"id": "vs_..."}``;
- ``POST /v1/vector_stores/{vs}/files`` and ``GET /v1/vector_stores/{vs}/files/{fid}``
  -> ``{"status": <vs_file_status>}``;
- ``DELETE /v1/files/{id}`` and ``DELETE /v1/vector_stores/{id}``.

Every request is recorded (method, path, query, headers, JSON body or multipart
fields). Tests script non-default behaviour with :meth:`MockLLM.script`: a
scripted responder is consumed by the first request it matches.
"""

from __future__ import annotations

import email.parser
import email.policy
import itertools
import json
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Optional
from urllib.parse import urlsplit

DEFAULT_TEXT_CHUNKS = ["Hello", " from", " the mock provider!"]
DEFAULT_TEXT = "".join(DEFAULT_TEXT_CHUNKS)
DEFAULT_USAGE = {"input_tokens": 42, "output_tokens": 7}
SUMMARY_TEXT = "The user and the assistant talked about mock topics."


@dataclass
class Recorded:
    method: str
    path: str  # normalized to the OpenAI layout (``/v1/...``)
    query: str
    headers: dict[str, str]
    json: Any = None
    multipart: dict[str, dict[str, Any]] = field(default_factory=dict)
    raw: bytes = b""
    raw_path: str = ""  # path as received
    listener: str = "openai"  # "openai" or "azure" (which mock port received it)

    @property
    def chat_id(self) -> Optional[str]:
        if isinstance(self.json, dict):
            return (self.json.get("metadata") or {}).get("chat_id")
        return None

    @property
    def request_type(self) -> Optional[str]:
        if isinstance(self.json, dict):
            return (self.json.get("metadata") or {}).get("request_type")
        return None


Responder = Callable[["_Handler", Recorded], None]


@dataclass
class _Script:
    method: str
    path_prefix: str
    match: Optional[Callable[[Recorded], bool]]
    responder: Responder
    times: int


# ── response builders (responders) ─────────────────────────────────────────


def text_stream(
    chunks: Optional[list[str]] = None,
    usage: Optional[dict] = None,
    delay: float = 0.0,
    annotations: Optional[list[dict]] = None,
    before: Optional[list[tuple[str, dict]]] = None,
    terminal: str = "response.completed",
    pause_after_created: float = 0.0,
) -> Responder:
    """A Responses SSE stream: created, optional ``before`` events, text deltas,
    optional ``output_item.done`` message with annotations, then the terminal."""

    chunks = DEFAULT_TEXT_CHUNKS if chunks is None else chunks
    usage = DEFAULT_USAGE if usage is None else usage

    def respond(h: "_Handler", req: Recorded) -> None:
        rid = h.mock.next_id("resp_")
        events: list[tuple[str, dict]] = [
            ("response.created", {"type": "response.created", "response": {"id": rid}})
        ]
        events += before or []
        if pause_after_created:
            events.append(("__pause__", {"seconds": pause_after_created}))
        for c in chunks:
            events.append(
                (
                    "response.output_text.delta",
                    {
                        "type": "response.output_text.delta",
                        "output_index": 0,
                        "content_index": 0,
                        "delta": c,
                    },
                )
            )
        text = "".join(chunks)
        if annotations:
            events.append(
                (
                    "response.output_item.done",
                    {
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": text, "annotations": annotations}
                            ],
                        },
                    },
                )
            )
        resp = {"id": rid, "status": "completed", "usage": usage}
        if terminal == "response.incomplete":
            resp["status"] = "incomplete"
            resp["incomplete_details"] = {"reason": "max_output_tokens"}
        events.append((terminal, {"type": terminal, "response": resp}))
        h.send_sse(events, delay=delay)

    return respond


def sse_events(events: list[tuple[str, dict]], delay: float = 0.0) -> Responder:
    """Raw SSE events (no implicit created/completed)."""

    def respond(h: "_Handler", req: Recorded) -> None:
        h.send_sse(events, delay=delay)

    return respond


def held_stream(release: threading.Event, max_wait: float = 30.0, first_chunk: str = "Partial") -> Responder:
    """Sends ``response.created`` and one delta, then holds the connection open
    until ``release`` is set (or ``max_wait`` elapses), then completes."""

    def respond(h: "_Handler", req: Recorded) -> None:
        rid = h.mock.next_id("resp_")
        h.start_sse()
        try:
            h.write_event("response.created", {"type": "response.created", "response": {"id": rid}})
            if first_chunk:
                h.write_event(
                    "response.output_text.delta",
                    {"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": first_chunk},
                )
            deadline = time.monotonic() + max_wait
            while not release.is_set() and time.monotonic() < deadline:
                # Probe the socket so a downstream close is noticed.
                h.wfile.write(b": keep-alive\n\n")
                h.wfile.flush()
                release.wait(0.2)
            h.write_event(
                "response.completed",
                {"type": "response.completed", "response": {"id": rid, "status": "completed", "usage": DEFAULT_USAGE}},
            )
        except (BrokenPipeError, ConnectionResetError, OSError):
            h.mock.disconnects.append(req)
        h.close_connection = True

    return respond


def json_response(status: int, body: Any, headers: Optional[dict[str, str]] = None) -> Responder:
    def respond(h: "_Handler", req: Recorded) -> None:
        h.send_json(status, body, headers)

    return respond


# ── server ────────────────────────────────────────────────────────────────


def normalize_path(path: str) -> str:
    """Azure OpenAI layout -> OpenAI layout: ``/openai/v1/x`` and ``/openai/x`` -> ``/v1/x``."""
    if path.startswith("/openai/v1/"):
        return path[len("/openai"):]
    if path.startswith("/openai/"):
        return "/v1/" + path[len("/openai/"):]
    return path


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    mock: "MockLLM"
    listener: str = "openai"

    # -- low level writers
    def send_json(self, status: int, obj: Any, headers: Optional[dict[str, str]] = None) -> None:
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def start_sse(self) -> None:
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()

    def write_event(self, name: str, data: dict) -> None:
        self.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
        self.wfile.flush()

    def send_sse(self, events: list[tuple[str, dict]], delay: float = 0.0) -> None:
        self.start_sse()
        try:
            for name, data in events:
                if name == "__pause__":
                    time.sleep(data["seconds"])
                    continue
                if delay:
                    time.sleep(delay)
                self.write_event(name, data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        self.close_connection = True

    # -- request handling
    def _record(self) -> Recorded:
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        parts = urlsplit(self.path)
        headers = {k.lower(): v for k, v in self.headers.items()}
        rec = Recorded(
            self.command,
            normalize_path(parts.path),
            parts.query,
            headers,
            raw=raw,
            raw_path=parts.path,
            listener=self.listener,
        )
        ctype = headers.get("content-type", "")
        if raw and "json" in ctype:
            try:
                rec.json = json.loads(raw)
            except ValueError:
                rec.json = None
        elif raw and ctype.startswith("multipart/"):
            msg = email.parser.BytesParser(policy=email.policy.default).parsebytes(
                b"Content-Type: " + ctype.encode() + b"\r\n\r\n" + raw
            )
            for part in msg.iter_parts():
                name = part.get_param("name", header="content-disposition")
                rec.multipart[name] = {
                    "filename": part.get_filename(),
                    "content_type": part.get_content_type(),
                    "data": part.get_payload(decode=True) or b"",
                }
        with self.mock.lock:
            self.mock.requests.append(rec)
        return rec

    def _dispatch(self) -> None:
        rec = self._record()
        responder = self.mock.take_script(rec)
        try:
            (responder or self.mock.default_responder)(self, rec)
        except (BrokenPipeError, ConnectionResetError):
            pass

    do_GET = do_POST = do_DELETE = do_PUT = _dispatch  # noqa: N815

    def log_message(self, fmt: str, *args: Any) -> None:  # silence
        return


class MockLLM:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.requests: list[Recorded] = []
        self.disconnects: list[Recorded] = []
        self._scripts: list[_Script] = []
        self._ids = itertools.count(1)
        self.vs_file_status = "completed"
        # Two listeners sharing one state: the ``openai`` and the ``azure``
        # provider entries get distinct OAGW upstreams (aliases).
        self._servers: dict[str, ThreadingHTTPServer] = {}
        for name in ("openai", "azure"):
            handler = type(f"BoundHandler_{name}", (_Handler,), {"mock": self, "listener": name})
            httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
            httpd.daemon_threads = True
            self._servers[name] = httpd
        self.port = self._servers["openai"].server_address[1]
        self.azure_port = self._servers["azure"].server_address[1]

    # lifecycle
    def start(self) -> "MockLLM":
        for httpd in self._servers.values():
            threading.Thread(target=httpd.serve_forever, daemon=True).start()
        return self

    def stop(self) -> None:
        for httpd in self._servers.values():
            httpd.shutdown()
            httpd.server_close()

    def next_id(self, prefix: str) -> str:
        # Provider-style ids: prefix + >=12 alphanumerics (sanitizer pattern).
        return f"{prefix}mock{next(self._ids):020d}"

    # scripting
    def script(
        self,
        responder: Responder,
        method: str = "POST",
        path: str = "/v1/responses",
        match: Optional[Callable[[Recorded], bool]] = None,
        times: int = 1,
    ) -> None:
        with self.lock:
            self._scripts.append(_Script(method, path, match, responder, times))

    def script_chat(self, chat_id: str, responder: Responder, times: int = 1) -> None:
        """Script the next streamed chat turn(s) of ``chat_id``."""
        self.script(
            responder,
            match=lambda r: r.chat_id == chat_id and r.request_type == "chat",
            times=times,
        )

    def take_script(self, rec: Recorded) -> Optional[Responder]:
        with self.lock:
            for s in self._scripts:
                if s.method == rec.method and rec.path.startswith(s.path_prefix) and (s.match is None or s.match(rec)):
                    s.times -= 1
                    if s.times <= 0:
                        self._scripts.remove(s)
                    return s.responder
        return None

    def reset_scripts(self) -> None:
        with self.lock:
            self._scripts.clear()
        self.vs_file_status = "completed"

    # queries
    def find(self, method: Optional[str] = None, path: Optional[str] = None, prefix: Optional[str] = None) -> list[Recorded]:
        with self.lock:
            out = list(self.requests)
        return [
            r
            for r in out
            if (method is None or r.method == method)
            and (path is None or r.path == path)
            and (prefix is None or r.path.startswith(prefix))
        ]

    def chat_requests(self, chat_id: str, request_type: str = "chat") -> list[Recorded]:
        return [
            r
            for r in self.find("POST", "/v1/responses")
            if r.chat_id == chat_id and r.request_type == request_type
        ]

    def wait_for(self, pred: Callable[[], Any], timeout: float = 10.0, interval: float = 0.1) -> Any:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            v = pred()
            if v:
                return v
            time.sleep(interval)
        return pred()

    # default behaviour
    def default_responder(self, h: _Handler, req: Recorded) -> None:
        p, m = req.path, req.method
        if m == "POST" and p == "/v1/responses":
            if isinstance(req.json, dict) and req.json.get("stream"):
                text_stream()(h, req)
            else:
                h.send_json(
                    200,
                    {
                        "id": self.next_id("resp_"),
                        "object": "response",
                        "status": "completed",
                        "output": [
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [
                                    {
                                        "type": "output_text",
                                        "text": f"<analysis>mock</analysis><summary>{SUMMARY_TEXT}</summary>",
                                    }
                                ],
                            }
                        ],
                        "usage": {"input_tokens": 100, "output_tokens": 20},
                    },
                )
        elif m == "POST" and p == "/v1/files":
            h.send_json(200, {"id": self.next_id("file-"), "object": "file", "purpose": "assistants"})
        elif m == "POST" and p == "/v1/vector_stores":
            h.send_json(200, {"id": self.next_id("vs_"), "object": "vector_store"})
        elif m == "POST" and p.startswith("/v1/vector_stores/") and p.endswith("/files"):
            fid = (req.json or {}).get("file_id", "")
            h.send_json(200, {"id": fid, "object": "vector_store.file", "status": self.vs_file_status})
        elif m == "GET" and p.startswith("/v1/vector_stores/") and "/files/" in p:
            h.send_json(200, {"id": p.rsplit("/", 1)[1], "status": self.vs_file_status})
        elif m == "DELETE" and p.startswith("/v1/files/"):
            h.send_json(200, {"id": p.rsplit("/", 1)[1], "object": "file", "deleted": True})
        elif m == "DELETE" and p.startswith("/v1/vector_stores/"):
            h.send_json(200, {"id": p.rsplit("/", 1)[1], "object": "vector_store.deleted", "deleted": True})
        else:
            h.send_json(404, {"error": {"message": f"mock: no route for {m} {p}"}})
