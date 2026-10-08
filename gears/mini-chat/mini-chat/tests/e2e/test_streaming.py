"""Send message streaming, SSE contract, provider request shape, persistence."""

from __future__ import annotations

import json
import threading
import time
import uuid

import httpx

import prov
from harness import TENANT_A, USER_A, from_blob, problem_reason, ub


def test_send_streams_and_persists(env):
    c = env.a
    chat = c.create_chat(title="s", model="gpt-4.1-mini")
    rid = str(uuid.uuid4())
    env.mock.script([prov.text_reply("Hello world, streamed.", usage={"input_tokens": 321, "output_tokens": 45})])
    r = c.send(chat["id"], "Say hello", request_id=rid)
    assert r.status == 200
    assert r.headers["content-type"].startswith("text/event-stream")
    assert r.headers["cache-control"] == "no-cache"
    assert r.names[0] == "stream_started"
    assert r.names[-1] == "done"
    assert set(r.names[1:-1]) == {"delta"}
    st = r.started
    assert st["request_id"] == rid and st["is_new_turn"] is True
    assert "thread_summary_applied" not in st
    assert r.text == "Hello world, streamed."
    for e in r.all("delta"):
        assert e.data["type"] == "text"
    done = r.done
    assert done["usage"] == {"input_tokens": 321, "output_tokens": 45}
    assert done["effective_model"] == "gpt-4.1-mini" and done["selected_model"] == "gpt-4.1-mini"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    raw = json.dumps([e.data for e in r.events])
    for leak in ("resp_", "prov-gpt", "mock", "credits"):
        assert leak not in raw, leak

    msgs = c.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    user, asst = msgs
    assert user["request_id"] == asst["request_id"] == rid
    assert asst["id"] == st["message_id"]
    assert asst["content"] == "Hello world, streamed."
    assert asst["model"] == "gpt-4.1-mini"
    assert asst["input_tokens"] == 321 and asst["output_tokens"] == 45
    assert "model" not in user and "input_tokens" not in user

    turn = c.turn(chat["id"], rid).json()
    assert turn["state"] == "done" and turn["assistant_message_id"] == st["message_id"]
    assert "error_code" not in turn
    row = env.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert row["state"] == "completed" and row["error_code"] is None
    assert row["provider_response_id"] == "resp_abc123def456"
    assert row["effective_model"] == "gpt-4.1-mini"
    assert row["reserve_tokens"] > 0 and row["reserved_credits_micro"] > 0
    assert row["policy_version_applied"] == 1
    assert row["completed_at"] is not None
    m = env.server.query("SELECT * FROM messages WHERE id = ?", (ub(asst["id"]),))[0]
    assert m["provider_response_id"] == "resp_abc123def456"


def test_provider_request_shape(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    c.send(chat["id"], "first question")
    c.send(chat["id"], "second question")
    reqs = env.mock.chat_requests()
    assert len(reqs) == 2
    req = reqs[1]
    assert req["path"] == "/v1/responses" and req["method"] == "POST"
    body = req["json"]
    assert body["model"] == "prov-gpt-4.1-mini"
    assert body["stream"] is True
    assert body["instructions"].startswith("SYSTEM PROMPT gpt-4.1-mini")
    assert body["max_output_tokens"] == 4096
    assert body["temperature"] == 0.5
    assert body["user"] == TENANT_A.replace("-", "") + USER_A.replace("-", "")
    assert len(body["user"]) == 64
    md = body["metadata"]
    assert md["tenant_id"] == TENANT_A and md["user_id"] == USER_A and md["chat_id"] == chat["id"]
    assert md["request_type"] == "chat" and md["feature"] == "none"
    assert "tools" not in body or body["tools"] == []
    inp = body["input"]
    assert inp[-1]["role"] == "user"
    assert inp[-1]["content"] == [{"type": "input_text", "text": "second question"}]
    # history: previous turn's user and assistant messages, in order
    assert inp[0]["role"] == "user" and inp[1]["role"] == "assistant"
    assert inp[1]["content"] == "Echo: first question"


def test_validation_before_provider_call(env):
    c = env.a
    chat = c.create_chat()
    cases = [
        ({"content": "   "}, 400, "EMPTY_CONTENT"),
        ({"content": "x", "attachment_ids": [str(uuid.uuid4())]}, 400, "invalid_attachment"),
        ({"content": "x", "attachment_ids": [str(uuid.uuid4()) for _ in range(60)]}, 400, "invalid_attachment"),
    ]
    dup = str(uuid.uuid4())
    cases.append(({"content": "x", "attachment_ids": [dup, dup]}, 400, "invalid_attachment"))
    for body, status, reason in cases:
        r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", body)
        assert r.status == status, (body, r.body)
        assert problem_reason(r.body) == reason
        assert r.headers["content-type"].startswith("application/problem+json")
    r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"nope": 1})
    assert r.status == 422
    r = c.stream_raw("POST", f"/chats/{uuid.uuid4()}/messages:stream", {"content": "hi"})
    assert r.status == 404
    assert env.mock.chat_requests() == []
    # no turn or message was left behind
    assert c.get(f"/chats/{chat['id']}").json()["message_count"] == 0
    assert env.server.query("SELECT count(*) AS n FROM chat_turns WHERE chat_id = ?", (ub(chat["id"]),))[0]["n"] == 0


def test_input_too_long_and_context_budget(env):
    c = env.a
    chat = c.create_chat(model="tiny-ctx")
    # tiny-ctx: max_input_tokens 2500, 4 bytes/token -> 12 KB is far above
    r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "x" * 12000})
    assert r.status == 400
    assert r.body["type"].endswith("out_of_range.v1~")
    assert problem_reason(r.body) == "INPUT_TOO_LONG"
    # system prompt + message under max_input_tokens but over the assembled budget
    # (budget = min(2500, 3000-500) - tool surcharges - overhead)
    # 9900 bytes: ceil(9900/4)+10 = 2485 <= max_input_tokens (2500), but the
    # system prompt (16) + message (2485) exceed the budget 2500 - 10 overhead
    r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "y" * 9900})
    assert r.status == 400
    assert r.body["type"].endswith("out_of_range.v1~")
    assert problem_reason(r.body) == "CONTEXT_BUDGET_EXCEEDED"
    assert env.mock.chat_requests() == []


def test_full_event_contract_tools_citations_and_ping(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    text = "See example and docs."
    ann = [{"type": "url_citation", "url": "https://example.com/a", "title": "Example", "start_index": 4, "end_index": 11}]
    env.mock.script([
        prov.sse(
            prov.created(),
            prov.sleep(6500),  # longer than sse_ping_interval_seconds (5)
            prov.ev("response.web_search_call.searching", item_id="ws_1"),
            prov.ev("response.web_search_call.completed", item_id="ws_1"),
            prov.delta("See example "),
            prov.delta("and docs."),
            prov.completed(text, annotations=ann),
        )
    ])
    r = c.send(chat["id"], "search please", web_search={"enabled": True})
    names = r.names
    assert names[0] == "stream_started"
    assert "ping" in names
    first_content = min(names.index("tool"), names.index("delta"))
    assert all(n == "ping" for n in names[1:first_content])
    assert "ping" not in names[first_content:]
    tools = r.all("tool")
    assert [(t.data["phase"], t.data["name"]) for t in tools] == [("start", "web_search"), ("done", "web_search")]
    assert names[-2] == "citations" and names[-1] == "done"
    items = r.first("citations").data["items"]
    assert items == [{"source": "web", "title": "Example", "url": "https://example.com/a", "snippet": "example", "span": {"start": 4, "end": 11}}]
    assert r.all("ping")[0].data == {}
    body = env.mock.chat_requests()[0]["json"]
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"] or any(t["type"] == "web_search" for t in body["tools"])
    assert "web_search" in body["instructions"]
    assert body["metadata"]["feature"] == "web_search"
    assert body["max_tool_calls"] == 2


def test_error_event_terminal_and_sanitized(env):
    c = env.a
    chat = c.create_chat()
    secret = "boom sk-abcdefghijklmnop at https://internal.example.com/x with resp_ABCDEFGH12345 and file-abcdefghijklmnop12"
    env.mock.script([prov.sse(prov.created(), prov.delta("partial "), prov.failed(secret))])
    r = c.send(chat["id"], "fail please")
    assert r.names[-1] == "error"
    assert r.names.count("error") == 1
    err = r.first("error").data
    assert err["code"] == "provider_error"
    for leak in ("sk-abcdef", "internal.example.com", "resp_ABCDEFGH", "file-abcdefghij"):
        assert leak not in err["message"]
    rid = r.started["request_id"]
    t = c.turn(chat["id"], rid).json()
    assert t["state"] == "error" and t["error_code"] == "provider_error"
    assert "assistant_message_id" not in t
    row = env.server.query("SELECT error_detail FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert "sk-abcdef" not in (row["error_detail"] or "")


def test_http_errors_map_to_stream_codes(env):
    c = env.a
    chat = c.create_chat()
    env.mock.script([{"kind": "error", "status": 429, "body": {"error": {"message": "slow down"}}, "headers": {"Retry-After": "7"}}])
    r = c.send(chat["id"], "a")
    err = r.first("error").data
    assert err["code"] == "rate_limited" and "7" in err["message"]
    env.mock.script([{"kind": "error", "status": 500, "body": {"error": {"message": "upstream exploded at https://x.y/z"}}}])
    r = c.send(chat["id"], "b")
    err = r.first("error").data
    assert err["code"] == "provider_error"
    assert "https://x.y" not in err["message"]
    # stream ends without a terminal provider event
    env.mock.script([prov.sse(prov.created(), prov.delta("cut"))])
    r = c.send(chat["id"], "c")
    assert r.first("error").data["code"] == "provider_error"


def test_incomplete_is_completed(env):
    c = env.a
    chat = c.create_chat()
    env.mock.script([prov.sse(prov.created(), prov.delta("trunc"), prov.completed("trunc", incomplete=True))])
    r = c.send(chat["id"], "long answer")
    assert r.names[-1] == "done"
    rid = r.started["request_id"]
    assert c.turn(chat["id"], rid).json()["state"] == "done"
    row = env.server.query("SELECT state, error_code FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert row == {"state": "completed", "error_code": None}


def test_streaming_is_not_buffered(env):
    c = env.a
    chat = c.create_chat()
    env.mock.script([prov.sse(prov.created(), prov.delta("early "), prov.sleep(3000), prov.delta("late"), prov.completed("early late"))])
    t0 = time.time()
    arrivals = {}
    with c.http.stream("POST", c.url(f"/chats/{chat['id']}/messages:stream"), json={"content": "x"}) as resp:
        from harness import parse_sse

        for e in parse_sse(resp.iter_lines()):
            if e.event == "delta":
                arrivals.setdefault(e.data["content"], time.time() - t0)
            if e.event in ("done", "error"):
                break
    assert arrivals["early "] < 2.0, arrivals
    assert arrivals["late"] >= 2.5, arrivals


def test_tool_events_details_and_counts(env):
    c = env.a
    chat = c.create_chat()
    env.mock.script([
        prov.sse(
            prov.created(),
            prov.ev("response.code_interpreter_call.in_progress"),
            {"event": "response.output_item.done", "data": {"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "42"}]}}},
            prov.delta("ok"),
            prov.completed("ok"),
        )
    ])
    r = c.send(chat["id"], "compute")
    tools = r.all("tool")
    assert tools[0].data == {"phase": "start", "name": "code_interpreter", "details": {}}
    assert tools[1].data == {"phase": "done", "name": "code_interpreter", "details": {"output": "42"}}
    rid = r.started["request_id"]
    row = env.server.query("SELECT code_interpreter_completed_count FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert row["code_interpreter_completed_count"] == 1


def test_unauthenticated(env):
    r = httpx.get(env.server.base + "/mini-chat/v1/chats")
    assert r.status_code == 401
    r = httpx.get(env.server.base + "/mini-chat/v1/chats", headers={"Authorization": "Bearer nope"})
    assert r.status_code == 401


def test_provider_timeout(make_env):
    e = make_env({"gears": {"oagw": {"config": {"proxy_timeout_secs": 2}}}})
    c = e.a
    chat = c.create_chat()
    e.mock.script([{"kind": "hang"}])
    r = c.send(chat["id"], "no answer")
    assert r.names[0] == "stream_started" and r.names[-1] == "error"
    assert r.first("error").data["code"] == "provider_timeout"
    rid = r.started["request_id"]
    assert c.turn(chat["id"], rid).json()["error_code"] == "provider_timeout"
