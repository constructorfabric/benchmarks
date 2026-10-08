"""Turn lifecycle & turn status API (acceptance: Turn Lifecycle; Cleanup & Recovery: orphan watchdog)."""

import uuid

import pytest

from mchelpers import (
    RT_TURN,
    BackgroundStream,
    assert_problem,
    nonce,
    parse_ts,
    shift_timestamp_old,
    ub,
    wait_for,
)

TURN_KEYS = {"request_id", "state", "error_code", "assistant_message_id", "updated_at"}


# Acceptance: Turn lifecycle — running -> completed, status API mapping done
def test_completed_turn_status(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "complete " + nonce())
    assert s.done
    t = api.turn(c["id"], s.request_id)
    assert t.status_code == 200
    body = t.json()
    assert set(body) <= TURN_KEYS, body
    assert "chat_id" not in body
    assert body["request_id"] == s.request_id
    assert body["state"] == "done"
    assert body["assistant_message_id"] == s.message_id
    assert not body.get("error_code")
    parse_ts(body["updated_at"])
    row = db.turn_row(c["id"], s.request_id)
    assert row["state"] == "completed" and row["completed_at"] is not None


# Acceptance: Turn lifecycle — running state is visible while the provider streams
@pytest.mark.timeout(60)
def test_running_turn_status(api, db):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "hang [[hang]] " + nonce()).wait_started()
    try:
        body = api.turn(c["id"], bg.request_id).json()
        assert body["state"] == "running"
        assert not body.get("error_code") and not body.get("assistant_message_id")
        row = db.turn_row(c["id"], bg.request_id)
        assert row["state"] == "running"
        assert row["completed_at"] is None
        assert row["last_progress_at"] is not None
    finally:
        bg.stop()


# Acceptance: Turn lifecycle — failed: state error + error_code, no assistant message
def test_failed_turn_status(api, db):
    c = api.create_chat()
    s = api.stream(c["id"], "fail [[fail]] " + nonce())
    assert s.error["code"] == "provider_error"
    body = api.wait_turn_state(c["id"], s.request_id, {"error"})
    assert body["error_code"] == "provider_error"
    assert not body.get("assistant_message_id")
    row = db.turn_row(c["id"], s.request_id)
    assert row["state"] == "failed" and row["error_code"] == "provider_error"
    assert row["completed_at"] is not None
    roles = [m["role"] for m in api.messages(c["id"])]
    assert roles == ["user"], "a failed turn persists no assistant message"


# Acceptance: Turn lifecycle — cancellation on disconnect persists partial content
@pytest.mark.timeout(60)
def test_cancelled_with_partial_content(api, db, mock_llm):
    c = api.create_chat()
    n = nonce()
    bg = BackgroundStream.send(api, c["id"], f"slow [[slow]] {n}").wait_started().wait_delta()
    rid = bg.request_id
    bg.stop()
    body = api.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=30)
    assert not body.get("error_code")
    mid = body.get("assistant_message_id")
    assert mid, "non-empty partial text must be persisted as the assistant message"
    msgs = api.messages(c["id"])
    a = [m for m in msgs if m["id"] == mid]
    assert a and a[0]["role"] == "assistant"
    assert a[0]["content"].startswith("Partial-0")
    assert "Partial-19" not in a[0]["content"]
    row = db.turn_row(c["id"], rid)
    assert row["state"] == "cancelled" and row["completed_at"] is not None
    # the provider connection was aborted (hard cancel)
    wait_for(lambda: mock_llm.chat_requests(contains=n)[0].get("client_disconnected"), timeout=15, desc="provider connection closed")


# Acceptance: Turn lifecycle — cancellation before any text: no assistant message
@pytest.mark.timeout(60)
def test_cancelled_without_content(api):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "thinking [[delay:30]] " + nonce()).wait_started()
    rid = bg.request_id
    bg.stop()
    body = api.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=40)
    assert not body.get("assistant_message_id")
    assert [m["role"] for m in api.messages(c["id"])] == ["user"]


# Acceptance: Turn lifecycle — terminal states are immutable (late provider data does not reopen)
@pytest.mark.timeout(60)
def test_terminal_state_is_immutable(api):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "slow [[slow:4:2]] " + nonce()).wait_started().wait_delta()
    rid = bg.request_id
    bg.stop()
    api.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=30)
    import time

    time.sleep(6)  # the provider would have finished by now
    assert api.turn(c["id"], rid).json()["state"] == "cancelled"


# Acceptance: Turn lifecycle — unknown / foreign turns are 404 with the turn resource type
def test_unknown_turn(api, api_for):
    c = api.create_chat()
    assert_problem(api.turn(c["id"], str(uuid.uuid4())), 404, "not_found", resource_type=RT_TURN)
    s = api.stream(c["id"], "mine " + nonce())
    assert s.done
    other = api_for("tok-a2")
    assert_problem(other.turn(c["id"], s.request_id), 404, "not_found")


# Acceptance: Cleanup & Recovery / Turn lifecycle — orphan watchdog finalizes a stale running turn
@pytest.mark.timeout(90)
def test_orphan_watchdog_finalizes_stale_turn(api_for, db):
    a = api_for("tok-q11")
    c = a.create_chat(model="std")
    bg = BackgroundStream.send(a, c["id"], "hang [[hang]] " + nonce()).wait_started().wait_delta()
    rid = bg.request_id
    try:
        row = db.turn_row(c["id"], rid)
        assert row["state"] == "running"
        reserved = int(row["reserved_credits_micro"] or 0)
        q_before = db.quota_snapshot("tok-q11")[("total", "daily")]
        # Make the turn stale: COALESCE(last_progress_at, started_at) <= now - timeout_secs.
        # Only last_progress_at is moved (same textual format); started_at keeps the quota period.
        assert row["last_progress_at"] is not None, "running turns must have last_progress_at"
        n = db.execute(
            "UPDATE chat_turns SET last_progress_at = ? WHERE chat_id = ? AND request_id = ? AND state = 'running'",
            (shift_timestamp_old(row["last_progress_at"]), ub(c["id"]), ub(rid)),
        )
        assert n == 1
        body = a.wait_turn_state(c["id"], rid, {"error"}, timeout=30)
        assert body["error_code"] == "orphan_timeout"
        row = db.turn_row(c["id"], rid)
        assert row["state"] == "failed" and row["error_code"] == "orphan_timeout"
        assert row["completed_at"] is not None
        # estimated settlement released this turn's reserve from the current-period bucket
        q_after = db.quota_snapshot("tok-q11")[("total", "daily")]
        assert q_after["reserved_credits_micro"] == q_before["reserved_credits_micro"] - reserved
        assert q_after["calls"] == q_before["calls"] + 1
        # the watchdog does not write messages
        assert [m["role"] for m in a.messages(c["id"])] == ["user"]
    finally:
        bg.stop()
