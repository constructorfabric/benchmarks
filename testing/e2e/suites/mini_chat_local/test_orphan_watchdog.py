"""Orphan turn watchdog black-box.

A ``running`` turn without progress for ``orphan_watchdog.timeout_secs`` is finalized ``failed`` /
``orphan_timeout`` by the periodic task (``orphan_watchdog.scan_interval_secs``), settled by the
estimated formula with exactly one usage event (``billing_outcome = aborted``,
``settlement_method = estimated``) and one ``turn_failed`` audit event; fresh, progressing and
terminal turns are untouched; the chat is unblocked afterwards; a stream that finishes after the
watchdog loses the finalization CAS and ends with ``stream_interrupted``.

The tests run on dedicated stacks (``stack_factory``): ``watchdog`` (enabled, scans every second,
timeout 90 s, the quota catalog limits) and ``watchdog_off`` (``orphan_watchdog.enabled = false``).
The minimum valid timeout is 90 s (B.9.1), so time is moved instead of waited for: the tests rewrite
``chat_turns.last_progress_at`` of a stalled turn (the mock provider holds the answer back) or seed
`running` rows dating from 2025, exactly the states a crashed pod leaves behind.

Source: DESIGN "Turn Lifecycle, Crash Recovery and Orphan Handling" ("Orphan Turn Watchdog",
"Watchdog Invariants"), B.9.1, 3.7 ``chat_turns`` (``last_progress_at``, NULL preflight columns),
5.7 / 5.8 / 5.9 (orphan finalization, ``orphan_timeout`` -> ``ABORTED`` / estimated, CAS loser ->
``stream_interrupted``), 3.2 "Quota period boundary invariant", ADR-0010.
"""

from __future__ import annotations

import copy
import time
import uuid
from typing import Any, Callable, Dict

import pytest

from . import catalog
from .mock_openai_provider import sse_text
from .mutation_support import (
    AUDIT_QUEUE,
    USAGE_QUEUE,
    MutationSeeder,
    audit_events,
    delete_turn,
    mutate_stream,
    usage_events,
)
from .support import (
    TENANT_A_ID,
    USER_A_ID,
    assert_conflict,
    assert_problem,
    dedupe_key,
    get_turn,
    open_stream,
    reject_message,
    send_message,
    stored_ts,
    uuid_text,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
RESPONSES = ("POST", "/v1/responses")
MSG = "hello"
STALE = stored_ts(0)  # 2025-01-01: an hour of silence is not needed, years are
WAIT_S = 30.0  # scan interval 1 s: a stale turn is finalized within a few seconds


def watchdog_overrides(enabled: bool) -> Dict[str, Any]:
    overrides = copy.deepcopy(catalog.quota_overrides())
    overrides["gears"]["mini-chat"]["config"]["orphan_watchdog"] = {
        "enabled": enabled, "timeout_secs": 90, "scan_interval_secs": 1}
    return overrides


@pytest.fixture(scope="module")
def watchdog_stack(stack_factory: Callable[[str, Dict[str, Any]], Any]) -> Any:
    return stack_factory("orphan_watchdog", watchdog_overrides(True))


@pytest.fixture(scope="module")
def watchdog_off_stack(stack_factory: Callable[[str, Dict[str, Any]], Any]) -> Any:
    return stack_factory("orphan_watchdog_off", watchdog_overrides(False))


@pytest.fixture()
def wd(watchdog_stack):
    watchdog_stack.mock.reset()
    watchdog_stack.seeder().clear_quota()
    watchdog_stack.mseed = MutationSeeder(watchdog_stack.server.db_path)
    try:
        yield watchdog_stack
    finally:
        watchdog_stack.mock.release()


@pytest.fixture()
def wd_api(wd):
    client = wd.api()
    yield client
    client.close()


def new_chat(seeder, model=PREMIUM):
    return seeder.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model)


def stall(stack, api, chat, text=MSG):
    """A running turn whose provider holds the answer back; returns the live stream and the
    request id once the turn exists and its reserve is held."""
    rid = str(uuid.uuid4())
    stack.mock.script(*RESPONSES, sse_text(["late answer"], pause_after_deltas=0))
    live = open_stream(api, chat, {"content": text, "request_id": rid})
    live.expect("stream_started")
    stack.mock.wait_paused()
    return live, rid


def make_stale(stack, chat, rid):
    stack.mseed.set_progress(chat, rid, STALE)


def wait_failed(stack, chat, rid):
    return wait_for(
        lambda: (lambda t: t if t is not None and t["state"] == "failed" else None)(
            stack.mseed.turn(chat, rid)),
        f"the watchdog to finalize turn {rid}", timeout=WAIT_S)


def orphaned(stack, api, text=MSG):
    """Stall a turn, make it stale and wait for the watchdog: ``(chat, rid, live)``."""
    chat = new_chat(stack.seeder())
    live, rid = stall(stack, api, chat, text)
    make_stale(stack, chat, rid)
    wait_failed(stack, chat, rid)
    return chat, rid, live


def finish_late(stack, live):
    stack.mock.release()
    try:
        return live.drain()
    finally:
        live.close()


# --------------------------------------------------------------------------- #
# the finalization
# --------------------------------------------------------------------------- #


def test_a_stale_turn_is_finalized_failed_with_orphan_timeout(wd, wd_api):
    chat = new_chat(wd.seeder())
    live, rid = stall(wd, wd_api, chat)
    assert wd.db().quota_amounts(USER_A_ID, "total", "daily")[1] > 0, "the reserve is held"
    make_stale(wd, chat, rid)

    turn = wait_failed(wd, chat, rid)

    assert turn["error_code"] == "orphan_timeout"
    assert turn["completed_at"] is not None and turn["assistant_message_id"] is None
    assert turn["deleted_at"] is None
    messages = wd.seeder().ordered_messages(chat)
    assert [m["role"] for m in messages] == ["user"], "no assistant message is stored"
    status = get_turn(wd_api, chat, rid)
    assert status.status_code == 200
    assert status.json()["state"] == "error" and status.json()["error_code"] == "orphan_timeout"
    assert "assistant_message_id" not in status.json()
    finish_late(wd, live)


def test_an_orphan_is_settled_by_the_estimated_formula_and_the_reserve_returns_to_zero(wd, wd_api):
    chat, rid, live = orphaned(wd, wd_api)
    expected = catalog.estimated_charge(PREMIUM, MSG)
    assert expected == 1_089
    db = wd.db()
    for period in ("daily", "monthly"):
        total = db.quota_row(USER_A_ID, "total", period)
        assert (total["spent_credits_micro"], total["reserved_credits_micro"], total["calls"]) \
            == (expected, 0, 1), f"total/{period}: {dict(total)}"
        assert (total["input_tokens"], total["output_tokens"]) == (0, 0), "estimated: no tokens"
        tier = db.quota_row(USER_A_ID, "tier:premium", period)
        assert (tier["spent_credits_micro"], tier["reserved_credits_micro"], tier["calls"]) \
            == (expected, 0, 1), f"tier:premium/{period}: {dict(tier)}"
    for row in db.quota_rows():
        assert row["reserved_credits_micro"] == 0, dict(row)
    finish_late(wd, live)


def test_an_orphan_publishes_one_usage_event_aborted_estimated(wd, wd_api):
    usage_before = wd.db().outbox_sequenced(USAGE_QUEUE)
    chat, rid, live = orphaned(wd, wd_api)

    events = wait_for(lambda: usage_events(wd.server, rid) or None,
                      "the usage event of the orphan to be delivered")

    assert len(events) == 1
    event = events[0]
    turn_id = uuid_text(wd.mseed.turn(chat, rid)["id"])
    assert event["terminal_state"] == "failed"
    assert event["billing_outcome"] == "aborted", "not `failed`: no provider terminal event"
    assert event["settlement_method"] == "estimated"
    assert event["actual_credits_micro"] == str(catalog.estimated_charge(PREMIUM, MSG))
    assert event["effective_model"] == PREMIUM and event["selected_model"] == PREMIUM
    assert event["policy_version_applied"] != "0"
    assert event["dedupe_key"] == dedupe_key(TENANT_A_ID, turn_id, rid)
    assert wd.db().outbox_sequenced(USAGE_QUEUE) == usage_before + 1
    finish_late(wd, live)


def test_an_orphan_publishes_one_turn_failed_audit_event(wd, wd_api):
    audit_before = wd.db().outbox_sequenced(AUDIT_QUEUE)
    chat, rid, live = orphaned(wd, wd_api)

    events = wait_for(
        lambda: [e for e in audit_events(wd.server, "turn_failed") if e.get("request_id") == rid]
        or None, "the turn_failed audit event of the orphan")

    assert len(events) == 1
    assert wd.db().outbox_sequenced(AUDIT_QUEUE) == audit_before + 1
    finish_late(wd, live)


def test_a_fresh_turn_is_left_alone_and_finishes_normally(wd, wd_api):
    chat = new_chat(wd.seeder())
    live, rid = stall(wd, wd_api, chat)
    reserved = wd.db().quota_amounts(USER_A_ID, "total", "daily")[1]

    time.sleep(4.5)  # several scans of the 1 s interval

    assert wd.mseed.turn(chat, rid)["state"] == "running"
    assert wd.db().quota_amounts(USER_A_ID, "total", "daily") == (0, reserved)
    assert usage_events(wd.server, rid) == []
    events = finish_late(wd, live)
    assert events[-1].event == "done"
    assert wd.mseed.turn(chat, rid)["state"] == "completed"
    assert len(wait_for(lambda: usage_events(wd.server, rid) or None, "the usage event")) == 1


def test_terminal_turns_are_never_finalized_even_when_they_look_stale(wd, wd_api):
    chat = new_chat(wd.seeder())
    seeded = [wd.mseed.insert_pair(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, state=state,
                                   answer=None, at=10 * (i + 1))
              for i, state in enumerate(("failed", "cancelled", "completed"))]
    time.sleep(4.5)
    for pair in seeded:
        turn = wd.mseed.turn(chat, pair["request_id"])
        assert turn["error_code"] != "orphan_timeout"
    assert [wd.mseed.turn(chat, p["request_id"])["state"] for p in seeded] == [
        "failed", "cancelled", "completed"]
    seeded_ids = {p["request_id"] for p in seeded}
    assert not [e for e in audit_events(wd.server, "turn_failed")
                if e.get("request_id") in seeded_ids]
    assert not [e for e in usage_events(wd.server) if e.get("request_id") in seeded_ids]


def test_a_turn_without_reserve_columns_is_finalized_without_touching_quota_rows(wd, wd_api):
    chat = new_chat(wd.seeder())
    rid = wd.mseed.insert_running(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, bare=True)
    quota_before = [dict(r) for r in wd.db().quota_rows()]

    turn = wait_failed(wd, chat, rid)

    assert turn["error_code"] == "orphan_timeout"
    events = wait_for(lambda: usage_events(wd.server, rid) or None,
                      "the usage event of the bare orphan")
    assert len(events) == 1
    assert events[0]["billing_outcome"] == "aborted"
    assert events[0]["settlement_method"] == "estimated"
    assert events[0]["actual_credits_micro"] == "0"
    assert events[0]["policy_version_applied"] == "0"
    assert [dict(r) for r in wd.db().quota_rows()] == quota_before, "no quota row changed"
    wait_for(lambda: [e for e in audit_events(wd.server, "turn_failed")
                      if e.get("request_id") == rid] or None, "the audit event of the bare orphan")


# --------------------------------------------------------------------------- #
# the race with the stream's own finalizer
# --------------------------------------------------------------------------- #


def test_a_stream_that_finishes_after_the_watchdog_ends_with_stream_interrupted(wd, wd_api):
    chat, rid, live = orphaned(wd, wd_api)
    settled = [dict(r) for r in wd.db().quota_rows()]
    wait_for(lambda: usage_events(wd.server, rid) or None, "the orphan's usage event")

    events = finish_late(wd, live)

    assert events[-1].event == "error" and events[-1].data["code"] == "stream_interrupted"
    assert all(e.event != "done" for e in events)
    turn = wd.mseed.turn(chat, rid)
    assert (turn["state"], turn["error_code"]) == ("failed", "orphan_timeout")
    assert [m["role"] for m in wd.seeder().ordered_messages(chat)] == ["user"]
    assert [dict(r) for r in wd.db().quota_rows()] == settled, "no double settlement"
    time.sleep(1.5)
    assert len(usage_events(wd.server, rid)) == 1, "exactly one usage event"
    assert len([e for e in audit_events(wd.server, "turn_failed")
                if e.get("request_id") == rid]) == 1, "exactly one audit event"


# --------------------------------------------------------------------------- #
# after the watchdog
# --------------------------------------------------------------------------- #


def test_the_chat_accepts_a_new_message_once_the_orphan_is_finalized(wd, wd_api):
    chat, rid, live = orphaned(wd, wd_api)
    finish_late(wd, live)

    sent = send_message(wd_api, chat, "now it works")

    assert sent.terminal.event == "done"
    assert_conflict(reject_message(wd_api, chat, {"content": "again", "request_id": rid}),
                    "request_id_conflict")


@pytest.mark.parametrize("op", ["retry", "edit", "delete"])
def test_an_orphaned_turn_can_be_retried_edited_or_deleted(wd, wd_api, op):
    chat, rid, live = orphaned(wd, wd_api)
    finish_late(wd, live)

    if op == "delete":
        assert delete_turn(wd_api, chat, rid).status_code == 204
        assert get_turn(wd_api, chat, rid).status_code == 404
    else:
        sent = mutate_stream(wd_api, op, chat, rid, **({"content": "reworded"} if op == "edit"
                                                       else {}))
        assert sent.terminal.event == "done"
        assert uuid_text(wd.mseed.turn(chat, rid)["replaced_by_request_id"]) == sent.request_id
    assert wd.mseed.turn(chat, rid)["error_code"] == "orphan_timeout"


def test_a_stale_seeded_turn_unblocks_its_chat(wd, wd_api):
    chat = new_chat(wd.seeder())
    stuck = wd.mseed.insert_running(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, bare=False)

    wait_failed(wd, chat, stuck)

    assert send_message(wd_api, chat, "after the crash").terminal.event == "done"


# --------------------------------------------------------------------------- #
# orphan_watchdog.enabled = false
# --------------------------------------------------------------------------- #


def test_a_disabled_watchdog_leaves_stale_turns_running_and_the_chat_blocked(watchdog_off_stack):
    stack = watchdog_off_stack
    stack.mock.reset()
    api = stack.api()
    try:
        seeder = MutationSeeder(stack.server.db_path)
        chat = new_chat(seeder)
        stuck = seeder.insert_running(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, bare=False)

        time.sleep(4.5)  # several scan intervals

        assert seeder.turn(chat, stuck)["state"] == "running"
        assert usage_events(stack.server, stuck) == []
        assert_conflict(reject_message(api, chat, {"content": "q"}), "turn_already_running")
        status = get_turn(api, chat, stuck)
        assert status.status_code == 200 and status.json()["state"] == "running"
    finally:
        api.close()
