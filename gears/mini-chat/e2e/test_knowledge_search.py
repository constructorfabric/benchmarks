"""Knowledge search (DESIGN §4 "Knowledge Search", ADR-0008) and unexpected function calls.

A dedicated server enables ``knowledge_search`` with ``provider_id = "openai"``; that entry is
switched to ``storage_kind = "azure"`` with an ``api_version`` (the knowledge retriever calls
``POST /{alias}/openai/vector_stores/{id}/search?api-version=...`` through OAGW, which the mock
serves). The chat itself still uses the Responses API path ``/v1/responses``.
"""

from __future__ import annotations

import json

import pytest

from harness import ServerOptions
from helpers import (
    assert_error_stream,
    assert_ok_stream,
    function_call_script,
    input_items,
    message_rows,
    new_chat,
    stream_script,
    text_of,
    tool,
    turn_row,
    upload_ok,
    wait_turn_state,
    wait_usage_event,
)

KB_STORE = "vs_kb_e2e"
API_VERSION = "2025-04-01-preview"
GUARD = "Search the company knowledge base with search_knowledge when needed."

KS_OPTS = ServerOptions(
    gear_overrides={
        "knowledge_search": {
            "enabled": True,
            "vector_store_id": KB_STORE,
            "provider_id": "openai",
            "max_calls_per_message": 2,
            "top_k": 3,
            "max_chunk_chars": 12,
            "guard": GUARD,
        },
        "providers": {"openai": {"storage_kind": "azure", "api_version": API_VERSION}},
    }
)

CHUNK = {
    "file_id": "assistant-kb1",
    "filename": "handbook.md",
    "score": 0.91,
    "attributes": {},
    "content": [{"type": "text", "text": "Vacation is 25 days per year."}],
}


@pytest.fixture(scope="session")
def knowledge_server(mc_factory):
    return mc_factory("knowledge", KS_OPTS)


@pytest.fixture
def kb(knowledge_server):
    knowledge_server.mock_reset()
    return knowledge_server


def search_requests(srv) -> list[dict]:
    return srv.mock_requests("/search", "POST")


def function_outputs(body: dict) -> list[dict]:
    return [i for i in input_items(body) if i.get("type") == "function_call_output"]


def function_calls(body: dict) -> list[dict]:
    return [i for i in input_items(body) if i.get("type") == "function_call"]


def test_agentic_loop_retrieves_and_answers(kb):
    kb.mock_config(vector_store_search_results=[CHUNK])
    cid = new_chat(kb, "gpt-4.1-mini")
    kb.mock_script(
        function_call_script("call_1", arguments='{"query": "vacation days", "top_k": 50}', usage={"input_tokens": 20, "output_tokens": 5}),
        stream_script("You get ", "25 days.", usage={"input_tokens": 300, "output_tokens": 40}),
    )
    r, events = kb.stream(cid, "How many vacation days do I get?")
    done = assert_ok_stream(r, events)
    assert text_of(events) == "You get 25 days."
    # The Responses adapter emits no tool events for the function tool.
    assert not [e for e in events if e.event == "tool"]
    # Only the final iteration's usage is reported / settled.
    assert done.data["usage"] == {"input_tokens": 300, "output_tokens": 40}

    reqs = kb.chat_requests()
    assert len(reqs) == 2
    first, second = reqs
    fn = tool(first, "function")
    assert fn is not None, first.get("tools")
    assert fn["name"] == "search_knowledge"
    assert fn["parameters"]["properties"]["query"]["type"] == "string"
    assert fn["parameters"]["properties"]["top_k"]["type"] == "integer"
    assert first["instructions"].endswith(GUARD)
    assert "max_tool_calls" not in first, "only built-in tools carry max_tool_calls"
    assert first["metadata"]["feature"] == "search_knowledge"
    assert not function_outputs(first)

    calls = function_calls(second)
    assert calls == [{"type": "function_call", "call_id": "call_1", "name": "search_knowledge",
                      "arguments": '{"query": "vacation days", "top_k": 50}'}]
    outs = function_outputs(second)
    assert len(outs) == 1 and outs[0]["call_id"] == "call_1"
    result = json.loads(outs[0]["output"])
    assert result["results"][0]["text"] == "Vacation is ", "chunk trimmed to max_chunk_chars"
    assert result["results"][0]["filename"] == "handbook.md"
    # The function call items follow the current user message.
    items = input_items(second)
    assert items[-2]["type"] == "function_call" and items[-1]["type"] == "function_call_output"
    assert items[-3].get("role") == "user"

    searches = search_requests(kb)
    assert len(searches) == 1
    s = searches[0]
    assert s["path"] == f"/openai/vector_stores/{KB_STORE}/search"
    assert s["query"].get("api-version") == API_VERSION
    assert s["body"] == {"query": "vacation days", "max_num_results": 3}, "top_k capped at knowledge_search.top_k"

    rid = events[0].data["request_id"]
    t = wait_turn_state(kb, cid, rid, ["done"])
    row = turn_row(kb, cid, rid)
    assert row["file_search_completed_count"] == 1
    ev = wait_usage_event(kb, rid)
    assert ev["file_search_calls"] == 1
    assert ev["usage"]["input_tokens"] == 300 and ev["usage"]["output_tokens"] == 40
    assistant = [m for m in message_rows(kb, cid) if m["role"] == "assistant"]
    assert assistant[-1]["content"] == "You get 25 days."
    assert t


def test_search_limit_then_iteration_cap(kb):
    """max_calls_per_message = 2: two retrievals, then "search limit reached"; the loop is capped at 4 requests."""
    kb.mock_config(vector_store_search_results=[CHUNK])
    cid = new_chat(kb, "gpt-4.1-mini")
    kb.mock_script(*[function_call_script(f"call_{i}", usage={"input_tokens": 10 + i, "output_tokens": 1}) for i in range(4)])
    r, events = kb.stream(cid, "keep searching")
    err = assert_error_stream(r, events, "agentic_iterations_exceeded")
    assert err["message"]
    reqs = kb.chat_requests()
    assert len(reqs) == 4
    outs = function_outputs(reqs[3])
    assert len(outs) == 3
    assert "results" in json.loads(outs[0]["output"])
    assert "results" in json.loads(outs[1]["output"])
    assert outs[2]["output"].startswith("Search limit reached")
    assert len(search_requests(kb)) == 2

    rid = events[0].data["request_id"]
    t = wait_turn_state(kb, cid, rid, ["error"])
    assert t["error_code"] == "agentic_iterations_exceeded"
    assert turn_row(kb, cid, rid)["file_search_completed_count"] == 2
    ev = wait_usage_event(kb, rid)
    assert ev["file_search_calls"] == 2
    assert ev["usage"]["input_tokens"] == 13, "the last iteration's usage is settled"


def test_failed_retrieval_is_returned_to_the_model(kb):
    kb.mock_config(vector_store_search_status=500)
    cid = new_chat(kb, "gpt-4.1-mini")
    kb.mock_script(function_call_script("call_x"), stream_script("No knowledge available."))
    r, events = kb.stream(cid, "question")
    assert_ok_stream(r, events)
    outs = function_outputs(kb.chat_requests()[1])
    assert "failed" in outs[0]["output"].lower()
    rid = events[0].data["request_id"]
    wait_turn_state(kb, cid, rid, ["done"])
    assert turn_row(kb, cid, rid)["file_search_completed_count"] == 0
    assert wait_usage_event(kb, rid)["file_search_calls"] == 1


def test_unknown_function_is_unexpected_tool_use(kb):
    cid = new_chat(kb, "gpt-4.1-mini")
    kb.mock_script(function_call_script("call_bad", name="delete_everything", arguments="{}"))
    r, events = kb.stream(cid, "do it")
    assert_error_stream(r, events, "unexpected_tool_use")
    assert len(kb.chat_requests()) == 1
    assert search_requests(kb) == []
    rid = events[0].data["request_id"]
    t = wait_turn_state(kb, cid, rid, ["error"])
    assert t["error_code"] == "unexpected_tool_use"


def test_file_search_wins_over_knowledge_search(kb):
    cid = new_chat(kb, "gpt-4.1-mini")
    upload_ok(kb, cid, "doc.pdf")
    kb.mock_reset()
    r, events = kb.stream(cid, "from my document")
    assert_ok_stream(r, events)
    req = kb.chat_requests()[-1]
    assert tool(req, "file_search") is not None
    assert tool(req, "function") is None, "search_knowledge is never sent together with file_search"
    assert GUARD not in req.get("instructions", "")
