"""S3 streaming contract: happy path, persistence, provider request, ping, relay.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "Streaming Contract" (``POST /v1/chats/{id}/messages:stream``,
  ``StreamMessageRequest``; ``mini_chat.stream_message``): request body,
  ``request_id`` semantics (client value echoed, server-generated UUID v4 when
  omitted);
* §3.3 "SSE Event Definitions" (``MiniChatSseEvent``, ``StreamStartedData``,
  ``DeltaData``, ``DoneData``, ``Usage``, ``PingData``), "SSE Event Ordering"
  (``stream_started ping* (delta | tool)* citations? (done | error)``) and
  "SSE stream close rules" (connection closed right after the terminal
  event);
* §3.3 "Provider Event Translation": ``response.output_text.delta`` ->
  ``delta`` 1:1, ``response.completed`` / ``response.incomplete`` -> ``done``,
  the provider ``response.id`` stored in ``chat_turns.provider_response_id``
  and never exposed, the incomplete reason never written to ``error_code``;
* §3.6 "Send Message with Streaming Response" (user message + touch
  ``chats.updated_at`` in the reserve transaction, finalization persists the
  assistant message, ``done`` only after the commit);
* "Context Plan Assembly" / "Recent messages query" (system prompt, recent
  messages in chronological order, then the current user message);
* "Provider Request Metadata" (``user`` = tenant hex + user hex, ``metadata``),
  the catalog fields ``provider_model_id``, ``max_output_tokens`` (applied
  value ``min(catalog, streaming.max_output_tokens)``), ``system_prompt``,
  ``max_tool_calls`` and ``general_config.api_params``;
* §3.7 ``messages`` / ``chat_turns`` columns, §5.7 "Terminal SSE Event
  Emission Guard" and "Content durability invariant";
* ADR-0010: ``ping`` only between ``stream_started`` and the first
  ``delta`` / ``tool`` event, every ``sse_ping_interval_seconds`` of idle time.

Tool events, citations, validation errors, cancellation and quota are covered
by the other S3 files.
"""

from __future__ import annotations

import re
import time
import uuid
from dataclasses import dataclass, field
from typing import Any

import httpx
import pytest

from conftest import OPENAI_SECRET_VALUE
from mc_helpers import db_uuid, is_uuid, parse_sse_lines
from mc_seed import parse_ts_ns

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
DEFAULT_PROVIDER_MODEL = "gpt-4.1-2025-04-14"
#: ``streaming.max_output_tokens`` default (gear config B.4).
STREAMING_MAX_OUTPUT_TOKENS = 32_768
#: Gear-level ``estimation_budgets.minimal_generation_floor`` default.
MINIMAL_GENERATION_FLOOR = 50
#: Version of the static model-policy plugin snapshot.
POLICY_VERSION = 1

STREAM_STARTED_FIELDS = {"request_id", "message_id", "is_new_turn", "thread_summary_applied"}
DONE_FIELDS = {
    "usage",
    "effective_model",
    "selected_model",
    "quota_decision",
    "downgrade_from",
    "downgrade_reason",
    "quota_warnings",
}
#: ``stream_started ping* delta+ done`` for a text-only completion.
TEXT_STREAM_ORDER = re.compile(r"^stream_started( ping)*( delta)+ done$")


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def _new_chat(server, user: str = "A1", **body: Any) -> dict[str, Any]:
    resp = server.post(CHATS, user, json={"title": "S3 stream contract", **body})
    assert resp.status_code == 201, resp.text
    return resp.json()


@dataclass
class TimedStream:
    """An SSE response read with per-event arrival times (``time.monotonic``)."""

    status_code: int
    headers: httpx.Headers
    sent_at: float
    events: list[tuple[float, str, Any]] = field(default_factory=list)
    raw: str = ""
    body: Any = None
    #: Monotonic time the server closed the stream (EOF), ``None`` when it did not.
    closed_at: float | None = None
    transport_error: str | None = None

    @property
    def is_sse(self) -> bool:
        return self.headers.get("content-type", "").startswith("text/event-stream")

    def names(self) -> list[str]:
        return [name for _, name, _ in self.events]

    def of(self, name: str) -> list[Any]:
        return [data for _, event, data in self.events if event == name]

    def first(self, name: str) -> Any:
        found = self.of(name)
        assert found, f"no {name!r} event in stream: {self.names()}"
        return found[0]

    def times(self, name: str) -> list[float]:
        return [at for at, event, _ in self.events if event == name]

    def text(self) -> str:
        return "".join(d["content"] for d in self.of("delta"))


def _timed_stream(
    server,
    chat_id: str,
    body: dict[str, Any],
    user: str = "A1",
    *,
    read_timeout: float = 20.0,
) -> TimedStream:
    """POST ``messages:stream`` and read the SSE stream to EOF, timing every event.

    A server that keeps the connection open after the terminal event is
    detected by the read timeout (``closed_at`` stays ``None``).
    """
    timeout = httpx.Timeout(connect=5.0, read=read_timeout, write=10.0, pool=10.0)
    lines: list[str] = []
    with httpx.Client(timeout=timeout) as client:
        sent_at = time.monotonic()
        with client.stream(
            "POST", server.url(_stream_path(chat_id)), headers=server.headers(user), json=body
        ) as resp:
            result = TimedStream(status_code=resp.status_code, headers=resp.headers, sent_at=sent_at)
            if not result.is_sse:
                resp.read()
                result.raw = resp.text
                try:
                    result.body = resp.json()
                except ValueError:
                    result.body = None
                return result

            def _lines():
                for line in resp.iter_lines():
                    lines.append(line)
                    yield line

            try:
                for name, data in parse_sse_lines(_lines()):
                    result.events.append((time.monotonic(), name, data))
                result.closed_at = time.monotonic()
            except httpx.ReadTimeout as exc:
                result.transport_error = f"ReadTimeout: {exc}"
            except (httpx.RemoteProtocolError, httpx.ReadError) as exc:
                result.transport_error = f"{type(exc).__name__}: {exc}"
    result.raw = "\n".join(lines)
    return result


def _send(server, chat_id: str, content: str, user: str = "A1", **extra: Any) -> TimedStream:
    """Send a message and require a complete ``done`` stream."""
    result = _timed_stream(server, chat_id, {"content": content, **extra}, user)
    assert result.status_code == 200, f"HTTP {result.status_code}: {result.raw[:1000]}"
    assert result.is_sse, result.headers
    assert result.names() and result.names()[-1] == "done", (
        f"stream did not end with done: {result.names()} {result.events[-1:]} {result.transport_error}"
    )
    return result


def _stream_requests(mock) -> list[dict[str, Any]]:
    """Streaming Responses API calls recorded by the mock (thread-summary calls excluded)."""
    return [r for r in mock.requests("/responses", method="POST") if r["stream"]]


def _item_text(content: Any) -> str:
    """Text of a Responses ``input`` item ``content`` (string or content-part array)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            str(part.get("text", "")) for part in content if isinstance(part, dict) and "text" in part
        )
    raise AssertionError(f"unexpected input content shape: {content!r}")


def _input_items(body: dict[str, Any]) -> list[dict[str, Any]]:
    items = body.get("input")
    assert isinstance(items, list) and items, f"provider request has no input items: {body}"
    return items


def _system_instructions(body: dict[str, Any]) -> str:
    """System instructions of a Responses request.

    DESIGN "Sent as system instructions": either the top-level ``instructions``
    field or a leading ``system`` / ``developer`` input item.
    """
    if body.get("instructions"):
        return str(body["instructions"])
    first = _input_items(body)[0]
    assert first.get("role") in ("system", "developer"), (
        f"no system instructions in the provider request: {body}"
    )
    return _item_text(first.get("content"))


def _conversation(body: dict[str, Any]) -> list[tuple[str, str]]:
    """``(role, text)`` of the non-system input items, in request order."""
    return [
        (item.get("role"), _item_text(item.get("content")))
        for item in _input_items(body)
        if item.get("role") not in ("system", "developer")
    ]


def _list_messages(server, chat_id: str, user: str = "A1") -> list[dict[str, Any]]:
    resp = server.get(f"{CHATS}/{chat_id}/messages", user, params={"limit": 100})
    assert resp.status_code == 200, resp.text
    return resp.json()["items"]


def _turn_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id",
        (db_uuid(chat_id),),
    )


def _message_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT id, request_id, role, content, model, input_tokens, output_tokens, "
        "provider_response_id, deleted_at FROM messages WHERE chat_id = ? ORDER BY created_at, id",
        (db_uuid(chat_id),),
    )


def _catalog_entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the catalog")


def _assert_stream_started(data: Any) -> None:
    assert isinstance(data, dict), data
    keys = set(data)
    assert {"request_id", "message_id", "is_new_turn"} <= keys, data
    assert keys <= STREAM_STARTED_FIELDS, f"unexpected stream_started fields {keys - STREAM_STARTED_FIELDS}"
    assert is_uuid(data["request_id"]) and is_uuid(data["message_id"]), data
    assert isinstance(data["is_new_turn"], bool), data


def _assert_done_shape(data: Any) -> None:
    assert isinstance(data, dict), data
    keys = set(data)
    assert {"usage", "effective_model", "selected_model", "quota_decision"} <= keys, data
    assert keys <= DONE_FIELDS, f"unexpected done fields {keys - DONE_FIELDS}: {data}"
    assert set(data["usage"]) == {"input_tokens", "output_tokens"}, data["usage"]
    assert data["quota_decision"] in ("allow", "downgrade"), data


# --------------------------------------------------------------------------
# Happy path
# --------------------------------------------------------------------------


def test_stream_happy_path_event_sequence_and_payloads(mc, mock):
    chat = _new_chat(mc)
    chunks = ["The capital ", "of France ", "is Paris."]
    mock.enqueue({"chunks": chunks, "usage": {"input_tokens": 211, "output_tokens": 47}})
    request_id = str(uuid.uuid4())

    result = _send(mc, chat["id"], "What is the capital of France?", request_id=request_id)

    assert result.headers["content-type"].startswith("text/event-stream"), result.headers
    assert result.headers.get("cache-control") == "no-cache", result.headers
    assert TEXT_STREAM_ORDER.match(" ".join(result.names())), result.names()

    started = result.first("stream_started")
    _assert_stream_started(started)
    assert started["request_id"] == request_id
    assert started["is_new_turn"] is True
    assert "thread_summary_applied" not in started, started

    for delta in result.of("delta"):
        assert delta == {"type": "text", "content": delta["content"]}, delta
        assert isinstance(delta["content"], str)
    assert [d["content"] for d in result.of("delta")] == chunks
    assert result.text() == "".join(chunks)

    done = result.first("done")
    _assert_done_shape(done)
    assert done["usage"] == {"input_tokens": 211, "output_tokens": 47}
    assert done["effective_model"] == done["selected_model"] == chat["model"] == DEFAULT_MODEL
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done, done
    assert "downgrade_reason" not in done, done
    for leaked in ("request_id", "message_id", "provider_response_id", "response_id", "id"):
        assert leaked not in done, f"done must not carry {leaked!r}: {done}"


def test_stream_closes_right_after_done(mc, mock):
    chat = _new_chat(mc)
    mock.enqueue({"chunks": ["Short ", "answer."]})

    result = _send(mc, chat["id"], "Close test")

    assert result.names().count("done") == 1
    assert result.names().count("error") == 0
    assert result.names()[-1] == "done", "no event may follow the terminal event"
    assert result.transport_error is None, result.transport_error
    assert result.closed_at is not None, "server kept the SSE connection open after done"
    done_at = result.times("done")[0]
    assert result.closed_at - done_at < 5.0, (
        f"stream closed {result.closed_at - done_at:.1f}s after done"
    )


def test_stream_payloads_never_expose_provider_identifiers(mc, mock):
    chat = _new_chat(mc)
    mock.enqueue({"chunks": ["Identifier ", "free."]})

    result = _send(mc, chat["id"], "Leak check")

    response_id = _stream_requests(mock)[-1]["response_id"]
    assert response_id and response_id.startswith("resp_")
    assert response_id not in result.raw
    assert not re.search(r"\b(resp_|msg_|chatcmpl-)[A-Za-z0-9]{8,}", result.raw), result.raw


def test_stream_without_request_id_gets_server_generated_uuid_v4(mc, mock):
    chat = _new_chat(mc)

    result = _send(mc, chat["id"], "No request id here")

    started = result.first("stream_started")
    _assert_stream_started(started)
    generated = uuid.UUID(started["request_id"])
    assert generated.version == 4, f"server-generated request_id must be UUID v4: {generated}"
    assert started["is_new_turn"] is True

    turn = mc.get(_turn_path(chat["id"], started["request_id"]), "A1")
    assert turn.status_code == 200, turn.text
    assert turn.json()["state"] == "done"
    messages = _list_messages(mc, chat["id"])
    assert [m["request_id"] for m in messages] == [started["request_id"]] * 2


def test_stream_null_request_id_is_treated_as_omitted(mc, mock):
    chat = _new_chat(mc)

    result = _send(mc, chat["id"], "Explicit null request id", request_id=None)

    started = result.first("stream_started")
    assert uuid.UUID(started["request_id"]).version == 4


def test_stream_accepts_client_request_id_of_any_uuid_version(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid1())

    result = _send(mc, chat["id"], "Version 1 request id", request_id=request_id)

    assert result.first("stream_started")["request_id"] == request_id


# --------------------------------------------------------------------------
# Persistence after done
# --------------------------------------------------------------------------


def test_stream_persists_user_and_assistant_messages(mc, mock):
    chat = _new_chat(mc)
    chunks = ["Two plus ", "two is ", "four."]
    mock.enqueue({"chunks": chunks, "usage": {"input_tokens": 133, "output_tokens": 9}})
    request_id = str(uuid.uuid4())

    result = _send(mc, chat["id"], "What is 2+2?", request_id=request_id)
    message_id = result.first("stream_started")["message_id"]

    messages = _list_messages(mc, chat["id"])
    assert [m["role"] for m in messages] == ["user", "assistant"], messages
    user, assistant = messages
    assert user["content"] == "What is 2+2?"
    assert user["request_id"] == request_id
    assert user["attachments"] == []
    for key in ("model", "input_tokens", "output_tokens"):
        assert key not in user, f"user message must omit {key}: {user}"
    assert assistant["id"] == message_id
    assert assistant["request_id"] == request_id
    assert assistant["content"] == "".join(chunks)
    assert assistant["model"] == result.first("done")["effective_model"] == DEFAULT_MODEL
    assert assistant["input_tokens"] == 133
    assert assistant["output_tokens"] == 9
    assert parse_ts_ns(user["created_at"]) < parse_ts_ns(assistant["created_at"])

    rows = _message_rows(mc, chat["id"])
    assert [r["role"] for r in rows] == ["user", "assistant"]
    assert rows[1]["id"] == message_id
    assert rows[0]["request_id"] == rows[1]["request_id"] == request_id
    assert rows[1]["model"] == DEFAULT_MODEL
    assert rows[1]["provider_response_id"] == _stream_requests(mock)[-1]["response_id"]
    assert all(r["deleted_at"] is None for r in rows)


def test_stream_finalizes_turn_row(mc, mock):
    chat = _new_chat(mc)
    mock.enqueue({"chunks": ["Finalized ", "answer."], "usage": {"input_tokens": 77, "output_tokens": 5}})
    request_id = str(uuid.uuid4())

    result = _send(mc, chat["id"], "Finalize me", request_id=request_id)
    message_id = result.first("stream_started")["message_id"]

    rows = _turn_rows(mc, chat["id"])
    assert len(rows) == 1, rows
    turn = rows[0]
    assert turn["request_id"] == request_id
    assert turn["state"] == "completed"
    assert turn["requester_type"] == "user"
    assert turn["requester_user_id"] == mc.identity("A1").user_id
    assert turn["assistant_message_id"] == message_id
    assert turn["error_code"] is None
    assert turn["deleted_at"] is None
    assert turn["completed_at"] is not None
    assert turn["completed_at"] >= turn["started_at"]
    assert turn["effective_model"] == DEFAULT_MODEL
    catalog_max = _catalog_entry(mc, DEFAULT_MODEL)["max_output_tokens"]
    applied = min(catalog_max, STREAMING_MAX_OUTPUT_TOKENS)
    assert turn["max_output_tokens_applied"] == applied
    assert turn["reserve_tokens"] is not None and turn["reserve_tokens"] > applied, turn
    assert turn["reserved_credits_micro"] is not None and turn["reserved_credits_micro"] > 0, turn
    assert turn["policy_version_applied"] == POLICY_VERSION
    assert turn["minimal_generation_floor_applied"] == min(MINIMAL_GENERATION_FLOOR, applied)
    assert turn["web_search_enabled"] == 0
    response_id = _stream_requests(mock)[-1]["response_id"]
    assert turn["provider_response_id"] == response_id


def test_stream_provider_response_id_stays_internal(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())

    _send(mc, chat["id"], "Internal id", request_id=request_id)
    response_id = _stream_requests(mock)[-1]["response_id"]

    assert _turn_rows(mc, chat["id"])[0]["provider_response_id"] == response_id
    for path in (
        f"{CHATS}/{chat['id']}/messages",
        _turn_path(chat["id"], request_id),
        f"{CHATS}/{chat['id']}",
    ):
        resp = mc.get(path, "A1")
        assert resp.status_code == 200, resp.text
        assert response_id not in resp.text, f"provider response id leaked by GET {path}"


def test_stream_turn_status_is_done_after_stream(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())

    result = _send(mc, chat["id"], "Status please", request_id=request_id)

    resp = mc.get(_turn_path(chat["id"], request_id), "A1")
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["request_id"] == request_id
    assert body["state"] == "done"
    assert body["assistant_message_id"] == result.first("stream_started")["message_id"]
    assert "error_code" not in body


def test_stream_updates_chat_message_count_and_activity(mc, mock):
    title = f"s3-activity-{uuid.uuid4().hex[:12]}"
    target = _new_chat(mc, title=title)
    time.sleep(0.01)
    newer = _new_chat(mc, title=title)

    def _listed() -> list[dict[str, Any]]:
        resp = mc.get(CHATS, "A1", params={"$filter": f"title eq '{title}'"})
        assert resp.status_code == 200, resp.text
        return resp.json()["items"]

    assert [c["id"] for c in _listed()] == [newer["id"], target["id"]]

    _send(mc, target["id"], "Bump my chat")

    detail = mc.get(f"{CHATS}/{target['id']}", "A1")
    assert detail.status_code == 200, detail.text
    assert detail.json()["message_count"] == 2
    assert parse_ts_ns(detail.json()["updated_at"]) > parse_ts_ns(target["updated_at"])
    listed = _listed()
    assert [c["id"] for c in listed] == [target["id"], newer["id"]], "chat must move first"
    assert listed[0]["message_count"] == 2


# --------------------------------------------------------------------------
# Provider request
# --------------------------------------------------------------------------


def test_stream_provider_request_shape(mc, mock):
    chat = _new_chat(mc)
    content = "Explain photosynthesis briefly."

    _send(mc, chat["id"], content)

    requests = _stream_requests(mock)
    assert len(requests) == 1, [r["path"] for r in requests]
    req = requests[0]
    assert req["method"] == "POST"
    assert req["path"].endswith("/v1/responses"), req["path"]
    assert req["headers"].get("authorization") == f"Bearer {OPENAI_SECRET_VALUE}", (
        "OAGW must inject the credstore secret as the Authorization header"
    )
    body = req["json"]
    assert isinstance(body, dict), req
    entry = _catalog_entry(mc, DEFAULT_MODEL)
    assert body["model"] == entry["provider_model_id"] == DEFAULT_PROVIDER_MODEL
    assert body["stream"] is True
    assert body["max_output_tokens"] == min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)
    assert body["max_tool_calls"] == entry["max_tool_calls"]
    api_params = entry["general_config"]["api_params"]
    assert body["temperature"] == api_params["temperature"]
    assert body["top_p"] == api_params["top_p"]
    assert not body.get("tools"), f"no tools without web search or attachments: {body.get('tools')}"


def test_stream_provider_request_identity_metadata(mc, mock):
    chat = _new_chat(mc)
    ident = mc.identity("A1")

    _send(mc, chat["id"], "Who am I?")

    body = _stream_requests(mock)[-1]["json"]
    expected_user = uuid.UUID(ident.tenant_id).hex + uuid.UUID(ident.user_id).hex
    assert body["user"] == expected_user
    assert len(body["user"]) == 64
    assert body["metadata"] == {
        "tenant_id": ident.tenant_id,
        "user_id": ident.user_id,
        "chat_id": chat["id"],
        "request_type": "chat",
        "feature": "none",
    }


def test_stream_provider_request_uses_identity_of_the_caller(mc, mock):
    chat = _new_chat(mc, user="B1")
    ident = mc.identity("B1")

    _send(mc, chat["id"], "Tenant B speaking", user="B1")

    body = _stream_requests(mock)[-1]["json"]
    assert body["user"] == uuid.UUID(ident.tenant_id).hex + uuid.UUID(ident.user_id).hex
    assert body["metadata"]["tenant_id"] == ident.tenant_id
    assert body["metadata"]["user_id"] == ident.user_id


def test_stream_provider_request_system_prompt_and_user_input(mc, mock):
    chat = _new_chat(mc)
    content = "Name three primary colors."

    _send(mc, chat["id"], content)

    body = _stream_requests(mock)[-1]["json"]
    assert _system_instructions(body) == _catalog_entry(mc, DEFAULT_MODEL)["system_prompt"]
    conversation = _conversation(body)
    assert conversation == [("user", content)], conversation


def test_stream_provider_request_uses_the_chat_model(mc, mock):
    chat = _new_chat(mc, model="gpt-4.1-mini")
    entry = _catalog_entry(mc, "gpt-4.1-mini")

    result = _send(mc, chat["id"], "Mini model please")

    body = _stream_requests(mock)[-1]["json"]
    assert body["model"] == entry["provider_model_id"]
    assert body["max_output_tokens"] == min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)
    done = result.first("done")
    assert done["effective_model"] == done["selected_model"] == "gpt-4.1-mini"
    assert done["quota_decision"] == "allow"


def test_stream_second_turn_sends_history_in_chronological_order(mc, mock):
    chat = _new_chat(mc)
    mock.enqueue({"chunks": ["Apples ", "are red."]}, {"chunks": ["Bananas ", "are yellow."]})

    _send(mc, chat["id"], "What color are apples?")
    _send(mc, chat["id"], "And bananas?")

    requests = _stream_requests(mock)
    assert len(requests) == 2
    assert _conversation(requests[0]["json"]) == [("user", "What color are apples?")]
    assert _conversation(requests[1]["json"]) == [
        ("user", "What color are apples?"),
        ("assistant", "Apples are red."),
        ("user", "And bananas?"),
    ]
    assert _system_instructions(requests[1]["json"]) == _catalog_entry(mc, DEFAULT_MODEL)["system_prompt"]


# --------------------------------------------------------------------------
# Provider event translation (text)
# --------------------------------------------------------------------------


def test_stream_incomplete_response_finishes_with_done(mc, mock):
    chat = _new_chat(mc)
    chunks = ["This answer was ", "cut short"]
    mock.enqueue(
        {
            "kind": "incomplete",
            "incomplete_reason": "max_output_tokens",
            "chunks": chunks,
            "usage": {"input_tokens": 64, "output_tokens": 12},
        }
    )
    request_id = str(uuid.uuid4())

    result = _send(mc, chat["id"], "Make it long", request_id=request_id)

    assert TEXT_STREAM_ORDER.match(" ".join(result.names())), result.names()
    assert "error" not in result.names()
    assert "citations" not in result.names()
    done = result.first("done")
    _assert_done_shape(done)
    assert done["usage"] == {"input_tokens": 64, "output_tokens": 12}
    assert result.text() == "".join(chunks)

    turn = _turn_rows(mc, chat["id"])[0]
    assert turn["state"] == "completed"
    assert turn["error_code"] is None
    assert turn["assistant_message_id"] == result.first("stream_started")["message_id"]
    assistant = _list_messages(mc, chat["id"])[-1]
    assert assistant["role"] == "assistant"
    assert assistant["content"] == "".join(chunks)
    status = mc.get(_turn_path(chat["id"], request_id), "A1").json()
    assert status["state"] == "done"
    assert "error_code" not in status


def test_stream_completed_without_text_persists_empty_assistant_message(mc, mock):
    chat = _new_chat(mc)
    mock.enqueue({"chunks": [], "usage": {"input_tokens": 20, "output_tokens": 0}})

    result = _timed_stream(mc, chat["id"], {"content": "Say nothing"})

    assert result.status_code == 200
    assert result.names()[0] == "stream_started"
    assert result.names()[-1] == "done", result.names()
    assert "delta" not in result.names()
    turn = _turn_rows(mc, chat["id"])[0]
    assert turn["state"] == "completed"
    assistant = _list_messages(mc, chat["id"])[-1]
    assert assistant["role"] == "assistant"
    assert assistant["content"] == ""
    assert assistant["id"] == result.first("stream_started")["message_id"]


# --------------------------------------------------------------------------
# Incremental relay and keepalive
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_stream_relays_deltas_incrementally(mc, mock):
    chat = _new_chat(mc)
    chunks = ["one ", "two ", "three ", "four"]
    mock.enqueue({"chunks": chunks, "delay_ms": 1000})

    result = _send(mc, chat["id"], "Count slowly")

    assert [d["content"] for d in result.of("delta")] == chunks
    delta_times = result.times("delta")
    done_at = result.times("done")[0]
    # The provider sleeps 1 s before every event after response.created: the
    # first delta leaves it at ~2 s, the terminal event at ~6 s.
    assert done_at - delta_times[0] >= 2.5, (
        f"first delta arrived only {done_at - delta_times[0]:.2f}s before done (buffered?)"
    )
    for earlier, later in zip(delta_times, delta_times[1:]):
        assert later - earlier >= 0.5, f"deltas arrived in a burst: {delta_times}"
    started_at = result.times("stream_started")[0]
    assert delta_times[0] - started_at < 4.5, "first delta was held back"


@pytest.mark.slow
def test_stream_ping_only_before_first_content(start_server, mock):
    server = start_server(
        {"gears": {"mini-chat": {"config": {"streaming": {"sse_ping_interval_seconds": 5}}}}},
        name="ping",
    )

    # Provider silent for 6.5 s before the response headers: >= 1 ping.
    chat = _new_chat(server)
    mock.enqueue({"initial_delay_ms": 6500, "chunks": ["Thought ", "about it."]})
    slow_start = _send(server, chat["id"], "Think first")

    names = slow_start.names()
    assert TEXT_STREAM_ORDER.match(" ".join(names)), names
    pings = slow_start.of("ping")
    assert pings, f"expected a ping during 6.5 s of idle time: {names}"
    assert all(p == {} for p in pings), pings
    first_delta = names.index("delta")
    assert all(name != "ping" for name in names[first_delta:]), names
    first_ping_at = slow_start.times("ping")[0]
    started_at = slow_start.times("stream_started")[0]
    assert 4.0 <= first_ping_at - started_at <= 6.5, (
        f"first ping {first_ping_at - started_at:.2f}s after stream_started (interval 5 s)"
    )

    # 5.5 s gaps after the first delta: no ping once content started.
    chat2 = _new_chat(server)
    mock.enqueue({"chunks": ["Alpha. ", "Beta."], "delay_ms": 5500})
    slow_chunks = _send(server, chat2["id"], "Slow chunks")

    names2 = slow_chunks.names()
    first_delta2 = names2.index("delta")
    assert all(name != "ping" for name in names2[first_delta2:]), names2
    assert names2[:1] == ["stream_started"]
    assert set(names2[1:first_delta2]) <= {"ping"}, names2
    assert slow_chunks.text() == "Alpha. Beta."


def test_stream_short_answer_has_no_ping_with_default_interval(mc, mock):
    chat = _new_chat(mc)

    result = _send(mc, chat["id"], "Quick one")

    assert "ping" not in result.names(), result.names()
