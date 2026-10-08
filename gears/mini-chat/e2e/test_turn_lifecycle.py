"""Turn state machine, cancellation, failure and orphan recovery.

Acceptance criteria covered:
* Turn Lifecycle — "Turn state machine (running → completed / cancelled / failed) is consistent end to end"
* Turn Lifecycle — "Partial and null-content cases on cancellation or failure are handled correctly"
* Cleanup & Recovery (orphan watchdog part of reliable recovery) / Settlement — orphan turns settle estimated, aborted
"""

from __future__ import annotations

import uuid

import httpx

from helpers import (
    RT_CHAT,
    RT_TURN,
    assert_not_found,
    assert_not_found_any,
    assert_problem,
    bg_send,
    get_chat,
    http_error,
    list_messages,
    message_rows,
    new_chat,
    quota_val,
    shift_ts,
    sleep,
    stream_script,
    turn_row,
    turn_status,
    usage_events_for,
    wait_running,
    wait_turn_state,
    wait_until,
)


def _disconnect_after(srv, cid, content, stop_on, request_id=None):
    body = {"request_id": request_id} if request_id else {}
    gen = srv.stream_events(cid, content, **body)
    seen = []
    for _, item in gen:
        if isinstance(item, httpx.Response):
            assert item.status_code == 200, item.text
            continue
        seen.append(item)
        if item.event == stop_on:
            break
    gen.close()
    return seen


def test_running_then_done_status(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("one", sleep(2000), " two"))
    bg = bg_send(fresh, cid, "x", request_id=rid)
    wait_running(fresh, cid)
    st = turn_status(fresh, cid, rid).json()
    assert st["state"] == "running"
    assert "error_code" not in st or st["error_code"] is None
    assert "assistant_message_id" not in st or st["assistant_message_id"] is None
    assert st["updated_at"]
    row = turn_row(fresh, cid, rid)
    assert row["completed_at"] is None and row["last_progress_at"] is not None
    bg.wait()
    st = wait_turn_state(fresh, cid, rid, {"done"})
    assert st["assistant_message_id"] == bg.events[0].data["message_id"]
    assert turn_row(fresh, cid, rid)["completed_at"] is not None


def test_turn_status_unknown_404(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    assert_not_found(turn_status(fresh, cid, str(uuid.uuid4())), RT_TURN)
    assert_not_found_any(turn_status(fresh, str(uuid.uuid4()), str(uuid.uuid4())), RT_CHAT, RT_TURN)
    r = fresh.req("GET", f"/chats/{cid}/turns/not-a-uuid")
    assert_problem(r, 400)


def test_cancel_with_partial_content(fresh):
    """Client disconnect after text: cancelled, partial assistant message persisted, provider request cancelled."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("partial text", sleep(5000), " never seen"))
    seen = _disconnect_after(fresh, cid, "cancel me", "delta", rid)
    assert seen[0].event == "stream_started"
    st = wait_turn_state(fresh, cid, rid, {"cancelled"})
    assert st.get("assistant_message_id"), st
    msgs = list_messages(fresh, cid)
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["id"] == st["assistant_message_id"]
    assert msgs[1]["content"] == "partial text"
    assert get_chat(fresh, cid)["message_count"] == 2
    row = turn_row(fresh, cid, rid)
    assert row["state"] == "cancelled" and row["completed_at"] is not None


def test_disconnect_cancels_provider_request(fresh):
    """A client disconnect cancels the outbound provider request (the mock sees the dropped connection)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("partial text", sleep(3000), " a", sleep(3000), " b"))
    _disconnect_after(fresh, cid, "cancel me", "delta", rid)
    wait_turn_state(fresh, cid, rid, {"cancelled"})
    assert wait_until(lambda: fresh.mock_stats()["cancelled"] >= 1, timeout=12), fresh.mock_stats()


def test_cancel_before_any_text_has_no_assistant_message(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script(sleep(5000), "late"))
    _disconnect_after(fresh, cid, "cancel early", "stream_started", rid)
    st = wait_turn_state(fresh, cid, rid, {"cancelled"})
    assert "assistant_message_id" not in st or st["assistant_message_id"] is None
    assert [m["role"] for m in list_messages(fresh, cid)] == ["user"]
    assert [m["role"] for m in message_rows(fresh, cid)] == ["user"]
    assert get_chat(fresh, cid)["message_count"] == 1


def test_failure_after_partial_text_keeps_user_message_only(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("half", terminal="failed", error={"code": "server_error", "message": "died"}, usage={}))
    r, events = fresh.stream(cid, "x")
    assert events[-1].event == "error"
    rid = events[0].data["request_id"]
    st = turn_status(fresh, cid, rid).json()
    assert st["state"] == "error" and st["error_code"] == "provider_error"
    assert "assistant_message_id" not in st or st["assistant_message_id"] is None
    assert [m["role"] for m in list_messages(fresh, cid)] == ["user"]


def test_http_error_failure_state(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(http_error(503, "unavailable"))
    r, events = fresh.stream(cid, "x")
    assert events[-1].event == "error" and events[-1].data["code"] == "provider_error"
    rid = events[0].data["request_id"]
    row = turn_row(fresh, cid, rid)
    assert row["state"] == "failed" and row["error_code"] == "provider_error" and row["completed_at"] is not None


def test_terminal_state_immutable_on_late_events(fresh):
    """A cancelled turn is never reopened by later provider output."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("p", sleep(1500), "q"))
    _disconnect_after(fresh, cid, "x", "delta", rid)
    wait_turn_state(fresh, cid, rid, {"cancelled"})
    import time

    time.sleep(2.5)
    assert turn_row(fresh, cid, rid)["state"] == "cancelled"


def test_orphan_watchdog_finalizes_stale_running_turn(fresh):
    """running turn with stale progress → failed / orphan_timeout, reserve released, aborted usage event."""
    user = "a2"
    cid = new_chat(fresh, "gpt-4.1-mini", user=user)
    rid = str(uuid.uuid4())
    reserved_before = quota_val(fresh, user, "daily", "total", "reserved_credits_micro")
    spent_before = quota_val(fresh, user, "daily", "total", "spent_credits_micro")
    # Late completion after the watchdog fired must not overwrite the terminal state.
    fresh.mock_script(stream_script("x", sleep(6000), "late", usage={"input_tokens": 1, "output_tokens": 1}))
    bg = bg_send(fresh, cid, "orphan me", user=user, request_id=rid)
    row = wait_running(fresh, cid)
    reserved = int(row["reserved_credits_micro"])
    assert quota_val(fresh, user, "daily", "total", "reserved_credits_micro") == reserved_before + reserved
    old_progress = shift_ts(row["last_progress_at"], -300)
    old_start = shift_ts(row["started_at"], -300)
    fresh.execute(
        "UPDATE chat_turns SET last_progress_at = ?, started_at = ? WHERE id = ?",
        (old_progress, old_start, row["id"]),
    )

    def orphaned():
        r = turn_row(fresh, cid, rid)
        return r if r["state"] == "failed" else None

    final = wait_until(orphaned, timeout=10)
    assert final, f"watchdog did not finalize: {turn_row(fresh, cid, rid)}"
    assert final["error_code"] == "orphan_timeout"
    assert final["completed_at"] is not None
    st = turn_status(fresh, cid, rid, user).json()
    assert st["state"] == "error" and st["error_code"] == "orphan_timeout"
    # Reserve released; estimated settlement charged (input estimate + minimal generation floor).
    assert quota_val(fresh, user, "daily", "total", "reserved_credits_micro") == reserved_before
    est_in = int(final["reserve_tokens"]) - int(final["max_output_tokens_applied"])
    floor = int(final["minimal_generation_floor_applied"])
    expected = est_in * 1 + floor * 3  # gpt-4.1-mini multipliers 1x / 3x
    assert quota_val(fresh, user, "daily", "total", "spent_credits_micro") == spent_before + expected
    ev = wait_until(lambda: usage_events_for(fresh, rid), timeout=10)
    assert ev and len(ev) == 1, ev
    assert ev[0]["billing_outcome"] == "aborted"
    assert ev[0]["settlement_method"] == "estimated"
    # The watchdog does not create messages.
    assert [m["role"] for m in message_rows(fresh, cid)] == ["user"]
    bg.wait(20)
    assert [m["role"] for m in message_rows(fresh, cid)] == ["user"], "a late completion persists no message"
    # Late provider completion did not reopen the turn.
    final2 = turn_row(fresh, cid, rid)
    assert final2["state"] == "failed" and final2["error_code"] == "orphan_timeout"
    assert len(usage_events_for(fresh, rid)) == 1, "exactly one settlement for the orphaned turn"


def test_watchdog_ignores_fresh_running_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("a", sleep(3000), "b"))
    bg = bg_send(fresh, cid, "x", request_id=rid)
    wait_running(fresh, cid)
    import time

    time.sleep(2.0)  # > scan interval (1 s) but < timeout (90 s)
    assert turn_row(fresh, cid, rid)["state"] == "running"
    bg.wait()
    assert turn_row(fresh, cid, rid)["state"] == "completed"
