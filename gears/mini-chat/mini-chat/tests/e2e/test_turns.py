"""Turn mutations (retry / edit / delete) and the turn lifecycle
(cancellation, partial content, orphan watchdog)."""

import time
import uuid

import mc
from mc import USER_A, problem_reason


def _turn(api, cid, content, **kw):
    rid = str(uuid.uuid4())
    s = api.send(cid, content, request_id=rid, **kw)
    assert s.status == 200, s.body
    return rid, s


def test_retry_latest_turn(api, db, mock, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid1, _ = _turn(api, cid, "first")
    rid2, s2 = _turn(api, cid, "second")
    mark = mock.mark()
    s = api.sse("POST", f"/chats/{cid}/turns/{rid2}/retry")
    assert s.status == 200, s.body
    st = s.first("stream_started")
    assert st["is_new_turn"] is True
    new_rid = st["request_id"]
    assert new_rid not in (rid1, rid2)
    assert uuid.UUID(new_rid).version == 4
    assert s.terminal[0] == "done"
    # Retry re-sends the original content through the full pipeline.
    body = [r for r in mock.since(mark) if r["path"].endswith("/responses")][0]["body"]
    assert "second" in str(body["input"][-1]["content"])
    contents = [str(i["content"]) for i in body["input"]]
    assert sum("second" in c for c in contents) == 1, "replaced turn is not in the history"
    # Old turn soft-deleted and linked.
    old = db.turn(rid2)
    assert old["deleted_at"] is not None
    assert mc.blob_uuid(old["replaced_by_request_id"]) == new_rid
    assert api.turn(cid, rid2).status_code == 404
    assert api.turn(cid, new_rid).json()["state"] == "done"
    msgs = api.messages(cid)["items"]
    assert [m["request_id"] for m in msgs] == [rid1, rid1, new_rid, new_rid]
    assert msgs[2]["content"] == "second"
    # Replay of the replaced request id is a conflict.
    r = api.send(cid, "x", request_id=rid2)
    assert r.status == 409 and problem_reason(r.body) == "request_id_conflict"
    # Audit event.
    ev = mc.wait_for(lambda: [e for e in mc.audit_events(env) if e.get("event_type") == "turn_retry" and e.get("new_request_id") == new_rid])
    assert ev[0]["original_request_id"] == rid2
    assert ev[0]["actor_user_id"] == USER_A


def test_edit_latest_turn(api, db, mock, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid, _ = _turn(api, cid, "original text")
    r = api.sse("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "  "})
    assert r.status == 400 and problem_reason(r.body) == "EMPTY_CONTENT"
    assert api.sse("PATCH", f"/chats/{cid}/turns/{rid}", {}).status == 422
    mark = mock.mark()
    s = api.sse("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "edited text"})
    assert s.status == 200 and s.terminal[0] == "done"
    new_rid = s.first("stream_started")["request_id"]
    body = [r for r in mock.since(mark) if r["path"].endswith("/responses")][0]["body"]
    assert "edited text" in str(body["input"][-1]["content"])
    assert "original text" not in str(body["input"])
    msgs = api.messages(cid)["items"]
    assert [m["content"] for m in msgs][0] == "edited text"
    assert len(msgs) == 2
    assert db.turn(rid)["deleted_at"] is not None
    mc.wait_for(lambda: [e for e in mc.audit_events(env) if e.get("event_type") == "turn_edit" and e.get("new_request_id") == new_rid])


def test_delete_latest_turn(api, db, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid1, _ = _turn(api, cid, "one")
    rid2, _ = _turn(api, cid, "two")
    # Only the latest turn can be mutated.
    for method, path, body in [
        ("POST", f"/chats/{cid}/turns/{rid1}/retry", None),
        ("PATCH", f"/chats/{cid}/turns/{rid1}", {"content": "x"}),
    ]:
        r = api.sse(method, path, body)
        assert r.status == 409 and problem_reason(r.body) == "NOT_LATEST_TURN", r.body
    r = api.delete(f"/chats/{cid}/turns/{rid1}")
    assert r.status_code == 409 and problem_reason(r.json()) == "NOT_LATEST_TURN"

    assert api.delete(f"/chats/{cid}/turns/{rid2}").status_code == 204
    assert api.turn(cid, rid2).status_code == 404
    t = db.turn(rid2)
    assert t["deleted_at"] is not None and t["replaced_by_request_id"] is None
    assert [m["request_id"] for m in api.messages(cid)["items"]] == [rid1, rid1]
    assert api.get(f"/chats/{cid}").json()["message_count"] == 2
    # A deleted turn is not latest anymore.
    r = api.delete(f"/chats/{cid}/turns/{rid2}")
    assert r.status_code == 409 and problem_reason(r.json()) == "NOT_LATEST_TURN"
    # The previous turn is now the latest and can be deleted.
    assert api.delete(f"/chats/{cid}/turns/{rid1}").status_code == 204
    assert api.messages(cid)["items"] == []
    assert api.delete(f"/chats/{cid}/turns/{uuid.uuid4()}").status_code == 404
    mc.wait_for(lambda: [e for e in mc.audit_events(env) if e.get("event_type") == "turn_delete" and e.get("request_id") == rid2])


def test_mutation_of_running_turn_and_concurrency(api):
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    t, out = mc.in_thread(lambda: api.send(cid, "slow #slow", request_id=rid))
    mc.wait_for(lambda: api.turn(cid, rid).status_code == 200)
    for method, path, body in [
        ("POST", f"/chats/{cid}/turns/{rid}/retry", None),
        ("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "x"}),
    ]:
        r = api.sse(method, path, body)
        assert r.status == 400, r.body
        v = r.body["context"]["violations"][0]
        assert (v["subject"], v["type"]) == ("turn_state", "STATE")
    r = api.delete(f"/chats/{cid}/turns/{rid}")
    assert r.status_code == 400
    t.join(30)
    assert out["result"].terminal[0] == "done"

    # Concurrent retries: exactly one wins.
    results = []
    threads = []
    for _ in range(3):
        th, o = mc.in_thread(lambda: api.sse("POST", f"/chats/{cid}/turns/{rid}/retry"))
        threads.append(th)
        results.append(o)
    for th in threads:
        th.join(60)
    oks = [o["result"] for o in results if o["result"].status == 200]
    errs = [o["result"] for o in results if o["result"].status != 200]
    assert len(oks) == 1, [(o["result"].status, o["result"].body) for o in results]
    for e in errs:
        assert e.status in (409, 400), e.body
        if e.status == 409:
            assert problem_reason(e.body) in ("NOT_LATEST_TURN", "GENERATION_IN_PROGRESS")
    turns = api.messages(cid)["items"]
    assert len(turns) == 2


def test_mutation_permissions(api, api_a2):
    chat = api.create_chat()
    cid = chat["id"]
    rid, _ = _turn(api, cid, "mine")
    assert api_a2.sse("POST", f"/chats/{cid}/turns/{rid}/retry").status == 404
    assert api_a2.delete(f"/chats/{cid}/turns/{rid}").status_code == 404


def test_mutation_carries_attachments_and_web_search(api, mock):
    chat = api.create_chat()
    cid = chat["id"]
    up = api.upload(cid, "notes.txt", b"some notes about the project")
    assert up.status_code == 201, up.text
    att = up.json()
    rid = str(uuid.uuid4())
    s = api.send(cid, "use it", request_id=rid, attachment_ids=[att["id"]], web_search={"enabled": True})
    assert s.terminal[0] == "done"
    mark = mock.mark()
    s = api.sse("POST", f"/chats/{cid}/turns/{rid}/retry")
    assert s.terminal[0] == "done"
    body = [r for r in mock.since(mark) if r["path"].endswith("/responses")][0]["body"]
    kinds = {t["type"] for t in body.get("tools", [])}
    assert "web_search" in kinds and "file_search" in kinds
    msgs = api.messages(cid)["items"]
    assert msgs[0]["attachments"][0]["attachment_id"] == att["id"]


def test_mutation_preflight_rejection_keeps_previous_turn(api, db):
    chat = api.create_chat(model="gpt-tiny")
    cid = chat["id"]
    rid, _ = _turn(api, cid, "short")
    # Edit content that exceeds the input limit is rejected before the commit.
    r = api.sse("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "q" * 5000})
    assert r.status == 400 and problem_reason(r.body) == "INPUT_TOO_LONG"
    assert db.turn(rid)["deleted_at"] is None
    assert api.turn(cid, rid).json()["state"] == "done"


def test_cancel_persists_partial_content(api, db, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    s = api.send_stop(cid, "stream slowly #slow", lambda evs: sum(1 for e, _ in evs if e == "delta") >= 3, request_id=rid)
    assert s.names()[0] == "stream_started"
    st = api.wait_turn(cid, rid)
    assert st["state"] == "cancelled"
    assert "error_code" not in st
    msg_id = st["assistant_message_id"]
    msgs = api.messages(cid)["items"]
    assert msgs[-1]["id"] == msg_id
    assert msgs[-1]["role"] == "assistant"
    assert msgs[-1]["content"].startswith("w0 w1 w2")
    assert msg_id == s.first("stream_started")["message_id"]
    t = db.turn(rid)
    assert t["state"] == "cancelled" and t["completed_at"] is not None
    key = rid.replace("-", "")
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key)])[0]
    assert ev["billing_outcome"] == "aborted" and ev["settlement_method"] == "estimated"
    assert ev["actual_credits_micro"] > 0
    # A new turn is accepted after cancellation.
    assert api.send(cid, "next").terminal[0] == "done"


def test_cancel_without_content(api, db):
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    s = api.send_stop(cid, "nothing #hang", lambda evs: len(evs) >= 1, request_id=rid)
    assert s.names() == ["stream_started"]
    st = api.wait_turn(cid, rid)
    assert st["state"] == "cancelled"
    assert "assistant_message_id" not in st
    assert [m["role"] for m in api.messages(cid)["items"]] == ["user"]
    assert db.turn(rid)["assistant_message_id"] is None


def test_orphan_watchdog_finalizes_stale_turn(api, db, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    t, out = mc.in_thread(lambda: api.send(cid, "stuck #hang", request_id=rid))
    mc.wait_for(lambda: api.turn(cid, rid).status_code == 200)
    # Make the turn stale (last progress 10 minutes ago).
    db.x(
        "update chat_turns set last_progress_at = '2020-01-01T00:00:00Z' where request_id = ?",
        mc.uuid_blob(rid),
    )
    st = api.wait_turn(cid, rid, states=("error",), timeout=20)
    assert st["error_code"] == "orphan_timeout"
    trow = db.turn(rid)
    assert trow["state"] == "failed"
    key = rid.replace("-", "")
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key)])[0]
    assert ev["billing_outcome"] == "aborted" and ev["settlement_method"] == "estimated"
    q = db.quota(USER_A)
    assert all(r["reserved_credits_micro"] >= 0 for r in q.values())
    # The hanging client connection gets no done event; close it.
    t.join(1)
