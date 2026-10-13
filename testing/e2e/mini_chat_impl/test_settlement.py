"""Settlement & finalization (acceptance: Settlement & Finalization; Quota Enforcement: credits and tokens)."""

import time

import pytest

from mchelpers import BackgroundStream, credits_micro, hex32, nonce


def _delta(after, before, key, bucket="total", period="daily"):
    return after.get((bucket, period), {}).get(key, 0) - before.get((bucket, period), {}).get(key, 0)


def _warm(api, model):
    c = api.create_chat(model=model)
    assert api.stream(c["id"], "warm " + nonce()).done


# Acceptance: Settlement — completed standard turn settles actual credits once and releases the reserve
def test_actual_settlement_standard(api_for, db):
    a = api_for("tok-q8")
    _warm(a, "std")
    before = db.quota_snapshot("tok-q8")
    c = a.create_chat(model="std")
    s = a.stream(c["id"], "settle [[usage:100:50]] " + nonce())
    assert s.done["usage"] == {"input_tokens": 100, "output_tokens": 50}
    after = db.quota_snapshot("tok-q8")
    expected = credits_micro(100, 50, "std")
    assert expected == 250
    for period in ("daily", "monthly"):
        assert _delta(after, before, "spent_credits_micro", period=period) == expected
        assert _delta(after, before, "reserved_credits_micro", period=period) == 0
        assert _delta(after, before, "calls", period=period) == 1
        assert _delta(after, before, "input_tokens", period=period) == 100
        assert _delta(after, before, "output_tokens", period=period) == 50
    # a standard turn does not touch the premium bucket
    assert _delta(after, before, "spent_credits_micro", bucket="tier:premium") == 0
    assert _delta(after, before, "calls", bucket="tier:premium") == 0


# Acceptance: Settlement / Credits per model and tier — premium turn charges both buckets, telemetry only on total
def test_actual_settlement_premium(api_for, db):
    a = api_for("tok-q8")
    _warm(a, "prem")
    before = db.quota_snapshot("tok-q8")
    c = a.create_chat(model="prem")
    assert a.stream(c["id"], "settle prem [[usage:1234:567]] " + nonce()).done
    after = db.quota_snapshot("tok-q8")
    expected = credits_micro(1234, 567, "prem")
    assert expected == 1234 * 3 + 567 * 15
    for bucket in ("total", "tier:premium"):
        for period in ("daily", "monthly"):
            assert _delta(after, before, "spent_credits_micro", bucket, period) == expected
            assert _delta(after, before, "reserved_credits_micro", bucket, period) == 0
            assert _delta(after, before, "calls", bucket, period) == 1
    assert _delta(after, before, "input_tokens", "total") == 1234
    assert _delta(after, before, "output_tokens", "total") == 567
    assert _delta(after, before, "input_tokens", "tier:premium") == 0
    assert _delta(after, before, "output_tokens", "tier:premium") == 0


# Acceptance: Settlement — per-component ceil rounding (DESIGN §5.3)
def test_credit_rounding_per_component(api_for, db):
    a = api_for("tok-q9")
    _warm(a, "std")
    before = db.quota_snapshot("tok-q9")
    c = a.create_chat(model="std")
    # in=1 tok * 1_000_000 / 1e6 = 1 ; out=1 tok * 3_000_000 / 1e6 = 3
    assert a.stream(c["id"], "round [[usage:1:1]] " + nonce()).done
    after = db.quota_snapshot("tok-q9")
    assert _delta(after, before, "spent_credits_micro") == 4


# Acceptance: Settlement — failed turn without usage settles estimated (estimated_input + floor) exactly once
def test_failed_turn_estimated_settlement(api_for, db):
    a = api_for("tok-q9")
    _warm(a, "std")
    before = db.quota_snapshot("tok-q9")
    c = a.create_chat(model="std")
    s = a.stream(c["id"], "fail [[fail]] " + nonce())
    assert s.error["code"] == "provider_error"
    a.wait_turn_state(c["id"], s.request_id, {"error"})
    row = db.turn_row(c["id"], s.request_id)
    est_in = row["reserve_tokens"] - row["max_output_tokens_applied"]
    floor = row["minimal_generation_floor_applied"]
    expected = credits_micro(est_in, floor, "std")
    after = db.quota_snapshot("tok-q9")
    for period in ("daily", "monthly"):
        assert _delta(after, before, "spent_credits_micro", period=period) == expected
        assert _delta(after, before, "reserved_credits_micro", period=period) == 0
        assert _delta(after, before, "calls", period=period) == 1
        # token telemetry only on actual settlements
        assert _delta(after, before, "input_tokens", period=period) == 0


# Acceptance: Settlement — cancelled turn settles estimated, reserve released, aborted usage event
@pytest.mark.timeout(60)
def test_cancelled_turn_estimated_settlement(api_for, db):
    a = api_for("tok-q10")
    _warm(a, "std")
    before = db.quota_snapshot("tok-q10")
    c = a.create_chat(model="std")
    bg = BackgroundStream.send(a, c["id"], "slow [[slow]] " + nonce()).wait_started().wait_delta()
    rid = bg.request_id
    bg.stop()
    a.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=30)
    row = db.turn_row(c["id"], rid)
    expected = credits_micro(row["reserve_tokens"] - row["max_output_tokens_applied"], row["minimal_generation_floor_applied"], "std")
    after = db.quota_snapshot("tok-q10")
    for period in ("daily", "monthly"):
        assert _delta(after, before, "spent_credits_micro", period=period) == expected
        assert _delta(after, before, "reserved_credits_micro", period=period) == 0
        assert _delta(after, before, "calls", period=period) == 1
    # exactly one usage message for the turn (tolerant: delivered rows may be vacuumed)
    payloads = db.outbox_payloads(hex32(rid), "billing_outcome")
    assert len(payloads) <= 1
    for p in payloads:
        assert "aborted" in p.lower() and "estimated" in p.lower(), p


# Acceptance: Settlement — usage published reliably, once per completed turn
def test_usage_event_enqueued_once(api_for, db):
    a = api_for("tok-q10")
    c = a.create_chat(model="std")
    s = a.stream(c["id"], "publish " + nonce())
    assert s.done
    payloads = db.outbox_payloads(hex32(s.request_id), "billing_outcome")
    assert len(payloads) <= 1, payloads
    for p in payloads:
        assert "completed" in p.lower() and "actual" in p.lower()
        # dedupe key = {tenant_hex}/{turn_hex}/{request_hex}
        assert hex32(s.request_id) in p


# Acceptance: Settlement / Web Search — completed web search calls are accounted on the total bucket
def test_web_search_calls_accounted(api_for, db):
    a = api_for("tok-q12")
    _warm(a, "prem")
    before = db.quota_snapshot("tok-q12")
    c = a.create_chat(model="prem")
    s = a.stream(c["id"], "search once [[websearch:1]] " + nonce(), web_search=True)
    assert s.done
    after = db.quota_snapshot("tok-q12")
    for period in ("daily", "monthly"):
        assert _delta(after, before, "web_search_calls", period=period) == 1
    assert _delta(after, before, "web_search_calls", bucket="tier:premium") == 0
    assert db.turn_row(c["id"], s.request_id)["web_search_completed_count"] == 1


# Acceptance: Settlement — web search per-turn limit: failed + estimated settlement
def test_web_search_limit_estimated_settlement(api_for, db):
    a = api_for("tok-q12")
    _warm(a, "prem")
    before = db.quota_snapshot("tok-q12")
    c = a.create_chat(model="prem")
    s = a.stream(c["id"], "search a lot [[websearch:3]] " + nonce(), web_search=True)
    assert s.error["code"] == "web_search_calls_exceeded"
    a.wait_turn_state(c["id"], s.request_id, {"error"})
    row = db.turn_row(c["id"], s.request_id)
    expected = credits_micro(row["reserve_tokens"] - row["max_output_tokens_applied"], row["minimal_generation_floor_applied"], "prem")
    after = db.quota_snapshot("tok-q12")
    assert _delta(after, before, "spent_credits_micro") == expected
    assert _delta(after, before, "reserved_credits_micro") == 0
    assert _delta(after, before, "calls") == 1


# Acceptance: Settlement — a pre-stream rejection (no reserve) does not touch quota rows
def test_rejected_request_does_not_settle(api_for, db):
    a = api_for("tok-q9")
    before = db.quota_snapshot("tok-q9")
    c = a.create_chat(model="std")
    s = a.stream(c["id"], "   ")
    assert s.status_code == 400
    time.sleep(0.3)
    assert db.quota_snapshot("tok-q9") == before
