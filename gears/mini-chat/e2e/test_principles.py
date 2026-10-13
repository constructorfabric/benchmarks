"""Cross-cutting principles and constraints.

Acceptance criteria covered (each test names its item):
* "Tenant and owner isolation enforced on every resource"
* "Context window budget enforced, for both the input message and the full assembled request"
* "Streaming responses are never buffered before relaying"
* "A chat's model is immutable once set"
* "Quota is checked before any outbound provider call"
"""

from __future__ import annotations

import httpx

from helpers import (
    QuotaRestorer,
    RT_CHAT,
    assert_not_found,
    assert_not_found_any,
    assert_problem,
    ensure_quota_rows,
    get_chat,
    new_chat,
    send_ok,
    set_quota,
    sleep,
    stream_script,
    tenant_id,
    ub,
    rows,
)


def test_tenant_and_owner_isolation(fresh):
    """Tenant and owner isolation enforced on every resource (rows carry tenant/owner; foreign access is 404)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    st, _, _ = send_ok(fresh, cid, "isolated")
    for table, col in (("chats", "id"), ("messages", "chat_id"), ("chat_turns", "chat_id")):
        rs = rows(fresh, f"SELECT tenant_id FROM {table} WHERE {col} = ?", (ub(cid),))
        assert rs and all(r["tenant_id"] == ub(tenant_id("a1")) for r in rs), table
    for user in ("a2", "b"):
        assert_not_found(fresh.req("GET", f"/chats/{cid}", user), RT_CHAT)
        assert_not_found_any(fresh.req("GET", f"/chats/{cid}/turns/{st['request_id']}", user), RT_CHAT, "gts.cf.core.mini_chat.turn.v1~")


def test_context_budget_input_and_assembled_request(fresh, lim):
    """Context window budget enforced, for both the input message and the full assembled request."""
    cid = new_chat(fresh, "tiny-ctx")
    r, _ = fresh.stream(cid, "m" * 13000)
    assert_problem(r, 400, field_reason="INPUT_TOO_LONG")
    cid2 = new_chat(lim, "budget-test")
    r, _ = lim.stream(cid2, "n" * 8000)
    assert_problem(r, 400, field_reason="CONTEXT_BUDGET_EXCEEDED")
    assert fresh.chat_requests() == [] and lim.chat_requests() == []


def test_streaming_not_buffered(fresh):
    """Streaming responses are never buffered before relaying."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("first", sleep(3000), " second"))
    first_at = end_at = None
    for t, item in fresh.stream_events(cid, "go"):
        if isinstance(item, httpx.Response):
            continue
        if item.event == "delta" and first_at is None:
            first_at = t
        if item.event in ("done", "error"):
            end_at = t
    assert first_at is not None and end_at is not None
    assert end_at - first_at >= 2.5


def test_chat_model_immutable(fresh):
    """A chat's model is immutable once set (PATCH ignores model, send has no model override)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    r = fresh.req("PATCH", f"/chats/{cid}", json={"title": "t", "model": "gpt-4.1"})
    assert r.status_code == 200 and r.json()["model"] == "gpt-4.1-mini"
    _, done, _ = send_ok(fresh, cid, "which model?", model="gpt-4.1")
    assert done["selected_model"] == "gpt-4.1-mini" and done["effective_model"] == "gpt-4.1-mini"
    assert fresh.chat_requests()[-1]["model"] == "gpt-4.1-mini"
    assert get_chat(fresh, cid)["model"] == "gpt-4.1-mini"


def test_quota_checked_before_provider_call(qs):
    """Quota is checked before any outbound provider call."""
    ensure_quota_rows(qs, "a1")
    saver = QuotaRestorer(qs, "a1")
    try:
        set_quota(qs, "a1", "daily", "total", spent_credits_micro=100_000_000)
        qs.mock_reset()
        cid = new_chat(qs, "gpt-4.1-mini")
        r, events = qs.stream(cid, "blocked")
        assert events == []
        assert_problem(r, 429, subject="tokens", description="quota_exceeded", category="resource_exhausted")
        assert qs.responses_requests() == []
    finally:
        saver.restore()
