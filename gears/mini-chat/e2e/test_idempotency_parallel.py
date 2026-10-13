"""Idempotent replay, request_id conflicts and the one-running-turn-per-chat guard.

Acceptance criteria covered:
* Idempotency — "Replaying a known request id returns the stored result without side effects (no provider call, no quota change)"
* Idempotency — "Conflicting reuse of a request id across turn states is rejected consistently"
* Idempotency — "Replay is checked before the parallel-turn guard"
* Parallel — "Only one turn may run per chat at a time"
* Parallel — "A new turn is accepted once the previous one reaches a terminal state"
"""

from __future__ import annotations

import threading
import time
import uuid

import httpx

from helpers import (
    all_of,
    assert_ok_stream,
    assert_problem,
    bg_send,
    get_chat,
    http_error,
    list_messages,
    new_chat,
    quota_snapshot,
    send_ok,
    sleep,
    stream_script,
    text_of,
    turn_rows,
    wait_running,
    wait_turn_state,
)


def _side_effect_state(srv):
    q = {k: (v["spent_credits_micro"], v["reserved_credits_micro"], v["calls"]) for k, v in quota_snapshot(srv, "a1").items()}
    return len(srv.responses_requests()), q, len(srv.outbox_events())


def test_replay_completed_turn_without_side_effects(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("Stored", " answer", usage={"input_tokens": 77, "output_tokens": 33}))
    st, done, _ = send_ok(fresh, cid, "original", request_id=rid)
    time.sleep(1.0)  # let background outbox processing settle
    before = _side_effect_state(fresh)
    count_before = get_chat(fresh, cid)["message_count"]

    r, events = fresh.stream(cid, "content is ignored on replay", request_id=rid)
    d = assert_ok_stream(r, events)
    s = events[0].data
    assert s["is_new_turn"] is False
    assert s["request_id"] == rid
    assert s["message_id"] == st["message_id"], "replay carries the persisted assistant message id"
    deltas = all_of(events, "delta")
    assert len(deltas) == 1 and deltas[0].data == {"type": "text", "content": "Stored answer"}
    assert not all_of(events, "citations")
    assert d.data["usage"] == {"input_tokens": 77, "output_tokens": 33}
    assert d.data["quota_decision"] == "allow"
    assert d.data["effective_model"] == "gpt-4.1-mini" and d.data["selected_model"] == "gpt-4.1-mini"
    assert "downgrade_reason" not in d.data
    assert "quota_warnings" not in d.data

    time.sleep(1.0)
    after = _side_effect_state(fresh)
    assert after == before, "replay must not call the provider, touch quota or enqueue outbox messages"
    assert get_chat(fresh, cid)["message_count"] == count_before
    assert len(turn_rows(fresh, cid)) == 1


def test_conflict_for_running_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("slow", sleep(3000), " end"))
    bg = bg_send(fresh, cid, "first", request_id=rid)
    wait_running(fresh, cid)
    r, events = fresh.stream(cid, "again", request_id=rid)
    assert events == []
    j = assert_problem(r, 409, reason="request_id_conflict")
    assert "code" not in j
    bg.wait()
    assert bg.events[-1].event == "done"


def test_conflict_for_failed_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(http_error(500, "nope"))
    r, events = fresh.stream(cid, "x", request_id=rid)
    assert events[-1].event == "error"
    n = len(fresh.responses_requests())
    r, events = fresh.stream(cid, "x", request_id=rid)
    assert_problem(r, 409, reason="request_id_conflict")
    assert len(fresh.responses_requests()) == n


def test_conflict_for_cancelled_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("partial", sleep(4000), " rest"))
    gen = fresh.stream_events(cid, "x", request_id=rid)
    for _, item in gen:
        if not isinstance(item, httpx.Response) and item.event == "delta":
            break
    gen.close()
    wait_turn_state(fresh, cid, rid, {"cancelled"})
    r, _ = fresh.stream(cid, "x", request_id=rid)
    assert_problem(r, 409, reason="request_id_conflict")


def test_conflict_for_deleted_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    send_ok(fresh, cid, "x", request_id=rid)
    assert fresh.req("DELETE", f"/chats/{cid}/turns/{rid}").status_code == 204
    r, _ = fresh.stream(cid, "x", request_id=rid)
    assert_problem(r, 409, reason="request_id_conflict")


def test_replay_checked_before_parallel_guard(fresh):
    """Turn A completed, turn B running: resending A's request_id replays A (not turn_already_running)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid_a = str(uuid.uuid4())
    send_ok(fresh, cid, "A", request_id=rid_a)
    fresh.mock_script(stream_script("B is slow", sleep(3000), "."))
    bg = bg_send(fresh, cid, "B")
    wait_running(fresh, cid)
    r, events = fresh.stream(cid, "A", request_id=rid_a)
    assert_ok_stream(r, events)
    assert events[0].data["is_new_turn"] is False
    assert text_of(events) == "Hello from mock"
    bg.wait()
    assert bg.events[-1].event == "done"


def test_parallel_turn_rejected_then_accepted(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("first", sleep(2500), " done"))
    bg = bg_send(fresh, cid, "first message")
    wait_running(fresh, cid)
    r, events = fresh.stream(cid, "second message")
    assert events == []
    assert_problem(r, 409, reason="turn_already_running")
    r2, _ = fresh.stream(cid, "third message", request_id=str(uuid.uuid4()))
    assert_problem(r2, 409, reason="turn_already_running")
    assert len(fresh.chat_requests()) == 1, "rejected sends never reach the provider"
    bg.wait()
    assert bg.events[-1].event == "done"
    # Terminal → a new turn is accepted.
    send_ok(fresh, cid, "second message")
    assert [m["content"] for m in list_messages(fresh, cid) if m["role"] == "user"] == ["first message", "second message"]


def test_new_turn_accepted_after_failed_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(http_error(500))
    r, events = fresh.stream(cid, "x")
    assert events[-1].event == "error"
    send_ok(fresh, cid, "y")


def test_concurrent_sends_one_wins(fresh):
    """Concurrent sends with different request ids: exactly one streams, the other gets turn_already_running."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("winner", sleep(1500), "!"))
    results: list = []
    barrier = threading.Barrier(3)

    def go(i):
        barrier.wait()
        results.append(fresh.stream(cid, f"msg {i}"))

    ts = [threading.Thread(target=go, args=(i,)) for i in range(3)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(30)
    statuses = sorted(r.status_code for r, _ in results)
    assert statuses == [200, 409, 409], statuses
    for r, events in results:
        if r.status_code == 409:
            assert_problem(r, 409, reason="turn_already_running")
        else:
            assert events[-1].event == "done"
    assert len([t for t in turn_rows(fresh, cid)]) == 1


def test_concurrent_same_request_id(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("one", sleep(1500), "!"))
    results: list = []
    barrier = threading.Barrier(2)

    def go():
        barrier.wait()
        results.append(fresh.stream(cid, "same", request_id=rid))

    ts = [threading.Thread(target=go) for _ in range(2)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(30)
    codes = sorted(r.status_code for r, _ in results)
    assert codes == [200, 409], codes
    loser = [r for r, _ in results if r.status_code == 409][0]
    assert_problem(loser, 409, reason="request_id_conflict")
    assert len(turn_rows(fresh, cid)) == 1
