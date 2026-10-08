"""Background indexing, cleanup retries, summary failure, audit content,
no buffering, catalog changes and settlement amounts."""

import time
import uuid

import mc
from mc import TENANT_A, USER_A, problem_reason


def _insert_chat(db, model: str) -> str:
    cid = str(uuid.uuid4())
    now = "2026-01-01T00:00:00.000000Z"
    db.x(
        "insert into chats (id, tenant_id, user_id, model, title, is_temporary, created_at, updated_at, deleted_at) "
        "values (?, ?, ?, ?, 'seeded', 0, ?, ?, NULL)",
        mc.uuid_blob(cid),
        mc.uuid_blob(TENANT_A),
        mc.uuid_blob(USER_A),
        model,
        now,
        now,
    )
    return cid


def test_disabled_and_removed_chat_model(api, db, mock):
    cid = _insert_chat(db, "gpt-off")
    s = api.send(cid, "my model was disabled")
    assert s.status == 200, s.body
    done = s.first("done")
    assert done["selected_model"] == "gpt-off"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "model_disabled"
    assert done["effective_model"] != "gpt-off"
    gone = _insert_chat(db, "removed-model")
    mark = mock.mark()
    s = api.send(gone, "my model was removed")
    assert s.status == 400 and problem_reason(s.body) == "INVALID_MODEL"
    r = api.upload(gone, "a.txt", b"x")
    assert r.status_code == 400 and problem_reason(r.json()) == "INVALID_MODEL"
    assert not [x for x in mock.since(mark) if x["path"].endswith(("/responses", "/files"))]


def test_deltas_are_not_buffered(api):
    chat = api.create_chat()
    stamps = []

    def stop(evs):
        stamps.append((time.time(), len(evs)))
        return False

    t0 = time.time()
    s = api.sse("POST", f"/chats/{chat['id']}/messages:stream", {"content": "slow #slow"}, stop)
    total = time.time() - t0
    assert s.terminal[0] == "done"
    first_delta = next(ts for ts, n in stamps if n >= 2)
    assert first_delta - t0 < total / 2, "the first delta must arrive while the provider is still streaming"


def test_audit_events_content(api, env):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    api.send(chat["id"], "audit me", request_id=rid)
    ev = mc.wait_for(lambda: [e for e in mc.audit_events(env) if e.get("request_id") == rid and e["kind"] == "turn"])[0]
    assert ev["event_type"] == "turn_completed"
    assert ev["terminal_state"] == "completed"
    assert ev["tenant_id"] == TENANT_A and ev["user_id"] == USER_A and ev["chat_id"] == chat["id"]
    assert ev["selected_model"] == ev["effective_model"] == "gpt-premium"
    assert ev["policy_decisions"]["quota"]["decision"] == "allow"
    assert ev["usage"]["input_tokens"] == 21
    rid2 = str(uuid.uuid4())
    api.send(chat["id"], "x #fail", request_id=rid2)
    ev = mc.wait_for(lambda: [e for e in mc.audit_events(env) if e.get("request_id") == rid2])[0]
    assert ev["event_type"] == "turn_failed" and ev["error_code"] == "provider_error"


def test_cancel_settlement_amount(api, db, env):
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    api.send_stop(cid, "abc #slow", lambda evs: sum(1 for e, _ in evs if e == "delta") >= 1, request_id=rid)
    api.wait_turn(cid, rid)
    t = db.turn(rid)
    est_input = t["reserve_tokens"] - t["max_output_tokens_applied"]
    expected = est_input * 2 + t["minimal_generation_floor_applied"] * 6
    key = rid.replace("-", "")
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key)])[0]
    assert ev["actual_credits_micro"] == expected
    assert ev["billing_outcome"] == "aborted"


def test_background_indexing_completes_and_fails(api, db, mock):
    chat = api.create_chat()
    cid = chat["id"]
    t0 = time.time()
    r = api.upload(cid, "bg.txt", b"INDEX_BG content")
    assert r.status_code == 201, r.text
    assert r.json()["status"] == "uploaded"
    assert time.time() - t0 < 29, "the upload returns before the gateway timeout"
    att = r.json()["id"]
    mc.wait_for(lambda: api.get(f"/chats/{cid}/attachments/{att}").json()["status"] == "ready", timeout=40)
    r = api.upload(cid, "bgfail.txt", b"INDEX_BGFAIL content")
    assert r.json()["status"] == "uploaded"
    att2 = r.json()["id"]
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(att2))[0]
    mark = mock.mark()
    mc.wait_for(lambda: api.get(f"/chats/{cid}/attachments/{att2}").json()["status"] == "failed", timeout=40)
    g = api.get(f"/chats/{cid}/attachments/{att2}").json()
    assert g["error_code"] == "indexing_failed"
    mc.wait_for(
        lambda: [x for x in mock.since(mark) if x["method"] == "DELETE" and x["path"] == f"/v1/files/{row['provider_file_id']}"],
        timeout=20,
    )
    mc.wait_for(lambda: db.q("select cleanup_status from attachments where id = ?", mc.uuid_blob(att2))[0]["cleanup_status"] == "done")


def test_chat_cleanup_retries_failed_deletes(api, db, mock):
    chat = api.create_chat()
    cid = chat["id"]
    a = api.upload(cid, "doc.txt", b"retry me").json()
    mock.config(delete_fail=True)
    try:
        assert api.delete(f"/chats/{cid}").status_code == 204
        mc.wait_for(
            lambda: db.q("select cleanup_attempts from attachments where id = ?", mc.uuid_blob(a["id"]))[0]["cleanup_attempts"] >= 1,
            timeout=30,
        )
        row = db.q("select * from attachments where id = ?", mc.uuid_blob(a["id"]))[0]
        assert row["cleanup_status"] == "pending" and row["last_cleanup_error"]
        assert db.q("select * from chat_vector_stores where chat_id = ?", mc.uuid_blob(cid)), "vector store kept while files are pending"
    finally:
        mock.config(delete_fail=False)
    mc.wait_for(
        lambda: db.q("select cleanup_status from attachments where id = ?", mc.uuid_blob(a["id"]))[0]["cleanup_status"] == "done",
        timeout=120,
    )
    mc.wait_for(lambda: not db.q("select * from chat_vector_stores where chat_id = ?", mc.uuid_blob(cid)), timeout=120)


def test_summary_failure_keeps_state(api, db, mock):
    chat = api.create_chat(model="gpt-tiny")
    cid = chat["id"]
    mark = mock.mark()
    for i in range(5):
        assert api.send(cid, f"#sumfail turn {i} " + "v" * 600).terminal[0] == "done"
    mc.wait_for(
        lambda: [x for x in mock.since(mark) if x["path"].endswith("/responses") and x["body"] and x["body"].get("stream") is False],
        timeout=30,
    )
    time.sleep(2)
    assert not db.q("select * from thread_summaries where chat_id = ?", mc.uuid_blob(cid))
    assert db.q("select count(*) c from messages where chat_id = ? and is_compressed = 1", mc.uuid_blob(cid))[0]["c"] == 0
    # The chat keeps working without a summary.
    s = api.send(cid, "still fine")
    assert s.terminal[0] == "done"
    assert "thread_summary_applied" not in s.first("stream_started")


def test_downgraded_image_turn_and_quota_rejected_mutation(env, db, mock):
    from test_attachments import make_png

    api = mc.Client(env, "user-quota")
    uid = mc.uuid_blob(mc.USER_QUOTA)
    chat = api.create_chat()
    cid = chat["id"]
    rid = str(uuid.uuid4())
    assert api.send(cid, "baseline", request_id=rid).terminal[0] == "done"
    img = api.upload(cid, "p.png", make_png(), "image/png").json()
    try:
        # Premium exhausted: the cascade picks a model without vision -> 400.
        db.x("update quota_usage set spent_credits_micro = 50000000 where user_id = ? and bucket = 'tier:premium'", uid)
        mark = mock.mark()
        s = api.send(cid, "what is this", attachment_ids=[img["id"]])
        assert s.status == 400 and problem_reason(s.body) == "VISION_NOT_SUPPORTED"
        assert not [x for x in mock.since(mark) if x["path"].endswith("/responses")]
        # All tiers exhausted: retry / edit are rejected and the turn survives.
        db.x("update quota_usage set spent_credits_micro = 1000000000 where user_id = ?", uid)
        r = api.sse("POST", f"/chats/{cid}/turns/{rid}/retry")
        assert r.status == 429 and r.body["context"]["violations"][0]["subject"] == "tokens"
        r = api.sse("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "edited"})
        assert r.status == 429
        assert db.turn(rid)["deleted_at"] is None
        assert api.turn(cid, rid).json()["state"] == "done"
    finally:
        db.x("update quota_usage set spent_credits_micro = 0 where user_id = ?", uid)


def test_retry_bumps_chat_ordering(api):
    a = api.create_chat(title="retry-me")
    rid = str(uuid.uuid4())
    api.send(a["id"], "hello", request_id=rid)
    b = api.create_chat(title="newer")
    api.send(b["id"], "hello")
    order = [c["id"] for c in api.get("/chats", params={"limit": 100}).json()["items"]]
    assert order.index(b["id"]) < order.index(a["id"])
    assert api.sse("POST", f"/chats/{a['id']}/turns/{rid}/retry").terminal[0] == "done"
    order = [c["id"] for c in api.get("/chats", params={"limit": 100}).json()["items"]]
    assert order.index(a["id"]) < order.index(b["id"])


def test_chat_completions_adapter(api, mock, db):
    chat = api.create_chat(model="gpt-cc")
    mark = mock.mark()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "hello cc", request_id=rid)
    assert s.terminal[0] == "done", s.events
    assert s.text() == "Hello from chat completions."
    assert s.first("done")["usage"] == {"input_tokens": 13, "output_tokens": 4}
    req = [x for x in mock.since(mark) if x["path"] == "/v1/chat/completions"][0]["body"]
    assert req["model"] == "mock-cc-model"
    assert req["stream"] is True
    assert req["messages"][0] == {"role": "system", "content": "CC system."}
    assert req["messages"][-1]["role"] == "user"
    assert len(req["user"]) == 64
    assert db.turn(rid)["state"] == "completed"
    s = api.send(chat["id"], "cut #incomplete")
    assert s.terminal[0] == "done"
    s = api.send(chat["id"], "busy #http429")
    assert s.terminal[0] == "error" and s.terminal[1]["code"] == "rate_limited"


def test_vllm_reasoning_deltas(api, mock):
    chat = api.create_chat(model="gpt-vllm")
    mark = mock.mark()
    s = api.send(chat["id"], "think first #think")
    assert s.terminal[0] == "done", s.events
    reasoning = "".join(d["content"] for e, d in s.events if e == "delta" and d["type"] == "reasoning")
    assert reasoning == "pondering"
    assert s.text() == "The answer."
    body = [x for x in mock.since(mark) if x["path"] == "/v1/responses"][0]["body"]
    assert "metadata" not in body
    msgs = api.messages(chat["id"])["items"]
    assert msgs[-1]["content"] == "The answer."
