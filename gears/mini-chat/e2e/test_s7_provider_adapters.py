"""S7 provider adapters: Chat Completions, vLLM Responses, Anthropic Messages.

Contract sources (gears/mini-chat/docs/DESIGN.md, ADR-0005):

* ADR-0005 "Multiple LLM provider adapters": the provider entry's ``kind``
  selects the adapter (``openai_chat_completions``, ``vllm_responses``,
  ``anthropic_messages``); each catalog model names its ``provider_id``;
  requests go through OAGW to ``{alias}{api_path}``; images of chats served by
  ``anthropic_messages`` get a secondary copy in the Anthropic Files API
  (``attachments.secondary_*``), documents and images larger than
  ``thumbnail.max_decode_bytes`` get none;
* §3.2 ``llm_provider``: Chat Completions drops ``file_search``,
  ``web_search`` and ``code_interpreter`` and keeps function tools; vLLM drops
  every tool; Anthropic drops ``file_search`` and maps ``web_search`` and
  ``code_interpreter`` to its server tools; the gate is the catalog
  ``tool_support``, so on an adapter that drops a tool the surcharge is still
  reserved and the guard is still sent; the Anthropic Files client uploads
  the secondary image copy and deletes it on cleanup;
* §3.3 ``event: delta`` (``type: "reasoning"`` for text inside vLLM
  ``<think>`` blocks), ``event: tool`` (Chat Completions function tools are
  reported as ``name: "function_call"``), "Provider Event Translation"
  (Anthropic ``code_execution`` blocks -> ``tool`` start / done named
  ``code_interpreter`` with ``details: {}``; provider 429 -> ``rate_limited``),
  "Streaming error codes" (``unexpected_tool_use`` for any tool use when
  knowledge search is off; ``rate_limited`` message carries a numeric
  ``Retry-After``);
* §4 "Provider Request Metadata": ``user`` (tenant hex + user hex) for the
  OpenAI Responses, Chat Completions and vLLM adapters, Anthropic sends it as
  ``metadata.user_id``; the ``metadata`` object only on OpenAI Responses;
* §4 "Model Catalog Configuration": ``extra_body`` keys merged into the top
  level of the request by the Chat and vLLM adapters (not Anthropic), keys
  the request controls are ignored; ``max_tool_calls`` sent only by the
  OpenAI Responses adapter ("File Search Per-Turn Call Limit Enforcement");
* §5.4 "Canonical provider usage fields" and §5.5.9: ``prompt_tokens`` /
  ``completion_tokens`` normalized to ``input_tokens`` / ``output_tokens``;
  ``cache_read_input_tokens`` (OpenAI ``cached_tokens``, Anthropic
  ``cache_read_input_tokens``), ``cache_write_input_tokens`` (Anthropic
  ``cache_creation_input_tokens``) and ``reasoning_tokens`` are subsets of the
  totals, stored on the assistant message; incomplete responses (Chat
  ``finish_reason = length``, Anthropic ``stop_reason = max_tokens``) finish
  as ``done`` with the turn ``completed`` and no ``error_code``;
* §3.6 "File Upload" (secondary upload for Anthropic chats), §3.7
  ``attachments.secondary_file_id`` / ``secondary_status`` /
  ``secondary_provider_kind``, "Attachment Deletion" (payload
  ``secondary_ref = {file_id, provider_kind, upstream_alias}``, the cleanup
  handler deletes the secondary file), B.9.2 chat cleanup ("provider files
  (and the secondary Anthropic file, if any)"); PRD: the secondary copy
  carries no ``purpose`` field.

All three adapters run on one module-scoped server whose extra provider
entries point at the mock provider under their own OAGW aliases. Each
adapter's models are added to the static policy plugin catalog.
"""

from __future__ import annotations

import copy
import json
import random
import struct
import threading
import time
import uuid
import zlib
from typing import Any, Iterator

import pytest

from conftest import (
    APIKEY_AUTH_PLUGIN,
    OPENAI_SECRET_KEY,
    OPENAI_SECRET_VALUE,
    create_server,
    default_model_catalog,
)
from mc_helpers import db_uuid, new_uuid, wait_until
from mc_seed import outbox_payloads_for_chat

CHATS = "/mini-chat/v1/chats"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
DEFAULT_PROVIDER = "openai"
#: ``streaming.max_output_tokens`` default (B.4).
STREAMING_MAX_OUTPUT_TOKENS = 32_768
#: Built-in ``context.web_search_guard``.
WEB_SEARCH_GUARD = (
    "Use web_search only if the answer cannot be obtained from the provided context or your "
    "training data. Never use it for general knowledge questions. At most one web_search call "
    "per request."
)
#: Built-in ``context.file_search_guard``.
FILE_SEARCH_GUARD = (
    "Use file_search to look up information in the documents attached to this chat when the "
    "question concerns their content. Base your answer on the retrieved passages. If the documents "
    "do not contain the answer, say so instead of guessing."
)
BUILT_IN_TOOL_TYPES = {"file_search", "web_search", "web_search_preview", "code_interpreter"}

# Provider entries (ADR-0005). Every entry has its own OAGW alias so the
# upstreams and routes do not collide although they share the mock host.
CC_PROVIDER = "mock-chat-completions"
CC_ALIAS = "mock-chat-completions"
CC_API_PATH = "/v1/chat/completions"
VLLM_PROVIDER = "mock-vllm"
VLLM_ALIAS = "mock-vllm"
VLLM_API_PATH = "/vllm/v1/responses"
ANTHROPIC_PROVIDER = "anthropic"
ANTHROPIC_ALIAS = "mock-anthropic"
ANTHROPIC_API_PATH = "/v1/messages"
ANTHROPIC_SECRET_KEY = "anthropic-key"
ANTHROPIC_SECRET_VALUE = "anthropic-test-secret-value-0001"

CC_MODEL = "cc-model"
CC_PROVIDER_MODEL = "gpt-4o-cc-2024-08-06"
VLLM_MODEL = "vllm-model"
VLLM_PROVIDER_MODEL = "qwen3-32b-awq"
CLAUDE_MODEL = "claude-model"
CLAUDE_PROVIDER_MODEL = "claude-sonnet-4-5-20250929"

#: ``extra_body`` of the Chat Completions model: two free keys plus keys the
#: request controls (ignored with a warning).
CC_EXTRA_BODY = {
    "top_k": 40,
    "repetition_penalty": 1.05,
    "model": "hijacked-model",
    "user": "hijacked-user",
    "stream": False,
    "messages": [],
    "max_tool_calls": 99,
}
VLLM_EXTRA_BODY = {
    "chat_template_kwargs": {"enable_thinking": True},
    "model": "hijacked-model",
    "tools": [{"type": "function", "name": "hijacked"}],
    "metadata": {"hijacked": "yes"},
}
CLAUDE_EXTRA_BODY = {"top_k": 7, "service_tier": "standard_only"}
#: ``thumbnail.max_decode_bytes`` of the module server: small, so a large image
#: gets no secondary copy (ADR-0005 storage dispatch).
MAX_DECODE_BYTES = 65_536

PNG_MIME = "image/png"
TXT_MIME = "text/plain"


# --------------------------------------------------------------------------
# Server
# --------------------------------------------------------------------------


def _model(
    model_id: str,
    *,
    provider_id: str,
    provider_model_id: str,
    display_name: str,
    tools: dict[str, bool],
    max_output_tokens: int,
    extra_body: dict[str, Any],
    sort_order: int,
    vision: bool = False,
) -> dict[str, Any]:
    base = next(e for e in default_model_catalog() if e["id"] == "gpt-4.1-mini")
    entry = copy.deepcopy(base)
    entry.update(
        {
            "id": model_id,
            "provider_model_id": provider_model_id,
            "display_name": display_name,
            "description": f"{display_name} (S7)",
            "provider_id": provider_id,
            "provider_display_name": provider_id,
            "tier": "Standard",
            "max_output_tokens": max_output_tokens,
            "multimodal_capabilities": ["VISION_INPUT"] if vision else [],
        }
    )
    entry["general_config"]["api_params"]["extra_body"] = copy.deepcopy(extra_body)
    entry["general_config"]["tool_support"].update(
        {
            "web_search": tools.get("web_search", False),
            "file_search": tools.get("file_search", False),
            "code_interpreter": tools.get("code_interpreter", False),
        }
    )
    entry["preference"] = {"is_default": False, "sort_order": sort_order}
    return entry


def adapter_catalog() -> list[dict[str, Any]]:
    return default_model_catalog() + [
        _model(
            CC_MODEL,
            provider_id=CC_PROVIDER,
            provider_model_id=CC_PROVIDER_MODEL,
            display_name="Chat Completions Model",
            tools={"web_search": True},
            max_output_tokens=4_096,
            extra_body=CC_EXTRA_BODY,
            sort_order=10,
        ),
        _model(
            VLLM_MODEL,
            provider_id=VLLM_PROVIDER,
            provider_model_id=VLLM_PROVIDER_MODEL,
            display_name="vLLM Model",
            tools={"web_search": True},
            max_output_tokens=2_048,
            extra_body=VLLM_EXTRA_BODY,
            sort_order=11,
        ),
        _model(
            CLAUDE_MODEL,
            provider_id=ANTHROPIC_PROVIDER,
            provider_model_id=CLAUDE_PROVIDER_MODEL,
            display_name="Claude Model",
            tools={"web_search": True, "file_search": True},
            max_output_tokens=8_192,
            extra_body=CLAUDE_EXTRA_BODY,
            sort_order=12,
            vision=True,
        ),
    ]


def _provider(kind: str, alias: str, api_path: str, mock_port: int, auth: dict[str, str]) -> dict[str, Any]:
    return {
        "kind": kind,
        "storage_kind": "openai",
        "host": "127.0.0.1",
        "port": mock_port,
        "use_http": True,
        "upstream_alias": alias,
        "api_path": api_path,
        "auth_plugin_type": APIKEY_AUTH_PLUGIN,
        "auth_config": auth,
    }


def adapter_overrides(mock_port: int) -> dict[str, Any]:
    bearer = {"header": "Authorization", "prefix": "Bearer ", "secret_ref": f"cred://{OPENAI_SECRET_KEY}"}
    anthropic = _provider(
        "anthropic_messages",
        ANTHROPIC_ALIAS,
        ANTHROPIC_API_PATH,
        mock_port,
        {"header": "x-api-key", "secret_ref": f"cred://{ANTHROPIC_SECRET_KEY}"},
    )
    anthropic["rag_provider"] = DEFAULT_PROVIDER
    return {
        "gears": {
            "static-credstore-plugin": {
                "config": {
                    "secrets": [
                        {"key": OPENAI_SECRET_KEY, "value": OPENAI_SECRET_VALUE},
                        {"key": ANTHROPIC_SECRET_KEY, "value": ANTHROPIC_SECRET_VALUE},
                    ]
                }
            },
            "mini-chat": {
                "config": {
                    "thumbnail": {"max_decode_bytes": MAX_DECODE_BYTES},
                    "providers": {
                        CC_PROVIDER: _provider("openai_chat_completions", CC_ALIAS, CC_API_PATH, mock_port, bearer),
                        VLLM_PROVIDER: _provider("vllm_responses", VLLM_ALIAS, VLLM_API_PATH, mock_port, bearer),
                        ANTHROPIC_PROVIDER: anthropic,
                    },
                }
            },
            POLICY_PLUGIN: {"config": {"model_catalog": adapter_catalog()}},
        }
    }


@pytest.fixture(scope="module")
def _s7_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


@pytest.fixture(scope="module")
def adapters(mock, _s7_servers):
    """Server with the default ``openai`` provider plus one entry per S7 adapter kind."""
    server = create_server(mock, adapter_overrides(mock.port), name="s7-adapters")
    _s7_servers.append(server)
    server.start()
    return server.wait_ready()


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def _entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the server catalog")


def _applied_max_output(server, model_id: str) -> int:
    return min(int(_entry(server, model_id)["max_output_tokens"]), STREAMING_MAX_OUTPUT_TOKENS)


def expected_reserve_tokens(entry: dict[str, Any], content: str, *, web_search: bool) -> int:
    """Preflight ``reserve_tokens`` of a first, text-only turn (DESIGN §5.4.1, §5.5)."""
    budgets = entry["estimation_budgets"]
    base = _ceil_div(len(content.encode("utf-8")), budgets["bytes_per_token_conservative"])
    est_input = _ceil_div((base + budgets["fixed_overhead_tokens"]) * (100 + budgets["safety_margin_pct"]), 100)
    if web_search and entry["general_config"]["tool_support"]["web_search"]:
        est_input += budgets["web_search_surcharge_tokens"]
    return est_input + min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)


def _expected_user(server, user: str = "A1") -> str:
    ident = server.identity(user)
    return uuid.UUID(ident.tenant_id).hex + uuid.UUID(ident.user_id).hex


def _new_chat(server, model: str, user: str = "A1") -> str:
    resp = server.post(CHATS, user, json={"title": "S7 adapters", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _send(
    server,
    chat_id: str,
    content: str,
    *,
    web_search: bool | None = None,
    attachment_ids: list[str] | None = None,
    user: str = "A1",
):
    request_id = new_uuid()
    body: dict[str, Any] = {"content": content, "request_id": request_id}
    if web_search is not None:
        body["web_search"] = {"enabled": web_search}
    if attachment_ids is not None:
        body["attachment_ids"] = attachment_ids
    result = server.stream("POST", f"{CHATS}/{chat_id}/messages:stream", user, json=body)
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert result.terminal is not None, names
    return result, request_id


def _send_done(server, chat_id: str, content: str, **kwargs: Any):
    result, request_id = _send(server, chat_id, content, **kwargs)
    assert result.terminal[0] == "done", (result.names(), result.events[-1:])
    return result, request_id


def _turn(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", (db_uuid(chat_id), db_uuid(request_id))
    )
    assert len(rows) == 1, rows
    return rows[0]


def _assistant_message(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM messages WHERE chat_id = ? AND request_id = ? AND role = 'assistant'",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def _attachment_row(server, attachment_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM attachments WHERE id = ?", (db_uuid(attachment_id),))
    assert len(rows) == 1, rows
    return rows[0]


def _deltas(result, kind: str) -> list[str]:
    return [str(d.get("content", "")) for d in result.of("delta") if isinstance(d, dict) and d.get("type") == kind]


def _stream_requests(mock, op: str, path_prefix: str | None = None) -> list[dict[str, Any]]:
    return [
        r
        for r in mock.requests_for(op)
        if r["stream"] and (path_prefix is None or r["path"].startswith(path_prefix))
    ]


def _last_stream_request(mock, op: str, path_prefix: str | None = None) -> dict[str, Any]:
    found = _stream_requests(mock, op, path_prefix)
    assert found, f"no streaming {op} request recorded (prefix {path_prefix!r})"
    return found[-1]


def _text_of(content: Any) -> str:
    """Text of a message ``content``: a string, or the ``text`` of its parts."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(p.get("text", "")) for p in content if isinstance(p, dict) and "text" in p)
    return ""


def _chat_system_text(body: dict[str, Any]) -> str:
    return "\n".join(
        _text_of(m.get("content")) for m in body.get("messages") or [] if m.get("role") in ("system", "developer")
    )


def _responses_system_text(body: dict[str, Any]) -> str:
    parts = [str(body.get("instructions") or "")]
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("role") in ("system", "developer"):
            parts.append(_text_of(item.get("content")))
    return "\n".join(parts)


def _anthropic_system_text(body: dict[str, Any]) -> str:
    return _text_of(body.get("system"))


def _tool_types(body: dict[str, Any]) -> list[str]:
    return [str(t.get("type")) for t in body.get("tools") or [] if isinstance(t, dict)]


def _max_output(body: dict[str, Any]) -> Any:
    """Chat Completions output cap: ``max_completion_tokens`` or ``max_tokens``."""
    if "max_completion_tokens" in body:
        return body["max_completion_tokens"]
    return body.get("max_tokens")


def make_png(width: int, height: int, *, noise: bool = False, seed: int = 11) -> bytes:
    """An 8-bit RGB PNG; ``noise=True`` makes it (nearly) incompressible."""
    rng = random.Random(seed)
    rows = []
    for y in range(height):
        if noise:
            row = rng.randbytes(width * 3)
        else:
            row = bytes(v for x in range(width) for v in (x * 255 // max(1, width - 1), y % 256, 90))
        rows.append(b"\x00" + row)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )


def _upload(server, chat_id: str, data: bytes, filename: str, content_type: str, user: str = "A1") -> dict[str, Any]:
    resp = server.post(
        f"{CHATS}/{chat_id}/attachments", user, files={"file": (filename, data, content_type)}, timeout=60
    )
    assert resp.status_code == 201, f"upload failed: HTTP {resp.status_code} {resp.text[:1000]}"
    body = resp.json()
    assert body["status"] == "ready", body
    return body


def _primary_uploads(mock) -> list[dict[str, Any]]:
    """Files API uploads to the RAG provider (``openai`` entry, bearer credential)."""
    return [r for r in mock.requests_for("files_upload") if "x-api-key" not in r["headers"]]


def _secondary_uploads(mock) -> list[dict[str, Any]]:
    """Files API uploads to the Anthropic entry (``x-api-key`` credential)."""
    return [r for r in mock.requests_for("files_upload") if "x-api-key" in r["headers"]]


def _multipart_names(record: dict[str, Any]) -> set[str]:
    return {f["name"] for f in record["multipart"] or []}


def _file_deletes(mock, file_id: str, *, anthropic: bool) -> list[dict[str, Any]]:
    """Files API deletes of ``file_id`` sent to the Anthropic entry (or to the RAG provider)."""
    return [
        r
        for r in mock.requests_for("files_delete")
        if r["path"].endswith(f"/files/{file_id}") and ("x-api-key" in r["headers"]) == anthropic
    ]


class CleanupRecorder:
    """Polls the outbox bodies of one chat in a background thread (bodies are vacuumed after ack)."""

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="s7-cleanup-recorder", daemon=True)

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

    def __enter__(self) -> "CleanupRecorder":
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def wait_cleanup_payload(self, attachment_id: str) -> dict[str, Any]:
        def found() -> list[dict[str, Any]]:
            self.poll()
            with self._lock:
                return [
                    p
                    for p in self._seen.values()
                    if str(p.get("attachment_id", "")).lower() == attachment_id.lower()
                    and p.get("event_type") == "attachment_deleted"
                ]

        payloads = wait_until(found, timeout=15, interval=0.05, description=f"cleanup payload of {attachment_id}")
        assert len(payloads) == 1, payloads
        return payloads[0]


# --------------------------------------------------------------------------
# openai_chat_completions
# --------------------------------------------------------------------------


def test_chat_completions_request_carries_system_history_user_and_identity(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue({"text": "First answer."})
    _send_done(adapters, chat_id, "First question?")
    mock.enqueue({"chunks": ["Second ", "answer."]})
    result, _ = _send_done(adapters, chat_id, "Second question?")

    record = _last_stream_request(mock, "chat_completions")
    assert record["path"] == CC_API_PATH, record["path"]
    assert record["headers"].get("authorization") == f"Bearer {OPENAI_SECRET_VALUE}", "OAGW injects the credential"
    body = record["json"]
    assert body["model"] == CC_PROVIDER_MODEL, body
    assert body["stream"] is True, body

    messages = body["messages"]
    assert messages[0]["role"] in ("system", "developer"), messages
    assert _chat_system_text(body) == _entry(adapters, CC_MODEL)["system_prompt"], messages[0]
    history = [(m["role"], _text_of(m.get("content"))) for m in messages[1:]]
    assert history == [
        ("user", "First question?"),
        ("assistant", "First answer."),
        ("user", "Second question?"),
    ], history

    assert body["user"] == _expected_user(adapters), body.get("user")
    assert len(body["user"]) == 64
    assert "metadata" not in body, "the metadata object is sent only by the OpenAI Responses adapter"
    assert "max_tool_calls" not in body, "max_tool_calls is sent only by the OpenAI Responses adapter"
    assert _max_output(body) == _applied_max_output(adapters, CC_MODEL), body
    assert not [t for t in _tool_types(body) if t in BUILT_IN_TOOL_TYPES], body.get("tools")

    assert result.text_content() == "Second answer."
    assert {d.get("type") for d in result.of("delta")} == {"text"}, result.of("delta")


def test_chat_completions_merges_extra_body_and_ignores_controlled_keys(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    _send_done(adapters, chat_id, "Use the extra body please.")

    body = _last_stream_request(mock, "chat_completions")["json"]
    assert body["top_k"] == CC_EXTRA_BODY["top_k"], body
    assert body["repetition_penalty"] == CC_EXTRA_BODY["repetition_penalty"], body
    assert body["model"] == CC_PROVIDER_MODEL, "model is controlled by the request"
    assert body["user"] == _expected_user(adapters), "user is controlled by the request"
    assert body["stream"] is True, "stream is controlled by the request"
    assert [m["role"] for m in body["messages"]][-1] == "user", "messages are controlled by the request"
    assert "max_tool_calls" not in body, "a controlled key from extra_body is ignored, not sent"


def test_chat_completions_usage_from_final_chunk_is_normalized(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue(
        {
            "text": "Counted answer.",
            "usage": {"input_tokens": 321, "output_tokens": 45, "cached_tokens": 100, "reasoning_tokens": 7},
        }
    )
    result, request_id = _send_done(adapters, chat_id, "Count my tokens.")

    done = result.terminal[1]
    assert done["usage"] == {"input_tokens": 321, "output_tokens": 45}, done
    message = _assistant_message(adapters, chat_id, request_id)
    assert (message["input_tokens"], message["output_tokens"]) == (321, 45), message
    assert message["cache_read_input_tokens"] == 100, message
    assert message["reasoning_tokens"] == 7, message
    assert message["cache_write_input_tokens"] == 0, message
    assert message["content"] == "Counted answer.", message
    assert _turn(adapters, chat_id, request_id)["state"] == "completed"


def test_chat_completions_finish_reason_length_finishes_as_done(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue({"kind": "incomplete", "text": "Truncated answ"})
    result, request_id = _send_done(adapters, chat_id, "Write a long essay.")

    assert result.text_content() == "Truncated answ"
    assert "citations" not in result.names()
    turn = _turn(adapters, chat_id, request_id)
    assert turn["state"] == "completed", turn
    assert turn["error_code"] is None, "an incomplete reason is never written to chat_turns.error_code"
    assert _assistant_message(adapters, chat_id, request_id)["content"] == "Truncated answ"


def test_chat_completions_drops_web_search_but_reserves_surcharge_and_sends_guard(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    content = "What is new in the world today?"
    _, request_id = _send_done(adapters, chat_id, content, web_search=True)

    body = _last_stream_request(mock, "chat_completions")["json"]
    assert not [t for t in _tool_types(body) if t in BUILT_IN_TOOL_TYPES], (
        "the Chat Completions adapter drops web_search", body.get("tools")
    )
    assert WEB_SEARCH_GUARD in _chat_system_text(body), "the guard is still sent (gate is tool_support)"
    turn = _turn(adapters, chat_id, request_id)
    assert turn["web_search_enabled"] == 1, turn
    assert turn["reserve_tokens"] == expected_reserve_tokens(_entry(adapters, CC_MODEL), content, web_search=True), (
        "the web_search surcharge is still reserved",
        turn["reserve_tokens"],
    )


def test_chat_completions_429_is_rate_limited(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue({"kind": "http_error", "status": 429, "retry_after": 17})
    result, request_id = _send(adapters, chat_id, "Hello?")

    name, error = result.terminal
    assert name == "error", result.names()
    assert error["code"] == "rate_limited", error
    assert "17" in error["message"], "a numeric Retry-After is included in the message"
    turn = _turn(adapters, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "rate_limited"), turn


def test_chat_completions_error_chunk_is_sanitized_provider_error(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue({"kind": "sse_error", "error_code": "server_error"})
    result, request_id = _send(adapters, chat_id, "Hello?")

    name, error = result.terminal
    assert name == "error", result.names()
    assert error["code"] == "provider_error", error
    for leaked in ("resp_abc123def456ghi789", "file-AbCdEf123456789012", "sk-proj-", "https://internal-llm"):
        assert leaked not in error["message"], error
    assert _turn(adapters, chat_id, request_id)["state"] == "failed"


def test_chat_completions_function_call_without_knowledge_search_is_unexpected_tool_use(adapters, mock):
    chat_id = _new_chat(adapters, CC_MODEL)
    mock.enqueue(
        {"function_calls": [{"name": "search_knowledge", "arguments": {"query": "x"}, "call_id": "call_cc0001"}]}
    )
    result, request_id = _send(adapters, chat_id, "Look it up.")

    name, error = result.terminal
    assert name == "error", result.names()
    assert error["code"] == "unexpected_tool_use", error
    tools = [t for t in result.of("tool") if t.get("name") == "function_call"]
    starts = [t for t in tools if t["phase"] == "start"]
    dones = [t for t in tools if t["phase"] == "done"]
    assert len(starts) == 1 and len(dones) == 1, tools
    assert {"index", "call_id", "name"} <= set(starts[0]["details"]), starts[0]
    assert starts[0]["details"]["call_id"] == "call_cc0001" and starts[0]["details"]["name"] == "search_knowledge"
    assert {"call_id", "name", "arguments"} <= set(dones[0]["details"]), dones[0]
    turn = _turn(adapters, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "unexpected_tool_use"), turn


# --------------------------------------------------------------------------
# vllm_responses
# --------------------------------------------------------------------------


def test_vllm_request_has_no_tools_no_metadata_and_merged_extra_body(adapters, mock):
    chat_id = _new_chat(adapters, VLLM_MODEL)
    content = "Search the web for me."
    _, request_id = _send_done(adapters, chat_id, content, web_search=True)

    record = _last_stream_request(mock, "responses", VLLM_API_PATH)
    assert record["path"] == VLLM_API_PATH, record["path"]
    body = record["json"]
    assert body["model"] == VLLM_PROVIDER_MODEL, body
    assert body["stream"] is True
    assert not body.get("tools"), ("the vLLM adapter drops all tools", body.get("tools"))
    assert "metadata" not in body, "vLLM omits the metadata object"
    assert "max_tool_calls" not in body
    assert "include" not in body
    assert body["user"] == _expected_user(adapters), body.get("user")
    assert body.get("max_output_tokens") == _applied_max_output(adapters, VLLM_MODEL), body
    system = _responses_system_text(body)
    assert _entry(adapters, VLLM_MODEL)["system_prompt"] in system, system
    assert WEB_SEARCH_GUARD in system, "the guard is still sent (gate is tool_support)"
    assert body["chat_template_kwargs"] == VLLM_EXTRA_BODY["chat_template_kwargs"], "extra_body is merged"
    turn = _turn(adapters, chat_id, request_id)
    assert turn["reserve_tokens"] == expected_reserve_tokens(_entry(adapters, VLLM_MODEL), content, web_search=True)


def test_vllm_think_block_is_streamed_as_reasoning_deltas(adapters, mock):
    chat_id = _new_chat(adapters, VLLM_MODEL)
    mock.enqueue({"chunks": ["<think>Let me ", "think about it.</think>", "The answer ", "is 42."]})
    result, request_id = _send_done(adapters, chat_id, "What is the answer?")

    reasoning = _deltas(result, "reasoning")
    text = _deltas(result, "text")
    assert "".join(reasoning).strip() == "Let me think about it.", result.of("delta")
    assert "".join(text).strip() == "The answer is 42.", result.of("delta")
    for delta in result.of("delta"):
        assert "<think>" not in delta["content"] and "</think>" not in delta["content"], delta
    kinds = [d["type"] for d in result.of("delta")]
    assert kinds.index("reasoning") < kinds.index("text"), kinds
    assert _turn(adapters, chat_id, request_id)["state"] == "completed"


# --------------------------------------------------------------------------
# anthropic_messages
# --------------------------------------------------------------------------


def test_anthropic_request_shape(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"text": "Hello, I am Claude."})
    _send_done(adapters, chat_id, "Who are you?")
    mock.enqueue({"chunks": ["Still ", "Claude."]})
    result, _ = _send_done(adapters, chat_id, "And now?")

    record = _last_stream_request(mock, "messages")
    assert record["path"] == ANTHROPIC_API_PATH, record["path"]
    assert record["headers"].get("x-api-key") == ANTHROPIC_SECRET_VALUE, "OAGW injects the Anthropic credential"
    body = record["json"]
    assert body["model"] == CLAUDE_PROVIDER_MODEL, body
    assert body["stream"] is True, body
    assert _anthropic_system_text(body) == _entry(adapters, CLAUDE_MODEL)["system_prompt"], body.get("system")
    roles = [m["role"] for m in body["messages"]]
    assert roles == ["user", "assistant", "user"], roles
    texts = [_text_of(m["content"]) for m in body["messages"]]
    assert texts == ["Who are you?", "Hello, I am Claude.", "And now?"], texts
    assert body["max_tokens"] == _applied_max_output(adapters, CLAUDE_MODEL), body
    assert body["metadata"] == {"user_id": _expected_user(adapters)}, body.get("metadata")
    assert "user" not in body
    assert "max_tool_calls" not in body
    for key in CLAUDE_EXTRA_BODY:
        assert key not in body, f"the Anthropic adapter does not send extra_body ({key})"
    assert not body.get("tools"), body.get("tools")

    assert result.text_content() == "Still Claude."
    assert {d.get("type") for d in result.of("delta")} == {"text"}
    assert "msg_" not in json.dumps(result.events), "no provider message id reaches the client"


def test_anthropic_usage_with_cache_counters_is_normalized(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue(
        {
            "text": "Cached answer.",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 9,
                "cache_read_input_tokens": 4000,
                "cache_creation_input_tokens": 100,
            },
        }
    )
    result, request_id = _send_done(adapters, chat_id, "Use the cache.")

    # Anthropic reports uncached input in input_tokens; the canonical
    # input_tokens has the cache counters as a subset (DESIGN §5.5.9).
    total_input = 50 + 4000 + 100
    message = _assistant_message(adapters, chat_id, request_id)
    assert message["cache_read_input_tokens"] == 4000, message
    assert message["cache_write_input_tokens"] == 100, message
    assert message["output_tokens"] == 9, message
    assert message["input_tokens"] == total_input, message
    assert result.terminal[1]["usage"] == {"input_tokens": total_input, "output_tokens": 9}


def test_anthropic_max_tokens_stop_finishes_as_done(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"kind": "incomplete", "text": "Cut off mid"})
    result, request_id = _send_done(adapters, chat_id, "Write a long story.")

    assert result.text_content() == "Cut off mid"
    turn = _turn(adapters, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("completed", None), turn


def test_anthropic_web_search_is_mapped_to_the_server_tool(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"tool_events": [{"type": "web_search"}], "text": "Fresh news."})
    result, request_id = _send_done(adapters, chat_id, "Any news?", web_search=True)

    body = _last_stream_request(mock, "messages")["json"]
    web = [t for t in body.get("tools") or [] if t.get("name") == "web_search"]
    assert len(web) == 1, body.get("tools")
    assert str(web[0].get("type", "")).startswith("web_search_"), web[0]
    assert "file_search" not in _tool_types(body)
    assert WEB_SEARCH_GUARD in _anthropic_system_text(body)
    assert result.text_content() == "Fresh news."
    assert _turn(adapters, chat_id, request_id)["web_search_enabled"] == 1


def test_anthropic_code_execution_blocks_become_code_interpreter_tool_events(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"tool_events": [{"type": "code_interpreter", "logs": "42"}], "text": "Computed."})
    result, request_id = _send_done(adapters, chat_id, "Compute something.")

    tools = [t for t in result.of("tool") if t.get("name") == "code_interpreter"]
    assert [t["phase"] for t in tools] == ["start", "done"], result.of("tool")
    assert all(t["details"] == {} for t in tools), tools
    assert _turn(adapters, chat_id, request_id)["code_interpreter_completed_count"] == 1


def test_anthropic_never_sends_file_search(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    _upload(adapters, chat_id, b"Quarterly revenue grew by 12 percent.\n" * 4, "notes.txt", TXT_MIME)
    _send_done(adapters, chat_id, "What does my document say?")

    body = _last_stream_request(mock, "messages")["json"]
    assert "file_search" not in _tool_types(body), body.get("tools")
    assert "vector_store_ids" not in json.dumps(body), "no vector store reference reaches Anthropic"
    assert FILE_SEARCH_GUARD in _anthropic_system_text(body), "the guard is still sent (gate is tool_support)"


def test_anthropic_error_event_is_sanitized_provider_error(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"kind": "sse_error", "error_code": "overloaded_error"})
    result, request_id = _send(adapters, chat_id, "Hello?")

    name, error = result.terminal
    assert name == "error", result.names()
    assert error["code"] == "provider_error", error
    for leaked in ("resp_abc123def456ghi789", "file-AbCdEf123456789012", "sk-proj-", "https://internal-llm"):
        assert leaked not in error["message"], error
    assert _turn(adapters, chat_id, request_id)["state"] == "failed"


def test_anthropic_429_is_rate_limited(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    mock.enqueue({"kind": "http_error", "status": 429, "retry_after": 23})
    result, request_id = _send(adapters, chat_id, "Hello?")

    name, error = result.terminal
    assert name == "error", result.events[-1:]
    assert error["code"] == "rate_limited", error
    assert "23" in error["message"], error
    turn = _turn(adapters, chat_id, request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "rate_limited"), turn


# --------------------------------------------------------------------------
# Anthropic secondary image copies
# --------------------------------------------------------------------------


def test_anthropic_image_gets_secondary_copy_and_is_sent_as_file_reference(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    image = _upload(adapters, chat_id, make_png(48, 32), "photo.png", PNG_MIME)

    primary_uploads = _primary_uploads(mock)
    secondary_uploads = _secondary_uploads(mock)
    assert len(primary_uploads) == 1 and len(secondary_uploads) == 1, mock.requests_for("files_upload")
    assert primary_uploads[0]["path"] == "/v1/files"
    assert primary_uploads[0]["headers"]["authorization"] == f"Bearer {OPENAI_SECRET_VALUE}"
    assert secondary_uploads[0]["path"] == "/v1/files"
    assert secondary_uploads[0]["headers"]["x-api-key"] == ANTHROPIC_SECRET_VALUE
    assert "purpose" not in _multipart_names(secondary_uploads[0]), "the secondary copy carries no purpose"

    row = _attachment_row(adapters, image["id"])
    primary_id = primary_uploads[0]["response_id"]
    secondary_id = secondary_uploads[0]["response_id"]
    assert row["provider_file_id"] == primary_id, row
    assert row["storage_backend"] == DEFAULT_PROVIDER, "files are stored with the rag_provider"
    assert row["secondary_file_id"] == secondary_id, row
    assert row["secondary_status"] == "uploaded", row
    assert row["secondary_provider_kind"] == "anthropic", row
    assert primary_id not in json.dumps(image) and secondary_id not in json.dumps(image)

    result, _ = _send_done(adapters, chat_id, "What is in this photo?", attachment_ids=[image["id"]])
    body = _last_stream_request(mock, "messages")["json"]
    last_user = body["messages"][-1]
    assert last_user["role"] == "user", body["messages"]
    images = [b for b in last_user["content"] if isinstance(b, dict) and b.get("type") == "image"]
    assert images == [{"type": "image", "source": {"type": "file", "file_id": secondary_id}}], last_user
    assert primary_id not in json.dumps(body), "the RAG provider file id is not sent to Anthropic"
    stream_text = json.dumps(result.events)
    assert primary_id not in stream_text and secondary_id not in stream_text


@pytest.mark.parametrize(
    ("case", "model", "filename", "content_type", "data"),
    [
        pytest.param("document", CLAUDE_MODEL, "notes.txt", TXT_MIME, b"Plain notes.\n" * 8, id="anthropic-document"),
        pytest.param(
            "large-image", CLAUDE_MODEL, "noise.png", PNG_MIME, make_png(200, 200, noise=True), id="anthropic-large-image"
        ),
        pytest.param("openai-image", "gpt-4.1", "photo.png", PNG_MIME, make_png(48, 32), id="openai-chat-image"),
    ],
)
def test_no_secondary_copy(adapters, mock, case, model, filename, content_type, data):
    if case == "large-image":
        assert len(data) > MAX_DECODE_BYTES, "test image must exceed thumbnail.max_decode_bytes"
    chat_id = _new_chat(adapters, model)
    attachment = _upload(adapters, chat_id, data, filename, content_type)

    assert _secondary_uploads(mock) == [], "no Anthropic Files API upload"
    assert len(_primary_uploads(mock)) == 1
    row = _attachment_row(adapters, attachment["id"])
    assert row["secondary_file_id"] is None, row
    assert row["secondary_status"] == "not_attempted", row
    assert row["secondary_provider_kind"] is None, row


def test_deleting_anthropic_image_attachment_deletes_the_secondary_file(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    image = _upload(adapters, chat_id, make_png(40, 40), "unused.png", PNG_MIME)
    row = _attachment_row(adapters, image["id"])
    primary_id, secondary_id = row["provider_file_id"], row["secondary_file_id"]
    assert primary_id and secondary_id, row

    with CleanupRecorder(adapters, chat_id) as recorder:
        resp = adapters.delete(f"{CHATS}/{chat_id}/attachments/{image['id']}", "A1")
        assert resp.status_code == 204, resp.text
        payload = recorder.wait_cleanup_payload(image["id"])

    assert payload["provider_file_id"] == primary_id, payload
    assert payload["secondary_ref"] == {
        "file_id": secondary_id,
        "provider_kind": "anthropic",
        "upstream_alias": ANTHROPIC_ALIAS,
    }, payload
    wait_until(lambda: _file_deletes(mock, primary_id, anthropic=False), timeout=30, description="primary delete")
    deletes = wait_until(
        lambda: _file_deletes(mock, secondary_id, anthropic=True), timeout=30, description="secondary delete"
    )
    assert deletes[0]["headers"]["x-api-key"] == ANTHROPIC_SECRET_VALUE
    assert secondary_id not in mock.files and primary_id not in mock.files
    wait_until(
        lambda: _attachment_row(adapters, image["id"])["cleanup_status"] == "done",
        timeout=30,
        description="attachment cleanup done",
    )


def test_deleting_anthropic_chat_deletes_the_secondary_files(adapters, mock):
    chat_id = _new_chat(adapters, CLAUDE_MODEL)
    image = _upload(adapters, chat_id, make_png(36, 36), "sent.png", PNG_MIME)
    _send_done(adapters, chat_id, "Describe it.", attachment_ids=[image["id"]])
    row = _attachment_row(adapters, image["id"])
    primary_id, secondary_id = row["provider_file_id"], row["secondary_file_id"]
    assert primary_id and secondary_id, row

    resp = adapters.delete(f"{CHATS}/{chat_id}", "A1")
    assert resp.status_code == 204, resp.text

    wait_until(lambda: _file_deletes(mock, primary_id, anthropic=False), timeout=30, description="primary delete")
    wait_until(lambda: _file_deletes(mock, secondary_id, anthropic=True), timeout=30, description="secondary delete")
    wait_until(
        lambda: _attachment_row(adapters, image["id"])["cleanup_status"] == "done",
        timeout=30,
        description="chat cleanup of the image attachment",
    )
    assert secondary_id not in mock.files
