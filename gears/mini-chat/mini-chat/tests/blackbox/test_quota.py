"""US5 black-box: quota status, reserve/settlement in ``quota_usage``, downgrade and 429.

Limits (static policy plugin): premium 600_000 credits-micro/day, total 700_000/day.
Reserve of ``gpt-4.1`` (premium, out multiplier 15x) is ~491_600 credits-micro, so one
premium turn fits; ``gpt-4.1-mini`` (standard, 3x) reserves ~98_400 + prior context.
"""

import uuid

from conftest import TENANT_A, TENANT_B, USER_A, USER_A2, USER_B, ub, wait_until

PREMIUM_DAILY = 600_000
TOTAL_DAILY = 700_000
MONTHLY = 1_000_000_000

POLICY_PATCH = {
    "default_premium_limits": {"limit_daily_credits_micro": PREMIUM_DAILY, "limit_monthly_credits_micro": MONTHLY},
    "default_standard_limits": {"limit_daily_credits_micro": TOTAL_DAILY, "limit_monthly_credits_micro": MONTHLY},
}

IN_PREMIUM, OUT_PREMIUM = 3, 15  # credits-micro per token (multiplier_micro / 1e6)
IN_STD, OUT_STD = 1, 3


def _settled(db, user):
    def check():
        rows = db.quota_usage(user_id=user)
        if rows and all(r["reserved_credits_micro"] == 0 for r in rows.values()):
            return rows
        return None

    return wait_until(check, desc="settled quota rows")


def _spent(rows, bucket, period="daily"):
    r = rows.get((bucket, period))
    return 0 if r is None else r["spent_credits_micro"]


def _period(status, tier, period):
    t = next(t for t in status["tiers"] if t["tier"] == tier)
    return next(p for p in t["periods"] if p["period"] == period)


def test_quota_status_initial(api_b):
    # tenant-B user: untouched quota
    st = api_b.quota()
    assert st["warning_threshold_pct"] == 80
    assert [t["tier"] for t in st["tiers"]] == ["premium", "total"]
    for tier, limit in (("premium", PREMIUM_DAILY), ("total", TOTAL_DAILY)):
        d = _period(st, tier, "daily")
        assert d["limit_credits_micro"] == limit
        assert d["used_credits_micro"] == 0
        assert d["remaining_credits_micro"] == limit
        assert d["remaining_percentage"] == 100
        assert d["warning"] is False and d["exhausted"] is False
        assert d["next_reset"].endswith("T00:00:00Z")
        assert _period(st, tier, "monthly")["limit_credits_micro"] == MONTHLY


def test_reserve_then_actual_settlement(api, mock, db):
    c = api.create_chat()  # gpt-4.1, premium
    mock.push({"text": "slow", "delay_before": 1.5, "usage": {"input_tokens": 20, "output_tokens": 10}})
    bg = api.send_in_background(c["id"], "hi")

    # while running, the reserve is held in both premium and total buckets
    def reserved():
        rows = db.quota_usage()
        return rows if rows and all(r["reserved_credits_micro"] > 0 for r in rows.values()) else None

    rows = wait_until(reserved, desc="reserve booked")
    turn = [t for t in db.turns(c["id"]) if t["state"] == "running"][0]
    assert set(rows) == {("total", "daily"), ("total", "monthly"), ("tier:premium", "daily"), ("tier:premium", "monthly")}
    for r in rows.values():
        assert r["reserved_credits_micro"] == turn["reserved_credits_micro"]
    assert turn["max_output_tokens_applied"] == 32768
    assert turn["reserve_tokens"] > 32768
    assert turn["reserved_credits_micro"] == (turn["reserve_tokens"] - 32768) * IN_PREMIUM + 32768 * OUT_PREMIUM
    st = api.quota()
    assert _period(st, "premium", "daily")["used_credits_micro"] == turn["reserved_credits_micro"]

    res = bg.join()
    assert res.names[-1] == "done"
    rows = _settled(db, USER_A)
    expected = 20 * IN_PREMIUM + 10 * OUT_PREMIUM  # 210
    for key in rows:
        assert rows[key]["spent_credits_micro"] == expected, key
        assert rows[key]["calls"] == 1
        # token telemetry is kept on the `total` bucket only (DESIGN quota_usage)
        tokens = (20, 10) if key[0] == "total" else (0, 0)
        assert (rows[key]["input_tokens"], rows[key]["output_tokens"]) == tokens, key
        assert rows[key]["tenant_id"] == ub(TENANT_A) and rows[key]["user_id"] == ub(USER_A)

    st = api.quota()
    for tier in ("premium", "total"):
        d = _period(st, tier, "daily")
        assert d["used_credits_micro"] == expected
        assert d["remaining_credits_micro"] == d["limit_credits_micro"] - expected
    # done.quota_warnings mirror the status endpoint (no next_reset unless warning/exhausted)
    for w in res.done["quota_warnings"]:
        assert "next_reset" not in w
        assert w["warning"] is False


def test_replay_does_not_touch_quota(api, mock, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    api.send(c["id"], "hi", request_id=rid)
    before = _settled(db, USER_A)
    replay = api.send(c["id"], "hi", request_id=rid)
    assert replay.started["is_new_turn"] is False
    assert db.quota_usage() == before


def test_cancelled_turn_settles_estimated(api, mock, db):
    c = api.create_chat()
    before = _settled(db, USER_A)
    rid = str(uuid.uuid4())
    mock.push({"chunks": ["a ", "b ", "c"], "delay": 0.8})
    api.open_and_drop(c["id"], "cancel me", lambda ev: any(n == "delta" for n, _ in ev), request_id=rid)
    wait_until(lambda: db.turn(c["id"], rid)["state"] == "cancelled", desc="cancelled")
    after = _settled(db, USER_A)
    t = db.turn(c["id"], rid)
    est_input = t["reserve_tokens"] - t["max_output_tokens_applied"]
    expected = est_input * IN_PREMIUM + t["minimal_generation_floor_applied"] * OUT_PREMIUM
    assert t["minimal_generation_floor_applied"] == 50
    for key in after:
        assert after[key]["spent_credits_micro"] - _spent(before, key[0], key[1]) == expected, key
        # estimated settlement records no provider token telemetry
        assert after[key]["input_tokens"] == before[key]["input_tokens"]


def test_downgrade_then_quota_exceeded(api, api_a2, mock, db):
    c = api_a2.create_chat()  # gpt-4.1 (premium)
    # 1) premium turn that overshoots the reserve beyond tolerance -> charged = reserve
    mock.push({"usage": {"input_tokens": 10, "output_tokens": 40000}})
    r1 = api_a2.send(c["id"], "first")
    assert r1.done["quota_decision"] == "allow"
    assert r1.done["effective_model"] == "gpt-4.1"
    rows = _settled(db, USER_A2)
    t1 = db.turn(c["id"], r1.request_id)
    assert t1["state"] == "completed"  # overshoot never changes the outcome
    assert _spent(rows, "tier:premium") == t1["reserved_credits_micro"]
    assert _spent(rows, "total") == t1["reserved_credits_micro"]
    st = api_a2.quota()
    prem = _period(st, "premium", "daily")
    assert prem["warning"] is True and prem["exhausted"] is False  # < 20% remaining
    warn = [w for w in r1.done["quota_warnings"] if w["tier"] == "premium" and w["period"] == "daily"][0]
    assert warn["warning"] is True and "next_reset" in warn

    # 2) premium no longer fits -> downgrade to the standard default model
    mock.push({"usage": {"input_tokens": 10, "output_tokens": 100000}})
    r2 = api_a2.send(c["id"], "second")
    assert r2.status == 200, r2
    done = r2.done
    assert done["quota_decision"] == "downgrade"
    assert done["selected_model"] == "gpt-4.1"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert done["downgrade_from"] == "gpt-4.1"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    assert mock.chat_requests()[-1]["json"]["model"] == "gpt-4.1-mini"
    assert api_a2.messages(c["id"])[-1]["model"] == "gpt-4.1-mini"
    assert api_a2.get(f"/chats/{c['id']}").json()["model"] == "gpt-4.1"  # chat model immutable
    rows2 = _settled(db, USER_A2)
    t2 = db.turn(c["id"], r2.request_id)
    assert t2["effective_model"] == "gpt-4.1-mini"
    assert _spent(rows2, "tier:premium") == _spent(rows, "tier:premium")  # standard turn
    assert _spent(rows2, "total") == _spent(rows, "total") + t2["reserved_credits_micro"]
    # replay rebuilds the downgrade decision without the reason
    rp = api_a2.send(c["id"], "second", request_id=r2.request_id)
    assert rp.done["quota_decision"] == "downgrade" and rp.done["downgrade_from"] == "gpt-4.1"
    assert "downgrade_reason" not in rp.done

    # 3) nothing fits anymore -> 429 quota_exceeded, no provider call, nothing persisted
    calls = len(mock.chat_requests())
    turns_before = db.turns(c["id"])
    msgs_before = db.messages(c["id"])
    r3 = api_a2.send(c["id"], "third")
    assert r3.status == 429, r3
    p = r3.problem
    assert p["type"].endswith("cf.core.err.resource_exhausted.v1~")
    v = p["context"]["violations"][0]
    assert v["subject"] == "tokens" and v["description"] == "quota_exceeded"
    assert len(mock.chat_requests()) == calls
    assert db.turns(c["id"]) == turns_before
    assert db.messages(c["id"]) == msgs_before
    assert db.quota_usage(user_id=USER_A2) == rows2

    # 4) retry of the last turn is rejected by the preflight and leaves it in place
    rr = api_a2.retry(c["id"], r2.request_id)
    assert rr.status == 429, rr
    assert db.turn(c["id"], r2.request_id)["deleted_at"] is None
    assert db.turns(c["id"]) == turns_before
    assert len(mock.chat_requests()) == calls

    assert _period(api_a2.quota(), "total", "daily")["used_credits_micro"] == _spent(rows2, "total")

    # 5) another user of the same tenant is unaffected (per-user quota)
    other = api.create_chat()
    ok = api.send(other["id"], "still fine")
    assert ok.status == 200 and ok.done["quota_decision"] == "allow"


def test_other_users_quota_is_independent(api, api_b, mock, db):
    c = api_b.create_chat()
    res = api_b.send(c["id"], "hello from tenant b")
    assert res.done["quota_decision"] == "allow"
    st = api_b.quota()
    assert _period(st, "premium", "daily")["used_credits_micro"] == 210
    assert set(db.quota_usage(user_id=USER_B, tenant_id=TENANT_B)) == {
        ("total", "daily"), ("total", "monthly"), ("tier:premium", "daily"), ("tier:premium", "monthly")
    }
