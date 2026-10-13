"""Direct SQLite seeding helpers for the mini-chat E2E suite (slice S2).

Messages, turns, attachments and reactions are produced by the streaming,
upload and reaction APIs. Until those exist (and to build states the API
cannot produce on demand, e.g. a ``running`` or ``cancelled`` turn), the
S2 tests insert rows straight into the running server's SQLite database.

Storage format (DESIGN §3.7, ``infra/db/migrations.rs``, ``infra/db/entities.rs``):

* UUID columns are ``TEXT`` but hold the 16 raw UUID bytes (``sqlx`` encodes
  ``uuid::Uuid`` as a BLOB);
* timestamps are ``TEXT`` in the fixed-width UTC form the gear writes on
  SQLite (``entities::fixed_text``): ``YYYY-MM-DDTHH:MM:SS.ffffffZ``, always
  six fractional digits (microseconds, sub-microsecond digits truncated) and
  ``Z``, so text order is chronological order (:func:`db_ts` reproduces it);
* booleans are ``0`` / ``1``; ``messages.features_used`` is the JSON text ``[]``.

Every helper opens its own read-write connection, runs one short
``BEGIN IMMEDIATE`` transaction (safe next to the server's WAL writers, the
harness sets ``busy_timeout``) and closes it.

The module also has read helpers for the shared ``toolkit-db`` outbox tables
(the chat-cleanup message enqueued by ``DELETE /v1/chats/{id}``) and an RFC 3339
parser with nanosecond precision for comparing API timestamps.
"""

from __future__ import annotations

import calendar
import json
import re
import sqlite3
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Iterator

from mc_helpers import new_uuid, uuid_bytes

#: Queue of the chat-cleanup outbox handler (``outbox.chat_cleanup_queue_name``).
CHAT_CLEANUP_QUEUE = "mini-chat.chat_cleanup"

#: Opaque WebP bytes used as a stored image thumbnail (the server returns the
#: stored bytes base64-encoded and never decodes them).
SAMPLE_WEBP_THUMBNAIL = bytes.fromhex(
    "524946461a000000574542505650384c0d0000002f00000010071011118888fe0700"
)
SAMPLE_THUMBNAIL_WIDTH = 64
SAMPLE_THUMBNAIL_HEIGHT = 48

_TERMINAL_STATES = ("completed", "failed", "cancelled")
_TURN_STATES = ("running",) + _TERMINAL_STATES


# --------------------------------------------------------------------------
# Timestamps
# --------------------------------------------------------------------------


def now_ns() -> int:
    """Current wall-clock time in nanoseconds since the epoch."""
    return time.time_ns()


def db_ts(at_ns: int | None = None, *, offset_ms: float = 0.0) -> str:
    """A timestamp in the exact text form the gear writes on SQLite.

    ``YYYY-MM-DDTHH:MM:SS.ffffffZ`` (UTC, microseconds, fixed width).
    ``at_ns`` defaults to now; ``offset_ms`` shifts it (negative = earlier).
    """
    total = (now_ns() if at_ns is None else at_ns) + int(round(offset_ms * 1_000_000))
    seconds, nanos = divmod(total, 1_000_000_000)
    base = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(seconds))
    return f"{base}.{nanos // 1_000:06d}Z"


_RFC3339 = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?([Zz]|[+-]\d{2}:\d{2})$"
)


def is_rfc3339(value: Any) -> bool:
    return isinstance(value, str) and _RFC3339.match(value) is not None


def parse_ts_ns(value: str) -> int:
    """Parse an RFC 3339 timestamp (up to nanosecond precision) to epoch nanoseconds."""
    match = _RFC3339.match(value)
    if match is None:
        raise AssertionError(f"not an RFC 3339 timestamp: {value!r}")
    year, month, day, hour, minute, second = (int(match.group(i)) for i in range(1, 7))
    fraction = (match.group(7) or "").ljust(9, "0")
    seconds = calendar.timegm((year, month, day, hour, minute, second, 0, 0, 0))
    offset = match.group(8)
    if offset not in ("Z", "z"):
        sign = 1 if offset[0] == "+" else -1
        seconds -= sign * (int(offset[1:3]) * 3600 + int(offset[4:6]) * 60)
    return seconds * 1_000_000_000 + int(fraction or "0")


# --------------------------------------------------------------------------
# Connection
# --------------------------------------------------------------------------


def _bool(value: bool) -> int:
    return 1 if value else 0


def _b(value: str | uuid.UUID | None) -> bytes | None:
    return None if value is None else uuid_bytes(value)


class _Tx:
    """Short write transaction on the server's SQLite database."""

    def __init__(self, mc: Any) -> None:
        self._conn: sqlite3.Connection = mc.sqlite(readonly=False)
        self._conn.isolation_level = None  # explicit BEGIN / COMMIT

    def __enter__(self) -> sqlite3.Connection:
        self._conn.execute("BEGIN IMMEDIATE")
        return self._conn

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        try:
            self._conn.execute("ROLLBACK" if exc_type is not None else "COMMIT")
        finally:
            self._conn.close()


def _insert(conn: sqlite3.Connection, table: str, row: dict[str, Any]) -> None:
    columns = ", ".join(row)
    marks = ", ".join("?" for _ in row)
    conn.execute(f"INSERT INTO {table} ({columns}) VALUES ({marks})", list(row.values()))


# --------------------------------------------------------------------------
# Chats (self-checks only: the S2 tests create chats through the API)
# --------------------------------------------------------------------------


def seed_chat(
    mc: Any,
    tenant_id: str,
    user_id: str,
    *,
    model: str = "gpt-4.1",
    title: str | None = None,
    deleted: bool = False,
) -> str:
    """Insert a ``chats`` row directly and return its id.

    Seeding is not activity: tests that depend on ``updated_at`` or on the
    create/rename/delete behaviour MUST use the API; this helper exists to
    exercise the seeding helpers against a server without the chat routes.
    """
    chat_id = new_uuid()
    stamp = db_ts()
    with _Tx(mc) as conn:
        _insert(
            conn,
            "chats",
            {
                "id": _b(chat_id),
                "tenant_id": _b(tenant_id),
                "user_id": _b(user_id),
                "model": model,
                "title": title,
                "is_temporary": 0,
                "created_at": stamp,
                "updated_at": stamp,
                "deleted_at": stamp if deleted else None,
            },
        )
    return chat_id


# --------------------------------------------------------------------------
# Messages and turns
# --------------------------------------------------------------------------


def _message_row(
    *,
    message_id: str,
    chat_id: str,
    tenant_id: str,
    request_id: str | None,
    role: str,
    content: str,
    created_at: str,
    model: str | None = None,
    input_tokens: int = 0,
    output_tokens: int = 0,
    deleted_at: str | None = None,
) -> dict[str, Any]:
    return {
        "id": _b(message_id),
        "tenant_id": _b(tenant_id),
        "chat_id": _b(chat_id),
        "request_id": _b(request_id),
        "role": role,
        "content": content,
        "content_type": "text",
        "token_estimate": 0,
        "provider_response_id": f"resp_seed_{uuid.UUID(message_id).hex[:16]}" if role == "assistant" else None,
        "request_kind": "chat",
        "features_used": "[]",
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_read_input_tokens": 0,
        "cache_write_input_tokens": 0,
        "reasoning_tokens": 0,
        "model": model,
        "is_compressed": 0,
        "created_at": created_at,
        "deleted_at": deleted_at,
    }


def seed_message(
    mc: Any,
    chat_id: str,
    tenant_id: str,
    *,
    role: str = "user",
    content: str = "Seeded message",
    request_id: str | None = None,
    model: str | None = None,
    input_tokens: int = 0,
    output_tokens: int = 0,
    deleted: bool = False,
    at_ns: int | None = None,
    offset_ms: float = 0.0,
) -> str:
    """Insert one ``messages`` row (no turn) and return its id.

    ``request_id`` defaults to a fresh UUID (the API always reports one).
    """
    message_id = new_uuid()
    created_at = db_ts(at_ns, offset_ms=offset_ms)
    with _Tx(mc) as conn:
        _insert(
            conn,
            "messages",
            _message_row(
                message_id=message_id,
                chat_id=chat_id,
                tenant_id=tenant_id,
                request_id=request_id or new_uuid(),
                role=role,
                content=content,
                created_at=created_at,
                model=model,
                input_tokens=input_tokens,
                output_tokens=output_tokens,
                deleted_at=db_ts() if deleted else None,
            ),
        )
    return message_id


@dataclass
class SeededTurn:
    """Ids and stored values of a seeded turn."""

    chat_id: str
    request_id: str
    turn_id: str
    state: str
    user_message_id: str
    assistant_message_id: str | None
    user_text: str
    assistant_text: str | None
    model: str
    input_tokens: int
    output_tokens: int
    error_code: str | None
    started_at: str
    updated_at: str
    completed_at: str | None
    deleted: bool
    message_ids: list[str] = field(default_factory=list)


def seed_turn(
    mc: Any,
    chat_id: str,
    tenant_id: str,
    user_id: str,
    *,
    state: str = "completed",
    request_id: str | None = None,
    user_text: str = "Hello there",
    assistant_text: str | None = None,
    error_code: str | None = None,
    deleted: bool = False,
    model: str = "gpt-4.1",
    input_tokens: int = 12,
    output_tokens: int = 34,
    at_ns: int | None = None,
    offset_ms: float = 0.0,
    assistant_offset_ms: float = 5.0,
    finish_offset_ms: float = 10.0,
) -> SeededTurn:
    """Insert a complete turn: user message, optional assistant message, ``chat_turns`` row.

    * ``state`` is the internal state (``running``, ``completed``, ``failed``,
      ``cancelled``).
    * The assistant message is created when ``assistant_text`` is given; for
      ``completed`` it defaults to ``"Answer to: <user_text>"`` (a completed
      turn always has one). Pass ``assistant_text`` for a ``cancelled`` turn
      with persisted partial content.
    * The assistant message carries ``model`` and the token counts; the user
      message has no model and zero tokens.
    * Timestamps: the user message is at ``at_ns`` (default now) +
      ``offset_ms``; the assistant message ``assistant_offset_ms`` later; the
      turn finishes (``completed_at``/``updated_at``) ``finish_offset_ms``
      after the start. A running turn has ``last_progress_at`` and no
      ``completed_at``; the preflight columns are NULL for it (so the orphan
      watchdog never settles quota for a seeded turn).
    * ``deleted=True`` soft-deletes the turn and its messages (a turn replaced
      by retry/edit or deleted).
    """
    if state not in _TURN_STATES:
        raise ValueError(f"unknown internal turn state {state!r}")
    if state == "completed" and assistant_text is None:
        assistant_text = f"Answer to: {user_text}"
    if state == "failed" and error_code is None:
        error_code = "provider_error"

    base_ns = (now_ns() if at_ns is None else at_ns) + int(round(offset_ms * 1_000_000))
    started_at = db_ts(base_ns)
    assistant_at = db_ts(base_ns, offset_ms=assistant_offset_ms)
    finished_at = db_ts(base_ns, offset_ms=finish_offset_ms)
    terminal = state in _TERMINAL_STATES
    deleted_at = db_ts(base_ns, offset_ms=finish_offset_ms + 1) if deleted else None

    request_id = request_id or new_uuid()
    turn_id = new_uuid()
    user_message_id = new_uuid()
    assistant_message_id = new_uuid() if assistant_text is not None else None

    with _Tx(mc) as conn:
        _insert(
            conn,
            "messages",
            _message_row(
                message_id=user_message_id,
                chat_id=chat_id,
                tenant_id=tenant_id,
                request_id=request_id,
                role="user",
                content=user_text,
                created_at=started_at,
                deleted_at=deleted_at,
            ),
        )
        if assistant_message_id is not None:
            _insert(
                conn,
                "messages",
                _message_row(
                    message_id=assistant_message_id,
                    chat_id=chat_id,
                    tenant_id=tenant_id,
                    request_id=request_id,
                    role="assistant",
                    content=assistant_text or "",
                    created_at=assistant_at,
                    model=model,
                    input_tokens=input_tokens,
                    output_tokens=output_tokens,
                    deleted_at=deleted_at,
                ),
            )
        preflight = terminal
        _insert(
            conn,
            "chat_turns",
            {
                "id": _b(turn_id),
                "tenant_id": _b(tenant_id),
                "chat_id": _b(chat_id),
                "request_id": _b(request_id),
                "requester_type": "user",
                "requester_user_id": _b(user_id),
                "state": state,
                "provider_name": None,
                "provider_response_id": f"resp_seed_{uuid.UUID(turn_id).hex[:16]}" if terminal else None,
                "assistant_message_id": _b(assistant_message_id),
                "error_code": error_code,
                "reserve_tokens": 2_000 if preflight else None,
                "max_output_tokens_applied": 1_000 if preflight else None,
                "reserved_credits_micro": 1 if preflight else None,
                "policy_version_applied": 1 if preflight else None,
                "effective_model": model if preflight else None,
                "minimal_generation_floor_applied": 50 if preflight else None,
                "error_detail": None,
                "deleted_at": deleted_at,
                "replaced_by_request_id": None,
                "started_at": started_at,
                "last_progress_at": finished_at if terminal else db_ts(),
                "web_search_enabled": 0,
                "web_search_completed_count": 0,
                "code_interpreter_completed_count": 0,
                "file_search_completed_count": 0,
                "completed_at": finished_at if terminal else None,
                "updated_at": finished_at if terminal else started_at,
            },
        )

    message_ids = [user_message_id] + ([assistant_message_id] if assistant_message_id else [])
    return SeededTurn(
        chat_id=chat_id,
        request_id=request_id,
        turn_id=turn_id,
        state=state,
        user_message_id=user_message_id,
        assistant_message_id=assistant_message_id,
        user_text=user_text,
        assistant_text=assistant_text,
        model=model,
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        error_code=error_code,
        started_at=started_at,
        updated_at=finished_at if terminal else started_at,
        completed_at=finished_at if terminal else None,
        deleted=deleted,
        message_ids=message_ids,
    )


# --------------------------------------------------------------------------
# Attachments and reactions
# --------------------------------------------------------------------------


def seed_attachment(
    mc: Any,
    chat_id: str,
    tenant_id: str,
    user_id: str,
    *,
    kind: str = "document",
    status: str = "ready",
    filename: str | None = None,
    content_type: str | None = None,
    size_bytes: int = 1024,
    provider_file_id: str | None = "auto",
    thumbnail: bytes | None = None,
    thumbnail_width: int | None = None,
    thumbnail_height: int | None = None,
    deleted: bool = False,
    offset_ms: float = 0.0,
) -> str:
    """Insert an ``attachments`` row and return its id.

    Images default to the :data:`SAMPLE_WEBP_THUMBNAIL` thumbnail
    (``SAMPLE_THUMBNAIL_WIDTH`` x ``SAMPLE_THUMBNAIL_HEIGHT``); documents never
    have one. ``provider_file_id="auto"`` generates ``file-seed-<hex>``.
    """
    if kind not in ("document", "image"):
        raise ValueError(f"unknown attachment kind {kind!r}")
    attachment_id = new_uuid()
    if filename is None:
        filename = "photo.png" if kind == "image" else "report.pdf"
    if content_type is None:
        content_type = "image/png" if kind == "image" else "application/pdf"
    if provider_file_id == "auto":
        provider_file_id = f"file-seed-{uuid.UUID(attachment_id).hex[:20]}"
    if kind == "image":
        thumbnail = SAMPLE_WEBP_THUMBNAIL if thumbnail is None else thumbnail
        thumbnail_width = SAMPLE_THUMBNAIL_WIDTH if thumbnail_width is None else thumbnail_width
        thumbnail_height = SAMPLE_THUMBNAIL_HEIGHT if thumbnail_height is None else thumbnail_height
    else:
        thumbnail = thumbnail_width = thumbnail_height = None
    created_at = db_ts(offset_ms=offset_ms)
    with _Tx(mc) as conn:
        _insert(
            conn,
            "attachments",
            {
                "id": _b(attachment_id),
                "tenant_id": _b(tenant_id),
                "chat_id": _b(chat_id),
                "uploaded_by_user_id": _b(user_id),
                "filename": filename,
                "content_type": content_type,
                "size_bytes": size_bytes,
                "storage_backend": "openai",
                "provider_file_id": provider_file_id,
                "status": status,
                "error_code": "upload_failed" if status == "failed" else None,
                "attachment_kind": kind,
                "for_file_search": _bool(kind == "document"),
                "for_code_interpreter": 0,
                "doc_summary": None,
                "img_thumbnail": thumbnail,
                "img_thumbnail_width": thumbnail_width,
                "img_thumbnail_height": thumbnail_height,
                "summary_model": None,
                "summary_updated_at": None,
                "cleanup_status": None,
                "cleanup_attempts": 0,
                "last_cleanup_error": None,
                "cleanup_updated_at": None,
                "created_at": created_at,
                "updated_at": created_at,
                "deleted_at": db_ts() if deleted else None,
                "secondary_file_id": None,
                "secondary_status": "not_attempted",
                "secondary_provider_kind": None,
            },
        )
    return attachment_id


def link_message_attachment(
    mc: Any, chat_id: str, tenant_id: str, message_id: str, attachment_id: str
) -> None:
    """Insert a ``message_attachments`` row (both rows must belong to ``chat_id``)."""
    with _Tx(mc) as conn:
        _insert(
            conn,
            "message_attachments",
            {
                "tenant_id": _b(tenant_id),
                "chat_id": _b(chat_id),
                "message_id": _b(message_id),
                "attachment_id": _b(attachment_id),
                "created_at": db_ts(),
            },
        )


def seed_reaction(
    mc: Any, message_id: str, tenant_id: str, user_id: str, reaction: str = "like"
) -> str:
    """Insert a ``message_reactions`` row for ``(message_id, user_id)`` and return its id."""
    reaction_id = new_uuid()
    with _Tx(mc) as conn:
        _insert(
            conn,
            "message_reactions",
            {
                "id": _b(reaction_id),
                "message_id": _b(message_id),
                "user_id": _b(user_id),
                "tenant_id": _b(tenant_id),
                "reaction": reaction,
                "created_at": db_ts(),
            },
        )
    return reaction_id


# --------------------------------------------------------------------------
# Shared outbox (toolkit-db) read helpers
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class OutboxTables:
    """Names of one outbox table set, discovered from ``sqlite_master``."""

    prefix: str

    @property
    def partitions(self) -> str:
        return f"{self.prefix}_partitions"

    @property
    def incoming(self) -> str:
        return f"{self.prefix}_incoming"

    @property
    def outgoing(self) -> str:
        return f"{self.prefix}_outgoing"

    @property
    def body(self) -> str:
        return f"{self.prefix}_body"

    @property
    def dead_letters(self) -> str:
        return f"{self.prefix}_dead_letters"


def _columns(conn: sqlite3.Connection, table: str) -> set[str]:
    return {row[1] for row in conn.execute(f"PRAGMA table_info({table})")}


def outbox_tables(mc: Any) -> list[OutboxTables]:
    """Every outbox table set in the gear database (a ``*_partitions`` table with a ``queue`` column)."""
    conn = mc.sqlite()
    try:
        names = [
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE '%\\_partitions' ESCAPE '\\'"
            )
        ]
        found = []
        for name in names:
            if {"queue", "sequence"} <= _columns(conn, name):
                found.append(OutboxTables(prefix=name[: -len("_partitions")]))
        return found
    finally:
        conn.close()


@dataclass(frozen=True)
class OutboxSnapshot:
    """Monotonic enqueue counter of a queue plus the body id high-water mark.

    ``activity`` is ``SUM(partitions.sequence) + COUNT(incoming rows)`` over
    the queue's partitions: an enqueued message is first an ``incoming`` row
    and is then sequenced (incrementing the partition sequence), so the sum
    never decreases and grows by one per enqueued message, even after the
    handler acked it and the vacuum removed the body.
    """

    queue: str
    registered_partitions: int
    activity: int
    body_high_water: int


def outbox_snapshot(mc: Any, queue: str = CHAT_CLEANUP_QUEUE) -> OutboxSnapshot:
    registered = activity = high_water = 0
    conn = mc.sqlite()
    try:
        for tables in outbox_tables(mc):
            row = conn.execute(
                f"SELECT COUNT(*), COALESCE(SUM(sequence), 0) FROM {tables.partitions} WHERE queue = ?",
                (queue,),
            ).fetchone()
            registered += int(row[0])
            activity += int(row[1])
            incoming_cols = _columns(conn, tables.incoming)
            if "partition_id" in incoming_cols:
                activity += int(
                    conn.execute(
                        f"SELECT COUNT(*) FROM {tables.incoming} i "
                        f"JOIN {tables.partitions} p ON p.id = i.partition_id WHERE p.queue = ?",
                        (queue,),
                    ).fetchone()[0]
                )
            seq = conn.execute(
                "SELECT seq FROM sqlite_sequence WHERE name = ?", (tables.body,)
            ).fetchone() if _has_table(conn, "sqlite_sequence") else None
            if seq is None:
                seq = conn.execute(f"SELECT COALESCE(MAX(id), 0) FROM {tables.body}").fetchone()
            high_water = max(high_water, int(seq[0] or 0))
    finally:
        conn.close()
    return OutboxSnapshot(queue, registered, activity, high_water)


def _has_table(conn: sqlite3.Connection, name: str) -> bool:
    return (
        conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", (name,)
        ).fetchone()
        is not None
    )


def _decode_payload(raw: Any) -> Any:
    if isinstance(raw, (bytes, bytearray, memoryview)):
        try:
            text = bytes(raw).decode("utf-8")
        except UnicodeDecodeError:
            return None
    else:
        text = str(raw)
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def _walk_dicts(value: Any) -> Iterator[dict[str, Any]]:
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from _walk_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from _walk_dicts(child)


def outbox_payloads_for_chat(
    mc: Any, chat_id: str, since_body_id: int = 0
) -> list[dict[str, Any]]:
    """Readable JSON payload objects (bodies newer than ``since_body_id`` and
    dead letters) that carry ``chat_id`` == ``chat_id``.

    Returns the innermost object holding the ``chat_id`` key; an empty list
    when the payload was already vacuumed or is not JSON.
    """
    found: list[dict[str, Any]] = []
    conn = mc.sqlite()
    try:
        for tables in outbox_tables(mc):
            sources = []
            if _has_table(conn, tables.body):
                sources.append(f"SELECT payload FROM {tables.body} WHERE id > {int(since_body_id)}")
            if _has_table(conn, tables.dead_letters) and "payload" in _columns(conn, tables.dead_letters):
                sources.append(f"SELECT payload FROM {tables.dead_letters}")
            for sql in sources:
                for (raw,) in conn.execute(sql):
                    payload = _decode_payload(raw)
                    for obj in _walk_dicts(payload):
                        if str(obj.get("chat_id", "")).lower() == chat_id.lower():
                            found.append(obj)
    finally:
        conn.close()
    return found
