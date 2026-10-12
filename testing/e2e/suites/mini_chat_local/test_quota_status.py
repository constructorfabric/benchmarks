"""``GET /v1/quota/status`` black-box: the per-tier, per-period quota of the caller.

Runs on the ``quota`` stack (small plugin limits, ``warning_threshold_pct`` 75): the status must
match the configured limits, count ``spent + reserved`` as used, follow the turns that ran, be
scoped to tenant + user and never write anything.

Source: DESIGN 3.2 "Quota Status Endpoint (P1)" and "Quota Warning Thresholds (P1)", 3.8 (USER_QUOTA
resource, action ``read``), 5.4.2, the OpenAPI schemas ``QuotaStatusResponse`` /
``QuotaTierStatus`` / ``QuotaPeriodStatus`` and the operation ``mini_chat.get_quota_status``.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest

from . import catalog
from .mock_openai_provider import sse_text
from .support import (
    TENANT_IDS,
    USER_IDS,
    assert_problem,
    next_reset,
    open_stream,
    parse_ts,
    read_all,
    send_message,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
LIMITS = catalog.QUOTA_LIMITS
PERIOD_KEYS = {"period", "limit_credits_micro", "used_credits_micro", "remaining_credits_micro",
               "remaining_percentage", "next_reset", "warning", "exhausted"}
RESPONSES = ("POST", "/v1/responses")


@pytest.fixture()
def qapi(quota):
    client = quota.api()
    yield client
    client.close()


def status(api, user="a"):
    resp = api.get("/v1/quota/status", user=user)
    assert resp.status_code == 200, resp.text
    return resp.json()


def entry(body, tier, period):
    tiers = [t for t in body["tiers"] if t["tier"] == tier]
    assert tiers, f"tier {tier} missing in {body}"
    found = [p for p in tiers[0]["periods"] if p["period"] == period]
    assert found, f"{tier}/{period} missing in {tiers[0]}"
    return found[0]


def seed_row(stack, bucket, period, *, user="a", **amounts):
    stack.seeder().insert_quota(tenant=TENANT_IDS[user], user=USER_IDS[user], bucket=bucket,
                                period=period, **amounts)


def new_chat(stack, model=PREMIUM, user="a"):
    return stack.seeder().insert_chat(tenant=TENANT_IDS[user], user=USER_IDS[user], model=model)


# --------------------------------------------------------------------------- #
# the contract
# --------------------------------------------------------------------------- #


def test_the_status_requires_authentication(quota, qapi):
    resp = qapi.get("/v1/quota/status", user=None)
    assert_problem(resp, 401, "unauthenticated")


def test_a_fresh_user_sees_every_limit_untouched(quota, qapi):
    body = status(qapi)
    assert set(body) == {"tiers", "warning_threshold_pct"}
    assert body["warning_threshold_pct"] == catalog.WARNING_THRESHOLD_PCT
    assert {t["tier"] for t in body["tiers"]} == {"premium", "total"}
    for tier, bucket in (("total", "total"), ("premium", "tier:premium")):
        for period in ("daily", "monthly"):
            p = entry(body, tier, period)
            assert set(p) == PERIOD_KEYS
            limit = LIMITS[bucket][period]
            assert p["limit_credits_micro"] == limit
            assert p["used_credits_micro"] == 0
            assert p["remaining_credits_micro"] == limit
            assert p["remaining_percentage"] == 100
            assert p["warning"] is False and p["exhausted"] is False


def test_next_reset_is_the_next_utc_midnight_and_the_first_of_next_month(quota, qapi):
    before = datetime.now(timezone.utc)
    body = status(qapi)
    after = datetime.now(timezone.utc)
    for tier in ("total", "premium"):
        daily = parse_ts(entry(body, tier, "daily")["next_reset"])
        monthly = parse_ts(entry(body, tier, "monthly")["next_reset"])
        assert daily in (next_reset("daily", before), next_reset("daily", after)), daily
        assert monthly in (next_reset("monthly", before), next_reset("monthly", after)), monthly
        assert monthly.day == 1 and monthly > after
        assert timedelta(0) < daily - after <= timedelta(days=1)


def test_used_is_spent_plus_reserved(quota, qapi):
    seed_row(quota, "total", "daily", spent=100_000, reserved=50_000)
    seed_row(quota, "tier:premium", "monthly", spent=2_500_000)
    body = status(qapi)

    p = entry(body, "total", "daily")
    limit = LIMITS["total"]["daily"]
    assert p["used_credits_micro"] == 150_000
    assert p["remaining_credits_micro"] == limit - 150_000
    assert p["remaining_percentage"] == (limit - 150_000) * 100 // limit
    p = entry(body, "premium", "monthly")
    assert p["used_credits_micro"] == 2_500_000
    assert p["remaining_percentage"] == 75
    assert entry(body, "total", "monthly")["used_credits_micro"] == 0


def test_the_warning_and_exhausted_flags_follow_the_threshold(quota, qapi):
    daily = LIMITS["total"]["daily"]
    seed_row(quota, "total", "daily", spent=daily * 80 // 100)  # 20 % left <= 100 - 75
    seed_row(quota, "tier:premium", "daily", spent=LIMITS["tier:premium"]["daily"])
    seed_row(quota, "total", "monthly", spent=LIMITS["total"]["monthly"] * 70 // 100)  # 30 % left
    body = status(qapi)

    p = entry(body, "total", "daily")
    assert p["remaining_percentage"] == 20
    assert (p["warning"], p["exhausted"]) == (True, False)
    p = entry(body, "premium", "daily")
    assert p["remaining_percentage"] == 0
    assert (p["warning"], p["exhausted"]) == (True, True)
    p = entry(body, "total", "monthly")
    assert p["remaining_percentage"] == 30
    assert p["warning"] is False


def test_only_the_current_period_rows_count(quota, qapi):
    seed_row(quota, "total", "daily", spent=LIMITS["total"]["daily"], stale=True)
    seed_row(quota, "total", "monthly", spent=LIMITS["total"]["monthly"], stale=True)
    body = status(qapi)
    assert entry(body, "total", "daily")["used_credits_micro"] == 0
    assert entry(body, "total", "monthly")["used_credits_micro"] == 0


def test_the_status_is_scoped_to_tenant_and_user(quota, qapi):
    seed_row(quota, "total", "daily", spent=40_000)
    seed_row(quota, "total", "daily", user="a_reviewer", spent=90_000)
    seed_row(quota, "total", "daily", user="b", spent=70_000)
    assert entry(status(qapi), "total", "daily")["used_credits_micro"] == 40_000
    assert entry(status(qapi, "a_reviewer"), "total", "daily")["used_credits_micro"] == 90_000
    assert entry(status(qapi, "b"), "total", "daily")["used_credits_micro"] == 70_000


def test_reading_the_status_writes_nothing(quota, qapi):
    seed_row(quota, "total", "daily", spent=1_000)
    before = [dict(r) for r in quota.db().quota_rows()]
    for _ in range(3):
        status(qapi)
    assert [dict(r) for r in quota.db().quota_rows()] == before
    assert not quota.mock.requests_to(*RESPONSES)


# --------------------------------------------------------------------------- #
# consistency with the turns that ran
# --------------------------------------------------------------------------- #


def test_the_status_follows_the_turns_that_ran(quota, qapi):
    premium_chat = new_chat(quota)
    standard_chat = new_chat(quota, STANDARD)
    quota.mock.script(*RESPONSES, sse_text(["a"], input_tokens=800, output_tokens=200))
    quota.mock.script(*RESPONSES, sse_text(["a"], input_tokens=100, output_tokens=50))
    send_message(qapi, premium_chat, "hello")
    send_message(qapi, standard_chat, "hello")

    premium = catalog.model_credits(PREMIUM, 800, 200)
    standard = catalog.model_credits(STANDARD, 100, 50)
    body = status(qapi)
    for period in ("daily", "monthly"):
        assert entry(body, "total", period)["used_credits_micro"] == premium + standard, period
        assert entry(body, "premium", period)["used_credits_micro"] == premium, period


def test_an_in_flight_reserve_counts_as_used_until_the_turn_settles(quota, qapi):
    chat = new_chat(quota)
    reserve = catalog.reserve_for(PREMIUM, "hello")["reserved_credits_micro"]
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], pause_after_deltas=1, input_tokens=800,
                                           output_tokens=200))
    live = open_stream(qapi, chat, {"content": "hello"})
    live.expect("stream_started")
    quota.mock.wait_paused()
    during = status(qapi)
    assert entry(during, "total", "daily")["used_credits_micro"] == reserve
    assert entry(during, "premium", "monthly")["used_credits_micro"] == reserve

    quota.mock.release()
    read_all(live)
    after = status(qapi)
    assert entry(after, "total", "daily")["used_credits_micro"] == \
        catalog.model_credits(PREMIUM, 800, 200), "the unused part of the reserve is returned"


def test_done_quota_warnings_agree_with_the_status(quota, qapi):
    seed_row(quota, "total", "daily", spent=LIMITS["total"]["daily"] * 60 // 100)
    chat = new_chat(quota, STANDARD)
    sent = send_message(qapi, chat, "hello")
    warnings = sent.done["quota_warnings"]
    body = status(qapi)
    for w in warnings:
        p = entry(body, w["tier"], w["period"])
        for key in ("remaining_percentage", "warning", "exhausted"):
            assert w[key] == p[key], (w, p)
        if w["warning"] or w["exhausted"]:
            assert parse_ts(w["next_reset"]) == parse_ts(p["next_reset"])
    assert {(w["tier"], w["period"]) for w in warnings} == {
        (t, p) for t in ("total", "premium") for p in ("daily", "monthly")}
