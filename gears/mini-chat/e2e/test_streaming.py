"""Send-message streaming: SSE contract, done/error events, provider request shape, preflight, persistence.

Acceptance criteria covered:
* Streaming — "Send-message endpoint streams a response, correlated by a request id"
* Streaming — "Preflight validation (content, attachments, limits) runs before any provider call"
* Streaming — "Assistant message and usage are persisted once a stream completes"
* SSE — "Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering"
* SSE — "Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers"
* SSE — "Error event is terminal and carries a sanitized message"
* Principles — "Streaming responses are never buffered before relaying"
"""

from __future__ import annotations

import uuid

import httpx

from helpers import (
    RT_CHAT,
    as_uuid,
    rows,
    ub,
    upload,
    SYSTEM_PROMPT,
    all_of,
    assert_error_stream,
    assert_not_found,
    assert_ok_stream,
    assert_problem,
    assert_sse_order,
    estimate_text_tokens,
    first,
    input_pairs,
    list_messages,
    make_png,
    message_rows,
    names,
    new_chat,
    no_provider_ids,
    send_ok,
    sleep,
    stream_script,
    text_of,
    tenant_id,
    turn_row,
    turn_rows,
    quota_snapshot,
    turn_status,
    upload_ok,
    user_id,
)
from harness import SseEvent


def test_successful_stream_contract(fresh):
    """stream_started (request_id, message_id, is_new_turn) → delta* → done, text/event-stream + no-cache."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    r, events = fresh.stream(cid, "Hello there")
    done = assert_ok_stream(r, events)
    assert "no-cache" in r.headers.get("cache-control", "")
    st = events[0].data
    assert set(st) >= {"request_id", "message_id", "is_new_turn"}
    assert st["is_new_turn"] is True
    uuid.UUID(st["request_id"])
    uuid.UUID(st["message_id"])
    assert "thread_summary_applied" not in st
    deltas = all_of(events, "delta")
    assert [d.data for d in deltas] == [{"type": "text", "content": c} for c in ("Hello", " from", " mock")]
    assert done.data["usage"] == {"input_tokens": 100, "output_tokens": 50}
    assert done.data["effective_model"] == "gpt-4.1-mini"
    assert done.data["selected_model"] == "gpt-4.1-mini"
    assert done.data["quota_decision"] == "allow"
    assert "downgrade_from" not in done.data and "downgrade_reason" not in done.data
    assert isinstance(done.data.get("quota_warnings"), list)
    for w in done.data["quota_warnings"]:
        assert {"tier", "period", "remaining_percentage", "warning", "exhausted"} <= set(w)
    # Turn status correlates by request id.
    ts = turn_status(fresh, cid, st["request_id"])
    assert ts.status_code == 200, ts.text
    assert ts.json()["state"] == "done"
    assert ts.json()["assistant_message_id"] == st["message_id"]
    assert ts.json()["request_id"] == st["request_id"]


def test_client_request_id_is_used(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid1())  # any UUID version is accepted
    st, _, _ = send_ok(fresh, cid, "hi", request_id=rid)
    assert st["request_id"] == rid
    msgs = list_messages(fresh, cid)
    assert {m["request_id"] for m in msgs} == {rid}
    row = turn_row(fresh, cid, rid)
    assert row and row["state"] == "completed"


def test_generated_request_id_is_v4(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    st, _, _ = send_ok(fresh, cid, "hi")
    assert uuid.UUID(st["request_id"]).version == 4


def test_assistant_message_and_usage_persisted(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("Persist", " me", usage={"input_tokens": 321, "output_tokens": 45}, resp_id="resp_persist0123456789abcdef"))
    st, done, events = send_ok(fresh, cid, "persist please")
    assert done["usage"] == {"input_tokens": 321, "output_tokens": 45}
    row = turn_row(fresh, cid, st["request_id"])
    assert row["state"] == "completed"
    assert row["completed_at"] is not None
    assert row["error_code"] is None
    assert row["provider_response_id"] == "resp_persist0123456789abcdef"
    msgs = message_rows(fresh, cid)
    asst = [m for m in msgs if m["role"] == "assistant"]
    assert len(asst) == 1
    assert asst[0]["content"] == "Persist me"
    assert asst[0]["input_tokens"] == 321 and asst[0]["output_tokens"] == 45
    assert asst[0]["model"] == "gpt-4.1-mini"
    listed = list_messages(fresh, cid)[1]
    assert listed["content"] == "Persist me" and listed["input_tokens"] == 321 and listed["output_tokens"] == 45
    # No provider identifiers anywhere in the SSE payloads.
    for e in events:
        assert "resp_persist0123456789abcdef" not in str(e.data)
        no_provider_ids(e.data)


def test_provider_request_shape(fresh):
    """Basic request: model, stream, max_output_tokens, user, metadata, store false, no tools, instructions."""
    cid = new_chat(fresh, "gpt-4.1")
    send_ok(fresh, cid, "Shape check")
    reqs = fresh.chat_requests()
    assert len(reqs) == 1
    b = reqs[0]
    assert b["model"] == "gpt-4.1"
    assert b["stream"] is True
    assert b["max_output_tokens"] == 32768
    assert b.get("store") is False
    assert not b.get("tools"), "no tools for a text-only message without attachments/web search"
    assert "max_tool_calls" not in b
    assert b["instructions"].startswith(SYSTEM_PROMPT)
    expected_user = uuid.UUID(tenant_id("a1")).hex + uuid.UUID(user_id("a1")).hex
    assert b["user"] == expected_user and len(b["user"]) == 64
    md = b["metadata"]
    assert md["tenant_id"] == tenant_id("a1")
    assert md["user_id"] == user_id("a1")
    assert md["chat_id"] == cid
    assert md["request_type"] == "chat"
    assert "feature" in md
    assert b.get("temperature") == 0.7, "catalog api_params that are set are forwarded"
    assert input_pairs(b)[-1] == ("user", "Shape check")
    # Proxied through OAGW to the configured api_path.
    paths = [r["path"] for r in fresh.responses_requests()]
    assert paths == ["/v1/responses"]


def test_provider_model_id_is_used(mc_factory):
    from harness import ServerOptions, catalog_entry, default_catalog

    cat = default_catalog()
    cat.append(catalog_entry("aliased", "Standard", provider_model_id="gpt-4o-2024-08-06"))
    srv = mc_factory("aliased", ServerOptions(catalog=cat))
    srv.mock_reset()
    cid = new_chat(srv, "aliased")
    send_ok(srv, cid, "x")
    assert srv.chat_requests()[0]["model"] == "gpt-4o-2024-08-06"


def test_no_buffering_first_delta_before_provider_completes(fresh):
    """Streaming responses are never buffered: the first delta arrives while the provider is still pausing."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("early", sleep(3000), " late"))
    t_first = t_done = None
    got: list[SseEvent] = []
    for t, item in fresh.stream_events(cid, "go"):
        if isinstance(item, httpx.Response):
            assert item.status_code == 200
            continue
        got.append(item)
        if item.event == "delta" and t_first is None:
            t_first = t
        if item.event == "done":
            t_done = t
    assert t_first is not None and t_done is not None, names(got)
    assert t_done - t_first >= 2.5, f"first delta at {t_first:.2f}s, done at {t_done:.2f}s: response was buffered"
    assert text_of(got) == "early late"


def test_ping_only_before_first_content(lim):
    """keepalive: ping {} every sse_ping_interval_seconds (5 s here) only before the first delta/tool."""
    cid = new_chat(lim, "gpt-4.1-mini")
    lim.mock_script(stream_script(sleep(6500), "after pause", sleep(6000), " more"))
    r, events = lim.stream(cid, "ping me")
    assert_ok_stream(r, events)
    idx_delta = names(events).index("delta")
    pings = [i for i, e in enumerate(events) if e.event == "ping"]
    assert pings, f"expected ping during the idle period: {names(events)}"
    assert all(i < idx_delta for i in pings), "no ping after content started"
    assert all(events[i].data == {} for i in pings)


def test_event_name_from_data_type(fresh):
    """The provider event name comes from data.type when the event: line is missing or 'message'."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    script = stream_script("A", terminal="completed")
    # Replace the delta event by raw frames: one without an event line, one with event: message.
    events = script["events"]
    raw1 = {"raw": 'data: {"type":"response.output_text.delta","delta":"B","item_id":"m","output_index":0,"content_index":0}\n\n'}
    raw2 = {"raw": 'event: message\ndata: {"type":"response.output_text.delta","delta":"C","item_id":"m","output_index":0,"content_index":0}\n\n'}
    events.insert(len(events) - 1, raw1)
    events.insert(len(events) - 1, raw2)
    fresh.mock_script(script)
    _, _, evs = send_ok(fresh, cid, "x")
    assert text_of(evs) == "ABC"


def test_tool_and_citation_events_order(fresh):
    """tool start/done (web search), delta, citations (web item), then done."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    from helpers import ws_completed, ws_searching

    ann = [{"type": "url_citation", "url": "https://example.com/a", "title": "Example A", "start_index": 0, "end_index": 6}]
    fresh.mock_script(
        stream_script(
            ws_searching(),
            ws_completed(),
            "Answer text",
            annotations=ann,
            output_extra=[{"type": "web_search_call", "id": "ws_1", "status": "completed"}],
        )
    )
    st, done, events = send_ok(fresh, cid, "search it", web_search={"enabled": True})
    seq = [e.event for e in events if e.event != "ping"]
    assert seq == ["stream_started", "tool", "tool", "delta", "citations", "done"], seq
    tools = all_of(events, "tool")
    assert tools[0].data["phase"] == "start" and tools[0].data["name"] == "web_search"
    assert tools[1].data["phase"] == "done" and tools[1].data["name"] == "web_search"
    assert isinstance(tools[0].data.get("details"), dict)
    items = first(events, "citations").data["items"]
    assert len(items) == 1, items
    c = items[0]
    assert c["source"] == "web" and c["url"] == "https://example.com/a" and c["title"] == "Example A"
    assert c["snippet"] == "Answer", "snippet = answer text in the annotation range"
    assert c.get("span") == {"start": 0, "end": 6}
    assert "attachment_id" not in c or c["attachment_id"] is None


def test_file_search_and_code_interpreter_tool_events(fresh):
    """file_search searching/completed → tool start/done (files_searched); code interpreter start/done with output."""
    from helpers import MIME_XLSX, ci_done, ci_in_progress, fs_completed, fs_searching, make_xlsx

    cid = new_chat(fresh, "gpt-4.1-mini")
    upload_ok(fresh, cid, "doc.pdf")
    upload_ok(fresh, cid, "data.xlsx", make_xlsx(), MIME_XLSX)
    fresh.mock_reset()
    fresh.mock_script(
        stream_script(
            fs_searching(),
            fs_completed(results=[{"file_id": "x"}, {"file_id": "y"}]),
            ci_in_progress(),
            ci_done(["line1", "line2"]),
            "Done.",
        )
    )
    _, _, events = send_ok(fresh, cid, "analyze")
    tools = [e.data for e in all_of(events, "tool")]
    assert tools[0]["phase"] == "start" and tools[0]["name"] == "file_search"
    assert tools[1]["phase"] == "done" and tools[1]["name"] == "file_search"
    assert tools[1]["details"].get("files_searched") == 2
    assert tools[2]["phase"] == "start" and tools[2]["name"] == "code_interpreter"
    assert tools[3]["phase"] == "done" and tools[3]["name"] == "code_interpreter"
    assert tools[3]["details"]["output"] == "line1\nline2"


def test_code_interpreter_output_truncated(fresh):
    from helpers import MIME_XLSX, ci_done, ci_in_progress, make_xlsx

    cid = new_chat(fresh, "gpt-4.1-mini")
    upload_ok(fresh, cid, "data.xlsx", make_xlsx(), MIME_XLSX)
    fresh.mock_reset()
    fresh.mock_script(stream_script(ci_in_progress(), ci_done(["x" * 9000]), "ok"))
    _, _, events = send_ok(fresh, cid, "big output")
    out = [e.data for e in all_of(events, "tool") if e.data["phase"] == "done"][0]["details"]["output"]
    assert out.endswith("...[truncated]")
    assert out.startswith("x" * 8192)
    assert len(out) == 8192 + len("...[truncated]")


def test_error_event_terminal_and_turn_failed(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("partial", terminal="failed", error={"code": "server_error", "message": "upstream exploded"}, usage={}))
    r, events = fresh.stream(cid, "fail me")
    err = assert_error_stream(r, events, "provider_error")
    assert "upstream exploded" in err["message"]
    rid = events[0].data["request_id"]
    ts = turn_status(fresh, cid, rid).json()
    assert ts["state"] == "error" and ts["error_code"] == "provider_error"
    assert "assistant_message_id" not in ts or ts["assistant_message_id"] is None
    assert turn_row(fresh, cid, rid)["error_code"] == "provider_error"


def test_done_has_no_internal_ids_and_replay_shape(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    resp_id = "resp_abc123def456ghi789jkl0"
    fresh.mock_script(stream_script("Hi", resp_id=resp_id))
    r, events = fresh.stream(cid, "q")
    assert_ok_stream(r, events)
    assert resp_id not in r.text
    st = events[0].data
    assert turn_row(fresh, cid, st["request_id"])["provider_response_id"] == resp_id


# ── preflight validation ───────────────────────────────────────────────────
def test_empty_content_rejected_before_provider(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    for content in ("", "   ", "\n\t "):
        r, events = fresh.stream(cid, content)
        assert events == []
        assert_problem(r, 400, field_reason="EMPTY_CONTENT", field="content")
    assert fresh.chat_requests() == []
    assert list_messages(fresh, cid) == []


def test_stream_into_unknown_chat_404(fresh):
    r, events = fresh.stream(str(uuid.uuid4()), "hi")
    assert events == []
    assert_not_found(r, RT_CHAT)
    assert fresh.chat_requests() == []


def test_schema_errors_on_stream(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    r = fresh.req("POST", f"/chats/{cid}/messages:stream", json={"content": "x", "attachment_ids": ["not-a-uuid"]})
    assert r.status_code == 422, r.text
    r = fresh.req("POST", f"/chats/{cid}/messages:stream", json={})
    assert r.status_code == 422, r.text
    r = fresh.req("POST", f"/chats/{cid}/messages:stream", content=b"{bad json", headers={"Content-Type": "application/json"})
    assert r.status_code == 400, r.text
    assert fresh.chat_requests() == []


def test_duplicate_and_too_many_attachment_ids(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    att = upload_ok(fresh, cid)["id"]
    r, _ = fresh.stream(cid, "x", attachment_ids=[att, att])
    assert_problem(r, 400, field_reason="invalid_attachment")
    too_many = [str(uuid.uuid4()) for _ in range(55)]  # > max_documents_per_chat (50) + max_images_per_message (4)
    r, _ = fresh.stream(cid, "x", attachment_ids=too_many)
    assert_problem(r, 400, field_reason="invalid_attachment")
    assert fresh.chat_requests() == []


def test_foreign_or_unknown_attachment_rolls_back(fresh):
    """Not-ready/foreign attachment: 400 invalid_attachment, no user message, turn or reserve remains."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    other = new_chat(fresh, "gpt-4.1-mini")
    att_other = upload_ok(fresh, other)["id"]
    fresh.mock_reset()
    before = {k: v["reserved_credits_micro"] for k, v in quota_snapshot(fresh, "a1").items()}
    for ids in ([att_other], [str(uuid.uuid4())]):
        r, events = fresh.stream(cid, "with bad attachment", attachment_ids=ids)
        assert events == []
        assert_problem(r, 400, field_reason="invalid_attachment")
    assert fresh.chat_requests() == []
    assert message_rows(fresh, cid) == []
    assert turn_rows(fresh, cid) == []
    after = {k: v["reserved_credits_micro"] for k, v in quota_snapshot(fresh, "a1").items()}
    for k, v in before.items():
        assert after.get(k) == v, f"reserve leaked in {k}"


def test_attachment_of_other_user_rejected(fresh):
    """An attachment uploaded by another user is not usable (owner-only chats make it foreign)."""
    cid_b = new_chat(fresh, "gpt-4.1-mini", user="b")
    att_b = upload_ok(fresh, cid_b, user="b")["id"]
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_reset()
    r, _ = fresh.stream(cid, "x", attachment_ids=[att_b])
    assert_problem(r, 400, field_reason="invalid_attachment")
    assert fresh.chat_requests() == []


def test_failed_attachment_rejected(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_config(file_upload_status=500)
    r = upload(fresh, cid, "bad.pdf", b"%PDF-1.4 x", "application/pdf")
    assert r.status_code == 503
    att = rows(fresh, "SELECT id FROM attachments WHERE chat_id = ?", (ub(cid),))
    assert att
    att_id = as_uuid(att[0]["id"])
    fresh.mock_reset()
    r, _ = fresh.stream(cid, "x", attachment_ids=[att_id])
    assert_problem(r, 400, field_reason="invalid_attachment")
    assert fresh.chat_requests() == []


def test_input_too_long_rejected_before_provider(fresh):
    cid = new_chat(fresh, "tiny-ctx")
    content = "w" * 12000  # estimate ≈ 3410 tokens > max_input_tokens 3072
    assert estimate_text_tokens(content) > 3072
    r, events = fresh.stream(cid, content)
    assert events == []
    assert_problem(r, 400, field_reason="INPUT_TOO_LONG")
    assert fresh.chat_requests() == []
    assert message_rows(fresh, cid) == []


def test_images_on_non_vision_model_rejected(fresh):
    cid = new_chat(fresh, "std-novision")
    img = upload_ok(fresh, cid, "pic.png", make_png(), "image/png")
    fresh.mock_reset()
    r, _ = fresh.stream(cid, "describe", attachment_ids=[img["id"]])
    assert_problem(r, 400, field_reason="VISION_NOT_SUPPORTED")
    assert fresh.chat_requests() == []


def test_too_many_images_rejected(lim):
    """rag.max_images_per_message = 2 on this server."""
    cid = new_chat(lim, "gpt-4.1-mini")
    ids = [upload_ok(lim, cid, f"p{i}.png", make_png(8 + i, 8), "image/png")["id"] for i in range(3)]
    lim.mock_reset()
    r, _ = lim.stream(cid, "three images", attachment_ids=ids)
    assert_problem(r, 400, field_reason="TOO_MANY_IMAGES", field="image_count")
    assert lim.chat_requests() == []
    # Two images are fine.
    send_ok(lim, cid, "two images", attachment_ids=ids[:2])


def test_web_search_kill_switch(ks):
    cid = new_chat(ks, "gpt-4.1-mini")
    r, _ = ks.stream(cid, "search", web_search={"enabled": True})
    assert_problem(r, 400, subject="web_search", vtype="FEATURE_DISABLED")
    assert ks.chat_requests() == []
    # Without web search the same chat works.
    send_ok(ks, cid, "no search")


def test_chat_model_removed_from_catalog(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.execute("UPDATE chats SET model = 'ghost-model' WHERE id = ?", (ub(cid),))
    r, events = fresh.stream(cid, "hello")
    assert events == []
    assert_problem(r, 400, field_reason="INVALID_MODEL")
    assert fresh.chat_requests() == []


def test_error_event_sse_order_with_deltas(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("a", "b", terminal="error", error={"code": "server_error", "message": "stream broke"}))
    r, events = fresh.stream(cid, "x")
    assert_sse_order(events)
    assert events[-1].event == "error" and events[-1].data["code"] == "provider_error"
    assert text_of(events) == "ab"
