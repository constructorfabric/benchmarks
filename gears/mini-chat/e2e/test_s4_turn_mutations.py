"""S4 turn mutations: retry, edit and delete of the latest turn.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.9 "Turn Mutation Rules (P1)": definition of the latest turn (greatest
  ``(started_at, id)`` among non-deleted turns), allowed operations (retry /
  edit soft-delete the old turn and set ``replaced_by_request_id``, delete
  only sets ``deleted_at``; attachment links are copied, deleted attachments
  excluded, images re-sent with the image guards), rules 1-9 (``NOT_LATEST_TURN``
  also for a soft-deleted target or a newer running turn, ``turn_state`` /
  ``STATE`` for a running target checked before the latest-turn check,
  server-generated UUID v4 ``request_id``, old ``request_id`` -> 409
  ``request_id_conflict`` on ``messages:stream``, soft-deleted turn -> 404 on
  Turn Status), the API contracts (``POST .../retry`` no body, ``PATCH`` with
  ``EditTurnRequest {content}``, ``DELETE`` -> 204), "Summary Interaction on
  Turn Mutation" and "Audit Events for Turn Mutations" (``turn_retry``,
  ``turn_edit``, ``turn_delete`` on the ``mini-chat.audit`` outbox queue);
* §3.6 "Retry / edit variant" (preview -> preflight -> commit -> context /
  provider / reserve -> stream; same SSE contract as ``messages:stream``);
* §3.3 endpoint list and REST error table (``EMPTY_CONTENT``, 422
  ``invalid_json_body``, ``invalid_path_params``, ``not_found`` resource
  types, ``failed_precondition`` ``turn_state`` / ``STATE``, ``aborted``
  ``NOT_LATEST_TURN``, ``FEATURE_DISABLED``);
* §3.7 ``chats.updated_at`` (bumped in the retry / edit transaction),
  ``chat_turns`` (``deleted_at``, ``replaced_by_request_id``),
  ``message_attachments`` (copy on retry / edit), ``thread_summaries``
  (frontier ``(summarized_up_to_created_at, summarized_up_to_message_id)``);
* "Context Plan Assembly" (summary replaces covered history, images as
  ``input_image`` items with the internal ``provider_file_id``) and
  "Internal: Multimodal Input Format";
* api.json ``mini_chat.retry_turn``, ``mini_chat.edit_turn`` and
  ``mini_chat.delete_turn``.

The pipeline order (preflight before the mutation, setup failure after the
commit), concurrency, billing and the requester check are covered by
``test_s4_mutation_pipeline.py``, which imports the helpers of this module.
"""

from __future__ import annotations

import json
import re
import threading
import time
import uuid
from typing import Any, Callable, Iterator

import httpx
import pytest

from conftest import create_server
from mc_helpers import (
    RT_CHAT,
    RT_TURN,
    SseResult,
    assert_problem,
    db_uuid,
    is_uuid,
    new_uuid,
    parse_sse_lines,
    uuid_bytes,
    wait_until,
)
from mc_seed import (
    SeededTurn,
    db_ts,
    is_rfc3339,
    link_message_attachment,
    outbox_payloads_for_chat,
    parse_ts_ns,
    seed_attachment,
    seed_turn,
)

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
AUDIT_QUEUE = "mini-chat.audit"
USAGE_QUEUE = "mini-chat.usage_snapshot"
EDIT_TEXT = "new text"
OPS = ("retry", "edit", "delete")
STREAMING_OPS = ("retry", "edit")

STREAM_STARTED_FIELDS = {"request_id", "message_id", "is_new_turn", "thread_summary_applied"}
DONE_FIELDS = {
    "usage",
    "effective_model",
    "selected_model",
    "quota_decision",
    "downgrade_from",
    "downgrade_reason",
    "quota_warnings",
}
#: ``stream_started ping* delta+ done`` for a text-only completion.
TEXT_STREAM_ORDER = re.compile(r"^stream_started( ping)*( delta)+ done$")
#: Keeps a turn running (up to ~30 s); a client disconnect is noticed at the
#: next delta, so the turn leaves ``running`` quickly after the release.
SLOW_STREAM = {"chunks": [f"slow-{i:03d} " for i in range(100)], "delay_ms": 300}

#: Two-turn conversation used by most tests.
Q1, A1 = "What color are apples?", "Apples are red."
Q2, A2 = "And bananas?", "Bananas are yellow."


# --------------------------------------------------------------------------
# Paths and HTTP helpers
# --------------------------------------------------------------------------


def stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def retry_path(chat_id: str, request_id: str) -> str:
    return f"{turn_path(chat_id, request_id)}/retry"


def new_chat(server, user: str = "A1", model: str = DEFAULT_MODEL) -> dict[str, Any]:
    resp = server.post(CHATS, user, json={"title": "S4 turn mutations", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()


def assert_sse_done(result: SseResult) -> dict[str, Any]:
    """A complete text stream with the ``messages:stream`` contract; returns ``stream_started``."""
    assert result.status_code == 200, f"HTTP {result.status_code}: {result.text[:1000]}"
    assert result.is_sse, f"expected an SSE stream: {result.headers}"
    names = " ".join(result.names())
    assert TEXT_STREAM_ORDER.match(names), f"unexpected event order: {names} {result.events[-1:]}"
    started = result.first("stream_started")
    assert isinstance(started, dict), started
    assert {"request_id", "message_id", "is_new_turn"} <= set(started), started
    assert set(started) <= STREAM_STARTED_FIELDS, started
    assert is_uuid(started["request_id"]) and is_uuid(started["message_id"]), started
    done = result.first("done")
    assert {"usage", "effective_model", "selected_model", "quota_decision"} <= set(done), done
    assert set(done) <= DONE_FIELDS, done
    return started


def send_done(server, chat_id: str, content: str, user: str = "A1") -> tuple[SseResult, str]:
    """Send a message with a client ``request_id`` and require a ``done`` stream."""
    request_id = new_uuid()
    result = server.stream(
        "POST", stream_path(chat_id), user, json={"content": content, "request_id": request_id}
    )
    started = assert_sse_done(result)
    assert started["request_id"] == request_id, started
    return result, request_id


def mutate(server, op: str, chat_id: str, request_id: str, user: str | None = "A1", body: Any = None) -> SseResult:
    """Retry (``POST .../retry``), edit (``PATCH``) or delete (``DELETE``) a turn."""
    if op == "retry":
        return server.stream("POST", retry_path(chat_id, request_id), user)
    if op == "edit":
        payload = {"content": EDIT_TEXT} if body is None else body
        return server.stream("PATCH", turn_path(chat_id, request_id), user, json=payload)
    if op == "delete":
        return server.stream("DELETE", turn_path(chat_id, request_id), user)
    raise ValueError(op)


def assert_rejected(result: SseResult, status: int, category: str | None = None, **problem: Any) -> dict[str, Any]:
    """A JSON Problem answer (no SSE stream was opened)."""
    assert not result.is_sse, f"expected a JSON {status}, got an SSE stream: {result.names()}"
    assert result.response is not None
    return assert_problem(result.response, status, category, **problem)


def assert_deleted_204(result: SseResult) -> None:
    assert result.status_code == 204, f"HTTP {result.status_code}: {result.text[:1000]}"
    assert result.text == "", f"204 must have no body: {result.text[:200]!r}"


def turn_status(server, chat_id: str, request_id: str, user: str = "A1") -> httpx.Response:
    return server.get(turn_path(chat_id, request_id), user)


def list_messages(server, chat_id: str, user: str = "A1") -> list[dict[str, Any]]:
    resp = server.get(f"{CHATS}/{chat_id}/messages", user, params={"limit": 100})
    assert resp.status_code == 200, resp.text
    return resp.json()["items"]


def chat_updated_at(server, chat_id: str, user: str = "A1") -> str:
    resp = server.get(f"{CHATS}/{chat_id}", user)
    assert resp.status_code == 200, resp.text
    return resp.json()["updated_at"]


def two_turn_chat(server, mock, user: str = "A1", model: str = DEFAULT_MODEL) -> tuple[str, str, str]:
    """A chat with two completed turns (Q1/A1, Q2/A2); returns ``(chat_id, rid1, rid2)``."""
    chat_id = new_chat(server, user, model)["id"]
    mock.enqueue({"chunks": ["Apples ", "are red."]}, {"chunks": ["Bananas ", "are yellow."]})
    _, rid1 = send_done(server, chat_id, Q1, user)
    _, rid2 = send_done(server, chat_id, Q2, user)
    return chat_id, rid1, rid2


# --------------------------------------------------------------------------
# Provider request helpers
# --------------------------------------------------------------------------


def stream_requests(mock) -> list[dict[str, Any]]:
    """Streaming Responses API calls recorded by the mock (thread summaries excluded)."""
    return [r for r in mock.requests("/responses", method="POST") if r.get("stream")]


def _item_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            str(part.get("text", "")) for part in content if isinstance(part, dict) and "text" in part
        )
    raise AssertionError(f"unexpected input content shape: {content!r}")


def conversation(body: dict[str, Any]) -> list[tuple[str, str]]:
    """``(role, text)`` of the non-system input items, in request order."""
    items = body.get("input")
    assert isinstance(items, list) and items, f"provider request has no input items: {body}"
    return [
        (item.get("role"), _item_text(item.get("content")))
        for item in items
        if item.get("role") not in ("system", "developer")
    ]


def last_user_images(body: dict[str, Any]) -> list[str]:
    """``file_id`` of the ``input_image`` parts of the last user input item."""
    users = [i for i in body.get("input") or [] if i.get("role") == "user"]
    assert users, body
    content = users[-1].get("content")
    if not isinstance(content, list):
        return []
    return [p.get("file_id") for p in content if isinstance(p, dict) and p.get("type") == "input_image"]


# --------------------------------------------------------------------------
# DB helpers
# --------------------------------------------------------------------------


def turn_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id", (db_uuid(chat_id),)
    )


def turn_row(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def live_turns(server, chat_id: str) -> list[dict[str, Any]]:
    """Non-deleted turns, latest first (DESIGN: greatest ``(started_at, id)``)."""
    return server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND deleted_at IS NULL "
        "ORDER BY started_at DESC, id DESC",
        (db_uuid(chat_id),),
    )


def message_rows(server, chat_id: str, request_id: str | None = None) -> list[dict[str, Any]]:
    sql = "SELECT * FROM messages WHERE chat_id = ?"
    params: list[Any] = [db_uuid(chat_id)]
    if request_id is not None:
        sql += " AND request_id = ?"
        params.append(db_uuid(request_id))
    return server.query(sql + " ORDER BY created_at, id", params)


def link_rows(server, message_id: str) -> set[str]:
    rows = server.query(
        "SELECT attachment_id FROM message_attachments WHERE message_id = ?", (db_uuid(message_id),)
    )
    return {r["attachment_id"] for r in rows}


def summary_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (db_uuid(chat_id),))


def snapshot(server, chat_id: str) -> dict[str, Any]:
    """Mutation-relevant state of a chat (compared before / after a rejection)."""
    cid = db_uuid(chat_id)
    return {
        "turns": server.query(
            "SELECT id, request_id, state, error_code, deleted_at, replaced_by_request_id, completed_at, "
            "reserve_tokens FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id",
            (cid,),
        ),
        "messages": server.query(
            "SELECT id, request_id, role, content, is_compressed, deleted_at FROM messages "
            "WHERE chat_id = ? ORDER BY created_at, id",
            (cid,),
        ),
        "links": server.query(
            "SELECT message_id, attachment_id FROM message_attachments WHERE chat_id = ? "
            "ORDER BY message_id, attachment_id",
            (cid,),
        ),
        "summaries": server.query(
            "SELECT id, summarized_up_to_message_id FROM thread_summaries WHERE chat_id = ?", (cid,)
        ),
        "chat_updated_at": server.query("SELECT updated_at FROM chats WHERE id = ?", (cid,))[0]["updated_at"],
    }


def assert_unchanged(server, mock, chat_id: str, before: dict[str, Any], provider_calls: int) -> None:
    after = snapshot(server, chat_id)
    for key, value in before.items():
        assert after[key] == value, f"{key} changed by a rejected mutation:\nbefore={value}\nafter={after[key]}"
    assert len(stream_requests(mock)) == provider_calls, "a rejected mutation must not call the provider"


def db_exec(server, sql: str, params: list[Any] | tuple[Any, ...] = ()) -> None:
    """One short write transaction on the server database (UUIDs bound as bytes by the caller)."""
    conn = server.sqlite(readonly=False)
    conn.isolation_level = None
    try:
        conn.execute("BEGIN IMMEDIATE")
        try:
            conn.execute(sql, list(params))
        except Exception:
            conn.execute("ROLLBACK")
            raise
        conn.execute("COMMIT")
    finally:
        conn.close()


def same_uuid(a: Any, b: Any) -> bool:
    return a is not None and b is not None and uuid.UUID(str(a)) == uuid.UUID(str(b))


# --------------------------------------------------------------------------
# Seeding helpers
# --------------------------------------------------------------------------


def seed(server, chat_id: str, user: str = "A1", requester: str | None = None, **kwargs: Any) -> SeededTurn:
    """``seed_turn`` in ``user``'s chat; ``requester`` overrides ``requester_user_id``."""
    ident = server.identity(user)
    requester_id = server.identity(requester).user_id if requester else ident.user_id
    return seed_turn(server, chat_id, ident.tenant_id, requester_id, **kwargs)


def seed_image(server, chat_id: str, user: str = "A1", **kwargs: Any) -> tuple[str, str]:
    """A ready image attachment; returns ``(attachment_id, provider_file_id)``."""
    ident = server.identity(user)
    attachment_id = seed_attachment(server, chat_id, ident.tenant_id, ident.user_id, kind="image", **kwargs)
    row = server.query("SELECT provider_file_id FROM attachments WHERE id = ?", (db_uuid(attachment_id),))[0]
    return attachment_id, row["provider_file_id"]


def link(server, chat_id: str, message_id: str, attachment_id: str, user: str = "A1") -> None:
    link_message_attachment(server, chat_id, server.identity(user).tenant_id, message_id, attachment_id)


def seed_summary(
    server, chat_id: str, frontier: dict[str, Any], covered_ids: list[str], text: str, user: str = "A1"
) -> str:
    """A committed ``thread_summaries`` row with frontier ``frontier`` (a ``messages`` row);
    ``is_compressed`` is set on ``covered_ids``."""
    summary_id = new_uuid()
    stamp = db_ts()
    db_exec(
        server,
        "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, summarized_up_to_created_at, "
        "summarized_up_to_message_id, token_estimate, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            uuid_bytes(summary_id),
            uuid_bytes(server.identity(user).tenant_id),
            uuid_bytes(chat_id),
            text,
            frontier["created_at"],
            uuid_bytes(frontier["id"]),
            20,
            stamp,
            stamp,
        ),
    )
    for message_id in covered_ids:
        db_exec(server, "UPDATE messages SET is_compressed = 1 WHERE id = ?", (uuid_bytes(message_id),))
    return summary_id


# --------------------------------------------------------------------------
# Running turns and outbox capture
# --------------------------------------------------------------------------


class RunningTurn:
    """A ``messages:stream`` turn kept running by :data:`SLOW_STREAM`.

    Entering waits for ``stream_started`` and the provider call; leaving the
    block disconnects the client (the turn becomes ``cancelled``) and waits
    until the turn left ``running``.
    """

    def __init__(self, server, mock, chat_id: str, user: str = "A1", content: str = "Keep running") -> None:
        self.server = server
        self.mock = mock
        self.chat_id = chat_id
        self.user = user
        self.request_id = new_uuid()
        self.body = {"content": content, "request_id": self.request_id}
        self._client = httpx.Client(timeout=httpx.Timeout(connect=5.0, read=60.0, write=10.0, pool=10.0))
        self._cm: Any = None
        self.started: dict[str, Any] | None = None

    def __enter__(self) -> "RunningTurn":
        before = len(stream_requests(self.mock))
        self.mock.enqueue(dict(SLOW_STREAM))
        self._cm = self._client.stream(
            "POST",
            self.server.url(stream_path(self.chat_id)),
            headers=self.server.headers(self.user),
            json=self.body,
        )
        resp = self._cm.__enter__()
        if resp.status_code != 200 or not resp.headers.get("content-type", "").startswith("text/event-stream"):
            resp.read()
            text = resp.text
            self._close()
            raise AssertionError(f"expected an SSE stream, got HTTP {resp.status_code}: {text[:1000]}")
        self._events = parse_sse_lines(resp.iter_lines())
        for name, data in self._events:
            if name == "stream_started":
                self.started = data
                break
        assert self.started is not None and self.started["request_id"] == self.request_id, self.started
        wait_until(
            lambda: len(stream_requests(self.mock)) > before,
            timeout=15.0,
            interval=0.05,
            description="the slow provider call",
        )
        return self

    def _close(self) -> None:
        if self._cm is not None:
            try:
                self._cm.__exit__(None, None, None)
            finally:
                self._cm = None
        self._client.close()

    def __exit__(self, *exc: Any) -> None:
        self._close()

        def left_running() -> bool:
            resp = turn_status(self.server, self.chat_id, self.request_id, self.user)
            return resp.status_code == 200 and resp.json()["state"] != "running"

        wait_until(left_running, timeout=45.0, interval=0.2, description="the running turn to finish")


class OutboxRecorder:
    """Polls the outbox payloads of one chat in a background thread.

    Usage and audit bodies are vacuumed shortly after delivery, so they are
    captured while still stored (same technique as the S3 tests).
    """

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="s4-outbox-recorder", daemon=True)

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

    def matching(self, predicate: Callable[[dict[str, Any]], bool]) -> list[dict[str, Any]]:
        return [p for p in self.payloads() if predicate(p)]

    def usage_events(self, request_id: str) -> list[dict[str, Any]]:
        return self.matching(lambda p: "dedupe_key" in p and same_uuid(p.get("request_id"), request_id))

    def audit_events(self, event_type: str, **ids: str) -> list[dict[str, Any]]:
        return self.matching(
            lambda p: p.get("event_type") == event_type
            and all(same_uuid(p.get(key), value) for key, value in ids.items())
        )

    def wait_one(self, probe: Callable[[], list[dict[str, Any]]], description: str) -> dict[str, Any]:
        found = wait_until(probe, timeout=15.0, interval=0.05, description=description)
        return found[0]


# --------------------------------------------------------------------------
# Module servers
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def _s4_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


def module_server(mock, servers: list[Any], name: str, overrides: dict[str, Any]):
    server = create_server(mock, overrides, name=name)
    servers.append(server)
    server.start()
    return server.wait_ready()


@pytest.fixture(scope="module")
def images_off_server(mock, _s4_servers):
    overrides = {
        "gears": {
            "static-mini-chat-model-policy-plugin": {"config": {"kill_switches": {"disable_images": True}}}
        }
    }
    return module_server(mock, _s4_servers, "s4-images-off", overrides)


# --------------------------------------------------------------------------
# Retry
# --------------------------------------------------------------------------


def test_retry_latest_turn_streams_a_new_turn_with_a_server_generated_request_id(mc, mock):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    mock.enqueue({"chunks": ["Bananas ", "are usually ", "yellow."]})

    result = mutate(mc, "retry", chat_id, rid2)

    started = assert_sse_done(result)
    new_rid = started["request_id"]
    assert new_rid != rid2, "retry must create a new request_id"
    assert uuid.UUID(new_rid).version == 4, f"server-generated request_id must be UUID v4: {new_rid}"
    assert started["is_new_turn"] is True, started
    assert result.text_content() == "Bananas are usually yellow."
    done = result.first("done")
    assert done["selected_model"] == done["effective_model"] == DEFAULT_MODEL, done
    assert done["quota_decision"] == "allow", done
    new_turn = turn_row(mc, chat_id, new_rid)
    assert new_turn["state"] == "completed", new_turn
    assert new_turn["assistant_message_id"] == started["message_id"], new_turn
    assert new_turn["requester_user_id"] == mc.identity("A1").user_id, new_turn
    assert new_turn["deleted_at"] is None and new_turn["replaced_by_request_id"] is None, new_turn


def test_retry_soft_deletes_the_old_turn_and_its_messages(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    first_before = turn_row(mc, chat_id, rid1)
    old_messages = message_rows(mc, chat_id, rid2)

    started = assert_sse_done(mutate(mc, "retry", chat_id, rid2))
    new_rid = started["request_id"]

    old = turn_row(mc, chat_id, rid2)
    assert old["deleted_at"] is not None, old
    assert old["replaced_by_request_id"] == new_rid, old
    assert old["state"] == "completed", "terminal states are immutable"
    for message in message_rows(mc, chat_id, rid2):
        assert message["deleted_at"] is not None, f"old message not soft-deleted: {message}"
    assert {m["id"] for m in message_rows(mc, chat_id, rid2)} == {m["id"] for m in old_messages}
    assert turn_row(mc, chat_id, rid1) == first_before, "the earlier turn must not change"

    new_messages = message_rows(mc, chat_id, new_rid)
    assert [m["role"] for m in new_messages] == ["user", "assistant"], new_messages
    assert new_messages[0]["content"] == Q2, "retry re-submits the original user content"
    assert new_messages[1]["id"] == started["message_id"]
    assert all(m["deleted_at"] is None for m in new_messages), new_messages
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [new_rid, rid1]


def test_retry_resends_the_original_content_after_the_history(mc, mock):
    chat_id, _, rid2 = two_turn_chat(mc, mock)

    assert_sse_done(mutate(mc, "retry", chat_id, rid2))

    requests = stream_requests(mock)
    assert len(requests) == 3, [r["path"] for r in requests]
    assert conversation(requests[-1]["json"]) == [("user", Q1), ("assistant", A1), ("user", Q2)], (
        "retry must send the history before the turn and the original content, not the old answer"
    )
    assert A2 not in json.dumps(requests[-1]["json"]), "the replaced answer must not be re-sent"


def test_retry_turn_status_of_old_is_not_found_and_new_is_done(mc, mock):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    started = assert_sse_done(mutate(mc, "retry", chat_id, rid2))

    assert_problem(turn_status(mc, chat_id, rid2), 404, category="not_found", resource_type=RT_TURN)
    resp = turn_status(mc, chat_id, started["request_id"])
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["request_id"] == started["request_id"], body
    assert body["state"] == "done", body
    assert body["assistant_message_id"] == started["message_id"], body
    assert "error_code" not in body, body
    assert is_rfc3339(body["updated_at"]), body


def test_retry_message_list_has_one_pair_for_the_turn(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    mock.enqueue({"text": "Bananas are yellow when ripe."})
    started = assert_sse_done(mutate(mc, "retry", chat_id, rid2))
    new_rid = started["request_id"]

    items = list_messages(mc, chat_id)

    assert [(m["request_id"], m["role"], m["content"]) for m in items] == [
        (rid1, "user", Q1),
        (rid1, "assistant", A1),
        (new_rid, "user", Q2),
        (new_rid, "assistant", "Bananas are yellow when ripe."),
    ], items
    assert rid2 not in {m["request_id"] for m in items}
    assert items[3]["id"] == started["message_id"]


def test_retry_old_request_id_is_request_id_conflict_on_stream(mc, mock):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    assert_sse_done(mutate(mc, "retry", chat_id, rid2))
    calls = len(stream_requests(mock))
    before = snapshot(mc, chat_id)

    result = mc.stream("POST", stream_path(chat_id), "A1", json={"content": Q2, "request_id": rid2})

    assert_rejected(result, 409, "aborted", reason="request_id_conflict")
    assert_unchanged(mc, mock, chat_id, before, calls)


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_retry_and_edit_bump_chat_updated_at(mc, mock, op):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    before = chat_updated_at(mc, chat_id)
    time.sleep(0.01)

    assert_sse_done(mutate(mc, op, chat_id, rid2))

    assert parse_ts_ns(chat_updated_at(mc, chat_id)) > parse_ts_ns(before)


# --------------------------------------------------------------------------
# Edit
# --------------------------------------------------------------------------


def test_edit_latest_turn_streams_a_new_turn_with_the_new_content(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    mock.enqueue({"text": "Here is a new answer."})

    result = mutate(mc, "edit", chat_id, rid2, body={"content": EDIT_TEXT})

    started = assert_sse_done(result)
    new_rid = started["request_id"]
    assert new_rid != rid2 and uuid.UUID(new_rid).version == 4, started
    assert started["is_new_turn"] is True, started
    assert result.text_content() == "Here is a new answer."

    old = turn_row(mc, chat_id, rid2)
    assert old["deleted_at"] is not None and old["replaced_by_request_id"] == new_rid, old
    assert all(m["deleted_at"] is not None for m in message_rows(mc, chat_id, rid2))
    new_messages = message_rows(mc, chat_id, new_rid)
    assert [(m["role"], m["content"]) for m in new_messages] == [
        ("user", EDIT_TEXT),
        ("assistant", "Here is a new answer."),
    ], new_messages

    body = stream_requests(mock)[-1]["json"]
    assert conversation(body) == [("user", Q1), ("assistant", A1), ("user", EDIT_TEXT)], conversation(body)
    assert Q2 not in json.dumps(body) and A2 not in json.dumps(body)

    assert_problem(turn_status(mc, chat_id, rid2), 404, category="not_found", resource_type=RT_TURN)
    status = turn_status(mc, chat_id, new_rid)
    assert status.status_code == 200 and status.json()["state"] == "done", status.text
    assert [(m["request_id"], m["role"], m["content"]) for m in list_messages(mc, chat_id)] == [
        (rid1, "user", Q1),
        (rid1, "assistant", A1),
        (new_rid, "user", EDIT_TEXT),
        (new_rid, "assistant", "Here is a new answer."),
    ]


@pytest.mark.parametrize("content", ["", "   ", " \n\t  \r\n "], ids=["empty", "spaces", "whitespace"])
def test_edit_empty_or_whitespace_content_is_empty_content(mc, mock, content):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    calls = len(stream_requests(mock))
    before = snapshot(mc, chat_id)

    result = mutate(mc, "edit", chat_id, rid2, body={"content": content})

    assert_rejected(result, 400, "invalid_argument", field_name="content", field_reason="EMPTY_CONTENT")
    assert_unchanged(mc, mock, chat_id, before, calls)
    assert live_turns(mc, chat_id)[0]["request_id"] == rid2


@pytest.mark.parametrize("body", [{}, {"content": 5}, {"text": EDIT_TEXT}], ids=["missing", "not-string", "wrong-field"])
def test_edit_body_without_string_content_is_422(mc, mock, body):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    calls = len(stream_requests(mock))
    before = snapshot(mc, chat_id)

    result = mutate(mc, "edit", chat_id, rid2, body=body)

    assert_rejected(result, 422, field_name="body", field_reason="invalid_json_body")
    assert_unchanged(mc, mock, chat_id, before, calls)


# --------------------------------------------------------------------------
# Delete
# --------------------------------------------------------------------------


def test_delete_latest_turn_soft_deletes_it_and_its_messages(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    first_before = turn_row(mc, chat_id, rid1)
    calls = len(stream_requests(mock))

    result = mutate(mc, "delete", chat_id, rid2)

    assert_deleted_204(result)
    assert len(stream_requests(mock)) == calls, "delete must not call the provider"
    old = turn_row(mc, chat_id, rid2)
    assert old["deleted_at"] is not None, old
    assert old["replaced_by_request_id"] is None, "delete sets no replacement"
    assert old["state"] == "completed", old
    messages = message_rows(mc, chat_id, rid2)
    assert len(messages) == 2 and all(m["deleted_at"] is not None for m in messages), messages
    assert turn_row(mc, chat_id, rid1) == first_before
    assert len(turn_rows(mc, chat_id)) == 2, "delete creates no turn"
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [rid1]
    assert [(m["request_id"], m["role"]) for m in list_messages(mc, chat_id)] == [
        (rid1, "user"),
        (rid1, "assistant"),
    ]
    assert_problem(turn_status(mc, chat_id, rid2), 404, category="not_found", resource_type=RT_TURN)


def test_delete_makes_the_previous_turn_latest_and_retryable(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    assert_deleted_204(mutate(mc, "delete", chat_id, rid2))
    mock.enqueue({"text": "Apples can also be green."})

    result = mutate(mc, "retry", chat_id, rid1)

    started = assert_sse_done(result)
    assert conversation(stream_requests(mock)[-1]["json"]) == [("user", Q1)]
    old = turn_row(mc, chat_id, rid1)
    assert old["deleted_at"] is not None and old["replaced_by_request_id"] == started["request_id"], old
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [started["request_id"]]
    assert [(m["role"], m["content"]) for m in list_messages(mc, chat_id)] == [
        ("user", Q1),
        ("assistant", "Apples can also be green."),
    ]


def test_delete_again_is_not_latest_turn(mc, mock):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    assert_deleted_204(mutate(mc, "delete", chat_id, rid2))
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, "delete", chat_id, rid2)

    assert_rejected(result, 409, "aborted", reason="NOT_LATEST_TURN")
    assert_unchanged(mc, mock, chat_id, before, calls)
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [rid1]


# --------------------------------------------------------------------------
# Latest-turn rule
# --------------------------------------------------------------------------


@pytest.mark.parametrize("op", OPS)
def test_mutation_of_an_older_turn_is_not_latest_turn(mc, mock, op):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, rid1)

    assert_rejected(result, 409, "aborted", reason="NOT_LATEST_TURN")
    assert_unchanged(mc, mock, chat_id, before, calls)
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [rid2, rid1]


@pytest.mark.parametrize("op", OPS)
def test_mutation_of_a_soft_deleted_turn_is_not_latest_turn(mc, mock, op):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    new_rid = assert_sse_done(mutate(mc, "retry", chat_id, rid2))["request_id"]
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, rid2)

    assert_rejected(result, 409, "aborted", reason="NOT_LATEST_TURN")
    assert_unchanged(mc, mock, chat_id, before, calls)
    assert live_turns(mc, chat_id)[0]["request_id"] == new_rid


def test_newer_running_turn_makes_the_target_not_latest(mc, mock):
    chat_id = new_chat(mc)["id"]
    _, rid1 = send_done(mc, chat_id, Q1)
    with RunningTurn(mc, mock, chat_id) as running:
        before = snapshot(mc, chat_id)
        calls = len(stream_requests(mock))
        for op in OPS:
            result = mutate(mc, op, chat_id, rid1)
            assert_rejected(result, 409, "aborted", reason="NOT_LATEST_TURN")
        assert_unchanged(mc, mock, chat_id, before, calls)
        assert turn_row(mc, chat_id, running.request_id)["state"] == "running"


# --------------------------------------------------------------------------
# Terminal-state rule
# --------------------------------------------------------------------------


def test_mutation_of_a_running_turn_is_failed_precondition(mc, mock):
    chat_id = new_chat(mc)["id"]
    send_done(mc, chat_id, Q1)
    with RunningTurn(mc, mock, chat_id) as running:
        before = snapshot(mc, chat_id)
        calls = len(stream_requests(mock))
        for op in OPS:
            result = mutate(mc, op, chat_id, running.request_id)
            assert_rejected(
                result, 400, "failed_precondition", violation_subject="turn_state", violation_type="STATE"
            )
        assert_unchanged(mc, mock, chat_id, before, calls)
        row = turn_row(mc, chat_id, running.request_id)
        assert row["state"] == "running" and row["deleted_at"] is None, row


@pytest.mark.parametrize("op", OPS)
def test_running_state_is_checked_before_the_latest_turn(mc, mock, op):
    """A running target that is not the latest turn is rejected with ``STATE``, not ``NOT_LATEST_TURN``."""
    chat_id = new_chat(mc)["id"]
    running = seed(mc, chat_id, state="running", user_text="still running", offset_ms=-1_000)
    seed(mc, chat_id, user_text="newer completed turn")
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, running.request_id)

    assert_rejected(result, 400, "failed_precondition", violation_subject="turn_state", violation_type="STATE")
    assert_unchanged(mc, mock, chat_id, before, calls)


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_failed_turn_can_be_retried_or_edited(mc, mock, op):
    chat_id = new_chat(mc)["id"]
    mock.enqueue({"kind": "failed"})
    failed_rid = new_uuid()
    failed = mc.stream("POST", stream_path(chat_id), "A1", json={"content": Q1, "request_id": failed_rid})
    assert failed.terminal is not None and failed.terminal[0] == "error", failed.events
    assert turn_row(mc, chat_id, failed_rid)["state"] == "failed"
    mock.enqueue({"text": "Second time lucky."})

    started = assert_sse_done(mutate(mc, op, chat_id, failed_rid))

    old = turn_row(mc, chat_id, failed_rid)
    assert old["state"] == "failed" and old["deleted_at"] is not None, old
    assert old["replaced_by_request_id"] == started["request_id"], old
    expected = Q1 if op == "retry" else EDIT_TEXT
    assert conversation(stream_requests(mock)[-1]["json"]) == [("user", expected)]
    assert turn_row(mc, chat_id, started["request_id"])["state"] == "completed"


def test_cancelled_turn_with_partial_answer_can_be_retried(mc, mock):
    chat_id = new_chat(mc)["id"]
    cancelled = seed(mc, chat_id, state="cancelled", user_text="Tell me a story", assistant_text="Once upon")
    mock.enqueue({"text": "Once upon a time, the end."})

    started = assert_sse_done(mutate(mc, "retry", chat_id, cancelled.request_id))

    body = stream_requests(mock)[-1]["json"]
    assert conversation(body) == [("user", "Tell me a story")], "the partial answer belongs to the old turn"
    old = turn_row(mc, chat_id, cancelled.request_id)
    assert old["state"] == "cancelled" and old["deleted_at"] is not None, old
    assert old["replaced_by_request_id"] == started["request_id"], old
    assert [(m["role"], m["content"]) for m in list_messages(mc, chat_id)] == [
        ("user", "Tell me a story"),
        ("assistant", "Once upon a time, the end."),
    ]


# --------------------------------------------------------------------------
# Resolution errors
# --------------------------------------------------------------------------


@pytest.mark.parametrize("op", OPS)
def test_unknown_request_id_is_turn_not_found(mc, mock, op):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, new_uuid())

    assert_rejected(result, 404, "not_found", resource_type=RT_TURN)
    assert_unchanged(mc, mock, chat_id, before, calls)


@pytest.mark.parametrize("op", OPS)
def test_turn_of_another_chat_is_turn_not_found(mc, mock, op):
    chat_id = new_chat(mc)["id"]
    send_done(mc, chat_id, Q1)
    other_chat = new_chat(mc)["id"]
    _, other_rid = send_done(mc, other_chat, Q2)
    before = snapshot(mc, other_chat)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, other_rid)

    assert_rejected(result, 404, "not_found", resource_type=RT_TURN)
    assert_unchanged(mc, mock, other_chat, before, calls)


@pytest.mark.parametrize("other", ["A2", "B1"], ids=["same-tenant-other-user", "other-tenant"])
@pytest.mark.parametrize("op", OPS)
def test_foreign_chat_is_chat_not_found(mc, mock, op, other):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, rid2, user=other)

    assert_rejected(result, 404, "not_found", resource_type=RT_CHAT)
    assert_unchanged(mc, mock, chat_id, before, calls)


@pytest.mark.parametrize("op", OPS)
def test_deleted_chat_is_chat_not_found(mc, mock, op):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    resp = mc.delete(f"{CHATS}/{chat_id}", "A1")
    assert resp.status_code == 204, resp.text

    result = mutate(mc, op, chat_id, rid2)

    assert_rejected(result, 404, "not_found", resource_type=RT_CHAT)


@pytest.mark.parametrize("bad", ["chat", "request"])
@pytest.mark.parametrize("op", OPS)
def test_non_uuid_path_is_invalid_path_params(mc, mock, op, bad):
    chat_id = new_chat(mc)["id"]
    target_chat = "not-a-uuid" if bad == "chat" else chat_id
    target_rid = "not-a-uuid" if bad == "request" else new_uuid()

    result = mutate(mc, op, target_chat, target_rid)

    assert_rejected(result, 400, "invalid_argument", field_reason="invalid_path_params")


@pytest.mark.parametrize("op", OPS)
def test_mutation_without_token_is_unauthenticated(mc, mock, op):
    chat_id, _, rid2 = two_turn_chat(mc, mock)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, rid2, user=None)

    assert_rejected(result, 401, "unauthenticated", reason="MISSING_BEARER")
    assert_unchanged(mc, mock, chat_id, before, calls)


# --------------------------------------------------------------------------
# Attachments
# --------------------------------------------------------------------------


def _image_turn(server, chat_id: str) -> tuple[SeededTurn, tuple[str, str], tuple[str, str]]:
    """A seeded turn whose user message links one live and one soft-deleted image."""
    turn = seed(server, chat_id, user_text="Describe this picture")
    live = seed_image(server, chat_id, filename="live.png")
    gone = seed_image(server, chat_id, filename="gone.png", deleted=True)
    link(server, chat_id, turn.user_message_id, live[0])
    link(server, chat_id, turn.user_message_id, gone[0])
    return turn, live, gone


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_retry_and_edit_copy_live_attachment_links_and_resend_images(mc, mock, op):
    chat_id = new_chat(mc)["id"]
    turn, (live_id, live_file), (gone_id, gone_file) = _image_turn(mc, chat_id)
    mock.enqueue({"text": "A cat on a sofa."})

    started = assert_sse_done(mutate(mc, op, chat_id, turn.request_id))

    new_rid = started["request_id"]
    new_user = [m for m in message_rows(mc, chat_id, new_rid) if m["role"] == "user"]
    assert len(new_user) == 1, new_user
    assert link_rows(mc, new_user[0]["id"]) == {live_id}, "only non-deleted attachments are copied"
    assert link_rows(mc, turn.user_message_id) == {live_id, gone_id}, "old links stay for audit"

    listed = {m["id"]: m for m in list_messages(mc, chat_id)}
    assert set(listed) == {new_user[0]["id"], started["message_id"]}, listed
    assert [a["attachment_id"] for a in listed[new_user[0]["id"]]["attachments"]] == [live_id]
    assert listed[started["message_id"]]["attachments"] == []

    body = stream_requests(mock)[-1]["json"]
    assert last_user_images(body) == [live_file], body.get("input")
    assert gone_file not in json.dumps(body), "a deleted attachment must not be sent"
    expected = "Describe this picture" if op == "retry" else EDIT_TEXT
    assert conversation(body)[-1] == ("user", expected)


@pytest.mark.slow
@pytest.mark.parametrize("op", STREAMING_OPS)
def test_image_turn_with_disable_images_is_feature_disabled_and_untouched(images_off_server, mock, op):
    server = images_off_server
    chat_id = new_chat(server)["id"]
    turn, _, _ = _image_turn(server, chat_id)
    before = snapshot(server, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(server, op, chat_id, turn.request_id)

    assert_rejected(
        result, 400, "failed_precondition", violation_subject="images", violation_type="FEATURE_DISABLED"
    )
    assert_unchanged(server, mock, chat_id, before, calls)
    latest = live_turns(server, chat_id)
    assert [t["request_id"] for t in latest] == [turn.request_id], latest


@pytest.mark.slow
def test_delete_of_an_image_turn_runs_no_image_guard(images_off_server, mock):
    server = images_off_server
    chat_id = new_chat(server)["id"]
    turn, _, _ = _image_turn(server, chat_id)

    assert_deleted_204(mutate(server, "delete", chat_id, turn.request_id))

    assert turn_row(server, chat_id, turn.request_id)["deleted_at"] is not None
    assert live_turns(server, chat_id) == []


# --------------------------------------------------------------------------
# Audit events
# --------------------------------------------------------------------------


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_retry_and_edit_enqueue_one_mutation_audit_event(mc, mock, op):
    ident = mc.identity("A1")
    chat_id = new_chat(mc)["id"]
    _, old_rid = send_done(mc, chat_id, Q1)
    event_type = f"turn_{op}"
    started_ns = time.time_ns()

    with OutboxRecorder(mc, chat_id) as recorder:
        started = assert_sse_done(mutate(mc, op, chat_id, old_rid))
        new_rid = started["request_id"]
        event = recorder.wait_one(
            lambda: recorder.audit_events(event_type, original_request_id=old_rid),
            f"{event_type} audit payload",
        )
        recorder.wait_one(
            lambda: recorder.audit_events("turn_completed", request_id=new_rid),
            "turn_completed audit payload of the new turn",
        )
        time.sleep(1.0)
        assert len(recorder.audit_events(event_type, original_request_id=old_rid)) == 1
        assert len(recorder.audit_events("turn_completed", request_id=new_rid)) == 1
        assert recorder.audit_events("turn_delete") == []

    required = {"event_type", "actor_user_id", "chat_id", "original_request_id", "new_request_id", "timestamp"}
    assert required <= set(event), event
    assert event["event_type"] == event_type
    assert same_uuid(event["actor_user_id"], ident.user_id), event
    assert same_uuid(event["chat_id"], chat_id), event
    assert same_uuid(event["original_request_id"], old_rid), event
    assert same_uuid(event["new_request_id"], new_rid), event
    if "tenant_id" in event:
        assert same_uuid(event["tenant_id"], ident.tenant_id), event
    assert is_rfc3339(event["timestamp"]), event
    assert started_ns - 5_000_000_000 <= parse_ts_ns(event["timestamp"]) <= time.time_ns() + 5_000_000_000


def test_delete_enqueues_one_turn_delete_audit_event(mc, mock):
    ident = mc.identity("A1")
    chat_id = new_chat(mc)["id"]
    _, rid = send_done(mc, chat_id, Q1)

    with OutboxRecorder(mc, chat_id) as recorder:
        assert_deleted_204(mutate(mc, "delete", chat_id, rid))
        event = recorder.wait_one(
            lambda: recorder.audit_events("turn_delete", request_id=rid), "turn_delete audit payload"
        )
        time.sleep(1.0)
        assert len(recorder.audit_events("turn_delete", request_id=rid)) == 1
        assert recorder.audit_events("turn_retry") == recorder.audit_events("turn_edit") == []

    required = {"event_type", "actor_user_id", "chat_id", "request_id", "timestamp"}
    assert required <= set(event), event
    assert same_uuid(event["actor_user_id"], ident.user_id), event
    assert same_uuid(event["chat_id"], chat_id), event
    assert same_uuid(event["request_id"], rid), event
    assert event.get("new_request_id") is None, f"delete creates no replacement: {event}"
    assert is_rfc3339(event["timestamp"]), event


# --------------------------------------------------------------------------
# Thread summary interaction
# --------------------------------------------------------------------------

SUMMARY_TEXT = "S4 seeded summary: the user asked about fruit colors."


def _messages_by_turn(server, chat_id: str, rid1: str, rid2: str) -> tuple[list[dict], list[dict]]:
    first = message_rows(server, chat_id, rid1)
    second = message_rows(server, chat_id, rid2)
    assert [m["role"] for m in first] == ["user", "assistant"], first
    assert [m["role"] for m in second] == ["user", "assistant"], second
    return first, second


@pytest.mark.parametrize("op", OPS)
def test_summary_covering_the_mutated_turn_is_dropped(mc, mock, op):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    first, second = _messages_by_turn(mc, chat_id, rid1, rid2)
    # Frontier exactly at the mutated turn's user message ("at or after" -> drop).
    seed_summary(mc, chat_id, second[0], [first[0]["id"], first[1]["id"], second[0]["id"]], SUMMARY_TEXT)
    assert len(summary_rows(mc, chat_id)) == 1

    result = mutate(mc, op, chat_id, rid2)

    if op == "delete":
        assert_deleted_204(result)
    else:
        assert_sse_done(result)
        body = stream_requests(mock)[-1]["json"]
        expected = Q2 if op == "retry" else EDIT_TEXT
        assert conversation(body) == [("user", Q1), ("assistant", A1), ("user", expected)], conversation(body)
        assert SUMMARY_TEXT not in json.dumps(body), "the dropped summary must not be sent"
    assert summary_rows(mc, chat_id) == [], "a summary covering the mutated turn must be deleted"
    flags = mc.query("SELECT id, is_compressed FROM messages WHERE chat_id = ?", (db_uuid(chat_id),))
    assert flags and all(int(r["is_compressed"]) == 0 for r in flags), flags


@pytest.mark.parametrize("op", OPS)
def test_summary_before_the_mutated_turn_is_kept(mc, mock, op):
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    first, second = _messages_by_turn(mc, chat_id, rid1, rid2)
    # Frontier at the previous turn's answer: strictly before the mutated turn.
    seed_summary(mc, chat_id, first[1], [first[0]["id"], first[1]["id"]], SUMMARY_TEXT)
    summary_before = summary_rows(mc, chat_id)

    result = mutate(mc, op, chat_id, rid2)

    if op == "delete":
        assert_deleted_204(result)
    else:
        assert_sse_done(result)
        # The next context is the existing summary followed by the replacement turn.
        conv = conversation(stream_requests(mock)[-1]["json"])
        expected = Q2 if op == "retry" else EDIT_TEXT
        assert len(conv) == 2, conv
        assert conv[0][0] == "user" and SUMMARY_TEXT in conv[0][1], conv
        assert conv[1] == ("user", expected), conv
    assert summary_rows(mc, chat_id) == summary_before, "a summary before the mutated turn is kept"
    flags = {
        r["id"]: int(r["is_compressed"])
        for r in mc.query("SELECT id, is_compressed FROM messages WHERE chat_id = ?", (db_uuid(chat_id),))
    }
    assert flags[first[0]["id"]] == flags[first[1]["id"]] == 1, flags
    assert flags[second[0]["id"]] == flags[second[1]["id"]] == 0, flags
