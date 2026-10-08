"""messages:stream happy path (DESIGN section 3.3 "Streaming Contract", section 3.6)."""

import re
import time
import uuid
from contextlib import closing

import pytest

from . import mock_provider as mp
from .helpers import PREFIX, api, captured_outbox, create_chat, db, reserved_credits, stream, uuid_bytes, wait_until

pytestmark = pytest.mark.usefixtures("server")


def _rows(sql: str, *args):
    with closing(db()) as conn:
        return conn.execute(sql, args).fetchall()


def test_default_script_streams_and_persists(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    res = stream(s, chat["id"], {"content": "hi", "request_id": rid})

    assert res.status == 200, res.raw
    headers = {k.lower(): v for k, v in res.headers.items()}
    assert headers["content-type"].startswith("text/event-stream")
    assert headers["cache-control"] == "no-cache"
    assert res.names() == ["stream_started", "delta", "delta", "done"]
    started = res.of("stream_started")[0]
    assert started["request_id"] == rid and started["is_new_turn"] is True
    assert "thread_summary_applied" not in started
    assert res.of("delta") == [{"type": "text", "content": "Hello"}, {"type": "text", "content": " world"}]
    done = res.of("done")[0]
    assert done["usage"] == {"input_tokens": 12, "output_tokens": 5}
    assert done["effective_model"] == done["selected_model"] == "gpt-premium"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    assert isinstance(done.get("quota_warnings"), list)

    msgs = _rows("SELECT id, role, content, request_id, model, input_tokens, output_tokens, token_estimate"
                 " FROM messages WHERE chat_id = ? ORDER BY created_at, id", uuid_bytes(chat["id"]))
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "hi"), ("assistant", "Hello world")]
    assert msgs[0]["request_id"] == msgs[1]["request_id"] == uuid_bytes(rid)
    # DESIGN section 3.7: `token_estimate` is reserved and always written as 0.
    assert [m["token_estimate"] for m in msgs] == [0, 0]
    assert msgs[1]["id"] == uuid_bytes(started["message_id"])
    assert (msgs[1]["model"], msgs[1]["input_tokens"], msgs[1]["output_tokens"]) == ("gpt-premium", 12, 5)

    turn = _rows("SELECT state, assistant_message_id, provider_response_id FROM chat_turns"
                 " WHERE chat_id = ? AND request_id = ?", uuid_bytes(chat["id"]), uuid_bytes(rid))[0]
    assert turn["state"] == "completed"
    assert turn["assistant_message_id"] == uuid_bytes(started["message_id"])
    assert turn["provider_response_id"].startswith("resp_")  # stored internally only
    assert "resp_" not in res.raw

    # Settlement committed: no reserve left on the caller's rows.
    tenant = _rows("SELECT tenant_id, user_id FROM chats WHERE id = ?", uuid_bytes(chat["id"]))[0]
    wait_until(lambda: _rows("SELECT SUM(reserved_credits_micro) AS r FROM quota_usage"
                             " WHERE tenant_id = ? AND user_id = ?",
                             tenant["tenant_id"], tenant["user_id"])[0]["r"] == 0,
               message="reserve released")

    reqs = reset_mock.requests(path="/responses")
    assert len(reqs) == 1
    body = reqs[0]["json"]
    assert body["stream"] is True
    assert body["model"] == "gpt-premium"
    assert re.fullmatch(r"[0-9a-f]{64}", body["user"]), body["user"]
    assert body["max_output_tokens"] == 16384
    assert body["metadata"]["request_type"] == "chat"
    assert body["metadata"]["chat_id"] == chat["id"]
    assert body["metadata"]["feature"] == "none"
    assert body["instructions"].startswith("You are a helpful assistant.")
    assert body["input"] == [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]


def test_second_turn_sends_history(reset_mock):
    s = api()
    chat = create_chat(s)
    assert stream(s, chat["id"], {"content": "first question"}).terminal[0] == "done"
    res = stream(s, chat["id"], {"content": "second question"})
    assert res.terminal[0] == "done"
    second = reset_mock.requests(path="/responses")[-1]["json"]
    assert second["input"] == [
        {"role": "user", "content": [{"type": "input_text", "text": "first question"}]},
        {"role": "assistant", "content": [{"type": "output_text", "text": "Hello world"}]},
        {"role": "user", "content": [{"type": "input_text", "text": "second question"}]},
    ]
    listed = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    assert [m["role"] for m in listed] == ["user", "assistant", "user", "assistant"]
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["message_count"] == 4


def test_turn_status_after_stream(reset_mock):
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    res = stream(s, chat["id"], {"content": "hi", "request_id": rid})
    status = s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").json()
    assert status["state"] == "done"
    assert status["assistant_message_id"] == res.of("stream_started")[0]["message_id"]


def test_chat_updated_at_is_bumped(reset_mock):
    s = api()
    chat = create_chat(s)
    stream(s, chat["id"], {"content": "hi"})
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["updated_at"] > chat["updated_at"]


def test_send_moves_the_chat_to_the_top_of_the_list(reset_mock):
    s = api()
    older = create_chat(s, title="older")
    newer = create_chat(s, title="newer")
    ids = [c["id"] for c in s.get(f"{PREFIX}/chats", params={"limit": 100}).json()["items"]]
    assert ids.index(newer["id"]) < ids.index(older["id"])
    assert stream(s, older["id"], {"content": "activity"}).terminal[0] == "done"
    ids = [c["id"] for c in s.get(f"{PREFIX}/chats", params={"limit": 100}).json()["items"]]
    assert ids[0] == older["id"]


# ── Turn lifecycle on client disconnect (DESIGN section 3.6 "Cancellation") ─


def _read_until(chat_id: str, rid: str, event: str) -> None:
    """Opens the stream, reads until the first ``event`` and drops the connection."""
    s = api()
    buf = ""
    with s.post(f"{PREFIX}/chats/{chat_id}/messages:stream", json={"content": "hi", "request_id": rid},
                headers={"Accept": "text/event-stream"}, stream=True, timeout=30) as r:
        assert r.status_code == 200, r.text
        for chunk in r.iter_content(chunk_size=None, decode_unicode=True):
            buf += chunk
            if f"event: {event}\n" in buf:
                break


def _turn_row(chat_id: str, rid: str):
    return _rows("SELECT state, assistant_message_id, completed_at FROM chat_turns"
                 " WHERE chat_id = ? AND request_id = ?", uuid_bytes(chat_id), uuid_bytes(rid))[0]


def test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("partial answer")], "hang": True})
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    reserved = reserved_credits(chat["id"])
    _read_until(chat["id"], rid, "delta")

    wait_until(lambda: _turn_row(chat["id"], rid)["state"] == "cancelled", message="turn cancelled")
    turn = _turn_row(chat["id"], rid)
    assert turn["completed_at"] is not None
    msgs = _rows("SELECT id, role, content FROM messages WHERE chat_id = ? AND deleted_at IS NULL"
                 " ORDER BY created_at, id", uuid_bytes(chat["id"]))
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "hi"), ("assistant", "partial answer")]
    assert turn["assistant_message_id"] == msgs[1]["id"]
    status = s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").json()
    assert status["state"] == "cancelled"
    assert status["assistant_message_id"] == str(uuid.UUID(bytes=msgs[1]["id"]))
    # Settled once with an estimate: the reserve is released again.
    wait_until(lambda: reserved_credits(chat["id"]) == reserved, message="reserve released")
    # Hard cancel: the provider connection is dropped too.
    wait_until(lambda: reset_mock.requests(route="responses")[-1]["client_disconnected"],
               message="provider request aborted")
    # The chat accepts a new turn right away.
    assert stream(s, chat["id"], {"content": "again"}).terminal[0] == "done"


def test_client_disconnect_before_any_text_cancels_without_a_message(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created()], "hang": True})
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    _read_until(chat["id"], rid, "stream_started")

    wait_until(lambda: _turn_row(chat["id"], rid)["state"] == "cancelled", message="turn cancelled")
    assert _turn_row(chat["id"], rid)["assistant_message_id"] is None
    roles = [m["role"] for m in _rows("SELECT role FROM messages WHERE chat_id = ?", uuid_bytes(chat["id"]))]
    assert roles == ["user"]
    status = s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").json()
    assert status["state"] == "cancelled" and "assistant_message_id" not in status


def test_provider_failure_after_partial_text_persists_no_answer(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("half an ans")],
                                     "failed": {"code": "server_error", "message": "boom"}})
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    res = stream(s, chat["id"], {"content": "hi", "request_id": rid})
    assert res.names() == ["stream_started", "delta", "error"]
    turn = _turn_row(chat["id"], rid)
    assert turn["state"] == "failed" and turn["assistant_message_id"] is None
    roles = [m["role"] for m in _rows("SELECT role FROM messages WHERE chat_id = ?", uuid_bytes(chat["id"]))]
    assert roles == ["user"]
    status = s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").json()
    assert status["state"] == "error" and status["error_code"] == "provider_error"
    assert "assistant_message_id" not in status


# ── Usage publication (DESIGN section 5.7 "Usage event", outbox) ─────────────

#: Capture table of the ``outbox_capture`` fixture (conftest.py).
CAPTURE = "e2e_usage_outbox_capture"


def _usage_events(rid: str) -> list[dict]:
    out = []
    for queue, _, body in captured_outbox(CAPTURE, "mini-chat.usage.v1"):
        if body.get("request_id") == rid:
            assert queue == "mini-chat.usage_snapshot"
            out.append(body)
    return out


def test_usage_event_is_published_exactly_once_per_turn(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    ok, failed = str(uuid.uuid4()), str(uuid.uuid4())
    assert stream(s, chat["id"], {"content": "hi", "request_id": ok}).terminal[0] == "done"
    reset_mock.enqueue("responses", {"failed": {"code": "server_error", "message": "boom"}})
    assert stream(s, chat["id"], {"content": "hi", "request_id": failed}).terminal[0] == "error"
    # A replay of the completed turn publishes nothing.
    assert stream(s, chat["id"], {"content": "hi", "request_id": ok}).terminal[0] == "done"

    done = wait_until(lambda: _usage_events(ok), message="usage event of the completed turn")
    assert len(done) == 1
    ev = done[0]
    assert (ev["terminal_state"], ev["billing_outcome"], ev["settlement_method"]) == \
        ("completed", "completed", "actual")
    assert ev["usage"]["input_tokens"] == 12 and ev["usage"]["output_tokens"] == 5
    assert ev["chat_id"] == chat["id"] and ev["requester_type"] == "user"
    assert ev["effective_model"] == ev["selected_model"] == "gpt-premium"
    assert ev["actual_credits_micro"] == 12 * 3 + 5 * 15
    err = wait_until(lambda: _usage_events(failed), message="usage event of the failed turn")
    assert len(err) == 1
    assert (err[0]["terminal_state"], err[0]["billing_outcome"]) == ("failed", "failed")
    assert err[0]["dedupe_key"] != ev["dedupe_key"]
    time.sleep(1)  # nothing else arrives later
    assert len(_usage_events(ok)) == 1 and len(_usage_events(failed)) == 1
