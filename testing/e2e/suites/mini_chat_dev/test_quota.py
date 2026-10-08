"""Quota: status, reserve/settle accounting, tier downgrade, 429 (seeded quota_usage rows)."""

from __future__ import annotations

import datetime as dt
import threading
import uuid

import pytest

from .conftest import USERS, assert_problem, quota_rows, ub, wait_until
from .mock_llm import DEFAULT_USAGE, held_stream

PREMIUM_DAILY = 50_000_000
PREMIUM_MONTHLY = 500_000_000
TOTAL_DAILY = 100_000_000
TOTAL_MONTHLY = 1_000_000_000
QUOTA_USER = "C"  # quota scenarios run as user C (tenant T2) and reset its rows


def now_ts() -> str:
    t = dt.datetime.now(dt.timezone.utc)
    return t.strftime("%Y-%m-%dT%H:%M:%S.") + f"{t.microsecond * 1000:09d}Z"


def period_starts() -> dict[str, str]:
    today = dt.datetime.now(dt.timezone.utc).date()
    return {"daily": today.isoformat(), "monthly": today.replace(day=1).isoformat()}


def clear_quota(db, user: str) -> None:
    u = USERS[user]
    db.execute("DELETE FROM quota_usage WHERE tenant_id = ? AND user_id = ?", (ub(u["tenant"]), ub(u["id"])))


def seed(db, user: str, period: str, bucket: str, spent: int = 0, reserved: int = 0, **counters: int) -> None:
    u = USERS[user]
    cols = ["id", "tenant_id", "user_id", "period_type", "period_start", "bucket", "spent_credits_micro",
            "reserved_credits_micro", "updated_at", *counters.keys()]
    vals = [uuid.uuid4().bytes, ub(u["tenant"]), ub(u["id"]), period, period_starts()[period], bucket, spent,
            reserved, now_ts(), *counters.values()]
    db.execute(
        f"INSERT INTO quota_usage ({', '.join(cols)}) VALUES ({', '.join('?' * len(cols))}) "
        "ON CONFLICT (tenant_id, user_id, period_type, period_start, bucket) DO UPDATE SET "
        "spent_credits_micro = excluded.spent_credits_micro, reserved_credits_micro = excluded.reserved_credits_micro"
        + "".join(f", {k} = excluded.{k}" for k in counters),
        vals,
    )


@pytest.fixture
def quota_user(db):
    clear_quota(db, QUOTA_USER)
    yield QUOTA_USER
    clear_quota(db, QUOTA_USER)


def status_map(body: dict) -> dict[tuple[str, str], dict]:
    return {(t["tier"], p["period"]): p for t in body["tiers"] for p in t["periods"]}


def test_quota_status_shape_for_fresh_user(api, quota_user):
    r = api(quota_user).get("/quota/status")
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["warning_threshold_pct"] == 80
    st = status_map(body)
    assert set(st) == {("premium", "daily"), ("premium", "monthly"), ("total", "daily"), ("total", "monthly")}
    limits = {("premium", "daily"): PREMIUM_DAILY, ("premium", "monthly"): PREMIUM_MONTHLY,
              ("total", "daily"): TOTAL_DAILY, ("total", "monthly"): TOTAL_MONTHLY}
    today = dt.datetime.now(dt.timezone.utc).date()
    for key, p in st.items():
        assert p["limit_credits_micro"] == limits[key]
        assert p["used_credits_micro"] == 0
        assert p["remaining_credits_micro"] == limits[key]
        assert p["remaining_percentage"] == 100
        assert p["warning"] is False and p["exhausted"] is False
    assert st[("total", "daily")]["next_reset"] == f"{today + dt.timedelta(days=1)}T00:00:00Z"
    nm = (today.replace(day=1) + dt.timedelta(days=32)).replace(day=1)
    assert st[("total", "monthly")]["next_reset"] == f"{nm}T00:00:00Z"


def test_turn_settles_actual_usage_and_status_matches(api, db, quota_user):
    c = api(quota_user)
    chat = c.create_chat()  # premium gpt-4.1: input x3, output x15
    c.send(chat["id"], "count my credits")
    expected = DEFAULT_USAGE["input_tokens"] * 3 + DEFAULT_USAGE["output_tokens"] * 15
    rows = wait_until(lambda: (r := quota_rows(db, quota_user)) and len(r) == 4 and r)
    for key in [("daily", "total"), ("monthly", "total"), ("daily", "tier:premium"), ("monthly", "tier:premium")]:
        assert rows[key]["spent_credits_micro"] == expected, key
        assert rows[key]["reserved_credits_micro"] == 0, key
        assert rows[key]["calls"] == 1
    assert rows[("daily", "total")]["input_tokens"] == 42
    assert rows[("daily", "total")]["output_tokens"] == 7
    st = status_map(c.get("/quota/status").json())
    assert st[("premium", "daily")]["used_credits_micro"] == expected
    assert st[("total", "monthly")]["remaining_credits_micro"] == TOTAL_MONTHLY - expected

    # standard model: x1 / x3, only the total bucket
    std = c.create_chat(model="gpt-4.1-mini")
    c.send(std["id"], "cheap")
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "total")]["spent_credits_micro"] == expected + 42 * 1 + 7 * 3
    assert rows[("daily", "tier:premium")]["spent_credits_micro"] == expected


def test_reserve_is_held_while_streaming(api, mock_llm, db, quota_user):
    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release))
    live = c.open_stream(chat["id"], "hold")
    try:
        events = live.read_until("delta")
        rid = events[0][1]["request_id"]
        rows = quota_rows(db, quota_user)
        reserved = rows[("daily", "total")]["reserved_credits_micro"]
        assert reserved > 0
        turn = db.execute("SELECT reserved_credits_micro FROM chat_turns WHERE request_id = ?", (ub(rid),)).fetchone()
        assert turn[0] == reserved
        st = status_map(c.get("/quota/status").json())
        assert st[("total", "daily")]["used_credits_micro"] == reserved  # spent + reserved
    finally:
        release.set()
    live.read_all()
    live.close()
    c.wait_turn(chat["id"], rid, ("done",))
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "total")]["reserved_credits_micro"] == 0
    assert rows[("daily", "total")]["spent_credits_micro"] == 42 + 7 * 3


def test_premium_exhausted_downgrades_to_standard(api, mock_llm, db, quota_user):
    seed(db, quota_user, "daily", "tier:premium", spent=PREMIUM_DAILY)
    c = api(quota_user)
    chat = c.create_chat()
    assert chat["model"] == "gpt-4.1"
    res = c.send(chat["id"], "downgrade me")
    d = res.done
    assert d["quota_decision"] == "downgrade"
    assert d["selected_model"] == "gpt-4.1"
    assert d["effective_model"] == "gpt-4.1-mini"
    assert d["downgrade_from"] == "gpt-4.1"
    assert d["downgrade_reason"] == "premium_quota_exhausted"
    prem = [w for w in d["quota_warnings"] if w["tier"] == "premium" and w["period"] == "daily"][0]
    assert prem["exhausted"] is True and prem["remaining_percentage"] == 0 and prem["next_reset"]

    body = mock_llm.chat_requests(chat["id"])[-1].json
    assert body["model"] == "gpt-4.1-mini"
    assert body["instructions"].startswith("You are the gpt-4.1-mini test assistant.")
    msgs = c.messages(chat["id"])
    assert msgs[-1]["model"] == "gpt-4.1-mini"
    assert c.get(f"/chats/{chat['id']}").json()["model"] == "gpt-4.1"  # chat model unchanged
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "tier:premium")]["spent_credits_micro"] == PREMIUM_DAILY  # premium not charged
    assert rows[("daily", "total")]["spent_credits_micro"] == 42 + 7 * 3

    # replay rebuilds the downgrade outcome without the reason
    replay = c.stream(chat["id"], "downgrade me", request_id=res.request_id)
    rd = replay.done
    assert rd["quota_decision"] == "downgrade" and rd["downgrade_from"] == "gpt-4.1"
    assert rd["effective_model"] == "gpt-4.1-mini" and "downgrade_reason" not in rd

    st = status_map(c.get("/quota/status").json())
    assert st[("premium", "daily")]["exhausted"] is True
    assert st[("premium", "daily")]["remaining_credits_micro"] == 0


def test_total_exhausted_is_429_before_provider_call(api, mock_llm, db, quota_user):
    seed(db, quota_user, "daily", "total", spent=TOTAL_DAILY)
    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    r = c.post(f"/chats/{chat['id']}/messages:stream", json={"content": "no budget"})
    p = assert_problem(r, 429)
    assert r.headers["content-type"].startswith("application/problem+json")
    v = p["context"]["violations"][0]
    assert v["subject"] == "tokens" and v["description"] == "quota_exceeded"
    assert mock_llm.chat_requests(chat["id"]) == []
    assert c.messages(chat["id"]) == []
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "total")]["reserved_credits_micro"] == 0
    # premium chat cascades down to standard and is rejected too
    prem = c.create_chat()
    assert_problem(c.post(f"/chats/{prem['id']}/messages:stream", json={"content": "x"}), 429)


def test_monthly_exhaustion_also_blocks(api, db, quota_user):
    seed(db, quota_user, "monthly", "total", spent=TOTAL_MONTHLY)
    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    assert_problem(c.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x"}), 429)


def test_web_search_daily_quota_is_429(api, mock_llm, db, quota_user):
    seed(db, quota_user, "daily", "total", spent=0, web_search_calls=75)
    c = api(quota_user)
    chat = c.create_chat()
    r = c.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "web_search": {"enabled": True}})
    p = assert_problem(r, 429)
    assert p["context"]["violations"][0]["subject"] == "web_search"
    # without web search the turn is allowed
    assert c.send(chat["id"], "no search").names[-1] == "done"
    assert mock_llm.chat_requests(chat["id"])[-1].json.get("tools") in (None, [])


def test_quota_warning_threshold(api, db, quota_user):
    seed(db, quota_user, "daily", "total", spent=TOTAL_DAILY * 85 // 100)
    c = api(quota_user)
    st = status_map(c.get("/quota/status").json())
    assert st[("total", "daily")]["warning"] is True
    assert st[("total", "daily")]["exhausted"] is False
    assert st[("total", "daily")]["remaining_percentage"] == 15
    chat = c.create_chat(model="gpt-4.1-mini")
    w = [x for x in c.send(chat["id"], "warn").done["quota_warnings"] if (x["tier"], x["period"]) == ("total", "daily")][0]
    assert w["warning"] is True and w["next_reset"]


def test_quota_is_per_user(api, db, quota_user):
    seed(db, quota_user, "daily", "total", spent=TOTAL_DAILY)
    st_b = status_map(api("B").get("/quota/status").json())
    assert st_b[("total", "daily")]["exhausted"] is False
    st_c = status_map(api(quota_user).get("/quota/status").json())
    assert st_c[("total", "daily")]["exhausted"] is True


# ── settlement per terminal outcome (DESIGN 5.7 / 5.8) ─────────────────────


def turn_row(db, rid: str):
    return db.execute(
        "SELECT state, reserve_tokens, max_output_tokens_applied, minimal_generation_floor_applied, "
        "reserved_credits_micro FROM chat_turns WHERE request_id = ?",
        (ub(rid),),
    ).fetchone()


def usage_event(db, rid: str) -> dict:
    from .conftest import outbox_payloads

    evs = wait_until(
        lambda: [m["payload"] for m in outbox_payloads(db, "mini-chat.usage_snapshot") if m["payload"].get("request_id") == rid]
    )
    assert len(evs) == 1, evs
    return evs[0]


def test_settlement_completed_is_actual(api, db, quota_user):
    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    res = c.send(chat["id"], "settle actual")
    ev = usage_event(db, res.request_id)
    assert ev["billing_outcome"] == "completed" and ev["settlement_method"] == "actual"
    assert ev["terminal_state"] == "completed"
    assert ev["usage"]["input_tokens"] == 42 and ev["usage"]["output_tokens"] == 7
    assert ev["actual_credits_micro"] == 42 + 7 * 3
    assert ev["selected_model"] == ev["effective_model"] == "gpt-4.1-mini"
    assert ev["requester_type"] == "user"
    assert ev["dedupe_key"]
    for internal in ("quota_decision", "reserved_credits_micro", "downgrade_reason"):
        assert internal not in ev
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "total")]["spent_credits_micro"] == ev["actual_credits_micro"]


def test_settlement_provider_error_without_usage_is_estimated(api, mock_llm, db, quota_user):
    from .mock_llm import json_response

    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    mock_llm.script_chat(chat["id"], json_response(500, {"error": {"message": "x"}}))
    res = c.stream(chat["id"], "fail without usage")
    assert res.error["code"] == "provider_error"
    t = turn_row(db, res.request_id)
    est_in = t["reserve_tokens"] - t["max_output_tokens_applied"]
    expected = est_in * 1 + t["minimal_generation_floor_applied"] * 3
    ev = usage_event(db, res.request_id)
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "estimated"
    assert ev["usage"] is None
    assert ev["actual_credits_micro"] == expected
    rows = quota_rows(db, quota_user)
    assert rows[("daily", "total")]["spent_credits_micro"] == expected
    assert rows[("daily", "total")]["reserved_credits_micro"] == 0


def test_settlement_provider_error_with_usage_is_actual(api, mock_llm, db, quota_user):
    from .mock_llm import sse_events

    c = api(quota_user)
    chat = c.create_chat(model="gpt-4.1-mini")
    mock_llm.script_chat(
        chat["id"],
        sse_events(
            [
                ("response.created", {"type": "response.created", "response": {"id": "resp_x"}}),
                (
                    "response.failed",
                    {
                        "type": "response.failed",
                        "response": {
                            "status": "failed",
                            "error": {"code": "server_error", "message": "boom"},
                            "usage": {"input_tokens": 10, "output_tokens": 2},
                        },
                    },
                ),
            ]
        ),
    )
    res = c.stream(chat["id"], "fail with usage")
    assert res.error["code"] == "provider_error"
    ev = usage_event(db, res.request_id)
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "actual"
    assert ev["actual_credits_micro"] == 10 * 1 + 2 * 3
    assert quota_rows(db, quota_user)[("daily", "total")]["spent_credits_micro"] == 16


def test_settlement_cancelled_is_estimated_aborted(api, mock_llm, db, quota_user):
    c = api(quota_user)
    chat = c.create_chat()  # premium: x3 / x15, both buckets
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release))
    live = c.open_stream(chat["id"], "abort me")
    try:
        rid = live.read_until("delta")[0][1]["request_id"]
    finally:
        live.close()
    try:
        c.wait_turn(chat["id"], rid, ("cancelled",), timeout=20)
    finally:
        release.set()
    t = turn_row(db, rid)
    est_in = t["reserve_tokens"] - t["max_output_tokens_applied"]
    expected = est_in * 3 + t["minimal_generation_floor_applied"] * 15
    ev = usage_event(db, rid)
    assert ev["billing_outcome"] == "aborted" and ev["settlement_method"] == "estimated"
    assert ev["terminal_state"] == "cancelled"
    assert ev["actual_credits_micro"] == expected
    rows = quota_rows(db, quota_user)
    for key in [("daily", "total"), ("monthly", "total"), ("daily", "tier:premium"), ("monthly", "tier:premium")]:
        assert rows[key]["spent_credits_micro"] == expected, key
        assert rows[key]["reserved_credits_micro"] == 0, key


def test_code_interpreter_daily_quota_is_429(api, db, quota_user):
    from .test_attachments import XLSX_MIME, xlsx_bytes

    c = api(quota_user)
    chat = c.create_chat()
    assert c.upload(chat["id"], "q.xlsx", xlsx_bytes(), XLSX_MIME).status_code == 201
    seed(db, quota_user, "daily", "total", spent=0, code_interpreter_calls=50)
    r = c.post(f"/chats/{chat['id']}/messages:stream", json={"content": "compute"})
    p = assert_problem(r, 429)
    assert p["context"]["violations"][0]["subject"] == "code_interpreter"
