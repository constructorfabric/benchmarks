"""Streaming: SSE contract, persistence, provider requests, errors,
cancellation, idempotency / replay and the parallel-turn guard."""

import time
import uuid

import httpx
import pytest

from harness import parse_sse, ubytes, wait_for


def names(events):
    return [n for n, _ in events]


def turn_row(stack, chat_id, request_id):
    rows = stack.query(
        "select * from chat_turns where chat_id = ? and request_id = ?",
        (ubytes(chat_id), ubytes(request_id)),
    )
    return rows[0] if rows else None


def quota_rows(stack, user="00000000-0000-0000-0000-0000000000a1"):
    return stack.query("select * from quota_usage where user_id = ?", (ubytes(user),))


def text_events(chunks, usage=None, rid="resp_abc"):
    ev = [{"type": "response.created", "response": {"id": rid}}]
    for c in chunks:
        ev.append({"type": "response.output_text.delta", "item_id": "m1", "content_index": 0, "delta": c})
    ev.append({"type": "response.completed", "response": {"id": rid, "usage": usage or {"input_tokens": 20, "output_tokens": 10}}})
    return ev


def test_basic_stream_contract_and_persistence(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.mock_script([{"events": text_events(["Hel", "lo"], {"input_tokens": 1000, "output_tokens": 100})}])
    r = client.stream(c["id"], "hello there", request_id=rid)
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/event-stream")
    assert "no-cache" in r.headers.get("cache-control", "")
    ev = parse_sse(r.text)
    assert names(ev)[0] == "stream_started"
    assert names(ev)[-1] == "done"
    assert names(ev).count("done") == 1 and "error" not in names(ev)
    started = ev[0][1]
    assert started["request_id"] == rid and started["is_new_turn"] is True
    assert "thread_summary_applied" not in started
    deltas = [p for n, p in ev if n == "delta"]
    assert "".join(d["content"] for d in deltas) == "Hello"
    assert all(d["type"] == "text" for d in deltas)
    done = ev[-1][1]
    assert done["usage"] == {"input_tokens": 1000, "output_tokens": 100}
    assert done["effective_model"] == "gpt-standard" and done["selected_model"] == "gpt-standard"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    assert "message_id" not in done and "request_id" not in done
    assert {(w["tier"], w["period"]) for w in done["quota_warnings"]} >= {("total", "daily"), ("total", "monthly")}

    msgs = client.messages(c["id"])["items"]
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[0]["request_id"] == msgs[1]["request_id"] == rid
    assert msgs[1]["id"] == started["message_id"]
    assert msgs[1]["content"] == "Hello"
    assert msgs[1]["model"] == "gpt-standard"
    assert msgs[1]["input_tokens"] == 1000 and msgs[1]["output_tokens"] == 100
    assert "model" not in msgs[0] and "input_tokens" not in msgs[0]

    t = turn_row(stack, c["id"], rid)
    assert t["state"] == "completed" and t["error_code"] is None
    assert uuid.UUID(bytes=t["assistant_message_id"]) == uuid.UUID(started["message_id"])
    assert t["provider_response_id"] == "resp_abc"
    assert t["effective_model"] == "gpt-standard"
    assert t["reserve_tokens"] > 0 and t["reserved_credits_micro"] > 0
    assert t["max_output_tokens_applied"] == 4096
    assert t["minimal_generation_floor_applied"] == 50
    assert t["policy_version_applied"] == 1
    assert t["completed_at"] is not None
    assert t["requester_type"] == "user"

    r = client.req("GET", f"/v1/chats/{c['id']}/turns/{rid}")
    assert r.status_code == 200
    st = r.json()
    assert st["state"] == "done" and st["assistant_message_id"] == started["message_id"]
    assert "error_code" not in st


def test_settlement_and_usage_event(stack, mock, logs):
    from harness import Client

    cl = Client(stack, "token-b1")
    c = cl.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    before = {(q["period_type"], q["bucket"]): q for q in quota_rows(stack, "00000000-0000-0000-0000-0000000000b1")}
    mock.mock_script([{"events": text_events(["ok"], {"input_tokens": 1000, "output_tokens": 100})}])
    cl.send(c["id"], "hello", request_id=rid)
    after = {(q["period_type"], q["bucket"]): q for q in quota_rows(stack, "00000000-0000-0000-0000-0000000000b1")}
    # credits: in 1000 * 1_000_000 / 1e6 + out 100 * 3_000_000 / 1e6 = 1000 + 300
    for period in ("daily", "monthly"):
        a = after[(period, "total")]
        b = before.get((period, "total"), {"spent_credits_micro": 0, "calls": 0, "input_tokens": 0, "output_tokens": 0})
        assert a["spent_credits_micro"] - b["spent_credits_micro"] == 1300
        assert a["reserved_credits_micro"] == 0
        assert a["calls"] - b["calls"] == 1
        assert a["input_tokens"] - b["input_tokens"] == 1000
        assert a["output_tokens"] - b["output_tokens"] == 100
    assert ("daily", "tier:premium") not in after  # standard turn: total only
    t = turn_row(stack, c["id"], rid)
    ev = wait_for(lambda: [e for e in logs("usage") if e["request_id"] == rid])
    assert len(ev) == 1
    e = ev[0]
    assert e["billing_outcome"] == "completed" and e["settlement_method"] == "actual"
    assert e["terminal_state"] == "completed"
    assert e["actual_credits_micro"] == 1300
    assert e["usage"]["input_tokens"] == 1000
    assert e["dedupe_key"] == "/".join(
        [uuid.UUID(c2).hex for c2 in ("00000000-0000-0000-0000-00000000000b",)]
        + [uuid.UUID(bytes=t["id"]).hex, uuid.UUID(rid).hex]
    )
    assert e["requester_type"] == "user" and e["policy_version_applied"] == 1
    assert e["selected_model"] == e["effective_model"] == "gpt-standard"
    audit = wait_for(lambda: [a for a in logs("audit") if a["request_id"] == rid])
    assert audit and audit[0]["event_type"] == "turn_completed"
    assert audit[0]["policy_decisions"]["quota"]["decision"] == "allow"


def test_server_generated_request_id(client, mock):
    c = client.create_chat(model="gpt-standard")
    ev = client.send(c["id"], "no request id")
    rid = ev[0][1]["request_id"]
    assert uuid.UUID(rid).version == 4
    msgs = client.messages(c["id"])["items"]
    assert {m["request_id"] for m in msgs} == {rid}


def test_provider_request_shape(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    client.send(c["id"], "first question")
    mock.mock_reset()
    client.send(c["id"], "second question")
    reqs = stack.mock_requests("/v1/responses")
    assert len(reqs) == 1
    body = reqs[0]["json"]
    assert body["model"] == "gpt-standard"
    assert body["stream"] is True
    assert body["instructions"] == "You are gpt-standard."
    assert body["max_output_tokens"] == 4096
    assert body["user"] == uuid.UUID("00000000-0000-0000-0000-00000000000a").hex + uuid.UUID(
        "00000000-0000-0000-0000-0000000000a1"
    ).hex
    assert body["metadata"]["request_type"] == "chat" and body["metadata"]["feature"] == "none"
    assert body["metadata"]["chat_id"] == c["id"]
    assert "tools" not in body
    texts = [(m["role"], m["content"][0]["text"]) for m in body["input"]]
    assert texts == [
        ("user", "first question"),
        ("assistant", "Hello from mock"),
        ("user", "second question"),
    ]


def test_tool_events_citations_and_web_search_accounting(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    events = [
        {"type": "response.created", "response": {"id": "resp_t"}},
        {"type": "response.output_item.added", "item": {"type": "web_search_call", "id": "ws_1"}},
        {"type": "response.web_search_call.in_progress", "item_id": "ws_1"},
        {"type": "response.web_search_call.searching", "item_id": "ws_1"},
        {"type": "response.web_search_call.completed", "item_id": "ws_1"},
        {"type": "response.output_text.delta", "item_id": "m1", "content_index": 0, "delta": "See example site"},
        {
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "content": [
                    {
                        "type": "output_text",
                        "text": "See example site",
                        "annotations": [
                            {"type": "url_citation", "url": "https://example.com/a", "title": "Example", "start_index": 4, "end_index": 11}
                        ],
                    }
                ],
            },
        },
        {"type": "response.completed", "response": {"id": "resp_t", "usage": {"input_tokens": 5, "output_tokens": 5}}},
    ]
    mock.mock_script([{"events": events}])
    before = {(q["period_type"], q["bucket"]): q["web_search_calls"] for q in quota_rows(stack)}
    ev = client.send(c["id"], "search the web", web_search={"enabled": True})
    n = names(ev)
    assert n[0] == "stream_started" and n[-1] == "done"
    tools = [p for nm, p in ev if nm == "tool"]
    assert tools[0] == {"phase": "start", "name": "web_search", "details": {}}
    assert tools[1]["phase"] == "done" and tools[1]["name"] == "web_search"
    assert len(tools) == 2
    assert n.index("citations") == len(n) - 2
    cit = [p for nm, p in ev if nm == "citations"][0]["items"][0]
    assert cit["source"] == "web" and cit["url"] == "https://example.com/a" and cit["title"] == "Example"
    assert cit["snippet"] == "example" and cit["span"] == {"start": 4, "end": 11}
    body = stack.mock_requests("/v1/responses")[0]["json"]
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"]
    assert body["max_tool_calls"] == 2
    assert "Use web_search only if" in body["instructions"]
    assert body["metadata"]["feature"] == "web_search"
    rid = ev[0][1]["request_id"]
    t = turn_row(stack, c["id"], rid)
    assert t["web_search_enabled"] == 1 and t["web_search_completed_count"] == 1
    after = {(q["period_type"], q["bucket"]): q["web_search_calls"] for q in quota_rows(stack)}
    assert after[("daily", "total")] - before.get(("daily", "total"), 0) == 1


def test_web_search_not_sent_for_unsupported_model(stack, client, mock):
    c = client.create_chat(model="gpt-novision")
    ev = client.send(c["id"], "x", web_search={"enabled": True})
    assert names(ev)[-1] == "done"
    body = stack.mock_requests("/v1/responses")[0]["json"]
    assert "tools" not in body and "web_search" not in body["instructions"]
    t = turn_row(stack, c["id"], ev[0][1]["request_id"])
    assert t["web_search_enabled"] == 1


def test_web_search_call_limit(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    events = [{"type": "response.created", "response": {"id": "resp_l"}}]
    for i in range(3):
        events.append({"type": "response.web_search_call.in_progress", "item_id": f"ws_{i}"})
    events.append({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})
    mock.mock_script([{"events": events}])
    ev = client.send(c["id"], "x", web_search={"enabled": True})
    assert names(ev)[-1] == "error"
    assert ev[-1][1]["code"] == "web_search_calls_exceeded"
    rid = ev[0][1]["request_id"]
    t = turn_row(stack, c["id"], rid)
    assert t["state"] == "failed" and t["error_code"] == "web_search_calls_exceeded"
    u = wait_for(lambda: [e for e in logs("usage") if e["request_id"] == rid])
    assert u[0]["billing_outcome"] == "failed" and u[0]["settlement_method"] == "estimated"
    assert u[0]["usage"] is None and u[0]["actual_credits_micro"] > 0
    st = client.req("GET", f"/v1/chats/{c['id']}/turns/{rid}").json()
    assert st["state"] == "error" and st["error_code"] == "web_search_calls_exceeded"


@pytest.mark.parametrize(
    "script,code",
    [
        (
            [{"type": "response.created", "response": {"id": "resp_f"}},
             {"type": "response.failed", "response": {"id": "resp_f", "error": {"code": "server_error",
              "message": "boom resp_0123456789abcdef file-abcdefghijklmnop at https://internal.example/x key sk-ABCDEFGHIJKLMNOP"}}}],
            "provider_error",
        ),
        ([{"type": "error", "code": "bad", "message": "flat error vs_abcdefghijklmnopq"}], "provider_error"),
    ],
)
def test_provider_stream_errors_sanitized(stack, client, mock, script, code):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"events": script}])
    ev = client.send(c["id"], "x")
    assert names(ev)[0] == "stream_started" and names(ev)[-1] == "error"
    err = ev[-1][1]
    assert err["code"] == code
    msg = err["message"]
    for leak in ("resp_0123", "file-abcdef", "https://", "sk-ABC", "vs_abcdef"):
        assert leak not in msg
    t = turn_row(stack, c["id"], ev[0][1]["request_id"])
    assert t["state"] == "failed" and t["error_code"] == code
    assert t["assistant_message_id"] is None
    assert len(client.messages(c["id"])["items"]) == 1


def test_provider_http_errors(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"status": 429, "body": {"error": {"message": "slow down"}}, "headers": {"Retry-After": "7"}}])
    ev = client.send(c["id"], "x")
    assert ev[-1][1]["code"] == "rate_limited" and "7" in ev[-1][1]["message"]
    mock.mock_script([{"status": 500, "body": {"error": {"message": "internal resp_0123456789abcdef"}}}])
    ev = client.send(c["id"], "y")
    assert ev[-1][1]["code"] == "provider_error"
    assert "resp_0123456789abcdef" not in ev[-1][1]["message"]
    # stream without a terminal event
    mock.mock_script([{"events": [{"type": "response.created", "response": {"id": "r"}},
                                  {"type": "response.output_text.delta", "delta": "partial"}]}])
    ev = client.send(c["id"], "z")
    assert names(ev)[-1] == "error" and ev[-1][1]["code"] == "provider_error"


def test_incomplete_is_completed(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"events": [
        {"type": "response.output_text.delta", "delta": "trunc"},
        {"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"},
                                                     "usage": {"input_tokens": 3, "output_tokens": 4}}},
    ]}])
    ev = client.send(c["id"], "x")
    assert names(ev)[-1] == "done" and "citations" not in names(ev)
    t = turn_row(stack, c["id"], ev[0][1]["request_id"])
    assert t["state"] == "completed" and t["error_code"] is None


def test_empty_completion_persists_empty_message(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"events": [{"type": "response.completed", "response": {}}]}])
    ev = client.send(c["id"], "x")
    assert names(ev)[-1] == "done"
    assert ev[-1][1]["usage"] == {"input_tokens": 0, "output_tokens": 0}
    msgs = client.messages(c["id"])["items"]
    assert msgs[-1]["role"] == "assistant" and msgs[-1]["content"] == ""


def test_ping_before_content(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    ev_script = text_events(["late"])
    ev_script[1]["_delay_ms"] = 6500
    mock.mock_script([{"events": ev_script}])
    ev = client.send(c["id"], "x")
    n = names(ev)
    assert n[0] == "stream_started"
    assert "ping" in n
    first_content = n.index("delta")
    assert all(x != "ping" for x in n[first_content:])
    assert all(i < first_content for i, x in enumerate(n) if x == "ping")
    assert [p for x, p in ev if x == "ping"][0] == {}


def _open_and_drop(client, chat_id, content, after_events=1, **body):
    payload = {"content": content}
    payload.update(body)
    got = []
    with client.c.stream("POST", f"/v1/chats/{chat_id}/messages:stream", json=payload, headers=client.h) as r:
        assert r.status_code == 200
        buf = ""
        it = r.iter_text()
        for chunk in it:
            buf += chunk
            got = parse_sse(buf)
            if len([n for n, _ in got if n not in ("ping",)]) >= after_events:
                break
        # make sure the provider request is in flight before disconnecting
        assert wait_for(lambda: client.s.mock_requests("/v1/responses"), timeout=10)
    return got


def test_disconnect_cancels_turn(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"hang": True}])
    got = _open_and_drop(client, c["id"], "wait forever")
    rid = got[0][1]["request_id"]
    t = wait_for(lambda: (r := turn_row(stack, c["id"], rid)) and r["state"] != "running" and r, timeout=20)
    assert t["state"] == "cancelled"
    assert t["assistant_message_id"] is None
    u = wait_for(lambda: [e for e in logs("usage") if e["request_id"] == rid])
    assert u[0]["billing_outcome"] == "aborted" and u[0]["settlement_method"] == "estimated"
    assert u[0]["terminal_state"] == "cancelled" and u[0]["actual_credits_micro"] > 0
    st = client.req("GET", f"/v1/chats/{c['id']}/turns/{rid}").json()
    assert st["state"] == "cancelled" and "assistant_message_id" not in st
    # the chat accepts a new turn afterwards
    ev = client.send(c["id"], "again")
    assert names(ev)[-1] == "done", ev


def test_disconnect_with_partial_text(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    events = [{"type": "response.output_text.delta", "delta": "partial answer"},
              {"type": "response.output_text.delta", "delta": " more", "_delay_ms": 60000}]
    mock.mock_script([{"events": events}])
    got = _open_and_drop(client, c["id"], "x", after_events=2)
    rid = got[0][1]["request_id"]
    t = wait_for(lambda: (r := turn_row(stack, c["id"], rid)) and r["state"] != "running" and r, timeout=20)
    assert t["state"] == "cancelled"
    assert t["assistant_message_id"] is not None
    msgs = client.messages(c["id"])["items"]
    assert msgs[-1]["role"] == "assistant" and msgs[-1]["content"] == "partial answer"
    st = client.req("GET", f"/v1/chats/{c['id']}/turns/{rid}").json()
    assert st["assistant_message_id"] == msgs[-1]["id"]


def test_replay_is_side_effect_free(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.mock_script([{"events": text_events(["full ", "answer"], {"input_tokens": 50, "output_tokens": 9})}])
    first = client.send(c["id"], "q", request_id=rid)
    quota_before = quota_rows(stack)
    calls_before = len(stack.mock_requests("/v1/responses"))
    usage_before = len(wait_for(lambda: [e for e in logs("usage") if e["request_id"] == rid]))
    assert usage_before == 1
    ev = client.send(c["id"], "different content is ignored", request_id=rid)
    assert names(ev) == ["stream_started", "delta", "done"]
    assert ev[0][1]["is_new_turn"] is False and ev[0][1]["request_id"] == rid
    assert ev[0][1]["message_id"] == first[0][1]["message_id"]
    assert ev[1][1] == {"type": "text", "content": "full answer"}
    assert ev[2][1]["usage"] == {"input_tokens": 50, "output_tokens": 9}
    assert ev[2][1]["quota_decision"] == "allow" and "quota_warnings" not in ev[2][1]
    time.sleep(1)
    assert len(stack.mock_requests("/v1/responses")) == calls_before
    assert quota_rows(stack) == quota_before
    assert len([e for e in logs("usage") if e["request_id"] == rid]) == usage_before
    assert len(client.messages(c["id"])["items"]) == 2


def test_request_id_conflicts(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.mock_script([{"status": 500, "body": {"error": {"message": "x"}}}])
    client.send(c["id"], "q", request_id=rid)
    r = client.stream(c["id"], "q", request_id=rid)
    assert r.status_code == 409
    prob = r.json()
    assert prob["context"]["reason"] == "request_id_conflict"
    assert rid not in prob["detail"]


def test_parallel_turn_guard_and_replay_priority(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    done_rid = str(uuid.uuid4())
    client.send(c["id"], "first", request_id=done_rid)
    mock.mock_script([{"hang": True}])
    running_rid = str(uuid.uuid4())
    with client.c.stream("POST", f"/v1/chats/{c['id']}/messages:stream",
                         json={"content": "slow", "request_id": running_rid}, headers=client.h) as resp:
        assert resp.status_code == 200
        it = resp.iter_text()
        next(it)
        r = client.stream(c["id"], "parallel")
        assert r.status_code == 409 and r.json()["context"]["reason"] == "turn_already_running"
        r = client.stream(c["id"], "same id", request_id=running_rid)
        assert r.status_code == 409 and r.json()["context"]["reason"] == "request_id_conflict"
        # replay of a completed turn wins over the running guard
        r = client.stream(c["id"], "replay", request_id=done_rid)
        assert r.status_code == 200
        assert names(parse_sse(r.text)) == ["stream_started", "delta", "done"]
        st = client.req("GET", f"/v1/chats/{c['id']}/turns/{running_rid}").json()
        assert st["state"] == "running"
    wait_for(lambda: turn_row(stack, c["id"], running_rid)["state"] != "running", timeout=20)
    ev = client.send(c["id"], "after")
    assert names(ev)[-1] == "done"


def test_preflight_validation(stack, client, mock):
    c = client.create_chat(model="gpt-standard")

    def reason(r):
        return [v.get("reason") for v in r.json()["context"].get("field_violations", [])]

    r = client.stream(c["id"], "   ")
    assert r.status_code == 400 and "EMPTY_CONTENT" in reason(r)
    a = str(uuid.uuid4())
    r = client.stream(c["id"], "x", attachment_ids=[a, a])
    assert r.status_code == 400 and "invalid_attachment" in reason(r)
    r = client.stream(c["id"], "x", attachment_ids=[str(uuid.uuid4())])
    assert r.status_code == 400 and "invalid_attachment" in reason(r)
    r = client.stream(c["id"], "x", attachment_ids=[str(uuid.uuid4()) for _ in range(60)])
    assert r.status_code == 400 and "invalid_attachment" in reason(r)
    r = client.stream(c["id"], "x", attachment_ids=["nope"])
    assert r.status_code == 422
    r = client.req("POST", f"/v1/chats/{c['id']}/messages:stream", json={})
    assert r.status_code == 422
    r = client.req("POST", f"/v1/chats/{c['id']}/messages:stream", content=b"{x", headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    # nothing persisted, no provider call, no reserve left behind
    assert client.messages(c["id"])["items"] == []
    assert stack.query("select * from chat_turns where chat_id = ?", (ubytes(c["id"]),)) == []
    assert stack.mock_requests("/v1/responses") == []
    assert all(q["reserved_credits_micro"] == 0 for q in quota_rows(stack))


def test_input_too_long_and_context_budget(stack, client, mock):
    c = client.create_chat(model="gpt-tiny")
    r = client.stream(c["id"], "x" * 8000)
    assert r.status_code == 400
    p = r.json()
    assert "out_of_range" in p["type"]
    assert [v["reason"] for v in p["context"]["field_violations"]] == ["INPUT_TOO_LONG"]
    r = client.stream(c["id"], "x" * 5000)
    assert r.status_code == 400
    assert [v["reason"] for v in r.json()["context"]["field_violations"]] == ["CONTEXT_BUDGET_EXCEEDED"]
    assert stack.mock_requests("/v1/responses") == []


def test_new_turn_after_terminal_states(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"status": 500, "body": {}}])
    assert names(client.send(c["id"], "a"))[-1] == "error"
    assert names(client.send(c["id"], "b"))[-1] == "done"
    assert len(stack.query("select * from chat_turns where chat_id = ?", (ubytes(c["id"]),))) == 2


def test_turn_status_404s(client, client_a2, mock):
    c = client.create_chat(model="gpt-standard")
    ev = client.send(c["id"], "x")
    rid = ev[0][1]["request_id"]
    assert client.req("GET", f"/v1/chats/{c['id']}/turns/{uuid.uuid4()}").status_code == 404
    r = client_a2.req("GET", f"/v1/chats/{c['id']}/turns/{rid}")
    assert r.status_code == 404
    r = client.req("GET", f"/v1/chats/{c['id']}/turns/nope")
    assert r.status_code == 400


def test_stream_is_not_buffered(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    events = [{"type": "response.output_text.delta", "delta": "first"}]
    events.append({"type": "response.output_text.delta", "delta": "second", "_delay_ms": 1500})
    events.append({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}, "_delay_ms": 1500})
    mock.mock_script([{"events": events}])
    t0 = time.time()
    first_delta_at = None
    buf = ""
    with client.c.stream("POST", f"/v1/chats/{c['id']}/messages:stream", json={"content": "x"}, headers=client.h) as r:
        for chunk in r.iter_text():
            buf += chunk
            if first_delta_at is None and '"first"' in buf:
                first_delta_at = time.time() - t0
    total = time.time() - t0
    assert first_delta_at is not None
    assert first_delta_at < 1.0 and total > 2.5, (first_delta_at, total)
