"""Web search: tool events, citations, accounting, per-message and daily limits, kill switch.

Acceptance criteria covered:
* Web Search — "Web search tool use is reported, cited, accounted, and quota-limited correctly"
* Context Assembly — "Tool availability and guidance are reflected correctly in the assembled request" (web search part)
"""

from __future__ import annotations

import pytest

from helpers import (
    QuotaRestorer,
    SYSTEM_PROMPT,
    all_of,
    assert_error_stream,
    assert_problem,
    ensure_quota_rows,
    first,
    new_chat,
    quota_snapshot,
    send_ok,
    set_quota,
    stream_script,
    tool,
    turn_row,
    wait_usage_event,
    ws_completed,
    ws_searching,
)

WEB_ANNOTATION = {
    "type": "url_citation",
    "url": "https://news.example.org/story",
    "title": "Story title",
    "start_index": 4,
    "end_index": 9,
}


def _search_script(n_completed=1, annotations=None):
    items = []
    for i in range(n_completed):
        items += [ws_searching(f"ws_{i}"), ws_completed(f"ws_{i}")]
    items.append("The answer is here.")
    return stream_script(
        *items,
        annotations=annotations if annotations is not None else [WEB_ANNOTATION],
        output_extra=[{"type": "web_search_call", "id": f"ws_{i}", "status": "completed"} for i in range(n_completed)],
    )


@pytest.fixture
def wq(qs):
    ensure_quota_rows(qs, "a1")
    saver = QuotaRestorer(qs, "a1")
    qs.mock_reset()
    yield qs
    saver.restore()


def test_web_search_tool_in_request(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "plain")
    plain = fresh.chat_requests()[-1]
    assert tool(plain, "web_search") is None
    send_ok(fresh, cid, "with search", web_search={"enabled": True})
    body = fresh.chat_requests()[-1]
    ws = tool(body, "web_search")
    assert ws is not None, body.get("tools")
    assert ws.get("search_context_size") == "low"
    assert body.get("max_tool_calls") == 10
    assert body["instructions"].startswith(SYSTEM_PROMPT)
    assert len(body["instructions"]) > len(plain["instructions"]), "web search guard appended to the instructions"
    send_ok(fresh, cid, "disabled explicitly", web_search={"enabled": False})
    assert tool(fresh.chat_requests()[-1], "web_search") is None


def test_web_search_ignored_on_model_without_support(fresh):
    cid = new_chat(fresh, "std-novision")
    send_ok(fresh, cid, "search please", web_search={"enabled": True})
    body = fresh.chat_requests()[-1]
    assert tool(body, "web_search") is None
    assert body["instructions"].strip() == SYSTEM_PROMPT, "no web search guard without the tool"


def test_web_search_reported_cited_and_accounted(wq):
    cid = new_chat(wq, "gpt-4.1-mini")
    before = quota_snapshot(wq, "a1")
    wq.mock_script(_search_script(1))
    st, done, events = send_ok(wq, cid, "what's new?", web_search={"enabled": True})
    tools = [e.data for e in all_of(events, "tool")]
    assert [(t["phase"], t["name"]) for t in tools] == [("start", "web_search"), ("done", "web_search")]
    cits = first(events, "citations").data["items"]
    assert len(cits) == 1
    c = cits[0]
    assert c["source"] == "web" and c["url"] == WEB_ANNOTATION["url"] and c["title"] == "Story title"
    assert c["snippet"] == "The answer is here."[4:9]
    assert c["span"] == {"start": 4, "end": 9}
    assert turn_row(wq, cid, st["request_id"])["web_search_completed_count"] == 1
    after = quota_snapshot(wq, "a1")
    assert int(after[("daily", "total")]["web_search_calls"]) - int(before[("daily", "total")]["web_search_calls"]) == 1
    ev = wait_usage_event(wq, st["request_id"])
    assert ev["web_search_calls"] == 1


def test_no_citations_event_without_annotations(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(_search_script(1, annotations=[]))
    _, _, events = send_ok(fresh, cid, "q", web_search={"enabled": True})
    assert not all_of(events, "citations")


def test_per_message_web_search_limit(wq):
    """Three started searches with the default per-message limit 2 → error web_search_calls_exceeded."""
    cid = new_chat(wq, "gpt-4.1-mini")
    wq.mock_script(
        stream_script(
            ws_searching("a"), ws_completed("a"), ws_searching("b"), ws_completed("b"), ws_searching("c"), ws_completed("c"), "too much"
        )
    )
    r, events = wq.stream(cid, "search a lot", web_search={"enabled": True})
    assert_error_stream(r, events, "web_search_calls_exceeded")
    rid = events[0].data["request_id"]
    row = turn_row(wq, cid, rid)
    assert row["state"] == "failed" and row["error_code"] == "web_search_calls_exceeded"
    ev = wait_usage_event(wq, rid)
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "estimated"


def test_daily_web_search_quota(wq):
    set_quota(wq, "a1", "daily", "total", web_search_calls=75)
    cid = new_chat(wq, "gpt-4.1-mini")
    r, events = wq.stream(cid, "search", web_search={"enabled": True})
    assert events == []
    assert_problem(r, 429, subject="web_search", description="quota_exceeded")
    assert wq.chat_requests() == []
    # Without web search the user is not limited.
    send_ok(wq, cid, "no search")
    # The daily quota only applies when the effective model supports web search.
    cid2 = new_chat(wq, "std-novision")
    send_ok(wq, cid2, "search on a model without the tool", web_search={"enabled": True})


def test_web_search_kill_switch(ks):
    cid = new_chat(ks, "gpt-4.1-mini")
    r, events = ks.stream(cid, "search", web_search={"enabled": True})
    assert events == []
    assert_problem(r, 400, subject="web_search", vtype="FEATURE_DISABLED")
    assert ks.chat_requests() == []
