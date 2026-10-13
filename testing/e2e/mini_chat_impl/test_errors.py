"""Error mapping & sanitization (acceptance: Error Mapping & Sanitization)."""

import uuid

import pytest

from mchelpers import RT_CHAT, assert_problem, nonce


# Acceptance: Error mapping — canonical Problem envelope on REST errors
def test_problem_envelope(api):
    r = api.get(f"/v1/chats/{uuid.uuid4()}")
    assert r.status_code == 404
    assert "application/problem+json" in r.headers.get("content-type", "")
    body = assert_problem(r, 404, "not_found", resource_type=RT_CHAT)
    assert str(body["type"]).endswith("cf.core.err.not_found.v1~")
    assert isinstance(body["title"], str) and isinstance(body["detail"], str)
    assert "instance" in body or "trace_id" in body


# Acceptance: Error mapping — non-UUID path parameters on every resource
@pytest.mark.parametrize(
    "method,path",
    [
        ("GET", "/v1/chats/not-a-uuid"),
        ("DELETE", "/v1/chats/not-a-uuid"),
        ("GET", "/v1/chats/not-a-uuid/messages"),
        ("GET", "/v1/chats/{chat}/turns/not-a-uuid"),
        ("DELETE", "/v1/chats/{chat}/turns/not-a-uuid"),
        ("GET", "/v1/chats/{chat}/attachments/not-a-uuid"),
        ("DELETE", "/v1/chats/{chat}/messages/not-a-uuid/reaction"),
    ],
)
def test_invalid_path_params(api, method, path):
    c = api.create_chat()
    r = api.c.request(method, path.format(chat=c["id"]))
    assert_problem(r, 400, "invalid_argument", field_reason="invalid_path_params")


# Acceptance: Error mapping — JSON extractor errors (422 / 400 / 415) on streaming endpoints, no SSE
def test_json_extractor_errors_on_stream(api):
    c = api.create_chat()
    path = f"/v1/chats/{c['id']}/messages:stream"
    r = api.post(path, json={"content": 5})
    assert_problem(r, 422, "invalid_argument", field_reason="invalid_json_body")
    r = api.post(path, content=b'{"content": ', headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument", field_reason="json_syntax_error")
    r = api.post(path, content=b'{"content": "x"}', headers={"Content-Type": "text/plain"})
    assert_problem(r, 415, "invalid_argument", field_reason="missing_json_content_type")


# Acceptance: Error mapping — authentication errors
def test_unauthenticated(api_for):
    anon = api_for(None)
    r = anon.get("/v1/chats")
    assert r.status_code == 401
    if r.headers.get("content-type", "").startswith("application/problem+json"):
        assert r.json().get("context", {}).get("reason") in (None, "MISSING_BEARER", "AUTHN_FAILED")
    bad = api_for("definitely-not-a-token")
    r = bad.get("/v1/chats")
    assert r.status_code == 401
    r = bad.post("/v1/chats", json={})
    assert r.status_code == 401


# Acceptance: Error mapping — consistent across REST and streaming: pre-stream JSON, post-open SSE error
def test_rest_vs_stream_consistency(api):
    c = api.create_chat()
    pre = api.stream(c["id"], "")
    assert not pre.is_sse and pre.status_code == 400
    post = api.stream(c["id"], "fail [[fail]] " + nonce())
    assert post.is_sse and post.status_code == 200
    err = post.error
    assert set(err) == {"code", "message"}
    status = api.wait_turn_state(c["id"], post.request_id, {"error"})
    assert status["error_code"] == err["code"]


# Acceptance: Error mapping / Sanitization — provider-originated details never reach the client
@pytest.mark.parametrize("directive", ["[[fail]]", "[[error_event]]", "[[http500]]"])
def test_sanitized_provider_errors(api, directive):
    from mchelpers import FAKE_IDS_AND_SECRETS

    c = api.create_chat()
    s = api.stream(c["id"], f"x {directive} " + nonce())
    err = s.error
    assert err["code"] == "provider_error"
    for bad in FAKE_IDS_AND_SECRETS:
        assert bad not in err["message"], err
        assert bad not in s.raw
    assert "sk-test-e2e-fake-key" not in s.raw
    t = api.turn(c["id"], s.request_id).json()
    for bad in FAKE_IDS_AND_SECRETS:
        assert bad not in str(t)


# Acceptance: Error mapping — 404 masking: foreign and deleted resources look alike
def test_404_masking(api, api_for):
    c = api.create_chat()
    foreign = api_for("tok-b").get(f"/v1/chats/{c['id']}")
    missing = api.get(f"/v1/chats/{uuid.uuid4()}")
    assert foreign.status_code == missing.status_code == 404
    assert foreign.json()["context"].get("resource_type") == missing.json()["context"].get("resource_type") == RT_CHAT
