"""Context assembly: system prompt, history, truncation within budget, tools and guards.

Acceptance criteria covered:
* Context Assembly — "System prompt, thread summary, and recent history are assembled and truncated deterministically within budget"
* Context Assembly — "Tool availability and guidance are reflected correctly in the assembled request"
* Principles — "Context window budget enforced, for both the input message and the full assembled request"
"""

from __future__ import annotations

from helpers import (
    SYSTEM_PROMPT,
    assert_problem,
    estimate_text_tokens,
    input_pairs,
    message_rows,
    new_chat,
    send_ok,
    stream_script,
    tool,
    turn_rows,
    upload_ok,
)


def test_system_prompt_is_instructions(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "hi")
    b = fresh.chat_requests()[-1]
    assert b["instructions"].strip() == SYSTEM_PROMPT
    roles = [r for r, _ in input_pairs(b)]
    assert "system" not in roles and "developer" not in roles, "the system prompt is not sent as a message"


def test_custom_system_prompt_from_catalog(mc_factory):
    from harness import ServerOptions, catalog_entry, default_catalog

    cat = default_catalog()
    cat.append(catalog_entry("pirate", "Standard", system_prompt="You are a pirate. Answer like one."))
    srv = mc_factory("pirate", ServerOptions(catalog=cat))
    srv.mock_reset()
    cid = new_chat(srv, "pirate")
    send_ok(srv, cid, "ahoy")
    assert srv.chat_requests()[-1]["instructions"].strip() == "You are a pirate. Answer like one."


def test_history_included_in_order(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("first answer"))
    send_ok(fresh, cid, "first question")
    send_ok(fresh, cid, "second question")
    pairs = input_pairs(fresh.chat_requests()[-1])
    assert pairs == [("user", "first question"), ("assistant", "first answer"), ("user", "second question")]


def test_recent_messages_limit(fresh):
    """Only the newest context.recent_messages_limit (10) history messages are sent, chronologically."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    for i in range(6):
        fresh.mock_script(stream_script(f"answer {i}"))
        send_ok(fresh, cid, f"question {i}")
    send_ok(fresh, cid, "final")
    pairs = input_pairs(fresh.chat_requests()[-1])
    assert len(pairs) == 11
    assert pairs[0] == ("user", "question 1"), "oldest turn dropped, the kept range starts with a user message"
    assert pairs[-2] == ("assistant", "answer 5")
    assert pairs[-1] == ("user", "final")


def test_deterministic_context_for_same_state(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "a")
    st, _, _ = send_ok(fresh, cid, "b")
    first_ctx = input_pairs(fresh.chat_requests()[-1])
    # Retry rebuilds the context for the same history.
    r, events = fresh.sse("POST", f"/chats/{cid}/turns/{st['request_id']}/retry")
    assert events[-1].event == "done"
    assert input_pairs(fresh.chat_requests()[-1]) == first_ctx


def test_truncation_drops_oldest_whole_turns(fresh):
    """tiny-ctx (budget ≈ 2972 tokens): history over budget → the oldest turn is omitted from the input."""
    cid = new_chat(fresh, "tiny-ctx")
    big = lambda tag: tag + ("." * 3960)  # ≈ 1200 estimated tokens each
    assert 1150 < estimate_text_tokens(big("AAA")) < 1250
    send_ok(fresh, cid, big("AAA"))
    send_ok(fresh, cid, big("BBB"))
    send_ok(fresh, cid, big("CCC"))
    b = fresh.chat_requests()[-1]
    pairs = input_pairs(b)
    texts = [t for _, t in pairs]
    assert texts[-1].startswith("CCC")
    assert any(t.startswith("BBB") for t in texts), "the newest history turn fits"
    assert not any(t.startswith("AAA") for t in texts), "the oldest turn is truncated"
    roles = [r for r, t in pairs if not t.startswith("This conversation has earlier messages")]
    assert roles[0] == "user", "the kept range never starts with an assistant message"
    assert b["max_output_tokens"] == 1024


def test_input_too_long_on_small_model(fresh):
    cid = new_chat(fresh, "tiny-ctx")
    r, events = fresh.stream(cid, "x" * 13000)
    assert events == []
    assert_problem(r, 400, field_reason="INPUT_TOO_LONG")
    assert fresh.chat_requests() == []


def test_context_budget_exceeded_for_mandatory_items(lim):
    """budget-test: input budget ≈ 900 tokens and no separate max_input_tokens → CONTEXT_BUDGET_EXCEEDED."""
    cid = new_chat(lim, "budget-test")
    r, events = lim.stream(cid, "y" * 8000)
    assert events == []
    assert_problem(r, 400, field_reason="CONTEXT_BUDGET_EXCEEDED")
    assert lim.chat_requests() == []
    assert message_rows(lim, cid) == [] and turn_rows(lim, cid) == []
    # A small message fits.
    send_ok(lim, cid, "small")


def test_context_budget_exceeded_when_output_reserve_fills_window(lim):
    cid = new_chat(lim, "no-room")
    r, _ = lim.stream(cid, "hi")
    assert_problem(r, 400, field_reason="CONTEXT_BUDGET_EXCEEDED")
    assert lim.chat_requests() == []


def test_tools_and_guards(fresh):
    """file_search only with a ready document; web_search only when requested; guards appended per tool."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "no tools")
    base = fresh.chat_requests()[-1]
    assert not base.get("tools")
    upload_ok(fresh, cid, "kb.pdf")
    fresh.mock_reset()
    send_ok(fresh, cid, "file search only")
    fs_only = fresh.chat_requests()[-1]
    assert tool(fs_only, "file_search") is not None and tool(fs_only, "web_search") is None
    fs_guard = fs_only["instructions"][len(SYSTEM_PROMPT):].strip()
    assert fs_guard, "file search guard appended"
    send_ok(fresh, cid, "both", web_search={"enabled": True})
    both = fresh.chat_requests()[-1]
    assert tool(both, "file_search") is not None and tool(both, "web_search") is not None
    assert both["instructions"].startswith(SYSTEM_PROMPT)
    assert fs_guard in both["instructions"]
    assert len(both["instructions"]) > len(fs_only["instructions"]), "web search guard appended too"
