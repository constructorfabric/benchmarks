"""Quota: status API, reserve-before-execute, downgrade cascade, settlement and usage events.

Runs on a dedicated server (``qs``) and restores the seeded quota rows after each test.

Acceptance criteria covered:
* Quota Status — "Quota status reporting is accurate and consistent with actual usage"
* Quota Enforcement — "Reserve-before-execute quota flow enforced on every provider call"
* Quota Enforcement — "Tier downgrade applied when a higher tier is exhausted"
* Quota Enforcement — "Credits and tokens are accounted correctly per model and tier"
* Settlement — "Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate"
* Settlement — "Usage is published reliably and exactly once per turn"
* Principles — "Quota is checked before any outbound provider call"
"""

from __future__ import annotations

import datetime as dt
import time
import uuid

import httpx
import pytest

from helpers import (
    QuotaRestorer,
    api_ts,
    assert_ok_stream,
    assert_problem,
    audit_event_types,
    audit_events,
    bg_send,
    credits_micro,
    ensure_quota_rows,
    estimate_text_tokens,
    http_error,
    list_messages,
    message_rows,
    new_chat,
    quota_snapshot,
    send_ok,
    set_quota,
    sleep,
    stream_script,
    tenant_id,
    turn_row,
    turn_rows,
    ub,
    usage_events_for,
    user_id,
    wait_running,
    wait_turn_state,
    wait_until,
    wait_usage_event,
)

TOTAL_DAILY, TOTAL_MONTHLY = 100_000_000, 1_000_000_000
PREMIUM_DAILY, PREMIUM_MONTHLY = 50_000_000, 500_000_000
MAX_OUT = 32768
MINI = (1_000_000, 3_000_000)
PREMIUM = (3_000_000, 15_000_000)


@pytest.fixture
def qa(qs):
    """Quota server with user a1's current rows present; restores them afterwards."""
    ensure_quota_rows(qs, "a1")
    saver = QuotaRestorer(qs, "a1")
    qs.mock_reset()
    yield qs
    saver.restore()


def _snap(srv, user="a1"):
    return quota_snapshot(srv, user)


def _delta(before, after, key, col):
    return int(after[key][col]) - int(before[key][col])


# ── status API ────────────────────────────────────────────────────────────
def test_quota_status_shape_and_consistency(qa):
    r = qa.req("GET", "/quota/status")
    assert r.status_code == 200, r.text
    j = r.json()
    assert j["warning_threshold_pct"] == 80
    tiers = {t["tier"]: t for t in j["tiers"]}
    assert set(tiers) == {"premium", "total"}
    limits = {
        ("total", "daily"): TOTAL_DAILY,
        ("total", "monthly"): TOTAL_MONTHLY,
        ("premium", "daily"): PREMIUM_DAILY,
        ("premium", "monthly"): PREMIUM_MONTHLY,
    }
    snap = _snap(qa)
    now = dt.datetime.now(dt.timezone.utc)
    tomorrow = dt.datetime.combine(now.date() + dt.timedelta(days=1), dt.time())
    first_next = dt.datetime(now.year + (now.month == 12), now.month % 12 + 1, 1)
    for tier, t in tiers.items():
        assert {p["period"] for p in t["periods"]} == {"daily", "monthly"}
        for p in t["periods"]:
            assert p["limit_credits_micro"] == limits[(tier, p["period"])]
            bucket = "total" if tier == "total" else "tier:premium"
            row = snap[(p["period"], bucket)]
            used = int(row["spent_credits_micro"]) + int(row["reserved_credits_micro"])
            assert p["used_credits_micro"] == used
            assert p["remaining_credits_micro"] == max(p["limit_credits_micro"] - used, 0)
            assert p["remaining_percentage"] == p["remaining_credits_micro"] * 100 // p["limit_credits_micro"]
            assert p["warning"] == (p["remaining_percentage"] <= 20)
            assert p["exhausted"] == (p["remaining_percentage"] == 0)
            expected_reset = tomorrow if p["period"] == "daily" else first_next
            assert api_ts(p["next_reset"]) == expected_reset


def test_quota_status_after_usage(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    send_ok(qa, cid, "consume")
    snap = _snap(qa)
    j = qa.req("GET", "/quota/status").json()
    total = [t for t in j["tiers"] if t["tier"] == "total"][0]
    daily = [p for p in total["periods"] if p["period"] == "daily"][0]
    row = snap[("daily", "total")]
    assert daily["used_credits_micro"] == int(row["spent_credits_micro"]) + int(row["reserved_credits_micro"])


def test_warning_and_exhausted_flags(qa):
    set_quota(qa, "a1", "daily", "total", spent_credits_micro=85_000_000, reserved_credits_micro=0)
    set_quota(qa, "a1", "monthly", "tier:premium", spent_credits_micro=PREMIUM_MONTHLY, reserved_credits_micro=0)
    j = qa.req("GET", "/quota/status").json()
    per = {(t["tier"], p["period"]): p for t in j["tiers"] for p in t["periods"]}
    assert per[("total", "daily")]["remaining_percentage"] == 15
    assert per[("total", "daily")]["warning"] is True and per[("total", "daily")]["exhausted"] is False
    assert per[("premium", "monthly")]["exhausted"] is True and per[("premium", "monthly")]["remaining_credits_micro"] == 0
    # done.quota_warnings reflects the same state, with next_reset on warning/exhausted entries.
    cid = new_chat(qa, "gpt-4.1-mini")
    _, done, _ = send_ok(qa, cid, "warn me")
    ws = {(w["tier"], w["period"]): w for w in done["quota_warnings"]}
    w = ws[("total", "daily")]
    assert w["warning"] is True and w["exhausted"] is False and w.get("next_reset")
    assert ws[("premium", "monthly")]["exhausted"] is True
    for w in done["quota_warnings"]:
        if not w["warning"] and not w["exhausted"]:
            assert "next_reset" not in w or w["next_reset"] is None


# ── reserve before execute ────────────────────────────────────────────────
def test_reserve_persisted_then_settled_actual(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    content = "reserve check message"
    before = _snap(qa)
    qa.mock_script(stream_script("slow", sleep(2000), " answer", usage={"input_tokens": 100, "output_tokens": 50}))
    bg = bg_send(qa, cid, content)
    row = wait_running(qa, cid)
    est = estimate_text_tokens(content)
    assert int(row["max_output_tokens_applied"]) == MAX_OUT
    assert int(row["reserve_tokens"]) == est + MAX_OUT
    reserved = credits_micro(est, MAX_OUT, *MINI)
    assert int(row["reserved_credits_micro"]) == reserved
    assert int(row["policy_version_applied"]) == 1
    assert row["effective_model"] == "gpt-4.1-mini"
    assert int(row["minimal_generation_floor_applied"]) == 50
    during = _snap(qa)
    for period in ("daily", "monthly"):
        assert _delta(before, during, (period, "total"), "reserved_credits_micro") == reserved
        assert _delta(before, during, (period, "tier:premium"), "reserved_credits_micro") == 0, "standard turns do not touch the premium bucket"
    # The reserve was taken before the provider call.
    assert len(qa.chat_requests()) == 1
    bg.wait()
    assert bg.events[-1].event == "done"
    after = _snap(qa)
    for period in ("daily", "monthly"):
        k = (period, "total")
        assert _delta(before, after, k, "reserved_credits_micro") == 0
        assert _delta(before, after, k, "spent_credits_micro") == 100 + 150
        assert _delta(before, after, k, "calls") == 1
        assert _delta(before, after, k, "input_tokens") == 100
        assert _delta(before, after, k, "output_tokens") == 50
        kp = (period, "tier:premium")
        assert _delta(before, after, kp, "spent_credits_micro") == 0
        assert _delta(before, after, kp, "calls") == 0
    ev = wait_usage_event(qa, bg.events[0].data["request_id"])
    assert ev["settlement_method"] == "actual" and ev["billing_outcome"] == "completed"
    assert ev["actual_credits_micro"] == 250


def test_premium_turn_settles_both_buckets(qa):
    cid = new_chat(qa, "gpt-4.1")
    content = "premium turn"
    before = _snap(qa)
    st, done, _ = send_ok(qa, cid, content)
    assert done["effective_model"] == "gpt-4.1" and done["quota_decision"] == "allow"
    row = turn_row(qa, cid, st["request_id"])
    est = estimate_text_tokens(content)
    assert int(row["reserved_credits_micro"]) == credits_micro(est, MAX_OUT, *PREMIUM)
    after = _snap(qa)
    expected = credits_micro(100, 50, *PREMIUM)  # 300 + 750
    for period in ("daily", "monthly"):
        for bucket in ("total", "tier:premium"):
            k = (period, bucket)
            assert _delta(before, after, k, "spent_credits_micro") == expected, k
            assert _delta(before, after, k, "reserved_credits_micro") == 0, k
            assert _delta(before, after, k, "calls") == 1, k
        assert _delta(before, after, (period, "tier:premium"), "input_tokens") == 0, "token telemetry only in total"
        assert _delta(before, after, (period, "total"), "input_tokens") == 100


def test_prior_context_tokens_in_reserve(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    qa.mock_script(stream_script("first", usage={"input_tokens": 400, "output_tokens": 60}))
    send_ok(qa, cid, "first")
    st, _, _ = send_ok(qa, cid, "second")
    row = turn_row(qa, cid, st["request_id"])
    assert int(row["reserve_tokens"]) == estimate_text_tokens("second") + 460 + MAX_OUT


def test_web_search_surcharge_in_reserve(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    st, _, _ = send_ok(qa, cid, "search", web_search={"enabled": True})
    row = turn_row(qa, cid, st["request_id"])
    assert int(row["reserve_tokens"]) == estimate_text_tokens("search") + 500 + MAX_OUT


def test_exhausted_user_rejected_before_provider(qa):
    set_quota(qa, "a1", "daily", "total", spent_credits_micro=TOTAL_DAILY)
    cid = new_chat(qa, "gpt-4.1-mini")
    r, events = qa.stream(cid, "no room")
    assert events == []
    assert_problem(r, 429, subject="tokens", description="quota_exceeded")
    assert qa.chat_requests() == []
    assert turn_rows(qa, cid) == [] and message_rows(qa, cid) == []
    # Premium chat also rejected: no tier available.
    cid2 = new_chat(qa, "gpt-4.1")
    r, _ = qa.stream(cid2, "no room")
    assert_problem(r, 429, subject="tokens", description="quota_exceeded")
    assert qa.chat_requests() == []


def test_monthly_exhaustion_rejected(qa):
    set_quota(qa, "a1", "monthly", "total", spent_credits_micro=TOTAL_MONTHLY - 1000)
    cid = new_chat(qa, "gpt-4.1-mini")
    r, _ = qa.stream(cid, "no room this month")
    assert_problem(r, 429, subject="tokens", description="quota_exceeded")
    assert qa.chat_requests() == []


def test_reserve_must_fit_not_just_spent(qa):
    """availability = spent + reserved + candidate_reserve <= limit."""
    cid = new_chat(qa, "gpt-4.1-mini")
    est = estimate_text_tokens("fits?")
    need = credits_micro(est, MAX_OUT, *MINI)
    set_quota(qa, "a1", "daily", "total", spent_credits_micro=TOTAL_DAILY - need + 1, reserved_credits_micro=0)
    r, _ = qa.stream(cid, "fits?")
    assert_problem(r, 429, subject="tokens")
    set_quota(qa, "a1", "daily", "total", spent_credits_micro=TOTAL_DAILY - need, reserved_credits_micro=0)
    send_ok(qa, cid, "fits?")


# ── downgrade cascade ─────────────────────────────────────────────────────
def test_premium_exhausted_downgrades_to_standard(qa):
    set_quota(qa, "a1", "daily", "tier:premium", spent_credits_micro=PREMIUM_DAILY)
    cid = new_chat(qa, "gpt-4.1")
    before = _snap(qa)
    st, done, _ = send_ok(qa, cid, "downgrade me")
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == "gpt-4.1"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    assert done["selected_model"] == "gpt-4.1"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert qa.chat_requests()[-1]["model"] == "gpt-4.1-mini"
    assert turn_row(qa, cid, st["request_id"])["effective_model"] == "gpt-4.1-mini"
    assert list_messages(qa, cid)[1]["model"] == "gpt-4.1-mini"
    after = _snap(qa)
    assert _delta(before, after, ("daily", "total"), "spent_credits_micro") == credits_micro(100, 50, *MINI)
    assert _delta(before, after, ("daily", "tier:premium"), "spent_credits_micro") == 0
    # Replay rebuilds the decision but omits the reason.
    r, events = qa.stream(cid, "x", request_id=st["request_id"])
    d = assert_ok_stream(r, events).data
    assert d["quota_decision"] == "downgrade" and d["downgrade_from"] == "gpt-4.1"
    assert "downgrade_reason" not in d


def test_model_disabled_downgrade(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    qa.execute("UPDATE chats SET model = 'disabled-model' WHERE id = ?", (ub(cid),))
    _, done, _ = send_ok(qa, cid, "my model is disabled")
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "model_disabled"
    assert done["selected_model"] == "disabled-model" and done["downgrade_from"] == "disabled-model"
    assert done["effective_model"] == "gpt-4.1-mini"


def test_disable_premium_tier_kill_switch(ks):
    cid = new_chat(ks, "gpt-4.1")
    _, done, _ = send_ok(ks, cid, "premium off")
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "disable_premium_tier"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert ks.chat_requests()[-1]["model"] == "gpt-4.1-mini"


def test_force_standard_tier_kill_switch(fs_srv):
    cid = new_chat(fs_srv, "gpt-4.1")
    _, done, _ = send_ok(fs_srv, cid, "forced standard")
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "force_standard_tier"
    assert done["effective_model"] == "gpt-4.1-mini"
    # Standard chats are not downgraded.
    cid2 = new_chat(fs_srv, "gpt-4.1-mini")
    _, done2, _ = send_ok(fs_srv, cid2, "standard")
    assert done2["quota_decision"] == "allow" and "downgrade_reason" not in done2


# ── settlement per terminal outcome ───────────────────────────────────────
def test_failed_turn_without_usage_settles_estimated(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    before = _snap(qa)
    qa.mock_script(http_error(500, "fail"))
    r, events = qa.stream(cid, "fail estimated")
    assert events[-1].event == "error"
    rid = events[0].data["request_id"]
    row = turn_row(qa, cid, rid)
    est_in = int(row["reserve_tokens"]) - int(row["max_output_tokens_applied"])
    expected = credits_micro(est_in, int(row["minimal_generation_floor_applied"]), *MINI)
    after = _snap(qa)
    k = ("daily", "total")
    assert _delta(before, after, k, "spent_credits_micro") == expected
    assert _delta(before, after, k, "reserved_credits_micro") == 0
    assert _delta(before, after, k, "calls") == 1
    assert _delta(before, after, k, "input_tokens") == 0, "estimated settlements add no token telemetry"
    ev = wait_usage_event(qa, rid)
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "estimated"
    assert ev["actual_credits_micro"] == expected


def test_failed_turn_with_usage_settles_actual(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    before = _snap(qa)
    qa.mock_script(stream_script("x", terminal="failed", usage={"input_tokens": 10, "output_tokens": 5}))
    r, events = qa.stream(cid, "fail actual")
    assert events[-1].event == "error"
    after = _snap(qa)
    assert _delta(before, after, ("daily", "total"), "spent_credits_micro") == 10 + 15
    ev = wait_usage_event(qa, events[0].data["request_id"])
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "actual"


def test_cancelled_turn_settles_estimated_aborted(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    before = _snap(qa)
    qa.mock_script(stream_script("partial", sleep(4000), "rest"))
    gen = qa.stream_events(cid, "cancel", request_id=rid)
    for _, item in gen:
        if not isinstance(item, httpx.Response) and item.event == "delta":
            break
    gen.close()
    wait_turn_state(qa, cid, rid, {"cancelled"})
    row = turn_row(qa, cid, rid)
    est_in = int(row["reserve_tokens"]) - int(row["max_output_tokens_applied"])
    expected = credits_micro(est_in, int(row["minimal_generation_floor_applied"]), *MINI)
    ev = wait_usage_event(qa, rid)
    assert ev["billing_outcome"] == "aborted" and ev["settlement_method"] == "estimated"
    after = _snap(qa)
    assert _delta(before, after, ("daily", "total"), "reserved_credits_micro") == 0
    assert _delta(before, after, ("daily", "total"), "spent_credits_micro") == expected
    assert _delta(before, after, ("daily", "total"), "calls") == 1


def test_overshoot_capped_at_reserve(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    before = _snap(qa)
    qa.mock_script(stream_script("huge", usage={"input_tokens": 2_000_000, "output_tokens": 1}))
    st, done, _ = send_ok(qa, cid, "overshoot")
    assert done["usage"]["input_tokens"] == 2_000_000
    row = turn_row(qa, cid, st["request_id"])
    assert row["state"] == "completed", "completed stays completed regardless of overshoot"
    after = _snap(qa)
    assert _delta(before, after, ("daily", "total"), "spent_credits_micro") == int(row["reserved_credits_micro"])
    ev = wait_usage_event(qa, st["request_id"])
    assert ev["actual_credits_micro"] == int(row["reserved_credits_micro"])
    assert ev["settlement_method"] == "actual"


# ── usage events ──────────────────────────────────────────────────────────
def test_usage_event_exactly_once_with_contract_fields(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    st, _, _ = send_ok(qa, cid, "usage event please")
    rid = st["request_id"]
    ev = wait_usage_event(qa, rid)
    time.sleep(1.0)
    assert len(usage_events_for(qa, rid)) == 1, "exactly one usage event per turn"
    row = turn_row(qa, cid, rid)
    turn_id = str(uuid.UUID(bytes=row["id"])) if isinstance(row["id"], bytes) else str(row["id"])
    assert ev["tenant_id"] == tenant_id("a1")
    assert ev["user_id"] == user_id("a1")
    assert ev["chat_id"] == cid
    assert ev["turn_id"] == turn_id
    assert ev["request_id"] == rid
    assert ev["effective_model"] == "gpt-4.1-mini" and ev["selected_model"] == "gpt-4.1-mini"
    assert ev["billing_outcome"] == "completed"
    assert ev["settlement_method"] == "actual"
    assert ev["actual_credits_micro"] == 250
    assert ev["usage"]["input_tokens"] == 100 and ev["usage"]["output_tokens"] == 50
    assert ev["policy_version_applied"] == 1
    assert ev["web_search_calls"] == 0 and ev["code_interpreter_calls"] == 0
    assert ev["requester_type"] == "user"
    assert "system_task_type" not in ev
    assert ev["dedupe_key"] == f"{uuid.UUID(tenant_id('a1')).hex}/{uuid.UUID(turn_id).hex}/{uuid.UUID(rid).hex}"
    assert ev.get("timestamp")
    # Published to the policy plugin (the static plugin logs each published event).
    assert wait_until(lambda: ev["dedupe_key"] in qa.server_log(), timeout=15), "usage event was not published"
    # One audit event for the finalized turn.
    assert wait_until(lambda: any("turn_completed" in audit_event_types(p) and rid in str(p) for p in audit_events(qa)), timeout=5)


def test_replay_does_not_publish_usage(qa):
    cid = new_chat(qa, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    send_ok(qa, cid, "once", request_id=rid)
    wait_usage_event(qa, rid)
    before = _snap(qa)
    r, events = qa.stream(cid, "once", request_id=rid)
    assert_ok_stream(r, events)
    time.sleep(1.0)
    assert len(usage_events_for(qa, rid)) == 1
    assert _snap(qa) == before
