"""Quota status endpoint (DESIGN section 3.2, "Quota Status Endpoint")."""

import datetime as dt
import uuid
from contextlib import closing

import pytest

from ._seed import chat_tenant
from . import mock_provider as mp
from .helpers import (CURRENT_PERIODS, PREFIX, TOKEN_A_REVIEWER, api, create_chat, db, owner_quota_rows, stream,
                      uuid_bytes)

pytestmark = pytest.mark.usefixtures("server")

URL = f"{PREFIX}/quota/status"

# static_model_policy defaults
LIMITS = {
    ("premium", "daily"): 50_000_000,
    ("premium", "monthly"): 500_000_000,
    ("total", "daily"): 100_000_000,
    ("total", "monthly"): 1_000_000_000,
}


def _parse(ts: str) -> dt.datetime:
    return dt.datetime.fromisoformat(ts.replace("Z", "+00:00"))


def _period(body, tier, period):
    t = next(t for t in body["tiers"] if t["tier"] == tier)
    return next(p for p in t["periods"] if p["period"] == period)


def _user_id(session) -> str:
    """The caller's user id, read from a chat it creates."""
    chat = create_chat(session)
    with closing(db()) as conn:
        row = conn.execute("SELECT user_id FROM chats WHERE id = ?", (uuid_bytes(chat["id"]),)).fetchone()
    return str(uuid.UUID(bytes=row["user_id"])), chat["id"]


def test_requires_authentication():
    assert api(None).get(URL).status_code == 401


def test_shape_two_tiers_two_periods():
    r = api(TOKEN_A_REVIEWER).get(URL)
    assert r.status_code == 200, r.text
    body = r.json()
    assert set(body) == {"tiers", "warning_threshold_pct"}
    assert body["warning_threshold_pct"] == 80
    assert [t["tier"] for t in body["tiers"]] == ["premium", "total"]

    now = dt.datetime.now(dt.timezone.utc)
    tomorrow = (now + dt.timedelta(days=1)).date()
    first_next_month = (now.replace(day=1) + dt.timedelta(days=32)).date().replace(day=1)
    for tier in body["tiers"]:
        assert set(tier) == {"tier", "periods"}
        assert [p["period"] for p in tier["periods"]] == ["daily", "monthly"]
        for p in tier["periods"]:
            assert set(p) == {"period", "limit_credits_micro", "used_credits_micro",
                              "remaining_credits_micro", "remaining_percentage", "next_reset",
                              "warning", "exhausted"}
            assert p["limit_credits_micro"] == LIMITS[(tier["tier"], p["period"])]
            assert p["used_credits_micro"] + p["remaining_credits_micro"] <= p["limit_credits_micro"]
            assert 0 <= p["remaining_percentage"] <= 100
            reset = _parse(p["next_reset"])
            assert reset.tzinfo is not None and reset.utcoffset() == dt.timedelta(0)
            assert (reset.hour, reset.minute, reset.second) == (0, 0, 0)
            expected = tomorrow if p["period"] == "daily" else first_next_month
            assert reset.date() == expected


def _bump(tenant: bytes, user_id: str, day: str, spent: int, reserved: int) -> None:
    """Adds to the caller's daily `tier:premium` row (created when missing)."""
    with closing(db()) as conn:
        conn.execute(
            "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket,"
            " spent_credits_micro, reserved_credits_micro) VALUES (?, ?, ?, 'daily', ?, 'tier:premium', ?, ?)"
            " ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET"
            " spent_credits_micro = spent_credits_micro + excluded.spent_credits_micro,"
            " reserved_credits_micro = reserved_credits_micro + excluded.reserved_credits_micro",
            (uuid_bytes(str(uuid.uuid4())), tenant, uuid_bytes(user_id), day, spent, reserved),
        )
        conn.commit()


def test_used_credits_reflect_seeded_usage():
    s = api()
    user_id, chat_id = _user_id(s)
    tenant = chat_tenant(chat_id)
    today = dt.datetime.now(dt.timezone.utc).date().isoformat()
    before = _period(s.get(URL).json(), "premium", "daily")["used_credits_micro"]
    other_before = api(TOKEN_A_REVIEWER).get(URL).json()

    _bump(tenant, user_id, today, 30_000_000, 10_000_000)
    try:
        body = s.get(URL).json()
        premium_daily = _period(body, "premium", "daily")
        limit = LIMITS[("premium", "daily")]
        used = before + 40_000_000
        remaining = max(0, limit - used)
        pct = remaining * 100 // limit
        assert premium_daily["used_credits_micro"] == used
        assert premium_daily["remaining_credits_micro"] == remaining
        assert premium_daily["remaining_percentage"] == pct
        assert premium_daily["warning"] is (pct <= 20)
        assert premium_daily["exhausted"] is (pct == 0)
        if before == 0:
            assert (pct, premium_daily["warning"], premium_daily["exhausted"]) == (20, True, False)

        # Another user's status does not include the seed.
        assert api(TOKEN_A_REVIEWER).get(URL).json() == other_before
    finally:
        with closing(db()) as conn:
            conn.execute(
                "UPDATE quota_usage SET spent_credits_micro = spent_credits_micro - 30000000,"
                " reserved_credits_micro = reserved_credits_micro - 10000000"
                " WHERE tenant_id = ? AND user_id = ? AND period_type = 'daily' AND period_start = ?"
                " AND bucket = 'tier:premium'",
                (tenant, uuid_bytes(user_id), today),
            )
            conn.commit()


def _spent(chat_id: str) -> dict:
    rows = owner_quota_rows(chat_id, "period_type, bucket, SUM(spent_credits_micro) AS s,"
                            " SUM(reserved_credits_micro) AS r", CURRENT_PERIODS, group_by="period_type, bucket")
    return {(r["period_type"], r["bucket"]): (r["s"], r["r"]) for r in rows}


@pytest.mark.parametrize("model,credits,premium", [
    # usage 1000 in / 200 out; ceil per component of tokens * multiplier / 1e6
    ("gpt-premium", 1000 * 3 + 200 * 15, True),   # premium: 3x / 15x
    ("gpt-standard", 1000 * 1 + 200 * 3, False),  # standard: 1x / 3x
])
def test_settlement_charges_actual_credits_per_model_and_tier(reset_mock, model, credits, premium):
    s = api()
    chat = create_chat(s, model=model)
    before = _spent(chat["id"])
    status_before = s.get(URL).json()
    reset_mock.enqueue("responses", {"events": mp.text_events("ok", usage={"input_tokens": 1000,
                                                                           "output_tokens": 200})})
    assert stream(s, chat["id"], {"content": "count me"}).terminal[0] == "done"
    after = _spent(chat["id"])

    def delta(key):
        return after.get(key, (0, 0))[0] - before.get(key, (0, 0))[0]

    for period in ("daily", "monthly"):
        assert delta((period, "total")) == credits, (period, before, after)
        assert delta((period, "tier:premium")) == (credits if premium else 0), (period, before, after)
        assert after[(period, "total")][1] == before.get((period, "total"), (0, 0))[1], "reserve released"
    # The status endpoint reports exactly the settled usage.
    status_after = s.get(URL).json()
    assert (_period(status_after, "total", "daily")["used_credits_micro"]
            - _period(status_before, "total", "daily")["used_credits_micro"]) == credits
    assert (_period(status_after, "premium", "daily")["used_credits_micro"]
            - _period(status_before, "premium", "daily")["used_credits_micro"]) == (credits if premium else 0)
