"""Helpers shared by every mini-chat black-box e2e test (see README.md)."""

from __future__ import annotations

import json
import os
import sqlite3
import time
import uuid
from contextlib import closing, contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

import requests

PREFIX = "/mini-chat/v1"

TOKEN_A = "e2e-token-tenant-a"
TOKEN_A_REVIEWER = "e2e-token-tenant-a-reviewer"
TOKEN_B = "e2e-token-tenant-b"

#: Filled in by the ``server`` fixture (conftest.py); read by ``api()`` / ``db()``.
STATE: dict[str, Any] = {"base_url": None, "home": None}


class ApiSession(requests.Session):
    """``requests.Session`` with a base URL, a Bearer token and a default timeout."""

    def __init__(self, base_url: str, token: str | None) -> None:
        super().__init__()
        self.base_url = base_url.rstrip("/")
        if token:
            self.headers["Authorization"] = f"Bearer {token}"

    def request(self, method: str, url: str, **kw: Any):  # type: ignore[override]
        if url.startswith("/"):
            url = self.base_url + url
        kw.setdefault("timeout", 30)
        return super().request(method, url, **kw)


def api(token: str | None = TOKEN_A, base_url: str | None = None) -> ApiSession:
    """Session authenticated as ``token`` (``None`` = no Authorization header).

    Relative URLs are resolved against the running server, e.g.
    ``api().get("/mini-chat/v1/models")``.
    """
    return ApiSession(base_url or STATE["base_url"], token)


def create_chat(session: ApiSession, **kw: Any) -> dict[str, Any]:
    """``POST /chats`` (body = kwargs, e.g. ``model=``, ``title=``); asserts 201, returns the JSON."""
    r = session.post(f"{PREFIX}/chats", json=kw)
    assert r.status_code == 201, f"create_chat failed: {r.status_code} {r.text}"
    return r.json()


# ── SSE ─────────────────────────────────────────────────────────────────────


@dataclass
class SseResult:
    status: int
    #: ``(event name, parsed JSON data)``; ``data`` is the raw string when it is not JSON.
    events: list[tuple[str, Any]] = field(default_factory=list)
    raw: str = ""
    headers: dict[str, str] = field(default_factory=dict)

    def names(self) -> list[str]:
        return [n for n, _ in self.events]

    def of(self, name: str) -> list[Any]:
        return [d for n, d in self.events if n == name]

    @property
    def terminal(self) -> tuple[str, Any] | None:
        for n, d in reversed(self.events):
            if n in ("done", "error"):
                return n, d
        return None

    def text(self) -> str:
        return "".join(d.get("content", "") for d in self.of("delta") if isinstance(d, dict))


def parse_sse(raw: str) -> list[tuple[str, Any]]:
    events: list[tuple[str, Any]] = []
    for block in raw.replace("\r\n", "\n").split("\n\n"):
        name, data = "message", []
        for line in block.split("\n"):
            if line.startswith(":") or not line:
                continue
            k, _, v = line.partition(":")
            v = v[1:] if v.startswith(" ") else v
            if k == "event":
                name = v
            elif k == "data":
                data.append(v)
        if data or name != "message":
            joined = "\n".join(data)
            try:
                events.append((name, json.loads(joined)))
            except ValueError:
                events.append((name, joined))
    return events


def stream(session: ApiSession, chat_id: str, body: dict[str, Any], timeout: float = 60,
           path: str | None = None, method: str = "POST") -> SseResult:
    """Run ``POST /chats/{id}/messages:stream`` (or ``path``) to completion and parse the SSE body.

    A non-200 answer (pre-stream JSON error) yields ``SseResult(status, [], raw)``; use
    ``assert_problem(resp, ...)`` with a plain ``session.post`` when you need the Problem body.
    """
    url = path or f"{PREFIX}/chats/{chat_id}/messages:stream"
    r = session.request(method, url, json=body, headers={"Accept": "text/event-stream"},
                        stream=True, timeout=timeout)
    try:
        raw = r.content.decode("utf-8", "replace")
    finally:
        r.close()
    ok = r.status_code == 200 and "text/event-stream" in r.headers.get("Content-Type", "")
    return SseResult(r.status_code, parse_sse(raw) if ok else [], raw, dict(r.headers))


# ── DB ──────────────────────────────────────────────────────────────────────


def find_db_path(home: str | os.PathLike[str] | None = None) -> Path:
    """Locate ``mini_chat.db`` anywhere under the server's home directory."""
    root = Path(home or STATE["home"] or "")
    hits = sorted(root.rglob("mini_chat.db"))
    assert hits, f"mini_chat.db not found under {root}"
    return hits[0]


def db() -> sqlite3.Connection:
    """Fresh sqlite3 connection (Row factory) to the running server's mini-chat DB.

    Close it (or use ``contextlib.closing``) when done; WAL mode lets it read concurrently.
    """
    conn = sqlite3.connect(find_db_path(), timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def uuid_bytes(u: str | uuid.UUID) -> bytes:
    """UUID -> the 16-byte BLOB the gear stores in SQLite id columns."""
    return (u if isinstance(u, uuid.UUID) else uuid.UUID(str(u))).bytes


def wait_until(fn: Callable[[], Any], timeout: float = 15, interval: float = 0.1,
               message: str = "condition") -> Any:
    """Poll ``fn`` until it returns a truthy value (returned); raise ``AssertionError`` on timeout."""
    deadline = time.monotonic() + timeout
    last: Any = None
    while True:
        try:
            last = fn()
        except (AssertionError, sqlite3.Error) as e:  # transient while the server converges
            last = e
        else:
            if last:
                return last
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out after {timeout}s waiting for {message} (last: {last!r})")
        time.sleep(interval)


# ── Owner quota rows ─────────────────────────────────────────────────────────

#: ``quota_usage`` predicate selecting the rows of a chat's owner; bind ``owner_args(chat_id)``.
OWNER_QUOTA = ("tenant_id = (SELECT tenant_id FROM chats WHERE id = ?)"
               " AND user_id = (SELECT user_id FROM chats WHERE id = ?)")
#: ``period_start`` predicate for the current UTC day and month.
CURRENT_PERIODS = "period_start IN (date('now'), date('now', 'start of month'))"


def owner_args(chat_id: str) -> tuple[bytes, bytes]:
    """Bind parameters for ``OWNER_QUOTA``."""
    return uuid_bytes(chat_id), uuid_bytes(chat_id)


def owner_quota_rows(chat_id: str, columns: str, where: str = "1 = 1", group_by: str = "") -> list[sqlite3.Row]:
    """``SELECT columns FROM quota_usage`` for the chat owner's rows matching ``where``."""
    sql = f"SELECT {columns} FROM quota_usage WHERE {OWNER_QUOTA} AND {where}"
    if group_by:
        sql += f" GROUP BY {group_by}"
    with closing(db()) as conn:
        return conn.execute(sql, owner_args(chat_id)).fetchall()


def reserved_credits(chat_id: str) -> int:
    """Sum of ``reserved_credits_micro`` over every quota row of the chat's owner."""
    return owner_quota_rows(chat_id, "COALESCE(SUM(reserved_credits_micro), 0) AS r")[0]["r"]


@contextmanager
def exhausted_quota(chat_id: str, where: str = "1 = 1") -> Iterator[None]:
    """Set the chat owner's quota rows matching ``where`` to an absurd spend; restore them on exit."""
    with closing(db()) as conn:
        saved = conn.execute(f"SELECT rowid, spent_credits_micro FROM quota_usage WHERE {OWNER_QUOTA} AND {where}",
                             owner_args(chat_id)).fetchall()
        assert saved, "the chat owner has quota rows to exhaust (send a message first)"
        conn.execute(f"UPDATE quota_usage SET spent_credits_micro = 9000000000000000 WHERE {OWNER_QUOTA}"
                     f" AND {where}", owner_args(chat_id))
        conn.commit()
    try:
        yield
    finally:
        with closing(db()) as conn:
            conn.executemany("UPDATE quota_usage SET spent_credits_micro = ? WHERE rowid = ?",
                             [(r["spent_credits_micro"], r["rowid"]) for r in saved])
            conn.commit()


# ── Outbox capture ──────────────────────────────────────────────────────────


@contextmanager
def outbox_capture(table: str) -> Iterator[str]:
    """Copy every enqueued outbox message into ``table`` (queue, payload type, JSON payload).

    The gear's handlers ack and vacuum outbox rows within seconds; an AFTER INSERT trigger on
    the incoming table keeps a durable copy for assertions. The trigger and the table are dropped
    on exit. Use the module-scoped ``outbox_capture`` fixture (conftest.py) in tests.
    """
    with closing(db()) as conn:
        conn.executescript(f"""
            CREATE TABLE IF NOT EXISTS {table} (
                id INTEGER PRIMARY KEY AUTOINCREMENT, queue TEXT, payload_type TEXT, payload TEXT);
            CREATE TRIGGER IF NOT EXISTS {table}_trg AFTER INSERT ON toolkit_outbox_incoming
            BEGIN
                INSERT INTO {table} (queue, payload_type, payload)
                SELECT p.queue, b.payload_type, CAST(b.payload AS TEXT)
                FROM toolkit_outbox_partitions p, toolkit_outbox_body b
                WHERE p.id = NEW.partition_id AND b.id = NEW.body_id;
            END;
        """)
        conn.commit()
    try:
        yield table
    finally:
        with closing(db()) as conn:
            conn.executescript(f"DROP TRIGGER IF EXISTS {table}_trg; DROP TABLE IF EXISTS {table};")
            conn.commit()


def captured_outbox(table: str, payload_type: str | None = None) -> list[tuple[str, str, dict[str, Any]]]:
    """``(queue, payload_type, payload)`` of every message captured in ``table``, oldest first."""
    sql = f"SELECT queue, payload_type, payload FROM {table}"
    args: tuple = ()
    if payload_type is not None:
        sql, args = sql + " WHERE payload_type = ?", (payload_type,)
    with closing(db()) as conn:
        rows = conn.execute(sql + " ORDER BY id", args).fetchall()
    return [(r["queue"], r["payload_type"], json.loads(r["payload"])) for r in rows]


# ── Problem assertions ──────────────────────────────────────────────────────

PROBLEM_TYPE_FMT = "gts://gts.cf.core.errors.err.v1~cf.core.err.{category}.v1~"


def assert_problem(resp: requests.Response, status: int, category: str, **context_checks: Any) -> dict[str, Any]:
    """Assert a canonical RFC 9457 Problem (ADR-0004) and return its JSON body.

    ``context_checks`` are compared against ``problem["context"]``, with these shortcuts:

    * ``reason="X"``  - ``context.reason == X`` **or** some ``context.field_violations[].reason == X``
    * ``field="f"``   - some ``context.field_violations[].field == f``
    * ``violation_type="T"`` - some ``context.violations[].type == T`` (``failed_precondition``)
    * anything else (``resource_type=...``, ``resource_name=...``) - ``context[key] == value``
    """
    assert resp.status_code == status, f"expected HTTP {status}, got {resp.status_code}: {resp.text}"
    assert "application/problem+json" in resp.headers.get("Content-Type", ""), \
        f"not problem+json: {resp.headers.get('Content-Type')} {resp.text}"
    p = resp.json()
    assert p.get("status") == status, p
    assert p["type"] == PROBLEM_TYPE_FMT.format(category=category), p
    assert "code" not in p, f"canonical Problem has no top-level code: {p}"
    for key in ("title", "detail"):
        assert p.get(key), p
    ctx = p.get("context") or {}
    fvs = ctx.get("field_violations") or []
    for key, want in context_checks.items():
        if key == "reason":
            got = {ctx.get("reason"), *(fv.get("reason") for fv in fvs)}
            assert want in got, f"reason {want!r} not in {got} ({p})"
        elif key == "field":
            assert want in {fv.get("field") for fv in fvs}, f"field {want!r} not in {fvs} ({p})"
        elif key == "violation_type":
            assert want in {v.get("type") for v in ctx.get("violations") or []}, p
        else:
            assert ctx.get(key) == want, f"context[{key!r}]={ctx.get(key)!r}, want {want!r} ({p})"
    return p
