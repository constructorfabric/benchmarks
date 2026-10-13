"""Quota status & enforcement (acceptance: Quota Status API; Quota Enforcement;
Principles: quota checked before any outbound provider call)."""

import datetime as dt

import pytest

from mchelpers import (
    PREM_LIMIT_DAILY,
    PREM_LIMIT_MONTHLY,
    STD_LIMIT_DAILY,
    STD_LIMIT_MONTHLY,
    BackgroundStream,
    assert_problem,
    credits_micro,
    estimated_text_tokens,
    find_tool,
    nonce,
    parse_ts,
    utc_today,
)

PERIOD_KEYS = {"period", "limit_credits_micro", "used_credits_micro", "remaining_credits_micro", "remaining_percentage", "next_reset", "warning", "exhausted"}


def _expected_resets():
    today = utc_today()
    tomorrow = dt.datetime.combine(today + dt.timedelta(days=1), dt.time(0), tzinfo=dt.timezone.utc)
    first = today.replace(day=1)
    nxt = (first.replace(year=first.year + 1, month=1) if first.month == 12 else first.replace(month=first.month + 1))
    return {"daily": tomorrow, "monthly": dt.datetime.combine(nxt, dt.time(0), tzinfo=dt.timezone.utc)}


def _period(status, tier, period):
    for t in status["tiers"]:
        if t["tier"] == tier:
            for p in t["periods"]:
                if p["period"] == period:
                    return p
    raise AssertionError(f"{tier}/{period} not in {status}")


def _ensure_rows(api, model="prem"):
    """Create quota_usage rows for the user with one cheap turn."""
    c = api.create_chat(model=model)
    assert api.stream(c["id"], "warm up " + nonce()).done
    return c


# Acceptance: Quota Status API — shape, limits and next_reset for a fresh user
def test_quota_status_fresh_user(api_for):
    a = api_for("tok-q1")
    st = a.quota()
    assert st["warning_threshold_pct"] == 80
    tiers = {t["tier"] for t in st["tiers"]}
    assert tiers == {"total", "premium"}
    limits = {
        ("total", "daily"): STD_LIMIT_DAILY,
        ("total", "monthly"): STD_LIMIT_MONTHLY,
        ("premium", "daily"): PREM_LIMIT_DAILY,
        ("premium", "monthly"): PREM_LIMIT_MONTHLY,
    }
    resets = _expected_resets()
    for (tier, period), limit in limits.items():
        p = _period(st, tier, period)
        assert set(p) >= PERIOD_KEYS
        assert p["limit_credits_micro"] == limit
        assert p["used_credits_micro"] == 0
        assert p["remaining_credits_micro"] == limit
        assert p["remaining_percentage"] == 100
        assert p["warning"] is False and p["exhausted"] is False
        assert parse_ts(p["next_reset"]) == resets[period]


# Acceptance: Quota Status API — consistent with actual usage (standard and premium turns)
def test_quota_status_tracks_usage(api_for, db):
    a = api_for("tok-q1")
    before = a.quota()
    c = a.create_chat(model="std")
    s = a.stream(c["id"], "std usage [[usage:100:50]] " + nonce())
    assert s.done
    after = a.quota()
    std_cost = credits_micro(100, 50, "std")
    assert std_cost == 250
    for period in ("daily", "monthly"):
        assert _period(after, "total", period)["used_credits_micro"] == _period(before, "total", period)["used_credits_micro"] + std_cost
        assert _period(after, "premium", period)["used_credits_micro"] == _period(before, "premium", period)["used_credits_micro"]
    c2 = a.create_chat(model="prem")
    assert a.stream(c2["id"], "prem usage [[usage:100:50]] " + nonce()).done
    final = a.quota()
    prem_cost = credits_micro(100, 50, "prem")
    assert prem_cost == 1050
    for period in ("daily", "monthly"):
        assert _period(final, "total", period)["used_credits_micro"] == _period(after, "total", period)["used_credits_micro"] + prem_cost
        assert _period(final, "premium", period)["used_credits_micro"] == _period(after, "premium", period)["used_credits_micro"] + prem_cost
    # consistent with the DB (used = spent + reserved)
    snap = db.quota_snapshot("tok-q1")
    for tier, bucket in (("total", "total"), ("premium", "tier:premium")):
        for period in ("daily", "monthly"):
            row = snap[(bucket, period)]
            p = _period(final, tier, period)
            assert p["used_credits_micro"] == row["spent_credits_micro"] + row["reserved_credits_micro"]
            assert p["remaining_credits_micro"] == p["limit_credits_micro"] - p["used_credits_micro"]


# Acceptance: Quota Status API — warning and exhausted flags (floored percentage)
def test_quota_status_warning_and_exhausted(api_for, db):
    a = api_for("tok-q2")
    _ensure_rows(a, model="std")
    db.seed_quota("tok-q2", "total", "daily", spent_credits_micro=85_000_000, reserved_credits_micro=0)
    p = a.quota_period("total", "daily")
    assert p["used_credits_micro"] == 85_000_000
    assert p["remaining_percentage"] == 15
    assert p["warning"] is True and p["exhausted"] is False
    db.seed_quota("tok-q2", "total", "daily", spent_credits_micro=99_500_000)
    p = a.quota_period("total", "daily")
    assert p["remaining_percentage"] == 0
    assert p["exhausted"] is True and p["warning"] is True
    m = a.quota_period("total", "monthly")
    assert m["exhausted"] is False


# Acceptance: Quota Enforcement / Principles — all tiers exhausted: 429 tokens before any provider call
def test_all_tiers_exhausted(api_for, db, mock_llm):
    a = api_for("tok-q3")
    _ensure_rows(a)
    db.seed_quota("tok-q3", "total", "daily", spent_credits_micro=STD_LIMIT_DAILY)
    try:
        c = a.create_chat()
        n = nonce()
        s = a.stream(c["id"], f"over quota {n}")
        assert not s.is_sse
        body = assert_problem(a.as_response(s), 429, "resource_exhausted", violation_subject="tokens")
        assert body["context"]["violations"][0]["subject"] == "tokens"
        assert body["context"]["violations"][0]["description"] == "quota_exceeded"
        assert mock_llm.chat_requests(contains=n) == []
        assert db.turns(c["id"]) == []
        assert a.messages(c["id"]) == []
    finally:
        db.seed_quota("tok-q3", "total", "daily", spent_credits_micro=0)


# Acceptance: Quota Enforcement — every enabled period must pass (monthly exhaustion blocks too)
def test_monthly_exhaustion_blocks(api_for, db, mock_llm):
    a = api_for("tok-q3")
    _ensure_rows(a, model="std")
    db.seed_quota("tok-q3", "total", "monthly", spent_credits_micro=STD_LIMIT_MONTHLY)
    try:
        c = a.create_chat(model="std")
        n = nonce()
        s = a.stream(c["id"], f"monthly {n}")
        assert_problem(a.as_response(s), 429, "resource_exhausted", violation_subject="tokens")
        assert mock_llm.chat_requests(contains=n) == []
    finally:
        db.seed_quota("tok-q3", "total", "monthly", spent_credits_micro=0)


# Acceptance: Quota Enforcement — tier downgrade when the premium bucket is exhausted
def test_premium_exhausted_downgrades(api_for, db, mock_llm):
    a = api_for("tok-q4")
    _ensure_rows(a, model="prem")
    db.seed_quota("tok-q4", "tier:premium", "daily", spent_credits_micro=PREM_LIMIT_DAILY)
    prem_before = db.quota_snapshot("tok-q4")[("tier:premium", "daily")]
    total_before = db.quota_snapshot("tok-q4")[("total", "daily")]
    c = a.create_chat(model="prem")
    n = nonce()
    s = a.stream(c["id"], f"downgrade me {n}")
    d = s.done
    assert d["quota_decision"] == "downgrade"
    assert d["downgrade_reason"] == "premium_quota_exhausted"
    assert d["downgrade_from"] == "prem"
    assert d["selected_model"] == "prem"
    assert d["effective_model"] == "std"
    assert a.get("/v1/models/" + d["effective_model"]).json()["tier"] == "standard"
    assert mock_llm.chat_requests(contains=n)[-1]["json"]["model"] == "mock-std"
    asst = a.messages(c["id"])[-1]
    assert asst["model"] == "std"
    assert db.turn_row(c["id"], s.request_id)["effective_model"] == "std"
    # premium bucket untouched, total charged at standard multipliers
    snap = db.quota_snapshot("tok-q4")
    assert snap[("tier:premium", "daily")]["spent_credits_micro"] == prem_before["spent_credits_micro"]
    assert snap[("total", "daily")]["spent_credits_micro"] == total_before["spent_credits_micro"] + credits_micro(10, 5, "std")
    # chat model unchanged (immutable)
    assert a.chat(c["id"])["model"] == "prem"
    # replay rebuilds quota_decision / downgrade_from, omits downgrade_reason
    r = a.stream(c["id"], "replay", request_id=s.request_id)
    rd = r.done
    assert r.started["is_new_turn"] is False
    assert rd["quota_decision"] == "downgrade" and rd["downgrade_from"] == "prem"
    assert rd["effective_model"] == "std" and rd["selected_model"] == "prem"
    assert rd.get("downgrade_reason") is None
    # a standard chat is not affected
    c2 = a.create_chat(model="std")
    d2 = a.stream(c2["id"], "std still fine " + nonce()).done
    assert d2["quota_decision"] == "allow" and d2["effective_model"] == "std"
    db.seed_quota("tok-q4", "tier:premium", "daily", spent_credits_micro=0)


# Acceptance: Quota Enforcement — force_standard_tier kill switch downgrades premium chats
def test_force_standard_tier_downgrade(ks_api_for):
    a = ks_api_for("tok-q4")
    c = a.create_chat()
    assert c["model"] == "prem"
    d = a.stream(c["id"], "forced " + nonce()).done
    assert d["quota_decision"] == "downgrade"
    assert d["downgrade_reason"] == "force_standard_tier"
    assert d["downgrade_from"] == "prem"
    assert d["effective_model"] == "std"


# Acceptance: Quota Enforcement / Web Search — daily web search quota is checked only when the tool is sent
def test_web_search_daily_quota(api_for, db, mock_llm):
    a = api_for("tok-q5")
    _ensure_rows(a)
    db.seed_quota("tok-q5", "total", "daily", web_search_calls=75)
    try:
        c = a.create_chat()
        n = nonce()
        s = a.stream(c["id"], f"search {n}", web_search=True)
        body = assert_problem(a.as_response(s), 429, "resource_exhausted", violation_subject="web_search")
        assert body["context"]["violations"][0]["description"] == "quota_exceeded"
        assert mock_llm.chat_requests(contains=n) == []
        # without the tool the same user is not rejected
        assert a.stream(c["id"], f"no search {n}").done
        # a model without web search support skips the tool and the quota check
        nv = a.create_chat(model="std-novision")
        m = nonce()
        assert a.stream(nv["id"], f"search on novision {m}", web_search=True).done
        assert find_tool(mock_llm.chat_requests(contains=m)[-1], "web_search") is None
    finally:
        db.seed_quota("tok-q5", "total", "daily", web_search_calls=0)


# Acceptance: Quota Enforcement — reserve before execute (formula, persisted fields, bucket increments)
@pytest.mark.timeout(60)
def test_reserve_before_execute(api_for, db):
    a = api_for("tok-q6")
    _ensure_rows(a, model="std")
    c = a.create_chat(model="std")  # fresh chat: prior_context_tokens = 0, no tools
    content = "reserve check [[hang]] " + nonce()
    base = db.quota_snapshot("tok-q6")
    bg = BackgroundStream.send(a, c["id"], content).wait_started()
    try:
        row = db.turn_row(c["id"], bg.request_id)
        est = estimated_text_tokens(content)
        assert row["max_output_tokens_applied"] == 4096
        assert row["reserve_tokens"] == est + 4096
        assert row["reserved_credits_micro"] == credits_micro(est, 4096, "std")
        assert row["effective_model"] == "std"
        assert row["policy_version_applied"] is not None
        assert row["minimal_generation_floor_applied"] == 50
        snap = db.quota_snapshot("tok-q6")
        for period in ("daily", "monthly"):
            assert snap[("total", period)]["reserved_credits_micro"] == base[("total", period)]["reserved_credits_micro"] + row["reserved_credits_micro"]
        assert ("tier:premium", "daily") not in snap or snap[("tier:premium", "daily")] == base.get(("tier:premium", "daily"))
        st = a.quota()
        p = _period(st, "total", "daily")
        assert p["used_credits_micro"] == snap[("total", "daily")]["spent_credits_micro"] + snap[("total", "daily")]["reserved_credits_micro"]
    finally:
        bg.stop()
    a.wait_turn_state(c["id"], bg.request_id, {"cancelled"}, timeout=30)


# Acceptance: Quota Enforcement — premium turns reserve on both total and tier:premium buckets
@pytest.mark.timeout(60)
def test_premium_reserve_on_both_buckets(api_for, db):
    a = api_for("tok-q6")
    _ensure_rows(a, model="prem")
    c = a.create_chat(model="prem")
    base = db.quota_snapshot("tok-q6")
    content = "premium reserve [[hang]] " + nonce()
    bg = BackgroundStream.send(a, c["id"], content).wait_started()
    try:
        row = db.turn_row(c["id"], bg.request_id)
        est = estimated_text_tokens(content)
        assert row["reserved_credits_micro"] == credits_micro(est, 4096, "prem")
        snap = db.quota_snapshot("tok-q6")
        for bucket in ("total", "tier:premium"):
            for period in ("daily", "monthly"):
                assert snap[(bucket, period)]["reserved_credits_micro"] == base[(bucket, period)]["reserved_credits_micro"] + row["reserved_credits_micro"]
    finally:
        bg.stop()
    a.wait_turn_state(c["id"], bg.request_id, {"cancelled"}, timeout=30)
