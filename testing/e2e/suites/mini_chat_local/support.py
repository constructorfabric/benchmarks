"""Small, dependency-light helpers shared by the mini-chat black-box tests."""

from __future__ import annotations

import base64
import json
import re
import sqlite3
import time
import uuid
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, Iterator, List, Optional, Sequence
from urllib.parse import quote

import httpx

URL_PREFIX = "/mini-chat"

# Credstore secret the mock provider's API-key auth reads (providers.openai.auth_config.secret_ref).
OPENAI_SECRET_KEY = "openai-key"
OPENAI_SECRET_VALUE = "sk-e2e-mock-provider-key"

# Identities of the static_tokens users written into the generated server config
# (same subjects/tenants as config/e2e-local.yaml).
TENANT_A_ID = "00000000-df51-5b42-9538-d2b56b7ee953"
TENANT_B_ID = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
USER_A_ID = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_A_REVIEWER_ID = "44444444-6a88-4768-9dfc-6bcd5187d9ed"
USER_B_ID = "22222222-6a88-4768-9dfc-6bcd5187d9ed"

TOKENS: Dict[str, str] = {
    "a": "e2e-token-tenant-a",
    "a_reviewer": "e2e-token-tenant-a-reviewer",
    "b": "e2e-token-tenant-b",
}
USER_IDS: Dict[str, str] = {"a": USER_A_ID, "a_reviewer": USER_A_REVIEWER_ID, "b": USER_B_ID}
TENANT_IDS: Dict[str, str] = {"a": TENANT_A_ID, "a_reviewer": TENANT_A_ID, "b": TENANT_B_ID}

# GTS resource types named by `context.resource_type` of 404 problems (ADR-0004).
RESOURCE_TYPES = {
    kind: f"gts.cf.core.mini_chat.{kind}.v1~"
    for kind in ("chat", "message", "turn", "attachment", "model")
}


def bearer(user: str = "a") -> Dict[str, str]:
    """``Authorization`` header of one of the configured users (``a``, ``a_reviewer``, ``b``)."""
    return {"Authorization": f"Bearer {TOKENS[user]}"}


class MiniChatApi:
    """Thin ``httpx.Client`` wrapper: paths are relative to the gear URL prefix.

    ``api.get("/v1/models")`` calls ``GET {base}/mini-chat/v1/models`` as user ``a``.
    """

    def __init__(self, base_url: str, prefix: str = URL_PREFIX, timeout: float = 5.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.prefix = prefix
        self._client = httpx.Client(base_url=self.base_url, timeout=timeout)

    def close(self) -> None:
        self._client.close()

    def url(self, path: str) -> str:
        return f"{self.prefix}{path}"

    def request(self, method: str, path: str, *, user: Optional[str] = "a",
                headers: Optional[Dict[str, str]] = None, raw_path: bool = False,
                **kwargs: Any) -> httpx.Response:
        """Send a request; ``user=None`` sends it without credentials, ``raw_path=True``
        skips the URL prefix (for asserting that nothing is served outside it)."""
        merged: Dict[str, str] = {}
        if user is not None:
            merged.update(bearer(user))
        merged.update(headers or {})
        return self._client.request(method, path if raw_path else self.url(path),
                                    headers=merged, **kwargs)

    def get(self, path: str, **kwargs: Any) -> httpx.Response:
        return self.request("GET", path, **kwargs)

    def post(self, path: str, **kwargs: Any) -> httpx.Response:
        return self.request("POST", path, **kwargs)

    def put(self, path: str, **kwargs: Any) -> httpx.Response:
        return self.request("PUT", path, **kwargs)

    def patch(self, path: str, **kwargs: Any) -> httpx.Response:
        return self.request("PATCH", path, **kwargs)

    def delete(self, path: str, **kwargs: Any) -> httpx.Response:
        return self.request("DELETE", path, **kwargs)


# --------------------------------------------------------------------------- #
# RFC 9457 problems (ADR-0004)
# --------------------------------------------------------------------------- #

PROBLEM_JSON = "application/problem+json"


def assert_problem(resp: httpx.Response, status: int, category: str) -> Dict[str, Any]:
    """Assert the canonical error envelope and return the parsed body.

    ``category`` is the canonical category name (``not_found``, ``permission_denied``,
    ``unauthenticated``, ...): it appears in the problem ``type`` URI.
    """
    assert resp.status_code == status, f"expected {status}, got {resp.status_code}: {resp.text}"
    ctype = resp.headers.get("content-type", "")
    assert ctype.startswith(PROBLEM_JSON), f"errors are {PROBLEM_JSON}, got {ctype!r}"
    body = resp.json()
    assert isinstance(body, dict), body
    for key in ("type", "title", "status", "detail", "context"):
        assert key in body, f"problem lacks `{key}`: {body}"
    assert "code" not in body, f"ADR-0004: no top-level `code` field: {body}"
    assert body["status"] == status
    assert f"cf.core.err.{category}.v1" in body["type"], body["type"]
    return body


def field_violation_reasons(problem: Dict[str, Any]) -> List[str]:
    return [v.get("reason") for v in problem["context"].get("field_violations", [])]


def assert_field_violation(problem: Dict[str, Any], field: str, reason: str) -> None:
    violations = problem["context"].get("field_violations", [])
    assert any(v.get("field") == field and v.get("reason") == reason for v in violations), \
        f"expected field_violations[{field}].reason = {reason}, got {violations}"


def assert_precondition(problem: Dict[str, Any], subject: str, kind: str) -> None:
    violations = problem["context"].get("violations", [])
    assert any(v.get("subject") == subject and v.get("type") == kind for v in violations), \
        f"expected violations[{{subject: {subject}, type: {kind}}}], got {violations}"


def assert_conflict(resp: httpx.Response, reason: str) -> Dict[str, Any]:
    """409 `aborted` problem with `context.reason`."""
    body = assert_problem(resp, 409, "aborted")
    assert body["context"].get("reason") == reason, body
    return body


# --------------------------------------------------------------------------- #
# Server-sent events (used from the streaming slice on)
# --------------------------------------------------------------------------- #


@dataclass
class SseEvent:
    event: str
    data: Any
    raw_data: str


def parse_sse(text: str) -> List[SseEvent]:
    """Parse an SSE body into events (comment/keep-alive frames are skipped)."""
    events: List[SseEvent] = []
    for frame in text.replace("\r\n", "\n").split("\n\n"):
        name = "message"
        data_lines: List[str] = []
        for line in frame.split("\n"):
            if line.startswith(":") or not line:
                continue
            field, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if field == "event":
                name = value
            elif field == "data":
                data_lines.append(value)
        if not data_lines:
            continue
        raw = "\n".join(data_lines)
        try:
            parsed: Any = json.loads(raw)
        except ValueError:
            parsed = raw
        events.append(SseEvent(event=name, data=parsed, raw_data=raw))
    return events


def _event(name: str, data_lines: List[str]) -> SseEvent:
    raw = "\n".join(data_lines)
    try:
        parsed: Any = json.loads(raw)
    except ValueError:
        parsed = raw
    return SseEvent(event=name, data=parsed, raw_data=raw)


def iter_sse(lines: Iterable[str]) -> Iterator[SseEvent]:
    """Incrementally parse SSE text lines into events (comment frames are skipped)."""
    name = "message"
    data: List[str] = []
    for line in lines:
        if line == "":
            if data:
                yield _event(name, data)
            name, data = "message", []
            continue
        if line.startswith(":"):
            continue
        field, _, value = line.partition(":")
        value = value[1:] if value.startswith(" ") else value
        if field == "event":
            name = value
        elif field == "data":
            data.append(value)
    if data:
        yield _event(name, data)


class LiveStream:
    """An open ``messages:stream`` response read incrementally.

    ``next_event()`` blocks until the next event arrives (``None`` at the end of the stream);
    ``close()`` drops the connection, i.e. disconnects the client. A JSON rejection is an
    ordinary response: ``is_sse`` is false and ``read()`` returns it for ``assert_problem``.
    """

    def __init__(self, cm: Any, response: httpx.Response) -> None:
        self._cm = cm
        self.response = response
        self.status_code = response.status_code
        self.headers = response.headers
        self._events: Optional[Iterator[SseEvent]] = None
        self._closed = False

    @property
    def is_sse(self) -> bool:
        return (self.status_code == 200
                and self.headers.get("content-type", "").startswith("text/event-stream"))

    def next_event(self) -> Optional[SseEvent]:
        if self._events is None:
            if not self.is_sse:
                self.response.read()
                raise AssertionError(
                    f"expected an SSE stream, got {self.status_code}: {self.response.text}")
            self._events = iter_sse(self.response.iter_lines())
        return next(self._events, None)

    def expect(self, name: str) -> SseEvent:
        event = self.next_event()
        assert event is not None, f"stream ended while waiting for `{name}`"
        assert event.event == name, f"expected `{name}`, got {event}"
        return event

    def next_non_ping(self) -> Optional[SseEvent]:
        while True:
            event = self.next_event()
            if event is None or event.event != "ping":
                return event

    def drain(self) -> List[SseEvent]:
        out: List[SseEvent] = []
        while True:
            event = self.next_event()
            if event is None:
                return out
            out.append(event)

    def read(self) -> httpx.Response:
        """Read a non-SSE (problem) body completely and return the response."""
        self.response.read()
        self.close()
        return self.response

    def close(self) -> None:
        if not self._closed:
            self._closed = True
            self._cm.__exit__(None, None, None)

    def __enter__(self) -> "LiveStream":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


@dataclass
class Sent:
    """A fully read SSE answer."""

    status_code: int
    headers: Any
    events: List[SseEvent]

    def names(self) -> List[str]:
        return [e.event for e in self.events]

    def names_no_ping(self) -> List[str]:
        return [e.event for e in self.events if e.event != "ping"]

    def of(self, name: str) -> List[SseEvent]:
        return [e for e in self.events if e.event == name]

    @property
    def started(self) -> Dict[str, Any]:
        assert self.events and self.events[0].event == "stream_started", self.names()
        return self.events[0].data

    @property
    def request_id(self) -> str:
        return self.started["request_id"]

    @property
    def message_id(self) -> str:
        return self.started["message_id"]

    @property
    def done(self) -> Dict[str, Any]:
        found = self.of("done")
        assert found, f"no done event in {self.names()}: {self.events}"
        return found[0].data

    @property
    def error(self) -> Dict[str, Any]:
        found = self.of("error")
        assert found, f"no error event in {self.names()}: {self.events}"
        return found[0].data

    @property
    def terminal(self) -> SseEvent:
        return self.events[-1]

    def text(self) -> str:
        return "".join(e.data.get("content", "") for e in self.of("delta"))

    def blob(self) -> str:
        return "\n".join(f"{e.event}:{e.raw_data}" for e in self.events)

    def assert_well_formed(self) -> None:
        """`stream_started ping* (delta|tool)* citations? (done|error)`."""
        names = self.names()
        assert names and names[0] == "stream_started", names
        terminals = [n for n in names if n in ("done", "error")]
        assert len(terminals) == 1, f"exactly one terminal event: {names}"
        assert names[-1] in ("done", "error"), f"the terminal event is last: {names}"
        assert names.count("citations") <= 1, names
        if "citations" in names:
            assert names.index("citations") == len(names) - 2, \
                f"citations come right before the terminal event: {names}"
        content = [i for i, n in enumerate(names) if n in ("delta", "tool")]
        if content:
            assert "ping" not in names[content[0]:], f"no ping after content started: {names}"


def read_all(live: LiveStream) -> Sent:
    assert live.is_sse, f"expected an SSE stream, got {live.status_code} {live.headers}"
    try:
        events = live.drain()
    finally:
        live.close()
    return Sent(status_code=live.status_code, headers=live.headers, events=events)


def open_stream(api: "MiniChatApi", chat_id: str, body: Any, *, user: Optional[str] = "a",
                headers: Optional[Dict[str, str]] = None, content: Optional[str] = None,
                path: Optional[str] = None, timeout: float = 60.0) -> LiveStream:
    """``POST messages:stream``; ``body`` is JSON-encoded unless ``content`` (raw text) is given."""
    merged: Dict[str, str] = {}
    if user is not None:
        merged.update(bearer(user))
    merged.update(headers or {})
    kwargs: Dict[str, Any] = {}
    if content is not None:
        kwargs["content"] = content
    else:
        kwargs["json"] = body
    cm = api._client.stream(  # noqa: SLF001 - the suite's own client
        "POST", path or api.url(f"/v1/chats/{chat_id}/messages:stream"), headers=merged,
        timeout=httpx.Timeout(timeout, connect=5.0), **kwargs)
    response = cm.__enter__()
    return LiveStream(cm, response)


def send_message(api: "MiniChatApi", chat_id: str, content: str = "hello", *,
                 request_id: Optional[str] = None, user: Optional[str] = "a",
                 **extra: Any) -> Sent:
    """Send a message and read the whole SSE answer."""
    body: Dict[str, Any] = {"content": content, **extra}
    if request_id is not None:
        body["request_id"] = request_id
    return read_all(open_stream(api, chat_id, body, user=user))


def reject_message(api: "MiniChatApi", chat_id: str, body: Any, *, user: Optional[str] = "a",
                   **kwargs: Any) -> httpx.Response:
    """Send a request that must be rejected before the stream opens (JSON problem)."""
    live = open_stream(api, chat_id, body, user=user, **kwargs)
    assert not live.is_sse, f"expected a JSON rejection, got an SSE stream: {live.headers}"
    return live.read()


def get_turn(api: "MiniChatApi", chat_id: str, request_id: str, *,
             user: Optional[str] = "a") -> httpx.Response:
    return api.get(f"/v1/chats/{chat_id}/turns/{request_id}", user=user)


def wait_for(predicate: Callable[[], Any], what: str, timeout: float = 15.0,
             interval: float = 0.05) -> Any:
    """Poll ``predicate`` until it returns a truthy value (returned); fail after ``timeout``."""
    deadline = time.monotonic() + timeout
    last: Any = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out after {timeout}s waiting for {what} (last: {last!r})")


# --------------------------------------------------------------------------- #
# SQLite access to the gear database
# --------------------------------------------------------------------------- #


def uuid_blob(value: str) -> bytes:
    """UUID columns hold the 16 raw bytes (DESIGN 3.7), not the text form."""
    return uuid.UUID(value).bytes


# --------------------------------------------------------------------------- #
# Quota periods (DESIGN 3.2 "Quota Period Reset Semantics": UTC calendar day / month)
# --------------------------------------------------------------------------- #


def period_start(period: str, now: Optional[datetime] = None) -> str:
    """``quota_usage.period_start`` text of the period of ``now`` (UTC): the day, or the 1st."""
    now = (now or datetime.now(timezone.utc)).astimezone(timezone.utc)
    return f"{now.year:04d}-{now.month:02d}-{1 if period == 'monthly' else now.day:02d}"


def previous_period_start(period: str) -> str:
    """Start of the period before the current one (yesterday / the 1st of last month)."""
    now = datetime.now(timezone.utc)
    if period == "monthly":
        last = now.replace(day=1) - timedelta(days=1)
        return f"{last.year:04d}-{last.month:02d}-01"
    y = now - timedelta(days=1)
    return f"{y.year:04d}-{y.month:02d}-{y.day:02d}"


def next_reset(period: str, now: Optional[datetime] = None) -> datetime:
    """Next UTC midnight (daily) / midnight of the 1st of next month (monthly)."""
    now = (now or datetime.now(timezone.utc)).astimezone(timezone.utc)
    if period == "monthly":
        year, month = (now.year + 1, 1) if now.month == 12 else (now.year, now.month + 1)
        return datetime(year, month, 1, tzinfo=timezone.utc)
    return datetime(now.year, now.month, now.day, tzinfo=timezone.utc) + timedelta(days=1)


def dedupe_key(tenant: str, turn: str, request: str) -> str:
    """DESIGN 5.6: ``{tenant}/{turn}/{request}`` as 32 lowercase hex characters each."""
    return "/".join(uuid.UUID(v).hex for v in (tenant, turn, request))



class GearDb:
    """Read-only view of the mini-chat SQLite database file."""

    def __init__(self, path: Path) -> None:
        self.path = path

    def _connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(f"file:{self.path}?mode=ro", uri=True, timeout=5.0)
        conn.row_factory = sqlite3.Row
        return conn

    def query(self, sql: str, params: Sequence[Any] = ()) -> List[sqlite3.Row]:
        conn = self._connect()
        try:
            return list(conn.execute(sql, tuple(params)).fetchall())
        finally:
            conn.close()

    def scalar(self, sql: str, params: Sequence[Any] = ()) -> Any:
        rows = self.query(sql, params)
        return rows[0][0] if rows else None

    def tables(self) -> List[str]:
        return [r["name"] for r in self.query(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")]

    def columns(self, table: str) -> List[str]:
        return [r["name"] for r in self.query(f"PRAGMA table_info({table})")]

    def count(self, table: str, where: str = "1=1", params: Sequence[Any] = ()) -> int:
        return int(self.scalar(f"SELECT COUNT(*) FROM {table} WHERE {where}", params))

    def iter_rows(self, table: str) -> Iterator[sqlite3.Row]:
        yield from self.query(f"SELECT * FROM {table}")

    # ---- streaming slice: turns and ordered messages ------------------------- #

    def turns_of(self, chat_id: str) -> List[sqlite3.Row]:
        return self.query("SELECT * FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id",
                          (uuid_blob(chat_id),))

    def turn(self, chat_id: str, request_id: str) -> Optional[sqlite3.Row]:
        rows = self.query("SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
                          (uuid_blob(chat_id), uuid_blob(request_id)))
        return rows[0] if rows else None

    def ordered_messages(self, chat_id: str) -> List[sqlite3.Row]:
        """Every message of the chat (soft-deleted included) in `(created_at, id)` order."""
        return self.query("SELECT * FROM messages WHERE chat_id = ? ORDER BY created_at, id",
                          (uuid_blob(chat_id),))

    def counts(self, chat_id: str) -> Dict[str, int]:
        """Rows of the chat in every table a rejected request must not touch."""
        blob = uuid_blob(chat_id)
        return {
            "messages": self.count("messages", "chat_id = ?", (blob,)),
            "turns": self.count("chat_turns", "chat_id = ?", (blob,)),
            "links": self.count("message_attachments", "chat_id = ?", (blob,)),
        }

    def outbox_payload_count(self) -> int:
        if "toolkit_outbox_body" not in self.tables():
            return 0
        return int(self.scalar("SELECT COUNT(*) FROM toolkit_outbox_body"))

    # ---- quota slice: buckets and outbox counters ---------------------------- #

    def quota_rows(self, user: Optional[str] = None) -> List[sqlite3.Row]:
        """Every ``quota_usage`` row (of ``user`` when given), ordered by key."""
        sql = "SELECT * FROM quota_usage"
        params: Sequence[Any] = ()
        if user is not None:
            sql += " WHERE user_id = ?"
            params = (uuid_blob(user),)
        return self.query(sql + " ORDER BY bucket, period_type, period_start", params)

    def quota_row(self, user: str, bucket: str, period: str, *,
                  stale: bool = False) -> Optional[sqlite3.Row]:
        """The row of (``bucket``, ``period``) for the current period (``stale``: the previous one)."""
        start = previous_period_start(period) if stale else period_start(period)
        rows = self.query(
            "SELECT * FROM quota_usage WHERE user_id = ? AND bucket = ? AND period_type = ? "
            "AND period_start = ?", (uuid_blob(user), bucket, period, start))
        return rows[0] if rows else None

    def quota_amounts(self, user: str, bucket: str, period: str) -> "tuple[int, int]":
        """``(spent, reserved)`` credits of a bucket row; ``(0, 0)`` without a row."""
        row = self.quota_row(user, bucket, period)
        return (0, 0) if row is None else (row["spent_credits_micro"], row["reserved_credits_micro"])

    def outbox_sequenced(self, queue: str) -> int:
        """Messages ever enqueued on ``queue`` (the sum of its partitions' sequence counters; the
        counters survive the vacuum, so this is exact even after delivery)."""
        if "toolkit_outbox_partitions" not in self.tables():
            return 0
        return int(self.scalar(
            "SELECT COALESCE(SUM(sequence), 0) FROM toolkit_outbox_partitions WHERE queue = ?",
            (queue,)))

    def outbox_queues(self) -> Dict[str, int]:
        """``{queue: partition count}`` of the queues registered in the shared outbox."""
        if "toolkit_outbox_partitions" not in self.tables():
            return {}
        return {r["queue"]: r["n"] for r in self.query(
            "SELECT queue, COUNT(*) AS n FROM toolkit_outbox_partitions GROUP BY queue")}

    def dead_letter_count(self) -> int:
        if "toolkit_outbox_dead_letters" not in self.tables():
            return 0
        return int(self.scalar("SELECT COUNT(*) FROM toolkit_outbox_dead_letters"))


def uuid_text(blob: Any) -> Optional[str]:
    """Text form of a UUID column value (16 raw bytes on SQLite)."""
    return str(uuid.UUID(bytes=bytes(blob))) if blob is not None else None


def names(rows: Iterable[Dict[str, Any]], key: str) -> List[str]:
    return [r[key] for r in rows]


# --------------------------------------------------------------------------- #
# Seeding the gear database (sending messages belongs to a later slice, so the
# message / attachment / reaction rows the S2 tests read are inserted directly)
# --------------------------------------------------------------------------- #

# Fixed WebP-like bytes stored as an image thumbnail; the API returns them base64 encoded.
THUMBNAIL_BYTES = b"RIFF\x00\x00\x00\x00WEBPVP8 e2e-thumbnail"
THUMBNAIL_WIDTH = 64
THUMBNAIL_HEIGHT = 48
DAY = 86_400


def iso(secs: int = 0) -> str:
    """``2025-01-01T00:00:00Z`` plus ``secs`` seconds (RFC 3339 wire/filter text; use ``stored_ts``
    for values written to the database)."""
    base = 1_735_689_600
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(base + secs))


def stored_ts(secs: int = 0, micros: int = 0) -> str:
    """Canonical stored text of ``2025-01-01T00:00:00Z`` + ``secs`` s + ``micros`` us.

    Every timestamp column of the gear database holds this encoding on SQLite: UTC with a
    ``+00:00`` offset and a fraction of 0, 3 or 6 digits (the shortest of the three that is
    exact), exactly the text the platform ``$filter`` / cursor binds, so plain text order is
    chronological order and equality is exact. Seeded rows MUST use it, never ``iso``.
    """
    base = 1_735_689_600 + secs + micros // 1_000_000
    micros %= 1_000_000
    text = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(base))
    if micros == 0:
        fraction = ""
    elif micros % 1000 == 0:
        fraction = f".{micros // 1000:03d}"
    else:
        fraction = f".{micros:06d}"
    return f"{text}{fraction}+00:00"


def wire_ts(secs: int = 0, micros: int = 0) -> str:
    """RFC 3339 spelling (``Z``, 6-digit fraction when not whole) of the same instant."""
    stored = stored_ts(secs, micros)
    return stored[: -len("+00:00")] + "Z"


# Five instants inside one second whose canonical stored text has different fraction widths
# (none, 6, 3, 6, none digits); strictly ascending as (seconds, microseconds) after the base.
MIXED_WIDTH_INSTANTS = [(10, 0), (10, 422_670), (10, 500_000), (10, 500_001), (11, 0)]


def parse_ts(value: str) -> datetime:
    """Parse an RFC 3339 timestamp (any fractional precision, ``Z`` or an offset)."""
    m = re.fullmatch(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)", value)
    assert m, f"not an RFC 3339 timestamp: {value!r}"
    base = datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S")
    micros = int((m.group(2) or "0")[:6].ljust(6, "0"))
    if m.group(3) == "Z":
        tz = timezone.utc
    else:
        sign = 1 if m.group(3)[0] == "+" else -1
        hours, minutes = int(m.group(3)[1:3]), int(m.group(3)[4:6])
        tz = timezone(sign * timedelta(hours=hours, minutes=minutes))
    return base.replace(microsecond=micros, tzinfo=tz)


def unique_tag() -> str:
    """A short token for titles, so a test finds its own chats in the shared session database."""
    return uuid.uuid4().hex[:10]


def uid(n: int) -> str:
    """Deterministic UUID text of ``n`` (byte order == numeric order)."""
    return str(uuid.UUID(int=n))


def qs(**params: Any) -> str:
    """Query string with OData keys (``filter`` -> ``$filter``) and RFC 3986 escaping."""
    parts = []
    for key, value in params.items():
        name = f"${key[1:]}" if key.startswith("_") else key
        parts.append(f"{name}={quote(str(value), safe=chr(39) + '(),$:')}")
    return "?" + "&".join(parts) if parts else ""


class GearSeeder(GearDb):
    """Read-write access to the mini-chat SQLite file for seeding fixtures.

    UUID columns are written as 16-byte blobs (DESIGN 3.7). Every ``insert_*`` returns the
    id (text) of the new row; reads use the inherited read-only helpers.
    """

    def _write(self, sql: str, params: Sequence[Any]) -> None:
        conn = sqlite3.connect(str(self.path), timeout=10.0)
        try:
            conn.execute("PRAGMA busy_timeout = 10000")
            conn.execute(sql, tuple(params))
            conn.commit()
        finally:
            conn.close()

    def insert_chat(self, *, tenant: str, user: str, title: Optional[str] = None,
                    model: str = "mock-premium", chat_id: Optional[str] = None,
                    created: int = 0, updated: Optional[int] = None,
                    created_micros: int = 0, updated_micros: Optional[int] = None,
                    deleted: bool = False) -> str:
        chat_id = chat_id or str(uuid.uuid4())
        if updated is None:
            updated = created
            if updated_micros is None:
                updated_micros = created_micros
        self._write(
            "INSERT INTO chats (id, tenant_id, user_id, model, title, is_temporary, "
            "created_at, updated_at, deleted_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?)",
            # An untitled chat is stored as the empty title, never NULL (title sort key).
            (uuid_blob(chat_id), uuid_blob(tenant), uuid_blob(user), model, title or "",
             stored_ts(created, created_micros), stored_ts(updated, updated_micros or 0),
             stored_ts(created + 1) if deleted else None))
        return chat_id

    def insert_message(self, *, tenant: str, chat: str, role: str, content: str = "hello",
                       message_id: Optional[str] = None, request_id: Optional[str] = "auto",
                       created: int = 0, created_micros: int = 0, model: Optional[str] = None,
                       input_tokens: int = 0,
                       output_tokens: int = 0, deleted: bool = False) -> str:
        message_id = message_id or str(uuid.uuid4())
        if request_id == "auto":
            request_id = str(uuid.uuid4())
        self._write(
            "INSERT INTO messages (id, tenant_id, chat_id, request_id, role, content, "
            "content_type, input_tokens, output_tokens, model, created_at, deleted_at) "
            "VALUES (?, ?, ?, ?, ?, ?, 'text', ?, ?, ?, ?, ?)",
            (uuid_blob(message_id), uuid_blob(tenant), uuid_blob(chat),
             uuid_blob(request_id) if request_id else None, role, content,
             input_tokens, output_tokens, model, stored_ts(created, created_micros),
             stored_ts(created + 1) if deleted else None))
        return message_id

    def insert_attachment(self, *, tenant: str, chat: str, user: str, filename: str,
                          kind: str = "document", status: str = "ready",
                          attachment_id: Optional[str] = None,
                          cleanup_status: Optional[str] = None, thumbnail: bool = False,
                          deleted: bool = False) -> str:
        attachment_id = attachment_id or str(uuid.uuid4())
        self._write(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, "
            "content_type, size_bytes, storage_backend, provider_file_id, status, "
            "attachment_kind, for_file_search, img_thumbnail, img_thumbnail_width, "
            "img_thumbnail_height, cleanup_status, created_at, updated_at, deleted_at) "
            "VALUES (?, ?, ?, ?, ?, ?, 1234, 'openai', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (uuid_blob(attachment_id), uuid_blob(tenant), uuid_blob(chat), uuid_blob(user),
             filename, "image/png" if kind == "image" else "application/pdf",
             f"file-{attachment_id}", status, kind, 0 if kind == "image" else 1,
             THUMBNAIL_BYTES if thumbnail else None,
             THUMBNAIL_WIDTH if thumbnail else None, THUMBNAIL_HEIGHT if thumbnail else None,
             cleanup_status, stored_ts(0), stored_ts(0), stored_ts(1) if deleted else None))
        return attachment_id

    def insert_turn(self, *, tenant: str, chat: str, request_id: str, state: str,
                    user: Optional[str] = None, error_code: Optional[str] = None,
                    assistant_message: Optional[str] = None, deleted: bool = False,
                    effective_model: str = "mock-premium", started: int = 10,
                    updated: int = 12, updated_micros: int = 345_678,
                    turn_id: Optional[str] = None) -> str:
        """A `chat_turns` row (`state` is the internal state: running / completed / failed /
        cancelled); terminal states get `completed_at = updated_at`."""
        turn_id = turn_id or str(uuid.uuid4())
        terminal = state != "running"
        updated_text = stored_ts(updated, updated_micros)
        self._write(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, "
            "requester_user_id, state, assistant_message_id, error_code, effective_model, "
            "deleted_at, started_at, last_progress_at, web_search_enabled, completed_at, "
            "updated_at) VALUES (?, ?, ?, ?, 'user', ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
            (uuid_blob(turn_id), uuid_blob(tenant), uuid_blob(chat), uuid_blob(request_id),
             uuid_blob(user) if user else None, state,
             uuid_blob(assistant_message) if assistant_message else None, error_code,
             effective_model, stored_ts(started + 1) if deleted else None, stored_ts(started),
             stored_ts(started), updated_text if terminal else None, updated_text))
        return turn_id

    def insert_vector_store(self, *, tenant: str, chat: str, vector_store_id: str,
                            provider: str = "openai") -> None:
        """The `chat_vector_stores` row that makes `file_search` available for the chat."""
        self._write(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, "
            "file_count, created_at) VALUES (?, ?, ?, ?, ?, 0, ?)",
            (uuid_blob(str(uuid.uuid4())), uuid_blob(tenant), uuid_blob(chat), vector_store_id,
             provider, stored_ts(0)))

    def insert_quota(self, *, tenant: str, user: str, bucket: str, period: str, spent: int = 0,
                     reserved: int = 0, calls: int = 0, input_tokens: int = 0,
                     output_tokens: int = 0, web_search_calls: int = 0,
                     code_interpreter_calls: int = 0, stale: bool = False) -> None:
        """A ``quota_usage`` row of the current period (``stale``: of the previous one).

        One row exists per ``(tenant, user, period type, period start, bucket)`` (DESIGN 3.7): when
        a turn already created it, the given amounts are added on top of it instead of failing the
        unique constraint.
        """
        self._write(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, "
            "spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens, "
            "web_search_calls, code_interpreter_calls) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) "
            "ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET "
            "spent_credits_micro = spent_credits_micro + excluded.spent_credits_micro, "
            "reserved_credits_micro = reserved_credits_micro + excluded.reserved_credits_micro, "
            "calls = calls + excluded.calls, "
            "input_tokens = input_tokens + excluded.input_tokens, "
            "output_tokens = output_tokens + excluded.output_tokens, "
            "web_search_calls = web_search_calls + excluded.web_search_calls, "
            "code_interpreter_calls = code_interpreter_calls + excluded.code_interpreter_calls",
            (uuid_blob(str(uuid.uuid4())), uuid_blob(tenant), uuid_blob(user), period,
             previous_period_start(period) if stale else period_start(period), bucket, spent,
             reserved, calls, input_tokens, output_tokens, web_search_calls,
             code_interpreter_calls))

    def clear_quota(self, user: Optional[str] = None) -> None:
        """Delete the ``quota_usage`` rows (of ``user`` when given) so a test starts from zero."""
        if user is None:
            self._write("DELETE FROM quota_usage", ())
        else:
            self._write("DELETE FROM quota_usage WHERE user_id = ?", (uuid_blob(user),))

    def link_attachment(self, *, tenant: str, chat: str, message: str, attachment: str) -> None:
        self._write(
            "INSERT INTO message_attachments (tenant_id, chat_id, message_id, attachment_id, "
            "created_at) VALUES (?, ?, ?, ?, ?)",
            (uuid_blob(tenant), uuid_blob(chat), uuid_blob(message), uuid_blob(attachment),
             stored_ts(0)))

    def insert_reaction(self, *, tenant: str, user: str, message: str, reaction: str) -> None:
        self._write(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, "
            "created_at) VALUES (?, ?, ?, ?, ?, ?)",
            (uuid_blob(str(uuid.uuid4())), uuid_blob(message), uuid_blob(user),
             uuid_blob(tenant), reaction, stored_ts(0)))

    # ---- reading rows back -------------------------------------------------- #

    def chat(self, chat_id: str) -> Optional[sqlite3.Row]:
        rows = self.query("SELECT * FROM chats WHERE id = ?", (uuid_blob(chat_id),))
        return rows[0] if rows else None

    def reactions(self, message_id: str) -> List[sqlite3.Row]:
        return self.query("SELECT * FROM message_reactions WHERE message_id = ?",
                          (uuid_blob(message_id),))

    def attachment(self, attachment_id: str) -> Optional[sqlite3.Row]:
        rows = self.query("SELECT * FROM attachments WHERE id = ?", (uuid_blob(attachment_id),))
        return rows[0] if rows else None

    def messages_of(self, chat_id: str) -> List[sqlite3.Row]:
        return self.query("SELECT * FROM messages WHERE chat_id = ?", (uuid_blob(chat_id),))

    def outbox_payloads(self) -> List[Dict[str, Any]]:
        """JSON payloads currently held by the shared outbox body table."""
        out: List[Dict[str, Any]] = []
        if "toolkit_outbox_body" not in self.tables():
            return out
        for row in self.query("SELECT payload FROM toolkit_outbox_body ORDER BY id DESC LIMIT 500"):
            try:
                out.append(json.loads(bytes(row["payload"]).decode("utf-8")))
            except (ValueError, UnicodeDecodeError):
                continue
        return out


def decode_thumbnail(thumbnail: Dict[str, Any]) -> bytes:
    return base64.b64decode(thumbnail["data_base64"])


def list_all(api: "MiniChatApi", path: str, *, user: str = "a", page_size: int = 100,
             **query: Any) -> List[Dict[str, Any]]:
    """Walk a cursor-paginated list endpoint and return every item.

    ``query`` carries OData options (``_filter`` -> ``$filter``); the filter is re-sent with
    every cursor, as a cursor is bound to the filter it was created with; ``_orderby`` is sent
    with the first page only.
    """
    items: List[Dict[str, Any]] = []
    cursor: Optional[str] = None
    for _ in range(1000):
        params = dict(query, limit=page_size)
        if cursor:
            params["cursor"] = cursor
            params.pop("_orderby", None)  # a cursor carries its order; sending both is a 400
        resp = api.get(path + qs(**params), user=user)
        assert resp.status_code == 200, resp.text
        page = resp.json()
        items.extend(page["items"])
        cursor = page["page_info"].get("next_cursor")
        if not cursor:
            return items
    raise AssertionError("pagination did not terminate")


# --------------------------------------------------------------------------- #
# The server log as evidence of outbox delivery (the bundled static plugins log what they get)
# --------------------------------------------------------------------------- #

USAGE_LOG_MESSAGE = "usage event received by the static model policy plugin"
AUDIT_LOG_MESSAGE = "mini-chat audit event"
_LOG_FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')


def log_events(log_path: Path, message: str) -> List[Dict[str, str]]:
    """``key=value`` fields of every server log line that carries ``message`` (quotes stripped)."""
    try:
        text = log_path.read_text(errors="replace")
    except OSError:
        return []
    events: List[Dict[str, str]] = []
    for line in text.splitlines():
        head, found, tail = line.partition(message)
        if not found:
            continue
        fields: Dict[str, str] = {}
        for key, value in _LOG_FIELD.findall(tail):
            fields[key] = value[1:-1] if value.startswith('"') and value.endswith('"') else value
        events.append(fields)
    return events


def usage_log(server: Any) -> List[Dict[str, str]]:
    """Usage events the static model-policy plugin logged (``publish_usage``)."""
    return log_events(server.log_path, USAGE_LOG_MESSAGE)


def audit_log(server: Any) -> List[Dict[str, str]]:
    """Audit events the static audit plugin logged (``emit``)."""
    return log_events(server.log_path, AUDIT_LOG_MESSAGE)
