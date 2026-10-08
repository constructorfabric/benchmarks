"""SSE event contract (acceptance: SSE Event Contract; Error Mapping & Sanitization)."""

import json

import pytest

from mchelpers import (
    FAKE_IDS_AND_SECRETS,
    assert_sse_grammar,
    nonce,
)

DONE_ALLOWED = {"usage", "effective_model", "selected_model", "quota_decision", "downgrade_from", "downgrade_reason", "quota_warnings"}


def _assert_sanitized(message: str):
    for bad in FAKE_IDS_AND_SECRETS:
        assert bad not in message, f"provider detail {bad!r} leaked in {message!r}"
    assert "http://" not in message and "https://" not in message, message


# Acceptance: SSE — done exposes usage and quota outcome without internal identifiers
def test_done_payload(api, mock_llm):
    c = api.create_chat()
    n = nonce()
    s = api.stream(c["id"], f"done fields {n}")
    d = s.done
    assert set(d) <= DONE_ALLOWED, f"unexpected done fields: {set(d) - DONE_ALLOWED}"
    assert d["usage"] == {"input_tokens": 10, "output_tokens": 5}
    assert d["selected_model"] == "prem"
    assert d["effective_model"] == "prem"
    assert d["quota_decision"] == "allow"
    assert d.get("downgrade_from") is None and d.get("downgrade_reason") is None
    assert "message_id" not in d and "request_id" not in d
    # quota_warnings are present on CAS-winning completed turns
    assert isinstance(d.get("quota_warnings"), list), d
    for w in d["quota_warnings"]:
        assert {"tier", "period", "remaining_percentage", "warning", "exhausted"} <= set(w)
        assert w["tier"] in ("premium", "total") and w["period"] in ("daily", "monthly")
        if not (w["warning"] or w["exhausted"]):
            assert w.get("next_reset") is None
    # no provider identifier anywhere in the stream
    resp_id = mock_llm.chat_requests(contains=n)[0]["response_id"]
    assert resp_id not in s.raw
    assert resp_id not in json.dumps(s.events)


# Acceptance: SSE — tool activity + web citations, ordering (citations once, before done)
def test_web_search_tool_and_citations(api):
    c = api.create_chat()
    s = api.stream(c["id"], "search the web [[websearch:1]] " + nonce(), web_search=True)
    assert_sse_grammar(s)
    tools = s.of("tool")
    assert {"phase": "start", "name": "web_search"} == {k: tools[0][k] for k in ("phase", "name")}
    assert [(t["phase"], t["name"]) for t in tools] == [("start", "web_search"), ("done", "web_search")]
    for t in tools:
        assert isinstance(t.get("details"), dict)
    cits = s.of("citations")
    assert len(cits) == 1
    names = s.names(include_ping=False)
    assert names.index("citations") == len(names) - 2
    items = cits[0]["items"]
    web = [i for i in items if i["source"] == "web"]
    assert web, items
    w = web[0]
    assert w["url"] == "https://example.com/mock-article"
    assert w["title"] == "Mock Article"
    assert isinstance(w["snippet"], str)
    # no annotation text: the snippet is the answer text in the annotation range
    assert w["snippet"] == "Hello"
    if w.get("span") is not None:
        assert w["span"] == {"start": 0, "end": 5}
    assert s.done


# Acceptance: SSE — code interpreter tool events (start / done with logs output)
def test_code_interpreter_tool_events(api):
    from mchelpers import MIME_XLSX, make_xlsx

    c = api.create_chat()
    api.upload_ready(c["id"], "sheet.xlsx", make_xlsx(), MIME_XLSX)
    s = api.stream(c["id"], "analyse [[codeint:1]] " + nonce())
    assert_sse_grammar(s)
    tools = [(t["phase"], t["name"]) for t in s.of("tool")]
    assert tools == [("start", "code_interpreter"), ("done", "code_interpreter")]
    done_tool = s.of("tool")[1]
    assert done_tool["details"].get("output") == "ci-output-0"
    assert s.done


# Acceptance: SSE — keepalive ping only between stream_started and the first delta
@pytest.mark.timeout(90)
def test_ping_before_first_delta(api):
    c = api.create_chat()
    s = api.stream(c["id"], "think first [[delay:12]] " + nonce(), timeout=60)
    assert_sse_grammar(s)
    names = s.names()
    assert "ping" in names, names
    first_delta = names.index("delta")
    assert all(i < first_delta for i, n in enumerate(names) if n == "ping")
    for p in s.of("ping"):
        assert p == {} or p is None
    assert s.done


# Acceptance: SSE — error event is terminal and sanitized (response.failed)
def test_provider_failed_error_event(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "fail please [[fail]] " + nonce())
    assert_sse_grammar(s)
    err = s.error
    assert set(err) == {"code", "message"}
    assert err["code"] == "provider_error"
    msg = err["message"]
    _assert_sanitized(msg)
    assert "[provider_id]" in msg
    assert "[url]" in msg
    assert "[credential]" in msg
    assert "Upstream failure for [provider_id]" in msg
    assert s.of("done") == []
    t = api.wait_turn_state(c["id"], s.request_id, {"error"})
    assert t["error_code"] == "provider_error"
    assert not t.get("assistant_message_id")
    assert db.turn_row(c["id"], s.request_id)["state"] == "failed"


# Acceptance: SSE — top-level provider `error` event maps to provider_error, sanitized
def test_provider_error_event(api):
    c = api.create_chat()
    s = api.stream(c["id"], "boom [[error_event]] " + nonce())
    assert s.error["code"] == "provider_error"
    assert "vs_abcdefghijklmnop1234" not in s.error["message"]
    assert "internal.example.net" not in s.error["message"]


# Acceptance: SSE — provider HTTP 500 -> provider_error (sanitized)
def test_provider_http_500(api):
    c = api.create_chat()
    s = api.stream(c["id"], "500 [[http500]] " + nonce())
    assert s.is_sse, "a provider failure after preflight is reported on the stream"
    assert_sse_grammar(s)
    assert s.error["code"] == "provider_error"
    _assert_sanitized(s.error["message"])
    assert api.wait_turn_state(c["id"], s.request_id, {"error"})["error_code"] == "provider_error"


# Acceptance: SSE — provider 429 -> rate_limited with the Retry-After delay in the message
def test_provider_http_429(api):
    c = api.create_chat()
    s = api.stream(c["id"], "429 [[http429]] " + nonce())
    assert s.error["code"] == "rate_limited"
    assert "7" in s.error["message"]
    assert api.wait_turn_state(c["id"], s.request_id, {"error"})["error_code"] == "rate_limited"


# Acceptance: SSE — incomplete provider response finalizes as done without citations
def test_incomplete_is_done(api):
    c = api.create_chat()
    s = api.stream(c["id"], "cut [[incomplete]] " + nonce())
    assert_sse_grammar(s)
    assert s.done["usage"] == {"input_tokens": 10, "output_tokens": 5}
    assert s.of("citations") == []
    t = api.wait_turn_state(c["id"], s.request_id, {"done"})
    assert not t.get("error_code")


# Acceptance: SSE — completion without text deltas still ends with done
def test_empty_completion(api):
    c = api.create_chat()
    s = api.stream(c["id"], "nothing [[empty]] " + nonce())
    assert_sse_grammar(s)
    assert s.of("delta") == []
    assert s.done
    assert api.wait_turn_state(c["id"], s.request_id, {"done"})


# Acceptance: SSE — no citations event when no citation maps (unknown provider file id)
def test_unmapped_file_citation_dropped(api):
    from mchelpers import make_pdf

    c = api.create_chat()
    api.upload_ready(c["id"], "doc.pdf", make_pdf(), "application/pdf")
    s = api.stream(c["id"], "cite [[filecite:unknown]] " + nonce())
    assert_sse_grammar(s)
    assert s.of("citations") == []
    assert s.done


# Acceptance: SSE — the connection closes right after the terminal event
def test_stream_closes_after_terminal(api):
    c = api.create_chat()
    s = api.stream(c["id"], "close " + nonce())
    # api.stream returned (EOF) and nothing came after the terminal event
    assert s.events[-1][0] == "done"
    assert not s.disconnected_early
