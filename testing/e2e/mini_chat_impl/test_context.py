"""Context assembly (acceptance: Context Assembly; Principles: context window budget)."""

import json

from mchelpers import (
    SYSTEM_PROMPT_MARKER,
    assert_problem,
    find_tool,
    nonce,
    request_input_roles_texts,
    text_blob,
)


def _chat_items(req):
    """(role, text) of user/assistant input items, in order."""
    return [(r, t) for r, t in request_input_roles_texts(req) if r in ("user", "assistant")]


def _instructions_text(req):
    body = req["json"]
    parts = [body.get("instructions") or ""]
    for r, t in request_input_roles_texts(req):
        if r in ("system", "developer"):
            parts.append(t)
    return "\n".join(parts)


# Acceptance: Context assembly — history assembled in order with the current message last
def test_history_in_order(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"first {n}").done
    assert api.stream(c["id"], f"second {n}").done
    req = mock_llm.chat_requests(contains=f"second {n}")[-1]
    assert _chat_items(req) == [("user", f"first {n}"), ("assistant", "Hello world"), ("user", f"second {n}")]


# Acceptance: Context assembly — system prompt as instructions, output cap, tool-call cap, stream flag
def test_request_parameters(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"params {n}").done
    req = mock_llm.chat_requests(contains=n)[-1]
    body = req["json"]
    assert SYSTEM_PROMPT_MARKER in _instructions_text(req)
    assert "model=prem" in _instructions_text(req)
    assert body["stream"] is True
    assert body["max_output_tokens"] == 4096  # min(catalog 4096, streaming.max_output_tokens 32768)
    assert body.get("max_tool_calls") == 4
    tiny = api.create_chat(model="std-tiny")
    m = nonce()
    assert api.stream(tiny["id"], f"tiny {m}").done
    tb = mock_llm.chat_requests(contains=m)[-1]["json"]
    assert tb["max_output_tokens"] == 1024
    assert tb["model"] == "mock-std-tiny"


# Acceptance: Context assembly — no tools without attachments or web search
def test_no_tools_by_default(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"no tools {n}").done
    req = mock_llm.chat_requests(contains=n)[-1]
    assert find_tool(req, "file_search") is None
    assert find_tool(req, "web_search") is None
    assert find_tool(req, "code_interpreter") is None


# Acceptance: Context assembly — tool guidance reflected (web search tool + guard only when sent)
def test_web_search_tool_and_guard(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"plain {n}").done
    plain = mock_llm.chat_requests(contains=f"plain {n}")[-1]
    assert api.stream(c["id"], f"with search {n}", web_search=True).done
    ws = mock_llm.chat_requests(contains=f"with search {n}")[-1]
    tool = find_tool(ws, "web_search")
    assert tool is not None
    assert tool.get("search_context_size") == "low"
    assert "web_search" in _instructions_text(ws)
    assert "web_search" not in _instructions_text(plain)
    assert api.stream(c["id"], f"explicitly off {n}", web_search=False).done
    assert find_tool(mock_llm.chat_requests(contains=f"explicitly off {n}")[-1], "web_search") is None


# Acceptance: Context assembly — recent history bounded by context.recent_messages_limit (10)
def test_recent_messages_limit(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    for i in range(6):
        assert api.stream(c["id"], f"turn {i} {n}").done
    assert api.stream(c["id"], f"turn 6 {n}").done
    req = mock_llm.chat_requests(contains=f"turn 6 {n}")[-1]
    items = _chat_items(req)
    history, current = items[:-1], items[-1]
    assert current == ("user", f"turn 6 {n}")
    # K = 10 recent messages. DESIGN lists the current message separately from the recent
    # messages; if an implementation counts it inside K, whole-turn trimming leaves 8.
    assert len(history) in (8, 10), history
    assert history[0][0] == "user", "history starts with a question"
    assert all(f"turn 0 {n}" != t for _, t in history), "the oldest turn is beyond the limit"
    expected_tail = []
    for i in range(6 - len(history) // 2, 6):
        expected_tail += [("user", f"turn {i} {n}"), ("assistant", "Hello world")]
    assert history == expected_tail


# Acceptance: Context assembly / Principles — truncation drops oldest whole turns within the budget
def test_budget_truncation_drops_oldest_turns(api, mock_llm):
    c = api.create_chat(model="std-tiny")
    a_marker, b_marker = "MARK-A-" + nonce(), "MARK-B-" + nonce()
    # each ~6 KB message is ~1650-1760 estimated tokens; two of them exceed the 2972-token budget
    assert api.stream(c["id"], text_blob(6000, a_marker)).done
    assert api.stream(c["id"], text_blob(6000, b_marker)).done
    req = mock_llm.chat_requests(contains=b_marker)[-1]
    items = _chat_items(req)
    body_text = json.dumps(req["json"])
    assert a_marker not in body_text, "the oldest turn must be dropped"
    assert items[-1][0] == "user" and b_marker in items[-1][1]
    non_summary = [it for it in items if "summarized" not in it[1]]
    assert non_summary[0][0] == "user", "an answer is never sent without its question"


# Acceptance: Principles — context budget enforced for the input message (INPUT_TOO_LONG)
def test_input_too_long(api, mock_llm, db):
    c = api.create_chat(model="std-tiny")
    marker = "TOO-LONG-" + nonce()
    s = api.stream(c["id"], text_blob(20000, marker))
    assert not s.is_sse
    assert_problem(api.as_response(s), 400, "out_of_range", field_reason="INPUT_TOO_LONG")
    assert mock_llm.chat_requests(contains=marker) == []
    assert db.turns(c["id"]) == []


# Acceptance: Principles — context budget enforced for the assembled request (CONTEXT_BUDGET_EXCEEDED)
def test_context_budget_exceeded(api, mock_llm, db):
    c = api.create_chat(model="std-budget")  # input budget min(3000, 4096-3000) - overhead < 1000
    marker = "BUDGET-" + nonce()
    s = api.stream(c["id"], text_blob(6000, marker))  # ~1760 tokens: below max_input_tokens 3000
    assert not s.is_sse
    assert_problem(api.as_response(s), 400, "out_of_range", field_reason="CONTEXT_BUDGET_EXCEEDED")
    assert mock_llm.chat_requests(contains=marker) == []
    # a short message fits
    assert api.stream(c["id"], "short " + nonce()).done


# Acceptance: Context assembly — deterministic: a retry assembles the same context
def test_retry_assembles_same_context(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"one {n}").done
    s = api.stream(c["id"], f"two {n}")
    assert s.done
    assert api.retry(c["id"], s.request_id).done
    reqs = mock_llm.chat_requests(contains=f"two {n}")
    assert len(reqs) == 2
    a, b = reqs[-2]["json"], reqs[-1]["json"]
    assert a["input"] == b["input"]
    assert a.get("instructions") == b.get("instructions")
    assert a.get("tools") == b.get("tools")
