"""Idempotent replay and request_id conflicts (DESIGN section 3.3 "Idempotency", section 4)."""

import uuid
from contextlib import closing

import pytest

from ._seed import insert_turn
from .helpers import PREFIX, api, assert_problem, create_chat, db, stream, uuid_bytes

pytestmark = pytest.mark.usefixtures("server")


def _quota_rows(chat_id: str):
    with closing(db()) as conn:
        owner = conn.execute("SELECT tenant_id, user_id FROM chats WHERE id = ?", (uuid_bytes(chat_id),)).fetchone()
        rows = conn.execute(
            "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, calls"
            " FROM quota_usage WHERE tenant_id = ? AND user_id = ? ORDER BY period_type, bucket",
            (owner["tenant_id"], owner["user_id"])).fetchall()
    return [tuple(r) for r in rows]


def test_completed_request_id_is_replayed_without_side_effects(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    first = stream(s, chat["id"], {"content": "hi", "request_id": rid})
    assert first.terminal[0] == "done"
    calls = len(reset_mock.requests(path="/responses"))
    quota = _quota_rows(chat["id"])

    replay = stream(s, chat["id"], {"content": "ignored on replay", "request_id": rid})
    assert replay.status == 200
    assert replay.names() == ["stream_started", "delta", "done"]
    started = replay.of("stream_started")[0]
    assert started == {"request_id": rid, "message_id": first.of("stream_started")[0]["message_id"],
                       "is_new_turn": False}
    assert replay.text() == first.text() == "Hello world"
    done = replay.of("done")[0]
    assert done["usage"] == {"input_tokens": 12, "output_tokens": 5}
    assert done["quota_decision"] == "allow"
    assert "quota_warnings" not in done and "downgrade_reason" not in done
    assert len(reset_mock.requests(path="/responses")) == calls
    assert _quota_rows(chat["id"]) == quota
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["message_count"] == 2


def test_failed_turn_request_id_is_409_conflict(reset_mock):
    reset_mock.enqueue("responses", {"failed": {"code": "server_error", "message": "boom"}})
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    assert stream(s, chat["id"], {"content": "hi", "request_id": rid}).terminal[0] == "error"
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "hi", "request_id": rid})
    assert_problem(r, 409, "aborted", reason="request_id_conflict")


@pytest.mark.parametrize("state,deleted", [("cancelled", False), ("running", False), ("completed", True)])
def test_other_states_are_409_conflict(reset_mock, state, deleted):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    insert_turn(chat["id"], rid, state, deleted=deleted)
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "hi", "request_id": rid})
    assert_problem(r, 409, "aborted", reason="request_id_conflict")
    assert reset_mock.requests(path="/responses") == []


def test_omitted_request_id_is_generated(reset_mock):
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    rid = res.of("stream_started")[0]["request_id"]
    assert uuid.UUID(rid).version == 4
    assert s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").json()["state"] == "done"


def test_replay_is_checked_before_the_parallel_turn_guard(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    assert stream(s, chat["id"], {"content": "hi", "request_id": rid}).terminal[0] == "done"
    calls = len(reset_mock.requests(path="/responses"))
    insert_turn(chat["id"], str(uuid.uuid4()), "running")  # another turn now runs in the chat

    # A new request id hits the parallel-turn guard ...
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "hi"})
    assert_problem(r, 409, "aborted", reason="turn_already_running")
    # ... but the completed request id is still replayed.
    replay = stream(s, chat["id"], {"content": "hi", "request_id": rid})
    assert replay.status == 200, replay.raw
    assert replay.of("stream_started")[0]["is_new_turn"] is False
    assert replay.terminal[0] == "done"
    assert len(reset_mock.requests(path="/responses")) == calls
