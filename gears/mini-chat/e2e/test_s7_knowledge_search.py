"""S7 knowledge search: the ``search_knowledge`` function tool and its agentic loop.

Contract sources (gears/mini-chat/docs/DESIGN.md):

* §4 "Knowledge Search": ``knowledge_search.enabled`` with
  ``vector_store_id`` and ``provider_id`` (an ``openai_responses`` /
  ``anthropic_messages`` entry with an upstream alias and ``api_version``);
  the knowledge retriever calls ``POST /{alias}/openai/vector_stores/
  {vector_store_id}/search?api-version={ver}`` through OAGW; the
  ``search_knowledge`` function tool is offered with ``knowledge_search.guard``
  appended to the system prompt; the model supplies ``query`` and optionally
  ``top_k`` (capped at ``knowledge_search.top_k``); every chunk is trimmed to
  ``knowledge_search.max_chunk_chars`` and returned as a
  ``function_call_output``; each call ends the provider request, the gear runs
  the retrieval, appends the call and its output to the input and issues the
  next request; at most ``max_calls_per_message`` retrievals per message,
  further calls get a "search limit reached" output; the loop is hard-capped
  at ``max_calls_per_message + 2`` iterations (SSE ``error``
  ``agentic_iterations_exceeded``, turn ``failed``); any other function name
  ends the turn with ``unexpected_tool_use``; ``file_search`` wins when the
  chat has ready documents (the two tools are never sent together);
  successful retrievals count in ``chat_turns.file_search_completed_count``,
  the usage event's ``file_search_calls`` counts ``search_knowledge`` calls;
* §3.3 ``event: tool``: the OpenAI Responses adapter emits no tool events for
  the knowledge-search function tool;
* "Streaming error codes": ``agentic_iterations_exceeded``,
  ``unexpected_tool_use`` ("any tool use when knowledge search is off");
* §3.5 / B.1: Azure RAG routes (``/openai`` prefix, ``api-version`` query).

The module server enables knowledge search with small limits
(``max_calls_per_message = 2``, ``top_k = 3``, ``max_chunk_chars = 40``) and a
custom guard; the knowledge provider is an Azure-storage ``openai_responses``
entry pointing at the mock under its own OAGW alias and credential header.
Chat models stay on the default ``openai`` provider (OpenAI Responses).
"""

from __future__ import annotations

import json
import threading
import time
from typing import Any, Iterator

import pytest

from conftest import APIKEY_AUTH_PLUGIN, OPENAI_SECRET_KEY, OPENAI_SECRET_VALUE, create_server
from mc_helpers import db_uuid, new_uuid, wait_until
from mc_seed import outbox_payloads_for_chat

CHATS = "/mini-chat/v1/chats"
MODEL = "gpt-4.1"
TOOL_NAME = "search_knowledge"

KB_PROVIDER = "kb-azure"
KB_ALIAS = "mock-kb-azure"
KB_API_VERSION = "2025-04-01-preview"
KB_VECTOR_STORE = "vs_KnowledgeBase000000000001"
KB_GUARD = "KB-GUARD-S7: consult search_knowledge for company facts before answering."
MAX_CALLS = 2
TOP_K = 3
MAX_CHUNK_CHARS = 40
#: The hard cap of the agentic loop (provider requests per turn).
MAX_ITERATIONS = MAX_CALLS + 2

TXT_MIME = "text/plain"


# --------------------------------------------------------------------------
# Server
# --------------------------------------------------------------------------


def knowledge_overrides(mock_port: int) -> dict[str, Any]:
    return {
        "gears": {
            "mini-chat": {
                "config": {
                    "providers": {
                        KB_PROVIDER: {
                            "kind": "openai_responses",
                            "storage_kind": "azure",
                            "api_version": KB_API_VERSION,
                            "host": "127.0.0.1",
                            "port": mock_port,
                            "use_http": True,
                            "upstream_alias": KB_ALIAS,
                            "api_path": "/openai/v1/responses",
                            "auth_plugin_type": APIKEY_AUTH_PLUGIN,
                            "auth_config": {"header": "api-key", "secret_ref": f"cred://{OPENAI_SECRET_KEY}"},
                        }
                    },
                    "knowledge_search": {
                        "enabled": True,
                        "vector_store_id": KB_VECTOR_STORE,
                        "provider_id": KB_PROVIDER,
                        "max_calls_per_message": MAX_CALLS,
                        "top_k": TOP_K,
                        "max_chunk_chars": MAX_CHUNK_CHARS,
                        "guard": KB_GUARD,
                    },
                }
            }
        }
    }


@pytest.fixture(scope="module")
def _s7_kb_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


@pytest.fixture(scope="module")
def kb(mock, _s7_kb_servers):
    """Server with knowledge search enabled."""
    server = create_server(mock, knowledge_overrides(mock.port), name="s7-knowledge")
    _s7_kb_servers.append(server)
    server.start()
    return server.wait_ready()


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def chunk_head(index: int) -> str:
    """The first ``MAX_CHUNK_CHARS`` characters of knowledge chunk ``index``."""
    head = f"KBCHUNK-{index}-"
    return head + "k" * (MAX_CHUNK_CHARS - len(head))


def knowledge_chunks(count: int = 5) -> list[dict[str, Any]]:
    """Chunks longer than ``max_chunk_chars``: the ``TAILCUT`` part must be trimmed."""
    return [
        {"text": chunk_head(i) + f"TAILCUT{i}" + "y" * 30, "filename": f"handbook-{i}.md"}
        for i in range(1, count + 1)
    ]


def _new_chat(server, user: str = "A1", model: str = MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S7 knowledge", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _send(server, chat_id: str, content: str, user: str = "A1"):
    request_id = new_uuid()
    result = server.stream(
        "POST", f"{CHATS}/{chat_id}/messages:stream", user, json={"content": content, "request_id": request_id}
    )
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert result.terminal is not None, names
    return result, request_id


def _turn(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", (db_uuid(chat_id), db_uuid(request_id))
    )
    assert len(rows) == 1, rows
    return rows[0]


def _stream_requests(mock) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("responses") if r["stream"]]


def _searches(mock) -> list[dict[str, Any]]:
    return [
        r for r in mock.requests_for("vector_store_search") if r["path"].endswith(f"/vector_stores/{KB_VECTOR_STORE}/search")
    ]


def _function_tools(body: dict[str, Any], name: str = TOOL_NAME) -> list[dict[str, Any]]:
    found = []
    for tool in body.get("tools") or []:
        if not isinstance(tool, dict) or tool.get("type") != "function":
            continue
        tool_name = tool.get("name") or (tool.get("function") or {}).get("name")
        if tool_name == name:
            found.append(tool)
    return found


def _tool_types(body: dict[str, Any]) -> list[str]:
    return [str(t.get("type")) for t in body.get("tools") or [] if isinstance(t, dict)]


def _text_of(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(p.get("text", "")) for p in content if isinstance(p, dict))
    return ""


def _system_instructions(body: dict[str, Any]) -> str:
    parts = [str(body.get("instructions") or "")]
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("role") in ("system", "developer"):
            parts.append(_text_of(item.get("content")))
    return "\n".join(parts)


def _input_items(body: dict[str, Any], item_type: str) -> list[dict[str, Any]]:
    return [i for i in body.get("input") or [] if isinstance(i, dict) and i.get("type") == item_type]


def _output_text(item: dict[str, Any]) -> str:
    output = item.get("output")
    return output if isinstance(output, str) else json.dumps(output)


def _call(call_id: str, query: str = "quarterly revenue", top_k: int | None = None, name: str = TOOL_NAME):
    arguments: dict[str, Any] = {"query": query}
    if top_k is not None:
        arguments["top_k"] = top_k
    return {"function_calls": [{"name": name, "arguments": arguments, "call_id": call_id}]}


class OutboxRecorder:
    """Polls the outbox bodies of one chat in a background thread (bodies are vacuumed after ack)."""

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="s7-outbox-recorder", daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.poll()
            time.sleep(self._interval)

    def poll(self) -> None:
        try:
            payloads = outbox_payloads_for_chat(self._server, self._chat_id)
        except Exception:  # noqa: BLE001 - transient SQLite contention, polled again
            return
        with self._lock:
            for payload in payloads:
                self._seen.setdefault(json.dumps(payload, sort_keys=True), payload)

    def __enter__(self) -> "OutboxRecorder":
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def wait_usage_event(self, request_id: str) -> dict[str, Any]:
        def found() -> list[dict[str, Any]]:
            self.poll()
            with self._lock:
                return [
                    p
                    for p in self._seen.values()
                    if str(p.get("request_id", "")).lower() == request_id.lower() and "dedupe_key" in p
                ]

        events = wait_until(found, timeout=15, interval=0.05, description=f"usage event of {request_id}")
        assert len(events) == 1, events
        return events[0]


# --------------------------------------------------------------------------
# Tool and guard inclusion
# --------------------------------------------------------------------------


def test_request_offers_search_knowledge_tool_and_guard(kb, mock):
    chat_id = _new_chat(kb)
    result, _ = _send(kb, chat_id, "What is our travel policy?")
    assert result.terminal[0] == "done", result.events[-1:]

    body = _stream_requests(mock)[-1]["json"]
    tools = _function_tools(body)
    assert len(tools) == 1, body.get("tools")
    parameters = tools[0].get("parameters") or (tools[0].get("function") or {}).get("parameters") or {}
    assert {"query", "top_k"} <= set(parameters.get("properties") or {}), parameters
    assert "query" in (parameters.get("required") or []), parameters
    assert KB_GUARD in _system_instructions(body)
    assert "file_search" not in _tool_types(body), "no ready documents: no file_search"
    assert _searches(mock) == [], "no retrieval without a search_knowledge call"


def test_ready_documents_make_file_search_win_over_knowledge_search(kb, mock):
    chat_id = _new_chat(kb)
    resp = kb.post(
        f"{CHATS}/{chat_id}/attachments",
        "A1",
        files={"file": ("policy.txt", b"Travel is reimbursed within 30 days.\n" * 4, TXT_MIME)},
        timeout=60,
    )
    assert resp.status_code == 201 and resp.json()["status"] == "ready", resp.text
    result, _ = _send(kb, chat_id, "What does the policy say?")
    assert result.terminal[0] == "done", result.events[-1:]

    body = _stream_requests(mock)[-1]["json"]
    assert "file_search" in _tool_types(body), body.get("tools")
    assert _function_tools(body) == [], "search_knowledge is not offered next to file_search"
    assert KB_GUARD not in _system_instructions(body), "the knowledge guard is not sent without the tool"


# --------------------------------------------------------------------------
# Agentic loop
# --------------------------------------------------------------------------


def test_search_knowledge_call_runs_retrieval_and_follow_up_request(kb, mock):
    mock.set_search_results(KB_VECTOR_STORE, knowledge_chunks(5))
    mock.enqueue(_call("call_kb_0001", top_k=50), {"text": "Revenue grew 12 percent."})
    chat_id = _new_chat(kb)
    with OutboxRecorder(kb, chat_id) as recorder:
        result, request_id = _send(kb, chat_id, "How did revenue develop?")
        assert result.terminal[0] == "done", result.events[-1:]
        usage_event = recorder.wait_usage_event(request_id)

    assert result.text_content() == "Revenue grew 12 percent."
    assert result.of("tool") == [], "the OpenAI Responses adapter emits no tool events for search_knowledge"

    searches = _searches(mock)
    assert len(searches) == 1, mock.requests_for("vector_store_search")
    search = searches[0]
    assert search["path"] == f"/openai/vector_stores/{KB_VECTOR_STORE}/search", search["path"]
    assert search["query"].get("api-version") == KB_API_VERSION, search["query"]
    assert search["headers"].get("api-key") == OPENAI_SECRET_VALUE, "sent through OAGW with the KB credential"
    assert search["json"]["query"] == "quarterly revenue", search["json"]
    if "max_num_results" in search["json"]:
        assert search["json"]["max_num_results"] <= TOP_K, search["json"]

    requests = _stream_requests(mock)
    assert len(requests) == 2, [r["seq"] for r in requests]
    follow_up = requests[1]["json"]
    calls = [c for c in _input_items(follow_up, "function_call") if c.get("call_id") == "call_kb_0001"]
    assert len(calls) == 1 and calls[0].get("name") == TOOL_NAME, follow_up["input"]
    outputs = [o for o in _input_items(follow_up, "function_call_output") if o.get("call_id") == "call_kb_0001"]
    assert len(outputs) == 1, follow_up["input"]
    output = _output_text(outputs[0])
    assert chunk_head(1) in output, output
    assert "TAILCUT" not in output, f"chunks are trimmed to max_chunk_chars: {output}"
    assert 1 <= output.count("KBCHUNK-") <= TOP_K, f"top_k is capped at knowledge_search.top_k: {output}"
    assert KB_VECTOR_STORE not in json.dumps(result.events)

    turn = _turn(kb, chat_id, request_id)
    assert turn["state"] == "completed", turn
    assert turn["file_search_completed_count"] == 1, turn
    assert usage_event["file_search_calls"] == 1, usage_event


def test_calls_beyond_max_calls_per_message_get_a_search_limit_output(kb, mock):
    mock.set_search_results(KB_VECTOR_STORE, knowledge_chunks(2))
    mock.enqueue(
        _call("call_kb_lim1", "first"),
        _call("call_kb_lim2", "second"),
        _call("call_kb_lim3", "third"),
        {"text": "Answer from what I have."},
    )
    chat_id = _new_chat(kb)
    result, request_id = _send(kb, chat_id, "Search a lot.")

    assert result.terminal[0] == "done", result.events[-1:]
    assert result.text_content() == "Answer from what I have."
    assert [s["json"]["query"] for s in _searches(mock)] == ["first", "second"], "at most max_calls retrievals"
    requests = _stream_requests(mock)
    assert len(requests) == 4, [r["seq"] for r in requests]
    last = requests[3]["json"]
    outputs = [o for o in _input_items(last, "function_call_output") if o.get("call_id") == "call_kb_lim3"]
    assert len(outputs) == 1, last["input"]
    assert "search limit reached" in _output_text(outputs[0]).lower(), outputs[0]
    for call_id in ("call_kb_lim1", "call_kb_lim2"):
        assert [o for o in _input_items(last, "function_call_output") if o.get("call_id") == call_id], (
            "earlier calls and outputs stay in the input",
            call_id,
        )
    turn = _turn(kb, chat_id, request_id)
    assert (turn["state"], turn["file_search_completed_count"]) == ("completed", MAX_CALLS), turn


def test_agentic_loop_hard_cap_fails_with_agentic_iterations_exceeded(kb, mock):
    mock.set_search_results(KB_VECTOR_STORE, knowledge_chunks(2))
    mock.set_default_scenario({"function_calls": [{"name": TOOL_NAME, "arguments": {"query": "again"}}]})
    chat_id = _new_chat(kb)
    result, request_id = _send(kb, chat_id, "Never stop searching.")

    name, error = result.terminal
    assert name == "error", result.names()
    assert error["code"] == "agentic_iterations_exceeded", error
    assert len(_stream_requests(mock)) == MAX_ITERATIONS, "the loop is capped at max_calls_per_message + 2"
    assert len(_searches(mock)) == MAX_CALLS
    turn = _turn(kb, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "agentic_iterations_exceeded"), turn


def test_unknown_function_name_is_unexpected_tool_use(kb, mock):
    mock.set_search_results(KB_VECTOR_STORE, knowledge_chunks(2))
    mock.enqueue(_call("call_kb_bad1", name="drop_all_tables"))
    chat_id = _new_chat(kb)
    result, request_id = _send(kb, chat_id, "Do something odd.")

    name, error = result.terminal
    assert name == "error", result.events[-1:]
    assert error["code"] == "unexpected_tool_use", error
    assert _searches(mock) == []
    assert len(_stream_requests(mock)) == 1, "no follow-up request after an unexpected tool use"
    turn = _turn(kb, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "unexpected_tool_use"), turn


def test_function_call_with_knowledge_search_disabled_is_unexpected_tool_use(mc, mock):
    mock.enqueue(_call("call_kb_off1"))
    chat_id = _new_chat(mc)
    result, request_id = _send(mc, chat_id, "Look it up.")

    body = _stream_requests(mock)[0]["json"]
    assert _function_tools(body) == [], "knowledge search is disabled on the default server"
    name, error = result.terminal
    assert name == "error", result.events[-1:]
    assert error["code"] == "unexpected_tool_use", error
    assert mock.requests_for("vector_store_search") == []
    turn = _turn(mc, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "unexpected_tool_use"), turn
