"""Quota: preflight cascade and downgrade, reserve / settlement, tool
quotas, kill switches and the quota status endpoint."""

import datetime
import uuid

import pytest

import harness
from harness import Client, parse_sse, ubytes, wait_for

TENANT = harness.TENANT_A
USER = harness.USER_A1

STD = {"limit_daily_credits_micro": 50_000_000, "limit_monthly_credits_micro": 500_000_000}
PREM = {"limit_daily_credits_micro": 20_000_000, "limit_monthly_credits_micro": 200_000_000}


@pytest.fixture(scope="module")
def qstack(tmp_root):
    s = harness.Stack(f"{tmp_root}/quota", standard_limits=STD, premium_limits=PREM)
    s.start()
    yield s
    s.stop()


@pytest.fixture
def q(qstack):
    qstack.mock_reset()
    qstack.execute("delete from quota_usage")
    return qstack


def today():
    return datetime.datetime.now(datetime.timezone.utc).date()


def seed(stack, bucket, period, spent=0, reserved=0, web=0, ci=0, user=USER, tenant=TENANT):
    d = today()
    start = d if period == "daily" else d.replace(day=1)
    stack.execute(
        "insert into quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, spent_credits_micro,"
        " reserved_credits_micro, calls, input_tokens, output_tokens, file_search_calls, web_search_calls,"
        " code_interpreter_calls, rag_retrieval_calls, image_inputs, image_upload_bytes, updated_at)"
        " values (?,?,?,?,?,?,?,?,0,0,0,0,?,?,0,0,0,?)",
        (uuid.uuid4().bytes, ubytes(tenant), ubytes(user), period, start.isoformat(), bucket, spent, reserved,
         web, ci, "2026-01-01T00:00:00.000000001Z"),
    )


def rows(stack):
    return {(r["period_type"], r["bucket"]): r for r in stack.query("select * from quota_usage where user_id = ?", (ubytes(USER),))}


def done_of(ev):
    return [p for n, p in ev if n == "done"][0]


def test_premium_turn_charges_both_buckets(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-premium")
    q.mock_script([{"events": [{"type": "response.completed", "response": {"usage": {"input_tokens": 1000, "output_tokens": 1000}}}]}])
    ev = cl.send(c["id"], "hi")
    d = done_of(ev)
    assert d["quota_decision"] == "allow" and d["effective_model"] == "gpt-premium"
    r = rows(q)
    # 1000 * 3 + 1000 * 15 = 18000 micro-credits
    for period in ("daily", "monthly"):
        assert r[(period, "total")]["spent_credits_micro"] == 18000
        assert r[(period, "tier:premium")]["spent_credits_micro"] == 18000
        assert r[(period, "tier:premium")]["calls"] == 1
        assert r[(period, "tier:premium")]["input_tokens"] == 0  # tokens only in total
        assert r[(period, "total")]["input_tokens"] == 1000
        assert r[(period, "total")]["reserved_credits_micro"] == 0
    assert r[("daily", "total")]["period_start"] == today().isoformat()
    assert r[("monthly", "total")]["period_start"] == today().replace(day=1).isoformat()


def test_downgrade_when_premium_exhausted(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-premium")
    seed(q, "tier:premium", "daily", spent=PREM["limit_daily_credits_micro"])
    ev = cl.send(c["id"], "hi")
    d = done_of(ev)
    assert d["quota_decision"] == "downgrade"
    assert d["effective_model"] == "gpt-standard" and d["selected_model"] == "gpt-premium"
    assert d["downgrade_from"] == "gpt-premium" and d["downgrade_reason"] == "premium_quota_exhausted"
    body = q.mock_requests("/v1/responses")[-1]["json"]
    assert body["model"] == "gpt-standard" and body["instructions"] == "You are gpt-standard."
    msgs = cl.messages(c["id"])["items"]
    assert msgs[-1]["model"] == "gpt-standard"
    rid = ev[0][1]["request_id"]
    t = q.query("select effective_model from chat_turns where request_id = ?", (ubytes(rid),))[0]
    assert t["effective_model"] == "gpt-standard"
    r = rows(q)
    assert r[("daily", "tier:premium")]["calls"] == 0  # standard turn: total only
    assert r[("daily", "total")]["calls"] == 1
    # replay rebuilds the downgrade from the stored models
    rep = parse_sse(cl.stream(c["id"], "x", request_id=rid).text)
    d2 = done_of(rep)
    assert d2["quota_decision"] == "downgrade" and d2["downgrade_from"] == "gpt-premium"
    assert "downgrade_reason" not in d2


def test_reserve_must_fit_remaining_budget(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-premium")
    # premium budget nearly used: the reserve (max_output 4096 * 15 credits) does not fit
    seed(q, "tier:premium", "monthly", spent=PREM["limit_monthly_credits_micro"] - 1000)
    d = done_of(cl.send(c["id"], "hi"))
    assert d["effective_model"] == "gpt-standard" and d["downgrade_reason"] == "premium_quota_exhausted"


def test_all_tiers_exhausted(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-premium")
    seed(q, "total", "monthly", spent=STD["limit_monthly_credits_micro"])
    r = cl.stream(c["id"], "hi")
    assert r.status_code == 429
    v = r.json()["context"]["violations"][0]
    assert v == {"subject": "tokens", "description": "quota_exceeded"}
    assert q.mock_requests("/v1/responses") == []
    assert cl.messages(c["id"])["items"] == []
    # standard chat is rejected too (no upgrade to premium)
    c2 = cl.create_chat(model="gpt-standard")
    assert cl.stream(c2["id"], "hi").status_code == 429


def test_reserved_by_others_counts(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    seed(q, "total", "daily", reserved=STD["limit_daily_credits_micro"])
    assert cl.stream(c["id"], "hi").status_code == 429


def test_web_search_daily_quota(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    seed(q, "total", "daily", web=75)
    r = cl.stream(c["id"], "hi", web_search={"enabled": True})
    assert r.status_code == 429
    assert r.json()["context"]["violations"][0]["subject"] == "web_search"
    # without web search the turn passes
    assert cl.stream(c["id"], "hi").status_code == 200
    # a model without web search support skips the check
    c2 = cl.create_chat(model="gpt-novision")
    assert cl.stream(c2["id"], "hi", web_search={"enabled": True}).status_code == 200


def test_code_interpreter_daily_quota(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    assert cl.upload(c["id"], "d.xlsx", b"PK", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet").status_code == 201
    seed(q, "total", "daily", ci=50)
    r = cl.stream(c["id"], "hi")
    assert r.status_code == 429
    assert r.json()["context"]["violations"][0]["subject"] == "code_interpreter"
    c2 = cl.create_chat(model="gpt-standard")
    assert cl.stream(c2["id"], "hi").status_code == 200


def test_overshoot_capped_at_reserve(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    q.mock_script([{"events": [{"type": "response.completed", "response": {"usage": {"input_tokens": 1_000_000, "output_tokens": 1_000_000}}}]}])
    ev = cl.send(c["id"], "hi")
    d = done_of(ev)
    assert d["usage"] == {"input_tokens": 1_000_000, "output_tokens": 1_000_000}
    rid = ev[0][1]["request_id"]
    t = q.query("select * from chat_turns where request_id = ?", (ubytes(rid),))[0]
    assert t["state"] == "completed"
    assert rows(q)[("daily", "total")]["spent_credits_micro"] == t["reserved_credits_micro"]


def test_reserve_formula(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    ev = cl.send(c["id"], "a" * 400)
    rid = ev[0][1]["request_id"]
    t = q.query("select * from chat_turns where request_id = ?", (ubytes(rid),))[0]
    # ceil((ceil(400/4) + 100) * 110 / 100) = 220 estimated input tokens
    assert t["reserve_tokens"] - t["max_output_tokens_applied"] == 220
    assert t["max_output_tokens_applied"] == 4096
    assert t["reserved_credits_micro"] == 220 * 1 + 4096 * 3
    # the next turn adds prior_context_tokens (last assistant message usage: 11 + 7)
    ev = cl.send(c["id"], "a" * 400)
    t2 = q.query("select * from chat_turns where request_id = ?", (ubytes(ev[0][1]["request_id"]),))[0]
    assert t2["reserve_tokens"] - t2["max_output_tokens_applied"] == 220 + 18


def test_estimated_settlement_on_failure(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    q.mock_script([{"status": 500, "body": {}}])
    ev = cl.send(c["id"], "a" * 400)
    t = q.query("select * from chat_turns where request_id = ?", (ubytes(ev[0][1]["request_id"]),))[0]
    # estimated: credits(estimated_input=220, floor=50) = 220 + 150
    r = rows(q)
    assert r[("daily", "total")]["spent_credits_micro"] == 220 + 50 * 3
    assert r[("daily", "total")]["reserved_credits_micro"] == 0
    assert r[("daily", "total")]["input_tokens"] == 0
    assert t["state"] == "failed"


def test_failed_with_usage_settles_actual(q):
    cl = Client(q)
    c = cl.create_chat(model="gpt-standard")
    q.mock_script([{"events": [{"type": "response.failed", "response": {"error": {"message": "x"},
                                 "usage": {"input_tokens": 10, "output_tokens": 10}}}]}])
    cl.send(c["id"], "hi")
    assert rows(q)[("daily", "total")]["spent_credits_micro"] == 10 + 30


def test_quota_status_endpoint(q):
    cl = Client(q)
    r = cl.req("GET", "/v1/quota/status")
    assert r.status_code == 200
    s = r.json()
    assert s["warning_threshold_pct"] == 80
    assert [t["tier"] for t in s["tiers"]] == ["premium", "total"]
    total = {p["period"]: p for p in s["tiers"][1]["periods"]}
    assert total["daily"]["limit_credits_micro"] == STD["limit_daily_credits_micro"]
    assert total["daily"]["used_credits_micro"] == 0 and total["daily"]["remaining_percentage"] == 100
    assert total["daily"]["warning"] is False and total["daily"]["exhausted"] is False
    tomorrow = today() + datetime.timedelta(days=1)
    assert total["daily"]["next_reset"].startswith(tomorrow.isoformat() + "T00:00:00")
    assert total["monthly"]["next_reset"][8:10] == "01"
    seed(q, "total", "daily", spent=40_000_000, reserved=1_000_000)  # 82% used
    seed(q, "tier:premium", "daily", spent=PREM["limit_daily_credits_micro"] - 1)
    s = cl.req("GET", "/v1/quota/status").json()
    total = {p["period"]: p for p in s["tiers"][1]["periods"]}
    assert total["daily"]["used_credits_micro"] == 41_000_000
    assert total["daily"]["remaining_credits_micro"] == 9_000_000
    assert total["daily"]["remaining_percentage"] == 18
    assert total["daily"]["warning"] is True and total["daily"]["exhausted"] is False
    prem = {p["period"]: p for p in s["tiers"][0]["periods"]}
    assert prem["daily"]["remaining_percentage"] == 0 and prem["daily"]["exhausted"] is True
    # warnings in done mirror the status
    c = cl.create_chat(model="gpt-standard")
    d = done_of(cl.send(c["id"], "hi"))
    w = {(x["tier"], x["period"]): x for x in d["quota_warnings"]}
    assert w[("total", "daily")]["warning"] is True and "next_reset" in w[("total", "daily")]
    assert "next_reset" not in w[("total", "monthly")]
    # another user sees their own data only
    other = Client(q, "token-a2").req("GET", "/v1/quota/status").json()
    assert {p["period"]: p for p in other["tiers"][1]["periods"]}["daily"]["used_credits_micro"] == 0


def test_kill_switch_force_standard(tmp_root):
    s = harness.Stack(f"{tmp_root}/force-std", kill_switches={"force_standard_tier": True})
    s.start()
    try:
        cl = Client(s)
        c = cl.create_chat(model="gpt-premium")
        d = done_of(cl.send(c["id"], "hi"))
        assert d["effective_model"] == "gpt-standard" and d["downgrade_reason"] == "force_standard_tier"
    finally:
        s.stop()


def test_kill_switch_disable_premium_and_model_disabled(tmp_root):
    s = harness.Stack(f"{tmp_root}/no-prem", kill_switches={"disable_premium_tier": True})
    s.start()
    try:
        cl = Client(s)
        c = cl.create_chat(model="gpt-premium")
        d = done_of(cl.send(c["id"], "hi"))
        assert d["effective_model"] == "gpt-standard" and d["downgrade_reason"] == "disable_premium_tier"
        # a chat whose model was disabled later is downgraded (model_disabled)
        c2 = cl.create_chat(model="gpt-standard")
        s.execute("update chats set model = 'gpt-disabled' where id = ?", (ubytes(c2["id"]),))
        d = done_of(cl.send(c2["id"], "hi"))
        assert d["downgrade_reason"] == "model_disabled" and d["selected_model"] == "gpt-disabled"
        assert d["effective_model"] == "gpt-standard"
        # a chat whose model left the catalog is rejected
        s.execute("update chats set model = 'gone' where id = ?", (ubytes(c2["id"]),))
        r = cl.stream(c2["id"], "hi")
        assert r.status_code == 400
        assert r.json()["context"]["field_violations"][0]["reason"] == "INVALID_MODEL"
        r = cl.upload(c2["id"], "a.txt", b"x", "text/plain")
        assert r.status_code == 400
        assert r.json()["context"]["field_violations"][0]["reason"] == "INVALID_MODEL"
    finally:
        s.stop()
