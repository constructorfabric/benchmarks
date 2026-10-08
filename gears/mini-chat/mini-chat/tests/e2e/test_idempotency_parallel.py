"""Idempotency & replay, parallel-turn enforcement."""

from __future__ import annotations

import uuid

import prov
from harness import BackgroundStream, problem_reason, ub, wait_until


def quota_rows(env):
    return env.server.query(
        "SELECT bucket, period_type, spent_credits_micro, reserved_credits_micro, calls FROM quota_usage ORDER BY bucket, period_type"
    )


def outbox_count(env):
    tables = [r["name"] for r in env.server.query("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%outbox%'")]
    total = 0
    for t in tables:
        if t.endswith("_body") or t.endswith("bodies"):
            continue
    return tables


def test_replay_completed_turn_has_no_side_effects(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    rid = str(uuid.uuid4())
    env.mock.script([prov.text_reply("Original answer", usage={"input_tokens": 50, "output_tokens": 7})])
    first = c.send(chat["id"], "q", request_id=rid)
    assert first.names[-1] == "done"
    wait_until(lambda: env.server.query("SELECT reserved_credits_micro FROM quota_usage WHERE bucket='total' AND period_type='daily'")[0]["reserved_credits_micro"] == 0)
    before_quota = quota_rows(env)
    before_reqs = len(env.mock.chat_requests())
    before_turns = env.server.query("SELECT count(*) AS n FROM chat_turns")[0]["n"]
    before_msgs = c.get(f"/chats/{chat['id']}").json()["message_count"]

    replay = c.send(chat["id"], "different content is ignored", request_id=rid)
    assert replay.status == 200
    assert replay.names == ["stream_started", "delta", "done"]
    st = replay.started
    assert st["is_new_turn"] is False
    assert st["request_id"] == rid
    assert st["message_id"] == first.started["message_id"]
    assert replay.text == "Original answer"
    d = replay.done
    assert d["usage"] == {"input_tokens": 50, "output_tokens": 7}
    assert d["effective_model"] == "gpt-4.1-mini" and d["selected_model"] == "gpt-4.1-mini"
    assert d["quota_decision"] == "allow"
    assert "quota_warnings" not in d and "downgrade_reason" not in d

    assert len(env.mock.chat_requests()) == before_reqs
    assert quota_rows(env) == before_quota
    assert env.server.query("SELECT count(*) AS n FROM chat_turns")[0]["n"] == before_turns
    assert c.get(f"/chats/{chat['id']}").json()["message_count"] == before_msgs


def test_request_id_conflicts_across_states(env):
    c = env.a
    chat = c.create_chat()
    # failed turn
    rid_failed = str(uuid.uuid4())
    env.mock.script([{"kind": "error", "status": 500, "body": {"error": {"message": "x"}}}])
    r = c.send(chat["id"], "a", request_id=rid_failed)
    assert r.first("error").data["code"] == "provider_error"
    r = c.send(chat["id"], "a", request_id=rid_failed)
    assert r.status == 409
    assert r.body["type"].endswith("aborted.v1~")
    assert r.body["context"]["reason"] == "request_id_conflict"
    # running turn with the same request id
    rid_run = str(uuid.uuid4())
    env.mock.script([prov.sse(prov.created(), prov.sleep(4000), prov.delta("x"), prov.completed("x"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "slow", "request_id": rid_run})
    bg.wait_event("stream_started")
    r = c.send(chat["id"], "slow", request_id=rid_run)
    assert r.status == 409 and r.body["context"]["reason"] == "request_id_conflict"
    bg.join()
    assert bg.names[-1] == "done"
    # soft-deleted (deleted turn) -> conflict
    assert c.delete(f"/chats/{chat['id']}/turns/{rid_run}").status_code == 204
    r = c.send(chat["id"], "again", request_id=rid_run)
    assert r.status == 409 and r.body["context"]["reason"] == "request_id_conflict"


def test_cancelled_turn_request_id_conflict(env):
    c = env.a
    chat = c.create_chat()
    rid = str(uuid.uuid4())
    env.mock.script([prov.sse(prov.created(), prov.delta("part"), prov.sleep(20000), prov.completed("part"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "x", "request_id": rid})
    bg.wait_event("delta")
    bg.disconnect()
    wait_until(lambda: c.turn(chat["id"], rid).json()["state"] == "cancelled", 15, msg="cancelled")
    r = c.send(chat["id"], "x", request_id=rid)
    assert r.status == 409 and r.body["context"]["reason"] == "request_id_conflict"


def test_replay_checked_before_parallel_guard(env):
    c = env.a
    chat = c.create_chat()
    rid_done = str(uuid.uuid4())
    c.send(chat["id"], "first", request_id=rid_done)
    env.mock.script([prov.sse(prov.created(), prov.sleep(4000), prov.delta("y"), prov.completed("y"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "running"})
    bg.wait_event("stream_started")
    # a new request is rejected by the parallel guard ...
    r = c.send(chat["id"], "parallel")
    assert r.status == 409 and r.body["context"]["reason"] == "turn_already_running"
    # ... but a replay of the completed request id is served
    replay = c.send(chat["id"], "first", request_id=rid_done)
    assert replay.status == 200 and replay.started["is_new_turn"] is False
    bg.join()


def test_one_running_turn_per_chat_and_next_turn_after_terminal(env):
    c = env.a
    chat = c.create_chat()
    other = c.create_chat()
    env.mock.script([prov.sse(prov.created(), prov.sleep(3000), prov.delta("z"), prov.completed("z"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "long"})
    bg.wait_event("stream_started")
    r = c.send(chat["id"], "second")
    assert r.status == 409
    assert problem_reason(r.body) == "turn_already_running"
    assert r.body["type"].endswith("aborted.v1~")
    # another chat is not blocked
    assert c.send(other["id"], "independent").names[-1] == "done"
    bg.join()
    assert bg.names[-1] == "done"
    # once terminal, a new turn is accepted
    r = c.send(chat["id"], "third")
    assert r.status == 200 and r.names[-1] == "done"
    rows = env.server.query("SELECT state FROM chat_turns WHERE chat_id = ? ORDER BY started_at", (ub(chat["id"]),))
    assert [x["state"] for x in rows] == ["completed", "completed"]
