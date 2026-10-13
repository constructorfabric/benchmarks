"""``messages:stream`` black-box: idempotency / replay and the parallel-turn guard.

Source: DESIGN 3.3 "Streaming Contract" (Idempotency table, "Parallel turn guard", "Replay is
side-effect-free invariant"), 4 "Turn Lifecycle, Crash Recovery and Orphan Handling"
(Idempotency Rules, Parallel Turn Policy and its normative check order, 409 Recovery), the REST
error table (`request_id_conflict`, `turn_already_running`) and ADR-0010 (a replay rebuilds
`quota_decision` from the stored models and sends no `downgrade_reason` / citations).
"""

from __future__ import annotations

import uuid
from concurrent.futures import ThreadPoolExecutor

from . import catalog
from .mock_openai_provider import error_response, sse_text
from .support import (
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    assert_conflict,
    assert_problem,
    open_stream,
    reject_message,
    send_message,
    uuid_text,
)

PREMIUM = catalog.PREMIUM_ID


def new_chat(seed, model=PREMIUM, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)


def chat_requests(mock):
    return mock.requests_to("POST", "/v1/responses")


def hold_running(api, mock, chat, request_id):
    """Start a turn that stays `running`: the provider pauses after its first delta."""
    mock.script("POST", "/v1/responses", sse_text(["held", " back"], pause_after_deltas=1))
    live = open_stream(api, chat, {"content": "hold", "request_id": request_id})
    live.expect("stream_started")
    live.next_non_ping()
    mock.wait_paused()
    return live


def release_and_finish(mock, live):
    mock.release()
    try:
        return live.drain()
    finally:
        live.close()


def row_dict(row):
    return None if row is None else {k: row[k] for k in row.keys()}


# --------------------------------------------------------------------------- #
# completed turns are replayed
# --------------------------------------------------------------------------- #


def test_a_completed_request_id_is_replayed_without_side_effects(api, seed, mock_provider):
    chat = new_chat(seed, created=0)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["Hel", "lo ", "there"], input_tokens=20, output_tokens=6))
    first = send_message(api, chat, "Hello", request_id=rid)
    assert first.terminal.event == "done"

    counts = seed.counts(chat)
    turn_before = row_dict(seed.turn(chat, rid))
    chat_before = row_dict(seed.chat(chat))
    outbox_before = seed.outbox_payload_count()
    calls = len(chat_requests(mock_provider))

    replay = send_message(api, chat, "Hello", request_id=rid)
    replay.assert_well_formed()
    assert replay.names_no_ping() == ["stream_started", "delta", "done"]
    started = replay.started
    assert (started["request_id"], started["message_id"], started["is_new_turn"]) == (
        rid, first.message_id, False)
    assert set(started) <= {"request_id", "message_id", "is_new_turn", "thread_summary_applied"}
    assert replay.of("delta")[0].data == {"type": "text", "content": "Hello there"}
    done = replay.done
    assert done["usage"] == {"input_tokens": 20, "output_tokens": 6}
    assert done["effective_model"] == PREMIUM and done["selected_model"] == PREMIUM
    assert done["quota_decision"] == "allow"
    assert not done.get("downgrade_reason")

    assert len(chat_requests(mock_provider)) == calls, "no provider call"
    assert seed.counts(chat) == counts, "no new rows"
    assert row_dict(seed.turn(chat, rid)) == turn_before, "the turn row is untouched"
    assert seed.chat(chat)["updated_at"] == chat_before["updated_at"], "updated_at is not bumped"
    assert seed.outbox_payload_count() == outbox_before, "no new outbox message"


def test_a_replay_ignores_the_body_content_and_does_not_store_it(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["original answer"]))
    send_message(api, chat, "original question", request_id=rid)
    replay = send_message(api, chat, "a completely different question", request_id=rid)
    assert replay.text() == "original answer"
    rows = seed.ordered_messages(chat)
    assert len(rows) == 2 and all("different" not in r["content"] for r in rows)


def test_an_older_turn_can_be_replayed_after_newer_ones(api, seed, mock_provider):
    chat = new_chat(seed)
    first_rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["first answer"]),
                         sse_text(["second answer"]))
    send_message(api, chat, "first", request_id=first_rid)
    send_message(api, chat, "second")
    assert send_message(api, chat, "first", request_id=first_rid).text() == "first answer"
    assert len(chat_requests(mock_provider)) == 2


def test_a_replay_rebuilds_usage_and_the_decision_from_the_stored_rows(api, seed, mock_provider):
    chat = new_chat(seed)  # mock-premium
    rid = str(uuid.uuid4())
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", request_id=rid,
                        content="stored question", created=1)
    assistant = seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant",
                                    request_id=rid, content="stored answer", created=2,
                                    model=catalog.STANDARD_ID, input_tokens=77, output_tokens=9)
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=rid, state="completed",
                     user=USER_A_ID, assistant_message=assistant,
                     effective_model=catalog.STANDARD_ID)
    replay = send_message(api, chat, "anything", request_id=rid)
    assert replay.started["message_id"] == assistant
    assert replay.started["is_new_turn"] is False
    assert replay.text() == "stored answer"
    done = replay.done
    assert done["usage"] == {"input_tokens": 77, "output_tokens": 9}
    assert done["effective_model"] == catalog.STANDARD_ID
    assert done["selected_model"] == PREMIUM
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == PREMIUM
    assert not done.get("downgrade_reason"), "the reason is not persisted"
    assert not chat_requests(mock_provider)


def test_replaying_someone_elses_turn_is_404(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    send_message(api, chat, "q", request_id=rid)
    calls = len(chat_requests(mock_provider))
    for user in ("a_reviewer", "b"):
        resp = reject_message(api, chat, {"content": "q", "request_id": rid}, user=user)
        assert_problem(resp, 404, "not_found")
    assert len(chat_requests(mock_provider)) == calls


def test_the_same_request_id_in_another_chat_is_a_new_turn(api, seed, mock_provider):
    rid = str(uuid.uuid4())
    one, two = new_chat(seed), new_chat(seed)
    first = send_message(api, one, "q", request_id=rid)
    second = send_message(api, two, "q", request_id=rid)
    assert first.started["is_new_turn"] is True and second.started["is_new_turn"] is True
    assert first.message_id != second.message_id
    assert len(chat_requests(mock_provider)) == 2


# --------------------------------------------------------------------------- #
# every other state of the key is a conflict
# --------------------------------------------------------------------------- #


def assert_generic_conflict(resp, internal_ids=()):
    assert_conflict(resp, "request_id_conflict")
    lowered = resp.text.lower()
    assert "unique" not in lowered and "sqlite" not in lowered, resp.text
    for internal in internal_ids:
        assert internal not in resp.text, f"internal id leaked: {resp.text}"


def test_reusing_the_request_id_of_a_running_turn_is_409_request_id_conflict(api, seed,
                                                                           mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    live = hold_running(api, mock_provider, chat, rid)
    turn_id = uuid_text(seed.turn(chat, rid)["id"])
    counts = seed.counts(chat)
    resp = reject_message(api, chat, {"content": "again", "request_id": rid})
    assert_generic_conflict(resp, [turn_id])
    assert len(chat_requests(mock_provider)) == 1
    assert seed.counts(chat) == counts
    release_and_finish(mock_provider, live)


def test_reusing_the_request_id_of_a_failed_turn_is_409_request_id_conflict(api, seed,
                                                                          mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", error_response(500, "nope"))
    assert send_message(api, chat, "q", request_id=rid).terminal.event == "error"
    calls = len(chat_requests(mock_provider))
    resp = reject_message(api, chat, {"content": "q", "request_id": rid})
    assert_generic_conflict(resp)
    assert len(chat_requests(mock_provider)) == calls
    assert seed.turn(chat, rid)["state"] == "failed", "a turn is never reused"


def test_reusing_the_request_id_of_a_cancelled_deleted_or_orphaned_turn_is_409(api, seed,
                                                                             mock_provider):
    chat = new_chat(seed)
    cancelled, deleted, orphan = (str(uuid.uuid4()) for _ in range(3))
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=cancelled, state="cancelled")
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=deleted, state="completed",
                     deleted=True)
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=orphan, state="failed",
                     error_code="orphan_timeout")
    for rid in (cancelled, deleted, orphan):
        assert_generic_conflict(reject_message(api, chat, {"content": "q", "request_id": rid}))
    assert not chat_requests(mock_provider)
    assert len(seed.turns_of(chat)) == 3


def test_two_sends_with_the_same_new_request_id_yield_one_stream_and_one_conflict(
        api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["slow"], pause_after_deltas=0))
    body = {"content": "race", "request_id": rid}
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(open_stream, api, chat, body) for _ in range(2)]
        lives = [f.result() for f in futures]
    streams = [live for live in lives if live.is_sse]
    losers = [live for live in lives if not live.is_sse]
    assert len(streams) == 1 and len(losers) == 1, [live.status_code for live in lives]
    assert_conflict(losers[0].read(), "request_id_conflict")
    mock_provider.release()
    try:
        events = streams[0].drain()
    finally:
        streams[0].close()
    assert events[-1].event == "done"
    assert len(chat_requests(mock_provider)) == 1
    assert len(seed.ordered_messages(chat)) == 2


# --------------------------------------------------------------------------- #
# at most one running turn per chat
# --------------------------------------------------------------------------- #


def test_a_second_send_while_a_turn_runs_is_409_turn_already_running(api, seed, mock_provider):
    chat = new_chat(seed)
    live = hold_running(api, mock_provider, chat, str(uuid.uuid4()))
    counts = seed.counts(chat)
    for body in ({"content": "another", "request_id": str(uuid.uuid4())},
                 {"content": "no request id at all"}):
        assert_conflict(reject_message(api, chat, body), "turn_already_running")
    assert len(chat_requests(mock_provider)) == 1, "the provider is called once"
    assert seed.counts(chat) == counts, "no user message of the rejected sends"
    assert release_and_finish(mock_provider, live)[-1].event == "done"


def test_after_the_first_turn_completes_a_new_send_is_accepted(api, seed, mock_provider):
    chat = new_chat(seed)
    live = hold_running(api, mock_provider, chat, str(uuid.uuid4()))
    assert_conflict(reject_message(api, chat, {"content": "too early"}), "turn_already_running")
    release_and_finish(mock_provider, live)
    assert send_message(api, chat, "now it is fine").terminal.event == "done"
    assert len(seed.ordered_messages(chat)) == 4


def test_the_guard_is_per_chat(api, seed, mock_provider):
    busy, other = new_chat(seed), new_chat(seed)
    live = hold_running(api, mock_provider, busy, str(uuid.uuid4()))
    mock_provider.script("POST", "/v1/responses", sse_text(["unaffected"]))
    assert send_message(api, other, "hello").terminal.event == "done"
    release_and_finish(mock_provider, live)


def test_two_simultaneous_sends_with_different_request_ids_yield_one_stream_and_one_409(
        api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["slow"], pause_after_deltas=0))
    bodies = [{"content": "one", "request_id": str(uuid.uuid4())},
              {"content": "two", "request_id": str(uuid.uuid4())}]
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(open_stream, api, chat, body) for body in bodies]
        lives = [f.result() for f in futures]
    streams = [live for live in lives if live.is_sse]
    losers = [live for live in lives if not live.is_sse]
    assert len(streams) == 1 and len(losers) == 1, [live.status_code for live in lives]
    assert_conflict(losers[0].read(), "turn_already_running")
    mock_provider.release()
    try:
        assert streams[0].drain()[-1].event == "done"
    finally:
        streams[0].close()
    assert len(chat_requests(mock_provider)) == 1
    assert len(seed.ordered_messages(chat)) == 2, "the loser left nothing behind"
    assert len(seed.turns_of(chat)) == 1


def test_a_running_turn_left_by_a_crashed_pod_blocks_the_chat(api, seed, mock_provider):
    chat = new_chat(seed)
    stuck = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=stuck, state="running")
    other = reject_message(api, chat, {"content": "q", "request_id": str(uuid.uuid4())})
    assert_conflict(other, "turn_already_running")
    same = reject_message(api, chat, {"content": "q", "request_id": stuck})
    assert_conflict(same, "request_id_conflict")
    assert not chat_requests(mock_provider)


def test_the_idempotency_check_runs_before_the_parallel_turn_guard(api, seed, mock_provider):
    chat = new_chat(seed)
    done_rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["finished earlier"]))
    send_message(api, chat, "q", request_id=done_rid)

    live = hold_running(api, mock_provider, chat, str(uuid.uuid4()))
    assert_conflict(reject_message(api, chat, {"content": "x"}), "turn_already_running")
    replay = send_message(api, chat, "q", request_id=done_rid)
    assert replay.started["is_new_turn"] is False
    assert replay.text() == "finished earlier"
    release_and_finish(mock_provider, live)


def test_other_users_and_tenants_are_not_blocked_by_a_running_turn(api, seed, mock_provider):
    chat = new_chat(seed)
    live = hold_running(api, mock_provider, chat, str(uuid.uuid4()))
    reviewer_chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, model=PREMIUM)
    tenant_b_chat = seed.insert_chat(tenant=TENANT_B_ID, user=USER_B_ID, model=PREMIUM)
    mock_provider.script("POST", "/v1/responses", sse_text(["one"]), sse_text(["two"]))
    assert send_message(api, reviewer_chat, "hi", user="a_reviewer").terminal.event == "done"
    assert send_message(api, tenant_b_chat, "hi", user="b").terminal.event == "done"
    release_and_finish(mock_provider, live)


# --------------------------------------------------------------------------- #
# concurrent sends: deterministic 409 classification (insert race)
# --------------------------------------------------------------------------- #

RACERS = 6


def race(api, chat, request_ids):
    """Open one send per request id at the same time; return ``(streams, conflict_reasons)``."""
    bodies = [{"content": "race", "request_id": rid} for rid in request_ids]
    with ThreadPoolExecutor(max_workers=len(bodies)) as pool:
        lives = [f.result() for f in [pool.submit(open_stream, api, chat, b) for b in bodies]]
    streams, reasons = [], []
    for live in lives:
        if live.is_sse:
            streams.append(live)
        else:
            resp = live.read()
            assert resp.status_code == 409, resp.text
            reasons.append(resp.json()["context"].get("reason"))
    return streams, reasons


def drain_all(streams):
    results = []
    for live in streams:
        try:
            results.append(live.drain())
        finally:
            live.close()
    return results


def test_racing_sends_with_different_ids_yield_exactly_one_stream_and_turn_already_running(
        api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["slow"], pause_after_deltas=0))
    streams, reasons = race(api, chat, [str(uuid.uuid4()) for _ in range(RACERS)])
    assert len(streams) == 1, (len(streams), reasons)
    assert reasons == ["turn_already_running"] * (RACERS - 1), reasons
    mock_provider.release()
    assert drain_all(streams)[0][-1].event == "done"
    assert len(chat_requests(mock_provider)) == 1
    assert len(seed.turns_of(chat)) == 1
    assert len(seed.ordered_messages(chat)) == 2, "the losers left nothing behind"


def test_racing_sends_with_the_same_id_yield_exactly_one_stream_and_request_id_conflict(
        api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["slow"], pause_after_deltas=0))
    rid = str(uuid.uuid4())
    streams, reasons = race(api, chat, [rid] * RACERS)
    assert len(streams) == 1, (len(streams), reasons)
    assert reasons == ["request_id_conflict"] * (RACERS - 1), reasons
    mock_provider.release()
    assert drain_all(streams)[0][-1].event == "done"
    assert len(chat_requests(mock_provider)) == 1
    assert len(seed.turns_of(chat)) == 1


def test_fast_finalizing_winners_never_turn_a_lost_race_into_already_exists(
        api, seed, mock_provider):
    """The provider fails instantly, so the winner usually finalizes before a loser re-reads:
    every loser is still `turn_already_running` (different ids) / `request_id_conflict`
    (same id, or a replay of a completed turn) - never `already_exists` / `unique_violation`."""
    mock_provider.set_default("POST", "/v1/responses", error_response(500, "fast failure"))
    for _ in range(12):
        chat = new_chat(seed)
        streams, reasons = race(api, chat, [str(uuid.uuid4()) for _ in range(RACERS)])
        drain_all(streams)
        assert streams, "someone always wins"
        assert set(reasons) <= {"turn_already_running"}, reasons
        assert len(seed.turns_of(chat)) == len(streams)

        chat = new_chat(seed)
        streams, reasons = race(api, chat, [str(uuid.uuid4())] * 4)
        drain_all(streams)
        assert len(streams) == 1, (len(streams), reasons)
        assert set(reasons) <= {"request_id_conflict"}, reasons
        assert len(seed.turns_of(chat)) == 1
