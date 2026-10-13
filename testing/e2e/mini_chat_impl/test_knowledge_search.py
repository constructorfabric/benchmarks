"""Knowledge search (`search_knowledge` function tool over a configured vector store)."""

import json

from conftest import KB_VECTOR_STORE
from mchelpers import assert_sse_grammar, nonce

GUARD_FRAGMENT = "search_knowledge"


def _function_tool(req: dict, name: str):
    for t in (req.get("json") or {}).get("tools") or []:
        if isinstance(t, dict) and t.get("type") == "function" and t.get("name") == name:
            return t
    return None


def _items_of_type(req: dict, typ: str) -> list:
    return [i for i in (req.get("json") or {}).get("input") or [] if isinstance(i, dict) and i.get("type") == typ]


def _searches(mock_llm, since: int) -> list:
    return [r for r in mock_llm.requests(path_contains="/search", method="POST", since_seq=since) if KB_VECTOR_STORE in r["path"]]


# Knowledge search: tool + guard offered, search executed via the provider, result fed back, answer streamed
def test_knowledge_search_loop(kb_api_for, kb_server, mock_llm):
    api = kb_api_for("tok-a")
    c = api.create_chat()
    n = nonce()
    since = mock_llm.last_seq()
    s = api.stream(c["id"], f"what does the handbook say [[ksearch]] {n}")
    assert_sse_grammar(s)
    assert s.done, s.terminal
    reqs = mock_llm.chat_requests(contains=n, since_seq=since)
    assert len(reqs) == 2
    first, second = reqs
    tool = _function_tool(first, "search_knowledge")
    assert tool is not None and "query" in json.dumps(tool.get("parameters"))
    assert GUARD_FRAGMENT in (first["json"].get("instructions") or "")
    assert _items_of_type(first, "function_call_output") == []
    calls = _items_of_type(second, "function_call")
    outputs = _items_of_type(second, "function_call_output")
    assert len(calls) == 1 and len(outputs) == 1
    assert calls[0]["call_id"] == outputs[0]["call_id"]
    out = json.loads(outputs[0]["output"])
    assert out["results"] and out["results"][0].startswith("KB-CHUNK-0")
    assert len(out["results"]) == 3  # clamped to knowledge_search.top_k
    searches = _searches(mock_llm, since)
    assert len(searches) == 1
    assert searches[0]["query"].get("api-version") == ["2025-04-01-preview"]
    assert searches[0]["json"]["query"] == "mock knowledge query"
    row = kb_server.db.turn_row(c["id"], s.request_id)
    assert row["state"] == "completed"
    assert row["file_search_completed_count"] == 1
    msgs = api.messages(c["id"])
    assert msgs[-1]["role"] == "assistant" and msgs[-1]["content"]


# Knowledge search: per-message call limit answered with a limit message instead of a search
def test_knowledge_search_call_limit(kb_api_for, kb_server, mock_llm):
    api = kb_api_for("tok-a")
    c = api.create_chat()
    n = nonce()
    since = mock_llm.last_seq()
    s = api.stream(c["id"], f"dig deep [[ksearch:3]] {n}")
    assert s.done, s.terminal
    assert len(_searches(mock_llm, since)) == 2  # max_calls_per_message = 2
    last = mock_llm.chat_requests(contains=n, since_seq=since)[-1]
    outputs = _items_of_type(last, "function_call_output")
    assert len(outputs) == 3
    assert "limit" in json.loads(outputs[-1]["output"])["error"]
    assert kb_server.db.turn_row(c["id"], s.request_id)["file_search_completed_count"] == 2


# Knowledge search: runaway tool loop is stopped with agentic_iterations_exceeded
def test_knowledge_search_iteration_cap(kb_api_for, kb_server):
    api = kb_api_for("tok-a")
    c = api.create_chat()
    s = api.stream(c["id"], "loop forever [[ksearch:50]] " + nonce())
    assert_sse_grammar(s)
    assert s.error["code"] == "agentic_iterations_exceeded"
    t = api.wait_turn_state(c["id"], s.request_id, {"error"})
    assert t["error_code"] == "agentic_iterations_exceeded"


# Knowledge search: provider search failure degrades to an error output, the turn still completes
def test_knowledge_search_failure_degrades(kb_api_for, kb_server, mock_llm):
    mock_llm.configure(kb_search_fail=True)
    api = kb_api_for("tok-a")
    c = api.create_chat()
    n = nonce()
    since = mock_llm.last_seq()
    s = api.stream(c["id"], f"handbook [[ksearch]] {n}")
    assert s.done, s.terminal
    last = mock_llm.chat_requests(contains=n, since_seq=since)[-1]
    out = json.loads(_items_of_type(last, "function_call_output")[0]["output"])
    assert "error" in out
    assert kb_server.db.turn_row(c["id"], s.request_id)["file_search_completed_count"] == 0


# Knowledge search disabled (default config): no tool offered, no guard
def test_knowledge_search_disabled_by_default(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"plain {n}").done
    req = mock_llm.chat_requests(contains=n)[-1]
    assert _function_tool(req, "search_knowledge") is None
    assert GUARD_FRAGMENT not in (req["json"].get("instructions") or "")


# A function call for a tool that was never offered fails the turn with unexpected_tool_use
def test_unexpected_tool_use(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "do it [[badtool]] " + nonce())
    assert_sse_grammar(s)
    assert s.error["code"] == "unexpected_tool_use"
    assert s.of("done") == []
    t = api.wait_turn_state(c["id"], s.request_id, {"error"})
    assert t["error_code"] == "unexpected_tool_use"
    assert db.turn_row(c["id"], s.request_id)["state"] == "failed"
