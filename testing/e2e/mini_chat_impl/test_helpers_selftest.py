"""Self-tests of the suite helpers (no gear server needed)."""

import sqlite3
import time
import uuid
from pathlib import Path

import httpx
import pytest

from mchelpers import (
    DB,
    Api,
    BackgroundStream,
    StreamResult,
    assert_problem,
    assert_sse_grammar,
    credits_micro,
    estimated_text_tokens,
    parse_sse_text,
    period_starts,
    shift_timestamp_old,
    ub,
    user_id,
)
from mock_llm import MockLLM

pytestmark = pytest.mark.noserver


@pytest.fixture(scope="module")
def mk():
    m = MockLLM().start()
    yield m
    m.stop()


def test_sse_parser_handles_comments_multiline_and_crlf():
    text = ": comment\r\nevent: a\r\ndata: {\"x\": 1}\r\n\r\nevent: b\ndata: line1\ndata: line2\n\ndata: {}\n\n"
    ev = parse_sse_text(text)
    assert ev == [("a", {"x": 1}), ("b", "line1\nline2"), ("message", {})]


def test_grammar_validator():
    ok = StreamResult(200, {}, events=[("stream_started", {}), ("ping", {}), ("delta", {}), ("tool", {}), ("citations", {}), ("done", {})])
    assert_sse_grammar(ok)
    for bad in (
        [("delta", {}), ("done", {})],
        [("stream_started", {}), ("delta", {}), ("ping", {}), ("done", {})],
        [("stream_started", {}), ("citations", {}), ("delta", {}), ("done", {})],
        [("stream_started", {}), ("done", {}), ("error", {})],
        [("stream_started", {}), ("delta", {})],
    ):
        with pytest.raises(AssertionError):
            assert_sse_grammar(StreamResult(200, {}, events=bad))


def test_credit_formulas():
    assert credits_micro(100, 50, "std") == 250
    assert credits_micro(100, 50, "prem") == 1050
    assert credits_micro(1, 1, "std") == 4
    # ceil((ceil(9/4) + 100) * 110 / 100) = ceil(103 * 1.1) = 114
    assert estimated_text_tokens("123456789") == 114


def test_assert_problem():
    body = {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid argument",
        "status": 400,
        "detail": "bad",
        "context": {"field_violations": [{"field": "title", "description": "d", "reason": "INVALID_TITLE"}]},
    }
    r = httpx.Response(400, json=body, headers={"content-type": "application/problem+json"})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_TITLE", field="title")
    with pytest.raises(AssertionError):
        assert_problem(r, 400, "not_found")
    with pytest.raises(AssertionError):
        assert_problem(r, 400, field_reason="INVALID_MODEL")


def test_shift_timestamp_keeps_format():
    assert shift_timestamp_old("2026-10-02 21:39:00.123456+00:00") == "2025-10-02 21:39:00.123456+00:00"
    assert shift_timestamp_old("2026-10-02T21:39:00Z") == "2025-10-02T21:39:00Z"
    assert shift_timestamp_old(1_800_000_000) < 1_800_000_000 - 3600


def test_api_sse_and_disconnect_against_mock(mk):
    api = Api(mk.base_url, None)
    api.c = httpx.Client(base_url=mk.base_url, timeout=30)
    body = {"model": "m", "stream": True, "input": [{"role": "user", "content": "x"}]}
    res = api.sse("POST", "/v1/responses", json_body=body)
    assert res.is_sse and res.events[-1][0] == "response.completed"
    assert len(res.times) == len(res.events)
    res = api.sse("POST", "/v1/responses", json_body={**body, "input": [{"role": "user", "content": "x [[slow:5:1]]"}]}, stop_after=4)
    assert res.disconnected_early and len(res.events) == 4
    # non-SSE responses are captured as JSON
    res = api.sse("POST", "/v1/responses", json_body={**body, "input": [{"role": "user", "content": "x [[http429]]"}]})
    assert not res.is_sse and res.status_code == 429 and res.body["error"]
    api.c.close()


def test_background_stream_stop_disconnects(mk):
    mk.reset()
    api = Api(mk.base_url.replace("/mini-chat", ""), None)
    bg = BackgroundStream(api, "POST", "/../v1/responses", {"model": "m", "stream": True, "input": [{"role": "user", "content": "x [[hang]]"}]})
    bg.start()
    deadline = time.time() + 5
    while time.time() < deadline and not mk.chat_requests():
        time.sleep(0.1)
    time.sleep(1.0)
    bg.stop()
    deadline = time.time() + 10
    while time.time() < deadline and not mk.chat_requests()[0].get("client_disconnected"):
        time.sleep(0.2)
    assert mk.chat_requests()[0].get("client_disconnected") is True
    api.close()


def test_db_helpers(tmp_path: Path):
    p = tmp_path / "mini_chat.db"
    con = sqlite3.connect(p)
    con.executescript(
        """
        CREATE TABLE quota_usage (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, user_id TEXT NOT NULL,
          period_type TEXT NOT NULL, period_start TEXT NOT NULL, bucket TEXT NOT NULL,
          spent_credits_micro INTEGER NOT NULL DEFAULT 0, reserved_credits_micro INTEGER NOT NULL DEFAULT 0,
          calls INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
          web_search_calls INTEGER NOT NULL DEFAULT 0, code_interpreter_calls INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL);
        CREATE TABLE toolkit_outbox_body (id INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB NOT NULL, payload_type TEXT NOT NULL);
        """
    )
    con.execute("INSERT INTO toolkit_outbox_body (payload, payload_type) VALUES (?, 'json')", (b'{"billing_outcome":"completed","dedupe_key":"t/u/abc"}',))
    con.commit()
    con.close()
    db = DB(lambda: p)
    db.seed_quota("tok-q1", "total", "daily", spent_credits_micro=5)  # insert
    db.seed_quota("tok-q1", "total", "daily", reserved_credits_micro=7)  # update
    snap = db.quota_snapshot("tok-q1")
    assert snap[("total", "daily")]["spent_credits_micro"] == 5
    assert snap[("total", "daily")]["reserved_credits_micro"] == 7
    row = db.quota_row("tok-q1", "total", "daily")
    assert bytes(row["user_id"]) == ub(user_id("tok-q1"))
    assert row["period_start"] == period_starts()["daily"]
    assert db.outbox_count("abc", "billing_outcome") == 1
    assert db.outbox_count("zzz") == 0


def test_background_stream_stop_sends_fin_on_silent_stream():
    """stop() must drop the connection even when the server sends nothing more."""
    import socket as _s
    import threading

    srv = _s.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    seen_eof = threading.Event()

    def serve():
        conn, _ = srv.accept()
        conn.recv(65536)
        conn.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        ev = b'event: stream_started\ndata: {"request_id": "r", "message_id": "m", "is_new_turn": true}\n\n'
        conn.sendall(b"%x\r\n%s\r\n" % (len(ev), ev))
        conn.settimeout(10)
        try:
            while True:
                if conn.recv(1024) == b"":
                    seen_eof.set()
                    break
        except OSError:
            seen_eof.set()
        conn.close()

    threading.Thread(target=serve, daemon=True).start()
    api = Api(f"http://127.0.0.1:{port}", None)
    bg = BackgroundStream(api, "POST", "/v1/x", {"content": "x"}).start()
    bg.wait_started(5)
    t0 = time.time()
    bg.stop()
    assert seen_eof.wait(3), "server did not observe the client disconnect"
    assert time.time() - t0 < 3
    srv.close()
    api.close()
