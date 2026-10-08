"""Web search tool, guard, daily quota and web citations (DESIGN section 4 "Web Search
Configuration", "Web Search Quota Enforcement", section 3.3 citations)."""

import datetime as dt
import uuid
from contextlib import closing

import pytest

from . import mock_provider as mp
from .helpers import (CURRENT_PERIODS, PREFIX, TOKEN_A_REVIEWER, api, assert_problem, create_chat, db, owner_quota_rows,
                      stream, uuid_bytes)

pytestmark = pytest.mark.usefixtures("server")

GUARD = "Use web_search only if the answer cannot be obtained"


def test_enabled_web_search_sends_tool_and_guard(reset_mock):
    s = api()
    chat = create_chat(s)  # gpt-premium supports web_search
    res = stream(s, chat["id"], {"content": "news?", "web_search": {"enabled": True}})
    assert res.terminal[0] == "done"
    body = reset_mock.requests(path="/responses")[-1]["json"]
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"]
    assert GUARD in body["instructions"]
    assert body["metadata"]["feature"] == "web_search"


def test_disabled_or_unsupported_web_search_sends_no_tool(reset_mock):
    s = api()
    chat = create_chat(s)
    stream(s, chat["id"], {"content": "hi", "web_search": {"enabled": False}})
    body = reset_mock.requests(path="/responses")[-1]["json"]
    assert "tools" not in body and GUARD not in body["instructions"]

    std = create_chat(s, model="gpt-standard")  # no web_search support
    res = stream(s, std["id"], {"content": "hi", "web_search": {"enabled": True}})
    assert res.terminal[0] == "done"
    body = reset_mock.requests(path="/responses")[-1]["json"]
    assert not any(t.get("type") == "web_search" for t in body.get("tools", []))
    assert GUARD not in body["instructions"]
    with closing(db()) as conn:
        flag = conn.execute("SELECT web_search_enabled FROM chat_turns WHERE chat_id = ?",
                            (uuid_bytes(std["id"]),)).fetchone()[0]
    assert flag == 1


def test_daily_web_search_quota_is_429(reset_mock):
    s = api(TOKEN_A_REVIEWER)
    chat = create_chat(s)
    with closing(db()) as conn:
        owner = conn.execute("SELECT tenant_id, user_id FROM chats WHERE id = ?",
                             (uuid_bytes(chat["id"]),)).fetchone()
    today = dt.datetime.now(dt.timezone.utc).date().isoformat()
    key = (owner["tenant_id"], owner["user_id"], today)
    with closing(db()) as conn:
        conn.execute(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, web_search_calls)"
            " VALUES (?, ?, ?, 'daily', ?, 'total', 75)"
            " ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET"
            " web_search_calls = web_search_calls + 75",
            (uuid_bytes(str(uuid.uuid4())), *key))
        conn.commit()
    try:
        r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream",
                   json={"content": "news?", "web_search": {"enabled": True}})
        p = assert_problem(r, 429, "resource_exhausted")
        assert p["context"]["violations"][0]["subject"] == "web_search"
        assert reset_mock.requests(path="/responses") == []
        # Without web search the same user is not blocked by this quota.
        assert stream(s, chat["id"], {"content": "plain"}).terminal[0] == "done"
    finally:
        with closing(db()) as conn:
            conn.execute(
                "UPDATE quota_usage SET web_search_calls = web_search_calls - 75 WHERE tenant_id = ?"
                " AND user_id = ? AND period_type = 'daily' AND period_start = ? AND bucket = 'total'", key)
            conn.commit()


def test_web_citations_are_mapped(reset_mock):
    reset_mock.enqueue("responses", {"events": [
        mp.ev_created(), mp.ev_delta("Hello world"),
        mp.ev_url_citation("https://example.com/page", title="Example Page", start=0, end=5),
        mp.ev_completed()]})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "cite", "web_search": {"enabled": True}})
    assert res.names() == ["stream_started", "delta", "citations", "done"]
    assert res.of("citations")[0] == {"items": [{
        "source": "web", "title": "Example Page", "url": "https://example.com/page",
        "snippet": "Hello", "span": {"start": 0, "end": 5}}]}


def test_web_search_call_limit_fails_turn(reset_mock):
    # quota.web_search_max_calls_per_message defaults to 2.
    reset_mock.enqueue("responses", {"events": [mp.ev_created()] + [mp.ev_web_search()] * 3, "hang": True})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "search a lot", "web_search": {"enabled": True}})
    assert res.terminal[1]["code"] == "web_search_calls_exceeded"
    assert [d["phase"] for d in res.of("tool")] == ["start", "start"]
    with closing(db()) as conn:
        row = conn.execute("SELECT state, error_code FROM chat_turns WHERE chat_id = ?",
                           (uuid_bytes(chat["id"]),)).fetchone()
    assert (row["state"], row["error_code"]) == ("failed", "web_search_calls_exceeded")


def _web_search_calls(chat_id: str) -> dict:
    rows = owner_quota_rows(chat_id, "period_type, web_search_calls", f"bucket = 'total' AND {CURRENT_PERIODS}")
    return {r["period_type"]: r["web_search_calls"] for r in rows}


def test_completed_web_search_calls_are_accounted(reset_mock):
    s = api()
    chat = create_chat(s)
    assert stream(s, chat["id"], {"content": "warm up"}).terminal[0] == "done"  # creates the rows
    before = _web_search_calls(chat["id"])
    reset_mock.enqueue("responses", {"events": [
        mp.ev_created(), mp.ev_web_search(), mp.ev_web_search(done=True), mp.ev_delta("found"),
        mp.ev_completed()]})
    res = stream(s, chat["id"], {"content": "search once", "web_search": {"enabled": True}})
    assert res.terminal[0] == "done", res.raw
    after = _web_search_calls(chat["id"])
    assert after == {k: v + 1 for k, v in before.items()} and set(after) == {"daily", "monthly"}
    with closing(db()) as conn:
        row = conn.execute("SELECT web_search_completed_count FROM chat_turns WHERE chat_id = ?"
                           " AND deleted_at IS NULL ORDER BY started_at DESC LIMIT 1",
                           (uuid_bytes(chat["id"]),)).fetchone()
    assert row["web_search_completed_count"] == 1
