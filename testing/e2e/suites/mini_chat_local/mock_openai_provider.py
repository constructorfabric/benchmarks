"""Scriptable mock of an OpenAI-compatible provider (Responses, Files and Vector
Stores APIs) for the mini-chat black-box suite.

The mock is an ``aiohttp`` server running in a background thread on a free
loopback port.  It

* **records every request** it receives (method, path, query, headers, raw
  body, parsed JSON body or parsed multipart parts) so tests can assert what
  mini-chat sent through OAGW (API-key header, model, tools, metadata, ...);
* answers with sensible defaults (a short streamed completion, successful file
  and vector-store calls);
* can be **scripted per test**: queue one-shot responses per route, install a
  sticky default, or compute a response from the recorded request.

Typical use::

    provider.script("POST", "/v1/responses", sse_text(["Hel", "lo"], input_tokens=12, output_tokens=2))
    ...
    sent = provider.requests_to("POST", "/v1/responses")[-1]
    assert sent.json["model"] == "gpt-mock-premium"

Routes are served both under ``/v1/...`` (OpenAI) and ``/openai/v1/...`` (Azure
OpenAI); recorded paths are the paths exactly as received.

Streaming scenarios (slice S3) are scripted with the same primitives:

* ``sse_text(chunks, pause_after=1)`` streams the first delta, then holds the
  connection open until ``provider.release()`` (``provider.wait_paused()``
  returns once the mock sits on the pause, so a test can prove that the client
  already received the first delta while the provider is still busy);
* ``inserts=[(n, tool_events...)]`` places tool events after the n-th delta
  (``file_search_events`` / ``web_search_events`` / ``code_interpreter_events``);
* ``annotations=[web_annotation(...), file_annotation(...)]`` fill the final
  response object (the only place the gear reads citations from);
* ``terminal=None`` ends the stream without a terminal event,
  ``terminal="response.incomplete"`` sends a truncated completion;
* ``failed_stream_events`` / ``error_event_stream`` / ``error_response`` script
  ``response.failed``, the ``error`` SSE event and HTTP errors;
* ``delay=6.5`` holds the answer back (one ``ping`` is observable with
  ``streaming.sse_ping_interval_seconds = 5``);
* ``client_disconnected`` on a recorded request tells that mini-chat closed the
  provider connection (the hard cancel of a client disconnect).

Everything here is generic: later slices (quota, attachments, thread summary)
add tests, not infrastructure.
"""

from __future__ import annotations

import asyncio
import contextlib
import itertools
import json
import threading
import time
from dataclasses import dataclass, field
from email import policy as email_policy
from email.parser import BytesParser
from typing import Any, Callable, Dict, List, Optional, Tuple
from urllib.parse import parse_qs

from aiohttp import web

# --------------------------------------------------------------------------- #
# Recorded requests
# --------------------------------------------------------------------------- #

MAX_RECORDED_PART_BYTES = 256 * 1024


@dataclass
class RecordedRequest:
    """One request as received by the mock provider."""

    method: str
    path: str
    query: Dict[str, List[str]]
    headers: Dict[str, str]  # lower-cased names
    body: bytes
    json: Any = None  # parsed JSON body (``None`` when the body is not JSON)
    parts: Optional[List[Dict[str, Any]]] = None  # multipart: name/filename/content_type/size/data
    received_at: float = 0.0
    route: str = ""  # normalised route key, e.g. "POST /v1/responses"
    client_disconnected: bool = False  # set when the peer went away while streaming
    status: Optional[int] = None  # status the mock answered with
    paused: bool = False  # currently sitting on a scripted ``pause_after``

    def header(self, name: str, default: Optional[str] = None) -> Optional[str]:
        return self.headers.get(name.lower(), default)

    # ---- views of a chat request body (``POST /v1/responses``) --------------- #

    def _items(self) -> List[Dict[str, Any]]:
        body = self.json if isinstance(self.json, dict) else {}
        value = body.get("input")
        if isinstance(value, list):
            return [i for i in value if isinstance(i, dict)]
        if isinstance(value, str):
            return [{"role": "user", "content": value}]
        return []

    @staticmethod
    def _item_text(item: Dict[str, Any]) -> str:
        content = item.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            return "".join(p.get("text", "") for p in content
                           if isinstance(p, dict) and isinstance(p.get("text"), str))
        return ""

    def system_text(self) -> str:
        """System prompt text: ``instructions`` plus every ``system`` / ``developer`` input
        item (an adapter may use either representation)."""
        body = self.json if isinstance(self.json, dict) else {}
        parts: List[str] = []
        if isinstance(body.get("instructions"), str):
            parts.append(body["instructions"])
        parts += [self._item_text(i) for i in self._items()
                  if i.get("role") in ("system", "developer")]
        return "\n".join(parts)

    def conversation(self) -> List[Tuple[str, str]]:
        """``(role, text)`` of every user / assistant input item, in order."""
        return [(i["role"], self._item_text(i)) for i in self._items()
                if i.get("role") in ("user", "assistant")]

    def conversation_texts(self) -> List[str]:
        return [f"{role}:{text}" for role, text in self.conversation()]

    def tools(self) -> List[Dict[str, Any]]:
        body = self.json if isinstance(self.json, dict) else {}
        tools = body.get("tools")
        return [t for t in tools if isinstance(t, dict)] if isinstance(tools, list) else []

    def tool_types(self) -> List[str]:
        return [str(t.get("type")) for t in self.tools()]

    def tool(self, kind: str) -> Optional[Dict[str, Any]]:
        return next((t for t in self.tools() if t.get("type") == kind), None)

    def part(self, name: str) -> Optional[Dict[str, Any]]:
        for part in self.parts or []:
            if part.get("name") == name:
                return part
        return None

    # ---- thread summary requests (slice S7) -------------------------------- #

    @property
    def metadata(self) -> Dict[str, Any]:
        body = self.json if isinstance(self.json, dict) else {}
        value = body.get("metadata")
        return value if isinstance(value, dict) else {}

    @property
    def is_summary(self) -> bool:
        """A thread summary request: ``metadata.request_type == "summary"`` (DESIGN 4 "Provider
        Request Metadata"); every other ``POST /v1/responses`` is a chat request."""
        return self.route == "POST /v1/responses" and self.metadata.get("request_type") == "summary"

    def prompt(self) -> str:
        """The summary prompt: the text of the single user input item."""
        items = self.conversation()
        assert len(items) == 1 and items[0][0] == "user", \
            f"a summary request has exactly one user item: {items}"
        return items[0][1]


# --------------------------------------------------------------------------- #
# Scripted responses
# --------------------------------------------------------------------------- #


@dataclass
class ProviderResponse:
    """What the mock answers for one request.

    Exactly one of ``sse_events`` / ``sse_frames`` / ``json_body`` / ``raw_body`` is used
    (in that order); with none of them the route's default body is served.
    """

    status: int = 200
    json_body: Any = None
    sse_events: Optional[List[Tuple[str, Any]]] = None  # (event name, data object|str)
    raw_body: Optional[bytes] = None
    headers: Dict[str, str] = field(default_factory=dict)
    delay: float = 0.0  # seconds before the response starts
    event_delay: float = 0.0  # seconds between SSE events
    hold_open: bool = False  # keep an SSE stream open after the last event until released
    pause_after: Optional[int] = None  # hold the stream after this many events until released
    sse_frames: Optional[List[str]] = None  # raw SSE frames (no ``event:`` line, junk, ...)
    drop_connection: bool = False  # close the connection without a response


Handler = Callable[[RecordedRequest], Optional[ProviderResponse]]


def _is_non_streaming(rec: "RecordedRequest") -> bool:
    """``True`` when the request body asks for a plain answer (``"stream": false``)."""
    return isinstance(rec.json, dict) and rec.json.get("stream") is False


def _sse_as_json(response: ProviderResponse) -> ProviderResponse:
    """The non-streaming twin of a scripted event sequence: the ``response`` object of its
    terminal event (``response.completed`` / ``response.incomplete`` / ``response.failed``), or a
    failed response when the sequence has no terminal event. A pause of the script holds the
    answer until ``release()``."""
    terminal = None
    deltas: List[str] = []
    for name, data in response.sse_events or []:
        if name == "response.output_text.delta" and isinstance(data, dict):
            deltas.append(str(data.get("delta", "")))
        elif name in ("response.completed", "response.incomplete", "response.failed") \
                and isinstance(data, dict) and isinstance(data.get("response"), dict):
            terminal = data["response"]
    if terminal is None:
        terminal = _response_object("resp_mock_no_terminal", "mock-model", "failed",
                                    "".join(deltas))
        terminal["error"] = {"code": "server_error", "message": "mock answer without a terminal"}
    return ProviderResponse(
        status=response.status, json_body=terminal, headers=dict(response.headers),
        delay=0.0, hold_open=response.hold_open or response.pause_after is not None)


def json_response(body: Any, status: int = 200, **kwargs: Any) -> ProviderResponse:
    return ProviderResponse(status=status, json_body=body, **kwargs)


def error_response(status: int, message: str = "mock provider error", code: str = "mock_error",
                   **kwargs: Any) -> ProviderResponse:
    """An OpenAI-style error body: ``{"error": {"message", "type", "code"}}``."""
    return ProviderResponse(
        status=status,
        json_body={"error": {"message": message, "type": "invalid_request_error", "code": code}},
        **kwargs,
    )


def usage_object(input_tokens: int, output_tokens: int, cached_tokens: int = 0,
                 reasoning_tokens: int = 0) -> Dict[str, Any]:
    return {
        "input_tokens": input_tokens,
        "input_tokens_details": {"cached_tokens": cached_tokens},
        "output_tokens": output_tokens,
        "output_tokens_details": {"reasoning_tokens": reasoning_tokens},
        "total_tokens": input_tokens + output_tokens,
    }


def _response_object(response_id: str, model: str, status: str, text: str = "",
                     usage: Optional[Dict[str, Any]] = None,
                     annotations: Optional[List[Dict[str, Any]]] = None) -> Dict[str, Any]:
    obj: Dict[str, Any] = {
        "id": response_id,
        "object": "response",
        "created_at": int(time.time()),
        "status": status,
        "model": model,
        "output": [],
    }
    if text or status == "completed":
        obj["output"] = [
            {
                "id": "msg_" + response_id,
                "type": "message",
                "status": "completed" if status == "completed" else "in_progress",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": text, "annotations": annotations or []}
                ],
            }
        ]
    if usage is not None:
        obj["usage"] = usage
    return obj


def text_stream_events(chunks: List[str], model: str = "mock-model",
                       response_id: str = "resp_mock_1",
                       input_tokens: int = 10, output_tokens: Optional[int] = None,
                       terminal: Optional[str] = "response.completed",
                       annotations: Optional[List[Dict[str, Any]]] = None,
                       inserts: Optional[List[Tuple[int, List[Tuple[str, Any]]]]] = None,
                       cached_tokens: int = 0, reasoning_tokens: int = 0
                       ) -> List[Tuple[str, Any]]:
    """The Responses-API SSE sequence of a plain text answer.

    ``response.created`` -> ``response.in_progress`` -> one
    ``response.output_text.delta`` per chunk -> ``response.output_text.done`` ->
    ``response.completed`` (or ``terminal``, e.g. ``response.incomplete``; ``None`` sends no
    terminal event). ``inserts`` places extra events (tool events, noise) after the n-th delta
    (``0`` = before the first one). ``annotations`` go on the ``output_text`` part of the final
    response object. ``cached_tokens`` / ``reasoning_tokens`` fill
    ``usage.input_tokens_details.cached_tokens`` / ``usage.output_tokens_details.reasoning_tokens``.
    """
    text = "".join(chunks)
    if output_tokens is None:
        output_tokens = max(1, len(chunks))
    seq = itertools.count(0)
    item_id = "msg_" + response_id
    events: List[Tuple[str, Any]] = [
        ("response.created", {
            "type": "response.created", "sequence_number": next(seq),
            "response": _response_object(response_id, model, "in_progress"),
        }),
        ("response.in_progress", {
            "type": "response.in_progress", "sequence_number": next(seq),
            "response": _response_object(response_id, model, "in_progress"),
        }),
    ]

    def extra(after: int) -> List[Tuple[str, Any]]:
        return [e for n, group in (inserts or []) if n == after for e in group]

    events.extend(extra(0))
    for index, chunk in enumerate(chunks, start=1):
        events.append(("response.output_text.delta", {
            "type": "response.output_text.delta", "sequence_number": next(seq),
            "item_id": item_id, "output_index": 0, "content_index": 0, "delta": chunk,
        }))
        events.extend(extra(index))
    events.append(("response.output_text.done", {
        "type": "response.output_text.done", "sequence_number": next(seq),
        "item_id": item_id, "output_index": 0, "content_index": 0, "text": text,
    }))
    if terminal is not None:
        final_status = "completed" if terminal == "response.completed" else "incomplete"
        events.append((terminal, {
            "type": terminal, "sequence_number": next(seq),
            "response": _response_object(
                response_id, model, final_status, text,
                usage_object(input_tokens, output_tokens, cached_tokens, reasoning_tokens),
                annotations),
        }))
    return events


def failed_stream_events(code: str = "server_error", message: str = "mock provider failure",
                         response_id: str = "resp_mock_failed", model: str = "mock-model",
                         partial_chunks: Optional[List[str]] = None,
                         usage: Optional[Tuple[int, int]] = None,
                         cached_tokens: int = 0, reasoning_tokens: int = 0
                         ) -> List[Tuple[str, Any]]:
    """A stream that ends with ``response.failed`` (optionally after some deltas)."""
    events = text_stream_events(partial_chunks or [], model=model, response_id=response_id,
                                terminal=None)
    events = [e for e in events if e[0] != "response.output_text.done"]
    failed = _response_object(response_id, model, "failed")
    failed["error"] = {"code": code, "message": message}
    if usage is not None:
        failed["usage"] = usage_object(usage[0], usage[1], cached_tokens, reasoning_tokens)
    events.append(("response.failed", {"type": "response.failed", "response": failed}))
    return events


def error_event_stream(data: Any, name: str = "error") -> List[Tuple[str, Any]]:
    """A stream that is just one ``error`` SSE event (``data`` is a dict or raw text)."""
    return [(name, data)]


# ---- tool events and annotations -------------------------------------------- #


def file_search_events(results: Optional[int] = None) -> List[Tuple[str, Any]]:
    """``in_progress`` / ``searching`` / ``completed`` of a ``file_search`` call. The real API
    sends no ``results``; pass a count to exercise ``files_searched``."""
    done: Dict[str, Any] = {"type": "response.file_search_call.completed", "item_id": "fs_1"}
    if results is not None:
        done["results"] = [{"file_id": f"f{i}"} for i in range(results)]
    return [
        ("response.file_search_call.in_progress",
         {"type": "response.file_search_call.in_progress", "item_id": "fs_1"}),
        ("response.file_search_call.searching",
         {"type": "response.file_search_call.searching", "item_id": "fs_1"}),
        ("response.file_search_call.completed", done),
    ]


def web_search_events() -> List[Tuple[str, Any]]:
    return [
        ("response.web_search_call.in_progress",
         {"type": "response.web_search_call.in_progress", "item_id": "ws_1"}),
        ("response.web_search_call.searching",
         {"type": "response.web_search_call.searching", "item_id": "ws_1"}),
        ("response.web_search_call.completed",
         {"type": "response.web_search_call.completed", "item_id": "ws_1"}),
    ]


def code_interpreter_events(logs: str) -> List[Tuple[str, Any]]:
    """``in_progress`` starts the tool; ``interpreting`` / ``completed`` are provider noise;
    ``output_item.done`` carries the logs."""
    return [
        ("response.code_interpreter_call.in_progress",
         {"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"}),
        ("response.code_interpreter_call.interpreting",
         {"type": "response.code_interpreter_call.interpreting", "item_id": "ci_1"}),
        ("response.code_interpreter_call.completed",
         {"type": "response.code_interpreter_call.completed", "item_id": "ci_1"}),
        ("response.output_item.done", {
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "ci_1", "type": "code_interpreter_call", "status": "completed",
                     "code": "print(1)", "outputs": [{"type": "logs", "logs": logs}]}}),
    ]


def noise_events() -> List[Tuple[str, Any]]:
    """Provider events that must not produce any client event."""
    return [
        ("response.content_part.added", {"type": "response.content_part.added"}),
        ("response.output_item.added", {"type": "response.output_item.added"}),
        ("response.reasoning_summary_text.delta",
         {"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
        ("response.content_part.done", {"type": "response.content_part.done"}),
        ("response.some_future_event", {"type": "response.some_future_event"}),
    ]


def web_annotation(url: str, title: str, start: Optional[int] = None,
                   end: Optional[int] = None) -> Dict[str, Any]:
    annotation: Dict[str, Any] = {"type": "url_citation", "url": url, "title": title}
    if start is not None and end is not None:
        annotation.update(start_index=start, end_index=end)
    return annotation


def file_annotation(file_id: str, filename: str) -> Dict[str, Any]:
    return {"type": "file_citation", "file_id": file_id, "filename": filename, "index": 0}


def sse_text(chunks: List[str], **kwargs: Any) -> ProviderResponse:
    """Scripted ``POST /v1/responses`` answer streaming ``chunks`` as text deltas.

    ``pause_after_deltas=n`` holds the stream right after the n-th delta (``0`` = before the
    first one) until ``provider.release()``. Other keyword arguments: ``text_stream_events``
    (``model``, ``response_id``, ``input_tokens``, ``output_tokens``, ``terminal``,
    ``annotations``, ``inserts``, ``cached_tokens``, ``reasoning_tokens``) and the ``ProviderResponse`` fields ``status``, ``headers``,
    ``delay``, ``event_delay``, ``hold_open``, ``pause_after``.
    """
    response_kwargs = {k: kwargs.pop(k) for k in
                       ("status", "headers", "delay", "event_delay", "hold_open", "pause_after")
                       if k in kwargs}
    pause_after_deltas = kwargs.pop("pause_after_deltas", None)
    events = text_stream_events(chunks, **kwargs)
    if pause_after_deltas is not None:
        if pause_after_deltas == 0:
            response_kwargs["pause_after"] = 2  # after response.created / response.in_progress
        else:
            seen = 0
            for index, (name, _data) in enumerate(events):
                if name == "response.output_text.delta":
                    seen += 1
                    if seen == pause_after_deltas:
                        response_kwargs["pause_after"] = index + 1
                        break
    return ProviderResponse(sse_events=events, **response_kwargs)


def sse_summary(summary: str, **kwargs: Any) -> ProviderResponse:
    """A summary answer in the requested format: an ``<analysis>`` block and the ``<summary>``
    block holding ``summary`` (streamed in three deltas). Keyword arguments as ``sse_text``."""
    return sse_text(["<analysis>reviewing the exchange</analysis>\n<summary>\n", summary,
                     "\n</summary>"], **kwargs)


def sse_frames(*frames: str, **kwargs: Any) -> ProviderResponse:
    """Scripted answer made of raw SSE frames (``"data: {...}\\n\\n"``): nameless events,
    ``event: message`` frames, unparseable data."""
    return ProviderResponse(sse_frames=list(frames), **kwargs)


def nameless_frames(events: List[Tuple[str, Any]], line: Optional[str] = None) -> List[str]:
    """Render named events as frames without an ``event:`` line (or with ``event: <line>``)."""
    out = []
    for _name, data in events:
        payload = data if isinstance(data, str) else json.dumps(data)
        head = f"event: {line}\n" if line else ""
        out.append(f"{head}data: {payload}\n\n")
    return out


# ---- Files API / Vector Stores API answers (slice S6) ------------------------ #

FILES = ("POST", "/v1/files")
FILE = ("DELETE", "/v1/files/{file_id}")
VECTOR_STORES = ("POST", "/v1/vector_stores")
VECTOR_STORE = ("DELETE", "/v1/vector_stores/{vs_id}")
VS_FILES = ("POST", "/v1/vector_stores/{vs_id}/files")
VS_FILE = ("GET", "/v1/vector_stores/{vs_id}/files/{file_id}")


def file_object(file_id: str, filename: str = "upload", size: int = 0,
                purpose: str = "assistants") -> Dict[str, Any]:
    """The Files API answer of a stored file."""
    return {"id": file_id, "object": "file", "bytes": size, "created_at": int(time.time()),
            "filename": filename, "purpose": purpose, "status": "processed"}


def vs_file_object(status: str, file_id: str = "file-mock", vector_store_id: str = "vs_mock",
                   last_error: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """A ``vector_store.file``: ``status`` is ``in_progress`` / ``completed`` / ``failed`` /
    ``cancelled`` (any other value is passed through, e.g. ``expired``)."""
    return {"id": file_id, "object": "vector_store.file", "status": status,
            "vector_store_id": vector_store_id, "last_error": last_error}


def vs_file_response(status: str, **kwargs: Any) -> ProviderResponse:
    return json_response(vs_file_object(status, **kwargs))


def vs_file_without_status() -> ProviderResponse:
    """A poll answer without ``status`` (counts as ``in_progress``)."""
    return json_response({"id": "file-mock", "object": "vector_store.file"})


# --------------------------------------------------------------------------- #
# The server
# --------------------------------------------------------------------------- #

SUMMARY_SUFFIX = "#summary"
SUMMARY_ROUTE = ("POST", "/v1/responses" + SUMMARY_SUFFIX)
DEFAULT_SUMMARY_TEXT = "Mock thread summary."

_ROUTE_TABLE = [
    ("POST", "/v1/responses"),
    ("POST", "/v1/files"),
    ("DELETE", "/v1/files/{file_id}"),
    ("POST", "/v1/vector_stores"),
    ("POST", "/v1/vector_stores/{vs_id}/files"),
    ("GET", "/v1/vector_stores/{vs_id}/files/{file_id}"),
    ("DELETE", "/v1/vector_stores/{vs_id}/files/{file_id}"),
    ("DELETE", "/v1/vector_stores/{vs_id}"),
]


class MockOpenAIProvider:
    """aiohttp-based mock provider; see the module docstring."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0, model: str = "mock-model") -> None:
        self.host = host
        self.port = port
        self.default_model = model
        self._lock = threading.Lock()
        self._requests: List[RecordedRequest] = []
        self._queues: Dict[str, List[ProviderResponse]] = {}
        self._sticky: Dict[str, ProviderResponse] = {}
        self._handlers: Dict[str, Handler] = {}
        self._counters: Dict[str, int] = {}
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None
        self._runner: Optional[web.AppRunner] = None
        self._ready = threading.Event()
        self._start_error: Optional[BaseException] = None
        self._release = threading.Event()

    # ---- lifecycle --------------------------------------------------------- #

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def start(self, timeout: float = 10.0) -> "MockOpenAIProvider":
        self._thread = threading.Thread(target=self._run, name="mock-openai-provider", daemon=True)
        self._thread.start()
        if not self._ready.wait(timeout):
            raise RuntimeError("mock provider did not start in time")
        if self._start_error is not None:
            raise RuntimeError(f"mock provider failed to start: {self._start_error}")
        return self

    def stop(self) -> None:
        self._release.set()
        loop, runner = self._loop, self._runner
        if loop is not None and runner is not None and loop.is_running():
            fut = asyncio.run_coroutine_threadsafe(runner.cleanup(), loop)
            try:
                fut.result(timeout=5)
            except Exception:  # noqa: BLE001 - best effort shutdown
                pass
            loop.call_soon_threadsafe(loop.stop)
        if self._thread is not None:
            self._thread.join(timeout=5)

    @contextlib.contextmanager
    def unavailable(self):
        """Stop listening (connections to the port are refused) for the duration of the
        block, then listen again on the same port."""
        port = self.port
        self.stop()
        try:
            yield
        finally:
            self._ready.clear()
            self._start_error = None
            self._loop = None
            self._runner = None
            self._thread = None
            self._release.clear()
            self.port = port
            self.start()

    def __enter__(self) -> "MockOpenAIProvider":
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()

    def _run(self) -> None:
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        self._loop = loop
        try:
            app = web.Application(client_max_size=64 * 1024 * 1024)
            app.router.add_get("/__mock/health", self._health)
            for prefix in ("", "/openai"):
                for method, template in _ROUTE_TABLE:
                    app.router.add_route(method, prefix + template, self._dispatch)
            app.router.add_route("*", "/{tail:.*}", self._dispatch)  # recorded 404s
            self._runner = web.AppRunner(app, access_log=None)
            loop.run_until_complete(self._runner.setup())
            site = web.TCPSite(self._runner, self.host, self.port)
            loop.run_until_complete(site.start())
            sockets = getattr(site._server, "sockets", None) or []  # noqa: SLF001
            if sockets:
                self.port = sockets[0].getsockname()[1]
        except BaseException as exc:  # noqa: BLE001
            self._start_error = exc
            self._ready.set()
            return
        self._ready.set()
        try:
            loop.run_forever()
        finally:
            loop.close()

    # ---- test-facing API --------------------------------------------------- #

    def reset(self) -> None:
        """Forget recorded requests, queued scripts, sticky defaults and counters."""
        with self._lock:
            self._requests.clear()
            self._queues.clear()
            self._sticky.clear()
            self._handlers.clear()
            self._counters.clear()
        self._release.clear()

    @property
    def requests(self) -> List[RecordedRequest]:
        with self._lock:
            return list(self._requests)

    def requests_to(self, method: str, path: str) -> List[RecordedRequest]:
        """Requests whose route matches ``method path`` (path may use ``{var}`` templates
        from the route table, or be the exact received path)."""
        method = method.upper()
        out = []
        for r in self.requests:
            if r.method != method:
                continue
            if r.path == path or r.route == f"{method} {path}" or r.path == "/openai" + path:
                out.append(r)
        return out

    def wait_for_request(self, method: str, path: str, count: int = 1,
                         timeout: float = 5.0) -> List[RecordedRequest]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = self.requests_to(method, path)
            if len(found) >= count:
                return found
            time.sleep(0.02)
        raise AssertionError(
            f"mock provider did not receive {count} x {method} {path} within {timeout}s; "
            f"saw {[(r.method, r.path) for r in self.requests]}")

    def script(self, method: str, path: str, *responses: ProviderResponse) -> None:
        """Queue one-shot responses for a route; they are served FIFO, then the default again."""
        with self._lock:
            self._queues.setdefault(self._key(method, path), []).extend(responses)

    def set_default(self, method: str, path: str, response: ProviderResponse) -> None:
        """Sticky response for every request of a route (until ``reset``)."""
        with self._lock:
            self._sticky[self._key(method, path)] = response

    def on(self, method: str, path: str, handler: Handler) -> None:
        """Compute the response from the recorded request; return ``None`` for the default."""
        with self._lock:
            self._handlers[self._key(method, path)] = handler

    def script_summary(self, *responses: ProviderResponse) -> None:
        """Queue one-shot answers for the thread summary requests (FIFO, then the default)."""
        self.script(*SUMMARY_ROUTE, *responses)

    def set_summary_default(self, response: ProviderResponse) -> None:
        """Sticky answer of every thread summary request (until ``reset``)."""
        self.set_default(*SUMMARY_ROUTE, response)

    def summary_requests(self) -> List[RecordedRequest]:
        """The thread summary requests received (``metadata.request_type == "summary"``)."""
        return [r for r in self.requests if r.is_summary]

    def chat_requests(self) -> List[RecordedRequest]:
        """The chat requests received (every ``POST /v1/responses`` that is not a summary)."""
        return [r for r in self.requests_to("POST", "/v1/responses") if not r.is_summary]

    def wait_for_summary_requests(self, count: int = 1, timeout: float = 15.0
                                  ) -> List[RecordedRequest]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = self.summary_requests()
            if len(found) >= count:
                return found
            time.sleep(0.02)
        raise AssertionError(f"the mock provider did not receive {count} summary request(s) "
                             f"within {timeout}s; saw {len(self.summary_requests())}")

    def release(self) -> None:
        """Let SSE streams scripted with ``hold_open=True`` / ``pause_after`` continue."""
        self._release.set()

    def paused_requests(self) -> List[RecordedRequest]:
        return [r for r in self.requests if r.paused]

    def wait_paused(self, count: int = 1, timeout: float = 5.0) -> List[RecordedRequest]:
        """Block until ``count`` streams sit on a scripted pause."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            paused = self.paused_requests()
            if len(paused) >= count:
                return paused
            time.sleep(0.02)
        raise AssertionError(f"mock provider has {len(self.paused_requests())} paused stream(s), "
                             f"expected {count}")

    def wait_client_disconnected(self, timeout: float = 5.0) -> RecordedRequest:
        """Block until mini-chat dropped a provider connection (hard cancel)."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for r in self.requests:
                if r.client_disconnected:
                    return r
            time.sleep(0.02)
        raise AssertionError("mini-chat never closed the provider connection")

    # ---- Files API / Vector Stores API helpers (slice S6) ------------------- #

    def file_uploads(self) -> List[RecordedRequest]:
        """``POST /v1/files`` requests (multipart: ``purpose`` and ``file`` parts)."""
        return self.requests_to(*FILES)

    def file_deletes(self) -> List[RecordedRequest]:
        return self.requests_to(*FILE)

    def deleted_file_ids(self) -> List[str]:
        """Provider file ids of the ``DELETE /v1/files/{id}`` requests, in order."""
        return [r.path.rsplit("/", 1)[-1] for r in self.file_deletes()]

    def vector_store_creates(self) -> List[RecordedRequest]:
        return self.requests_to(*VECTOR_STORES)

    def vector_store_adds(self) -> List[RecordedRequest]:
        return self.requests_to(*VS_FILES)

    def vector_store_polls(self) -> List[RecordedRequest]:
        return self.requests_to(*VS_FILE)

    def vector_store_deletes(self) -> List[RecordedRequest]:
        return self.requests_to(*VECTOR_STORE)

    def storage_requests(self) -> List[RecordedRequest]:
        """Every Files / Vector Stores request, in arrival order."""
        return [r for r in self.requests if "/files" in r.path or "/vector_stores" in r.path]

    def index_statuses(self, *statuses: str) -> None:
        """The next polls of a vector store file answer these statuses (FIFO, then ``completed``);
        the add call is made to answer ``in_progress`` so the upload polls."""
        self.set_default(*VS_FILES, vs_file_response("in_progress"))
        self.script(*VS_FILE, *[vs_file_response(status) for status in statuses])

    def index_forever(self) -> None:
        """Indexing never finishes: the add and every poll answer ``in_progress``."""
        self.set_default(*VS_FILES, vs_file_response("in_progress"))
        self.set_default(*VS_FILE, vs_file_response("in_progress"))

    def index_status_now(self, status: str) -> None:
        """Every later poll answers ``status`` (what a still running background wait sees)."""
        self.set_default(*VS_FILE, vs_file_response(status))

    def index_instantly(self) -> None:
        """The add call already answers ``completed`` (no poll is needed)."""
        self.set_default(*VS_FILES, vs_file_response("completed"))

    def fail(self, route: Tuple[str, str], status: int = 500, message: str = "mock provider error",
             times: int = 1) -> None:
        """The next ``times`` requests of ``route`` answer an HTTP error."""
        self.script(*route, *[error_response(status, message) for _ in range(times)])

    def fail_always(self, route: Tuple[str, str], status: int = 500,
                    message: str = "mock provider error") -> None:
        self.set_default(*route, error_response(status, message))

    # ---- internals --------------------------------------------------------- #

    @staticmethod
    def _normalise(path: str) -> str:
        if path.startswith("/openai/"):
            path = path[len("/openai"):]
        return path

    def _key(self, method: str, path: str) -> str:
        return f"{method.upper()} {self._normalise(path)}"

    def _route_key(self, method: str, path: str) -> str:
        path = self._normalise(path)
        parts = path.strip("/").split("/")
        # /v1/files/{id}
        if len(parts) == 3 and parts[:2] == ["v1", "files"]:
            return f"{method} /v1/files/{{file_id}}"
        if len(parts) >= 3 and parts[:2] == ["v1", "vector_stores"]:
            if len(parts) == 3:
                return f"{method} /v1/vector_stores/{{vs_id}}"
            if len(parts) == 4 and parts[3] == "files":
                return f"{method} /v1/vector_stores/{{vs_id}}/files"
            if len(parts) == 5 and parts[3] == "files":
                return f"{method} /v1/vector_stores/{{vs_id}}/files/{{file_id}}"
        return f"{method} {path}"

    def _next(self, name: str) -> int:
        with self._lock:
            self._counters[name] = self._counters.get(name, 0) + 1
            return self._counters[name]

    async def _health(self, _request: web.Request) -> web.Response:
        return web.json_response({"status": "ok"})

    async def _record(self, request: web.Request) -> RecordedRequest:
        body = await request.read()
        ctype = request.headers.get("Content-Type", "")
        parsed_json: Any = None
        parts: Optional[List[Dict[str, Any]]] = None
        if "json" in ctype and body:
            try:
                parsed_json = json.loads(body)
            except ValueError:
                parsed_json = None
        elif ctype.startswith("multipart/"):
            parts = _parse_multipart(ctype, body)
        rec = RecordedRequest(
            method=request.method,
            path=request.path,
            query={k: v for k, v in parse_qs(request.query_string).items()},
            headers={k.lower(): v for k, v in request.headers.items()},
            body=body,
            json=parsed_json,
            parts=parts,
            received_at=time.time(),
            route=self._route_key(request.method, request.path),
        )
        with self._lock:
            self._requests.append(rec)
        return rec

    def _pick(self, rec: RecordedRequest) -> Optional[ProviderResponse]:
        # thread summary requests have their own script queue / sticky default / handler, so a
        # test scripts the summary answers apart from the chat answers (they interleave freely)
        key = rec.route + SUMMARY_SUFFIX if rec.is_summary else rec.route
        with self._lock:
            handler = self._handlers.get(key)
            queue = self._queues.get(key)
            sticky = self._sticky.get(key)
            queued = queue.pop(0) if queue else None
        if queued is not None:
            return queued
        if handler is not None:
            produced = handler(rec)
            if produced is not None:
                return produced
        return sticky

    async def _dispatch(self, request: web.Request) -> web.StreamResponse:
        rec = await self._record(request)
        scripted = self._pick(rec)
        if scripted is not None and scripted.delay:
            await asyncio.sleep(scripted.delay)
        if scripted is not None and scripted.drop_connection:
            rec.status = 0
            if request.transport is not None:
                request.transport.close()
            return web.Response(status=500)
        response = scripted if scripted is not None else self._default_response(rec)
        if response is None:
            rec.status = 404
            return web.json_response(
                {"error": {"message": f"no mock route for {rec.method} {rec.path}",
                           "type": "invalid_request_error", "code": "not_found"}},
                status=404)
        if (response.sse_events is not None and rec.is_summary and _is_non_streaming(rec)
                and 200 <= response.status < 300):
            # the summary call is non-streaming: a scripted event sequence (``sse_summary`` ...)
            # is served as the JSON body of the final response, like the real Responses API does
            response = _sse_as_json(response)
        rec.status = response.status
        if response.sse_events is not None or response.sse_frames is not None:
            return await self._stream(request, rec, response)
        if response.json_body is not None:
            if response.hold_open and not await self._hold(request, rec):
                return web.Response(status=499)
            return web.json_response(response.json_body, status=response.status,
                                     headers=response.headers)
        return web.Response(status=response.status, body=response.raw_body or b"",
                            headers=response.headers)

    async def _stream(self, request: web.Request, rec: RecordedRequest,
                      response: ProviderResponse) -> web.StreamResponse:
        headers = {"Content-Type": "text/event-stream", "Cache-Control": "no-cache",
                   **response.headers}
        resp = web.StreamResponse(status=response.status, headers=headers)
        await resp.prepare(request)
        if response.sse_frames is not None:
            frames = list(response.sse_frames)
        else:
            frames = [
                f"event: {name}\ndata: {data if isinstance(data, str) else json.dumps(data)}\n\n"
                for name, data in response.sse_events or []]
        try:
            for index, frame in enumerate(frames, start=1):
                await resp.write(frame.encode())
                if response.pause_after is not None and index == response.pause_after:
                    if not await self._hold(request, rec):
                        return resp
                if response.event_delay:
                    await asyncio.sleep(response.event_delay)
            if response.hold_open:
                if not await self._hold(request, rec):
                    return resp
            await resp.write_eof()
        except (ConnectionResetError, ConnectionError, asyncio.CancelledError):
            rec.client_disconnected = True
        return resp

    async def _hold(self, request: web.Request, rec: RecordedRequest) -> bool:
        """Wait until ``release()``; ``False`` when the peer went away meanwhile."""
        rec.paused = True
        try:
            while not self._release.is_set():
                await asyncio.sleep(0.05)
                if request.transport is None or request.transport.is_closing():
                    rec.client_disconnected = True
                    return False
            return True
        finally:
            rec.paused = False

    # ---- default behaviour -------------------------------------------------- #

    def _default_response(self, rec: RecordedRequest) -> Optional[ProviderResponse]:
        route = rec.route
        if route == "POST /v1/responses":
            body = rec.json if isinstance(rec.json, dict) else {}
            model = body.get("model") or self.default_model
            n = self._next("responses")
            response_id = f"resp_mock_{n}"
            if rec.is_summary:
                text = f"<analysis>mock analysis</analysis>\n<summary>{DEFAULT_SUMMARY_TEXT}</summary>"
                if body.get("stream"):
                    return ProviderResponse(sse_events=text_stream_events(
                        [text], model=model, response_id=response_id,
                        input_tokens=40, output_tokens=12))
                return ProviderResponse(json_body=_response_object(
                    response_id, model, "completed", text, usage_object(40, 12)))
            if body.get("stream"):
                return ProviderResponse(sse_events=text_stream_events(
                    ["Hello", " from", " the", " mock", " provider."],
                    model=model, response_id=response_id, input_tokens=12, output_tokens=5))
            return ProviderResponse(json_body=_response_object(
                response_id, model, "completed", "Mock summary.", usage_object(12, 5)))
        if route == "POST /v1/files":
            n = self._next("files")
            part = rec.part("file") or {}
            return ProviderResponse(json_body={
                "id": f"file-mock-{n}", "object": "file", "bytes": part.get("size", 0),
                "created_at": int(time.time()), "filename": part.get("filename") or "upload",
                "purpose": (rec.part("purpose") or {}).get("text", "assistants"),
                "status": "processed"})
        if route == "DELETE /v1/files/{file_id}":
            return ProviderResponse(json_body={
                "id": rec.path.rsplit("/", 1)[-1], "object": "file", "deleted": True})
        if route == "POST /v1/vector_stores":
            n = self._next("vector_stores")
            return ProviderResponse(json_body={
                "id": f"vs_mock_{n}", "object": "vector_store", "status": "completed",
                "name": (rec.json or {}).get("name") if isinstance(rec.json, dict) else None,
                "file_counts": {"in_progress": 0, "completed": 0, "failed": 0,
                                "cancelled": 0, "total": 0}})
        if route == "POST /v1/vector_stores/{vs_id}/files":
            body = rec.json if isinstance(rec.json, dict) else {}
            return ProviderResponse(json_body={
                "id": body.get("file_id", "file-mock"), "object": "vector_store.file",
                "status": "in_progress", "vector_store_id": rec.path.split("/")[-2]
                if rec.path.endswith("/files") else ""})
        if route == "GET /v1/vector_stores/{vs_id}/files/{file_id}":
            segments = rec.path.strip("/").split("/")
            return ProviderResponse(json_body={
                "id": segments[-1], "object": "vector_store.file", "status": "completed",
                "vector_store_id": segments[-3]})
        if route == "DELETE /v1/vector_stores/{vs_id}/files/{file_id}":
            return ProviderResponse(json_body={
                "id": rec.path.rsplit("/", 1)[-1], "object": "vector_store.file.deleted",
                "deleted": True})
        if route == "DELETE /v1/vector_stores/{vs_id}":
            return ProviderResponse(json_body={
                "id": rec.path.rsplit("/", 1)[-1], "object": "vector_store.deleted",
                "deleted": True})
        return None


def _parse_multipart(content_type: str, body: bytes) -> List[Dict[str, Any]]:
    """Parse a multipart body into ``[{name, filename, content_type, size, data, text}]``."""
    raw = b"Content-Type: " + content_type.encode("latin-1") + b"\r\nMIME-Version: 1.0\r\n\r\n" + body
    parts: List[Dict[str, Any]] = []
    try:
        message = BytesParser(policy=email_policy.HTTP).parsebytes(raw)
        for part in message.iter_parts():
            data = part.get_payload(decode=True) or b""
            entry: Dict[str, Any] = {
                "name": part.get_param("name", header="content-disposition"),
                "filename": part.get_filename(),
                "content_type": part.get("Content-Type"),
                "size": len(data),
                "data": bytes(data[:MAX_RECORDED_PART_BYTES]),
            }
            try:
                entry["text"] = bytes(data[:4096]).decode("utf-8")
            except UnicodeDecodeError:
                entry["text"] = None
            parts.append(entry)
    except Exception:  # noqa: BLE001 - recording must never break the mock
        return parts
    return parts
