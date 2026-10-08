"""Regression tests for review findings: unexpected function calls, stored content, usage
null, check order of the send pipeline, chat list ordering by a nullable title, pings while
the provider has not answered yet.
"""

from __future__ import annotations

import uuid
from urllib.parse import quote

from helpers import (
    assert_error_stream,
    assert_ok_stream,
    assert_problem,
    function_call_script,
    message_rows,
    new_chat,
    names,
    send_ok,
    stream_script,
    text_of,
    wait_turn_state,
    wait_usage_event,
)


def test_function_call_without_knowledge_search_is_unexpected_tool_use(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(function_call_script("call_1", text="Let me check. ", usage={"input_tokens": 12, "output_tokens": 3}))
    r, events = fresh.stream(cid, "search please")
    err = assert_error_stream(r, events, "unexpected_tool_use")
    assert err["message"]
    assert len(fresh.chat_requests()) == 1
    req = fresh.chat_requests()[0]
    assert not [t for t in req.get("tools") or [] if t.get("type") == "function"]
    rid = events[0].data["request_id"]
    t = wait_turn_state(fresh, cid, rid, ["error"])
    assert t["error_code"] == "unexpected_tool_use"
    ev = wait_usage_event(fresh, rid)
    assert ev["usage"]["input_tokens"] == 12


def test_stored_content_is_delta_text_only(fresh):
    """No fallback to the terminal output_text when the stream had no text deltas."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    script = stream_script()
    # Terminal output text that was never streamed as deltas.
    script["events"][-1]["data"]["response"]["output"][0]["content"][0]["text"] = "not streamed"
    fresh.mock_script(script)
    r, events = fresh.stream(cid, "hi")
    assert_ok_stream(r, events)
    assert text_of(events) == ""
    rid = events[0].data["request_id"]
    wait_turn_state(fresh, cid, rid, ["done"])
    assistant = [m for m in message_rows(fresh, cid) if m["role"] == "assistant"]
    assert assistant[-1]["content"] == ""


def test_usage_is_null_when_provider_reports_none(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    script = stream_script("answer")
    del script["events"][-1]["data"]["response"]["usage"]
    fresh.mock_script(script)
    r, events = fresh.stream(cid, "hi")
    assert_ok_stream(r, events)
    rid = events[0].data["request_id"]
    ev = wait_usage_event(fresh, rid)
    assert ev["settlement_method"] == "actual"
    assert ev["usage"] is None


def test_idempotency_check_runs_before_validation(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    send_ok(fresh, cid, "original", request_id=rid)
    dup = str(uuid.uuid4())
    # Replay although the retried body is invalid (empty content, duplicate attachment ids).
    r, events = fresh.stream(cid, "   ", request_id=rid, attachment_ids=[dup, dup])
    assert_ok_stream(r, events)
    assert events[0].data["is_new_turn"] is False

    # A failed turn: 409 request_id_conflict before EMPTY_CONTENT.
    failed = str(uuid.uuid4())
    fresh.mock_script(stream_script("x", terminal="failed"))
    fresh.stream(cid, "will fail", request_id=failed)
    wait_turn_state(fresh, cid, failed, ["error"])
    r, _ = fresh.stream(cid, "  ", request_id=failed)
    assert r.status_code == 409, r.text
    assert "request_id_conflict" in r.text


def test_ping_while_the_provider_has_not_answered(lim):
    """A provider that delays its response headers still gets keepalive pings (5 s interval here)."""
    cid = new_chat(lim, "gpt-4.1-mini")
    script = stream_script("late answer")
    script["pre_delay_ms"] = 6500
    lim.mock_script(script)
    r, events = lim.stream(cid, "slow provider")
    assert_ok_stream(r, events)
    n = names(events)
    assert n[1] == "ping", n
    assert all(i < n.index("delta") for i, x in enumerate(n) if x == "ping")


def test_list_ordered_by_title_with_untitled_chats(fresh):
    """$orderby=title pages through untitled chats (ordered as an empty title)."""
    user = "a2"
    qs = fresh
    mine = [new_chat(qs, "gpt-4.1-mini", user=user, title=t) for t in ("b", None, "a", None, "c")]
    seen: list[str] = []
    q = f"limit=2&{quote('$orderby')}={quote('title asc')}"
    for _ in range(200):
        r = qs.req("GET", f"/chats?{q}", user)
        assert r.status_code == 200, r.text
        page = r.json()
        seen += [c["id"] for c in page["items"]]
        nxt = page["page_info"].get("next_cursor")
        if not nxt:
            break
        q = f"limit=2&cursor={nxt}"
    ours = [c for c in seen if c in mine]
    assert len(ours) == len(set(ours)) == 5, seen
    titled = [c for c in ours if c in (mine[0], mine[2], mine[4])]
    assert titled == [mine[2], mine[0], mine[4]], "a, b, c ascending"
    untitled_pos = [ours.index(mine[1]), ours.index(mine[3])]
    assert max(untitled_pos) < ours.index(mine[2]), "untitled chats sort first ascending"
