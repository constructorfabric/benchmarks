"""US2 black-box: idempotent replay, 409 conflicts, turn status, disconnect -> cancelled."""

import uuid

from conftest import reason, ub, us, wait_until


def _running(db, chat_id):
    return wait_until(
        lambda: [t for t in db.turns(chat_id) if t["state"] == "running"],
        timeout=10,
        desc="running turn",
    )[0]


def _terminal(db, chat_id, request_id):
    return wait_until(
        lambda: (lambda t: t if t["state"] != "running" else None)(db.turn(chat_id, request_id)),
        timeout=15,
        desc="terminal turn",
    )


def _usage_snapshot(db, user=None):
    rows = db.quota_usage() if user is None else db.quota_usage(user_id=user)
    return {k: (v["spent_credits_micro"], v["reserved_credits_micro"], v["calls"], v["updated_at"]) for k, v in rows.items()}


# ------------------------------------------------------------------ replay
def test_replay_completed_turn_is_side_effect_free(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    mock.push({"text": "The original answer", "usage": {"input_tokens": 11, "output_tokens": 5}})
    first = api.send(c["id"], "question", request_id=rid)
    assert first.names[-1] == "done"
    calls_before = len(mock.responses_requests())
    usage_before = wait_until(lambda: _usage_snapshot(db), desc="quota usage rows")
    msgs_before = db.messages(c["id"])
    turns_before = db.turns(c["id"])

    replay = api.send(c["id"], "question", request_id=rid)
    assert replay.status == 200
    assert replay.names == ["stream_started", "delta", "done"]
    st = replay.started
    assert st["is_new_turn"] is False
    assert st["request_id"] == rid
    assert st["message_id"] == first.message_id  # persisted assistant id
    assert replay.all("delta") == [{"type": "text", "content": "The original answer"}]
    done = replay.done
    assert done["usage"] == {"input_tokens": 11, "output_tokens": 5}
    assert done["effective_model"] == done["selected_model"] == "gpt-4.1"
    assert done["quota_decision"] == "allow"
    assert "downgrade_reason" not in done and "quota_warnings" not in done

    # no provider call, no quota change, no new rows
    assert len(mock.responses_requests()) == calls_before
    assert _usage_snapshot(db) == usage_before
    assert db.messages(c["id"]) == msgs_before
    assert db.turns(c["id"]) == turns_before

    # replay with different content still replays (key is (chat_id, request_id))
    again = api.send(c["id"], "something else", request_id=rid)
    assert again.started["is_new_turn"] is False and again.text_content == "The original answer"


def test_replay_checked_before_parallel_turn_guard(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    api.send(c["id"], "first", request_id=rid)
    mock.push({"text": "slow", "delay_before": 2.0})
    bg = api.send_in_background(c["id"], "second")
    _running(db, c["id"])
    replay = api.send(c["id"], "first", request_id=rid)
    assert replay.status == 200 and replay.started["is_new_turn"] is False
    other = api.send(c["id"], "third")
    assert other.status == 409 and reason(other.problem) == "turn_already_running"
    assert bg.join().names[-1] == "done"


def test_server_generated_request_id_is_replayable(api, mock):
    c = api.create_chat()
    res = api.send(c["id"], "no id")
    rid = res.request_id
    uuid.UUID(rid)
    replay = api.send(c["id"], "no id", request_id=rid)
    assert replay.started["is_new_turn"] is False
    assert len(mock.chat_requests()) == 1


# ------------------------------------------------------------------ conflicts
def test_turn_already_running(api, api_a2, mock, db):
    c = api.create_chat()
    mock.push({"text": "slow answer", "delay_before": 2.0})
    rid = str(uuid.uuid4())
    bg = api.send_in_background(c["id"], "slow", request_id=rid)
    running = _running(db, c["id"])
    assert us(running["request_id"]) == rid

    # running turn: status endpoint
    st = api.turn_status(c["id"], rid)
    assert st.status_code == 200
    body = st.json()
    assert body["state"] == "running"
    assert "error_code" not in body and "assistant_message_id" not in body
    assert api_a2.turn_status(c["id"], rid).status_code == 404

    # a different request id -> turn_already_running
    r = api.send(c["id"], "parallel")
    assert r.status == 409
    assert r.problem["context"]["reason"] == "turn_already_running"
    assert r.problem["type"].endswith("cf.core.err.aborted.v1~")
    # the same request id while running -> request_id_conflict
    r = api.send(c["id"], "slow", request_id=rid)
    assert r.status == 409 and reason(r.problem) == "request_id_conflict"
    assert rid not in r.problem.get("detail", "")

    res = bg.join()
    assert res.names[-1] == "done"
    # exactly one provider call was made
    assert len(mock.chat_requests()) == 1
    # a new turn is accepted once the previous one is terminal
    assert api.send(c["id"], "next").names[-1] == "done"


def test_request_id_conflict_for_failed_turn(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    mock.push({"end": "failed"})
    res = api.send(c["id"], "boom", request_id=rid)
    assert res.error["code"] == "provider_error"
    calls = len(mock.chat_requests())
    r = api.send(c["id"], "boom", request_id=rid)
    assert r.status == 409 and reason(r.problem) == "request_id_conflict"
    assert len(mock.chat_requests()) == calls
    assert db.turn(c["id"], rid)["state"] == "failed"


def test_request_id_conflict_for_deleted_turn(api, mock):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    api.send(c["id"], "to delete", request_id=rid)
    assert api.delete_turn(c["id"], rid).status_code == 204
    r = api.send(c["id"], "to delete", request_id=rid)
    assert r.status == 409 and reason(r.problem) == "request_id_conflict"
    assert api.turn_status(c["id"], rid).status_code == 404


def test_request_id_is_scoped_per_chat(api, mock):
    c1, c2 = api.create_chat(), api.create_chat()
    rid = str(uuid.uuid4())
    assert api.send(c1["id"], "a", request_id=rid).started["is_new_turn"] is True
    assert api.send(c2["id"], "b", request_id=rid).started["is_new_turn"] is True
    assert len(mock.chat_requests()) == 2


# ------------------------------------------------------------------ turn status
def test_turn_status_done_and_not_found(api, api_a2, api_b):
    c = api.create_chat()
    res = api.send(c["id"], "hello")
    r = api.turn_status(c["id"], res.request_id)
    assert r.status_code == 200
    body = r.json()
    assert body["request_id"] == res.request_id
    assert body["state"] == "done"
    assert body["assistant_message_id"] == res.message_id
    assert "error_code" not in body
    assert "chat_id" not in body
    assert set(body) <= {"request_id", "state", "error_code", "assistant_message_id", "updated_at"}
    for other in (api_a2, api_b):
        assert other.turn_status(c["id"], res.request_id).status_code == 404
    nf = api.turn_status(c["id"], str(uuid.uuid4()))
    assert nf.status_code == 404
    assert nf.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.turn.v1~"


# ------------------------------------------------------------------ disconnect
def test_client_disconnect_cancels_turn_with_partial_content(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    mock.push({"chunks": ["part one ", "part two ", "part three ", "part four"], "delay": 0.8})
    seen = api.open_and_drop(
        c["id"], "please stream", lambda ev: any(n == "delta" for n, _ in ev), request_id=rid
    )
    assert seen[0][0] == "stream_started"
    message_id = seen[0][1]["message_id"]
    turn = _terminal(db, c["id"], rid)
    assert turn["state"] == "cancelled"
    assert us(turn["assistant_message_id"]) == message_id

    st = api.turn_status(c["id"], rid).json()
    assert st["state"] == "cancelled"
    assert st["assistant_message_id"] == message_id
    assert "error_code" not in st

    msgs = api.messages(c["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    partial = msgs[1]["content"]
    assert partial.startswith("part one") and "part four" not in partial

    # the upstream request is abandoned (gear stops reading)
    wait_until(lambda: mock.chat_requests()[0].get("aborted"), timeout=10, desc="upstream aborted")

    # cancellation settles the reserve (estimated) and releases the chat
    def settled():
        u = db.quota_usage()
        return u if u and all(r["reserved_credits_micro"] == 0 for r in u.values()) else None

    usage = wait_until(settled, desc="settlement")
    assert all(r["spent_credits_micro"] > 0 for r in usage.values())
    r = api.send(c["id"], "please stream", request_id=rid)
    assert r.status == 409 and reason(r.problem) == "request_id_conflict"
    assert api.send(c["id"], "new one").names[-1] == "done"


def test_disconnect_before_first_delta_cancels_without_message(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    mock.push({"text": "never seen", "delay_before": 3.0})
    seen = api.open_and_drop(c["id"], "wait", lambda ev: any(n == "stream_started" for n, _ in ev), request_id=rid)
    assert [n for n, _ in seen] == ["stream_started"]
    turn = _terminal(db, c["id"], rid)
    assert turn["state"] == "cancelled"
    assert turn["assistant_message_id"] is None
    st = api.turn_status(c["id"], rid).json()
    assert st["state"] == "cancelled" and "assistant_message_id" not in st
    assert [m["role"] for m in api.messages(c["id"])] == ["user"]
    assert db.rows(
        "SELECT * FROM messages WHERE chat_id = ? AND role = 'assistant'", ub(c["id"])
    ) == []


def test_turns_are_owned_by_requester(api, api_a2):
    c = api.create_chat()
    res = api.send(c["id"], "mine")
    # another user of the same tenant cannot see the chat or its turns
    assert api_a2.turn_status(c["id"], res.request_id).status_code == 404
    assert api_a2.retry(c["id"], res.request_id).status == 404
    assert api_a2.delete_turn(c["id"], res.request_id).status_code == 404
