"""Retry, edit and delete of the last turn (DESIGN section 3.9, section 3.6 retry/edit variant)."""

import json
import threading
import uuid
from contextlib import closing

import pytest

from ._seed import insert_turn
from .helpers import (PREFIX, api, assert_problem, captured_outbox, create_chat, db, exhausted_quota, stream,
                      uuid_bytes, wait_until)

pytestmark = pytest.mark.usefixtures("server")

#: Capture table of the ``outbox_capture`` fixture (conftest.py).
CAPTURE = "e2e_mutation_outbox_capture"


def _mutation_audits(chat_id: str) -> list[dict]:
    out = []
    for queue, _, body in captured_outbox(CAPTURE):
        if body.get("kind") == "mutation" and body.get("chat_id") == chat_id:
            assert queue == "mini-chat.audit"
            out.append(body)
    return out


def _turn(chat_id: str, rid: str):
    with closing(db()) as conn:
        return conn.execute(
            "SELECT state, error_code, deleted_at, replaced_by_request_id, reserve_tokens, effective_model,"
            " web_search_enabled FROM chat_turns WHERE chat_id = ? AND request_id = ?",
            (uuid_bytes(chat_id), uuid_bytes(rid))).fetchone()


def _live_messages(chat_id: str):
    with closing(db()) as conn:
        return conn.execute(
            "SELECT id, role, content, request_id FROM messages WHERE chat_id = ? AND deleted_at IS NULL"
            " ORDER BY created_at, id", (uuid_bytes(chat_id),)).fetchall()


def _turn_url(chat_id: str, rid: str) -> str:
    return f"{PREFIX}/chats/{chat_id}/turns/{rid}"


def _send(s, chat_id: str, content: str = "hi", **extra) -> str:
    rid = str(uuid.uuid4())
    res = stream(s, chat_id, {"content": content, "request_id": rid, **extra})
    assert res.terminal and res.terminal[0] == "done", res.raw
    return rid


def _last_provider_input(mock) -> str:
    return json.dumps(mock.requests(path="/responses")[-1]["json"]["input"])


# ── Retry ────────────────────────────────────────────────────────────────────


def test_retry_streams_a_new_turn_and_replaces_the_old(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    old = _send(s, chat["id"], "what is two plus two")
    calls = len(reset_mock.requests(path="/responses"))

    res = stream(s, chat["id"], {}, path=f"{_turn_url(chat['id'], old)}/retry")

    assert res.status == 200, res.raw
    assert res.names() == ["stream_started", "delta", "delta", "done"]
    started = res.of("stream_started")[0]
    new = started["request_id"]
    assert new != old and uuid.UUID(new).version == 4
    assert started["is_new_turn"] is True
    assert len(reset_mock.requests(path="/responses")) == calls + 1
    assert "what is two plus two" in _last_provider_input(reset_mock)

    old_row, new_row = _turn(chat["id"], old), _turn(chat["id"], new)
    assert old_row["deleted_at"] is not None
    assert old_row["replaced_by_request_id"] == uuid_bytes(new)
    assert new_row["state"] == "completed" and new_row["deleted_at"] is None
    assert new_row["reserve_tokens"] is not None and new_row["effective_model"] == "gpt-premium"
    msgs = _live_messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "what is two plus two"),
                                                          ("assistant", "Hello world")]
    assert all(m["request_id"] == uuid_bytes(new) for m in msgs)

    assert s.get(_turn_url(chat["id"], new)).json()["state"] == "done"
    assert_problem(s.get(_turn_url(chat["id"], old)), 404, "not_found")
    # The old request id is no longer replayable.
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "x", "request_id": old})
    assert_problem(r, 409, "aborted", reason="request_id_conflict")

    audits = wait_until(lambda: _mutation_audits(chat["id"]), message="turn_retry audit")
    assert len(audits) == 1
    a = audits[0]
    assert a["event_type"] == "turn_retry"
    assert a["original_request_id"] == old and a["new_request_id"] == new
    assert a["request_id"] is None
    assert a["actor_user_id"] and a["timestamp"]


def test_retry_of_deleted_turn_is_409_not_latest(reset_mock):
    s = api()
    chat = create_chat(s)
    old = _send(s, chat["id"])
    res = stream(s, chat["id"], {}, path=f"{_turn_url(chat['id'], old)}/retry")
    assert res.terminal[0] == "done"
    for r in (s.post(f"{_turn_url(chat['id'], old)}/retry"),
              s.patch(_turn_url(chat["id"], old), json={"content": "x"}),
              s.delete(_turn_url(chat["id"], old))):
        assert_problem(r, 409, "aborted", reason="NOT_LATEST_TURN")


def test_mutation_of_non_latest_turn_is_409(reset_mock):
    s = api()
    chat = create_chat(s)
    first = _send(s, chat["id"], "one")
    _send(s, chat["id"], "two")
    calls = len(reset_mock.requests(path="/responses"))
    for r in (s.post(f"{_turn_url(chat['id'], first)}/retry"),
              s.patch(_turn_url(chat["id"], first), json={"content": "x"}),
              s.delete(_turn_url(chat["id"], first))):
        assert_problem(r, 409, "aborted", reason="NOT_LATEST_TURN")
    assert len(reset_mock.requests(path="/responses")) == calls
    assert _turn(chat["id"], first)["deleted_at"] is None


def test_running_turn_is_400_turn_state(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    insert_turn(chat["id"], rid, "running")
    for r in (s.post(f"{_turn_url(chat['id'], rid)}/retry"),
              s.patch(_turn_url(chat["id"], rid), json={"content": "x"}),
              s.delete(_turn_url(chat["id"], rid))):
        assert_problem(r, 400, "failed_precondition", violation_type="STATE")
    assert reset_mock.requests(path="/responses") == []


def test_other_requester_is_403(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = _send(s, chat["id"])
    with closing(db()) as conn:
        conn.execute("UPDATE chat_turns SET requester_user_id = ? WHERE chat_id = ? AND request_id = ?",
                     (uuid.uuid4().bytes, uuid_bytes(chat["id"]), uuid_bytes(rid)))
        conn.commit()
    for r in (s.post(f"{_turn_url(chat['id'], rid)}/retry"),
              s.patch(_turn_url(chat["id"], rid), json={"content": "x"}),
              s.delete(_turn_url(chat["id"], rid))):
        assert_problem(r, 403, "permission_denied")
    assert _turn(chat["id"], rid)["deleted_at"] is None


def test_unknown_turn_and_foreign_chat_are_404(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = _send(s, chat["id"])
    assert_problem(s.post(f"{_turn_url(chat['id'], str(uuid.uuid4()))}/retry"), 404, "not_found")
    other = api("e2e-token-tenant-a-reviewer")
    assert_problem(other.post(f"{_turn_url(chat['id'], rid)}/retry"), 404, "not_found")
    assert_problem(other.delete(_turn_url(chat["id"], rid)), 404, "not_found")


def test_retry_quota_rejection_keeps_the_previous_turn(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    rid = _send(s, chat["id"])
    # Exhaust every bucket of the caller (the downgrade cascade has nowhere to go).
    with exhausted_quota(chat["id"]):
        r = s.post(f"{_turn_url(chat['id'], rid)}/retry")
        assert_problem(r, 429, "resource_exhausted")
        assert _turn(chat["id"], rid)["deleted_at"] is None
        assert len(_live_messages(chat["id"])) == 2
        assert _mutation_audits(chat["id"]) == []


# ── Edit ─────────────────────────────────────────────────────────────────────


def test_edit_streams_with_new_content(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    old = _send(s, chat["id"], "old question")

    res = stream(s, chat["id"], {"content": "new question"}, path=_turn_url(chat["id"], old), method="PATCH")

    assert res.status == 200, res.raw
    assert res.terminal[0] == "done"
    new = res.of("stream_started")[0]["request_id"]
    assert new != old
    sent = _last_provider_input(reset_mock)
    assert "new question" in sent and "old question" not in sent
    msgs = _live_messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "new question"), ("assistant", "Hello world")]
    assert _turn(chat["id"], old)["replaced_by_request_id"] == uuid_bytes(new)
    audits = wait_until(lambda: _mutation_audits(chat["id"]), message="turn_edit audit")
    assert [(a["event_type"], a["original_request_id"], a["new_request_id"]) for a in audits] == \
        [("turn_edit", old, new)]


def test_edit_validation(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = _send(s, chat["id"])
    assert_problem(s.patch(_turn_url(chat["id"], rid), json={"content": "   "}), 400, "invalid_argument",
                   reason="EMPTY_CONTENT")
    r = s.patch(_turn_url(chat["id"], rid), json={})
    assert r.status_code in (400, 422), r.text
    assert _turn(chat["id"], rid)["deleted_at"] is None


# ── Delete ───────────────────────────────────────────────────────────────────


def test_delete_soft_deletes_and_status_404(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    first = _send(s, chat["id"], "one")
    second = _send(s, chat["id"], "two")

    r = s.delete(_turn_url(chat["id"], second))
    assert r.status_code == 204, r.text
    assert r.content == b""

    assert_problem(s.get(_turn_url(chat["id"], second)), 404, "not_found")
    assert _turn(chat["id"], second)["deleted_at"] is not None
    assert _turn(chat["id"], second)["replaced_by_request_id"] is None
    listed = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    assert [m["content"] for m in listed if m["role"] == "user"] == ["one"]
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["message_count"] == 2
    audits = wait_until(lambda: _mutation_audits(chat["id"]), message="turn_delete audit")
    assert [(a["event_type"], a["request_id"], a["original_request_id"], a["new_request_id"]) for a in audits] == \
        [("turn_delete", second, None, None)]

    # The previous turn is now the latest and can be deleted; the next send works.
    assert s.delete(_turn_url(chat["id"], first)).status_code == 204
    _send(s, chat["id"], "three")


def test_openapi_declares_the_mutation_operations():
    doc = api(None).get("/openapi.json").json()
    item = doc["paths"]["/mini-chat/v1/chats/{id}/turns/{request_id}"]
    retry = doc["paths"]["/mini-chat/v1/chats/{id}/turns/{request_id}/retry"]["post"]
    edit, delete = item["patch"], item["delete"]
    assert (retry["operationId"], edit["operationId"], delete["operationId"]) == \
        ("mini_chat.retry_turn", "mini_chat.edit_turn", "mini_chat.delete_turn")
    for op in (retry, edit, delete):
        assert op["tags"] == ["Mini Chat Turns"]
    assert "requestBody" not in retry
    assert edit["requestBody"]["content"]["application/json"]["schema"]["$ref"].endswith("/EditTurnRequest")
    for op in (retry, edit):
        assert op["responses"]["200"]["content"]["text/event-stream"]["schema"]["$ref"].endswith("/MiniChatSseEvent")
    assert set(retry["responses"]) == {"200", "400", "401", "403", "404", "409", "429", "500", "503"}
    assert set(edit["responses"]) == {"200", "400", "401", "403", "404", "409", "422", "429", "500", "503"}
    assert set(delete["responses"]) == {"204", "400", "401", "403", "404", "409", "500", "503"}
    schema = doc["components"]["schemas"]["EditTurnRequest"]
    assert schema["required"] == ["content"] and schema["properties"]["content"]["type"] == "string"


# ── Concurrency ──────────────────────────────────────────────────────────────


def test_concurrent_retries_have_exactly_one_winner(reset_mock):
    s = api()
    chat = create_chat(s)
    old = _send(s, chat["id"])
    calls = len(reset_mock.requests(path="/responses"))
    # Keep the winner's provider call open while the loser arrives.
    reset_mock.enqueue("responses", {"start_delay_ms": 1500})
    barrier = threading.Barrier(2)
    results: list = []

    def retry():
        barrier.wait()
        r = api().post(f"{_turn_url(chat['id'], old)}/retry", headers={"Accept": "text/event-stream"},
                       stream=True, timeout=60)
        try:
            _ = r.content  # read the whole body (cached on the response) before closing
        finally:
            r.close()
        results.append(r)

    threads = [threading.Thread(target=retry) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=60)
    statuses = sorted(r.status_code for r in results)
    assert statuses == [200, 409], [(r.status_code, r.text) for r in results]
    winner = next(r for r in results if r.status_code == 200)
    assert "text/event-stream" in winner.headers["Content-Type"]
    loser = next(r for r in results if r.status_code == 409)
    p = assert_problem(loser, 409, "aborted")
    assert p["context"]["reason"] in {"GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"}, p
    assert len(reset_mock.requests(path="/responses")) == calls + 1
    with closing(db()) as conn:
        live = conn.execute("SELECT state FROM chat_turns WHERE chat_id = ? AND deleted_at IS NULL",
                            (uuid_bytes(chat["id"]),)).fetchall()
    assert [r["state"] for r in live] == ["completed"]
    assert len(_live_messages(chat["id"])) == 2
