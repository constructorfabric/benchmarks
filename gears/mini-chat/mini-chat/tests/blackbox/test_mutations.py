"""US3 black-box: retry / edit / delete of the last turn (DESIGN §3.9)."""

import uuid

from conftest import Background, reason, ub, us, wait_until


def _send(api, mock, chat_id, content, answer):
    mock.push({"text": answer})
    res = api.send(chat_id, content)
    assert res.names[-1] == "done", res
    return res


def _running(db, chat_id):
    return wait_until(lambda: [t for t in db.turns(chat_id) if t["state"] == "running"], desc="running turn")[0]


def _contents(api, chat_id):
    return [(m["role"], m["content"]) for m in api.messages(chat_id)]


def test_retry_last_turn(api, mock, db):
    c = api.create_chat()
    _send(api, mock, c["id"], "q1", "a1")
    t2 = _send(api, mock, c["id"], "q2", "a2")
    mock.push({"text": "a2 retried"})
    res = api.retry(c["id"], t2.request_id)
    assert res.status == 200, res
    assert res.names[0] == "stream_started" and res.names[-1] == "done"
    new_rid = res.request_id
    assert new_rid != t2.request_id
    assert res.started["is_new_turn"] is True
    # the provider got the original user message again, with prior history
    assert mock.chat_requests()[-1]["json"]["input"] == [
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": "a1"},
        {"role": "user", "content": "q2"},
    ]
    assert _contents(api, c["id"]) == [("user", "q1"), ("assistant", "a1"), ("user", "q2"), ("assistant", "a2 retried")]
    msgs = api.messages(c["id"])
    assert msgs[-1]["request_id"] == msgs[-2]["request_id"] == new_rid

    old = db.turn(c["id"], t2.request_id)
    assert old["deleted_at"] is not None
    assert us(old["replaced_by_request_id"]) == new_rid
    new = db.turn(c["id"], new_rid)
    assert new["state"] == "completed" and new["deleted_at"] is None
    # old messages are soft-deleted, retained for audit
    old_msgs = db.rows("SELECT * FROM messages WHERE request_id = ?", ub(t2.request_id))
    assert len(old_msgs) == 2 and all(m["deleted_at"] is not None for m in old_msgs)

    assert api.turn_status(c["id"], t2.request_id).status_code == 404
    assert api.turn_status(c["id"], new_rid).json()["state"] == "done"
    # old request id can no longer be replayed
    r = api.send(c["id"], "q2", request_id=t2.request_id)
    assert r.status == 409 and reason(r.problem) == "request_id_conflict"


def test_edit_last_turn(api, mock, db):
    c = api.create_chat()
    _send(api, mock, c["id"], "q1", "a1")
    t2 = _send(api, mock, c["id"], "q2", "a2")
    mock.push({"text": "edited answer"})
    res = api.edit(c["id"], t2.request_id, "q2 edited")
    assert res.status == 200 and res.names[-1] == "done", res
    assert res.request_id != t2.request_id
    assert mock.chat_requests()[-1]["json"]["input"][-1] == {"role": "user", "content": "q2 edited"}
    assert _contents(api, c["id"]) == [("user", "q1"), ("assistant", "a1"), ("user", "q2 edited"), ("assistant", "edited answer")]
    assert us(db.turn(c["id"], t2.request_id)["replaced_by_request_id"]) == res.request_id
    # edit validates content like a send
    r = api.edit(c["id"], res.request_id, "")
    assert r.status == 400


def test_delete_last_turn(api, mock, db):
    c = api.create_chat()
    t1 = _send(api, mock, c["id"], "q1", "a1")
    t2 = _send(api, mock, c["id"], "q2", "a2")
    r = api.delete_turn(c["id"], t2.request_id)
    assert r.status_code == 204
    assert _contents(api, c["id"]) == [("user", "q1"), ("assistant", "a1")]
    old = db.turn(c["id"], t2.request_id)
    assert old["deleted_at"] is not None and old["replaced_by_request_id"] is None
    assert api.get(f"/chats/{c['id']}").json()["message_count"] == 2
    # the deleted turn is gone from the context of the next turn
    _send(api, mock, c["id"], "q3", "a3")
    assert mock.chat_requests()[-1]["json"]["input"] == [
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": "a1"},
        {"role": "user", "content": "q3"},
    ]
    # deleting / mutating the already deleted turn -> NOT_LATEST_TURN
    for r in (
        api.delete_turn(c["id"], t2.request_id),
        api.retry(c["id"], t2.request_id),
        api.edit(c["id"], t2.request_id, "x"),
    ):
        status = r.status_code if hasattr(r, "status_code") else r.status
        problem = r.json() if hasattr(r, "json") else r.problem
        assert status == 409 and reason(problem) == "NOT_LATEST_TURN"
    assert t1.request_id


def test_mutating_a_non_latest_turn_is_rejected(api, mock, db):
    c = api.create_chat()
    t1 = _send(api, mock, c["id"], "q1", "a1")
    _send(api, mock, c["id"], "q2", "a2")
    calls = len(mock.chat_requests())
    before = db.turns(c["id"])
    r = api.retry(c["id"], t1.request_id)
    assert r.status == 409 and reason(r.problem) == "NOT_LATEST_TURN"
    assert r.problem["context"]["resource_type"] == "gts.cf.core.mini_chat.turn.v1~"
    r = api.edit(c["id"], t1.request_id, "new")
    assert r.status == 409 and reason(r.problem) == "NOT_LATEST_TURN"
    r = api.delete_turn(c["id"], t1.request_id)
    assert r.status_code == 409 and reason(r.json()) == "NOT_LATEST_TURN"
    assert len(mock.chat_requests()) == calls
    assert db.turns(c["id"]) == before
    r = api.retry(c["id"], str(uuid.uuid4()))
    assert r.status in (404, 409)


def test_mutation_of_running_turn_is_failed_precondition(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    mock.push({"text": "slow", "delay_before": 2.0})
    bg = api.send_in_background(c["id"], "slow", request_id=rid)
    _running(db, c["id"])
    for r in (api.retry(c["id"], rid), api.edit(c["id"], rid, "x")):
        assert r.status == 400, r
        v = r.problem["context"]["violations"][0]
        assert v["subject"] == "turn_state" and v["type"] == "STATE"
        assert r.problem["type"].endswith("cf.core.err.failed_precondition.v1~")
    r = api.delete_turn(c["id"], rid)
    assert r.status_code == 400
    assert bg.join().names[-1] == "done"
    # once terminal, mutation is allowed
    assert api.delete_turn(c["id"], rid).status_code == 204


def test_retry_after_failed_and_cancelled_turns(api, mock, db):
    c = api.create_chat()
    mock.push({"end": "failed"})
    failed = api.send(c["id"], "will fail")
    assert failed.error["code"] == "provider_error"
    mock.push({"text": "recovered"})
    res = api.retry(c["id"], failed.request_id)
    assert res.names[-1] == "done" and res.text_content == "recovered"
    assert _contents(api, c["id"]) == [("user", "will fail"), ("assistant", "recovered")]

    rid = str(uuid.uuid4())
    mock.push({"chunks": ["p1 ", "p2 ", "p3"], "delay": 0.8})
    api.open_and_drop(c["id"], "cancel me", lambda ev: any(n == "delta" for n, _ in ev), request_id=rid)
    wait_until(lambda: db.turn(c["id"], rid)["state"] == "cancelled", desc="cancelled")
    mock.push({"text": "edited after cancel"})
    res = api.edit(c["id"], rid, "edited content")
    assert res.names[-1] == "done"
    assert _contents(api, c["id"])[-2:] == [("user", "edited content"), ("assistant", "edited after cancel")]


def test_concurrent_retries_resolve_deterministically(api, mock, db):
    c = api.create_chat()
    t = _send(api, mock, c["id"], "q", "a")
    for _ in range(2):
        mock.push({"text": "retried", "delay_before": 1.5})
    a = Background(lambda: api.retry(c["id"], t.request_id))
    b = Background(lambda: api.retry(c["id"], t.request_id))
    ra, rb = a.join(), b.join()
    statuses = sorted([ra.status, rb.status])
    assert statuses == [200, 409], (ra, rb)
    loser = ra if ra.status == 409 else rb
    assert reason(loser.problem) in ("NOT_LATEST_TURN", "GENERATION_IN_PROGRESS")
    winner = ra if ra.status == 200 else rb
    assert winner.names[-1] == "done"
    live = db.turns(c["id"], include_deleted=False)
    assert len(live) == 1 and us(live[0]["request_id"]) == winner.request_id
    assert len(mock.chat_requests()) == 2  # original + one retry


def test_retry_carries_attachments_forward(api, mock, db):
    c = api.create_chat()
    up = api.upload(c["id"], "notes.txt", b"important notes " * 30, "text/plain")
    assert up.status_code == 201, up.text
    att = up.json()["id"]
    mock.push({"text": "with doc"})
    t = api.send(c["id"], "use the doc", attachment_ids=[att])
    assert t.names[-1] == "done"
    mock.push({"text": "retried with doc"})
    res = api.retry(c["id"], t.request_id)
    assert res.names[-1] == "done"
    body = mock.chat_requests()[-1]["json"]
    assert any(tool["type"] == "file_search" for tool in body.get("tools", []))
    user_msg = api.messages(c["id"])[0]
    assert user_msg["request_id"] == res.request_id
    assert [a["attachment_id"] for a in user_msg["attachments"]] == [att]
    links = db.rows("SELECT * FROM message_attachments WHERE attachment_id = ?", ub(att))
    assert len(links) == 2  # old (soft-deleted message) + copied to the new one
