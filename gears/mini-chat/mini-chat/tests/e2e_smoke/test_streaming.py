"""messages:stream — SSE contract, provider request, idempotency, parallel guard, provider errors,
web search, keepalive and cancellation (DESIGN §3.3 Streaming Contract, SSE Event Definitions,
Streaming error codes, Idempotency Rules, Parallel Turn Policy, Streaming Cancellation)."""

import re
import time
import uuid

import pytest
import requests

from conftest import (
    CHAT_RT,
    TENANT_A,
    USER_A,
    Client,
    assert_no_provider_ids,
    assert_problem,
    background_stream,
    sse_request,
    wait_until,
)

ORDER_RE = re.compile(r"^stream_started( ping)*( (delta|tool))*( citations)? (done|error)$")


def assert_order(res):
    seq = " ".join(res.names)
    assert ORDER_RE.match(seq), seq
    assert res.names.count("stream_started") == 1


def test_stream_event_order_and_payloads(api, chat, fresh_mock):
    rid = str(uuid.uuid4())
    res = api.stream(chat["id"], "hello there", request_id=rid)
    assert res.status == 200
    assert res.headers["content-type"].startswith("text/event-stream")
    assert res.headers.get("cache-control") == "no-cache"
    assert_order(res)
    assert res.terminal_name == "done"
    started = res.started
    assert started["request_id"] == rid
    assert started["is_new_turn"] is True
    uuid.UUID(started["message_id"])
    assert "thread_summary_applied" not in started
    deltas = res.all("delta")
    assert [d["type"] for d in deltas] == ["text"] * len(deltas)
    assert res.text == "Hello from mock."
    done = res.terminal
    assert done["usage"] == {"input_tokens": 120, "output_tokens": 30}
    assert done["effective_model"] == "gpt-4.1"
    assert done["selected_model"] == "gpt-4.1"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    warnings = done["quota_warnings"]
    assert {(w["tier"], w["period"]) for w in warnings} == {
        ("premium", "daily"), ("premium", "monthly"), ("total", "daily"), ("total", "monthly")}
    for w in warnings:
        assert 0 <= w["remaining_percentage"] <= 100
        assert w["warning"] is False and w["exhausted"] is False
        assert "next_reset" not in w  # only when warning/exhausted
    assert "citations" not in res.names
    assert_no_provider_ids(res.raw)

    # Persisted turn + messages.
    turn = api.get(f"/chats/{chat['id']}/turns/{rid}").json()
    assert turn["state"] == "done"
    assert turn["assistant_message_id"] == started["message_id"]
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["id"] == started["message_id"]
    assert msgs[1]["content"] == "Hello from mock."
    assert {m["request_id"] for m in msgs} == {rid}
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2


def test_server_generates_request_id(api, chat):
    res = api.turn(chat["id"], "no id")
    rid = res.started["request_id"]
    assert uuid.UUID(rid).version == 4
    assert api.get(f"/chats/{chat['id']}/turns/{rid}").json()["state"] == "done"


def test_provider_request_body(api, chat, fresh_mock):
    api.turn(chat["id"], "what is up")
    calls = fresh_mock.responses_calls()
    assert len(calls) == 1
    body = calls[0]["body"]
    assert body["model"] == "gpt-4.1"
    assert body["stream"] is True
    assert body["instructions"].startswith("You are a test assistant.")
    assert "web_search" not in body["instructions"]
    assert body["input"][-1]["role"] == "user"
    content = body["input"][-1]["content"]
    text = content if isinstance(content, str) else " ".join(p.get("text", "") for p in content)
    assert "what is up" in text
    expected_user = TENANT_A.replace("-", "") + USER_A.replace("-", "")
    assert body["user"] == expected_user and len(body["user"]) == 64
    md = body["metadata"]
    assert md["request_type"] == "chat"
    assert md["chat_id"] == chat["id"]
    assert md["tenant_id"] == TENANT_A and md["user_id"] == USER_A
    assert md["feature"] == "none"
    tools = body.get("tools") or []
    assert not any(t.get("type") in ("file_search", "web_search") for t in tools), tools


def test_history_is_sent_on_second_turn(api, chat, fresh_mock):
    api.turn(chat["id"], "first question")
    api.turn(chat["id"], "second question")
    body = fresh_mock.responses_calls()[-1]["body"]
    flat = str(body["input"])
    assert "first question" in flat and "Hello from mock." in flat and "second question" in flat


def test_chat_updated_at_bumped_by_turn(api, chat):
    time.sleep(0.01)
    api.turn(chat["id"], "bump")
    after = api.get(f"/chats/{chat['id']}").json()
    assert after["updated_at"] > chat["updated_at"]


def test_empty_content_rejected(api, chat, fresh_mock):
    for content in ("", "   \n\t "):
        res = api.stream(chat["id"], content)
        assert res.status == 400
        assert_problem(res.problem, 400, "invalid_argument", field="content", reason="EMPTY_CONTENT")
    assert fresh_mock.responses_calls() == []


def test_stream_schema_errors(api, chat):
    r = api.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "request_id": "nope"})
    assert_problem(r, 422, "invalid_argument", reason="invalid_json_body")
    r = api.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "attachment_ids": ["nope"]})
    assert_problem(r, 422, "invalid_argument", reason="invalid_json_body")


def test_stream_unknown_chat(api):
    res = api.stream(str(uuid.uuid4()), "hi")
    assert_problem(res.problem, 404, "not_found", resource_type=CHAT_RT)


# ------------------------------------------------------------------ idempotency
def test_replay_completed_request_id(api, chat, fresh_mock, db):
    rid = str(uuid.uuid4())
    first = api.turn(chat["id"], "replay me", request_id=rid)
    time.sleep(1.0)  # let the outbox settle
    calls_before = len(fresh_mock.responses_calls())
    quota_before = db.quota_rows()
    bodies_before = len(db.outbox_bodies())
    replay = api.stream(chat["id"], "different text is ignored", request_id=rid)
    assert replay.status == 200
    assert replay.names == ["stream_started", "delta", "done"]
    assert replay.started == {"request_id": rid, "message_id": first.started["message_id"], "is_new_turn": False}
    assert replay.first("delta") == {"type": "text", "content": "Hello from mock."}
    done = replay.terminal
    assert done["usage"] == first.terminal["usage"]
    assert done["effective_model"] == "gpt-4.1" and done["selected_model"] == "gpt-4.1"
    assert done["quota_decision"] == "allow"
    assert "quota_warnings" not in done and "downgrade_reason" not in done
    # Side-effect free.
    assert len(fresh_mock.responses_calls()) == calls_before
    assert db.quota_rows() == quota_before
    assert len(db.outbox_bodies()) == bodies_before
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2


def test_same_request_id_while_running_conflicts(api, chat, fresh_mock):
    rid = str(uuid.uuid4())
    bg = background_stream(api, chat["id"], "slow one [[slow]]", request_id=rid).wait_started()
    try:
        res = api.stream(chat["id"], "again", request_id=rid)
        assert_problem(res.problem, 409, "aborted", ctx_reason="request_id_conflict")
        st = api.get(f"/chats/{chat['id']}/turns/{rid}").json()
        assert st["state"] == "running"
        assert "assistant_message_id" not in st and "error_code" not in st
    finally:
        result = bg.finish()
    assert result.terminal_name == "done"
    assert result.text.startswith("tok0 ")
    assert api.get(f"/chats/{chat['id']}/turns/{rid}").json()["state"] == "done"


def test_parallel_turn_guard(api, chat, fresh_mock):
    done_rid = str(uuid.uuid4())
    api.turn(chat["id"], "earlier", request_id=done_rid)
    bg = background_stream(api, chat["id"], "slow [[slow]]").wait_started()
    try:
        res = api.stream(chat["id"], "second while running", request_id=str(uuid.uuid4()))
        assert_problem(res.problem, 409, "aborted", ctx_reason="turn_already_running")
        res = api.stream(chat["id"], "second without id")
        assert_problem(res.problem, 409, "aborted", ctx_reason="turn_already_running")
        # Replay is checked before the parallel-turn guard.
        replay = api.stream(chat["id"], "x", request_id=done_rid)
        assert replay.status == 200, replay.problem
        assert replay.started["is_new_turn"] is False
    finally:
        result = bg.finish()
    assert result.terminal_name == "done"
    # A new turn is accepted once the previous one is terminal.
    api.turn(chat["id"], "after")
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 6


# ------------------------------------------------------------------ provider failures
def _assert_failed_turn(api, chat, res, code):
    assert res.status == 200
    assert_order(res)
    assert res.terminal_name == "error"
    err = res.terminal
    assert err["code"] == code, err
    assert isinstance(err["message"], str)
    assert_no_provider_ids(res.raw)
    st = api.get(f"/chats/{chat['id']}/turns/{res.started['request_id']}").json()
    assert st["state"] == "error"
    assert st["error_code"] == code
    assert "assistant_message_id" not in st
    return err


def test_provider_failed_event(api, chat):
    res = api.stream(chat["id"], "boom [[error]]")
    err = _assert_failed_turn(api, chat, res, "provider_error")
    assert "file-abcdef0123456789abcd" not in err["message"]
    assert "[provider_id]" in err["message"], err


def test_provider_http_500(api, chat):
    res = api.stream(chat["id"], "boom [[500]]")
    err = _assert_failed_turn(api, chat, res, "provider_error")
    assert "file-abcdef0123456789abcd" not in err["message"]


def test_provider_http_429(api, chat):
    res = api.stream(chat["id"], "busy [[429]]")
    err = _assert_failed_turn(api, chat, res, "rate_limited")
    assert "7" in err["message"], err


def test_failed_turn_request_id_cannot_be_reused(api, chat):
    res = api.stream(chat["id"], "boom [[error]]")
    rid = res.started["request_id"]
    again = api.stream(chat["id"], "hello", request_id=rid)
    assert_problem(again.problem, 409, "aborted", ctx_reason="request_id_conflict")
    # The chat is not blocked by the failed turn.
    api.turn(chat["id"], "next")


def test_incomplete_response_is_done(api, chat):
    res = api.turn(chat["id"], "cut [[incomplete]]")
    assert "citations" not in res.names
    st = api.get(f"/chats/{chat['id']}/turns/{res.started['request_id']}").json()
    assert st["state"] == "done" and "error_code" not in st


def test_empty_completion_is_done(api, chat):
    res = api.turn(chat["id"], "say nothing [[empty]]")
    assert res.all("delta") == []
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert msgs[-1]["role"] == "assistant" and msgs[-1]["content"] == ""


# ------------------------------------------------------------------ web search
def test_web_search_tool_and_citations(api, chat, fresh_mock):
    res = api.turn(chat["id"], "news? [[web_search]]", web_search={"enabled": True})
    assert_order(res)
    tools = res.all("tool")
    assert {"phase": "start", "name": "web_search", "details": {}} in tools
    assert {"phase": "done", "name": "web_search", "details": {}} in tools
    assert res.names.index("citations") == len(res.names) - 2
    items = res.first("citations")["items"]
    assert len(items) == 1
    item = items[0]
    assert item["source"] == "web"
    assert item["url"] == "https://example.com/a"
    assert item["title"] == "Example"
    assert item["snippet"] == "Hello"
    assert item["span"] == {"start": 0, "end": 5}
    assert "attachment_id" not in item
    body = fresh_mock.responses_calls()[-1]["body"]
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"]
    assert body["metadata"]["feature"] == "web_search"
    assert "web_search" in body["instructions"]  # web_search guard appended


def test_web_search_disabled_not_sent(api, chat, fresh_mock):
    api.turn(chat["id"], "no search", web_search={"enabled": False})
    body = fresh_mock.responses_calls()[-1]["body"]
    assert not any(t.get("type") == "web_search" for t in body.get("tools") or [])


def test_web_search_calls_exceeded(api, chat, db):
    res = api.stream(chat["id"], "many [[web_search3]]", web_search={"enabled": True})
    _assert_failed_turn(api, chat, res, "web_search_calls_exceeded")
    starts = [t for t in res.all("tool") if t["phase"] == "start"]
    assert len(starts) <= 3
    rid = res.started["request_id"]
    usage = wait_until(lambda: [b["json"] for b in db.outbox_bodies("mini_chat.usage_event.v1")
                                if b["json"]["request_id"] == rid], timeout=10, desc="usage event")
    # DESIGN §4 Web Search Quota Enforcement: billing_outcome=failed, settlement_method=estimated.
    assert usage[0]["billing_outcome"] == "failed" and usage[0]["settlement_method"] == "estimated", usage[0]


# ------------------------------------------------------------------ keepalive
def test_ping_before_first_content(api, chat, fresh_mock):
    """sse_ping_interval_seconds=5 in the default variant; the mock stalls 6 s before deltas."""
    res = api.turn(chat["id"], "think first [[stall]]")
    assert_order(res)
    assert "ping" in res.names, res.names
    assert res.names[1] == "ping"
    assert res.first("ping") == {}
    first_content = min(i for i, n in enumerate(res.names) if n in ("delta", "tool"))
    assert all(n != "ping" for n in res.names[first_content:])


# ------------------------------------------------------------------ cancellation
def test_client_disconnect_cancels_turn(api, chat, db):
    rid = str(uuid.uuid4())
    session = requests.Session()
    session.headers.update(api.s.headers)
    seen = []

    def stop(name, data):
        seen.append(name)
        return name == "delta" and len([n for n in seen if n == "delta"]) >= 2

    res = sse_request(api, "POST", f"/chats/{chat['id']}/messages:stream",
                      {"content": "long [[slow]]", "request_id": rid}, stop=stop, session=session)
    session.close()
    assert res.names[0] == "stream_started"
    assert res.terminal_name == "delta"
    partial = res.text

    st = wait_until(lambda: (lambda s: s if s["state"] != "running" else None)(
        api.get(f"/chats/{chat['id']}/turns/{rid}").json()), timeout=20, desc="turn leaves running")
    assert st["state"] == "cancelled", st
    assert "error_code" not in st
    assert st.get("assistant_message_id") == res.started["message_id"], st
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert msgs[-1]["role"] == "assistant"
    assert msgs[-1]["id"] == res.started["message_id"]
    assert msgs[-1]["content"].startswith(partial)
    assert len(msgs[-1]["content"]) < len("".join(f"tok{i} " for i in range(10)))
    # The cancelled request_id cannot be reused; a new turn is accepted.
    again = api.stream(chat["id"], "again", request_id=rid)
    assert_problem(again.problem, 409, "aborted", ctx_reason="request_id_conflict")
    api.turn(chat["id"], "after cancel")
    turn = db.turn(chat["id"], rid)
    assert turn["state"] == "cancelled"
    # ABORTED -> estimated settlement published once, reserve released.
    usage = wait_until(lambda: [b["json"] for b in db.outbox_bodies("mini_chat.usage_event.v1")
                                if b["json"]["request_id"] == rid], timeout=10, desc="usage event")
    assert len(usage) == 1
    assert usage[0]["billing_outcome"] == "aborted" and usage[0]["settlement_method"] == "estimated", usage[0]
    wait_until(lambda: all(r["reserved_credits_micro"] == 0 for r in db.quota_rows()), timeout=10,
               desc="reserve released")


def test_no_new_stream_when_chat_deleted(api, chat):
    api.delete(f"/chats/{chat['id']}")
    res = api.stream(chat["id"], "hi")
    assert_problem(res.problem, 404, "not_found", resource_type=CHAT_RT)


def test_input_too_long(api, chat, fresh_mock):
    res = api.stream(chat["id"], "a" * 500_000)  # > max_input_tokens (100000) at ~4 bytes/token
    assert_problem(res.problem, 400, "out_of_range", reason="INPUT_TOO_LONG")
    assert fresh_mock.responses_calls() == []
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 0


def test_request_id_is_scoped_per_chat(api, chat):
    other = api.create_chat()
    rid = str(uuid.uuid4())
    api.turn(chat["id"], "a", request_id=rid)
    res = api.turn(other["id"], "b", request_id=rid)
    assert res.started["is_new_turn"] is True
