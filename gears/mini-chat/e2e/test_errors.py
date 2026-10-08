"""Canonical error contract (REST Problems, SSE error events) and provider error sanitization.

Acceptance criteria covered:
* Error Mapping — "All errors map to the canonical error contract, consistently across REST and streaming"
* Error Mapping — "Provider-originated error details are sanitized before reaching the client"
* SSE — "Error event is terminal and carries a sanitized message"
"""

from __future__ import annotations

import uuid

import httpx
import pytest

from helpers import (
    RT_CHAT,
    assert_error_stream,
    assert_problem,
    field_reasons,
    http_error,
    new_chat,
    no_provider_ids,
    problem,
    send_ok,
    stream_script,
    turn_row,
    turn_status,
)


def test_problem_shape(fresh):
    r = fresh.req("GET", f"/chats/{uuid.uuid4()}")
    j = problem(r, 404)
    assert set(j) >= {"type", "title", "status", "detail", "instance", "context"}
    assert "code" not in j
    assert j["instance"] == r.request.url.path
    assert "trace_id" in j
    assert "json" in r.headers.get("content-type", "")
    assert j["context"]["resource_type"] == RT_CHAT
    assert "cf.core.err.not_found.v1~" in j["type"]


def test_non_uuid_path_params(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    bad = "not-a-uuid"
    cases = [
        ("GET", f"/chats/{bad}", None),
        ("PATCH", f"/chats/{bad}", {"title": "x"}),
        ("DELETE", f"/chats/{bad}", None),
        ("GET", f"/chats/{bad}/messages", None),
        ("POST", f"/chats/{bad}/messages:stream", {"content": "x"}),
        ("GET", f"/chats/{cid}/turns/{bad}", None),
        ("POST", f"/chats/{cid}/turns/{bad}/retry", None),
        ("PATCH", f"/chats/{cid}/turns/{bad}", {"content": "x"}),
        ("DELETE", f"/chats/{cid}/turns/{bad}", None),
        ("GET", f"/chats/{cid}/attachments/{bad}", None),
        ("DELETE", f"/chats/{cid}/attachments/{bad}", None),
        ("PUT", f"/chats/{cid}/messages/{bad}/reaction", {"reaction": "like"}),
        ("DELETE", f"/chats/{cid}/messages/{bad}/reaction", None),
    ]
    for method, path, body in cases:
        kw = {"json": body} if body is not None else {}
        r = fresh.req(method, path, **kw)
        j = problem(r, 400)
        assert "invalid_path_params" in field_reasons(j) or "invalid_path_params" in str(j), (method, path, j)
    assert fresh.chat_requests() == []


def test_json_body_errors(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    hdr = {"Content-Type": "application/json"}
    for method, path in (("POST", "/chats"), ("PATCH", f"/chats/{cid}"), ("POST", f"/chats/{cid}/messages:stream")):
        r = fresh.req(method, path, content=b'{"title": ', headers=hdr)
        j = problem(r, 400)
        assert "json_syntax_error" in field_reasons(j), j
        r = fresh.req(method, path, content=b'{"title":"x","content":"x"}', headers={"Content-Type": "text/plain"})
        j = problem(r, 415)
        assert "missing_json_content_type" in field_reasons(j), j
    r = fresh.req("POST", "/chats", json={"title": 123})
    j = problem(r, 422)
    assert "invalid_json_body" in field_reasons(j), j


def test_unauthenticated(fresh):
    r = httpx.get(f"{fresh.base}/chats", timeout=10)
    j = problem(r, 401)
    assert j["context"].get("reason") in ("MISSING_BEARER", "AUTHN_FAILED"), j
    r = httpx.get(f"{fresh.base}/chats", headers={"Authorization": "Bearer nope"}, timeout=10)
    problem(r, 401)
    r = httpx.post(f"{fresh.base}/chats/{uuid.uuid4()}/messages:stream", json={"content": "x"}, timeout=10)
    problem(r, 401)
    assert fresh.chat_requests() == []


# ── streaming errors ──────────────────────────────────────────────────────
def _fail_stream(srv, script, code):
    cid = new_chat(srv, "gpt-4.1-mini")
    srv.mock_script(script)
    r, events = srv.stream(cid, "trigger error")
    err = assert_error_stream(r, events, code)
    rid = events[0].data["request_id"]
    # Same code in the turn record and the status API (REST/stream consistency).
    assert turn_row(srv, cid, rid)["error_code"] == code
    ts = turn_status(srv, cid, rid).json()
    assert ts["state"] == "error" and ts["error_code"] == code
    no_provider_ids(err["message"])
    return err


def test_response_failed_is_sanitized(fresh):
    err = _fail_stream(
        fresh,
        stream_script(terminal="failed", error={"code": "server_error", "message": "bad file file-abcdefghijklmnop at https://x.y/z"}, usage={}),
        "provider_error",
    )
    assert err["message"] == "bad file [provider_id] at [url]"


def test_sse_error_event_sanitizes_ids_and_credentials(fresh):
    msg = "resp_abc123XYZ failed for vs_abcdefghijklmnop using sk-ABCDEFGHIJ1234 and Bearer abc.def.ghi; file-based file_search ok"
    err = _fail_stream(fresh, stream_script(terminal="error", error={"code": "server_error", "message": msg}), "provider_error")
    m = err["message"]
    assert "resp_abc123XYZ" not in m and "vs_abcdefghijklmnop" not in m
    assert "sk-ABCDEFGHIJ1234" not in m and "abc.def.ghi" not in m
    assert "[provider_id]" in m and "[credential]" in m
    assert "file-based" in m and "file_search" in m, "ordinary words stay intact"


def test_provider_http_error_maps_to_provider_error(fresh):
    err = _fail_stream(fresh, http_error(500, "internal failure for file-zyxwvutsrqponm see https://status.example.com"), "provider_error")
    assert "file-zyxwvutsrqponm" not in err["message"] and "https://status.example.com" not in err["message"]


def test_provider_400_maps_to_provider_error(fresh):
    _fail_stream(fresh, http_error(400, "invalid request"), "provider_error")


def test_provider_429_maps_to_rate_limited(fresh):
    err = _fail_stream(fresh, http_error(429, "slow down", headers={"Retry-After": "7"}), "rate_limited")
    assert "7" in err["message"]


def test_provider_timeout(fresh):
    """No response within the gateway timeout (OAGW proxy_timeout_secs = 10) → provider_timeout."""
    script = stream_script("never")
    script["pre_delay_ms"] = 13000
    _fail_stream(fresh, script, "provider_timeout")


def test_invalid_provider_stream_is_provider_error(fresh):
    script = stream_script(terminal=None)  # stream ends without a terminal event
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(script)
    r, events = fresh.stream(cid, "x")
    assert events[-1].event == "error"
    assert events[-1].data["code"] in ("provider_error", "stream_interrupted")
    rid = events[0].data["request_id"]
    assert turn_row(fresh, cid, rid)["state"] == "failed"


def test_pre_stream_errors_are_json_not_sse(fresh):
    """Validation/authorization/quota failures never open an SSE stream (Problem JSON instead)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    for body in ({"content": " "}, {"content": "x", "attachment_ids": [str(uuid.uuid4())]}):
        r = fresh.req("POST", f"/chats/{cid}/messages:stream", json=body)
        assert "text/event-stream" not in r.headers.get("content-type", "")
        problem(r, 400)
    r = fresh.req("POST", f"/chats/{uuid.uuid4()}/messages:stream", json={"content": "x"})
    problem(r, 404)


@pytest.mark.parametrize(
    "status,reason",
    [(409, "turn_already_running"), (409, "request_id_conflict")],
)
def test_conflicts_have_reason_not_code(fresh, status, reason):
    from helpers import bg_send, sleep, wait_running

    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("a", sleep(2000), "b"))
    bg = bg_send(fresh, cid, "x", request_id=rid)
    wait_running(fresh, cid)
    body = {"request_id": rid} if reason == "request_id_conflict" else {}
    r, _ = fresh.stream(cid, "y", **body)
    assert_problem(r, status, reason=reason, category="aborted")
    bg.wait()


def test_canonical_error_mapping_table(fresh, lim):
    """ADR-0004 table: category, HTTP status and machine-readable reason for representative conditions."""
    from helpers import RT_MODEL, RT_ODATA, list_messages, upload, upload_ok

    cid = new_chat(fresh, "gpt-4.1-mini")
    st, _, _ = send_ok(fresh, cid, "seed")
    user_msg = list_messages(fresh, cid)[0]["id"]
    cases = [
        (fresh.req("GET", "/models/disabled-model"), 404, dict(category="not_found", resource_type=RT_MODEL)),
        (fresh.req("POST", "/chats", json={"model": "disabled-model"}), 400, dict(category="invalid_argument", field_reason="INVALID_MODEL")),
        (fresh.req("POST", "/chats", json={"title": " "}), 400, dict(category="invalid_argument", field_reason="INVALID_TITLE")),
        (fresh.stream(cid, " ")[0], 400, dict(category="invalid_argument", field_reason="EMPTY_CONTENT")),
        (
            fresh.req("PUT", f"/chats/{cid}/messages/{st['message_id']}/reaction", json={"reaction": "meh"}),
            400,
            dict(category="invalid_argument", field_reason="INVALID_REACTION"),
        ),
        (fresh.req("GET", "/chats", params={"$filter": "x eq"}), 400, dict(category="invalid_argument", resource_type=RT_ODATA)),
        (fresh.stream(cid, "x", attachment_ids=[str(uuid.uuid4())])[0], 400, dict(category="invalid_argument", field_reason="invalid_attachment")),
        (
            upload(fresh, cid, "a.exe", b"MZ", "application/x-msdownload"),
            400,
            dict(category="invalid_argument", field_reason="UNSUPPORTED_CONTENT_TYPE"),
        ),
        (fresh.stream(new_chat(fresh, "tiny-ctx"), "q" * 13000)[0], 400, dict(category="out_of_range", field_reason="INPUT_TOO_LONG")),
        (
            fresh.req("PUT", f"/chats/{cid}/messages/{user_msg}/reaction", json={"reaction": "like"}),
            400,
            dict(category="failed_precondition", subject="reaction_target", vtype="STATE"),
        ),
        (
            fresh.req("POST", f"/chats/{cid}/turns/{uuid.uuid4()}/retry"),
            404,
            dict(category="not_found"),
        ),
    ]
    for r, status, kw in cases:
        assert_problem(r, status, **kw)
    # out_of_range FILE_TOO_LARGE and 429 document limit (limits server).
    lcid = new_chat(lim, "gpt-4.1-mini")
    assert_problem(upload(lim, lcid, "big.pdf", b"0" * (800 * 1024), "application/pdf"), 400, category="out_of_range", field_reason="FILE_TOO_LARGE")
    for i in range(3):
        upload_ok(lim, lcid, f"{i}.pdf")
    j = assert_problem(upload(lim, lcid, "x.pdf", b"%PDF", "application/pdf"), 429, category="resource_exhausted")
    assert "document_limit" in str(j["context"])
    # already_exists attachment_locked.
    att = upload_ok(fresh, cid, "locked.pdf")
    send_ok(fresh, cid, "use", attachment_ids=[att["id"]])
    assert_problem(fresh.req("DELETE", f"/chats/{cid}/attachments/{att['id']}"), 409, category="already_exists", resource_name="attachment_locked")
    # service_unavailable with Retry-After on storage failure.
    fresh.mock_config(file_upload_status=502)
    r = upload(fresh, cid, "s.pdf", b"%PDF", "application/pdf")
    j = assert_problem(r, 503, category="service_unavailable")
    assert r.headers.get("retry-after") == "10"
    assert j["context"].get("retry_after_seconds") == 10
