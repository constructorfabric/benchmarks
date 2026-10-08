"""Knowledge search (search_knowledge function tool and agentic loop)."""

import pytest

import harness
from harness import Client, ubytes, wait_for


@pytest.fixture(scope="module")
def kstack(tmp_root):
    s = harness.Stack(f"{tmp_root}/knowledge", mini_chat_overrides={})
    # add an Azure knowledge provider and enable knowledge search
    orig = harness.build_config

    def patched(home, api_port, mock_port, **kw):
        cfg = orig(home, api_port, mock_port, **kw)
        mc = cfg["gears"]["mini-chat"]["config"]
        mc["providers"]["kb"] = {
            "kind": "openai_responses",
            "host": "127.0.0.1",
            "port": mock_port,
            "use_http": True,
            "api_path": "/openai/v1/responses",
            "storage_kind": "azure",
            "api_version": "2025-03-01-preview",
        }
        mc["knowledge_search"] = {"enabled": True, "vector_store_id": "vs_kb", "provider_id": "kb",
                                  "max_calls_per_message": 1}
        return cfg

    harness.build_config = patched
    try:
        s.start()
    finally:
        harness.build_config = orig
    yield s
    s.stop()


def fc(call_id, args):
    return {"events": [
        {"type": "response.output_item.done", "item": {"type": "function_call", "call_id": call_id,
                                                         "name": "search_knowledge", "arguments": args}},
        {"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 5}}},
    ]}


def test_knowledge_loop(kstack, logs=None):
    kstack.mock_reset()
    cl = Client(kstack)
    c = cl.create_chat(model="gpt-standard")
    kstack.mock_script([
        fc("call_1", '{"query": "answer?", "top_k": 50}'),
        fc("call_2", '{"query": "again"}'),
        {"events": [{"type": "response.output_text.delta", "delta": "42"},
                    {"type": "response.completed", "response": {"usage": {"input_tokens": 9, "output_tokens": 1}}}]},
    ])
    ev = cl.send(c["id"], "what is the answer")
    assert ev[-1][0] == "done", ev
    reqs = kstack.mock_requests("/responses")
    assert len(reqs) == 3
    first = reqs[0]["json"]
    tool = [t for t in first["tools"] if t["type"] == "function"][0]
    assert tool["name"] == "search_knowledge"
    assert "search_knowledge" in first["instructions"]
    second = reqs[1]["json"]["input"]
    assert {"type": "function_call", "call_id": "call_1", "name": "search_knowledge",
            "arguments": '{"query": "answer?", "top_k": 50}'} in second
    out = [i for i in second if i.get("type") == "function_call_output"][0]
    assert "42" in out["output"]
    third = reqs[2]["json"]["input"]
    outs = [i for i in third if i.get("type") == "function_call_output"]
    assert len(outs) == 2 and "limit reached" in outs[1]["output"]
    search = kstack.mock_requests("/search")
    assert len(search) == 1
    assert search[0]["path"] == "/openai/vector_stores/vs_kb/search"
    assert search[0]["query"]["api-version"] == "2025-03-01-preview"
    assert search[0]["json"]["max_num_results"] == 5  # capped at knowledge_search.top_k
    rid = ev[0][1]["request_id"]
    t = kstack.query("select file_search_completed_count from chat_turns where request_id = ?", (ubytes(rid),))[0]
    assert t["file_search_completed_count"] == 1
    assert ev[-1][1]["usage"] == {"input_tokens": 9, "output_tokens": 1}


def test_knowledge_iteration_cap(kstack):
    kstack.mock_reset()
    cl = Client(kstack)
    c = cl.create_chat(model="gpt-standard")
    kstack.mock_script([fc(f"c{i}", '{"query": "x"}') for i in range(5)])
    ev = cl.send(c["id"], "loop forever")
    assert ev[-1][0] == "error" and ev[-1][1]["code"] == "agentic_iterations_exceeded"


def test_unexpected_tool_use(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"events": [
        {"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "x", "name": "other", "arguments": "{}"}},
        {"type": "response.completed", "response": {}},
    ]}])
    ev = client.send(c["id"], "x")
    assert ev[-1][0] == "error" and ev[-1][1]["code"] == "unexpected_tool_use"
