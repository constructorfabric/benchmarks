"""Knowledge search: `search_knowledge` function tool and the agentic loop."""

from __future__ import annotations

import json
import uuid

import pytest

from conftest import _fresh, assert_problem, ok_stream, ub, wait_until


@pytest.fixture
def kb(servers):
    srv = servers("knowledge")
    return srv, _fresh(srv)


def call(call_id: str, query: str, name: str = "search_knowledge", top_k: int | None = None) -> dict:
    args = {"query": query}
    if top_k is not None:
        args["top_k"] = top_k
    return {
        "chunks": [],
        "events_after": [
            {
                "type": "response.output_item.done",
                "item": {"type": "function_call", "call_id": call_id, "name": name, "arguments": json.dumps(args)},
            }
        ],
    }


def searches(mock):
    return [r for r in mock.requests("POST") if r["path"].endswith("/vector_stores/vs_kb_1/search")]


def test_tool_offered_with_guard(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    ok_stream(api.send(chat["id"], "hi"))
    body = mock.chat_requests(chat["id"])[0]["json"]
    fn = [t for t in body["tools"] if t.get("type") == "function"]
    assert len(fn) == 1 and fn[0]["name"] == "search_knowledge"
    assert "query" in fn[0]["parameters"]["properties"]
    assert "search_knowledge" in body["instructions"]
    assert not [t for t in body["tools"] if t["type"] == "file_search"]


def test_agentic_loop_runs_retrieval(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.script(call("call_1", "vacation days", top_k=50), {"text": "You get 25 days."})
    r = ok_stream(api.send(chat["id"], "How many vacation days?", request_id=rid))
    assert r.text == "You get 25 days."
    assert "tool" not in r.names
    reqs = mock.chat_requests(chat["id"])
    assert len(reqs) == 2
    s = searches(mock)
    assert len(s) == 1
    assert s[0]["json"]["query"] == "vacation days"
    assert s[0]["json"]["max_num_results"] == 4  # capped at knowledge_search.top_k
    assert s[0]["query"].get("api-version") == "2025-04-01-preview"
    second = reqs[1]["json"]["input"]
    fc = [i for i in second if i.get("type") == "function_call"]
    out = [i for i in second if i.get("type") == "function_call_output"]
    assert fc and fc[0]["call_id"] == "call_1" and fc[0]["name"] == "search_knowledge"
    assert out and out[0]["call_id"] == "call_1"
    assert "Employees get 25 vacation days per year."[:20] in out[0]["output"]
    assert "Employees get 25 vacation days per year." not in out[0]["output"]  # trimmed to max_chunk_chars
    assert second[0]["role"] == "user"
    row = srv.query("SELECT file_search_completed_count, state FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["state"] == "completed"
    assert row["file_search_completed_count"] == 1
    ev = wait_until(lambda: srv.usage_events(request_id=rid), msg="usage")[0]
    assert ev["file_search_calls"] == 1
    msgs = api.messages(chat["id"])
    assert msgs[1]["content"] == "You get 25 days."


def test_search_limit_then_answer(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    mock.script(*[call(f"c{i}", f"q{i}") for i in range(4)], {"text": "final"})
    r = ok_stream(api.send(chat["id"], "dig"))
    assert r.text == "final"
    assert len(searches(mock)) == 3
    last = mock.chat_requests(chat["id"])[-1]["json"]["input"]
    outs = [i for i in last if i.get("type") == "function_call_output"]
    assert len(outs) == 4
    assert "limit" in outs[-1]["output"].lower()


def test_iteration_cap(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.script(*[call(f"c{i}", f"q{i}") for i in range(8)])
    r = api.send(chat["id"], "loop forever", request_id=rid)
    assert r.names[-1] == "error"
    assert r.first("error")["code"] == "agentic_iterations_exceeded"
    assert len(mock.chat_requests(chat["id"])) == 5  # max_calls_per_message + 2
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error" and t["error_code"] == "agentic_iterations_exceeded"


def test_unknown_function_is_unexpected(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    mock.script(call("c1", "x", name="delete_everything"))
    r = api.send(chat["id"], "x")
    assert r.first("error")["code"] == "unexpected_tool_use"


def test_failed_retrieval_is_reported_to_model(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.config(vs_search_status=500)
    mock.script(call("c1", "x"), {"text": "sorry"})
    ok_stream(api.send(chat["id"], "x", request_id=rid))
    out = [i for i in mock.chat_requests(chat["id"])[-1]["json"]["input"] if i.get("type") == "function_call_output"]
    assert out and "fail" in out[0]["output"].lower()
    row = srv.query("SELECT file_search_completed_count FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["file_search_completed_count"] == 0
    ev = wait_until(lambda: srv.usage_events(request_id=rid), msg="usage")[0]
    assert ev["file_search_calls"] == 1


def test_file_search_wins_over_knowledge_search(kb, mock):
    srv, api = kb
    chat = api.create_chat(model="gpt-standard")
    assert api.upload(chat["id"], b"doc", "d.txt", "text/plain").status_code == 201
    ok_stream(api.send(chat["id"], "q"))
    body = mock.chat_requests(chat["id"])[-1]["json"]
    types = [t["type"] for t in body["tools"]]
    assert "file_search" in types and "function" not in types
    assert "search_knowledge" not in body["instructions"]


def test_function_call_without_knowledge_search(api, mock):
    chat = api.create_chat()
    mock.script(call("c1", "x"))
    r = api.send(chat["id"], "x")
    assert r.names[-1] == "error"
    assert r.first("error")["code"] == "unexpected_tool_use"
