"""Quota status, reserve-before-execute, downgrade, credits accounting,
settlement of every terminal outcome, usage publication, web search limits."""

from __future__ import annotations

import math
import re
import time
import uuid

import pytest

import prov
from harness import USER_A, USER_A2, BackgroundStream, problem_reason, ub, wait_until


def rows(env, user=USER_A):
    out = {}
    for r in env.server.query("SELECT * FROM quota_usage WHERE user_id = ?", (ub(user),)):
        out[(r["bucket"], r["period_type"])] = r
    return out


def settled(env, user=USER_A):
    return wait_until(
        lambda: (lambda rs: rs if rs and all(r["reserved_credits_micro"] == 0 for r in rs.values()) else None)(rows(env, user)),
        msg="reserves released",
    )


def credits(inp, out, im, om):
    return math.ceil(inp * im / 1_000_000) + math.ceil(out * om / 1_000_000)


def usage_lines(env, dedupe_prefix=""):
    return [l for l in env.server.log_text().splitlines() if "usage event published" in l and dedupe_prefix in l]


@pytest.fixture(scope="module")
def qenv(make_env):
    return make_env({
        "gears": {
            "mini-chat": {"config": {"quota": {"web_search_daily_quota": 1, "web_search_max_calls_per_message": 2}}},
            "static-mini-chat-model-policy-plugin": {
                "config": {
                    "default_premium_limits": {"limit_daily_credits_micro": 1000, "limit_monthly_credits_micro": 1_000_000},
                    "default_standard_limits": {"limit_daily_credits_micro": 10_000_000, "limit_monthly_credits_micro": 100_000_000},
                }
            },
        }
    })


def test_downgrade_when_premium_exhausted(qenv):
    c = qenv.a
    chat = c.create_chat(model="premium-1")
    r = c.send(chat["id"], "hello premium")
    assert r.names[-1] == "done"
    d = r.done
    assert d["selected_model"] == "premium-1" and d["effective_model"] == "gpt-4.1-mini"
    assert d["quota_decision"] == "downgrade"
    assert d["downgrade_from"] == "premium-1" and d["downgrade_reason"] == "premium_quota_exhausted"
    body = qenv.mock.chat_requests()[0]["json"]
    assert body["model"] == "prov-gpt-4.1-mini"
    assert body["instructions"].startswith("SYSTEM PROMPT gpt-4.1-mini")
    rid = r.started["request_id"]
    turn = qenv.server.query("SELECT effective_model FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert turn["effective_model"] == "gpt-4.1-mini"
    msgs = c.messages(chat["id"])
    assert msgs[1]["model"] == "gpt-4.1-mini"
    # the premium bucket is untouched, the total bucket is charged
    rs = settled(qenv)
    assert ("tier:premium", "daily") not in rs or rs[("tier:premium", "daily")]["spent_credits_micro"] == 0
    assert rs[("total", "daily")]["spent_credits_micro"] == credits(100, 20, 1_000_000, 3_000_000)
    # replay rebuilds the downgrade outcome from the stored models
    rep = c.send(chat["id"], "x", request_id=rid)
    assert rep.done["quota_decision"] == "downgrade" and rep.done["downgrade_from"] == "premium-1"
    assert "downgrade_reason" not in rep.done


def test_credits_accounting_and_status(qenv):
    c = qenv.a2  # fresh user
    chat = c.create_chat(model="gpt-4.1-mini")
    qenv.mock.script([prov.text_reply("a", usage={"input_tokens": 1234, "output_tokens": 567})])
    rid = c.send(chat["id"], "count me").started["request_id"]
    rs = settled(qenv, USER_A2)
    total_d = rs[("total", "daily")]
    expected = credits(1234, 567, 1_000_000, 3_000_000)
    assert total_d["spent_credits_micro"] == expected
    assert total_d["calls"] == 1 and total_d["input_tokens"] == 1234 and total_d["output_tokens"] == 567
    assert rs[("total", "monthly")]["spent_credits_micro"] == expected
    turn = qenv.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert turn["policy_version_applied"] == 1
    assert turn["reserved_credits_micro"] == credits(turn["reserve_tokens"] - turn["max_output_tokens_applied"], turn["max_output_tokens_applied"], 1_000_000, 3_000_000)
    # usage event published once, with the committed credits
    line = wait_until(lambda: usage_lines(qenv, uuid.UUID(rid).hex), msg="usage event")
    assert len(line) == 1
    assert f"actual_credits_micro={expected}" in line[0] and "settlement_method=actual" in line[0]
    # status reflects the usage
    st = c.quota()
    assert st["warning_threshold_pct"] == 80
    tiers = {t["tier"]: t for t in st["tiers"]}
    assert [t["tier"] for t in st["tiers"]] == ["premium", "total"]
    daily = [p for p in tiers["total"]["periods"] if p["period"] == "daily"][0]
    assert daily["limit_credits_micro"] == 10_000_000
    assert daily["used_credits_micro"] == expected
    assert daily["remaining_credits_micro"] == 10_000_000 - expected
    assert daily["remaining_percentage"] == (10_000_000 - expected) * 100 // 10_000_000
    assert daily["warning"] is False and daily["exhausted"] is False
    assert re.match(r"^\d{4}-\d{2}-\d{2}T00:00:00Z$", daily["next_reset"])
    monthly = [p for p in tiers["total"]["periods"] if p["period"] == "monthly"][0]
    assert monthly["next_reset"].endswith("-01T00:00:00Z")


def test_reserve_held_while_running_then_settled(qenv):
    c = qenv.a
    chat = c.create_chat(model="gpt-4.1-mini")
    before = settled(qenv)[("total", "daily")]["spent_credits_micro"]
    qenv.mock.script([prov.sse(prov.created(), prov.sleep(2500), prov.delta("z"), prov.completed("z", usage={"input_tokens": 10, "output_tokens": 10}))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "reserve"})
    rid = bg.wait_event("stream_started").data["request_id"]
    turn = qenv.server.query("SELECT reserved_credits_micro FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    r = rows(qenv)[("total", "daily")]
    assert r["reserved_credits_micro"] == turn["reserved_credits_micro"] > 0
    # quota status counts the reserve as used
    daily = [p for t in c.quota()["tiers"] if t["tier"] == "total" for p in t["periods"] if p["period"] == "daily"][0]
    assert daily["used_credits_micro"] == r["spent_credits_micro"] + r["reserved_credits_micro"]
    bg.join()
    after = settled(qenv)[("total", "daily")]
    assert after["spent_credits_micro"] == before + credits(10, 10, 1_000_000, 3_000_000)


def test_estimated_settlement_on_cancel_and_failure(qenv):
    c = qenv.a
    chat = c.create_chat(model="gpt-4.1-mini")
    base = settled(qenv)[("total", "daily")]["spent_credits_micro"]
    # cancelled -> aborted / estimated
    qenv.mock.script([prov.sse(prov.created(), prov.delta("p"), prov.sleep(20000), prov.completed("p"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "cancel"})
    rid = bg.wait_event("stream_started").data["request_id"]
    bg.wait_event("delta")
    bg.disconnect()
    wait_until(lambda: c.turn(chat["id"], rid).json()["state"] == "cancelled", msg="cancel")
    t = qenv.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    est = credits(t["reserve_tokens"] - t["max_output_tokens_applied"], t["minimal_generation_floor_applied"], 1_000_000, 3_000_000)
    assert t["minimal_generation_floor_applied"] == 50
    rs = settled(qenv)
    assert rs[("total", "daily")]["spent_credits_micro"] == base + est
    line = wait_until(lambda: usage_lines(qenv, uuid.UUID(rid).hex), msg="usage")
    assert "billing_outcome=aborted" in line[0] and "settlement_method=estimated" in line[0]
    # failed without usage -> estimated; failed with usage -> actual
    base = rs[("total", "daily")]["spent_credits_micro"]
    qenv.mock.script([{"kind": "error", "status": 500, "body": {"error": {"message": "x"}}}])
    rid2 = c.send(chat["id"], "fail").started["request_id"]
    t2 = qenv.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid2),))[0]
    est2 = credits(t2["reserve_tokens"] - t2["max_output_tokens_applied"], 50, 1_000_000, 3_000_000)
    rs = settled(qenv)
    assert rs[("total", "daily")]["spent_credits_micro"] == base + est2
    line = wait_until(lambda: usage_lines(qenv, uuid.UUID(rid2).hex), msg="usage")
    assert "billing_outcome=failed" in line[0] and "settlement_method=estimated" in line[0]
    base = rs[("total", "daily")]["spent_credits_micro"]
    qenv.mock.script([prov.sse(prov.created(), prov.failed("bad", usage={"input_tokens": 30, "output_tokens": 5}))])
    rid3 = c.send(chat["id"], "fail2").started["request_id"]
    rs = settled(qenv)
    assert rs[("total", "daily")]["spent_credits_micro"] == base + credits(30, 5, 1_000_000, 3_000_000)
    line = wait_until(lambda: usage_lines(qenv, uuid.UUID(rid3).hex), msg="usage")
    assert "settlement_method=actual" in line[0]


def test_web_search_quota_and_limits(qenv):
    c = qenv.a
    chat = c.create_chat(model="premium-1")
    ws = [prov.ev("response.web_search_call.searching"), prov.ev("response.web_search_call.completed")]
    # per-message call limit (2): a third call fails the turn
    qenv.mock.script([prov.sse(prov.created(), *ws, *ws, prov.ev("response.web_search_call.searching"), prov.delta("x"), prov.completed("x"))])
    r = c.send(chat["id"], "search a lot", web_search={"enabled": True})
    err = r.first("error").data
    assert err["code"] == "web_search_calls_exceeded"
    assert c.turn(chat["id"], r.started["request_id"]).json()["error_code"] == "web_search_calls_exceeded"
    # completed web search calls are counted against the daily quota (1)
    d = settled(qenv)[("total", "daily")]
    assert d["web_search_calls"] == 2
    r = c.send(chat["id"], "search again", web_search={"enabled": True})
    assert r.status == 429
    v = r.body["context"]["violations"][0]
    assert (v["subject"], v["description"]) == ("web_search", "quota_exceeded")
    assert r.body["type"].endswith("resource_exhausted.v1~")
    # without web search the turn is allowed and the tool is not sent
    qenv.mock.reset()
    r = c.send(chat["id"], "no search")
    assert r.names[-1] == "done"
    body = qenv.mock.chat_requests()[0]["json"]
    assert not any(t["type"] == "web_search" for t in body.get("tools", []))


def test_quota_warnings_overshoot_and_exhaustion(make_env):
    e = make_env({
        "gears": {
            "static-mini-chat-model-policy-plugin": {
                "config": {"default_standard_limits": {"limit_daily_credits_micro": 1800, "limit_monthly_credits_micro": 1_000_000}}
            }
        }
    })
    c = e.a
    chat = c.create_chat(model="tiny-ctx")
    e.mock.script([prov.text_reply("big", usage={"input_tokens": 1000, "output_tokens": 300})])
    r = c.send(chat["id"], "hey")
    assert r.names[-1] == "done"
    rid = r.started["request_id"]
    t = e.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    reserved = t["reserved_credits_micro"]
    # overshoot beyond tolerance: committed credits capped at the reserve; turn stays completed
    rs = settled(e)
    assert rs[("total", "daily")]["spent_credits_micro"] == reserved
    assert c.turn(chat["id"], rid).json()["state"] == "done"
    pct = (1800 - reserved) * 100 // 1800
    assert pct <= 20
    warns = r.done["quota_warnings"]
    total_daily = [w for w in warns if w["tier"] == "total" and w["period"] == "daily"][0]
    assert total_daily["warning"] is True and total_daily["exhausted"] is False
    assert total_daily["remaining_percentage"] == pct and "next_reset" in total_daily
    total_monthly = [w for w in warns if w["tier"] == "total" and w["period"] == "monthly"][0]
    assert total_monthly["warning"] is False and "next_reset" not in total_monthly
    st = c.quota()
    daily = [p for t_ in st["tiers"] if t_["tier"] == "total" for p in t_["periods"] if p["period"] == "daily"][0]
    assert daily["warning"] is True and daily["remaining_percentage"] == pct
    # no tier can cover the next reserve: rejected before any provider call
    e.mock.reset()
    r = c.send(chat["id"], "again")
    assert r.status == 429
    v = r.body["context"]["violations"][0]
    assert (v["subject"], v["description"]) == ("tokens", "quota_exceeded")
    assert e.mock.chat_requests() == []
    # retry is also rejected by the preflight and leaves the turn unchanged
    r = c.retry(chat["id"], rid)
    assert r.status == 429
    assert c.turn(chat["id"], rid).json()["state"] == "done"


def test_kill_switches(make_env):
    e = make_env({
        "gears": {
            "static-mini-chat-model-policy-plugin": {
                "config": {"kill_switches": {"disable_web_search": True, "disable_images": True, "force_standard_tier": True}}
            }
        }
    })
    c = e.a
    chat = c.create_chat(model="premium-1")
    r = c.send(chat["id"], "x", web_search={"enabled": True})
    assert r.status == 400
    v = r.body["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("web_search", "FEATURE_DISABLED")
    r = c.upload(chat["id"], "a.png", b"\x89PNG\r\n\x1a\n", "image/png")
    assert r.status_code == 400
    v = r.json()["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("images", "FEATURE_DISABLED")
    r = c.send(chat["id"], "x")
    assert r.done["effective_model"] == "gpt-4.1-mini"
    assert r.done["downgrade_reason"] == "force_standard_tier"
    assert e.mock.chat_requests()[0]["json"]["model"] == "prov-gpt-4.1-mini"
