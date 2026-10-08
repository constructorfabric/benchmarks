"""Send message, SSE contract, provider errors, sanitization, idempotency,
parallel turns, provider request format."""

import re
import time
import uuid

import mc
from mc import USER_A, problem_reason

PROVIDER_ID_RE = re.compile(r"(resp_|chatcmpl-|file-|vs_|sk-)[A-Za-z0-9]{6,}")


def test_send_streams_and_persists(api, db, mock, env):
    chat = api.create_chat()
    mark = mock.mark()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "hello world", request_id=rid)
    assert s.status == 200
    assert s.headers["content-type"].startswith("text/event-stream")
    names = s.names()
    assert names[0] == "stream_started"
    assert names[-1] == "done"
    assert "ping" not in names[names.index("delta"):]
    started = s.first("stream_started")
    assert started["request_id"] == rid
    assert started["is_new_turn"] is True
    assert "thread_summary_applied" not in started
    assert s.text() == "Hello from the mock provider."
    done = s.first("done")
    assert done["usage"] == {"input_tokens": 21, "output_tokens": 7}
    assert done["effective_model"] == done["selected_model"] == "gpt-premium"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    assert isinstance(done["quota_warnings"], list) and done["quota_warnings"]
    for w in done["quota_warnings"]:
        assert set(w) <= {"tier", "period", "remaining_percentage", "warning", "exhausted", "next_reset"}

    # Assistant message + usage persisted; ids match the stream.
    msgs = api.messages(chat["id"])["items"]
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert all(m["request_id"] == rid for m in msgs)
    assert msgs[1]["id"] == started["message_id"]
    assert msgs[1]["content"] == "Hello from the mock provider."
    assert msgs[1]["model"] == "gpt-premium"
    assert (msgs[1]["input_tokens"], msgs[1]["output_tokens"]) == (21, 7)
    assert "input_tokens" not in msgs[0] and "model" not in msgs[0]
    t = db.turn(rid)
    assert t["state"] == "completed"
    assert mc.blob_uuid(t["assistant_message_id"]) == started["message_id"]
    assert t["error_code"] is None
    assert t["provider_response_id"].startswith("resp_")
    row = db.q("select * from messages where id = ?", mc.uuid_blob(started["message_id"]))[0]
    assert row["provider_response_id"].startswith("resp_")
    assert (row["cache_read_input_tokens"], row["reasoning_tokens"]) == (3, 1)

    # Provider request format.
    reqs = [r for r in mock.since(mark) if r["path"].endswith("/responses")]
    assert len(reqs) == 1
    body = reqs[0]["body"]
    assert body["model"] == "mock-premium"
    assert body["stream"] is True
    assert body["store"] is False
    assert body["max_output_tokens"] == 4096
    assert body["instructions"].startswith("You are a test assistant.")
    assert body["user"] == USER_A.replace("-", "")[:0] + mc.TENANT_A.replace("-", "") + USER_A.replace("-", "")
    assert len(body["user"]) == 64
    assert body["metadata"] == {
        "tenant_id": mc.TENANT_A,
        "user_id": USER_A,
        "chat_id": chat["id"],
        "request_type": "chat",
        "feature": "none",
    }
    assert body["input"][-1]["role"] == "user"
    assert "hello world" in str(body["input"][-1]["content"])
    assert "tools" not in body or body["tools"] == []

    # Usage published exactly once with the dedupe key.
    key_suffix = rid.replace("-", "")
    mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key_suffix)])
    time.sleep(1)
    evs = [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key_suffix)]
    assert len(evs) == 1
    assert evs[0]["billing_outcome"] == "completed"
    assert evs[0]["settlement_method"] == "actual"
    assert evs[0]["actual_credits_micro"] == 21 * 2 + 7 * 6


def test_server_generated_request_id_and_history(api, mock):
    chat = api.create_chat(model="gpt-standard")
    s1 = api.send(chat["id"], "first question")
    rid1 = s1.first("stream_started")["request_id"]
    assert uuid.UUID(rid1).version == 4
    mark = mock.mark()
    s2 = api.send(chat["id"], "second question")
    assert s2.terminal[0] == "done"
    body = [r for r in mock.since(mark) if r["path"].endswith("/responses")][0]["body"]
    roles = [i["role"] for i in body["input"]]
    assert roles == ["user", "assistant", "user"]
    assert "first question" in str(body["input"][0]["content"])
    assert body["model"] == "mock-standard"
    assert body["instructions"].startswith("You are a standard assistant.")
    msgs = api.messages(chat["id"])["items"]
    assert len(msgs) == 4
    assert [m["role"] for m in msgs] == ["user", "assistant", "user", "assistant"]
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 4


def test_preflight_validation_before_provider(api, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    cases = [
        ({"content": "   "}, 400, "EMPTY_CONTENT"),
        ({"content": ""}, 400, "EMPTY_CONTENT"),
        ({"content": "x", "attachment_ids": [str(uuid.uuid4())]}, 400, "invalid_attachment"),
        ({"content": "x", "attachment_ids": ["a" * 8]}, 422, None),
    ]
    dup = str(uuid.uuid4())
    cases.append(({"content": "x", "attachment_ids": [dup, dup]}, 400, "invalid_attachment"))
    cases.append(({"content": "x", "attachment_ids": [str(uuid.uuid4()) for _ in range(6)]}, 400, "invalid_attachment"))
    for body, status, reason in cases:
        s = api.sse("POST", f"/chats/{cid}/messages:stream", body)
        assert s.status == status, (body, s.body)
        if reason:
            assert problem_reason(s.body) == reason, s.body
    assert api.sse("POST", f"/chats/{cid}/messages:stream", {"nope": 1}).status == 422
    assert api.sse("POST", f"/chats/{uuid.uuid4()}/messages:stream", {"content": "x"}).status == 404
    # Nothing reached the provider and nothing was written.
    assert not [r for r in mock.since(mark) if r["path"].endswith("/responses")]
    assert db.q("select count(*) c from chat_turns where chat_id = ?", mc.uuid_blob(cid))[0]["c"] == 0
    assert db.q("select count(*) c from messages where chat_id = ?", mc.uuid_blob(cid))[0]["c"] == 0


def test_input_and_context_budget(api, mock, db):
    chat = api.create_chat(model="gpt-tiny")
    mark = mock.mark()
    s = api.send(chat["id"], "y" * 5000)
    assert s.status == 400
    assert problem_reason(s.body) == "INPUT_TOO_LONG"
    assert not [r for r in mock.since(mark) if r["path"].endswith("/responses")]
    # Within the input limit, the assembled request is truncated to the budget.
    for i in range(4):
        s = api.send(chat["id"], f"message {i} " + "z" * 600)
        assert s.terminal[0] == "done", s.events
    last = [r for r in mock.since(mark) if r["path"].endswith("/responses")][-1]["body"]
    sent = sum(len(str(i["content"])) for i in last["input"])
    assert sent < 4 * 600, "older history must be dropped to fit the budget"
    assert "message 3" in str(last["input"][-1]["content"])


def test_provider_error_is_sanitized_and_terminal(api, db, env):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "please #fail", request_id=rid)
    assert s.status == 200
    assert s.names()[0] == "stream_started"
    name, data = s.terminal
    assert name == "error"
    assert data["code"] == "provider_error"
    assert not PROVIDER_ID_RE.search(data["message"]), data["message"]
    assert "https://" not in data["message"]
    assert "[provider_id]" in data["message"] and "[url]" in data["message"] and "[credential]" in data["message"]
    assert s.names().count("error") == 1
    st = api.turn(chat["id"], rid).json()
    assert st["state"] == "error" and st["error_code"] == "provider_error"
    assert "assistant_message_id" not in st
    t = db.turn(rid)
    assert t["state"] == "failed"
    assert t["assistant_message_id"] is None
    # No assistant message is persisted for a failed turn.
    roles = [m["role"] for m in api.messages(chat["id"])["items"]]
    assert roles == ["user"]
    # Failed with known usage settles on actual usage.
    key = rid.replace("-", "")
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key)])[0]
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "actual"


def test_http_errors_map_to_stream_codes(api, db):
    chat = api.create_chat()
    for directive, code in [("#http429", "rate_limited"), ("#http500", "provider_error"), ("#noterminal", "provider_error")]:
        rid = str(uuid.uuid4())
        s = api.send(chat["id"], f"go {directive}", request_id=rid)
        assert s.terminal[0] == "error", s.events
        assert s.terminal[1]["code"] == code
        if directive == "#http429":
            assert "7" in s.terminal[1]["message"]
        assert not PROVIDER_ID_RE.search(s.terminal[1]["message"])
        t = db.turn(rid)
        assert t["state"] == "failed" and t["error_code"] == code


def test_incomplete_is_completed(api, db):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "long #incomplete", request_id=rid)
    assert s.terminal[0] == "done"
    assert "citations" not in s.names()
    t = db.turn(rid)
    assert t["state"] == "completed" and t["error_code"] is None


def test_empty_completion_persists_empty_message(api, db):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "say nothing #empty", request_id=rid)
    assert s.terminal[0] == "done"
    msgs = api.messages(chat["id"])["items"]
    assert msgs[-1]["role"] == "assistant" and msgs[-1]["content"] == ""


def test_idempotent_replay_is_side_effect_free(api, db, mock, env):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    first = api.send(chat["id"], "replay me", request_id=rid)
    assert first.terminal[0] == "done"
    quota_before = {k: dict(v) for k, v in db.quota(USER_A).items()}
    mark = mock.mark()
    usage_before = len(mc.usage_events(env))
    replay = api.send(chat["id"], "different content is ignored", request_id=rid)
    assert replay.names() == ["stream_started", "delta", "done"]
    st = replay.first("stream_started")
    assert st["is_new_turn"] is False
    assert st["request_id"] == rid
    assert st["message_id"] == first.first("stream_started")["message_id"]
    assert replay.text() == first.text()
    done = replay.first("done")
    assert done["usage"] == first.first("done")["usage"]
    assert done["quota_decision"] == "allow"
    assert "quota_warnings" not in done
    assert not [r for r in mock.since(mark) if r["path"].endswith("/responses")]
    quota_after = {k: dict(v) for k, v in db.quota(USER_A).items()}
    for k in quota_before:
        for col in ("spent_credits_micro", "reserved_credits_micro", "calls"):
            assert quota_before[k][col] == quota_after[k][col]
    time.sleep(1)
    assert len(mc.usage_events(env)) == usage_before
    assert len(api.messages(chat["id"])["items"]) == 2


def test_request_id_conflicts(api):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "x #fail", request_id=rid)
    assert s.terminal[0] == "error"
    r = api.send(chat["id"], "again", request_id=rid)
    assert r.status == 409
    assert problem_reason(r.body) == "request_id_conflict"
    # A request id used in another chat by the same user is independent.
    other = api.create_chat()
    assert api.send(other["id"], "ok", request_id=rid).terminal[0] == "done"


def test_parallel_turn_guard_and_replay_precedence(api):
    chat = api.create_chat()
    cid = chat["id"]
    done_rid = str(uuid.uuid4())
    assert api.send(cid, "completed first", request_id=done_rid).terminal[0] == "done"
    t, out = mc.in_thread(lambda: api.send(cid, "slow one #slow"))
    mc.wait_for(lambda: api.get(f"/chats/{cid}/messages").json()["items"][-1]["content"] == "slow one #slow")
    time.sleep(0.3)
    r = api.send(cid, "parallel")
    assert r.status == 409
    assert problem_reason(r.body) == "turn_already_running"
    # Replay is checked before the parallel-turn guard.
    replay = api.send(cid, "x", request_id=done_rid)
    assert replay.status == 200 and replay.first("stream_started")["is_new_turn"] is False
    # A running turn's request_id is a conflict, not a replay.
    t.join(30)
    assert out["result"].terminal[0] == "done"
    rid = out["result"].first("stream_started")["request_id"]
    # Once terminal, new turns are accepted.
    assert api.send(cid, "after").terminal[0] == "done"
    assert api.turn(cid, rid).json()["state"] == "done"


def test_ping_before_content(api):
    chat = api.create_chat()
    s = api.send_stop(chat["id"], "wait #hang", lambda evs: any(e == "ping" for e, _ in evs))
    assert s.names()[0] == "stream_started"
    pings = s.all("ping")
    assert pings and pings[0] == {}


def test_turn_status_unknown(api):
    chat = api.create_chat()
    r = api.turn(chat["id"], str(uuid.uuid4()))
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.turn.v1~"
