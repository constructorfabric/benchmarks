"""SSE event contract (DESIGN section 3.3 "SSE Event Definitions", "SSE Event Ordering")."""

import uuid

import pytest

from . import mock_provider as mp
from .helpers import api, create_chat, stream

pytestmark = pytest.mark.usefixtures("server")

TERMINAL = {"done", "error"}


def _assert_well_formed(names):
    assert names[0] == "stream_started"
    assert names[-1] in TERMINAL
    assert sum(1 for n in names if n in TERMINAL) == 1
    assert names.count("citations") <= 1
    first_content = next((i for i, n in enumerate(names) if n in ("delta", "tool")), len(names))
    assert all(n != "ping" for n in names[first_content:])


def test_done_payload_shape(reset_mock):
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    _assert_well_formed(res.names())
    done = res.of("done")[0]
    assert "request_id" not in done and "message_id" not in done
    assert set(done["usage"]) == {"input_tokens", "output_tokens"}
    assert {"usage", "effective_model", "selected_model", "quota_decision"} <= set(done)
    started = res.of("stream_started")[0]
    assert set(started) == {"request_id", "message_id", "is_new_turn"}
    uuid.UUID(started["message_id"])


def test_provider_failure_is_terminal_error_event(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("par")],
                                     "failed": {"code": "server_error", "message": "upstream exploded"}})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    assert res.status == 200
    _assert_well_formed(res.names())
    assert res.names() == ["stream_started", "delta", "error"]
    err = res.of("error")[0]
    assert set(err) == {"code", "message"}
    assert err["code"] == "provider_error"
    assert "upstream exploded" in err["message"]


def test_tool_events_are_relayed(reset_mock):
    reset_mock.enqueue("responses", {"events": [
        mp.ev_created(), mp.ev_web_search(), mp.ev_web_search(done=True), mp.ev_delta("ok"), mp.ev_completed()]})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "news?", "web_search": {"enabled": True}})
    _assert_well_formed(res.names())
    assert res.of("tool") == [
        {"phase": "start", "name": "web_search", "details": {}},
        {"phase": "done", "name": "web_search", "details": {}},
    ]


def test_pings_are_sent_while_the_model_is_silent(reset_mock):
    # The harness sets sse_ping_interval_seconds to 5; delay the first token past it
    # (but below OAGW's 10 s proxy timeout).
    reset_mock.enqueue("responses", {"events": [
        mp.ev_created(), mp.ev_delta("late") | {"delay_ms": 7_000}, mp.ev_completed()]})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"}, timeout=60)
    _assert_well_formed(res.names())
    assert res.names() == ["stream_started", "ping", "delta", "done"], res.names()
    assert res.of("ping")[0] == {}


def test_openapi_declares_the_stream_operation():
    doc = api(None).get("/openapi.json").json()
    op = doc["paths"]["/mini-chat/v1/chats/{id}/messages:stream"]["post"]
    assert op["operationId"] == "mini_chat.stream_message"
    assert op["tags"] == ["Mini Chat Messages"]
    assert op["requestBody"]["content"]["application/json"]["schema"]["$ref"].endswith("/StreamMessageRequest")
    sse = op["responses"]["200"]["content"]["text/event-stream"]["schema"]["$ref"]
    assert sse.endswith("/MiniChatSseEvent")
    assert {"400", "401", "403", "404", "409", "422", "429", "500", "503"} <= set(op["responses"])
    schemas = doc["components"]["schemas"]
    for name in ("StreamStartedData", "ThreadSummaryInfo", "PingData", "DeltaData", "DeltaKind", "ToolData",
                 "ToolPhase", "CitationsData", "Citation", "CitationSource", "TextSpan", "DoneData", "Usage",
                 "ErrorData", "WebSearchConfig"):
        assert name in schemas, name
    events = [v["properties"]["event"]["enum"][0] for v in schemas["MiniChatSseEvent"]["oneOf"]]
    assert events == ["stream_started", "ping", "delta", "tool", "citations", "done", "error"]
