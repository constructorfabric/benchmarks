"""Quota status, reserve/settle accounting, downgrade cascade and enforcement."""

from __future__ import annotations

import datetime as dt
import uuid

import pytest

from conftest import _fresh, assert_problem, ok_stream, ub, wait_until

STD_DAILY, STD_MONTHLY = 100_000_000, 1_000_000_000_000
PREM_DAILY, PREM_MONTHLY = 50_000_000, 500_000_000_000


def _status(api):
    r = api.get("/quota/status")
    assert r.status_code == 200, r.text
    return r.json()


def _period(status, tier, period):
    t = next(t for t in status["tiers"] if t["tier"] == tier)
    return next(p for p in t["periods"] if p["period"] == period)


def test_quota_status_fresh_user(api):
    s = _status(api)
    assert s["warning_threshold_pct"] == 80
    assert {t["tier"] for t in s["tiers"]} == {"premium", "total"}
    now = dt.datetime.now(dt.timezone.utc)
    for tier, daily, monthly in (("total", STD_DAILY, STD_MONTHLY), ("premium", PREM_DAILY, PREM_MONTHLY)):
        d = _period(s, tier, "daily")
        assert d["limit_credits_micro"] == daily
        assert d["used_credits_micro"] == 0
        assert d["remaining_credits_micro"] == daily
        assert d["remaining_percentage"] == 100
        assert d["warning"] is False and d["exhausted"] is False
        assert d["next_reset"].startswith((now + dt.timedelta(days=1)).date().isoformat())
        m = _period(s, tier, "monthly")
        assert m["limit_credits_micro"] == monthly
        first_next = (now.replace(day=1) + dt.timedelta(days=32)).replace(day=1).date().isoformat()
        assert m["next_reset"].startswith(first_next)


def test_quota_status_reflects_usage(api, server):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "hello"))
    s = _status(api)
    cost = 42 * 3 + 7 * 15
    for tier in ("premium", "total"):
        for period in ("daily", "monthly"):
            p = _period(s, tier, period)
            assert p["used_credits_micro"] == cost, (tier, period, p)
            assert p["remaining_credits_micro"] == p["limit_credits_micro"] - cost
    rows = server.query(
        "SELECT bucket, period_type, spent_credits_micro, reserved_credits_micro, calls FROM quota_usage WHERE user_id = ?",
        ub(api.user_id),
    )
    assert {(r["bucket"], r["period_type"]) for r in rows} == {
        ("total", "daily"),
        ("total", "monthly"),
        ("tier:premium", "daily"),
        ("tier:premium", "monthly"),
    }
    for r in rows:
        assert r["spent_credits_micro"] == cost
        assert r["reserved_credits_micro"] == 0
        assert r["calls"] == 1


def test_standard_model_only_charges_total(api, server):
    chat = api.create_chat(model="gpt-standard")
    ok_stream(api.send(chat["id"], "hello"))
    s = _status(api)
    assert _period(s, "total", "daily")["used_credits_micro"] == 42 * 1 + 7 * 2
    assert _period(s, "premium", "daily")["used_credits_micro"] == 0


def test_reserve_is_held_while_running(api, server, mock):
    from test_idempotency import Background, _wait_running

    chat = api.create_chat()
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 3})
    bg = Background(api, chat["id"])
    _wait_running(server, chat["id"])
    rows = server.query("SELECT reserved_credits_micro FROM quota_usage WHERE user_id = ?", ub(api.user_id))
    assert rows and all(r["reserved_credits_micro"] > 0 for r in rows)
    turn = server.query("SELECT reserved_credits_micro FROM chat_turns WHERE chat_id = ?", ub(chat["id"]))[0]
    assert all(r["reserved_credits_micro"] == turn["reserved_credits_micro"] for r in rows)
    used = _period(_status(api), "total", "daily")["used_credits_micro"]
    assert used == turn["reserved_credits_micro"]
    bg.join()
    rows = server.query("SELECT reserved_credits_micro FROM quota_usage WHERE user_id = ?", ub(api.user_id))
    assert all(r["reserved_credits_micro"] == 0 for r in rows)


# ───────────────────────── tight limits (dedicated server) ─────────────────────────


@pytest.fixture
def tight(servers):
    srv = servers("tight_quota")
    return srv, _fresh(srv)


def test_downgrade_when_premium_exhausted(tight, mock):
    srv, api = tight
    chat = api.create_chat()
    assert chat["model"] == "gpt-premium"
    rid = str(uuid.uuid4())
    r = ok_stream(api.send(chat["id"], "hi", request_id=rid))
    done = r.first("done")
    assert done["selected_model"] == "gpt-premium"
    assert done["effective_model"] == "gpt-standard"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == "gpt-premium"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    body = mock.chat_requests(chat["id"])[0]["json"]
    assert body["model"] == "gpt-standard-provider"
    asst = api.messages(chat["id"])[1]
    assert asst["model"] == "gpt-standard"
    row = srv.query("SELECT effective_model FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert row["effective_model"] == "gpt-standard"
    # the chat's model is unchanged
    assert api.get(f"/chats/{chat['id']}").json()["model"] == "gpt-premium"
    ev = wait_until(lambda: srv.usage_events(request_id=rid), msg="usage")[0]
    assert ev["selected_model"] == "gpt-premium" and ev["effective_model"] == "gpt-standard"


def test_quota_exceeded_before_provider_call(tight, mock):
    srv, api = tight
    chat = api.create_chat(model="gpt-standard")
    r = api.send(chat["id"], "x" * 60_000)
    assert_problem(r, 429, category="resource_exhausted", reason="tokens")
    assert mock.chat_requests(chat["id"]) == []
    assert api.messages(chat["id"]) == []
    assert srv.query("SELECT id FROM chat_turns WHERE chat_id = ?", ub(chat["id"])) == []
    rows = srv.query("SELECT reserved_credits_micro, spent_credits_micro FROM quota_usage WHERE user_id = ?", ub(api.user_id))
    assert all(r["reserved_credits_micro"] == 0 and r["spent_credits_micro"] == 0 for r in rows)


def test_quota_warnings_and_exhaustion(tight, mock):
    srv, api = tight
    # separate chats: the previous answer's usage does not inflate the next reserve
    chat0 = api.create_chat(model="gpt-standard")
    chat = api.create_chat(model="gpt-standard")
    big = {"text": "big", "usage": {"input_tokens": 60_000, "output_tokens": 0}}
    mock.script(big, big)
    r1 = ok_stream(api.send(chat0["id"], "one"))
    w1 = {(w["tier"], w["period"]): w for w in r1.first("done")["quota_warnings"]}
    assert w1[("total", "daily")]["warning"] is False
    r2 = ok_stream(api.send(chat["id"], "two"))
    w2 = {(w["tier"], w["period"]): w for w in r2.first("done")["quota_warnings"]}
    daily = w2[("total", "daily")]
    assert daily["warning"] is True
    assert daily["remaining_percentage"] <= 20
    assert daily["next_reset"]
    s = _status(api)
    p = _period(s, "total", "daily")
    assert p["warning"] is True
    assert p["used_credits_micro"] > 0.8 * 20_000
    # overshoot beyond tolerance is capped at the reserve: spent equals two reserves
    turns = srv.query(
        "SELECT reserved_credits_micro FROM chat_turns WHERE chat_id IN (?, ?)", ub(chat0["id"]), ub(chat["id"])
    )
    assert len(turns) == 2
    assert p["used_credits_micro"] == sum(t["reserved_credits_micro"] for t in turns)
    # the next turn no longer fits any tier
    r3 = api.send(chat["id"], "three")
    assert_problem(r3, 429, reason="tokens")


def test_retry_rejected_by_quota_changes_nothing(tight, mock):
    srv, api = tight
    chat0 = api.create_chat(model="gpt-standard")
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    big = {"text": "big", "usage": {"input_tokens": 60_000, "output_tokens": 0}}
    mock.script(big, big)
    ok_stream(api.send(chat0["id"], "one"))
    ok_stream(api.send(chat["id"], "two", request_id=rid))
    r = api.retry(chat["id"], rid)
    assert_problem(r, 429, reason="tokens")
    assert api.turn(chat["id"], rid).json()["state"] == "done"
    assert len(api.messages(chat["id"])) == 2
