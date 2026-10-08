"""Turn state machine: cancellation (client disconnect), orphan watchdog, settlement."""

from __future__ import annotations

import threading
import time
import uuid

import httpx

from conftest import PREFIX, ok_stream, parse_sse_lines, ub, wait_until
from test_idempotency import _wait_running

OLD_TS = "2000-01-01T00:00:00.000000001Z"


def _open_and_drop(server, api, chat_id, body, stop_after):
    """Opens the stream, reads until `stop_after` matches, then disconnects."""
    seen = []
    with httpx.Client(base_url=server.base, headers={"Authorization": f"Bearer {api.token}"}, timeout=30) as c:
        with c.stream("POST", f"{PREFIX}/chats/{chat_id}/messages:stream", json=body) as resp:
            assert resp.status_code == 200

            def on(name, data):
                seen.append((name, data))
                return stop_after(name, data)

            parse_sse_lines(resp.iter_lines(), on)
    return seen


def _reserved(server, user_id):
    rows = server.query("SELECT SUM(reserved_credits_micro) AS r FROM quota_usage WHERE user_id = ?", ub(user_id))
    return rows[0]["r"] or 0


def test_client_disconnect_cancels_with_partial_content(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": ["part one. "] + ["more "] * 30, "chunk_delay_ms": 300})
    seen = _open_and_drop(server, api, chat["id"], {"content": "long", "request_id": rid}, lambda n, d: n == "delta")
    assert seen[0][0] == "stream_started"
    t = wait_until(lambda: (lambda b: b if b["state"] != "running" else None)(api.turn(chat["id"], rid).json()), msg="cancel")
    assert t["state"] == "cancelled"
    assert "error_code" not in t
    assert "assistant_message_id" in t
    msgs = api.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["content"].startswith("part one.")
    assert msgs[1]["id"] == t["assistant_message_id"]
    # provider connection is closed promptly (hard cancel)
    wait_until(lambda: mock.stats()["active_streams"] == 0, timeout=5, msg="provider stream closed")
    row = server.query("SELECT state FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["state"] == "cancelled"
    # estimated settlement, reserve released
    ev = wait_until(lambda: server.usage_events(request_id=rid), msg="usage")[0]
    assert ev["billing_outcome"] == "aborted"
    assert ev["settlement_method"] == "estimated"
    assert ev["terminal_state"] == "cancelled"
    assert ev["usage"] is None
    assert ev["actual_credits_micro"] > 0
    assert _reserved(server, api.user_id) == 0
    audit = wait_until(lambda: server.audit_events(request_id=rid), msg="audit")[0]
    assert audit["event_type"] == "turn_failed"


def test_disconnect_before_content_has_no_message(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 20})
    _open_and_drop(server, api, chat["id"], {"content": "x", "request_id": rid}, lambda n, d: n == "stream_started")
    t = wait_until(lambda: (lambda b: b if b["state"] != "running" else None)(api.turn(chat["id"], rid).json()), msg="cancel")
    assert t["state"] == "cancelled"
    assert "assistant_message_id" not in t
    assert [m["role"] for m in api.messages(chat["id"])] == ["user"]
    wait_until(lambda: mock.stats()["active_streams"] == 0, timeout=5, msg="provider stream closed")


def test_failed_turn_with_partial_output_has_no_message(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": ["half "], "terminal": "failed"})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.names[-1] == "error"
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error"
    assert "assistant_message_id" not in t
    assert [m["role"] for m in api.messages(chat["id"])] == ["user"]
    ev = wait_until(lambda: server.usage_events(request_id=rid), msg="usage")[0]
    assert ev["billing_outcome"] == "failed"
    assert ev["settlement_method"] == "estimated"
    assert _reserved(server, api.user_id) == 0


def test_failed_turn_with_usage_settles_actual(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": ["half "], "terminal": "failed", "failed_usage": True})
    api.send(chat["id"], "x", request_id=rid)
    ev = wait_until(lambda: server.usage_events(request_id=rid), msg="usage")[0]
    assert ev["billing_outcome"] == "failed"
    assert ev["settlement_method"] == "actual"
    assert ev["usage"]["input_tokens"] == 42


def test_orphan_watchdog_finalizes_stale_turn(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 30})
    result = {}

    def hold():
        events = []
        with httpx.Client(base_url=server.base, headers={"Authorization": f"Bearer {api.token}"}, timeout=60) as c:
            with c.stream("POST", f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "x", "request_id": rid}) as resp:
                parse_sse_lines(resp.iter_lines(), lambda n, d: (events.append((n, d)), n in ("done", "error") or result.get("stop"))[1])
        result["events"] = events

    th = threading.Thread(target=hold, daemon=True)
    th.start()
    _wait_running(server, chat["id"])
    # turn without recent progress: last_progress_at older than orphan_watchdog.timeout_secs
    assert server.execute("UPDATE chat_turns SET last_progress_at = ? WHERE request_id = ?", OLD_TS, ub(rid)) == 1
    t = wait_until(
        lambda: (lambda b: b if b["state"] != "running" else None)(api.turn(chat["id"], rid).json()), timeout=20, msg="orphan"
    )
    assert t["state"] == "error"
    assert t["error_code"] == "orphan_timeout"
    row = server.query("SELECT state, error_code, completed_at FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert (row["state"], row["error_code"]) == ("failed", "orphan_timeout")
    assert row["completed_at"] is not None
    ev = wait_until(lambda: server.usage_events(request_id=rid), msg="usage")[0]
    assert ev["billing_outcome"] == "aborted"
    assert ev["settlement_method"] == "estimated"
    assert ev["usage"] is None
    assert ev["actual_credits_micro"] > 0
    assert _reserved(server, api.user_id) == 0
    audit = wait_until(lambda: server.audit_events(request_id=rid), msg="audit")[0]
    assert audit["event_type"] == "turn_failed"
    assert audit["error_code"] == "orphan_timeout"
    result["stop"] = True
    th.join(40)
    # the provider task lost the CAS: no second settlement / usage event
    time.sleep(1)
    assert len(server.usage_events(request_id=rid)) == 1
    names = [n for n, _ in result.get("events", [])]
    assert "done" not in names
    if "error" in names:
        err = dict(result["events"])["error"]
        assert err["code"] == "stream_interrupted"


def test_watchdog_ignores_turns_with_recent_progress(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": ["a"] * 6, "chunk_delay_ms": 800})
    # old started_at but fresh last_progress_at must not be finalized
    th = threading.Thread(target=lambda: api.send(chat["id"], "x", request_id=rid), daemon=True)
    th.start()
    _wait_running(server, chat["id"])
    server.execute("UPDATE chat_turns SET started_at = ? WHERE request_id = ?", OLD_TS, ub(rid))
    th.join(30)
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "done"


def test_completed_turn_settles_actual_usage(api, server, mock):
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    mock.script({"text": "ok", "usage": {"input_tokens": 1000, "output_tokens": 500}})
    ok_stream(api.send(chat["id"], "x", request_id=rid))
    rows = server.query(
        "SELECT period_type, bucket, spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens "
        "FROM quota_usage WHERE user_id = ?",
        ub(api.user_id),
    )
    total = [r for r in rows if r["bucket"] == "total"]
    assert {r["period_type"] for r in total} == {"daily", "monthly"}
    assert not [r for r in rows if r["bucket"] == "tier:premium" and r["spent_credits_micro"]]
    for r in total:
        # gpt-standard: 1000 * 1 + 500 * 2
        assert r["spent_credits_micro"] == 2000
        assert r["reserved_credits_micro"] == 0
        assert r["calls"] == 1
        assert r["input_tokens"] == 1000 and r["output_tokens"] == 500
