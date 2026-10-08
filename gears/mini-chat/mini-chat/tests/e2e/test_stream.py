"""Send-message streaming: SSE contract, provider request, persistence, errors."""

from __future__ import annotations

import json
import time
import uuid

import httpx

from conftest import PREFIX, TENANT_A, assert_problem, ok_stream, parse_sse_lines, ub, wait_until

UUID_LEN = 36


def test_stream_contract_and_persistence(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"text": "Hello there, general Kenobi!", "response_id": "resp_mock0001"})
    r = api.send(chat["id"], "hello", request_id=rid)
    ok_stream(r)
    assert r.headers["content-type"].startswith("text/event-stream")
    assert r.headers.get("cache-control") == "no-cache"
    started = r.first("stream_started")
    assert started["request_id"] == rid
    assert started["is_new_turn"] is True
    assert len(started["message_id"]) == UUID_LEN
    assert "thread_summary_applied" not in started
    assert r.text == "Hello there, general Kenobi!"
    for d in r.all("delta"):
        assert d["type"] == "text"
    done = r.first("done")
    assert done["usage"] == {"input_tokens": 42, "output_tokens": 7}
    assert done["effective_model"] == "gpt-premium"
    assert done["selected_model"] == "gpt-premium"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    # no internal identifiers leak
    raw = json.dumps(r.events)
    assert "resp_mock0001" not in raw
    assert "gpt-premium-provider" not in raw

    msgs = api.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    user, asst = msgs
    assert user["content"] == "hello" and user["request_id"] == rid
    assert asst["id"] == started["message_id"]
    assert asst["request_id"] == rid
    assert asst["content"] == "Hello there, general Kenobi!"
    assert asst["model"] == "gpt-premium"
    assert asst["input_tokens"] == 42 and asst["output_tokens"] == 7
    assert "model" not in user
    assert user["attachments"] == [] and asst["attachments"] == []
    assert user["my_reaction"] is None and asst["my_reaction"] is None

    t = api.turn(chat["id"], rid)
    assert t.status_code == 200
    tb = t.json()
    assert tb["request_id"] == rid
    assert tb["state"] == "done"
    assert tb["assistant_message_id"] == asst["id"]
    assert "error_code" not in tb

    rows = server.query("SELECT * FROM chat_turns WHERE chat_id = ?", ub(chat["id"]))
    assert len(rows) == 1
    row = rows[0]
    assert row["state"] == "completed"
    assert row["provider_response_id"] == "resp_mock0001"
    assert row["assistant_message_id"] == ub(asst["id"])
    assert row["requester_user_id"] == ub(api.user_id)
    assert row["requester_type"] == "user"
    assert row["effective_model"] == "gpt-premium"
    assert row["max_output_tokens_applied"] == 4096
    assert row["reserve_tokens"] > 4096
    assert row["completed_at"] is not None
    m = server.query("SELECT model, input_tokens, output_tokens FROM messages WHERE id = ?", ub(asst["id"]))[0]
    assert (m["model"], m["input_tokens"], m["output_tokens"]) == ("gpt-premium", 42, 7)


def test_provider_request_shape(api, mock):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "what is up"))
    reqs = mock.chat_requests(chat["id"])
    assert len(reqs) == 1
    req = reqs[0]
    assert req["path"] == "/v1/responses"
    body = req["json"]
    assert body["model"] == "gpt-premium-provider"
    assert body["stream"] is True
    assert body["store"] is False
    assert body["max_output_tokens"] == 4096
    assert "You are gpt-premium." in body["instructions"]
    assert body["input"][-1] == {"role": "user", "content": [{"type": "input_text", "text": "what is up"}]}
    assert body["user"] == uuid.UUID(TENANT_A).hex + uuid.UUID(api.user_id).hex
    meta = body["metadata"]
    assert meta["tenant_id"] == TENANT_A
    assert meta["user_id"] == api.user_id
    assert meta["chat_id"] == chat["id"]
    assert meta["request_type"] == "chat"
    assert meta["feature"] == "none"
    assert "tools" not in body
    assert body["max_tool_calls"] == 2
    assert body.get("temperature") == 0.7


def test_history_is_sent_in_order(api, mock):
    chat = api.create_chat()
    mock.script({"text": "first answer"}, {"text": "second answer"})
    ok_stream(api.send(chat["id"], "first question"))
    ok_stream(api.send(chat["id"], "second question"))
    body = mock.chat_requests(chat["id"])[-1]["json"]
    roles = [(m["role"], m["content"][0]["text"]) for m in body["input"]]
    assert roles == [
        ("user", "first question"),
        ("assistant", "first answer"),
        ("user", "second question"),
    ]
    assert body["input"][1]["content"][0]["type"] == "output_text"


def test_server_generates_request_id(api):
    chat = api.create_chat()
    r = ok_stream(api.send(chat["id"], "hi"))
    rid = r.first("stream_started")["request_id"]
    uuid.UUID(rid)
    assert api.turn(chat["id"], rid).json()["state"] == "done"


def test_streaming_is_not_buffered(api, server, mock):
    chat = api.create_chat()
    mock.script({"chunks": ["a", "b", "c", "d", "e"], "chunk_delay_ms": 400})
    times: dict[str, list[float]] = {}
    with httpx.Client(base_url=server.base, headers={"Authorization": f"Bearer {api.token}"}, timeout=30) as c:
        with c.stream("POST", f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "go"}) as resp:
            assert resp.status_code == 200

            def mark(name, _data):
                times.setdefault(name, []).append(time.monotonic())
                return False

            parse_sse_lines(resp.iter_lines(), mark)
    first_delta = times["delta"][0]
    done = times["done"][0]
    assert done - first_delta > 1.0, times
    assert times["delta"][-1] - times["delta"][0] > 1.0


def test_ping_before_first_content(api, mock):
    # ping interval is 5 s in the test config; the provider stays silent for 6 s
    chat = api.create_chat()
    mock.script({"initial_delay_ms": 6500, "text": "late"})
    r = ok_stream(api.send(chat["id"], "slow"))
    names = r.names
    assert names[0] == "stream_started"
    assert "ping" in names
    first_delta = names.index("delta")
    assert all(n == "ping" for n in names[1:first_delta])
    assert "ping" not in names[first_delta:]
    assert r.all("ping")[0] == {}


def _assert_failed_turn(api, server, chat_id, rid, code):
    t = api.turn(chat_id, rid).json()
    assert t["state"] == "error"
    assert t["error_code"] == code
    assert "assistant_message_id" not in t
    msgs = api.messages(chat_id)
    assert [m["role"] for m in msgs] == ["user"]
    row = server.query("SELECT state, error_code FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert (row["state"], row["error_code"]) == ("failed", code)


def test_provider_http_error(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"http_status": 500, "error": {"message": "internal failure for resp_abcdefgh123456", "type": "server_error"}})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.status == 200
    assert r.names == ["stream_started", "error"]
    err = r.first("error")
    assert err["code"] == "provider_error"
    assert "resp_abcdefgh123456" not in err["message"]
    _assert_failed_turn(api, server, chat["id"], rid, "provider_error")


def test_provider_rate_limited(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"http_status": 429, "retry_after": 7, "error": {"message": "slow down", "type": "rate_limit"}})
    r = api.send(chat["id"], "x", request_id=rid)
    err = r.first("error")
    assert r.names[-1] == "error"
    assert err["code"] == "rate_limited"
    assert "7" in err["message"]
    _assert_failed_turn(api, server, chat["id"], rid, "rate_limited")


def test_provider_failed_event_is_sanitized(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    secret_msg = (
        "Failure in resp_0123456789abcdef for file-ABCDEFGHIJKLMNOP see https://internal.example.com/x "
        "key sk-abcdefghijklmnop Bearer abc.def.ghi"
    )
    mock.script({"chunks": ["partial "], "terminal": "failed", "error": {"code": "server_error", "message": secret_msg}})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.names[-1] == "error"
    assert "done" not in r.names
    err = r.first("error")
    assert err["code"] == "provider_error"
    msg = err["message"]
    for leak in ("resp_0123456789abcdef", "file-ABCDEFGHIJKLMNOP", "https://", "internal.example.com", "sk-abcdefghijklmnop", "abc.def.ghi"):
        assert leak not in msg, msg
    assert "[provider_id]" in msg
    assert "[url]" in msg
    assert "[credential]" in msg
    _assert_failed_turn(api, server, chat["id"], rid, "provider_error")


def test_provider_error_event(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "error_event", "error": {"code": "server_error", "message": "boom"}})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.names == ["stream_started", "error"]
    assert r.first("error")["code"] == "provider_error"
    _assert_failed_turn(api, server, chat["id"], rid, "provider_error")


def test_provider_stream_ends_without_terminal(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "none"})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.names[-1] == "error"
    assert r.first("error")["code"] in ("provider_error", "provider_timeout")
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error"


def test_incomplete_response_is_done(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"text": "cut short", "terminal": "incomplete"})
    r = ok_stream(api.send(chat["id"], "x", request_id=rid))
    assert r.text == "cut short"
    assert "citations" not in r.names
    assert api.turn(chat["id"], rid).json()["state"] == "done"
    row = server.query("SELECT state FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["state"] == "completed"


def test_completed_with_empty_text(api, mock):
    chat = api.create_chat()
    mock.script({"text": "", "chunks": []})
    r = ok_stream(api.send(chat["id"], "x"))
    msgs = api.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["content"] == ""
    assert r.first("done")


def test_empty_content_rejected_before_provider(api, mock):
    chat = api.create_chat()
    r = api.send(chat["id"], "")
    assert_problem(r, 400, category="invalid_argument", reason="EMPTY_CONTENT")
    assert mock.chat_requests(chat["id"]) == []
    assert api.messages(chat["id"]) == []


def test_stream_body_validation(api):
    chat = api.create_chat()
    r = api.stream_path(f"/chats/{chat['id']}/messages:stream", {"nocontent": True})
    assert_problem(r, 422)
    r = api.post(f"/chats/{chat['id']}/messages:stream", content=b"{", headers={"content-type": "application/json"})
    assert_problem(r, 400)


def test_stream_unknown_chat(api, other_user):
    r = api.send(str(uuid.uuid4()), "hi")
    assert_problem(r, 404, resource_type="gts.cf.core.mini_chat.chat.v1~")
    chat = api.create_chat()
    r = other_user.send(chat["id"], "hi")
    assert_problem(r, 404)


def test_input_too_long(api, mock):
    chat = api.create_chat(model="gpt-tiny")
    r = api.send(chat["id"], "x" * 20_000)
    assert_problem(r, 400, category="out_of_range", reason="INPUT_TOO_LONG")
    assert mock.chat_requests(chat["id"]) == []
    assert api.messages(chat["id"]) == []


def test_context_budget_exceeded_or_too_long_near_limit(api, mock):
    # message estimate just below max_input_tokens but above the assembled budget
    chat = api.create_chat(model="gpt-tiny")
    r = api.send(chat["id"], "y" * 10_600)
    assert_problem(r, 400, category="out_of_range", reason="CONTEXT_BUDGET_EXCEEDED")
    assert mock.chat_requests(chat["id"]) == []


def test_context_truncation_keeps_budget(api, mock):
    """Old history is dropped (oldest first) when it no longer fits gpt-tiny's budget."""
    chat = api.create_chat(model="gpt-tiny")
    for i in range(4):
        mock.script({"text": f"answer {i} " + "z" * 2400})
        ok_stream(api.send(chat["id"], f"question {i} " + "q" * 2400))
    body = mock.chat_requests(chat["id"])[-1]["json"]
    texts = [m["content"][0]["text"] for m in body["input"]]
    assert texts[-1].startswith("question 3")
    assert not any(t.startswith("question 0") for t in texts)
    # the dropped history never starts with an orphaned assistant message
    first_history = body["input"][0]
    assert first_history["role"] == "user"
    total_bytes = sum(len(t) for t in texts) + len(body["instructions"])
    assert total_bytes / 4 < 3072


def test_invalid_attachment_reference(api, mock):
    chat = api.create_chat()
    r = api.send(chat["id"], "see file", attachment_ids=[str(uuid.uuid4())])
    assert_problem(r, 400, category="invalid_argument", reason="invalid_attachment", field="attachment")
    assert mock.chat_requests(chat["id"]) == []
    assert api.messages(chat["id"]) == []


def test_web_search_tool_and_citations(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    text = "Rust 1.80 was released."
    mock.script(
        {
            "events_before": [
                {"type": "response.web_search_call.searching", "item_id": "ws_1"},
                {"type": "response.web_search_call.completed", "item_id": "ws_1"},
            ],
            "text": text,
            "annotations": [
                {"type": "url_citation", "url": "https://blog.rust-lang.org/x", "title": "Rust Blog", "start_index": 0, "end_index": 9}
            ],
        }
    )
    r = ok_stream(api.send(chat["id"], "news?", request_id=rid, web_search={"enabled": True}))
    tools = r.all("tool")
    assert tools == [
        {"phase": "start", "name": "web_search", "details": {}},
        {"phase": "done", "name": "web_search", "details": {}},
    ]
    names = r.names
    assert names.index("citations") == len(names) - 2
    cit = r.first("citations")["items"]
    assert len(cit) == 1
    c = cit[0]
    assert c["source"] == "web"
    assert c["url"] == "https://blog.rust-lang.org/x"
    assert c["title"] == "Rust Blog"
    assert c["span"] == {"start": 0, "end": 9}
    assert c["snippet"] == text[0:9]
    assert "score" not in c
    body = mock.chat_requests(chat["id"])[0]["json"]
    assert any(t["type"] == "web_search" for t in body["tools"])
    assert body["max_tool_calls"] == 2
    assert "web_search" in body["metadata"]["feature"]
    assert "web_search" in body["instructions"]
    row = server.query("SELECT web_search_enabled, web_search_completed_count FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["web_search_enabled"] == 1
    assert row["web_search_completed_count"] == 1


def test_web_search_skipped_for_model_without_support(api, mock):
    chat = api.create_chat(model="gpt-novision")
    ok_stream(api.send(chat["id"], "news?", web_search={"enabled": True}))
    body = mock.chat_requests(chat["id"])[0]["json"]
    assert "tools" not in body
    assert "web_search" not in body["instructions"]


def test_web_search_call_limit(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    searching = [{"type": "response.web_search_call.searching", "item_id": f"ws_{i}"} for i in range(3)]
    mock.script({"events_before": searching, "text": "never"})
    r = api.send(chat["id"], "news?", request_id=rid, web_search={"enabled": True})
    assert r.names[-1] == "error"
    assert r.first("error")["code"] == "web_search_calls_exceeded"
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error" and t["error_code"] == "web_search_calls_exceeded"


def test_usage_and_audit_published_once(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "count me", request_id=rid))
    usage = wait_until(lambda: server.usage_events(request_id=rid), msg="usage event")
    time.sleep(1.0)
    usage = server.usage_events(request_id=rid)
    assert len(usage) == 1
    ev = usage[0]
    turn = server.query("SELECT id FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    turn_id = uuid.UUID(bytes=turn["id"])
    assert ev["dedupe_key"] == f"{uuid.UUID(TENANT_A).hex}/{turn_id.hex}/{uuid.UUID(rid).hex}"
    assert ev["billing_outcome"] == "completed"
    assert ev["settlement_method"] == "actual"
    assert ev["terminal_state"] == "completed"
    assert ev["user_id"] == api.user_id
    assert ev["turn_id"] == str(turn_id)
    assert ev["usage"]["input_tokens"] == 42 and ev["usage"]["output_tokens"] == 7
    # premium: 42 * 3 + 7 * 15
    assert ev["actual_credits_micro"] == 42 * 3 + 7 * 15
    assert ev["requester_type"] == "user"
    audit = wait_until(lambda: server.audit_events(request_id=rid), msg="audit event")
    assert len(audit) == 1
    assert audit[0]["event_type"] == "turn_completed"
    assert audit[0]["effective_model"] == "gpt-premium"
