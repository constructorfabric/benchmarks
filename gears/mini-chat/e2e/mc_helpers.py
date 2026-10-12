"""Shared helpers for the mini-chat black-box E2E suite.

* Canonical ``Problem`` assertions (ADR-0004, ``toolkit-canonical-errors``).
* An SSE client/parser for ``httpx`` streaming responses.
* UUID helpers, including the 16-byte BLOB form mini-chat writes into SQLite
  (DESIGN §3.7: UUID columns are declared ``TEXT`` but hold the UUID bytes).
* SQLite query helpers and a polling helper for asynchronous effects.
* ``deep_merge`` with a :data:`DELETE` sentinel for config patches.
"""

from __future__ import annotations

import copy
import json
import re
import sqlite3
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Iterator, TypeVar

import httpx

T = TypeVar("T")

# --------------------------------------------------------------------------
# Canonical error categories (libs/toolkit-canonical-errors/src/problem.rs)
# --------------------------------------------------------------------------

#: category -> (HTTP status, Problem title). ``type`` is
#: ``gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~``.
CATEGORIES: dict[str, tuple[int, str]] = {
    "cancelled": (499, "Cancelled"),
    "unknown": (500, "Unknown"),
    "invalid_argument": (400, "Invalid argument"),
    "deadline_exceeded": (504, "Deadline exceeded"),
    "not_found": (404, "Not found"),
    "already_exists": (409, "Already exists"),
    "permission_denied": (403, "Permission denied"),
    "resource_exhausted": (429, "Resource exhausted"),
    "failed_precondition": (400, "Failed precondition"),
    "aborted": (409, "Aborted"),
    "out_of_range": (400, "Out of range"),
    "unimplemented": (501, "Unimplemented"),
    "internal": (500, "Internal"),
    "service_unavailable": (503, "Service unavailable"),
    "data_loss": (500, "Data loss"),
    "unauthenticated": (401, "Unauthenticated"),
}
_TITLE_TO_CATEGORY = {title.lower(): name for name, (_, title) in CATEGORIES.items()}
_TYPE_RE = re.compile(r"cf\.core\.err\.([a-z_]+)\.v1~")

# Resource types of the mini-chat ``not_found`` Problems (DESIGN §3.3).
RT_CHAT = "gts.cf.core.mini_chat.chat.v1~"
RT_MESSAGE = "gts.cf.core.mini_chat.message.v1~"
RT_TURN = "gts.cf.core.mini_chat.turn.v1~"
RT_ATTACHMENT = "gts.cf.core.mini_chat.attachment.v1~"
RT_MODEL = "gts.cf.core.mini_chat.model.v1~"
RT_ODATA_QUERY = "gts.cf.core.odata.query.v1~"


def problem_type_uri(category: str) -> str:
    """Canonical ``type`` URI of a Problem category."""
    return f"gts://gts.cf.core.errors.err.v1~cf.core.err.{category}.v1~"


def problem_category(problem: dict[str, Any]) -> str | None:
    """Infer the canonical category from a Problem's ``type`` (or ``title``)."""
    match = _TYPE_RE.search(str(problem.get("type", "")))
    if match:
        return match.group(1)
    return _TITLE_TO_CATEGORY.get(str(problem.get("title", "")).strip().lower())


def _response_json(resp: httpx.Response) -> Any:
    try:
        return resp.json()
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise AssertionError(
            f"expected a JSON body (HTTP {resp.status_code}), got: {resp.text[:500]!r}"
        ) from exc


def assert_problem(
    resp: httpx.Response,
    status: int,
    category: str | None = None,
    reason: str | None = None,
    field_reason: str | None = None,
    field_name: str | None = None,
    violation_subject: str | None = None,
    violation_type: str | None = None,
    resource_type: str | None = None,
    resource_name: str | None = None,
) -> dict[str, Any]:
    """Assert ``resp`` is a canonical RFC 9457 Problem (ADR-0004) and return it.

    * HTTP status equals ``status`` and the body's ``status``;
    * ``type``, ``title``, ``status``, ``detail`` and ``context`` are present,
      there is no top-level ``code``;
    * the category inferred from ``type``/``title`` equals ``category`` and
      its canonical status equals ``status``;
    * ``reason`` -> ``context.reason``;
    * ``field_reason``/``field_name`` -> an entry of ``context.field_violations``;
    * ``violation_subject``/``violation_type`` -> an entry of ``context.violations``;
    * ``resource_type``/``resource_name`` -> ``context.resource_type``/``resource_name``.
    """
    body = _response_json(resp)
    assert resp.status_code == status, (
        f"expected HTTP {status}, got {resp.status_code}: {json.dumps(body)[:1000]}"
    )
    assert isinstance(body, dict), f"Problem body must be an object, got {body!r}"
    content_type = resp.headers.get("content-type", "")
    assert "json" in content_type, f"Problem content-type must be JSON, got {content_type!r}"
    for key in ("type", "title", "status", "detail", "context"):
        assert key in body, f"Problem is missing {key!r}: {body}"
    assert "code" not in body, f"Problem must not carry a top-level 'code' (ADR-0004): {body}"
    assert body["status"] == status, f"Problem.status {body['status']} != HTTP {status}"
    assert isinstance(body["context"], dict), f"Problem.context must be an object: {body}"

    inferred = problem_category(body)
    if category is not None:
        assert inferred == category, (
            f"expected category {category!r}, got {inferred!r} (type={body['type']!r}, "
            f"title={body['title']!r})"
        )
        assert CATEGORIES[category][0] == status, (
            f"category {category!r} maps to HTTP {CATEGORIES[category][0]}, not {status}"
        )

    context = body["context"]
    if reason is not None:
        assert context.get("reason") == reason, (
            f"expected context.reason {reason!r}, got {context.get('reason')!r}: {body}"
        )
    if field_reason is not None or field_name is not None:
        violations = context.get("field_violations") or []
        assert any(
            (field_reason is None or v.get("reason") == field_reason)
            and (field_name is None or v.get("field") == field_name)
            for v in violations
        ), f"no field violation field={field_name!r} reason={field_reason!r} in {violations}"
    if violation_subject is not None or violation_type is not None:
        violations = context.get("violations") or []
        assert any(
            (violation_subject is None or v.get("subject") == violation_subject)
            and (violation_type is None or v.get("type") == violation_type)
            for v in violations
        ), f"no violation subject={violation_subject!r} type={violation_type!r} in {violations}"
    if resource_type is not None:
        assert context.get("resource_type") == resource_type, (
            f"expected context.resource_type {resource_type!r}, got "
            f"{context.get('resource_type')!r}: {body}"
        )
    if resource_name is not None:
        assert context.get("resource_name") == resource_name, (
            f"expected context.resource_name {resource_name!r}, got "
            f"{context.get('resource_name')!r}: {body}"
        )
    return body


def retry_after_seconds(resp: httpx.Response) -> int | None:
    """``Retry-After`` header as an integer, or ``None`` when absent."""
    value = resp.headers.get("retry-after")
    return int(value) if value is not None else None


# --------------------------------------------------------------------------
# SSE
# --------------------------------------------------------------------------


@dataclass
class SseResult:
    """Outcome of an SSE request.

    ``events`` is a list of ``(event, data)`` tuples where ``data`` is the
    parsed JSON payload (or the raw string when it is not JSON). For a
    non-SSE answer (a JSON error before the stream opened) ``events`` is empty
    and ``body`` holds the parsed JSON, ``response`` the full response.
    """

    status_code: int
    headers: httpx.Headers
    events: list[tuple[str, Any]] = field(default_factory=list)
    body: Any = None
    text: str = ""
    response: httpx.Response | None = None
    disconnected_early: bool = False
    transport_error: str | None = None

    @property
    def is_sse(self) -> bool:
        return self.headers.get("content-type", "").startswith("text/event-stream")

    def names(self) -> list[str]:
        return [name for name, _ in self.events]

    def of(self, name: str) -> list[Any]:
        """Payloads of every event named ``name``."""
        return [data for event, data in self.events if event == name]

    def first(self, name: str) -> Any:
        found = self.of(name)
        assert found, f"no {name!r} event in stream: {self.names()}"
        return found[0]

    @property
    def terminal(self) -> tuple[str, Any] | None:
        """The last event when it is ``done`` or ``error``."""
        if self.events and self.events[-1][0] in ("done", "error"):
            return self.events[-1]
        return None

    def text_content(self) -> str:
        """Concatenated text of all ``delta`` events."""
        return "".join(
            str(d.get("content", "")) for d in self.of("delta") if isinstance(d, dict)
        )


def parse_sse_lines(lines: Iterable[str]) -> Iterator[tuple[str, Any]]:
    """Parse SSE lines into ``(event, data)`` tuples (WHATWG event-stream rules).

    Comment lines (``:``) are ignored, multi-line ``data`` is joined with
    ``\\n``, the default event name is ``message`` and ``data`` is JSON-decoded
    when possible.
    """
    event_name: str | None = None
    data_lines: list[str] = []
    for raw in lines:
        line = raw.rstrip("\r")
        if line == "":
            if data_lines or event_name is not None:
                yield _sse_event(event_name, data_lines)
            event_name, data_lines = None, []
            continue
        if line.startswith(":"):
            continue
        name, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if name == "event":
            event_name = value
        elif name == "data":
            data_lines.append(value)
    if data_lines or event_name is not None:
        yield _sse_event(event_name, data_lines)


def _sse_event(event_name: str | None, data_lines: list[str]) -> tuple[str, Any]:
    payload = "\n".join(data_lines)
    try:
        data: Any = json.loads(payload) if payload else None
    except json.JSONDecodeError:
        data = payload
    return (event_name or "message", data)


def stream_sse(
    client: httpx.Client,
    method: str,
    url: str,
    *,
    max_events: int | None = None,
    stop_when: Callable[[str, Any], bool] | None = None,
    timeout: float | httpx.Timeout = 60.0,
    **kwargs: Any,
) -> SseResult:
    """Send a request and read its SSE stream.

    Reading stops (and the connection is closed, i.e. the client disconnects
    early) after ``max_events`` events or as soon as ``stop_when(event, data)``
    returns true. Without a limit the stream is read to EOF.
    """
    with client.stream(method, url, timeout=timeout, **kwargs) as resp:
        result = SseResult(status_code=resp.status_code, headers=resp.headers)
        if not result.is_sse:
            resp.read()
            result.text = resp.text
            result.response = resp
            try:
                result.body = resp.json()
            except (json.JSONDecodeError, UnicodeDecodeError):
                result.body = None
            return result
        try:
            for event in parse_sse_lines(resp.iter_lines()):
                result.events.append(event)
                if max_events is not None and len(result.events) >= max_events:
                    result.disconnected_early = True
                    break
                if stop_when is not None and stop_when(*event):
                    result.disconnected_early = True
                    break
        except (httpx.RemoteProtocolError, httpx.ReadError) as exc:
            result.transport_error = f"{type(exc).__name__}: {exc}"
        result.response = resp
    return result


# --------------------------------------------------------------------------
# UUIDs and SQLite
# --------------------------------------------------------------------------


def new_uuid() -> str:
    return str(uuid.uuid4())


def is_uuid(value: Any) -> bool:
    try:
        uuid.UUID(str(value))
    except (TypeError, ValueError, AttributeError):
        return False
    return True


def uuid_bytes(value: str | uuid.UUID) -> bytes:
    """16-byte form of a UUID, as mini-chat stores it in SQLite."""
    return uuid.UUID(str(value)).bytes


def uuid_from_db(value: Any) -> str | None:
    """Normalize a UUID column value (16-byte BLOB or text) to its string form."""
    if value is None:
        return None
    if isinstance(value, (bytes, bytearray, memoryview)):
        raw = bytes(value)
        if len(raw) == 16:
            return str(uuid.UUID(bytes=raw))
        return str(uuid.UUID(raw.decode("ascii")))
    return str(uuid.UUID(str(value)))


def _normalize_value(column: str, value: Any) -> Any:
    if isinstance(value, (bytes, bytearray, memoryview)) and len(bytes(value)) == 16:
        if column == "id" or column.endswith("_id"):
            return uuid_from_db(value)
    return value


def db_query(conn: sqlite3.Connection, sql: str, params: Iterable[Any] = ()) -> list[dict[str, Any]]:
    """Run ``sql`` and return rows as dicts.

    UUID parameters (``uuid.UUID`` objects) are bound as their 16-byte form.
    16-byte BLOBs in ``id`` / ``*_id`` columns are returned as UUID strings.
    """
    bound = [p.bytes if isinstance(p, uuid.UUID) else p for p in params]
    cursor = conn.execute(sql, bound)
    columns = [d[0] for d in cursor.description or ()]
    return [
        {col: _normalize_value(col, val) for col, val in zip(columns, row)}
        for row in cursor.fetchall()
    ]


def db_one(conn: sqlite3.Connection, sql: str, params: Iterable[Any] = ()) -> dict[str, Any] | None:
    """First row of :func:`db_query` or ``None``."""
    rows = db_query(conn, sql, params)
    return rows[0] if rows else None


def db_uuid(value: str | uuid.UUID) -> uuid.UUID:
    """Wrap a UUID string so :func:`db_query` binds it as 16 bytes."""
    return uuid.UUID(str(value))


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", (name,)
    ).fetchone()
    return row is not None


# --------------------------------------------------------------------------
# Polling
# --------------------------------------------------------------------------


def wait_until(
    probe: Callable[[], T],
    timeout: float = 30.0,
    interval: float = 0.25,
    description: str = "condition",
) -> T:
    """Call ``probe`` until it returns a truthy value and return that value.

    Exceptions raised by ``probe`` are retried until the deadline; the last
    one is chained to the timeout ``AssertionError``.
    """
    deadline = time.monotonic() + timeout
    last_value: Any = None
    last_error: BaseException | None = None
    while True:
        try:
            last_value = probe()
            last_error = None
            if last_value:
                return last_value
        except Exception as exc:  # noqa: BLE001 - retried until the deadline
            last_error = exc
        if time.monotonic() >= deadline:
            message = f"timed out after {timeout}s waiting for {description}; last value: {last_value!r}"
            if last_error is not None:
                raise AssertionError(message) from last_error
            raise AssertionError(message)
        time.sleep(interval)


# --------------------------------------------------------------------------
# Config patches
# --------------------------------------------------------------------------


class _Delete:
    """Sentinel: a ``deep_merge`` patch value that removes the key."""

    def __repr__(self) -> str:
        return "DELETE"

    def __deepcopy__(self, memo: dict[int, Any]) -> "_Delete":
        return self


DELETE: Any = _Delete()


def deep_merge(base: dict[str, Any], patch: dict[str, Any]) -> dict[str, Any]:
    """Return a copy of ``base`` with ``patch`` merged in recursively.

    Nested dicts are merged; any other value (lists included) replaces the
    base value; :data:`DELETE` removes the key.
    """
    result = copy.deepcopy(base)
    for key, value in patch.items():
        if value is DELETE:
            result.pop(key, None)
        elif isinstance(value, dict):
            current = result.get(key)
            result[key] = deep_merge(current if isinstance(current, dict) else {}, value)
        else:
            result[key] = copy.deepcopy(value)
    return result
