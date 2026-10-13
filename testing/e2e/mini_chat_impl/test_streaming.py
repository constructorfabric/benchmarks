"""Send message streaming (acceptance: Streaming: Send Message; Principles: no buffering)."""

import uuid

import pytest

from mchelpers import (
    RT_CHAT,
    as_uuid,
    assert_problem,
    assert_sse_grammar,
    hex32,
    nonce,
    parse_ts,
    tenant_id,
    user_id,
)


# Acceptance: Streaming — normal turn: stream_started, two deltas, done, in order
def test_normal_turn_event_sequence(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"hello {n}")
    assert s.status_code == 200
    assert "text/event-stream" in s.headers["content-type"]
    assert "no-cache" in s.headers.get("cache-control", "")
    assert_sse_grammar(s)
    assert s.names(include_ping=False) == ["stream_started", "delta", "delta", "done"]
    st = s.started
    assert st["is_new_turn"] is True
    uuid.UUID(st["request_id"])
    uuid.UUID(st["message_id"])
    assert s.of("delta") == [{"type": "text", "content": "Hello"}, {"type": "text", "content": " world"}]


# Acceptance: Streaming — request_id: server-generated v4 when omitted, client value of any version kept
def test_request_id_generated_and_client_supplied(api):
    c = api.create_chat()
    s = api.stream(c["id"], "gen " + nonce())
    assert uuid.UUID(s.request_id).version == 4
    rid = uuid.uuid1()  # any UUID version is accepted
    s2 = api.stream(c["id"], "client " + nonce(), request_id=rid)
    assert s2.request_id == str(rid)
    assert s2.done


# Acceptance: Streaming — request reaches the provider through OAGW with the configured auth header
def test_provider_request_via_oagw(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"oagw {n}").done
    reqs = mock_llm.chat_requests(contains=n)
    assert len(reqs) == 1
    req = reqs[0]
    assert req["path"] == "/v1/responses"
    assert req["headers"].get("authorization") == "Bearer sk-test-e2e-fake-key"
    assert req["json"]["stream"] is True
    assert req["json"]["model"] == "mock-prem"


# Acceptance: Streaming — assistant message and usage persisted once the stream completes
def test_persistence_on_completion(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "persist " + nonce())
    done = s.done
    assert done["usage"] == {"input_tokens": 10, "output_tokens": 5}
    msgs = api.messages(c["id"])
    a = msgs[-1]
    assert a["id"] == s.message_id and a["content"] == "Hello world"
    assert a["input_tokens"] == 10 and a["output_tokens"] == 5
    assert a["model"] == done["effective_model"]
    t = api.turn(c["id"], s.request_id).json()
    assert t["state"] == "done" and t["assistant_message_id"] == s.message_id
    row = db.turn_row(c["id"], s.request_id)
    assert row["state"] == "completed"
    assert row["completed_at"] is not None
    assert as_uuid(row["assistant_message_id"]) == s.message_id
    # a usage outbox message was enqueued in the finalization transaction (tolerant: payload search)
    assert db.outbox_count(hex32(s.request_id)) + db.outbox_count(s.request_id) >= 1


# Acceptance: Streaming — chat updated_at is bumped by a sent message
def test_send_bumps_chat_updated_at(api):
    c = api.create_chat()
    before = parse_ts(api.chat(c["id"])["updated_at"])
    assert api.stream(c["id"], "bump " + nonce()).done
    assert parse_ts(api.chat(c["id"])["updated_at"]) > before


# Acceptance: Streaming — preflight validation (empty content) before any provider call; JSON error, no SSE
@pytest.mark.parametrize("content", ["", "   ", "\n\t "])
def test_empty_content_rejected_before_provider(api, mock_llm, db, content):
    c = api.create_chat()
    before = len(mock_llm.chat_requests())
    s = api.stream(c["id"], content)
    assert not s.is_sse
    r = api.as_response(s)
    assert "application/problem+json" in s.headers.get("content-type", "") or "json" in s.headers.get("content-type", "")
    assert_problem(r, 400, "invalid_argument", field_reason="EMPTY_CONTENT", field="content")
    assert len(mock_llm.chat_requests()) == before
    assert db.turns(c["id"]) == []


# Acceptance: Streaming — unknown chat / non-UUID chat id / malformed body
def test_stream_bad_targets(api):
    s = api.stream(str(uuid.uuid4()), "x")
    assert_problem(api.as_response(s), 404, "not_found", resource_type=RT_CHAT)
    s = api.stream("not-a-uuid", "x")
    assert_problem(api.as_response(s), 400, "invalid_argument", field_reason="invalid_path_params")
    c = api.create_chat()
    r = api.post(f"/v1/chats/{c['id']}/messages:stream", json={"content": "x", "request_id": "nope"})
    assert_problem(r, 422, "invalid_argument")
    r = api.post(f"/v1/chats/{c['id']}/messages:stream", json={"content": "x", "attachment_ids": ["nope"]})
    assert_problem(r, 422, "invalid_argument")
    r = api.post(f"/v1/chats/{c['id']}/messages:stream", json={})
    assert_problem(r, 422, "invalid_argument")


# Acceptance: Principles — streaming responses are never buffered before relaying
@pytest.mark.timeout(60)
def test_no_buffering(api):
    c = api.create_chat()
    s = api.stream(c["id"], "slow please [[slow:3:2]] " + nonce())
    assert s.done
    first_delta_t = next(t for (e, _), t in zip(s.events, s.times) if e == "delta")
    done_t = s.times[-1]
    # the provider spends >= 4 s between the first and the last delta
    assert done_t - first_delta_t >= 3.0, f"first delta at {first_delta_t:.2f}s, done at {done_t:.2f}s"
    delta_times = [t for (e, _), t in zip(s.events, s.times) if e == "delta"]
    assert len(delta_times) == 3
    assert delta_times[1] - delta_times[0] >= 1.0


# Acceptance: Context assembly — provider request carries user/metadata identifiers
def test_provider_request_identity_fields(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"ids {n}").done
    body = mock_llm.chat_requests(contains=n)[0]["json"]
    expected_user = hex32(tenant_id("tok-a")) + hex32(user_id("tok-a"))
    assert str(body.get("user", "")).replace("-", "").lower() == expected_user
    md = body.get("metadata") or {}
    assert {"tenant_id", "user_id", "chat_id", "request_type"} <= set(md), md
    assert md["request_type"] == "chat"
    assert str(md["chat_id"]).replace("-", "").lower() == hex32(c["id"])
    assert "feature" in md
