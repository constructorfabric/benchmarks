"""OpenAI-compatible mock LLM / RAG provider for the mini-chat E2E suite.

The mock is an aiohttp server that runs on its own event loop in a background
thread, so synchronous pytest code can drive it. It serves every upstream API
the mini-chat gear talks to through OAGW:

* OpenAI Responses API (``POST .../responses``), streaming and non-streaming;
* OpenAI Chat Completions (``POST .../chat/completions``);
* Anthropic Messages (``POST .../messages``);
* Files API (``POST .../files``, ``GET|DELETE .../files/{id}``);
* Vector stores (create, add file, file status poll, search, delete).

Paths are matched by suffix, so any prefix works (``/v1/responses``,
``/openai/v1/responses``, an OAGW alias prefix, ...). The query string (for
example Azure's ``api-version``) is ignored for matching and recorded.

Every request is recorded (method, path, query, headers, JSON body or
multipart field metadata). Tests inspect them with :meth:`MockProvider.requests`
or over HTTP via ``GET /__mock/requests``.

Streaming ``/responses`` calls consume scenarios from a FIFO queue
(:meth:`MockProvider.enqueue`, ``POST /__mock/script``); when the queue is
empty the default scenario is used. Non-streaming calls (the thread-summary
request) consume a separate queue (:meth:`MockProvider.enqueue_summary`), so a
background summary call never steals a scenario meant for a chat turn.

Scenario keys (all optional):

``text`` / ``chunks``
    Output text, either as one string (split into a few deltas) or as an
    explicit list of deltas.
``delay_ms``
    Delay before every event after ``response.created``.
``initial_delay_ms``
    Delay before the response headers are sent.
``usage``
    Usage override (``input_tokens``, ``output_tokens``, ``cached_tokens``,
    ``reasoning_tokens`` or the nested OpenAI shape). ``None`` omits usage.
``kind``
    ``completed`` (default) | ``incomplete`` | ``failed`` | ``sse_error`` |
    ``http_error`` | ``hang`` | ``drop``.
``incomplete_reason``
    Reason of ``response.incomplete`` (default ``max_output_tokens``).
``error_code`` / ``error_message``
    Error of ``failed``, ``sse_error`` and ``http_error`` scenarios. The
    default message embeds provider identifiers, an API key and a URL so
    sanitization can be asserted.
``status`` / ``retry_after``
    HTTP status (default 500) and optional ``Retry-After`` header of an
    ``http_error`` scenario.
``tool_events``
    Tool calls emitted before the text, e.g. ``{"type": "web_search",
    "count": 2}``, ``{"type": "file_search", "results": 3}``,
    ``{"type": "code_interpreter", "logs": "42"}``.
``annotations``
    Annotations attached to the output text (``url_citation`` with
    ``url``/``title``/``start_index``/``end_index``; ``file_citation`` with
    ``file_id``/``filename``/``index``).
``hang_max_secs``
    Upper bound for a ``hang`` scenario (default 600 s).
``function_calls``
    Client (function) tool calls emitted after the tool events, e.g.
    ``[{"name": "search_knowledge", "arguments": {"query": "q"}, "call_id":
    "call_1"}]`` (``arguments`` may be a dict or a JSON string; ``call_id``
    defaults to a random id). Responses: ``function_call`` output items;
    Chat Completions: ``tool_calls`` deltas with ``finish_reason =
    "tool_calls"``; Anthropic: ``tool_use`` blocks with ``stop_reason =
    "tool_use"``. Without an explicit ``text`` / ``chunks`` such a response
    carries no text.

Per-API wire shapes:

* Chat Completions: ``usage`` is sent in a final chunk as ``prompt_tokens`` /
  ``completion_tokens`` with ``prompt_tokens_details.cached_tokens`` and
  ``completion_tokens_details.reasoning_tokens``; ``incomplete`` ends with
  ``finish_reason = "length"``; ``sse_error`` / ``failed`` send a
  ``data: {"error": {...}}`` chunk.
* Anthropic Messages: ``usage`` may also carry ``cache_read_input_tokens`` and
  ``cache_creation_input_tokens`` (sent in ``message_start``); ``tool_events``
  of type ``web_search`` / ``code_interpreter`` become ``server_tool_use``
  blocks (``web_search`` / ``code_execution``) followed by their result
  blocks; ``incomplete`` ends with ``stop_reason = "max_tokens"``;
  ``sse_error`` / ``failed`` send an ``event: error`` with an Anthropic error
  object.
* Files API: a request carrying ``x-api-key`` or ``anthropic-version`` is
  answered in the Anthropic Files API shape (ids ``file_...``).
* Knowledge search: :meth:`MockProvider.set_search_results` registers the
  chunks returned by ``POST .../vector_stores/{id}/search`` for a store id
  that was never created through the mock (the organization knowledge
  base); ``max_num_results`` in the request body limits the result count.
"""

from __future__ import annotations

import asyncio
import copy
import json
import logging
import math
import random
import re
import socket
import string
import threading
import time
from collections import deque
from typing import Any, Callable

from aiohttp import web

__all__ = [
    "DEFAULT_FAILED_MESSAGE",
    "DEFAULT_SUMMARY_TEXT",
    "DEFAULT_TEXT",
    "DEFAULT_USAGE",
    "OPERATIONS",
    "MockProvider",
    "random_id",
]

DEFAULT_TEXT = "Hello from the mock provider. This is a streamed answer."
DEFAULT_SUMMARY_TEXT = (
    "<analysis>The user and the assistant exchanged a few messages.</analysis>"
    "<summary>mock summary</summary>"
)
DEFAULT_FAILED_MESSAGE = (
    "Upstream failure for response resp_abc123def456ghi789 while reading "
    "file-AbCdEf123456789012 with key sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123 "
    "at https://internal-llm.example.com/v1/responses"
)
DEFAULT_USAGE: dict[str, int] = {
    "input_tokens": 120,
    "output_tokens": 30,
    "cached_tokens": 0,
    "reasoning_tokens": 0,
}

#: Operation names accepted by :meth:`MockProvider.fail_next`.
OPERATIONS: tuple[str, ...] = (
    "responses",
    "chat_completions",
    "messages",
    "files_upload",
    "files_get",
    "files_delete",
    "vector_store_create",
    "vector_store_get",
    "vector_store_delete",
    "vector_store_add_file",
    "vector_store_file_get",
    "vector_store_file_delete",
    "vector_store_search",
)

_SCENARIO_KEYS = frozenset(
    {
        "text",
        "chunks",
        "delay_ms",
        "initial_delay_ms",
        "usage",
        "kind",
        "incomplete_reason",
        "error_code",
        "error_message",
        "status",
        "retry_after",
        "tool_events",
        "annotations",
        "hang_max_secs",
        "function_calls",
    }
)
_SCENARIO_KINDS = frozenset(
    {"completed", "incomplete", "failed", "sse_error", "http_error", "hang", "drop"}
)
_TOOL_TYPES = frozenset({"web_search", "file_search", "code_interpreter"})
_INDEXING_STATUSES = frozenset({"completed", "in_progress", "failed", "cancelled"})

# (method, path regex, operation). Order matters: the more specific vector
# store file routes must win over the plain ``/files/{id}`` routes.
_ROUTES: tuple[tuple[str, re.Pattern[str], str], ...] = tuple(
    (method, re.compile(pattern), op)
    for method, pattern, op in (
        ("POST", r"/responses$", "responses"),
        ("POST", r"/chat/completions$", "chat_completions"),
        ("POST", r"/messages$", "messages"),
        ("POST", r"/vector_stores/(?P<vs>[^/]+)/files$", "vector_store_add_file"),
        ("GET", r"/vector_stores/(?P<vs>[^/]+)/files/(?P<file>[^/]+)$", "vector_store_file_get"),
        (
            "DELETE",
            r"/vector_stores/(?P<vs>[^/]+)/files/(?P<file>[^/]+)$",
            "vector_store_file_delete",
        ),
        ("POST", r"/vector_stores/(?P<vs>[^/]+)/search$", "vector_store_search"),
        ("GET", r"/vector_stores/(?P<vs>[^/]+)$", "vector_store_get"),
        ("DELETE", r"/vector_stores/(?P<vs>[^/]+)$", "vector_store_delete"),
        ("POST", r"/vector_stores$", "vector_store_create"),
        ("POST", r"/files$", "files_upload"),
        ("GET", r"/files/(?P<file>[^/]+)$", "files_get"),
        ("DELETE", r"/files/(?P<file>[^/]+)$", "files_delete"),
    )
)

_ALNUM = string.ascii_letters + string.digits
_log = logging.getLogger("mini_chat_e2e.mock_provider")


def random_id(prefix: str, length: int = 24) -> str:
    """Return ``prefix`` followed by ``length`` random alphanumeric characters."""
    return prefix + "".join(random.choices(_ALNUM, k=length))


def _split_chunks(text: str, parts: int = 3) -> list[str]:
    if not text:
        return []
    size = max(1, math.ceil(len(text) / parts))
    return [text[i : i + size] for i in range(0, len(text), size)]


def _normalize_usage(raw: dict[str, Any] | None) -> dict[str, Any]:
    """Build an OpenAI Responses ``usage`` object from a (partial) override."""
    values = dict(DEFAULT_USAGE)
    if raw:
        for key in ("input_tokens", "output_tokens", "cached_tokens", "reasoning_tokens"):
            if key in raw:
                values[key] = int(raw[key])
        if isinstance(raw.get("input_tokens_details"), dict):
            values["cached_tokens"] = int(raw["input_tokens_details"].get("cached_tokens", 0))
        if isinstance(raw.get("output_tokens_details"), dict):
            values["reasoning_tokens"] = int(
                raw["output_tokens_details"].get("reasoning_tokens", 0)
            )
    return {
        "input_tokens": values["input_tokens"],
        "input_tokens_details": {"cached_tokens": values["cached_tokens"]},
        "output_tokens": values["output_tokens"],
        "output_tokens_details": {"reasoning_tokens": values["reasoning_tokens"]},
        "total_tokens": values["input_tokens"] + values["output_tokens"],
    }


def _validate_scenario(scenario: dict[str, Any]) -> dict[str, Any]:
    if not isinstance(scenario, dict):
        raise TypeError(f"scenario must be a dict, got {type(scenario).__name__}")
    unknown = set(scenario) - _SCENARIO_KEYS
    if unknown:
        raise ValueError(f"unknown scenario keys: {sorted(unknown)}")
    kind = scenario.get("kind", "completed")
    if kind not in _SCENARIO_KINDS:
        raise ValueError(f"unknown scenario kind {kind!r}; expected one of {sorted(_SCENARIO_KINDS)}")
    for spec in scenario.get("tool_events") or []:
        if spec.get("type") not in _TOOL_TYPES:
            raise ValueError(f"unknown tool event type in {spec!r}")
    for call in scenario.get("function_calls") or []:
        if not isinstance(call, dict) or not isinstance(call.get("name"), str):
            raise ValueError(f"function call needs a string 'name': {call!r}")
    return copy.deepcopy(scenario)


def _function_calls(scenario: dict[str, Any], id_prefix: str = "call_") -> list[dict[str, Any]]:
    """Normalized ``function_calls`` of a scenario: ``name``, ``call_id``, ``arguments`` (JSON string)."""
    calls = []
    for raw in scenario.get("function_calls") or []:
        arguments = raw.get("arguments", {})
        if not isinstance(arguments, str):
            arguments = json.dumps(arguments, separators=(",", ":"))
        calls.append(
            {"name": raw["name"], "call_id": raw.get("call_id") or random_id(id_prefix), "arguments": arguments}
        )
    return calls


def _is_anthropic(record: dict[str, Any]) -> bool:
    headers = record.get("headers") or {}
    return "x-api-key" in headers or "anthropic-version" in headers


class _ClientGone(Exception):
    """Raised internally when the streaming client has disconnected."""


class _SseWriter:
    """Writes SSE events to a prepared stream and detects client disconnects."""

    def __init__(self, request: web.Request, response: web.StreamResponse):
        self._request = request
        self._response = response
        self.sequence = 0

    def client_gone(self) -> bool:
        transport = self._request.transport
        return transport is None or transport.is_closing()

    async def sleep(self, delay_ms: float) -> None:
        """Sleep in small steps, aborting as soon as the client disconnects."""
        deadline = time.monotonic() + max(0.0, delay_ms) / 1000.0
        while True:
            if self.client_gone():
                raise _ClientGone()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            await asyncio.sleep(min(0.05, remaining))

    async def raw(self, payload: str) -> None:
        if self.client_gone():
            raise _ClientGone()
        try:
            await self._response.write(payload.encode("utf-8"))
        except (ConnectionError, RuntimeError) as exc:
            raise _ClientGone() from exc

    async def event(self, name: str | None, data: Any, *, add_type: bool = True) -> None:
        if isinstance(data, dict) and add_type and name is not None:
            data = {"type": name, "sequence_number": self.sequence, **data}
        self.sequence += 1
        body = data if isinstance(data, str) else json.dumps(data, separators=(",", ":"))
        prefix = f"event: {name}\n" if name is not None else ""
        await self.raw(f"{prefix}data: {body}\n\n")


class MockProvider:
    """OpenAI-compatible mock provider running in a background thread."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        self.host = host
        self._requested_port = port
        self.port: int = 0
        self._lock = threading.RLock()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._runner: web.AppRunner | None = None
        self._stopping = threading.Event()
        self._seq = 0
        self._reset_state()

    # ------------------------------------------------------------------ state

    def _reset_state(self) -> None:
        with self._lock:
            self._requests: list[dict[str, Any]] = []
            self._stream_queue: deque[dict[str, Any]] = deque()
            self._summary_queue: deque[dict[str, Any]] = deque()
            self._default_scenario: dict[str, Any] = {}
            self._summary_text = DEFAULT_SUMMARY_TEXT
            self._failures: dict[str, deque[dict[str, Any]]] = {}
            self._files: dict[str, dict[str, Any]] = {}
            self._vector_stores: dict[str, dict[str, Any]] = {}
            self._search_results: dict[str, list[dict[str, Any]]] = {}
            self._indexing: dict[str, Any] = {
                "initial": "completed",
                "polls": 0,
                "final": "completed",
            }
            self._disconnects = 0

    # -------------------------------------------------------------- lifecycle

    @property
    def url(self) -> str:
        """Base URL of the mock (no path)."""
        return f"http://{self.host}:{self.port}"

    def start(self) -> "MockProvider":
        if self._thread is not None:
            return self
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((self.host, self._requested_port))
        sock.listen(128)
        sock.setblocking(False)
        self.port = sock.getsockname()[1]

        loop = asyncio.new_event_loop()
        self._loop = loop
        self._stopping.clear()
        self._thread = threading.Thread(
            target=self._run_loop, args=(loop,), name=f"mock-provider-{self.port}", daemon=True
        )
        self._thread.start()
        asyncio.run_coroutine_threadsafe(self._start_site(sock), loop).result(timeout=15)
        return self

    def stop(self) -> None:
        if self._thread is None or self._loop is None:
            return
        self._stopping.set()
        loop = self._loop
        if self._runner is not None:
            try:
                asyncio.run_coroutine_threadsafe(self._runner.cleanup(), loop).result(timeout=15)
            except Exception:  # noqa: BLE001 - best-effort shutdown
                _log.exception("mock provider cleanup failed")
        loop.call_soon_threadsafe(loop.stop)
        self._thread.join(timeout=15)
        loop.close()
        self._thread = None
        self._loop = None
        self._runner = None

    def __enter__(self) -> "MockProvider":
        return self.start()

    def __exit__(self, *exc: object) -> None:
        self.stop()

    @staticmethod
    def _run_loop(loop: asyncio.AbstractEventLoop) -> None:
        asyncio.set_event_loop(loop)
        loop.run_forever()

    async def _start_site(self, sock: socket.socket) -> None:
        app = web.Application(client_max_size=256 * 1024 * 1024)
        app.router.add_route("*", "/{tail:.*}", self._dispatch)
        runner = web.AppRunner(app, access_log=None, handle_signals=False, shutdown_timeout=2)
        await runner.setup()
        await web.SockSite(runner, sock).start()
        self._runner = runner

    # ------------------------------------------------------------ public API

    def reset(self) -> None:
        """Forget recorded requests, queued scenarios, failures and stored objects."""
        self._reset_state()

    def requests(self, path_suffix: str | None = None, method: str | None = None) -> list[dict[str, Any]]:
        """Recorded requests (oldest first), optionally filtered by path suffix and method."""
        with self._lock:
            items = [
                r
                for r in self._requests
                if (path_suffix is None or r["path"].endswith(path_suffix))
                and (method is None or r["method"] == method.upper())
            ]
            return copy.deepcopy(items)

    def requests_for(self, op: str) -> list[dict[str, Any]]:
        """Recorded requests routed to operation ``op`` (see :data:`OPERATIONS`)."""
        with self._lock:
            return copy.deepcopy([r for r in self._requests if r["op"] == op])

    def last_request(self, path_suffix: str | None = None) -> dict[str, Any]:
        found = self.requests(path_suffix)
        if not found:
            raise AssertionError(f"mock provider recorded no request matching {path_suffix!r}")
        return found[-1]

    def wait_for_requests(
        self,
        path_suffix: str | None = None,
        count: int = 1,
        timeout: float = 15.0,
        method: str | None = None,
    ) -> list[dict[str, Any]]:
        """Block until at least ``count`` matching requests were recorded."""
        deadline = time.monotonic() + timeout
        while True:
            found = self.requests(path_suffix, method)
            if len(found) >= count:
                return found
            if time.monotonic() >= deadline:
                raise AssertionError(
                    f"expected >= {count} request(s) matching {path_suffix!r}, got {len(found)}"
                )
            time.sleep(0.05)

    def enqueue(self, *scenarios: dict[str, Any]) -> None:
        """Queue scenarios for successive streaming calls."""
        validated = [_validate_scenario(s) for s in scenarios]
        with self._lock:
            self._stream_queue.extend(validated)

    def enqueue_summary(self, *scenarios: dict[str, Any]) -> None:
        """Queue scenarios for successive non-streaming (thread summary) calls."""
        validated = [_validate_scenario(s) for s in scenarios]
        with self._lock:
            self._summary_queue.extend(validated)

    def set_default_scenario(self, scenario: dict[str, Any] | None) -> None:
        """Scenario used by streaming calls when the queue is empty."""
        validated = _validate_scenario(scenario or {})
        with self._lock:
            self._default_scenario = validated

    def set_summary_text(self, text: str) -> None:
        """Output text of non-streaming calls when no summary scenario is queued."""
        with self._lock:
            self._summary_text = text

    @property
    def pending_scenarios(self) -> int:
        with self._lock:
            return len(self._stream_queue)

    def fail_next(
        self,
        op: str,
        status: int = 500,
        times: int = 1,
        body: dict[str, Any] | None = None,
        retry_after: int | str | None = None,
    ) -> None:
        """Make the next ``times`` calls of operation ``op`` fail with ``status``."""
        if op not in OPERATIONS:
            raise ValueError(f"unknown operation {op!r}; expected one of {OPERATIONS}")
        failure = {"status": int(status), "body": body, "retry_after": retry_after}
        with self._lock:
            queue = self._failures.setdefault(op, deque())
            for _ in range(times):
                queue.append(dict(failure))

    def set_indexing(
        self, initial: str = "completed", polls: int | None = 0, final: str = "completed"
    ) -> None:
        """Configure vector store file indexing.

        ``initial`` is returned by ``POST .../vector_stores/{id}/files``. When
        it is ``in_progress``, status polls return ``in_progress`` for
        ``polls`` more calls and then ``final``; ``polls=None`` keeps the file
        ``in_progress`` forever.
        """
        for status in (initial, final):
            if status not in _INDEXING_STATUSES:
                raise ValueError(f"unknown indexing status {status!r}")
        with self._lock:
            self._indexing = {"initial": initial, "polls": polls, "final": final}

    def set_search_results(self, vector_store_id: str, results: list[dict[str, Any]]) -> None:
        """Chunks returned by ``POST .../vector_stores/{vector_store_id}/search``.

        For a store that does not exist in the mock (an organization knowledge
        base). Each result: ``text`` (required), optional ``file_id``,
        ``filename``, ``score``, ``attributes``. ``max_num_results`` in the
        search request body limits how many are returned.
        """
        normalized = []
        for index, raw in enumerate(results):
            normalized.append(
                {
                    "file_id": raw.get("file_id") or random_id("file-"),
                    "filename": raw.get("filename") or f"kb-{index}.txt",
                    "score": float(raw.get("score", 0.9)),
                    "attributes": dict(raw.get("attributes") or {}),
                    "content": [{"type": "text", "text": str(raw["text"])}],
                }
            )
        with self._lock:
            self._search_results[vector_store_id] = normalized

    @property
    def disconnects(self) -> int:
        """Number of streaming requests whose client disconnected before the end."""
        with self._lock:
            return self._disconnects

    def wait_for_disconnect(self, count: int = 1, timeout: float = 15.0) -> int:
        deadline = time.monotonic() + timeout
        while self.disconnects < count:
            if time.monotonic() >= deadline:
                raise AssertionError(
                    f"expected >= {count} client disconnect(s), got {self.disconnects}"
                )
            time.sleep(0.05)
        return self.disconnects

    @property
    def files(self) -> dict[str, dict[str, Any]]:
        """Files currently stored in the mock (not deleted)."""
        with self._lock:
            return copy.deepcopy(self._files)

    @property
    def vector_stores(self) -> dict[str, dict[str, Any]]:
        """Vector stores currently stored in the mock (not deleted)."""
        with self._lock:
            return copy.deepcopy(self._vector_stores)

    def state(self) -> dict[str, Any]:
        with self._lock:
            return {
                "disconnects": self._disconnects,
                "pending_scenarios": len(self._stream_queue),
                "pending_summary_scenarios": len(self._summary_queue),
                "files": copy.deepcopy(self._files),
                "vector_stores": copy.deepcopy(self._vector_stores),
                "request_count": len(self._requests),
            }

    # -------------------------------------------------------------- dispatch

    async def _dispatch(self, request: web.Request) -> web.StreamResponse:
        if request.path.startswith("/__mock/"):
            return await self._control(request)

        op, params = self._route(request.method, request.path)
        record = await self._record(request, op)
        if op is None:
            return self._error_json(404, f"mock: no route for {request.method} {request.path}", "not_found")

        failure = self._take_failure(op)
        if failure is not None:
            return self._failure_response(failure, op)

        handler: Callable[..., Any] = getattr(self, f"_op_{op}")
        return await handler(request, record, **params)

    @staticmethod
    def _route(method: str, path: str) -> tuple[str | None, dict[str, str]]:
        for route_method, pattern, op in _ROUTES:
            if route_method != method:
                continue
            match = pattern.search(path)
            if match:
                return op, match.groupdict()
        return None, {}

    async def _record(self, request: web.Request, op: str | None) -> dict[str, Any]:
        record: dict[str, Any] = {
            "seq": 0,
            "time": time.time(),
            "op": op,
            "method": request.method,
            "path": request.path,
            "query": {k: request.query.getall(k)[-1] for k in request.query.keys()},
            "headers": {k.lower(): v for k, v in request.headers.items()},
            "json": None,
            "body_text": None,
            "multipart": None,
            "stream": False,
            "disconnected": False,
            "finished": False,
            "response_id": None,
        }
        content_type = request.headers.get("Content-Type", "")
        if content_type.startswith("multipart/"):
            record["multipart"] = await self._read_multipart(request)
        elif request.can_read_body:
            raw = await request.read()
            text = raw.decode("utf-8", errors="replace")
            try:
                record["json"] = json.loads(text) if text else None
            except json.JSONDecodeError:
                record["body_text"] = text[:65536]
        if isinstance(record["json"], dict):
            record["stream"] = bool(record["json"].get("stream"))
        with self._lock:
            self._seq += 1
            record["seq"] = self._seq
            self._requests.append(record)
        return record

    @staticmethod
    async def _read_multipart(request: web.Request) -> list[dict[str, Any]]:
        fields: list[dict[str, Any]] = []
        reader = await request.multipart()
        while True:
            part = await reader.next()
            if part is None:
                break
            data = await part.read(decode=False)
            field: dict[str, Any] = {
                "name": part.name,
                "filename": part.filename,
                "content_type": part.headers.get("Content-Type"),
                "size": len(data),
                "value": None,
            }
            if part.filename is None:
                field["value"] = data.decode("utf-8", errors="replace")
            fields.append(field)
        return fields

    def _update_record(self, record: dict[str, Any], **changes: Any) -> None:
        with self._lock:
            record.update(changes)

    def _mark_disconnected(self, record: dict[str, Any]) -> None:
        with self._lock:
            if not record["disconnected"]:
                record["disconnected"] = True
                self._disconnects += 1

    def _take_failure(self, op: str) -> dict[str, Any] | None:
        with self._lock:
            queue = self._failures.get(op)
            if queue:
                return queue.popleft()
        return None

    def _take_scenario(self, stream: bool) -> dict[str, Any]:
        with self._lock:
            if stream:
                if self._stream_queue:
                    return self._stream_queue.popleft()
                return copy.deepcopy(self._default_scenario)
            if self._summary_queue:
                return self._summary_queue.popleft()
            return {"text": self._summary_text}

    @staticmethod
    def _error_json(
        status: int,
        message: str,
        code: str | None,
        error_type: str = "invalid_request_error",
        retry_after: int | str | None = None,
    ) -> web.Response:
        headers = {"Retry-After": str(retry_after)} if retry_after is not None else None
        body = {"error": {"message": message, "type": error_type, "code": code, "param": None}}
        return web.json_response(body, status=status, headers=headers)

    def _failure_response(self, failure: dict[str, Any], op: str) -> web.Response:
        status = failure["status"]
        if failure["body"] is not None:
            headers = (
                {"Retry-After": str(failure["retry_after"])}
                if failure["retry_after"] is not None
                else None
            )
            return web.json_response(failure["body"], status=status, headers=headers)
        error_type = "rate_limit_error" if status == 429 else "server_error"
        code = "rate_limit_exceeded" if status == 429 else "server_error"
        return self._error_json(
            status, f"mock: injected {op} failure", code, error_type, failure["retry_after"]
        )

    # --------------------------------------------------------------- control

    async def _control(self, request: web.Request) -> web.Response:
        action = request.path[len("/__mock/") :].strip("/")
        if request.method == "GET" and action == "requests":
            suffix = request.query.get("path_suffix")
            return web.json_response({"requests": self.requests(suffix)})
        if request.method == "GET" and action == "state":
            return web.json_response(self.state())
        if request.method == "POST" and action == "reset":
            self.reset()
            return web.json_response({"reset": True})
        if request.method == "POST" and action == "script":
            payload = await request.json()
            target = "stream"
            if isinstance(payload, dict) and "scenarios" in payload:
                target = payload.get("target", "stream")
                scenarios = payload["scenarios"]
            elif isinstance(payload, list):
                scenarios = payload
            else:
                scenarios = [payload]
            try:
                if target == "summary":
                    self.enqueue_summary(*scenarios)
                elif target == "default":
                    self.set_default_scenario(scenarios[0] if scenarios else {})
                else:
                    self.enqueue(*scenarios)
            except (TypeError, ValueError) as exc:
                return web.json_response({"error": str(exc)}, status=400)
            return web.json_response({"queued": len(scenarios), "target": target})
        if request.method == "POST" and action == "fail":
            payload = await request.json()
            try:
                self.fail_next(
                    payload["op"],
                    status=payload.get("status", 500),
                    times=payload.get("times", 1),
                    body=payload.get("body"),
                    retry_after=payload.get("retry_after"),
                )
            except (KeyError, ValueError) as exc:
                return web.json_response({"error": str(exc)}, status=400)
            return web.json_response({"armed": payload["op"]})
        if request.method == "POST" and action == "indexing":
            payload = await request.json()
            try:
                self.set_indexing(**payload)
            except (TypeError, ValueError) as exc:
                return web.json_response({"error": str(exc)}, status=400)
            return web.json_response({"indexing": payload})
        return web.json_response({"error": f"unknown control action {action!r}"}, status=404)

    # ------------------------------------------------------ Responses API

    async def _op_responses(self, request: web.Request, record: dict[str, Any]) -> web.StreamResponse:
        body = record["json"] if isinstance(record["json"], dict) else {}
        scenario = self._take_scenario(record["stream"])
        if record["stream"]:
            return await self._stream_responses(request, record, body, scenario)
        return await self._json_responses(record, body, scenario)

    async def _json_responses(
        self, record: dict[str, Any], body: dict[str, Any], scenario: dict[str, Any]
    ) -> web.Response:
        await asyncio.sleep(scenario.get("initial_delay_ms", 0) / 1000.0)
        kind = scenario.get("kind", "completed")
        if kind == "http_error":
            return self._scenario_http_error(scenario)
        response_id = random_id("resp_")
        self._update_record(record, response_id=response_id)
        text = scenario["text"] if "text" in scenario else "".join(scenario.get("chunks") or [])
        response = self._response_object(
            response_id,
            body.get("model"),
            status="completed",
            output=[self._message_item(random_id("msg_"), text, [])],
            usage=scenario.get("usage", {}),
        )
        if kind == "incomplete":
            response["status"] = "incomplete"
            response["incomplete_details"] = {
                "reason": scenario.get("incomplete_reason", "max_output_tokens")
            }
        elif kind == "failed":
            response["status"] = "failed"
            response["output"] = []
            response["error"] = {
                "code": scenario.get("error_code", "server_error"),
                "message": scenario.get("error_message", DEFAULT_FAILED_MESSAGE),
            }
        self._update_record(record, finished=True)
        return web.json_response(response)

    def _scenario_http_error(self, scenario: dict[str, Any]) -> web.Response:
        status = int(scenario.get("status", 500))
        error_type = "rate_limit_error" if status == 429 else "server_error"
        default_code = "rate_limit_exceeded" if status == 429 else "server_error"
        return self._error_json(
            status,
            scenario.get("error_message", DEFAULT_FAILED_MESSAGE),
            scenario.get("error_code", default_code),
            error_type,
            scenario.get("retry_after"),
        )

    @staticmethod
    def _response_object(
        response_id: str,
        model: Any,
        *,
        status: str,
        output: list[dict[str, Any]],
        usage: dict[str, Any] | None,
    ) -> dict[str, Any]:
        response: dict[str, Any] = {
            "id": response_id,
            "object": "response",
            "created_at": int(time.time()),
            "status": status,
            "model": model,
            "output": output,
            "error": None,
            "incomplete_details": None,
        }
        if usage is not None:
            response["usage"] = _normalize_usage(usage)
        return response

    @staticmethod
    def _message_item(item_id: str, text: str, annotations: list[dict[str, Any]]) -> dict[str, Any]:
        return {
            "type": "message",
            "id": item_id,
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": annotations}],
        }

    @staticmethod
    def _scenario_chunks(scenario: dict[str, Any], kind: str) -> list[str]:
        if "chunks" in scenario:
            return [str(c) for c in scenario["chunks"]]
        if "text" in scenario:
            return _split_chunks(str(scenario["text"]))
        if kind in ("completed", "incomplete", "drop"):
            return _split_chunks(DEFAULT_TEXT)
        return []

    @staticmethod
    def _annotations(scenario: dict[str, Any], text: str) -> list[dict[str, Any]]:
        result: list[dict[str, Any]] = []
        for raw in scenario.get("annotations") or []:
            annotation = dict(raw)
            if "type" not in annotation:
                annotation["type"] = "url_citation" if "url" in annotation else "file_citation"
            if annotation["type"] == "url_citation":
                annotation.setdefault("title", "")
                annotation.setdefault("start_index", 0)
                annotation.setdefault("end_index", len(text))
            elif annotation["type"] == "file_citation":
                annotation.setdefault("filename", "")
                annotation.setdefault("index", len(text))
            result.append(annotation)
        return result

    async def _stream_responses(
        self,
        request: web.Request,
        record: dict[str, Any],
        body: dict[str, Any],
        scenario: dict[str, Any],
    ) -> web.StreamResponse:
        kind = scenario.get("kind", "completed")
        await asyncio.sleep(scenario.get("initial_delay_ms", 0) / 1000.0)
        if kind == "http_error":
            self._update_record(record, finished=True)
            return self._scenario_http_error(scenario)

        response = web.StreamResponse(
            status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"}
        )
        await response.prepare(request)
        sse = _SseWriter(request, response)
        response_id = random_id("resp_")
        self._update_record(record, response_id=response_id)
        model = body.get("model")
        delay_ms = float(scenario.get("delay_ms", 0))

        try:
            in_progress = self._response_object(
                response_id, model, status="in_progress", output=[], usage=None
            )
            in_progress["usage"] = None
            await sse.event("response.created", {"response": in_progress})

            if kind == "hang":
                await self._hang(sse, float(scenario.get("hang_max_secs", 600)))
                return response

            await sse.sleep(delay_ms)
            await sse.event("response.in_progress", {"response": in_progress})

            output_items: list[dict[str, Any]] = []
            for spec in scenario.get("tool_events") or []:
                output_items.extend(await self._emit_tool_events(sse, spec, len(output_items), delay_ms))
            calls = _function_calls(scenario)
            for call in calls:
                output_items.append(
                    await self._emit_function_call(sse, call, len(output_items), delay_ms)
                )

            chunks = self._scenario_chunks(scenario, kind)
            if calls and "text" not in scenario and "chunks" not in scenario:
                chunks = []
            text = "".join(chunks)
            annotations = self._annotations(scenario, text)
            message_index = len(output_items)
            message_id = random_id("msg_")
            if chunks or (kind in ("completed", "incomplete") and not calls):
                emitted = await self._emit_message(
                    sse, message_id, message_index, chunks, annotations, delay_ms,
                    drop_midway=(kind == "drop"),
                )
                if not emitted:
                    self._close_transport(request)
                    return response
                output_items.append(self._message_item(message_id, text, annotations))

            if kind == "drop":
                self._close_transport(request)
                return response

            await sse.sleep(delay_ms)
            await self._emit_terminal(sse, kind, scenario, response_id, model, output_items)
            self._update_record(record, finished=True)
            await response.write_eof()
        except _ClientGone:
            self._mark_disconnected(record)
        except asyncio.CancelledError:
            self._mark_disconnected(record)
            raise
        return response

    @staticmethod
    def _close_transport(request: web.Request) -> None:
        """Close the connection without a terminal event (``drop`` scenario)."""
        if request.transport is not None:
            request.transport.close()

    async def _hang(self, sse: _SseWriter, max_secs: float) -> None:
        deadline = time.monotonic() + max_secs
        while time.monotonic() < deadline and not self._stopping.is_set():
            if sse.client_gone():
                raise _ClientGone()
            await asyncio.sleep(0.05)

    async def _emit_message(
        self,
        sse: _SseWriter,
        message_id: str,
        output_index: int,
        chunks: list[str],
        annotations: list[dict[str, Any]],
        delay_ms: float,
        *,
        drop_midway: bool,
    ) -> bool:
        """Emit a message item. Returns ``False`` when it stopped midway (``drop``)."""
        text = "".join(chunks)
        await sse.event(
            "response.output_item.added",
            {
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": message_id,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            },
        )
        await sse.event(
            "response.content_part.added",
            {
                "item_id": message_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            },
        )
        stop_after = max(1, len(chunks) // 2) if drop_midway else None
        for index, chunk in enumerate(chunks):
            if stop_after is not None and index >= stop_after:
                return False
            await sse.sleep(delay_ms)
            await sse.event(
                "response.output_text.delta",
                {
                    "item_id": message_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "delta": chunk,
                },
            )
        if drop_midway:
            return False
        for annotation_index, annotation in enumerate(annotations):
            await sse.event(
                "response.output_text.annotation.added",
                {
                    "item_id": message_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "annotation_index": annotation_index,
                    "annotation": annotation,
                },
            )
        await sse.event(
            "response.output_text.done",
            {"item_id": message_id, "output_index": output_index, "content_index": 0, "text": text},
        )
        await sse.event(
            "response.content_part.done",
            {
                "item_id": message_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": annotations},
            },
        )
        await sse.event(
            "response.output_item.done",
            {"output_index": output_index, "item": self._message_item(message_id, text, annotations)},
        )
        return True

    async def _emit_tool_events(
        self, sse: _SseWriter, spec: dict[str, Any], first_index: int, delay_ms: float
    ) -> list[dict[str, Any]]:
        tool = spec["type"]
        count = int(spec.get("count", 1))
        items: list[dict[str, Any]] = []
        for call in range(count):
            index = first_index + call
            await sse.sleep(delay_ms)
            if tool == "web_search":
                item_id = random_id("ws_")
                await sse.event("response.web_search_call.searching", {"item_id": item_id, "output_index": index})
                await sse.event("response.web_search_call.completed", {"item_id": item_id, "output_index": index})
                items.append(
                    {
                        "type": "web_search_call",
                        "id": item_id,
                        "status": "completed",
                        "action": {"type": "search", "query": spec.get("query", "mock query")},
                    }
                )
            elif tool == "file_search":
                item_id = random_id("fs_")
                results = self._file_search_results(int(spec.get("results", 2)), spec.get("file_ids"))
                await sse.event("response.file_search_call.searching", {"item_id": item_id, "output_index": index})
                await sse.event(
                    "response.file_search_call.completed",
                    {"item_id": item_id, "output_index": index, "results": results},
                )
                items.append(
                    {
                        "type": "file_search_call",
                        "id": item_id,
                        "status": "completed",
                        "queries": [spec.get("query", "mock query")],
                        "results": results,
                    }
                )
            else:
                item_id = random_id("ci_")
                logs = str(spec.get("logs", "mock code interpreter output"))
                await sse.event(
                    "response.code_interpreter_call.in_progress", {"item_id": item_id, "output_index": index}
                )
                await sse.event(
                    "response.code_interpreter_call.interpreting", {"item_id": item_id, "output_index": index}
                )
                await sse.event(
                    "response.code_interpreter_call.completed", {"item_id": item_id, "output_index": index}
                )
                item = {
                    "type": "code_interpreter_call",
                    "id": item_id,
                    "status": "completed",
                    "code": spec.get("code", "print('mock')"),
                    "container_id": random_id("cntr_"),
                    "outputs": [{"type": "logs", "logs": logs}],
                }
                await sse.event("response.output_item.done", {"output_index": index, "item": item})
                items.append(item)
        return items

    async def _emit_function_call(
        self, sse: _SseWriter, call: dict[str, Any], index: int, delay_ms: float
    ) -> dict[str, Any]:
        """Emit one Responses ``function_call`` output item; returns the completed item."""
        item_id = random_id("fc_")
        await sse.sleep(delay_ms)
        await sse.event(
            "response.output_item.added",
            {
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call["call_id"],
                    "name": call["name"],
                    "arguments": "",
                    "status": "in_progress",
                },
            },
        )
        for piece in _split_chunks(call["arguments"], 2):
            await sse.event(
                "response.function_call_arguments.delta",
                {"item_id": item_id, "output_index": index, "delta": piece},
            )
        await sse.event(
            "response.function_call_arguments.done",
            {"item_id": item_id, "output_index": index, "arguments": call["arguments"]},
        )
        item = {
            "type": "function_call",
            "id": item_id,
            "call_id": call["call_id"],
            "name": call["name"],
            "arguments": call["arguments"],
            "status": "completed",
        }
        await sse.event("response.output_item.done", {"output_index": index, "item": item})
        return item

    def _file_search_results(self, count: int, file_ids: list[str] | None) -> list[dict[str, Any]]:
        with self._lock:
            known = list(self._files.items())
        results: list[dict[str, Any]] = []
        for i in range(count):
            if file_ids:
                file_id, filename = file_ids[i % len(file_ids)], f"doc-{i}.txt"
            elif known:
                file_id, meta = known[i % len(known)]
                filename = meta.get("filename") or f"doc-{i}.txt"
            else:
                file_id, filename = random_id("file-"), f"doc-{i}.txt"
            results.append(
                {"file_id": file_id, "filename": filename, "score": 0.9, "text": f"chunk {i}", "attributes": {}}
            )
        return results

    async def _emit_terminal(
        self,
        sse: _SseWriter,
        kind: str,
        scenario: dict[str, Any],
        response_id: str,
        model: Any,
        output_items: list[dict[str, Any]],
    ) -> None:
        if kind in ("completed", "incomplete"):
            usage = scenario.get("usage", {})
            final = self._response_object(
                response_id, model, status=kind, output=output_items, usage=usage
            )
            if kind == "incomplete":
                final["incomplete_details"] = {
                    "reason": scenario.get("incomplete_reason", "max_output_tokens")
                }
            await sse.event(f"response.{kind}", {"response": final})
            return
        error = {
            "code": scenario.get("error_code", "server_error"),
            "message": scenario.get("error_message", DEFAULT_FAILED_MESSAGE),
        }
        if kind == "failed":
            usage = scenario.get("usage") if "usage" in scenario else None
            final = self._response_object(
                response_id, model, status="failed", output=output_items, usage=usage
            )
            final["error"] = error
            await sse.event("response.failed", {"response": final})
            return
        # sse_error
        await sse.event("error", {**error, "param": None})

    # ------------------------------------------------- Chat Completions API

    async def _op_chat_completions(
        self, request: web.Request, record: dict[str, Any]
    ) -> web.StreamResponse:
        body = record["json"] if isinstance(record["json"], dict) else {}
        scenario = self._take_scenario(record["stream"])
        await asyncio.sleep(scenario.get("initial_delay_ms", 0) / 1000.0)
        if scenario.get("kind") == "http_error":
            self._update_record(record, finished=True)
            return self._scenario_http_error(scenario)
        kind = scenario.get("kind", "completed")
        calls = _function_calls(scenario)
        chunks = self._scenario_chunks(scenario, "completed")
        if calls and "text" not in scenario and "chunks" not in scenario:
            chunks = []
        if kind in ("sse_error", "failed") and "text" not in scenario and "chunks" not in scenario:
            chunks = []
        raw_usage = scenario.get("usage", {})
        usage = _normalize_usage(raw_usage) if raw_usage is not None else None
        completion_id = random_id("chatcmpl-")
        created = int(time.time())
        model = body.get("model")
        if not record["stream"]:
            message = {"role": "assistant", "content": "".join(chunks)}
            payload: dict[str, Any] = {
                "id": completion_id,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
            }
            if usage is not None:
                payload["usage"] = self._chat_usage(usage)
            self._update_record(record, finished=True, response_id=completion_id)
            return web.json_response(payload)

        response = web.StreamResponse(
            status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"}
        )
        await response.prepare(request)
        sse = _SseWriter(request, response)
        self._update_record(record, response_id=completion_id)
        delay_ms = float(scenario.get("delay_ms", 0))

        def chunk(delta: dict[str, Any], finish: str | None) -> dict[str, Any]:
            return {
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }

        try:
            await sse.event(None, chunk({"role": "assistant", "content": ""}, None), add_type=False)
            if kind == "hang":
                await self._hang(sse, float(scenario.get("hang_max_secs", 600)))
                return response
            for piece in chunks:
                await sse.sleep(delay_ms)
                await sse.event(None, chunk({"content": piece}, None), add_type=False)
            if kind == "drop":
                self._close_transport(request)
                return response
            if kind in ("sse_error", "failed"):
                error = {
                    "message": scenario.get("error_message", DEFAULT_FAILED_MESSAGE),
                    "type": "server_error",
                    "code": scenario.get("error_code", "server_error"),
                    "param": None,
                }
                await sse.event(None, {"error": error}, add_type=False)
                self._update_record(record, finished=True)
                await response.write_eof()
                return response
            for index, call in enumerate(calls):
                await sse.sleep(delay_ms)
                await sse.event(
                    None,
                    chunk(
                        {
                            "tool_calls": [
                                {
                                    "index": index,
                                    "id": call["call_id"],
                                    "type": "function",
                                    "function": {"name": call["name"], "arguments": ""},
                                }
                            ]
                        },
                        None,
                    ),
                    add_type=False,
                )
                for piece in _split_chunks(call["arguments"], 2):
                    await sse.event(
                        None,
                        chunk({"tool_calls": [{"index": index, "function": {"arguments": piece}}]}, None),
                        add_type=False,
                    )
            if calls:
                finish = "tool_calls"
            else:
                finish = "length" if kind == "incomplete" else "stop"
            await sse.event(None, chunk({}, finish), add_type=False)
            if usage is not None:
                await sse.event(
                    None,
                    {
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [],
                        "usage": self._chat_usage(usage),
                    },
                    add_type=False,
                )
            await sse.event(None, "[DONE]", add_type=False)
            self._update_record(record, finished=True)
            await response.write_eof()
        except _ClientGone:
            self._mark_disconnected(record)
        except asyncio.CancelledError:
            self._mark_disconnected(record)
            raise
        return response

    @staticmethod
    def _chat_usage(usage: dict[str, Any]) -> dict[str, Any]:
        return {
            "prompt_tokens": usage["input_tokens"],
            "completion_tokens": usage["output_tokens"],
            "total_tokens": usage["total_tokens"],
            "prompt_tokens_details": {"cached_tokens": usage["input_tokens_details"]["cached_tokens"]},
            "completion_tokens_details": {
                "reasoning_tokens": usage["output_tokens_details"]["reasoning_tokens"]
            },
        }

    # ------------------------------------------------ Anthropic Messages API

    @staticmethod
    def _anthropic_usage(raw: dict[str, Any] | None) -> dict[str, int]:
        """Anthropic usage: ``input_tokens`` (uncached input), ``output_tokens`` and the cache counters."""
        values = dict(DEFAULT_USAGE)
        values.update({"cache_read_input_tokens": 0, "cache_creation_input_tokens": 0})
        for key in values:
            if raw and key in raw:
                values[key] = int(raw[key])
        return {
            "input_tokens": values["input_tokens"],
            "output_tokens": values["output_tokens"],
            "cache_read_input_tokens": values["cache_read_input_tokens"],
            "cache_creation_input_tokens": values["cache_creation_input_tokens"],
        }

    async def _op_messages(self, request: web.Request, record: dict[str, Any]) -> web.StreamResponse:
        body = record["json"] if isinstance(record["json"], dict) else {}
        scenario = self._take_scenario(record["stream"])
        await asyncio.sleep(scenario.get("initial_delay_ms", 0) / 1000.0)
        if scenario.get("kind") == "http_error":
            self._update_record(record, finished=True)
            return self._scenario_http_error(scenario)
        kind = scenario.get("kind", "completed")
        calls = _function_calls(scenario, "toolu_")
        chunks = self._scenario_chunks(scenario, "completed")
        if calls and "text" not in scenario and "chunks" not in scenario:
            chunks = []
        if kind in ("sse_error", "failed") and "text" not in scenario and "chunks" not in scenario:
            chunks = []
        raw_usage = scenario.get("usage", {})
        usage = self._anthropic_usage(raw_usage if raw_usage is not None else {})
        message_id = random_id("msg_")
        model = body.get("model")
        if calls:
            stop_reason = "tool_use"
        else:
            stop_reason = "max_tokens" if kind == "incomplete" else "end_turn"
        if not record["stream"]:
            self._update_record(record, finished=True, response_id=message_id)
            return web.json_response(
                {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [{"type": "text", "text": "".join(chunks)}],
                    "stop_reason": stop_reason,
                    "stop_sequence": None,
                    "usage": {
                        "input_tokens": usage["input_tokens"],
                        "output_tokens": usage["output_tokens"],
                    },
                }
            )

        response = web.StreamResponse(
            status=200, headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"}
        )
        await response.prepare(request)
        sse = _SseWriter(request, response)
        self._update_record(record, response_id=message_id)
        delay_ms = float(scenario.get("delay_ms", 0))

        async def block(index: int, content_block: dict[str, Any], deltas: list[dict[str, Any]]) -> None:
            await sse.event(
                "content_block_start",
                {"type": "content_block_start", "index": index, "content_block": content_block},
                add_type=False,
            )
            for delta in deltas:
                await sse.sleep(delay_ms)
                await sse.event(
                    "content_block_delta",
                    {"type": "content_block_delta", "index": index, "delta": delta},
                    add_type=False,
                )
            await sse.event("content_block_stop", {"type": "content_block_stop", "index": index}, add_type=False)

        try:
            await sse.event(
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {
                            "input_tokens": usage["input_tokens"],
                            "cache_creation_input_tokens": usage["cache_creation_input_tokens"],
                            "cache_read_input_tokens": usage["cache_read_input_tokens"],
                            "output_tokens": 1,
                        },
                    },
                },
                add_type=False,
            )
            if kind == "hang":
                await self._hang(sse, float(scenario.get("hang_max_secs", 600)))
                return response
            await sse.event("ping", {"type": "ping"}, add_type=False)
            index = 0
            web_searches = 0
            for spec in scenario.get("tool_events") or []:
                tool = spec["type"]
                if tool not in ("web_search", "code_interpreter"):
                    continue
                for _ in range(int(spec.get("count", 1))):
                    tool_use_id = random_id("srvtoolu_")
                    if tool == "web_search":
                        web_searches += 1
                        call_input = {"query": spec.get("query", "mock query")}
                        await block(
                            index,
                            {"type": "server_tool_use", "id": tool_use_id, "name": "web_search", "input": {}},
                            [{"type": "input_json_delta", "partial_json": json.dumps(call_input)}],
                        )
                        result = {
                            "type": "web_search_tool_result",
                            "tool_use_id": tool_use_id,
                            "content": [
                                {
                                    "type": "web_search_result",
                                    "url": spec.get("url", "https://example.com/mock"),
                                    "title": spec.get("title", "Mock result"),
                                    "encrypted_content": "mock-encrypted",
                                    "page_age": None,
                                }
                            ],
                        }
                    else:
                        call_input = {"code": spec.get("code", "print('mock')")}
                        await block(
                            index,
                            {"type": "server_tool_use", "id": tool_use_id, "name": "code_execution", "input": {}},
                            [{"type": "input_json_delta", "partial_json": json.dumps(call_input)}],
                        )
                        result = {
                            "type": "code_execution_tool_result",
                            "tool_use_id": tool_use_id,
                            "content": {
                                "type": "code_execution_result",
                                "stdout": str(spec.get("logs", "mock code interpreter output")),
                                "stderr": "",
                                "return_code": 0,
                                "content": [],
                            },
                        }
                    await block(index + 1, result, [])
                    index += 2
            if chunks:
                await block(
                    index,
                    {"type": "text", "text": ""},
                    [{"type": "text_delta", "text": piece} for piece in chunks],
                )
                index += 1
            if kind == "drop":
                self._close_transport(request)
                return response
            if kind in ("sse_error", "failed"):
                await sse.event(
                    "error",
                    {
                        "type": "error",
                        "error": {
                            "type": scenario.get("error_code", "api_error"),
                            "message": scenario.get("error_message", DEFAULT_FAILED_MESSAGE),
                        },
                    },
                    add_type=False,
                )
                self._update_record(record, finished=True)
                await response.write_eof()
                return response
            for call in calls:
                await block(
                    index,
                    {"type": "tool_use", "id": call["call_id"], "name": call["name"], "input": {}},
                    [{"type": "input_json_delta", "partial_json": piece} for piece in _split_chunks(call["arguments"], 2)],
                )
                index += 1
            delta_usage: dict[str, Any] = {"output_tokens": usage["output_tokens"]}
            if web_searches:
                delta_usage["server_tool_use"] = {"web_search_requests": web_searches}
            await sse.event(
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                    "usage": delta_usage,
                },
                add_type=False,
            )
            await sse.event("message_stop", {"type": "message_stop"}, add_type=False)
            self._update_record(record, finished=True)
            await response.write_eof()
        except _ClientGone:
            self._mark_disconnected(record)
        except asyncio.CancelledError:
            self._mark_disconnected(record)
            raise
        return response

    # ------------------------------------------------------------ Files API

    async def _op_files_upload(self, request: web.Request, record: dict[str, Any]) -> web.Response:
        fields = record["multipart"] or []
        file_field = next((f for f in fields if f["name"] == "file"), None)
        if file_field is None:
            return self._error_json(400, "mock: multipart field 'file' is required", "missing_file")
        purpose = next((f["value"] for f in fields if f["name"] == "purpose"), None)
        anthropic = _is_anthropic(record)
        file_id = random_id("file_" if anthropic else "file-")
        meta = {
            "id": file_id,
            "object": "file",
            "bytes": file_field["size"],
            "created_at": int(time.time()),
            "filename": file_field["filename"],
            "purpose": purpose,
            "status": "processed",
            "content_type": file_field["content_type"],
        }
        if anthropic:
            meta["anthropic"] = True
        with self._lock:
            self._files[file_id] = meta
        self._update_record(record, finished=True, response_id=file_id)
        if anthropic:
            return web.json_response(
                {
                    "id": file_id,
                    "type": "file",
                    "filename": file_field["filename"],
                    "mime_type": file_field["content_type"],
                    "size_bytes": file_field["size"],
                    "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(meta["created_at"])),
                    "downloadable": False,
                }
            )
        return web.json_response({k: v for k, v in meta.items() if k != "content_type"})

    async def _op_files_get(self, request: web.Request, record: dict[str, Any], file: str) -> web.Response:
        with self._lock:
            meta = copy.deepcopy(self._files.get(file))
        if meta is None:
            return self._error_json(404, f"No such File object: {file}", "not_found")
        meta.pop("content_type", None)
        meta.pop("anthropic", None)
        return web.json_response(meta)

    async def _op_files_delete(self, request: web.Request, record: dict[str, Any], file: str) -> web.Response:
        with self._lock:
            existed = self._files.pop(file, None) is not None
        if not existed:
            return self._error_json(404, f"No such File object: {file}", "not_found")
        if _is_anthropic(record):
            return web.json_response({"id": file, "type": "file_deleted"})
        return web.json_response({"id": file, "object": "file", "deleted": True})

    # ---------------------------------------------------- Vector stores API

    async def _op_vector_store_create(self, request: web.Request, record: dict[str, Any]) -> web.Response:
        body = record["json"] if isinstance(record["json"], dict) else {}
        store_id = random_id("vs_")
        store = {
            "id": store_id,
            "object": "vector_store",
            "created_at": int(time.time()),
            "name": body.get("name"),
            "metadata": body.get("metadata") or {},
            "status": "completed",
            "files": {},
        }
        with self._lock:
            self._vector_stores[store_id] = store
        self._update_record(record, finished=True, response_id=store_id)
        return web.json_response(self._store_view(store))

    @staticmethod
    def _store_view(store: dict[str, Any]) -> dict[str, Any]:
        statuses = [f["status"] for f in store["files"].values()]
        return {
            "id": store["id"],
            "object": "vector_store",
            "created_at": store["created_at"],
            "name": store["name"],
            "metadata": store["metadata"],
            "status": store["status"],
            "file_counts": {
                "in_progress": statuses.count("in_progress"),
                "completed": statuses.count("completed"),
                "failed": statuses.count("failed"),
                "cancelled": statuses.count("cancelled"),
                "total": len(statuses),
            },
        }

    async def _op_vector_store_get(self, request: web.Request, record: dict[str, Any], vs: str) -> web.Response:
        with self._lock:
            store = copy.deepcopy(self._vector_stores.get(vs))
        if store is None:
            return self._error_json(404, f"No such vector store: {vs}", "not_found")
        return web.json_response(self._store_view(store))

    async def _op_vector_store_delete(self, request: web.Request, record: dict[str, Any], vs: str) -> web.Response:
        with self._lock:
            existed = self._vector_stores.pop(vs, None) is not None
        if not existed:
            return self._error_json(404, f"No such vector store: {vs}", "not_found")
        return web.json_response({"id": vs, "object": "vector_store.deleted", "deleted": True})

    @staticmethod
    def _vs_file_view(store_id: str, entry: dict[str, Any]) -> dict[str, Any]:
        view = {
            "id": entry["id"],
            "object": "vector_store.file",
            "vector_store_id": store_id,
            "created_at": entry["created_at"],
            "status": entry["status"],
            "attributes": entry.get("attributes") or {},
            "last_error": None,
        }
        if entry["status"] == "failed":
            view["last_error"] = {"code": "server_error", "message": "mock: indexing failed"}
        return view

    async def _op_vector_store_add_file(
        self, request: web.Request, record: dict[str, Any], vs: str
    ) -> web.Response:
        body = record["json"] if isinstance(record["json"], dict) else {}
        file_id = body.get("file_id")
        if not file_id:
            return self._error_json(400, "mock: file_id is required", "missing_file_id")
        with self._lock:
            store = self._vector_stores.get(vs)
            if store is None:
                return self._error_json(404, f"No such vector store: {vs}", "not_found")
            indexing = dict(self._indexing)
            entry = {
                "id": file_id,
                "created_at": int(time.time()),
                "status": indexing["initial"],
                "polls_left": indexing["polls"],
                "final": indexing["final"],
                "attributes": body.get("attributes") or {},
            }
            store["files"][file_id] = entry
            view = self._vs_file_view(vs, entry)
        return web.json_response(view)

    async def _op_vector_store_file_get(
        self, request: web.Request, record: dict[str, Any], vs: str, file: str
    ) -> web.Response:
        with self._lock:
            store = self._vector_stores.get(vs)
            entry = store["files"].get(file) if store else None
            if entry is None:
                return self._error_json(404, f"No such vector store file: {vs}/{file}", "not_found")
            if entry["status"] == "in_progress" and entry["polls_left"] is not None:
                if entry["polls_left"] > 0:
                    entry["polls_left"] -= 1
                else:
                    entry["status"] = entry["final"]
            view = self._vs_file_view(vs, entry)
        return web.json_response(view)

    async def _op_vector_store_file_delete(
        self, request: web.Request, record: dict[str, Any], vs: str, file: str
    ) -> web.Response:
        with self._lock:
            store = self._vector_stores.get(vs)
            existed = bool(store) and store["files"].pop(file, None) is not None
        if not existed:
            return self._error_json(404, f"No such vector store file: {vs}/{file}", "not_found")
        return web.json_response({"id": file, "object": "vector_store.file.deleted", "deleted": True})

    async def _op_vector_store_search(
        self, request: web.Request, record: dict[str, Any], vs: str
    ) -> web.Response:
        body = record["json"] if isinstance(record["json"], dict) else {}
        with self._lock:
            store = copy.deepcopy(self._vector_stores.get(vs))
            files = copy.deepcopy(self._files)
            knowledge = copy.deepcopy(self._search_results.get(vs))
        if store is None and knowledge is not None:
            limit = body.get("max_num_results")
            if isinstance(limit, int) and limit >= 0:
                knowledge = knowledge[:limit]
            return web.json_response(
                {
                    "object": "vector_store.search_results.page",
                    "search_query": body.get("query"),
                    "data": knowledge,
                    "has_more": False,
                    "next_page": None,
                }
            )
        if store is None:
            return self._error_json(404, f"No such vector store: {vs}", "not_found")
        data = []
        for file_id in store["files"] or {random_id("file-"): {}}:
            data.append(
                {
                    "file_id": file_id,
                    "filename": (files.get(file_id) or {}).get("filename") or "doc.txt",
                    "score": 0.9,
                    "attributes": {},
                    "content": [{"type": "text", "text": "chunk"}],
                }
            )
        return web.json_response(
            {
                "object": "vector_store.search_results.page",
                "search_query": body.get("query"),
                "data": data,
                "has_more": False,
                "next_page": None,
            }
        )


if __name__ == "__main__":  # Manual smoke run: python3 mock_provider.py
    logging.basicConfig(level=logging.INFO)
    with MockProvider() as provider:
        print(f"mock provider listening on {provider.url} (Ctrl-C to stop)")
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            pass
