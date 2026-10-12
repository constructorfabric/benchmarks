"""S5 attachments in chat turns: file search, images, code interpreter, citations, deletion.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* "File Search Tool Availability" and "Retrieval Invariants": ``file_search``
  only when the chat has at least one ready, non-deleted document, then on
  every request, as ``{"type": "file_search", "vector_store_ids": [<chat
  store>], "max_num_results": <catalog max_num_results>}``; the guard text
  ``context.file_search_guard`` is appended to the system prompt only then;
* "Provider Request Metadata" (``metadata.feature``: tool names joined with
  ``+`` in the order ``file_search``, ``web_search``, ``code_interpreter``);
* "Internal: Multimodal Input Format" and context assembly step 8: images of
  the current ``attachment_ids`` become ``input_image`` items with the
  provider file id in the user input; images of earlier turns are never sent
  again unless re-attached;
* "Code Interpreter Tool Availability" / "Code Interpreter Configuration":
  ``{"type": "code_interpreter", "container": {"type": "auto", "file_ids":
  [...]}}`` with ``include: ["code_interpreter_call.outputs"]`` when the chat
  has a ready XLSX, the model supports it and ``disable_code_interpreter`` is
  off (silently omitted otherwise); daily quota
  ``quota.code_interpreter_daily_quota`` (429 ``resource_exhausted``, scope
  ``code_interpreter``); per-message limit
  ``quota.code_interpreter_max_calls_per_message`` (SSE error
  ``code_interpreter_calls_exceeded``, turn ``failed``);
* "Provider Event Translation": code interpreter ``tool`` start / done with
  ``details.output`` (the ``logs`` outputs);
* "Citation File ID and Title Resolution" and §3.3 ``event: citations``: a
  ``file_citation`` maps to ``{source: file, attachment_id, title: filename,
  snippet: ""}`` without ``span``; unknown or deleted files are omitted; no
  provider identifier reaches the client;
* "Attachment Mutability and Deletion", "Attachment Deletion": 409
  ``already_exists`` / ``attachment_locked`` for an attachment referenced by a
  submitted message; soft delete + ``mini-chat.attachment_cleanup`` outbox
  message (``event_type = attachment_deleted``, payload table) in one
  transaction, 204; repeated ``DELETE`` 204 without a second message; 404 for
  another user's attachment (uploader check before idempotency); Phase 2
  deletes the provider file (2xx and 404 are success, no Vector Stores call),
  retries failures and marks ``cleanup_status = failed`` after
  ``cleanup_worker.max_attempts`` (message dead-lettered);
* §3.3 "List Messages": the user message lists its ``AttachmentSummary``
  items.

Attachments are created through the upload API (``POST
/v1/chats/{id}/attachments``) and referenced by ``attachment_ids`` on
``messages:stream``.
"""

from __future__ import annotations

import json
import random
import struct
import threading
import time
import zlib
from datetime import datetime, timezone
from typing import Any

import httpx
import pytest

from conftest import REPO_ROOT
from mc_helpers import (
    RT_ATTACHMENT,
    assert_problem,
    db_uuid,
    new_uuid,
    uuid_from_db,
    wait_until,
)
from mc_seed import (
    is_rfc3339,
    outbox_payloads_for_chat,
    outbox_snapshot,
    outbox_tables,
    seed_attachment,
)

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
PROVIDER_ID = "openai"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
ATTACHMENT_CLEANUP_QUEUE = "mini-chat.attachment_cleanup"
TESTDATA = REPO_ROOT / "testing" / "e2e" / "testdata"

#: Built-in ``context.file_search_guard``.
FILE_SEARCH_GUARD = (
    "Use file_search to look up information in the documents attached to this chat when the "
    "question concerns their content. Base your answer on the retrieved passages. If the documents "
    "do not contain the answer, say so instead of guessing."
)
CODE_INTERPRETER_INCLUDE = "code_interpreter_call.outputs"
CLEANUP_PAYLOAD_FIELDS = {
    "event_type",
    "tenant_id",
    "chat_id",
    "attachment_id",
    "provider_file_id",
    "vector_store_id",
    "storage_backend",
    "attachment_kind",
    "deleted_at",
    "secondary_ref",
}

PDF_MIME = "application/pdf"
TXT_MIME = "text/plain"
XLSX_MIME = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


# --------------------------------------------------------------------------
# Test data
# --------------------------------------------------------------------------


def make_png(width: int, height: int, seed: int = 3) -> bytes:
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


def load_testdata(relative: str) -> bytes:
    path = TESTDATA / relative
    if not path.is_file():
        pytest.skip(f"test data file missing: {path}")
    return path.read_bytes()


def text_document(label: str = "doc") -> bytes:
    return f"Mini-chat E2E {label}: the warranty period is 24 months.\n".encode() * 4


# --------------------------------------------------------------------------
# API helpers
# --------------------------------------------------------------------------


def _attachment_path(chat_id: str, attachment_id: str) -> str:
    return f"{CHATS}/{chat_id}/attachments/{attachment_id}"


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _new_chat(server, user: str = "A1", model: str = DEFAULT_MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S5 attachment usage", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def upload(
    server, chat_id: str, data: bytes, filename: str, content_type: str, user: str = "A1"
) -> httpx.Response:
    return server.post(
        f"{CHATS}/{chat_id}/attachments",
        user,
        files={"file": (filename, data, content_type)},
        timeout=60,
    )


def upload_ok(server, chat_id: str, data: bytes, filename: str, content_type: str, user: str = "A1") -> dict[str, Any]:
    resp = upload(server, chat_id, data, filename, content_type, user=user)
    assert resp.status_code == 201, f"upload failed: HTTP {resp.status_code} {resp.text[:1000]}"
    body = resp.json()
    assert body["status"] == "ready", body
    return body


def upload_document(server, chat_id: str, filename: str = "warranty.txt", user: str = "A1") -> dict[str, Any]:
    return upload_ok(server, chat_id, text_document(filename), filename, TXT_MIME, user=user)


def upload_image(server, chat_id: str, filename: str = "photo.png") -> dict[str, Any]:
    return upload_ok(server, chat_id, make_png(48, 32), filename, "image/png")


def upload_xlsx(server, chat_id: str, filename: str = "sales.xlsx") -> dict[str, Any]:
    return upload_ok(server, chat_id, load_testdata("xlsx/simple_data.xlsx"), filename, XLSX_MIME)


def upload_xlsx_as(server, chat_id: str, user: str) -> dict[str, Any]:
    return upload_ok(server, chat_id, load_testdata("xlsx/simple_data.xlsx"), "sheet.xlsx", XLSX_MIME, user=user)


def _attachment_row(server, attachment_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM attachments WHERE id = ?", (db_uuid(attachment_id),))
    assert len(rows) == 1, rows
    return rows[0]


def _provider_file_id(server, attachment_id: str) -> str:
    value = _attachment_row(server, attachment_id)["provider_file_id"]
    assert value, f"attachment {attachment_id} has no provider file id"
    return value


def _vector_store_id(server, chat_id: str) -> str:
    rows = server.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (db_uuid(chat_id),))
    assert len(rows) == 1 and rows[0]["vector_store_id"], rows
    return rows[0]["vector_store_id"]


def _send(server, chat_id: str, content: str, attachment_ids: list[str] | None = None, user: str = "A1"):
    request_id = new_uuid()
    body: dict[str, Any] = {"content": content, "request_id": request_id}
    if attachment_ids is not None:
        body["attachment_ids"] = attachment_ids
    result = server.stream("POST", _stream_path(chat_id), user, json=body)
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert result.terminal is not None, names
    return result, request_id


def _send_done(server, chat_id: str, content: str, attachment_ids: list[str] | None = None, user: str = "A1"):
    result, request_id = _send(server, chat_id, content, attachment_ids, user)
    assert result.terminal[0] == "done", (result.names(), result.events[-1:])
    return result, request_id


def _stream_requests(mock) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("responses") if r["stream"]]


def _last_body(mock) -> dict[str, Any]:
    requests = _stream_requests(mock)
    assert requests, "no streaming provider request recorded"
    return requests[-1]["json"]


def _tools(body: dict[str, Any], kind: str) -> list[dict[str, Any]]:
    return [t for t in body.get("tools") or [] if isinstance(t, dict) and t.get("type") == kind]


def _item_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(p.get("text", "")) for p in content if isinstance(p, dict))
    return ""


def _system_instructions(body: dict[str, Any]) -> str:
    parts = [str(body.get("instructions") or "")]
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("role") in ("system", "developer"):
            parts.append(_item_text(item.get("content")))
    return "\n".join(parts)


def _user_items(body: dict[str, Any]) -> list[dict[str, Any]]:
    return [i for i in body.get("input") or [] if isinstance(i, dict) and i.get("role") == "user"]


def _input_images(item: dict[str, Any]) -> list[str]:
    content = item.get("content")
    if not isinstance(content, list):
        return []
    return [p.get("file_id") for p in content if isinstance(p, dict) and p.get("type") == "input_image"]


def _all_input_images(body: dict[str, Any]) -> list[str]:
    return [file_id for item in body.get("input") or [] if isinstance(item, dict) for file_id in _input_images(item)]


def _turn(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", (db_uuid(chat_id), db_uuid(request_id))
    )
    assert len(rows) == 1, rows
    return rows[0]


def _list_messages(server, chat_id: str, user: str = "A1") -> list[dict[str, Any]]:
    resp = server.get(f"{CHATS}/{chat_id}/messages", user, params={"limit": 100})
    assert resp.status_code == 200, resp.text
    return resp.json()["items"]


def _sse_text(result) -> str:
    return json.dumps(result.events)


def _assert_no_ids_in_stream(result, *identifiers: str) -> None:
    text = _sse_text(result)
    for value in identifiers:
        assert value not in text, f"provider identifier {value!r} reached the client"


def _date_text(value: Any) -> str:
    if isinstance(value, (bytes, bytearray, memoryview)):
        value = bytes(value).decode("ascii")
    return str(value)[:10]


def _daily_code_interpreter_calls(server, user: str) -> int:
    ident = server.identity(user)
    today = datetime.now(timezone.utc).date().isoformat()
    rows = server.query(
        "SELECT period_start, code_interpreter_calls FROM quota_usage "
        "WHERE tenant_id = ? AND user_id = ? AND period_type = 'daily' AND bucket = 'total'",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    return sum(r["code_interpreter_calls"] for r in rows if _date_text(r["period_start"]) == today)


def _policy(**config: Any) -> dict[str, Any]:
    return {"gears": {POLICY_PLUGIN: {"config": config}}}


def _gear(**config: Any) -> dict[str, Any]:
    return {"gears": {"mini-chat": {"config": config}}}


def _entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the server catalog")


class CleanupRecorder:
    """Polls the outbox bodies of one chat in a background thread (bodies are vacuumed after ack)."""

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="cleanup-recorder", daemon=True)

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

    def __enter__(self) -> "CleanupRecorder":
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def cleanup_payloads(self, attachment_id: str) -> list[dict[str, Any]]:
        self.poll()
        with self._lock:
            return [
                p
                for p in self._seen.values()
                if str(p.get("attachment_id", "")).lower() == attachment_id.lower() and "event_type" in p
            ]

    def wait_cleanup_payload(self, attachment_id: str) -> dict[str, Any]:
        found = wait_until(
            lambda: self.cleanup_payloads(attachment_id),
            timeout=15,
            interval=0.05,
            description=f"attachment cleanup payload of {attachment_id}",
        )
        assert len(found) == 1, found
        return found[0]


def _wait_cleanup_status(server, attachment_id: str, status: str, timeout: float = 30.0) -> dict[str, Any]:
    return wait_until(
        lambda: (row := _attachment_row(server, attachment_id))["cleanup_status"] == status and row,
        timeout=timeout,
        interval=0.2,
        description=f"cleanup_status {status!r} of attachment {attachment_id}",
    )


def _wait_cleanups_settled(server, timeout: float = 30.0) -> None:
    """No attachment cleanup of an earlier test is still pending (rows of
    deleted chats belong to chat cleanup and are not waited for).

    Scripted ``files_delete`` failures are consumed by whichever delete comes
    next, so a cleanup left running by an earlier test (for example
    ``test_deleting_the_only_document_removes_file_search``) must finish first.
    """
    wait_until(
        lambda: server.query(
            "SELECT COUNT(*) AS n FROM attachments a JOIN chats c ON c.id = a.chat_id "
            "WHERE a.cleanup_status = 'pending' AND c.deleted_at IS NULL"
        )[0]["n"]
        == 0,
        timeout=timeout,
        interval=0.2,
        description="pending attachment cleanups of earlier tests",
    )


def _file_deletes(mock, provider_file_id: str) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("files_delete") if r["path"].endswith(f"/files/{provider_file_id}")]


def _json_payload(raw: Any) -> Any:
    if isinstance(raw, (bytes, bytearray, memoryview)):
        raw = bytes(raw).decode("utf-8", errors="replace")
    try:
        return json.loads(str(raw))
    except json.JSONDecodeError:
        return None


def _dead_letter_payloads(server) -> list[Any]:
    found: list[Any] = []
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
            found.extend(_json_payload(raw) for (raw,) in conn.execute(f"SELECT payload FROM {tables.dead_letters}"))
    finally:
        conn.close()
    return found


# --------------------------------------------------------------------------
# File search
# --------------------------------------------------------------------------


def test_document_turn_sends_file_search_tool_guard_and_feature(mc, mock):
    entry = _entry(mc, DEFAULT_MODEL)
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id, "warranty.txt")
    vector_store_id = _vector_store_id(mc, chat_id)

    result, request_id = _send_done(mc, chat_id, "How long is the warranty?", [doc["id"]])

    body = _last_body(mock)
    assert _tools(body, "file_search") == [
        {"type": "file_search", "vector_store_ids": [vector_store_id], "max_num_results": entry["max_num_results"]}
    ], body.get("tools")
    assert {t.get("type") for t in body.get("tools") or []} == {"file_search"}, body.get("tools")
    assert FILE_SEARCH_GUARD in _system_instructions(body)
    assert body["metadata"]["feature"] == "file_search", body["metadata"]
    assert _all_input_images(body) == [], "documents are not sent as input items"
    _assert_no_ids_in_stream(result, _provider_file_id(mc, doc["id"]), vector_store_id)

    messages = _list_messages(mc, chat_id)
    user = next(m for m in messages if m["role"] == "user" and m["request_id"] == request_id)
    assert len(user["attachments"]) == 1, user
    summary = user["attachments"][0]
    assert {k: v for k, v in summary.items() if v is not None} == {
        "attachment_id": doc["id"],
        "kind": "document",
        "filename": "warranty.txt",
        "status": "ready",
    }, summary
    links = mc.query("SELECT attachment_id FROM message_attachments WHERE chat_id = ?", (db_uuid(chat_id),))
    assert [link["attachment_id"] for link in links] == [doc["id"]], links

    # Every later request of the chat carries the tool, referenced or not.
    _send_done(mc, chat_id, "And what about returns?")
    later = _last_body(mock)
    assert _tools(later, "file_search") == _tools(body, "file_search"), later.get("tools")
    assert FILE_SEARCH_GUARD in _system_instructions(later)


def test_chat_without_documents_sends_no_file_search(mc, mock):
    chat_id = _new_chat(mc)
    _send_done(mc, chat_id, "No documents here")
    body = _last_body(mock)
    assert _tools(body, "file_search") == [], body.get("tools")
    assert FILE_SEARCH_GUARD not in _system_instructions(body)

    # An image is not a document: still no file_search.
    upload_image(mc, chat_id)
    _send_done(mc, chat_id, "Still no documents")
    body = _last_body(mock)
    assert _tools(body, "file_search") == [], body.get("tools")
    assert FILE_SEARCH_GUARD not in _system_instructions(body)
    assert body["metadata"]["feature"] == "none", body["metadata"]


def test_file_search_and_code_interpreter_feature_label(mc, mock):
    chat_id = _new_chat(mc)
    upload_document(mc, chat_id)
    upload_xlsx(mc, chat_id)

    _send_done(mc, chat_id, "Compare the sheet with the document")

    body = _last_body(mock)
    assert {t.get("type") for t in body.get("tools") or []} == {"file_search", "code_interpreter"}, body["tools"]
    assert body["metadata"]["feature"] == "file_search+code_interpreter", body["metadata"]


# --------------------------------------------------------------------------
# Images
# --------------------------------------------------------------------------


def test_image_is_sent_only_with_the_turn_that_attaches_it(mc, mock):
    chat_id = _new_chat(mc)
    image = upload_image(mc, chat_id, "diagram.png")
    provider_file_id = _provider_file_id(mc, image["id"])

    result, request_id = _send_done(mc, chat_id, "What is in this picture?", [image["id"]])

    body = _last_body(mock)
    current = _user_items(body)[-1]
    assert current["content"][0] == {"type": "input_text", "text": "What is in this picture?"}, current
    assert _input_images(current) == [provider_file_id], current
    assert _all_input_images(body) == [provider_file_id], body["input"]
    assert _tools(body, "file_search") == [], body.get("tools")
    _assert_no_ids_in_stream(result, provider_file_id)

    messages = _list_messages(mc, chat_id)
    user = next(m for m in messages if m["role"] == "user" and m["request_id"] == request_id)
    assert len(user["attachments"]) == 1, user
    summary = user["attachments"][0]
    assert (summary["attachment_id"], summary["kind"], summary["filename"], summary["status"]) == (
        image["id"],
        "image",
        "diagram.png",
        "ready",
    ), summary
    assert summary.get("img_thumbnail") == image["img_thumbnail"], summary

    # The next turn does not re-send the image.
    _send_done(mc, chat_id, "Tell me more about it")
    assert _all_input_images(_last_body(mock)) == [], _last_body(mock)["input"]

    # Re-attaching it sends it again, once, with the current message only.
    _send_done(mc, chat_id, "Look at it again", [image["id"]])
    body = _last_body(mock)
    assert _input_images(_user_items(body)[-1]) == [provider_file_id], body["input"]
    assert _all_input_images(body) == [provider_file_id], body["input"]


def test_document_and_image_in_one_turn(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id)
    image = upload_image(mc, chat_id)

    _send_done(mc, chat_id, "Does the picture match the document?", [doc["id"], image["id"]])

    body = _last_body(mock)
    assert _all_input_images(body) == [_provider_file_id(mc, image["id"])], body["input"]
    assert len(_tools(body, "file_search")) == 1, body.get("tools")


# --------------------------------------------------------------------------
# Code interpreter
# --------------------------------------------------------------------------


def test_ready_xlsx_sends_code_interpreter_tool_with_container_files(mc, mock):
    chat_id = _new_chat(mc)
    sheet = upload_xlsx(mc, chat_id)
    provider_file_id = _provider_file_id(mc, sheet["id"])

    result, _ = _send_done(mc, chat_id, "Sum the revenue column")

    body = _last_body(mock)
    assert _tools(body, "code_interpreter") == [
        {"type": "code_interpreter", "container": {"type": "auto", "file_ids": [provider_file_id]}}
    ], body.get("tools")
    assert _tools(body, "file_search") == [], "an XLSX is not in a vector store"
    assert CODE_INTERPRETER_INCLUDE in (body.get("include") or []), body.get("include")
    assert body["metadata"]["feature"] == "code_interpreter", body["metadata"]
    assert FILE_SEARCH_GUARD not in _system_instructions(body)
    _assert_no_ids_in_stream(result, provider_file_id)

    # A second spreadsheet joins the container.
    second = upload_xlsx(mc, chat_id, "costs.xlsx")
    _send_done(mc, chat_id, "Now compare both sheets")
    tool = _tools(_last_body(mock), "code_interpreter")
    assert len(tool) == 1, tool
    assert sorted(tool[0]["container"]["file_ids"]) == sorted(
        [provider_file_id, _provider_file_id(mc, second["id"])]
    ), tool


def test_code_interpreter_tool_events_are_streamed_and_counted(mc, mock):
    chat_id = _new_chat(mc)
    upload_xlsx(mc, chat_id)
    before = _daily_code_interpreter_calls(mc, "A1")
    mock.enqueue(
        {
            "text": "The total revenue is 42.",
            "tool_events": [{"type": "code_interpreter", "logs": "total = 42", "code": "print(sum(df.revenue))"}],
        }
    )

    result, request_id = _send_done(mc, chat_id, "What is the total revenue?")

    assert result.of("tool") == [
        {"phase": "start", "name": "code_interpreter", "details": {}},
        {"phase": "done", "name": "code_interpreter", "details": {"output": "total = 42"}},
    ], result.of("tool")
    names = result.names()
    assert names.index("tool") < names.index("delta"), names
    assert result.text_content() == "The total revenue is 42."
    turn = _turn(mc, chat_id, request_id)
    assert turn["state"] == "completed" and turn["code_interpreter_completed_count"] == 1, turn
    wait_until(
        lambda: _daily_code_interpreter_calls(mc, "A1") == before + 1,
        timeout=10,
        description="daily code_interpreter_calls counter",
    )


def test_code_interpreter_calls_over_per_message_limit_fail_the_turn(mc, mock):
    limit = int(mc.gear_config.get("quota", {}).get("code_interpreter_max_calls_per_message", 10))
    chat_id = _new_chat(mc)
    upload_xlsx(mc, chat_id)
    mock.enqueue(
        {"text": "Too much code.", "tool_events": [{"type": "code_interpreter", "count": limit + 1, "logs": "x"}]}
    )

    result, request_id = _send(mc, chat_id, "Run a lot of code")

    terminal, error = result.terminal
    assert terminal == "error", result.names()
    assert error["code"] == "code_interpreter_calls_exceeded", error
    assert "done" not in result.names(), result.names()
    assert {t["name"] for t in result.of("tool")} == {"code_interpreter"}, result.of("tool")
    turn = _turn(mc, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "code_interpreter_calls_exceeded"), turn


@pytest.mark.slow
def test_code_interpreter_daily_quota_rejects_turns_that_include_the_tool(start_server, mock):
    server = start_server(_gear(quota={"code_interpreter_daily_quota": 1}), name="ci-daily-quota")
    chat_id = _new_chat(server)
    upload_xlsx(server, chat_id)
    mock.enqueue({"text": "Ran once.", "tool_events": [{"type": "code_interpreter", "logs": "1"}]})
    _send_done(server, chat_id, "Use the code interpreter once")
    wait_until(lambda: _daily_code_interpreter_calls(server, "A1") == 1, timeout=10, description="daily counter")
    requests_before = len(_stream_requests(mock))

    resp = server.post(_stream_path(chat_id), "A1", json={"content": "Again please", "request_id": new_uuid()})

    problem = assert_problem(resp, 429, category="resource_exhausted", violation_subject="code_interpreter")
    violation = problem["context"]["violations"][0]
    assert (violation["subject"], violation["description"]) == ("code_interpreter", "quota_exceeded"), problem
    assert len(_stream_requests(mock)) == requests_before

    # A chat without spreadsheets does not include the tool and is not checked.
    plain_chat = _new_chat(server)
    _send_done(server, plain_chat, "No code needed")
    assert _tools(_last_body(mock), "code_interpreter") == []

    # The quota is per user.
    other_chat = _new_chat(server, "A2")
    upload_xlsx_as(server, other_chat, "A2")
    _send_done(server, other_chat, "My own spreadsheet", user="A2")
    assert len(_tools(_last_body(mock), "code_interpreter")) == 1


@pytest.mark.slow
def test_disable_code_interpreter_omits_the_tool_at_stream_time(start_server, mock):
    first = start_server(name="ci-before-kill-switch")
    chat_id = _new_chat(first)
    sheet = upload_xlsx(first, chat_id)
    doc = upload_document(first, chat_id)
    home_dir = str(first.home_dir)
    first.stop()

    second = start_server(
        {"server": {"home_dir": home_dir}, **_policy(kill_switches={"disable_code_interpreter": True})},
        name="ci-kill-switch",
    )
    assert second.db_path == first.db_path

    result, request_id = _send_done(second, chat_id, "Sum the sheet", [sheet["id"], doc["id"]])

    body = _last_body(mock)
    assert _tools(body, "code_interpreter") == [], body.get("tools")
    assert CODE_INTERPRETER_INCLUDE not in (body.get("include") or []), body.get("include")
    assert len(_tools(body, "file_search")) == 1, "other tools are unaffected"
    assert body["metadata"]["feature"] == "file_search", body["metadata"]
    assert _turn(second, chat_id, request_id)["state"] == "completed"


# --------------------------------------------------------------------------
# File citations
# --------------------------------------------------------------------------


def test_file_citation_maps_to_attachment_and_filename(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id, "Warranty Terms.txt")
    provider_file_id = _provider_file_id(mc, doc["id"])
    text = "The warranty lasts 24 months."
    mock.enqueue(
        {
            "text": text,
            "tool_events": [{"type": "file_search", "results": 1, "file_ids": [provider_file_id]}],
            "annotations": [
                {"type": "file_citation", "file_id": provider_file_id, "filename": "provider-name.txt", "index": 10},
                {"type": "file_citation", "file_id": "file-UnknownToMiniChat0000", "filename": "x.txt", "index": 12},
            ],
        }
    )

    result, _ = _send_done(mc, chat_id, "How long is the warranty?", [doc["id"]])

    names = result.names()
    assert names.count("citations") == 1 and names.index("citations") == len(names) - 2, names
    items = result.first("citations")["items"]
    assert len(items) == 1, f"the unknown file is omitted: {items}"
    item = items[0]
    assert {k: v for k, v in item.items() if v is not None} == {
        "source": "file",
        "attachment_id": doc["id"],
        "title": "Warranty Terms.txt",
        "snippet": "",
    }, item
    assert "span" not in item and "score" not in item, item
    tools = result.of("tool")
    assert [t["phase"] for t in tools] == ["start", "done"] and {t["name"] for t in tools} == {"file_search"}, tools
    _assert_no_ids_in_stream(result, provider_file_id, "file-UnknownToMiniChat0000", _vector_store_id(mc, chat_id))


def test_only_unknown_file_citations_send_no_citations_event(mc, mock):
    chat_id = _new_chat(mc)
    upload_document(mc, chat_id)
    mock.enqueue(
        {
            "text": "Answer with a foreign source.",
            "annotations": [{"type": "file_citation", "file_id": "file-ForeignFile000000000001", "index": 3}],
        }
    )

    result, _ = _send_done(mc, chat_id, "Cite something")

    assert "citations" not in result.names(), result.names()
    _assert_no_ids_in_stream(result, "file-ForeignFile000000000001")


def test_citation_of_a_deleted_attachment_is_omitted(mc, mock):
    chat_id = _new_chat(mc)
    kept = upload_document(mc, chat_id, "kept.txt")
    removed = upload_document(mc, chat_id, "removed.txt")
    kept_file = _provider_file_id(mc, kept["id"])
    removed_file = _provider_file_id(mc, removed["id"])
    assert mc.delete(_attachment_path(chat_id, removed["id"]), "A1").status_code == 204
    mock.enqueue(
        {
            "text": "Both documents say so.",
            "annotations": [
                {"type": "file_citation", "file_id": removed_file, "index": 4},
                {"type": "file_citation", "file_id": kept_file, "index": 9},
            ],
        }
    )

    result, _ = _send_done(mc, chat_id, "What do the documents say?")

    items = result.first("citations")["items"]
    assert [(i["attachment_id"], i["title"]) for i in items] == [(kept["id"], "kept.txt")], items
    _assert_no_ids_in_stream(result, removed_file, kept_file)


# --------------------------------------------------------------------------
# Deletion
# --------------------------------------------------------------------------


@pytest.mark.parametrize("kind", ["document", "image"])
def test_delete_unreferenced_attachment_soft_deletes_and_cleans_up_the_provider_file(mc, mock, kind):
    ident = mc.identity("A1")
    chat_id = _new_chat(mc)
    attachment = upload_document(mc, chat_id) if kind == "document" else upload_image(mc, chat_id)
    attachment_id = attachment["id"]
    provider_file_id = _provider_file_id(mc, attachment_id)
    vector_store_id = _vector_store_id(mc, chat_id) if kind == "document" else None
    queue_before = outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity

    with CleanupRecorder(mc, chat_id) as recorder:
        resp = mc.delete(_attachment_path(chat_id, attachment_id), "A1")
        assert resp.status_code == 204, resp.text
        assert resp.content == b""
        payload = recorder.wait_cleanup_payload(attachment_id)

    assert set(payload) == CLEANUP_PAYLOAD_FIELDS, payload
    assert payload["event_type"] == "attachment_deleted", payload
    assert payload["tenant_id"] == ident.tenant_id and payload["chat_id"] == chat_id, payload
    assert payload["attachment_id"] == attachment_id, payload
    assert payload["provider_file_id"] == provider_file_id, payload
    assert payload["vector_store_id"] is None, payload
    assert payload["storage_backend"] == PROVIDER_ID, payload
    assert payload["attachment_kind"] == kind, payload
    assert is_rfc3339(payload["deleted_at"]), payload
    assert payload["secondary_ref"] is None, payload
    assert outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity == queue_before + 1

    row = _attachment_row(mc, attachment_id)
    assert row["deleted_at"] is not None, row
    assert row["cleanup_status"] in ("pending", "done"), row
    assert_problem(mc.get(_attachment_path(chat_id, attachment_id), "A1"), 404, category="not_found",
                   resource_type=RT_ATTACHMENT)

    wait_until(lambda: _file_deletes(mock, provider_file_id), timeout=30, description="provider file delete")
    row = _wait_cleanup_status(mc, attachment_id, "done")
    assert row["cleanup_updated_at"] is not None, row
    assert provider_file_id not in mock.files
    assert [
        r for r in mock.requests_for("vector_store_file_delete") if r["path"].endswith(f"/files/{provider_file_id}")
    ] == [], "no Vector Stores call on attachment cleanup"
    if vector_store_id is not None:
        assert [
            r for r in mock.requests_for("vector_store_delete") if r["path"].endswith(f"/vector_stores/{vector_store_id}")
        ] == [], "the chat vector store is only deleted with the chat"
        assert vector_store_id in mock.vector_stores

    # Repeated DELETE: 204, no second outbox message.
    again = mc.delete(_attachment_path(chat_id, attachment_id), "A1")
    assert again.status_code == 204, again.text
    time.sleep(0.5)
    assert outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity == queue_before + 1
    assert len(_file_deletes(mock, provider_file_id)) == 1


def test_deleting_the_only_document_removes_file_search(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id)
    _send_done(mc, chat_id, "Before deletion")
    assert len(_tools(_last_body(mock), "file_search")) == 1

    assert mc.delete(_attachment_path(chat_id, doc["id"]), "A1").status_code == 204
    _send_done(mc, chat_id, "After deletion")

    body = _last_body(mock)
    assert _tools(body, "file_search") == [], body.get("tools")
    assert FILE_SEARCH_GUARD not in _system_instructions(body)
    assert body["metadata"]["feature"] == "none", body["metadata"]


def test_delete_attachment_referenced_by_a_message_is_attachment_locked(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id)
    _send_done(mc, chat_id, "Read this", [doc["id"]])
    queue_before = outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity

    resp = mc.delete(_attachment_path(chat_id, doc["id"]), "A1")

    assert_problem(resp, 409, category="already_exists", resource_name="attachment_locked")
    row = _attachment_row(mc, doc["id"])
    assert row["deleted_at"] is None and row["cleanup_status"] is None, row
    assert outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity == queue_before
    got = mc.get(_attachment_path(chat_id, doc["id"]), "A1")
    assert got.status_code == 200 and got.json()["status"] == "ready", got.text


@pytest.mark.parametrize("user", ["A2", "B1"])
def test_delete_attachment_in_another_users_chat_is_not_found(mc, mock, user):
    chat_id = _new_chat(mc, "A1")
    doc = upload_document(mc, chat_id)

    resp = mc.delete(_attachment_path(chat_id, doc["id"]), user)

    assert_problem(resp, 404, category="not_found")
    assert _attachment_row(mc, doc["id"])["deleted_at"] is None


def test_delete_attachment_uploaded_by_another_user_is_attachment_not_found(mc, mock):
    a2 = mc.identity("A2")
    chat_id = _new_chat(mc, "A1")
    foreign = seed_attachment(mc, chat_id, a2.tenant_id, a2.user_id, filename="a2.pdf")
    foreign_deleted = seed_attachment(mc, chat_id, a2.tenant_id, a2.user_id, filename="gone.pdf", deleted=True)
    queue_before = outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity

    resp = mc.delete(_attachment_path(chat_id, foreign), "A1")
    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)
    assert _attachment_row(mc, foreign)["deleted_at"] is None

    # The uploader check runs before the idempotency check.
    resp = mc.delete(_attachment_path(chat_id, foreign_deleted), "A1")
    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)
    assert outbox_snapshot(mc, ATTACHMENT_CLEANUP_QUEUE).activity == queue_before


def test_delete_unknown_or_foreign_chat_attachment_is_attachment_not_found(mc, mock):
    chat_id = _new_chat(mc)
    other_chat = _new_chat(mc)
    doc = upload_document(mc, other_chat)

    assert_problem(
        mc.delete(_attachment_path(chat_id, new_uuid()), "A1"), 404, category="not_found", resource_type=RT_ATTACHMENT
    )
    assert_problem(
        mc.delete(_attachment_path(chat_id, doc["id"]), "A1"), 404, category="not_found", resource_type=RT_ATTACHMENT
    )
    assert _attachment_row(mc, doc["id"])["deleted_at"] is None
    resp = mc.delete(f"{CHATS}/{chat_id}/attachments/not-a-uuid", "A1")
    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_failed_provider_delete_is_retried_until_it_succeeds(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id)
    provider_file_id = _provider_file_id(mc, doc["id"])
    _wait_cleanups_settled(mc)
    mock.fail_next("files_delete", 500, times=2)

    assert mc.delete(_attachment_path(chat_id, doc["id"]), "A1").status_code == 204

    row = _wait_cleanup_status(mc, doc["id"], "done", timeout=60)
    deletes = _file_deletes(mock, provider_file_id)
    assert len(deletes) == 3, f"two failed attempts and one success: {len(deletes)}"
    assert row["cleanup_attempts"] >= 2, row
    assert provider_file_id not in mock.files


def test_provider_not_found_on_delete_counts_as_success(mc, mock):
    chat_id = _new_chat(mc)
    doc = upload_document(mc, chat_id)
    provider_file_id = _provider_file_id(mc, doc["id"])
    _wait_cleanups_settled(mc)
    mock.fail_next("files_delete", 404)

    assert mc.delete(_attachment_path(chat_id, doc["id"]), "A1").status_code == 204

    row = _wait_cleanup_status(mc, doc["id"], "done")
    assert len(_file_deletes(mock, provider_file_id)) == 1, "404 is success, no retry"
    assert row["cleanup_attempts"] == 0, row


@pytest.mark.slow
def test_provider_delete_failing_max_attempts_times_marks_cleanup_failed(start_server, mock):
    server = start_server(_gear(cleanup_worker={"max_attempts": 2}), name="cleanup-max-attempts")
    chat_id = _new_chat(server)
    doc = upload_document(server, chat_id)
    provider_file_id = _provider_file_id(server, doc["id"])
    mock.fail_next("files_delete", 500, times=10)

    assert server.delete(_attachment_path(chat_id, doc["id"]), "A1").status_code == 204

    row = _wait_cleanup_status(server, doc["id"], "failed", timeout=60)
    assert row["cleanup_attempts"] == 2, row
    assert row["last_cleanup_error"], row
    assert row["cleanup_updated_at"] is not None, row
    time.sleep(2.0)
    assert len(_file_deletes(mock, provider_file_id)) == 2, "no attempt after the limit"
    assert _attachment_row(server, doc["id"])["cleanup_status"] == "failed"

    dead = wait_until(
        lambda: [
            p
            for p in _dead_letter_payloads(server)
            if isinstance(p, dict) and str(p.get("attachment_id", "")).lower() == doc["id"]
        ],
        timeout=15,
        description="dead-lettered attachment cleanup message",
    )
    assert dead[0]["event_type"] == "attachment_deleted", dead
    assert uuid_from_db(dead[0]["chat_id"]) == chat_id, dead
