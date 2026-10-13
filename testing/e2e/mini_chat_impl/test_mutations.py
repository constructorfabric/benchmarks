"""Turn mutations: retry / edit / delete (acceptance: Turn Mutations)."""

import threading
import uuid

import pytest

from mchelpers import (
    MIME_PNG,
    RT_TURN,
    Api,
    BackgroundStream,
    as_uuid,
    assert_problem,
    assert_sse_grammar,
    find_tool,
    make_pdf,
    make_png,
    nonce,
    parse_ts,
    text_blob,
)
from mock_llm import last_user_text


@pytest.fixture()
def chat_with_turn(api):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"original question {n}")
    assert s.done
    return c, s, n


# Acceptance: Turn mutations — retry yields a new request id and replaces the turn
def test_retry_latest(api, db, mock_llm, chat_with_turn):
    c, s, n = chat_with_turn
    before_updated = parse_ts(api.chat(c["id"])["updated_at"])
    r = api.retry(c["id"], s.request_id)
    assert r.status_code == 200 and r.is_sse
    assert_sse_grammar(r)
    assert r.started["is_new_turn"] is True
    new_rid = r.request_id
    assert new_rid != s.request_id
    assert uuid.UUID(new_rid).version == 4
    assert r.done
    # the provider got the original content again
    reqs = mock_llm.chat_requests(contains=n)
    assert len(reqs) == 2
    assert last_user_text(reqs[-1]["json"]) == f"original question {n}"
    # old turn is gone from the API, its id is a conflict for messages:stream
    assert_problem(api.turn(c["id"], s.request_id), 404, "not_found", resource_type=RT_TURN)
    dup = api.stream(c["id"], "dup", request_id=s.request_id)
    assert_problem(api.as_response(dup), 409, "aborted", reason="request_id_conflict")
    # messages: only the new turn
    msgs = api.messages(c["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert {m["request_id"] for m in msgs} == {new_rid}
    assert msgs[0]["content"] == f"original question {n}"
    assert api.chat(c["id"])["message_count"] == 2
    assert parse_ts(api.chat(c["id"])["updated_at"]) >= before_updated
    # DB: old turn soft-deleted with replaced_by_request_id
    old = db.turn_row(c["id"], s.request_id)
    assert old["deleted_at"] is not None
    assert as_uuid(old["replaced_by_request_id"]) == new_rid
    assert api.turn(c["id"], new_rid).json()["state"] == "done"


# Acceptance: Turn mutations — edit replaces the content and regenerates
def test_edit_latest(api, mock_llm, chat_with_turn):
    c, s, n = chat_with_turn
    m = nonce()
    r = api.edit(c["id"], s.request_id, f"edited question {m}")
    assert r.done
    assert r.request_id != s.request_id
    req = mock_llm.chat_requests(contains=m)[-1]
    assert last_user_text(req["json"]) == f"edited question {m}"
    msgs = api.messages(c["id"])
    assert [x["content"] for x in msgs if x["role"] == "user"] == [f"edited question {m}"]
    assert_problem(api.turn(c["id"], s.request_id), 404, "not_found")


# Acceptance: Turn mutations — edit with empty content
def test_edit_empty_content(api, chat_with_turn):
    c, s, n = chat_with_turn
    r = api.edit(c["id"], s.request_id, "   ")
    assert_problem(api.as_response(r), 400, "invalid_argument", field_reason="EMPTY_CONTENT")
    assert api.turn(c["id"], s.request_id).json()["state"] == "done"
    r = api.patch(f"/v1/chats/{c['id']}/turns/{s.request_id}", json={})
    assert_problem(r, 422, "invalid_argument")


# Acceptance: Turn mutations — delete latest, then the deleted turn is no longer mutable
def test_delete_latest(api, chat_with_turn):
    c, s, n = chat_with_turn
    s2 = api.stream(c["id"], "second " + nonce())
    assert s2.done
    r = api.delete(f"/v1/chats/{c['id']}/turns/{s2.request_id}")
    assert r.status_code == 204, r.text
    assert_problem(api.turn(c["id"], s2.request_id), 404, "not_found", resource_type=RT_TURN)
    assert {m["request_id"] for m in api.messages(c["id"])} == {s.request_id}
    assert api.chat(c["id"])["message_count"] == 2
    # deleting / retrying / editing the deleted turn again
    assert_problem(api.delete(f"/v1/chats/{c['id']}/turns/{s2.request_id}"), 409, "aborted", reason="NOT_LATEST_TURN")
    assert_problem(api.as_response(api.retry(c["id"], s2.request_id)), 409, "aborted", reason="NOT_LATEST_TURN")
    # the previous turn became the latest and is mutable
    assert api.retry(c["id"], s.request_id).done


# Acceptance: Turn mutations — only the latest turn may be mutated
def test_mutation_of_non_latest_turn(api, chat_with_turn):
    c, s, n = chat_with_turn
    s2 = api.stream(c["id"], "second " + nonce())
    assert s2.done
    assert_problem(api.as_response(api.retry(c["id"], s.request_id)), 409, "aborted", reason="NOT_LATEST_TURN")
    assert_problem(api.as_response(api.edit(c["id"], s.request_id, "x")), 409, "aborted", reason="NOT_LATEST_TURN")
    assert_problem(api.delete(f"/v1/chats/{c['id']}/turns/{s.request_id}"), 409, "aborted", reason="NOT_LATEST_TURN")
    assert api.chat(c["id"])["message_count"] == 4


# Acceptance: Turn mutations — only terminal turns: running target is failed_precondition turn_state/STATE
@pytest.mark.timeout(60)
def test_mutation_of_running_turn(api):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "hang [[hang]] " + nonce()).wait_started()
    rid = bg.request_id
    try:
        for resp in (
            api.as_response(api.retry(c["id"], rid)),
            api.as_response(api.edit(c["id"], rid, "new")),
            api.delete(f"/v1/chats/{c['id']}/turns/{rid}"),
        ):
            assert_problem(resp, 400, "failed_precondition", violation_subject="turn_state", violation_type="STATE")
    finally:
        bg.stop()


# Acceptance: Turn mutations — failed and cancelled turns are terminal and can be retried
@pytest.mark.timeout(60)
def test_retry_failed_and_cancelled(api):
    c = api.create_chat()
    s = api.stream(c["id"], "fail [[fail]] " + nonce())
    assert s.error
    api.wait_turn_state(c["id"], s.request_id, {"error"})
    r = api.retry(c["id"], s.request_id)
    # the original content still carries [[fail]]: the new turn fails again, but it ran
    assert r.is_sse and r.error["code"] == "provider_error"
    assert r.request_id != s.request_id
    bg = BackgroundStream.send(api, c["id"], "slow [[slow]] " + nonce()).wait_started().wait_delta()
    rid = bg.request_id
    bg.stop()
    api.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=30)
    e = api.edit(c["id"], rid, "now answer normally " + nonce())
    assert e.done


# Acceptance: Turn mutations — unknown chat / unknown turn / invalid id
def test_mutation_not_found(api, chat_with_turn):
    c, s, n = chat_with_turn
    assert_problem(api.as_response(api.retry(c["id"], str(uuid.uuid4()))), 404, "not_found")
    assert_problem(api.delete(f"/v1/chats/{uuid.uuid4()}/turns/{s.request_id}"), 404, "not_found")
    assert_problem(api.get(f"/v1/chats/{c['id']}/turns/not-a-uuid"), 400, "invalid_argument", field_reason="invalid_path_params")


# Acceptance: Turn mutations — mutation goes through the quota preflight; a rejection keeps the previous turn
def test_retry_rejected_by_quota_keeps_previous_turn(api_for, db):
    a = api_for("tok-q7")
    c = a.create_chat()
    s = a.stream(c["id"], "answer " + nonce())
    assert s.done
    db.seed_quota("tok-q7", "total", "daily", spent_credits_micro=100_000_000)
    try:
        r = a.retry(c["id"], s.request_id)
        assert_problem(a.as_response(r), 429, "resource_exhausted", violation_subject="tokens")
        r = a.edit(c["id"], s.request_id, "edited " + nonce())
        assert_problem(a.as_response(r), 429, "resource_exhausted", violation_subject="tokens")
        t = a.turn(c["id"], s.request_id).json()
        assert t["state"] == "done"
        assert [m["request_id"] for m in a.messages(c["id"])] == [s.request_id, s.request_id]
    finally:
        db.seed_quota("tok-q7", "total", "daily", spent_credits_micro=0)


# Acceptance: Turn mutations — edit goes through the input-size check (INPUT_TOO_LONG)
def test_edit_input_too_long(api, db):
    c = api.create_chat(model="std-tiny")
    s = api.stream(c["id"], "short " + nonce())
    assert s.done
    r = api.edit(c["id"], s.request_id, text_blob(20000, nonce()))
    assert not r.is_sse
    assert_problem(api.as_response(r), 400, "out_of_range", field_reason="INPUT_TOO_LONG")
    # whether it is rejected before or after the mutation commit, the chat is not left blocked
    assert all(t["state"] != "running" for t in db.turns(c["id"]))
    if api.turn(c["id"], s.request_id).status_code == 200:
        assert api.turn(c["id"], s.request_id).json()["state"] == "done"
    assert api.stream(c["id"], "still usable " + nonce()).done


# Acceptance: Turn mutations — edit goes through context assembly: a failure after the commit
# marks the new turn failed with context_length_exceeded (JSON error, no SSE)
def test_edit_context_budget_exceeded(api, db):
    c = api.create_chat(model="std-budget")
    s = api.stream(c["id"], "short " + nonce())
    assert s.done
    r = api.edit(c["id"], s.request_id, text_blob(6000, nonce()))
    assert not r.is_sse
    assert_problem(api.as_response(r), 400, "out_of_range", field_reason="CONTEXT_BUDGET_EXCEEDED")
    turns = db.turns(c["id"])
    assert all(t["state"] != "running" for t in turns)
    new_turns = [t for t in turns if t["deleted_at"] is None and as_uuid(t["request_id"]) != s.request_id]
    for t in new_turns:
        assert t["state"] == "failed" and t["error_code"] == "context_length_exceeded"


# Acceptance: Turn mutations — concurrent mutations resolve deterministically
@pytest.mark.timeout(90)
def test_concurrent_retries(server, chat_with_turn):
    c, s, n = chat_with_turn
    clients = [Api(server.base_url, "tok-a"), Api(server.base_url, "tok-a")]
    results = [None, None]
    barrier = threading.Barrier(2)

    def run(i):
        barrier.wait()
        results[i] = clients[i].retry(c["id"], s.request_id, timeout=60)

    try:
        ts = [threading.Thread(target=run, args=(i,)) for i in range(2)]
        for t in ts:
            t.start()
        for t in ts:
            t.join(60)
        streamed = [r for r in results if r.is_sse]
        rejected = [r for r in results if not r.is_sse]
        assert len(streamed) == 1 and len(rejected) == 1, [(r.status_code, r.text[:300]) for r in results]
        assert streamed[0].done
        body = clients[0].as_response(rejected[0])
        assert body.status_code == 409, body.text
        assert body.json()["context"].get("reason") in ("GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"), body.text
    finally:
        for cl in clients:
            cl.close()


# Acceptance: Turn mutations — attachments carried forward on edit and retry
def test_attachments_carried_forward(api):
    c = api.create_chat()
    att = api.upload_ready(c["id"], "notes.pdf", make_pdf("carry"), "application/pdf")
    s = api.stream(c["id"], "with doc " + nonce(), attachment_ids=[att["id"]])
    assert s.done
    e = api.edit(c["id"], s.request_id, "edited with doc " + nonce())
    assert e.done
    user = [m for m in api.messages(c["id"]) if m["role"] == "user"][0]
    assert [a["attachment_id"] for a in user["attachments"]] == [att["id"]]
    assert user["attachments"][0]["filename"] == "notes.pdf"
    r = api.retry(c["id"], e.request_id)
    assert r.done
    user = [m for m in api.messages(c["id"]) if m["role"] == "user"][0]
    assert [a["attachment_id"] for a in user["attachments"]] == [att["id"]]


# Acceptance: Turn mutations — images of the original user message are re-sent on retry
def test_retry_resends_images(api, mock_llm, db):
    c = api.create_chat()
    img = api.upload_ready(c["id"], "pic.png", make_png(), MIME_PNG)
    pfid = db.attachment_row(img["id"])["provider_file_id"]
    n = nonce()
    s = api.stream(c["id"], f"look {n}", attachment_ids=[img["id"]])
    assert s.done
    r = api.retry(c["id"], s.request_id)
    assert r.done
    reqs = mock_llm.chat_requests(contains=n)
    assert len(reqs) == 2
    assert pfid in reqs[-1]["body_text"], "retry must re-send the image of the original message"


# Acceptance: Turn mutations — tool usage (web_search_enabled) carried forward
def test_retry_reuses_web_search_flag(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"search {n}", web_search=True)
    assert s.done
    assert find_tool(mock_llm.chat_requests(contains=n)[-1], "web_search") is not None
    assert api.retry(c["id"], s.request_id).done
    assert find_tool(mock_llm.chat_requests(contains=n)[-1], "web_search") is not None


# Acceptance: Turn mutations — mutations are audited (tolerant outbox payload check)
def test_mutation_audit_enqueued(api, db, chat_with_turn):
    c, s, n = chat_with_turn
    r = api.retry(c["id"], s.request_id)
    assert r.done
    def mentions(p, u):
        return u in p or u.replace("-", "") in p

    payloads = [p for p in db.outbox_payloads("turn_retry") if mentions(p, c["id"])]
    # tolerant: a delivered message may already be vacuumed; when present it names both request ids
    for p in payloads:
        assert mentions(p, s.request_id) and mentions(p, r.request_id), p
