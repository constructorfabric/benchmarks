"""S6 background: chat-deletion cleanup, orphan turn watchdog and upload reaper.

Contract sources (gears/mini-chat/docs/DESIGN.md):

* §3.6 "Cleanup on Chat Deletion" and §4 "Cleanup on Chat Deletion": the
  chat cleanup handler (queue ``mini-chat.chat_cleanup``) deletes the provider
  file of every attachment with ``cleanup_status = pending`` (2xx and ``404``
  are success), records ``done`` or, after ``cleanup_worker.max_attempts``
  failed attempts, terminal ``failed`` (``cleanup_attempts``,
  ``last_cleanup_error``, ``cleanup_updated_at``); only when every attachment
  is terminal it deletes the chat vector store (``404`` is success) and then
  the ``chat_vector_stores`` row; a failing vector-store delete returns
  ``Retry`` until the delivery that reaches ``cleanup_worker.max_attempts``
  (all deliveries count), then ``Reject`` (dead letter) and the row is kept;
  a ``failed`` attachment does not block the vector-store delete;
* §4 "Orphan Turn Watchdog" and B.9.1: a ``running``, non-deleted turn whose
  ``last_progress_at`` (or ``started_at`` when NULL) is older than
  ``orphan_watchdog.timeout_secs`` becomes ``failed`` / ``orphan_timeout``
  (``completed_at`` / ``updated_at`` set) through the CAS; estimated
  settlement (``charged = credits_micro(reserve_tokens -
  max_output_tokens_applied, minimal_generation_floor_applied, in, out)``,
  reserve released, ``calls + 1``); usage event ``billing_outcome = aborted``,
  ``settlement_method = estimated``, ``usage = null``, ``selected_model`` =
  the effective model; audit ``turn_failed`` with ``quota.decision =
  unknown``; turns with NULL reserve fields are finalized without settlement
  but still emit a usage event (``actual_credits_micro = 0``, models ``""``,
  ``policy_version_applied = 0``); recent progress is never finalized; no
  ``messages`` row is created; Turn Status ``state = error``,
  ``error_code = orphan_timeout``;
* B.9.5 "Upload reaper": ``pending`` / ``uploaded`` rows with ``deleted_at IS
  NULL``, ``cleanup_status IS NULL`` and ``updated_at`` older than
  ``upload_reaper.stale_after_secs`` become ``failed`` / ``upload_abandoned``
  (``updated_at`` refreshed); a row with a ``provider_file_id`` also gets
  ``cleanup_status = pending`` and an attachment cleanup message
  (``event_type = attachment_upload_abandoned``) that deletes the provider
  file; the row stays visible via ``GET`` and no longer counts toward
  ``rag.max_documents_per_chat``.

The orphan watchdog runs on the default server (``scan_interval_secs = 1``,
default ``timeout_secs = 300``): turns are seeded with progress ten minutes in
the past instead of waiting. Tests with their own servers are ``slow``.
"""

from __future__ import annotations

import json
import random
import struct
import threading
import time
import uuid
import zlib
from datetime import datetime, timezone
from typing import Any, Iterator

import pytest

from conftest import create_server
from mc_helpers import assert_problem, db_uuid, new_uuid, uuid_bytes, wait_until
from mc_seed import (
    db_ts,
    is_rfc3339,
    now_ns,
    outbox_payloads_for_chat,
    outbox_tables,
    seed_attachment,
    seed_turn,
)

CHATS = "/mini-chat/v1/chats"
UPLOAD_MODEL = "gpt-4.1"
#: Standard tier: the estimated settlement touches the ``total`` bucket only.
ORPHAN_MODEL = "gpt-4.1-mini"
ORPHAN_USER = "A2"
TXT_MIME = "text/plain"
CLEANUP_MAX_ATTEMPTS = 2
DEFAULT_ORPHAN_TIMEOUT_SECS = 300
DEFAULT_STREAMING_MAX_OUTPUT_TOKENS = 32_768
DEFAULT_MINIMAL_GENERATION_FLOOR = 50
#: How far in the past "stale" progress / uploads are seeded.
STALE_SECS = 600
REAPER_STALE_AFTER_SECS = 60
POLICY_VERSION = 1


# --------------------------------------------------------------------------
# Servers
# --------------------------------------------------------------------------


def _gear(**config: Any) -> dict[str, Any]:
    return {"gears": {"mini-chat": {"config": config}}}


@pytest.fixture(scope="module")
def _s6_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


def _module_server(mock, servers: list[Any], name: str, overrides: dict[str, Any]):
    server = create_server(mock, overrides, name=name)
    servers.append(server)
    server.start()
    return server.wait_ready()


@pytest.fixture(scope="module")
def cleanup_server(mock, _s6_servers):
    """``cleanup_worker.max_attempts = 2``."""
    return _module_server(
        mock, _s6_servers, "s6-cleanup", _gear(cleanup_worker={"max_attempts": CLEANUP_MAX_ATTEMPTS})
    )


@pytest.fixture(scope="module")
def reaper_server(mock, _s6_servers):
    """Upload reaper scanning every second, stale after 60 s; one document per chat."""
    overrides = _gear(
        upload_reaper={
            "enabled": True,
            "scan_interval_secs": 1,
            "stale_after_secs": REAPER_STALE_AFTER_SECS,
        },
        rag={"max_documents_per_chat": 1},
    )
    return _module_server(mock, _s6_servers, "s6-reaper", overrides)


# --------------------------------------------------------------------------
# Generic helpers
# --------------------------------------------------------------------------


def make_png(width: int, height: int, seed: int = 7) -> bytes:
    rng = random.Random(seed)
    rows = b"".join(b"\x00" + bytes(rng.randrange(256) for _ in range(width * 3)) for _ in range(height))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def text_document(label: str) -> bytes:
    return f"Mini-chat S6 {label}: the warranty period is 24 months.\n".encode() * 4


def new_chat(server, user: str = "A1", model: str = UPLOAD_MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S6 cleanup and recovery", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def upload(server, chat_id: str, data: bytes, filename: str, content_type: str, user: str = "A1"):
    return server.post(
        f"{CHATS}/{chat_id}/attachments", user, files={"file": (filename, data, content_type)}, timeout=60
    )


def upload_ok(server, chat_id: str, data: bytes, filename: str, content_type: str) -> dict[str, Any]:
    resp = upload(server, chat_id, data, filename, content_type)
    assert resp.status_code == 201, f"upload failed: HTTP {resp.status_code} {resp.text[:1000]}"
    body = resp.json()
    assert body["status"] == "ready", body
    return body


def upload_document(server, chat_id: str, label: str = "doc") -> dict[str, Any]:
    return upload_ok(server, chat_id, text_document(label), f"{label}.txt", TXT_MIME)


def upload_image(server, chat_id: str) -> dict[str, Any]:
    return upload_ok(server, chat_id, make_png(48, 32), "photo.png", "image/png")


def send_done(server, chat_id: str, content: str, user: str = "A1") -> str:
    request_id = new_uuid()
    result = server.stream(
        "POST", f"{CHATS}/{chat_id}/messages:stream", user, json={"content": content, "request_id": request_id}
    )
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.terminal is not None and result.terminal[0] == "done", (result.names(), result.events[-1:])
    return request_id


def attachment_row(server, attachment_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM attachments WHERE id = ?", (db_uuid(attachment_id),))
    assert len(rows) == 1, rows
    return rows[0]


def vector_store_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (db_uuid(chat_id),))


def file_deletes(mock, provider_file_id: str) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("files_delete") if r["path"].endswith(f"/files/{provider_file_id}")]


def vector_store_deletes(mock, vector_store_id: str) -> list[dict[str, Any]]:
    return [
        r for r in mock.requests_for("vector_store_delete") if r["path"].endswith(f"/vector_stores/{vector_store_id}")
    ]


def wait_cleanup_status(server, attachment_id: str, status: str, timeout: float = 45.0) -> dict[str, Any]:
    return wait_until(
        lambda: (row := attachment_row(server, attachment_id))["cleanup_status"] == status and row,
        timeout=timeout,
        interval=0.2,
        description=f"cleanup_status {status!r} of attachment {attachment_id}",
    )


def db_exec(server, statements: list[tuple[str, list[Any]]]) -> list[int]:
    """Run ``statements`` in one write transaction; returns each rowcount."""
    conn = server.sqlite(readonly=False)
    conn.isolation_level = None
    counts: list[int] = []
    try:
        conn.execute("BEGIN IMMEDIATE")
        try:
            for sql, params in statements:
                counts.append(conn.execute(sql, params).rowcount)
        except Exception:
            conn.execute("ROLLBACK")
            raise
        conn.execute("COMMIT")
    finally:
        conn.close()
    return counts


def _json_payload(raw: Any) -> Any:
    if isinstance(raw, (bytes, bytearray, memoryview)):
        raw = bytes(raw).decode("utf-8", errors="replace")
    try:
        return json.loads(str(raw))
    except json.JSONDecodeError:
        return None


def dead_letter_payloads(server) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    conn = server.sqlite()
    try:
        for tables in outbox_tables(server):
            exists = conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", (tables.dead_letters,)
            ).fetchone()
            if not exists:
                continue
            columns = {row[1] for row in conn.execute(f"PRAGMA table_info({tables.dead_letters})")}
            if "payload" not in columns:
                continue
            for (raw,) in conn.execute(f"SELECT payload FROM {tables.dead_letters}"):
                payload = _json_payload(raw)
                if isinstance(payload, dict):
                    found.append(payload)
    finally:
        conn.close()
    return found


class OutboxRecorder:
    """Captures the outbox payloads of one chat (bodies are vacuumed after ack)."""

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="s6-outbox-recorder", daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.poll()
            time.sleep(self._interval)

    def poll(self) -> None:
        try:
            payloads = outbox_payloads_for_chat(self._server, self._chat_id)
        except Exception:  # noqa: BLE001 - transient SQLite contention, polled again
            return
        with self._lock:
            for payload in payloads:
                self._seen.setdefault(json.dumps(payload, sort_keys=True), payload)

    def __enter__(self) -> "OutboxRecorder":
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def payloads(self) -> list[dict[str, Any]]:
        self.poll()
        with self._lock:
            return list(self._seen.values())

    def usage_events(self, turn_id: str) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if "dedupe_key" in p and str(p.get("turn_id", "")).lower() == turn_id.lower()
        ]

    def audit_events(self, request_id: str, event_type: str) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if p.get("event_type") == event_type and str(p.get("request_id", "")).lower() == request_id.lower()
        ]

    def cleanup_events(self, attachment_id: str) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if "event_type" in p and str(p.get("attachment_id", "")).lower() == attachment_id.lower()
        ]


# --------------------------------------------------------------------------
# Chat deletion cleanup
# --------------------------------------------------------------------------


def test_chat_deletion_deletes_every_provider_file_then_the_vector_store(mc, mock):
    chat_id = new_chat(mc)
    doc = upload_document(mc, chat_id, "s6-cleanup-doc")
    image = upload_image(mc, chat_id)
    doc_file = attachment_row(mc, doc["id"])["provider_file_id"]
    image_file = attachment_row(mc, image["id"])["provider_file_id"]
    stores = vector_store_rows(mc, chat_id)
    assert len(stores) == 1 and stores[0]["vector_store_id"], stores
    vector_store_id = stores[0]["vector_store_id"]

    assert mc.delete(f"{CHATS}/{chat_id}").status_code == 204

    doc_row = wait_cleanup_status(mc, doc["id"], "done")
    image_row = wait_cleanup_status(mc, image["id"], "done")
    wait_until(lambda: vector_store_rows(mc, chat_id) == [], timeout=45, description="chat_vector_stores row removed")

    for row in (doc_row, image_row):
        assert row["cleanup_attempts"] == 0, row
        assert row["last_cleanup_error"] is None, row
        assert row["cleanup_updated_at"] is not None, row
    doc_deletes = file_deletes(mock, doc_file)
    image_deletes = file_deletes(mock, image_file)
    store_deletes = vector_store_deletes(mock, vector_store_id)
    assert len(doc_deletes) == 1, doc_deletes
    assert len(image_deletes) == 1, image_deletes
    assert len(store_deletes) == 1, store_deletes
    assert store_deletes[0]["seq"] > max(doc_deletes[0]["seq"], image_deletes[0]["seq"]), (
        "the vector store is deleted only after every attachment is terminal"
    )
    assert doc_file not in mock.files and image_file not in mock.files
    assert vector_store_id not in mock.vector_stores


def test_provider_not_found_counts_as_success_for_files_and_the_vector_store(mc, mock):
    chat_id = new_chat(mc)
    doc = upload_document(mc, chat_id, "s6-cleanup-404")
    doc_file = attachment_row(mc, doc["id"])["provider_file_id"]
    vector_store_id = vector_store_rows(mc, chat_id)[0]["vector_store_id"]
    mock.fail_next("files_delete", 404)
    mock.fail_next("vector_store_delete", 404)

    assert mc.delete(f"{CHATS}/{chat_id}").status_code == 204

    row = wait_cleanup_status(mc, doc["id"], "done")
    wait_until(lambda: vector_store_rows(mc, chat_id) == [], timeout=45, description="chat_vector_stores row removed")
    time.sleep(1.0)
    assert row["cleanup_attempts"] == 0, row
    assert row["last_cleanup_error"] is None, row
    assert len(file_deletes(mock, doc_file)) == 1, "404 is success: no retry"
    assert len(vector_store_deletes(mock, vector_store_id)) == 1, "404 is success: no retry"


@pytest.mark.slow
def test_file_delete_failing_max_attempts_marks_attachment_failed_and_still_deletes_vector_store(
    cleanup_server, mock
):
    server = cleanup_server
    chat_id = new_chat(server)
    doc = upload_document(server, chat_id, "s6-cleanup-failing-file")
    doc_file = attachment_row(server, doc["id"])["provider_file_id"]
    vector_store_id = vector_store_rows(server, chat_id)[0]["vector_store_id"]
    mock.fail_next("files_delete", 500, times=10)

    assert server.delete(f"{CHATS}/{chat_id}").status_code == 204

    row = wait_cleanup_status(server, doc["id"], "failed", timeout=60)
    wait_until(
        lambda: vector_store_rows(server, chat_id) == [], timeout=45, description="chat_vector_stores row removed"
    )
    time.sleep(2.0)

    assert row["cleanup_attempts"] == CLEANUP_MAX_ATTEMPTS, row
    assert row["last_cleanup_error"], row
    assert row["cleanup_updated_at"] is not None, row
    deletes = file_deletes(mock, doc_file)
    assert len(deletes) == CLEANUP_MAX_ATTEMPTS, f"no attempt after the limit: {len(deletes)}"
    store_deletes = vector_store_deletes(mock, vector_store_id)
    assert len(store_deletes) == 1, store_deletes
    assert store_deletes[0]["seq"] > deletes[-1]["seq"], "vector store deleted after the attachment is terminal"
    assert attachment_row(server, doc["id"])["cleanup_status"] == "failed"


@pytest.mark.slow
def test_vector_store_delete_failing_keeps_the_row_and_dead_letters_the_message(cleanup_server, mock):
    server = cleanup_server
    chat_id = new_chat(server)
    doc = upload_document(server, chat_id, "s6-cleanup-failing-store")
    vector_store_id = vector_store_rows(server, chat_id)[0]["vector_store_id"]
    mock.fail_next("vector_store_delete", 500, times=10)

    assert server.delete(f"{CHATS}/{chat_id}").status_code == 204

    wait_cleanup_status(server, doc["id"], "done")
    dead = wait_until(
        lambda: [
            p
            for p in dead_letter_payloads(server)
            if str(p.get("chat_id", "")).lower() == chat_id and p.get("reason") == "chat_soft_delete"
        ],
        timeout=60,
        description="dead-lettered chat cleanup message",
    )
    time.sleep(2.0)

    store_deletes = vector_store_deletes(mock, vector_store_id)
    assert len(store_deletes) == CLEANUP_MAX_ATTEMPTS, (
        f"every delivery counts toward cleanup_worker.max_attempts: {len(store_deletes)}"
    )
    rows = vector_store_rows(server, chat_id)
    assert len(rows) == 1 and rows[0]["vector_store_id"] == vector_store_id, (
        "the chat_vector_stores row is kept for a dead-letter replay"
    )
    assert len(dead) == 1, dead
    assert str(dead[0]["tenant_id"]).lower() == server.identity("A1").tenant_id, dead
    assert is_rfc3339(dead[0]["chat_deleted_at"]), dead
    assert dead[0]["system_request_id"], dead


# --------------------------------------------------------------------------
# Orphan watchdog
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def credits_micro(input_tokens: int, output_tokens: int, in_mult: int, out_mult: int) -> int:
    return _ceil_div(input_tokens * in_mult, 1_000_000) + _ceil_div(output_tokens * out_mult, 1_000_000)


def _catalog_entry(server, model: str) -> dict[str, Any]:
    return next(e for e in server.catalog if e["id"] == model)


def preflight_reserve(server, content: str, model: str = ORPHAN_MODEL) -> dict[str, Any]:
    """Preflight columns of a fresh text-only turn (§5.4.1)."""
    entry = _catalog_entry(server, model)
    budgets = entry["estimation_budgets"]
    base = _ceil_div(len(content.encode("utf-8")), int(budgets["bytes_per_token_conservative"]))
    base += int(budgets["fixed_overhead_tokens"])
    estimated_input = _ceil_div(base * (100 + int(budgets["safety_margin_pct"])), 100)
    cap = int(server.gear_config.get("streaming", {}).get("max_output_tokens", DEFAULT_STREAMING_MAX_OUTPUT_TOKENS))
    max_out = min(int(entry["max_output_tokens"]), cap)
    floor = min(
        int(server.gear_config.get("estimation_budgets", {}).get("minimal_generation_floor", DEFAULT_MINIMAL_GENERATION_FLOOR)),
        max_out,
    )
    in_mult = int(entry["input_tokens_credit_multiplier_micro"])
    out_mult = int(entry["output_tokens_credit_multiplier_micro"])
    return {
        "reserve_tokens": estimated_input + max_out,
        "max_output_tokens_applied": max_out,
        "reserved_credits_micro": credits_micro(estimated_input, max_out, in_mult, out_mult),
        "policy_version_applied": POLICY_VERSION,
        "effective_model": model,
        "minimal_generation_floor_applied": floor,
        # §5.8 estimated charge of an aborted turn.
        "charged": credits_micro(estimated_input, floor, in_mult, out_mult),
    }


def orphan_timeout_secs(server) -> int:
    return int(server.gear_config.get("orphan_watchdog", {}).get("timeout_secs", DEFAULT_ORPHAN_TIMEOUT_SECS))


def stale_at_ns() -> int:
    return now_ns() - STALE_SECS * 1_000_000_000


def _today() -> str:
    return datetime.now(timezone.utc).date().isoformat()


def _month_start() -> str:
    return datetime.now(timezone.utc).date().replace(day=1).isoformat()


def _date_text(value: Any) -> str:
    if isinstance(value, (bytes, bytearray, memoryview)):
        value = bytes(value).decode("ascii")
    return str(value)[:10]


def total_quota_rows(server, user: str) -> dict[str, dict[str, int]]:
    """Current-period ``total`` bucket rows of ``user`` keyed by ``period_type``."""
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, calls, "
        "input_tokens, output_tokens FROM quota_usage WHERE tenant_id = ? AND user_id = ? AND bucket = 'total'",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    current = {"daily": _today(), "monthly": _month_start()}
    return {
        r["period_type"]: {
            "spent": int(r["spent_credits_micro"]),
            "reserved": int(r["reserved_credits_micro"]),
            "calls": int(r["calls"]),
            "input_tokens": int(r["input_tokens"]),
            "output_tokens": int(r["output_tokens"]),
        }
        for r in rows
        if current.get(r["period_type"]) == _date_text(r["period_start"])
    }


def all_quota_rows(server, user: str) -> list[dict[str, Any]]:
    ident = server.identity(user)
    return server.query(
        "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? ORDER BY period_type, period_start, bucket",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )


def quota_baseline(server, mock, user: str) -> dict[str, dict[str, int]]:
    """Make sure ``user`` has current ``total`` rows and no reserve in flight."""
    if set(total_quota_rows(server, user)) != {"daily", "monthly"}:
        warm_chat = new_chat(server, user, ORPHAN_MODEL)
        send_done(server, warm_chat, "S6 warm-up message", user)
    return wait_until(
        lambda: (rows := total_quota_rows(server, user))
        and set(rows) == {"daily", "monthly"}
        and all(v["reserved"] == 0 for v in rows.values())
        and rows,
        timeout=45,
        description=f"{user} quota rows without a reserve in flight",
    )


def turn_row(server, turn_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM chat_turns WHERE id = ?", (db_uuid(turn_id),))
    assert len(rows) == 1, rows
    return rows[0]


def turn_messages(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT id, role, content, deleted_at FROM messages WHERE chat_id = ? ORDER BY created_at, id",
        (db_uuid(chat_id),),
    )


def make_stale(
    server,
    turn_id: str,
    *,
    reserve: dict[str, Any] | None = None,
    user: str = ORPHAN_USER,
    last_progress: str | None = "stale",
) -> None:
    """Move the turn's progress ``STALE_SECS`` into the past (``last_progress=None``
    sets it NULL) and, with ``reserve``, write the preflight columns and add the
    reserve to the user's current ``total`` buckets, in one transaction."""
    stale = db_ts(stale_at_ns())
    statements: list[tuple[str, list[Any]]] = [
        (
            "UPDATE chat_turns SET last_progress_at = ? WHERE id = ?",
            [stale if last_progress == "stale" else last_progress, uuid_bytes(turn_id)],
        )
    ]
    if reserve is not None:
        ident = server.identity(user)
        statements.append(
            (
                "UPDATE chat_turns SET reserve_tokens = ?, max_output_tokens_applied = ?, reserved_credits_micro = ?, "
                "policy_version_applied = ?, effective_model = ?, minimal_generation_floor_applied = ? WHERE id = ?",
                [
                    reserve["reserve_tokens"],
                    reserve["max_output_tokens_applied"],
                    reserve["reserved_credits_micro"],
                    reserve["policy_version_applied"],
                    reserve["effective_model"],
                    reserve["minimal_generation_floor_applied"],
                    uuid_bytes(turn_id),
                ],
            )
        )
        statements.append(
            (
                "UPDATE quota_usage SET reserved_credits_micro = reserved_credits_micro + ? "
                "WHERE tenant_id = ? AND user_id = ? AND bucket = 'total' AND ("
                "(period_type = 'daily' AND substr(period_start, 1, 10) = ?) OR "
                "(period_type = 'monthly' AND substr(period_start, 1, 10) = ?))",
                [
                    reserve["reserved_credits_micro"],
                    uuid_bytes(ident.tenant_id),
                    uuid_bytes(ident.user_id),
                    _today(),
                    _month_start(),
                ],
            )
        )
    counts = db_exec(server, statements)
    assert counts[0] == 1, counts
    if reserve is not None:
        assert counts[2] == 2, f"expected the daily and monthly total rows: {counts}"


def wait_turn_state(server, turn_id: str, state: str, timeout: float = 20.0) -> dict[str, Any]:
    return wait_until(
        lambda: (row := turn_row(server, turn_id))["state"] == state and row,
        timeout=timeout,
        interval=0.2,
        description=f"turn {turn_id} to become {state!r}",
    )


def soft_delete_turn(server, turn_id: str) -> None:
    db_exec(server, [("UPDATE chat_turns SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL", [db_ts(), uuid_bytes(turn_id)])])


def test_stale_running_turn_is_failed_as_orphan_timeout_and_settled_estimated(mc, mock):
    assert STALE_SECS > orphan_timeout_secs(mc), "test design: the seeded progress must be stale"
    user = ORPHAN_USER
    ident = mc.identity(user)
    baseline = quota_baseline(mc, mock, user)
    chat_id = new_chat(mc, user, ORPHAN_MODEL)
    content = "S6 orphan: a question whose stream was lost with its pod."
    reserve = preflight_reserve(mc, content)

    with OutboxRecorder(mc, chat_id) as recorder:
        seeded = seed_turn(
            mc, chat_id, ident.tenant_id, ident.user_id, state="running", user_text=content,
            model=ORPHAN_MODEL, at_ns=stale_at_ns(),
        )
        make_stale(mc, seeded.turn_id, reserve=reserve, user=user)

        row = wait_turn_state(mc, seeded.turn_id, "failed")
        usage = wait_until(
            lambda: recorder.usage_events(seeded.turn_id), timeout=20, interval=0.05, description="usage event"
        )
        audit = wait_until(
            lambda: recorder.audit_events(seeded.request_id, "turn_failed"),
            timeout=20,
            interval=0.05,
            description="turn_failed audit event",
        )
        time.sleep(2.0)
        usage = recorder.usage_events(seeded.turn_id)
        audit = recorder.audit_events(seeded.request_id, "turn_failed")

    # Turn: CAS-finalized as an orphan; no message created or changed.
    assert row["error_code"] == "orphan_timeout", row
    assert row["completed_at"] is not None, row
    assert row["updated_at"] == row["completed_at"], row
    assert row["deleted_at"] is None, row
    assert row["assistant_message_id"] is None, row
    messages = turn_messages(mc, chat_id)
    assert [(m["id"], m["role"], m["content"]) for m in messages] == [
        (seeded.user_message_id, "user", content)
    ], messages

    # Quota: reserve released, estimated charge applied once.
    after = total_quota_rows(mc, user)
    for period in ("daily", "monthly"):
        old, new = baseline[period], after[period]
        assert new["reserved"] == old["reserved"], (period, old, new)
        assert new["spent"] == old["spent"] + reserve["charged"], (period, old, new, reserve)
        assert new["calls"] == old["calls"] + 1, (period, old, new)
        assert (new["input_tokens"], new["output_tokens"]) == (old["input_tokens"], old["output_tokens"]), (
            "no token telemetry on the estimated path"
        )

    # Usage event: aborted / estimated, selected model = effective model.
    assert len(usage) == 1, usage
    event = usage[0]
    assert event["billing_outcome"] == "aborted", event
    assert event["settlement_method"] == "estimated", event
    assert int(event["actual_credits_micro"]) == reserve["charged"], event
    assert event["usage"] is None, event
    assert event["terminal_state"] == "failed", event
    assert event["effective_model"] == ORPHAN_MODEL, event
    assert event["selected_model"] == ORPHAN_MODEL, event
    assert event["requester_type"] == "user", event
    assert str(event["user_id"]).lower() == ident.user_id, event
    assert str(event["request_id"]).lower() == seeded.request_id, event
    assert int(event["policy_version_applied"]) == POLICY_VERSION, event
    assert event["dedupe_key"] == (
        f"{uuid.UUID(ident.tenant_id).hex}/{uuid.UUID(seeded.turn_id).hex}/{uuid.UUID(seeded.request_id).hex}"
    ), event

    # Audit: turn_failed with an unknown quota decision.
    assert len(audit) == 1, audit
    audit_event = audit[0]
    assert audit_event["error_code"] == "orphan_timeout", audit_event
    assert audit_event["policy_decisions"]["quota"]["decision"] == "unknown", audit_event
    assert audit_event["selected_model"] == audit_event["effective_model"] == ORPHAN_MODEL, audit_event
    assert str(audit_event["turn_id"]).lower() == seeded.turn_id, audit_event
    assert str(audit_event["user_id"]).lower() == ident.user_id, audit_event

    # Turn Status and recovery: the chat accepts a new message.
    resp = mc.get(f"{CHATS}/{chat_id}/turns/{seeded.request_id}", user)
    assert resp.status_code == 200, resp.text
    status = resp.json()
    assert status["state"] == "error", status
    assert status["error_code"] == "orphan_timeout", status
    assert "assistant_message_id" not in status or status["assistant_message_id"] is None, status
    send_done(mc, chat_id, "Are you there again?", user)
    assert turn_row(mc, seeded.turn_id)["state"] == "failed", "a finalized orphan is never reopened"


def test_stale_running_turn_without_reserve_is_finalized_with_a_zero_usage_event(mc, mock):
    user = ORPHAN_USER
    ident = mc.identity(user)
    quota_before = all_quota_rows(mc, user)
    chat_id = new_chat(mc, user, ORPHAN_MODEL)

    with OutboxRecorder(mc, chat_id) as recorder:
        seeded = seed_turn(
            mc, chat_id, ident.tenant_id, ident.user_id, state="running",
            user_text="S6 orphan without reserve", model=ORPHAN_MODEL, at_ns=stale_at_ns(),
        )
        assert turn_row(mc, seeded.turn_id)["reserve_tokens"] is None, "test design: NULL reserve fields"
        make_stale(mc, seeded.turn_id)

        row = wait_turn_state(mc, seeded.turn_id, "failed")
        wait_until(lambda: recorder.usage_events(seeded.turn_id), timeout=20, interval=0.05, description="usage event")
        time.sleep(1.5)
        usage = recorder.usage_events(seeded.turn_id)

    assert row["error_code"] == "orphan_timeout", row
    assert len(usage) == 1, usage
    event = usage[0]
    assert event["billing_outcome"] == "aborted", event
    assert event["settlement_method"] == "estimated", event
    assert int(event["actual_credits_micro"]) == 0, event
    assert event["usage"] is None, event
    assert event["effective_model"] == "", event
    assert event["selected_model"] == "", event
    assert int(event["policy_version_applied"]) == 0, event
    assert all_quota_rows(mc, user) == quota_before, "no settlement without reserve fields"


def test_watchdog_leaves_recent_progress_null_progress_with_recent_start_and_deleted_turns(mc, mock):
    user = ORPHAN_USER
    ident = mc.identity(user)
    seeded: dict[str, Any] = {}
    try:
        for name in ("recent_progress", "null_progress_recent_start", "null_progress_old_start", "deleted", "control"):
            chat_id = new_chat(mc, user, ORPHAN_MODEL)
            started = stale_at_ns() if name != "null_progress_recent_start" else None
            seeded[name] = seed_turn(
                mc, chat_id, ident.tenant_id, ident.user_id, state="running",
                user_text=f"S6 watchdog {name}", model=ORPHAN_MODEL, at_ns=started,
            )
        # "recent_progress" keeps its seeded state: old start, fresh last_progress_at.
        # NULL last_progress_at falls back to started_at (recent / old).
        make_stale(mc, seeded["null_progress_recent_start"].turn_id, last_progress=None)
        make_stale(mc, seeded["null_progress_old_start"].turn_id, last_progress=None)
        # Stale and soft-deleted in one transaction (the watchdog scans every second).
        db_exec(
            mc,
            [
                (
                    "UPDATE chat_turns SET last_progress_at = ?, deleted_at = ? WHERE id = ?",
                    [db_ts(stale_at_ns()), db_ts(), uuid_bytes(seeded["deleted"].turn_id)],
                )
            ],
        )
        make_stale(mc, seeded["control"].turn_id)

        # The watchdog is active: the stale control and the NULL-progress turn
        # with an old start (falls back to started_at) are finalized.
        for name in ("control", "null_progress_old_start"):
            row = wait_turn_state(mc, seeded[name].turn_id, "failed")
            assert row["error_code"] == "orphan_timeout", (name, row)
        time.sleep(3.0)

        for name in ("recent_progress", "null_progress_recent_start", "deleted"):
            row = turn_row(mc, seeded[name].turn_id)
            assert row["state"] == "running", (name, row)
            assert row["error_code"] is None, (name, row)
            assert row["completed_at"] is None, (name, row)
    finally:
        for turn in seeded.values():
            soft_delete_turn(mc, turn.turn_id)


# --------------------------------------------------------------------------
# Upload reaper
# --------------------------------------------------------------------------


def stale_offset_ms() -> float:
    return -float(STALE_SECS * 1000)


def get_attachment(server, chat_id: str, attachment_id: str) -> dict[str, Any]:
    resp = server.get(f"{CHATS}/{chat_id}/attachments/{attachment_id}")
    assert resp.status_code == 200, resp.text
    return resp.json()


def wait_attachment_status(server, attachment_id: str, status: str, timeout: float = 30.0) -> dict[str, Any]:
    return wait_until(
        lambda: (row := attachment_row(server, attachment_id))["status"] == status and row,
        timeout=timeout,
        interval=0.2,
        description=f"attachment {attachment_id} to become {status!r}",
    )


@pytest.mark.slow
def test_stale_pending_and_uploaded_rows_are_failed_as_upload_abandoned(reaper_server, mock):
    server = reaper_server
    assert STALE_SECS > REAPER_STALE_AFTER_SECS, "test design"
    ident = server.identity("A1")
    chat_id = new_chat(server)
    seeded_at = db_ts()
    with OutboxRecorder(server, chat_id) as recorder:
        pending = seed_attachment(
            server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="pending",
            provider_file_id=None, offset_ms=stale_offset_ms(),
        )
        uploaded = seed_attachment(
            server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="uploaded",
            offset_ms=stale_offset_ms(),
        )
        provider_file_id = attachment_row(server, uploaded)["provider_file_id"]
        assert provider_file_id, "test design: the uploaded row has a provider file"

        pending_row = wait_attachment_status(server, pending, "failed")
        uploaded_row = wait_attachment_status(server, uploaded, "failed")
        done_row = wait_cleanup_status(server, uploaded, "done")
        events = wait_until(
            lambda: recorder.cleanup_events(uploaded), timeout=20, interval=0.05, description="cleanup event"
        )
        time.sleep(1.0)
        events = recorder.cleanup_events(uploaded)
        pending_events = recorder.cleanup_events(pending)

    for attachment_id, row in ((pending, pending_row), (uploaded, uploaded_row)):
        assert row["error_code"] == "upload_abandoned", row
        assert row["deleted_at"] is None, "a reaped row is not soft-deleted"
        # Seeded STALE_SECS in the past; the reaper refreshes updated_at.
        assert row["updated_at"] >= seeded_at, (row["updated_at"], seeded_at)
        detail = get_attachment(server, chat_id, attachment_id)
        assert detail["status"] == "failed", detail
        assert detail["error_code"] == "upload_abandoned", detail

    # Without a provider file: no cleanup ownership, no provider call, no message.
    assert attachment_row(server, pending)["cleanup_status"] is None
    assert pending_events == [], pending_events

    # With a provider file: attachment cleanup deletes it.
    assert done_row["cleanup_attempts"] == 0, done_row
    assert len(file_deletes(mock, provider_file_id)) == 1
    assert len(events) == 1, events
    event = events[0]
    assert event["event_type"] == "attachment_upload_abandoned", event
    assert str(event["tenant_id"]).lower() == ident.tenant_id, event
    assert str(event["chat_id"]).lower() == chat_id, event
    assert event["provider_file_id"] == provider_file_id, event
    assert event["attachment_kind"] == "document", event
    assert event["storage_backend"] == "openai", event
    assert event.get("secondary_ref") is None, event
    assert is_rfc3339(event["deleted_at"]), event


@pytest.mark.slow
def test_reaper_skips_rows_owned_by_cleanup_fresh_rows_and_deleted_rows(reaper_server, mock):
    server = reaper_server
    ident = server.identity("A1")
    chat_id = new_chat(server)
    # Seeded fresh, then owned by cleanup and made stale in one transaction.
    owned = seed_attachment(server, chat_id, ident.tenant_id, ident.user_id, kind="image", status="uploaded")
    db_exec(
        server,
        [
            (
                "UPDATE attachments SET cleanup_status = 'done', updated_at = ? WHERE id = ?",
                [db_ts(stale_at_ns()), uuid_bytes(owned)],
            )
        ],
    )
    fresh = seed_attachment(server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="pending",
                            provider_file_id=None)
    deleted = seed_attachment(
        server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="uploaded",
        offset_ms=stale_offset_ms(), deleted=True,
    )
    control = seed_attachment(
        server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="pending",
        provider_file_id=None, offset_ms=stale_offset_ms(),
    )
    before = {a: attachment_row(server, a) for a in (owned, fresh, deleted)}

    wait_attachment_status(server, control, "failed")
    time.sleep(3.0)

    for attachment_id, old in before.items():
        row = attachment_row(server, attachment_id)
        assert row["status"] == old["status"], (attachment_id, old, row)
        assert row["error_code"] is None, row
        assert row["updated_at"] == old["updated_at"], row
        assert row["cleanup_status"] == old["cleanup_status"], row
        if row["provider_file_id"]:
            assert file_deletes(mock, row["provider_file_id"]) == [], row


@pytest.mark.slow
def test_reaped_document_no_longer_counts_toward_the_document_limit(reaper_server, mock):
    server = reaper_server
    ident = server.identity("A1")
    chat_id = new_chat(server)
    stuck = seed_attachment(
        server, chat_id, ident.tenant_id, ident.user_id, kind="document", status="pending", provider_file_id=None
    )
    resp = upload(server, chat_id, text_document("s6-limit-1"), "limit-1.txt", TXT_MIME)
    assert_problem(resp, 429, category="resource_exhausted", violation_subject="document_limit")

    db_exec(
        server,
        [("UPDATE attachments SET updated_at = ? WHERE id = ?", [db_ts(stale_at_ns()), uuid_bytes(stuck)])],
    )
    row = wait_attachment_status(server, stuck, "failed")
    assert row["error_code"] == "upload_abandoned", row

    upload_document(server, chat_id, "s6-limit-2")
