"""Turn mutations (retry / edit / delete) and the turn lifecycle."""

from __future__ import annotations

import threading
import time
import uuid

import prov
from harness import BackgroundStream, from_blob, problem_reason, ub, wait_until

PNG_1PX = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
    "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
)


def turn_row(env, rid):
    rows = env.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))
    return rows[0] if rows else None


def test_retry_latest_turn(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    c.send(chat["id"], "first")
    r0 = c.send(chat["id"], "second")
    old_rid = r0.started["request_id"]
    env.mock.reset()
    env.mock.script([prov.text_reply("A retried answer")])
    r = c.retry(chat["id"], old_rid)
    assert r.status == 200
    assert r.names[0] == "stream_started" and r.names[-1] == "done"
    new_rid = r.started["request_id"]
    assert new_rid != old_rid and r.started["is_new_turn"] is True
    assert uuid.UUID(new_rid).version == 4
    assert r.text == "A retried answer"
    # same user content re-submitted
    body = env.mock.chat_requests()[0]["json"]
    assert body["input"][-1]["content"] == [{"type": "input_text", "text": "second"}]
    assert all(m.get("content") != "Echo: second" for m in body["input"])
    # old turn gone from the API, recorded in storage
    assert c.turn(chat["id"], old_rid).status_code == 404
    old = turn_row(env, old_rid)
    assert old["deleted_at"] is not None and from_blob(old["replaced_by_request_id"]) == new_rid
    assert c.turn(chat["id"], new_rid).json()["state"] == "done"
    msgs = c.messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [
        ("user", "first"),
        ("assistant", "Echo: first"),
        ("user", "second"),
        ("assistant", "A retried answer"),
    ]
    assert msgs[2]["request_id"] == new_rid
    assert c.get(f"/chats/{chat['id']}").json()["message_count"] == 4
    # replaying the replaced request id is a conflict
    r = c.send(chat["id"], "second", request_id=old_rid)
    assert r.status == 409 and r.body["context"]["reason"] == "request_id_conflict"
    wait_until(lambda: "turn_retry" in env.server.log_text(), msg="turn_retry audit event")


def test_edit_latest_turn(env):
    c = env.a
    chat = c.create_chat()
    r0 = c.send(chat["id"], "original question")
    rid = r0.started["request_id"]
    r = c.edit(chat["id"], rid, "   ")
    assert r.status == 400 and problem_reason(r.body) == "EMPTY_CONTENT"
    r = c.stream_raw("PATCH", f"/chats/{chat['id']}/turns/{rid}", {})
    assert r.status == 422
    r = c.edit(chat["id"], rid, "edited question")
    assert r.status == 200 and r.names[-1] == "done"
    assert r.text == "Echo: edited question"
    msgs = c.messages(chat["id"])
    assert [m["content"] for m in msgs] == ["edited question", "Echo: edited question"]
    wait_until(lambda: "turn_edit" in env.server.log_text(), msg="turn_edit audit event")


def test_delete_latest_turn_and_rules(env):
    c = env.a
    chat = c.create_chat()
    r1 = c.send(chat["id"], "one")
    r2 = c.send(chat["id"], "two")
    rid1, rid2 = r1.started["request_id"], r2.started["request_id"]
    # only the latest turn can be mutated
    for resp in (c.retry(chat["id"], rid1), c.edit(chat["id"], rid1, "x")):
        assert resp.status == 409 and resp.body["context"]["reason"] == "NOT_LATEST_TURN"
    d = c.delete(f"/chats/{chat['id']}/turns/{rid1}")
    assert d.status_code == 409 and d.json()["context"]["reason"] == "NOT_LATEST_TURN"
    # unknown turn
    assert c.delete(f"/chats/{chat['id']}/turns/{uuid.uuid4()}").status_code == 404
    assert c.retry(chat["id"], str(uuid.uuid4())).status == 404
    # delete the latest
    d = c.delete(f"/chats/{chat['id']}/turns/{rid2}")
    assert d.status_code == 204
    assert c.turn(chat["id"], rid2).status_code == 404
    assert [m["content"] for m in c.messages(chat["id"])] == ["one", "Echo: one"]
    # deleted turn cannot be mutated again
    d = c.delete(f"/chats/{chat['id']}/turns/{rid2}")
    assert d.status_code == 409 and d.json()["context"]["reason"] == "NOT_LATEST_TURN"
    # the previous turn is now the latest
    assert c.retry(chat["id"], rid1).names[-1] == "done"
    wait_until(lambda: "turn_delete" in env.server.log_text(), msg="turn_delete audit event")


def test_running_turn_cannot_be_mutated(env):
    c = env.a
    chat = c.create_chat()
    env.mock.script([prov.sse(prov.created(), prov.sleep(4000), prov.delta("x"), prov.completed("x"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "slow"})
    rid = bg.wait_event("stream_started").data["request_id"]
    for resp in (c.retry(chat["id"], rid), c.edit(chat["id"], rid, "e")):
        assert resp.status == 400
        assert resp.body["type"].endswith("failed_precondition.v1~")
        v = resp.body["context"]["violations"][0]
        assert (v["subject"], v["type"]) == ("turn_state", "STATE")
    d = c.delete(f"/chats/{chat['id']}/turns/{rid}")
    assert d.status_code == 400
    bg.join()


def test_mutation_runs_full_pipeline_checks(env):
    c = env.a
    chat = c.create_chat(model="tiny-ctx")
    rid = c.send(chat["id"], "short").started["request_id"]
    r = c.edit(chat["id"], rid, "z" * 12000)
    assert r.status == 400 and problem_reason(r.body) == "INPUT_TOO_LONG"
    # rejected before the mutation: the turn is untouched
    assert c.turn(chat["id"], rid).json()["state"] == "done"
    assert turn_row(env, rid)["deleted_at"] is None


def test_concurrent_mutations_resolve_deterministically(env):
    c = env.a
    chat = c.create_chat()
    rid = c.send(chat["id"], "base").started["request_id"]
    env.mock.script([prov.text_reply("r1", delay_ms=200), prov.text_reply("r2", delay_ms=200)])
    results = []

    def go():
        results.append(c.server.client(c.token).retry(chat["id"], rid))

    ts = [threading.Thread(target=go) for _ in range(4)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    ok = [r for r in results if r.status == 200]
    rejected = [r for r in results if r.status != 200]
    assert len(ok) == 1, [(r.status, r.body) for r in results]
    for r in rejected:
        assert r.status == 409
        assert r.body["context"]["reason"] in ("GENERATION_IN_PROGRESS", "NOT_LATEST_TURN")
    rows = env.server.query("SELECT count(*) AS n FROM chat_turns WHERE chat_id = ? AND deleted_at IS NULL", (ub(chat["id"]),))
    assert rows[0]["n"] == 1


def test_mutation_carries_forward_attachments_and_web_search(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    up = c.upload(chat["id"], "pic.png", PNG_1PX, "image/png")
    assert up.status_code == 201, up.text
    att = up.json()
    rid = c.send(chat["id"], "describe", attachment_ids=[att["id"]], web_search={"enabled": True}).started["request_id"]
    env.mock.reset()
    r = c.retry(chat["id"], rid)
    assert r.names[-1] == "done"
    body = env.mock.chat_requests()[0]["json"]
    parts = body["input"][-1]["content"]
    assert parts[0] == {"type": "input_text", "text": "describe"}
    assert parts[1]["type"] == "input_image" and parts[1]["file_id"].startswith("file-")
    assert any(t["type"] == "web_search" for t in body["tools"])
    new_rid = r.started["request_id"]
    assert turn_row(env, new_rid)["web_search_enabled"] == 1
    msgs = c.messages(chat["id"])
    user = [m for m in msgs if m["role"] == "user"][-1]
    assert [a["attachment_id"] for a in user["attachments"]] == [att["id"]]
    # an attachment deleted since the original turn is silently excluded:
    # (the attachment is locked while referenced, so delete the turn first)
    rid2 = c.send(chat["id"], "follow-up").started["request_id"]
    assert c.retry(chat["id"], rid2).names[-1] == "done"


def test_turn_state_machine_failed_and_cancelled(env):
    c = env.a
    chat = c.create_chat()
    # failed with no content: no assistant message
    env.mock.script([{"kind": "error", "status": 500, "body": {"error": {"message": "x"}}}])
    rf = c.send(chat["id"], "fail")
    rid_f = rf.started["request_id"]
    t = c.turn(chat["id"], rid_f).json()
    assert t["state"] == "error" and t["error_code"] == "provider_error" and "assistant_message_id" not in t
    assert [m["role"] for m in c.messages(chat["id"])] == ["user"]
    # cancelled with partial content: partial assistant message persisted
    env.mock.script([prov.sse(prov.created(), prov.delta("partial answer"), prov.sleep(30000), prov.completed("x"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "cancel me"})
    rid_c = bg.wait_event("stream_started").data["request_id"]
    bg.wait_event("delta")
    bg.disconnect()
    t = wait_until(lambda: (lambda j: j if j["state"] == "cancelled" else None)(c.turn(chat["id"], rid_c).json()), 15, msg="cancel")
    assert "error_code" not in t
    msgs = c.messages(chat["id"])
    asst = [m for m in msgs if m["request_id"] == rid_c and m["role"] == "assistant"]
    assert len(asst) == 1 and asst[0]["content"] == "partial answer"
    assert t["assistant_message_id"] == asst[0]["id"]
    row = turn_row(env, rid_c)
    assert row["state"] == "cancelled" and row["completed_at"] is not None
    # cancelled before any content: no assistant message
    env.mock.script([prov.sse(prov.created(), prov.sleep(30000), prov.completed("x"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "cancel early"})
    rid_e = bg.wait_event("stream_started").data["request_id"]
    time.sleep(0.5)
    bg.disconnect()
    t = wait_until(lambda: (lambda j: j if j["state"] == "cancelled" else None)(c.turn(chat["id"], rid_e).json()), 15, msg="cancel")
    assert "assistant_message_id" not in t
    assert not [m for m in c.messages(chat["id"]) if m["request_id"] == rid_e and m["role"] == "assistant"]
    # every terminal turn released its reserve
    wait_until(
        lambda: all(r["reserved_credits_micro"] == 0 for r in env.server.query("SELECT reserved_credits_micro FROM quota_usage")),
        msg="reserves released",
    )
    # a new turn is accepted after the cancelled one
    assert c.send(chat["id"], "after").names[-1] == "done"
