"""Quota and billing black-box: quota before outbound (429), the reserve written before the provider
is called, the tier downgrade, settlement by actual / estimated usage and ``done.quota_warnings``.

The tests run on the ``quota`` stack (``conftest.quota_stack``): an extra server with small plugin
limits (``catalog.QUOTA_LIMITS``), warning threshold 75 %, its own mock provider and database. Every
test starts from empty ``quota_usage`` and seeds rows directly where exhaustion would otherwise need
many turns.

Source: DESIGN 2.2 "Quota Before Outbound" and kill switches, 3.2 "Quota Warning Thresholds",
3.3 ``event: done``, 3.6 "Send Message with Streaming Response", 3.7 ``chat_turns`` /
``quota_usage``, 4 "Two-Tier Rate Limiting & Throttling" / "Downgrade Decision Flow" / "Effective
Model Computed Once", 5.3 - 5.10 (credit arithmetic, reserve, settlement, overshoot, estimated
settlement, terminal outcomes), the REST error table (429 ``resource_exhausted``).
"""

from __future__ import annotations

import threading
import uuid
from concurrent.futures import ThreadPoolExecutor

import pytest

from . import catalog
from .mock_openai_provider import (
    ProviderResponse,
    error_response,
    failed_stream_events,
    sse_text,
)
from .support import (
    TENANT_A_ID,
    TENANT_IDS,
    USER_A_ID,
    USER_IDS,
    assert_field_violation,
    assert_problem,
    open_stream,
    period_start,
    read_all,
    reject_message,
    send_message,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
DISABLED = catalog.DISABLED_ID
MSG = "hello"
LIMITS = catalog.QUOTA_LIMITS
RESPONSES = ("POST", "/v1/responses")


@pytest.fixture()
def qapi(quota):
    client = quota.api()
    yield client
    client.close()


def new_chat(stack, model=PREMIUM, user="a"):
    return stack.seeder().insert_chat(tenant=TENANT_IDS[user], user=USER_IDS[user], model=model)


def chat_requests(stack):
    return stack.mock.requests_to(*RESPONSES)


def answer(stack, input_tokens, output_tokens, chunks=("Hel", "lo")):
    stack.mock.script(*RESPONSES, sse_text(list(chunks), input_tokens=input_tokens,
                                           output_tokens=output_tokens))


def exhaust(stack, bucket, period, user="a"):
    stack.seeder().insert_quota(tenant=TENANT_IDS[user], user=USER_IDS[user], bucket=bucket,
                                period=period, spent=LIMITS[bucket][period])


def amounts(stack, bucket, period, user="a"):
    return stack.db().quota_amounts(USER_IDS[user], bucket, period)


def assert_exhausted(resp, scope):
    problem = assert_problem(resp, 429, "resource_exhausted")
    violations = problem["context"]["violations"]
    assert violations[0]["subject"] == scope, problem
    assert violations[0]["description"] == "quota_exceeded", problem
    return problem


def assert_nothing_reserved(stack):
    for row in stack.db().quota_rows():
        assert row["reserved_credits_micro"] == 0, f"reserve still held: {dict(row)}"


def assert_settled(stack, *, premium, spent, calls=1, input_tokens=0, output_tokens=0, user="a"):
    """Both periods of the buckets of ``user`` after a settlement."""
    db = stack.db()
    for period in ("daily", "monthly"):
        total = db.quota_row(USER_IDS[user], "total", period)
        assert total is not None, f"no total/{period} row"
        assert (total["spent_credits_micro"], total["reserved_credits_micro"], total["calls"],
                total["input_tokens"], total["output_tokens"]) == \
            (spent, 0, calls, input_tokens, output_tokens), f"total/{period}: {dict(total)}"
        tier = db.quota_row(USER_IDS[user], "tier:premium", period)
        if premium:
            assert tier is not None, f"no tier:premium/{period} row"
            assert (tier["spent_credits_micro"], tier["reserved_credits_micro"], tier["calls"],
                    tier["input_tokens"], tier["output_tokens"]) == (spent, 0, calls, 0, 0), \
                f"tier:premium/{period}: {dict(tier)}"
        else:
            assert tier is None, "a standard turn never touches the premium bucket"


def wait_state(stack, chat, rid, state):
    return wait_for(
        lambda: (lambda t: t if t is not None and t["state"] == state else None)(
            stack.db().turn(chat, rid)),
        f"turn {rid} to become {state}")


# --------------------------------------------------------------------------- #
# quota before outbound: 429 resource_exhausted, JSON, nothing written
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("period", ["daily", "monthly"])
def test_all_tiers_exhausted_is_429_tokens_before_any_provider_call(quota, qapi, period):
    chat = new_chat(quota)
    exhaust(quota, "total", period)
    before = quota.db().counts(chat)
    rows_before = len(quota.db().quota_rows())

    resp = reject_message(qapi, chat, {"content": MSG})

    assert_exhausted(resp, "tokens")
    assert not chat_requests(quota), "the provider is never called"
    assert quota.db().counts(chat) == before, "no message, turn or link is written"
    assert len(quota.db().quota_rows()) == rows_before, "no quota row is touched"


def test_the_rejection_is_json_not_an_event_stream(quota, qapi):
    chat = new_chat(quota)
    exhaust(quota, "total", "daily")
    live = open_stream(qapi, chat, {"content": MSG})
    assert not live.is_sse
    resp = live.read()
    assert resp.headers["content-type"].startswith("application/problem+json")
    assert resp.status_code == 429


@pytest.mark.parametrize("model", [PREMIUM, STANDARD])
def test_the_reserve_must_fit_and_an_exact_fit_is_admitted(quota, qapi, model):
    reserve = catalog.reserve_for(model, MSG)["reserved_credits_micro"]
    # A premium chat is downgraded when only the cheaper standard reserve still fits the shared
    # cap (DESIGN 4 "Downgrade Decision Flow" 3.c), so the request is rejected only when not even
    # the cheapest candidate of the cascade fits: for a standard chat that is its own reserve.
    cheapest = catalog.reserve_for(STANDARD, MSG)["reserved_credits_micro"]
    limit = LIMITS["total"]["daily"]
    chat = new_chat(quota, model)

    # one micro-credit too little room left in the shared cap for every candidate: rejected
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=limit - cheapest + 1)
    assert_exhausted(reject_message(qapi, chat, {"content": MSG}), "tokens")
    assert not chat_requests(quota)

    # exactly enough room: admitted
    quota.seeder().clear_quota()
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=limit - reserve)
    sent = send_message(qapi, chat, MSG)
    assert sent.names_no_ping()[-1] == "done", sent.events


def test_reserves_of_other_in_flight_turns_count_against_the_limit(quota, qapi):
    reserve = catalog.reserve_for(STANDARD, MSG)["reserved_credits_micro"]
    chat = new_chat(quota, STANDARD)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", reserved=LIMITS["total"]["daily"] - reserve + 1)
    assert_exhausted(reject_message(qapi, chat, {"content": MSG}), "tokens")


def test_validation_and_input_length_follow_the_documented_order(quota, qapi):
    chat = new_chat(quota)
    # within quota the message is too long
    resp = reject_message(qapi, chat, {"content": "x" * 450_000})
    assert_problem(resp, 400, "out_of_range")

    exhaust(quota, "total", "daily")
    # empty content is a 400 even though the quota is gone ...
    resp = reject_message(qapi, chat, {"content": "  "})
    assert_field_violation(assert_problem(resp, 400, "invalid_argument"), "content",
                           "EMPTY_CONTENT")
    # ... and the quota check comes before the input length check
    assert_exhausted(reject_message(qapi, chat, {"content": "x" * 450_000}), "tokens")


def test_a_replay_of_a_completed_turn_wins_over_an_exhausted_quota(quota, qapi):
    chat = new_chat(quota)
    rid = str(uuid.uuid4())
    first = send_message(qapi, chat, MSG, request_id=rid)
    assert first.names_no_ping()[-1] == "done"
    exhaust(quota, "total", "monthly")
    requests = len(chat_requests(quota))
    before = [dict(r) for r in quota.db().quota_rows()]

    replay = send_message(qapi, chat, MSG, request_id=rid)

    assert replay.started["is_new_turn"] is False
    assert replay.names_no_ping()[-1] == "done"
    assert len(chat_requests(quota)) == requests
    assert [dict(r) for r in quota.db().quota_rows()] == before, "a replay is side-effect free"


def test_the_quota_of_one_user_does_not_limit_another(quota, qapi):
    for user in ("a_reviewer", "b"):
        exhaust(quota, "total", "daily", user=user)
    chat = new_chat(quota)
    sent = send_message(qapi, chat, MSG)
    assert sent.names_no_ping()[-1] == "done"
    other = new_chat(quota, user="a_reviewer")
    assert_exhausted(reject_message(qapi, other, {"content": MSG}, user="a_reviewer"), "tokens")


# --------------------------------------------------------------------------- #
# reserve before execute
# --------------------------------------------------------------------------- #


def test_the_reserve_is_booked_while_the_provider_runs(quota, qapi):
    chat = new_chat(quota)
    r = catalog.reserve_for(PREMIUM, MSG)
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], pause_after_deltas=1, input_tokens=800,
                                           output_tokens=200))
    live = open_stream(qapi, chat, {"content": MSG})
    rid = live.expect("stream_started").data["request_id"]
    quota.mock.wait_paused()

    turn = quota.db().turn(chat, rid)
    assert turn["state"] == "running"
    assert turn["reserve_tokens"] == r["reserve_tokens"]
    assert turn["max_output_tokens_applied"] == r["max_output_tokens_applied"]
    assert turn["reserved_credits_micro"] == r["reserved_credits_micro"]
    assert turn["policy_version_applied"] == 1, "the static plugin serves policy version 1"
    assert turn["effective_model"] == PREMIUM
    assert turn["minimal_generation_floor_applied"] == catalog.MINIMAL_GENERATION_FLOOR
    for bucket in ("total", "tier:premium"):
        for period in ("daily", "monthly"):
            row = quota.db().quota_row(USER_A_ID, bucket, period)
            assert row is not None, f"no {bucket}/{period} row while the turn runs"
            assert row["reserved_credits_micro"] == r["reserved_credits_micro"], \
                f"{bucket}/{period}"
            assert row["spent_credits_micro"] == 0
            assert row["calls"] == 0

    quota.mock.release()
    read_all(live)
    assert_settled(quota, premium=True,
                   spent=catalog.model_credits(PREMIUM, 800, 200), input_tokens=800,
                   output_tokens=200)
    done_turn = quota.db().turn(chat, rid)
    for column in ("reserve_tokens", "max_output_tokens_applied", "reserved_credits_micro",
                   "policy_version_applied", "effective_model", "minimal_generation_floor_applied"):
        assert done_turn[column] == turn[column], f"{column} is immutable after insert"


def test_a_standard_turn_reserves_only_the_total_bucket(quota, qapi):
    chat = new_chat(quota, STANDARD)
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], pause_after_deltas=1))
    live = open_stream(qapi, chat, {"content": MSG})
    live.expect("stream_started")
    quota.mock.wait_paused()
    reserve = catalog.reserve_for(STANDARD, MSG)["reserved_credits_micro"]
    assert quota.db().quota_amounts(USER_A_ID, "total", "daily") == (0, reserve)
    assert quota.db().quota_row(USER_A_ID, "tier:premium", "daily") is None
    quota.mock.release()
    read_all(live)


def test_period_starts_are_the_utc_day_and_the_first_of_the_month(quota, qapi):
    chat = new_chat(quota)
    candidates_daily = {period_start("daily")}
    send_message(qapi, chat, MSG)
    candidates_daily.add(period_start("daily"))
    rows = quota.db().quota_rows(USER_A_ID)
    assert len(rows) == 4
    for row in rows:
        if row["period_type"] == "daily":
            assert row["period_start"] in candidates_daily
        else:
            assert row["period_type"] == "monthly"
            assert row["period_start"].endswith("-01")
            assert row["period_start"] == period_start("monthly")


def test_prior_context_tokens_come_from_the_newest_assistant_message_with_usage(quota, qapi):
    chat = new_chat(quota)
    seed = quota.seeder()
    rid = str(uuid.uuid4())
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", request_id=rid, created=1)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", request_id=rid,
                        created=2, model=PREMIUM, input_tokens=600, output_tokens=287)
    other = str(uuid.uuid4())
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", request_id=other,
                        created=3, model=PREMIUM, input_tokens=0, output_tokens=0)
    sent = send_message(qapi, chat, MSG)
    turn = quota.db().turn(chat, sent.request_id)
    r = catalog.reserve_for(PREMIUM, MSG, prior_context=887)
    assert turn["reserve_tokens"] == r["reserve_tokens"]
    assert turn["reserved_credits_micro"] == r["reserved_credits_micro"]


def test_the_output_cap_and_the_floor_are_persisted_per_turn(quota, qapi):
    chat = new_chat(quota)
    sent = send_message(qapi, chat, MSG)
    turn = quota.db().turn(chat, sent.request_id)
    assert turn["max_output_tokens_applied"] == 4096
    assert turn["minimal_generation_floor_applied"] == 50
    assert chat_requests(quota)[-1].json["max_output_tokens"] == 4096


def test_concurrent_first_turns_of_one_user_all_succeed_on_a_fresh_bucket(quota):
    chats = [new_chat(quota) for _ in range(5)]

    def run(chat):
        api = quota.api()
        try:
            return send_message(api, chat, MSG)
        finally:
            api.close()

    with ThreadPoolExecutor(max_workers=5) as pool:
        results = list(pool.map(run, chats))
    for sent in results:
        assert sent.names_no_ping()[-1] == "done", sent.events

    rows = quota.db().quota_rows(USER_A_ID)
    assert len(rows) == 4, [dict(r) for r in rows]
    total = quota.db().quota_row(USER_A_ID, "total", "daily")
    assert total["calls"] == 5
    assert total["reserved_credits_micro"] == 0


def test_concurrent_requests_cannot_book_more_than_the_limit(quota):
    reserve = catalog.reserve_for(STANDARD, MSG)["reserved_credits_micro"]
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=LIMITS["total"]["daily"] - reserve)
    chats = [new_chat(quota, STANDARD) for _ in range(4)]
    quota.mock.set_default(*RESPONSES, sse_text(["a", "b"], pause_after_deltas=1))
    lives = [None] * len(chats)
    barrier = threading.Barrier(len(chats))

    def start(i):
        api = quota.api()
        barrier.wait()
        lives[i] = (api, open_stream(api, chats[i], {"content": MSG}))

    with ThreadPoolExecutor(max_workers=len(chats)) as pool:
        list(pool.map(start, range(len(chats))))
    admitted = [pair for pair in lives if pair[1].is_sse]
    rejected = [pair for pair in lives if not pair[1].is_sse]
    assert len(admitted) == 1, "room for exactly one reserve"
    for _api, live in rejected:
        assert_exhausted(live.read(), "tokens")
    assert quota.db().quota_amounts(USER_A_ID, "total", "daily") == \
        (LIMITS["total"]["daily"] - reserve, reserve)

    quota.mock.release()
    for api, live in admitted:
        read_all(live)
    for api, _live in lives:
        api.close()
    assert_nothing_reserved(quota)


# --------------------------------------------------------------------------- #
# tier downgrade
# --------------------------------------------------------------------------- #


def done_of(api, chat, text=MSG):
    sent = send_message(api, chat, text)
    assert sent.names_no_ping()[-1] == "done", sent.events
    return sent.done, sent.request_id


@pytest.mark.parametrize("period", ["daily", "monthly"])
def test_an_exhausted_premium_tier_runs_on_the_standard_model(quota, qapi, period):
    chat = new_chat(quota)
    exhaust(quota, "tier:premium", period)
    answer(quota, 20, 10)

    done, rid = done_of(qapi, chat)

    assert done["quota_decision"] == "downgrade"
    assert done["selected_model"] == PREMIUM
    assert done["effective_model"] == STANDARD
    assert done["downgrade_from"] == PREMIUM
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    request = chat_requests(quota)[-1]
    assert request.json["model"] == catalog.entry(STANDARD)["provider_model_id"], \
        "the provider gets the downgraded model's provider_model_id"
    turn = quota.db().turn(chat, rid)
    assert turn["effective_model"] == STANDARD
    assert turn["reserved_credits_micro"] == catalog.reserve_for(STANDARD, MSG)[
        "reserved_credits_micro"], "the booked reserve is the standard candidate's"
    assistant = [m for m in quota.db().ordered_messages(chat) if m["role"] == "assistant"][0]
    assert assistant["model"] == STANDARD
    assert quota.seeder().chat(chat)["model"] == PREMIUM, "the chat keeps its selected model"
    premium = quota.db().quota_row(USER_A_ID, "tier:premium", period)
    assert (premium["spent_credits_micro"], premium["reserved_credits_micro"], premium["calls"]) \
        == (LIMITS["tier:premium"][period], 0, 0), "the premium sub-cap is untouched"


def test_a_shared_cap_that_fits_standard_but_not_premium_downgrades_too(quota, qapi):
    chat = new_chat(quota)
    premium = catalog.reserve_for(PREMIUM, MSG)["reserved_credits_micro"]
    standard = catalog.reserve_for(STANDARD, MSG)["reserved_credits_micro"]
    assert standard < premium
    # leave room for the standard reserve only
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=LIMITS["total"]["daily"] - standard)
    done, _ = done_of(qapi, chat)
    assert done["effective_model"] == STANDARD
    assert done["downgrade_reason"] == "premium_quota_exhausted"


def test_a_disabled_selected_model_runs_on_an_enabled_one(quota, qapi):
    chat = new_chat(quota, DISABLED)
    done, rid = done_of(qapi, chat)
    assert done["quota_decision"] == "downgrade"
    assert done["selected_model"] == DISABLED
    assert done["downgrade_from"] == DISABLED
    assert done["downgrade_reason"] == "model_disabled"
    assert done["effective_model"] == STANDARD
    assert quota.db().turn(chat, rid)["effective_model"] == STANDARD


def test_force_standard_tier_downgrades_every_premium_selection(force_standard):
    api = force_standard.api()
    try:
        chat = new_chat(force_standard)
        done, rid = done_of(api, chat)
        assert done["quota_decision"] == "downgrade"
        assert done["downgrade_from"] == PREMIUM
        assert done["downgrade_reason"] == "force_standard_tier"
        assert done["effective_model"] == STANDARD
        assert force_standard.mock.requests_to(*RESPONSES)[-1].json["model"] == \
            catalog.entry(STANDARD)["provider_model_id"]
        # a standard chat is unaffected
        other = new_chat(force_standard, STANDARD)
        done, _ = done_of(api, other)
        assert done["quota_decision"] == "allow"
        assert not done.get("downgrade_reason")
    finally:
        api.close()


def test_disable_premium_tier_downgrades_with_its_own_reason(no_premium):
    api = no_premium.api()
    try:
        chat = new_chat(no_premium)
        done, _ = done_of(api, chat)
        assert done["quota_decision"] == "downgrade"
        assert done["downgrade_reason"] == "disable_premium_tier"
        assert done["effective_model"] == STANDARD
    finally:
        api.close()


def test_a_kill_switch_does_not_bypass_an_exhausted_shared_cap(force_standard):
    api = force_standard.api()
    try:
        chat = new_chat(force_standard)
        force_standard.seeder().insert_quota(
            tenant=TENANT_A_ID, user=USER_A_ID, bucket="total", period="monthly",
            spent=LIMITS["total"]["monthly"])
        assert_exhausted(reject_message(api, chat, {"content": MSG}), "tokens")
    finally:
        api.close()


def test_an_unchanged_selection_is_allow_without_downgrade_fields(quota, qapi):
    chat = new_chat(quota)
    done, _ = done_of(qapi, chat)
    assert done["quota_decision"] == "allow"
    assert done["effective_model"] == done["selected_model"] == PREMIUM
    assert not done.get("downgrade_from") and not done.get("downgrade_reason")


def test_images_after_a_downgrade_to_a_model_without_vision_are_rejected(quota, qapi):
    chat = new_chat(quota)  # premium has VISION_INPUT, standard has not
    image = quota.seeder().insert_attachment(
        tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="photo.png", kind="image")
    exhaust(quota, "tier:premium", "daily")
    before = quota.db().counts(chat)
    rows_before = len(quota.db().quota_rows())

    resp = reject_message(qapi, chat, {"content": MSG, "attachment_ids": [image]})

    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "VISION_NOT_SUPPORTED")
    assert not chat_requests(quota)
    assert quota.db().counts(chat) == before
    assert len(quota.db().quota_rows()) == rows_before, "no reserve is left behind"


# --------------------------------------------------------------------------- #
# settlement
# --------------------------------------------------------------------------- #


def test_a_completed_premium_turn_commits_the_actual_credits(quota, qapi):
    chat = new_chat(quota)
    answer(quota, 800, 200)
    sent = send_message(qapi, chat, MSG)
    assert sent.done["usage"] == {"input_tokens": 800, "output_tokens": 200}
    assert_settled(quota, premium=True, spent=catalog.model_credits(PREMIUM, 800, 200),
                   input_tokens=800, output_tokens=200)
    assistant = [m for m in quota.db().ordered_messages(chat) if m["role"] == "assistant"][0]
    assert (assistant["input_tokens"], assistant["output_tokens"]) == (800, 200)


def test_a_completed_standard_turn_settles_the_total_bucket_only(quota, qapi):
    chat = new_chat(quota, STANDARD)
    answer(quota, 1000, 250)
    send_message(qapi, chat, MSG)
    assert_settled(quota, premium=False, spent=catalog.model_credits(STANDARD, 1000, 250),
                   input_tokens=1000, output_tokens=250)


def test_settlement_accumulates_and_counts_every_turn(quota, qapi):
    chat = new_chat(quota)
    spent = 0
    for i, (inp, out) in enumerate([(100, 10), (200, 20), (300, 30)]):
        answer(quota, inp, out)
        send_message(qapi, chat, f"turn {i}")
        spent += catalog.model_credits(PREMIUM, inp, out)
    assert_settled(quota, premium=True, spent=spent, calls=3, input_tokens=600, output_tokens=60)


def test_an_overshoot_within_the_tolerance_is_committed_in_full(quota, qapi):
    # 4_400 + 200 = 4_600 tokens > the 4_209 reserved tokens, ratio 1.093 <= 1.10
    chat = new_chat(quota)
    answer(quota, 4400, 200)
    sent = send_message(qapi, chat, MSG)
    assert sent.names_no_ping()[-1] == "done"
    assert_settled(quota, premium=True, spent=catalog.model_credits(PREMIUM, 4400, 200),
                   input_tokens=4400, output_tokens=200)


def test_an_overshoot_beyond_the_tolerance_is_capped_at_the_reserve(quota, qapi):
    # 9_000 tokens against a 4_209 token reserve: ratio 2.1 > 1.10 -> committed = reserved credits
    chat = new_chat(quota)
    answer(quota, 9000, 0)
    sent = send_message(qapi, chat, MSG)
    assert sent.names_no_ping()[-1] == "done", "a completed answer is never cancelled"
    assert sent.done["usage"]["input_tokens"] == 9000, "the client sees the actual usage"
    reserved = catalog.reserve_for(PREMIUM, MSG)["reserved_credits_micro"]
    assert_settled(quota, premium=True, spent=reserved, input_tokens=9000, output_tokens=0)
    assert quota.db().turn(chat, sent.request_id)["state"] == "completed"


def test_a_provider_error_settles_estimated(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, error_response(500, "upstream exploded"))
    sent = send_message(qapi, chat, MSG)
    assert sent.error["code"] == "provider_error"
    turn = quota.db().turn(chat, sent.request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "provider_error")
    # the provider request started: never free; the estimated split is not telemetry
    assert_settled(quota, premium=True, spent=catalog.estimated_charge(PREMIUM, MSG))


def test_a_failed_stream_with_usage_settles_actual(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, ProviderResponse(sse_events=failed_stream_events(
        "server_error", "overloaded", partial_chunks=["par"], usage=(700, 40))))
    sent = send_message(qapi, chat, MSG)
    assert sent.error["code"] == "provider_error"
    assert_settled(quota, premium=True, spent=catalog.model_credits(PREMIUM, 700, 40),
                   input_tokens=700, output_tokens=40)


def test_a_failed_stream_with_all_zero_usage_is_estimated(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, ProviderResponse(sse_events=failed_stream_events(
        "server_error", "boom", usage=(0, 0))))
    send_message(qapi, chat, MSG)
    assert_settled(quota, premium=True, spent=catalog.estimated_charge(PREMIUM, MSG))


def test_a_client_disconnect_is_settled_estimated(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["Hel", "lo", " world"], pause_after_deltas=2,
                                           input_tokens=500, output_tokens=400))
    live = open_stream(qapi, chat, {"content": MSG})
    rid = live.expect("stream_started").data["request_id"]
    live.next_non_ping()
    live.next_non_ping()
    quota.mock.wait_paused()
    live.close()
    quota.mock.wait_client_disconnected()

    wait_state(quota, chat, rid, "cancelled")
    # the cancel path carries no usage: estimated input + the floor, never free
    assert_settled(quota, premium=True, spent=catalog.estimated_charge(PREMIUM, MSG))


def test_the_reserve_returns_to_zero_after_every_terminal_outcome(quota, qapi):
    chat = new_chat(quota)
    answer(quota, 10, 10)
    send_message(qapi, chat, "one")
    assert_nothing_reserved(quota)
    quota.mock.script(*RESPONSES, error_response(500, "boom"))
    send_message(qapi, chat, "two")
    assert_nothing_reserved(quota)
    quota.mock.script(*RESPONSES, sse_text(["x"], pause_after_deltas=0))
    live = open_stream(qapi, chat, {"content": "three"})
    rid = live.expect("stream_started").data["request_id"]
    quota.mock.wait_paused()
    live.close()
    wait_state(quota, chat, rid, "cancelled")
    assert_nothing_reserved(quota)
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["calls"] == 3


def test_a_replay_never_takes_a_reserve_or_debits_credits(quota, qapi):
    chat = new_chat(quota)
    rid = str(uuid.uuid4())
    answer(quota, 800, 200)
    send_message(qapi, chat, MSG, request_id=rid)
    before = [dict(r) for r in quota.db().quota_rows()]
    for _ in range(3):
        replay = send_message(qapi, chat, MSG, request_id=rid)
        assert replay.started["is_new_turn"] is False
    assert [dict(r) for r in quota.db().quota_rows()] == before
    assert len(chat_requests(quota)) == 1


# --------------------------------------------------------------------------- #
# done.quota_warnings
# --------------------------------------------------------------------------- #


def warning(done, tier, period):
    found = [w for w in done.get("quota_warnings") or []
             if w["tier"] == tier and w["period"] == period]
    return found[0] if found else None


def test_done_carries_a_warning_entry_per_tier_and_period(quota, qapi):
    chat = new_chat(quota, STANDARD)
    answer(quota, 1000, 200)
    done, _ = done_of(qapi, chat)
    for tier in ("total", "premium"):
        for period in ("daily", "monthly"):
            w = warning(done, tier, period)
            assert w is not None, f"no {tier}/{period} in {done.get('quota_warnings')}"
            assert set(w) - {"next_reset"} == {"tier", "period", "remaining_percentage",
                                               "warning", "exhausted"}
    spent = catalog.model_credits(STANDARD, 1000, 200)
    limit = LIMITS["total"]["daily"]
    w = warning(done, "total", "daily")
    assert w["remaining_percentage"] == (limit - spent) * 100 // limit, \
        "the figures are taken after this turn's settlement"
    assert w["warning"] is False and w["exhausted"] is False
    assert not w.get("next_reset"), "next_reset only with a warning or exhausted flag"
    assert warning(done, "premium", "daily")["remaining_percentage"] == 100


def test_the_threshold_flags_the_warning_and_adds_next_reset(quota, qapi):
    chat = new_chat(quota, STANDARD)
    limit = LIMITS["total"]["daily"]
    # 80 % of the shared daily cap used before the turn: remaining 20 % <= 100 - 75 -> warning
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=limit * 80 // 100)
    answer(quota, 1000, 200)
    done, _ = done_of(qapi, chat)
    w = warning(done, "total", "daily")
    assert w["warning"] is True and w["exhausted"] is False
    assert 0 < w["remaining_percentage"] <= 25
    assert w["next_reset"], "next_reset is present once warning is true"
    monthly = warning(done, "total", "monthly")
    assert monthly["warning"] is False and not monthly.get("next_reset")


def test_there_are_no_quota_warnings_on_errors_or_replays(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, error_response(500, "boom"))
    failed = send_message(qapi, chat, MSG)
    assert not failed.of("done")
    assert "quota_warnings" not in failed.blob()

    rid = str(uuid.uuid4())
    first = send_message(qapi, chat, "again", request_id=rid)
    assert first.done.get("quota_warnings"), first.done
    replay = send_message(qapi, chat, "again", request_id=rid)
    assert not replay.done.get("quota_warnings"), "absent on replay"
