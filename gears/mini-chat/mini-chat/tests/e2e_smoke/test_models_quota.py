"""Models API, quota status, quota accounting and outbox publication.

DESIGN §3.3 Models API, §3.2 Quota Status Endpoint, §5.4 settlement, §5.6/5.7 outbox.
"""

import datetime as dt
import time
import uuid

from conftest import MODEL_RT, assert_problem, wait_until

MODEL_KEYS = {"model_id", "display_name", "tier", "multiplier_display", "description",
              "multimodal_capabilities", "context_window"}
FORBIDDEN_MODEL_KEYS = {"provider", "provider_id", "provider_model_id", "is_default", "credits_micro",
                        "input_tokens_credit_multiplier_micro", "output_tokens_credit_multiplier_micro",
                        "policy_version", "max_output_tokens", "enabled", "preference"}


# ------------------------------------------------------------------ models
def test_list_models(api):
    r = api.get("/models")
    assert r.status_code == 200, r.text
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert set(ids) == {"gpt-4.1", "gpt-4.1-mini", "text-only"}
    assert "disabled-model" not in ids
    by_id = {m["model_id"]: m for m in items}
    assert by_id["gpt-4.1"]["tier"] == "premium"
    assert by_id["gpt-4.1-mini"]["tier"] == "standard"
    assert by_id["gpt-4.1"]["display_name"] == "GPT-4.1"
    assert by_id["gpt-4.1"]["context_window"] == 128000
    assert by_id["gpt-4.1"]["multimodal_capabilities"] == ["VISION_INPUT"]
    assert by_id["text-only"]["multimodal_capabilities"] == []
    assert by_id["gpt-4.1"]["multiplier_display"] == "1x"
    for m in items:
        assert set(m) <= MODEL_KEYS, m
        assert not (set(m) & FORBIDDEN_MODEL_KEYS), m


def test_get_model(api):
    listed = {m["model_id"]: m for m in api.get("/models").json()["items"]}
    r = api.get("/models/gpt-4.1-mini")
    assert r.status_code == 200
    assert r.json() == listed["gpt-4.1-mini"]
    for mid in ("disabled-model", "nope"):
        assert_problem(api.get(f"/models/{mid}"), 404, "not_found", resource_type=MODEL_RT)


# ------------------------------------------------------------------ quota status
def quota(api):
    r = api.get("/quota/status")
    assert r.status_code == 200, r.text
    return r.json()


def period(q, tier, name):
    t = next(t for t in q["tiers"] if t["tier"] == tier)
    return next(p for p in t["periods"] if p["period"] == name)


def test_quota_status_shape(api):
    q = quota(api)
    assert q["warning_threshold_pct"] == 80
    assert [t["tier"] for t in q["tiers"]] == ["premium", "total"]
    now = dt.datetime.now(dt.timezone.utc)
    tomorrow = (now + dt.timedelta(days=1)).strftime("%Y-%m-%dT00:00:00Z")
    nm = (now.replace(day=1) + dt.timedelta(days=32)).replace(day=1).strftime("%Y-%m-%dT00:00:00Z")
    limits = {("premium", "daily"): 50_000_000, ("premium", "monthly"): 500_000_000,
              ("total", "daily"): 100_000_000, ("total", "monthly"): 1_000_000_000}
    for t in q["tiers"]:
        assert [p["period"] for p in t["periods"]] == ["daily", "monthly"]
        for p in t["periods"]:
            assert set(p) == {"period", "limit_credits_micro", "used_credits_micro", "remaining_credits_micro",
                              "remaining_percentage", "next_reset", "warning", "exhausted"}
            assert p["limit_credits_micro"] == limits[(t["tier"], p["period"])]
            assert p["remaining_credits_micro"] == p["limit_credits_micro"] - p["used_credits_micro"]
            assert p["next_reset"] == (tomorrow if p["period"] == "daily" else nm)
            assert p["warning"] is False and p["exhausted"] is False


def test_quota_usage_after_turn(api, db):
    before = quota(api)
    chat = api.create_chat(title="quota-premium")
    res = api.turn(chat["id"], "count me")
    rid = res.started["request_id"]
    # gpt-4.1: 120 input * 1.0 + 30 output * 3.0 = 210 credits (micro units of the multipliers).
    expected = 120 * 1 + 30 * 3
    after = quota(api)
    for tier in ("premium", "total"):
        for name in ("daily", "monthly"):
            delta = period(after, tier, name)["used_credits_micro"] - period(before, tier, name)["used_credits_micro"]
            assert delta == expected, (tier, name, delta)
    turn = db.turn(chat["id"], rid)
    assert turn["state"] == "completed" and turn["effective_model"] == "gpt-4.1"
    rows = db.quota_rows()
    assert all(r["reserved_credits_micro"] == 0 for r in rows), rows
    buckets = {(r["period_type"], r["bucket"]) for r in rows}
    assert {("daily", "total"), ("monthly", "total"), ("daily", "tier:premium"),
            ("monthly", "tier:premium")} <= buckets


def test_standard_model_does_not_touch_premium(api, db):
    before = quota(api)
    chat = api.create_chat(model="gpt-4.1-mini")
    res = api.turn(chat["id"], "standard")
    assert res.terminal["effective_model"] == "gpt-4.1-mini"
    assert res.terminal["selected_model"] == "gpt-4.1-mini"
    after = quota(api)
    assert period(after, "premium", "daily")["used_credits_micro"] == period(before, "premium", "daily")["used_credits_micro"]
    assert period(after, "total", "daily")["used_credits_micro"] - period(before, "total", "daily")["used_credits_micro"] == 210


def test_failed_turn_settles_and_releases_reserve(api, db):
    chat = api.create_chat()
    res = api.stream(chat["id"], "fail [[500]]")
    assert res.terminal_name == "error"
    wait_until(lambda: all(r["reserved_credits_micro"] == 0 for r in db.quota_rows()), timeout=10,
               desc="reserve released")


# ------------------------------------------------------------------ outbox
def _outbox_processed(db, body_id):
    out = db.query("SELECT partition_id, seq FROM toolkit_outbox_outgoing WHERE body_id = ?", (body_id,))
    inc = db.query("SELECT id FROM toolkit_outbox_incoming WHERE body_id = ?", (body_id,))
    if inc:
        return False
    if not out:
        return True  # vacuumed after processing
    p = out[0]
    proc = db.query("SELECT processed_seq FROM toolkit_outbox_processor WHERE partition_id = ?", (p["partition_id"],))
    return bool(proc) and proc[0]["processed_seq"] >= p["seq"]


def test_outbox_usage_and_audit_events(api, db):
    chat = api.create_chat()
    res = api.turn(chat["id"], "publish me")
    rid = res.started["request_id"]

    def find():
        usage = [b for b in db.outbox_bodies("mini_chat.usage_event.v1") if b["json"]["request_id"] == rid]
        audit = [b for b in db.outbox_bodies("mini_chat.audit_event.v1") if b["json"].get("request_id") == rid]
        return (usage, audit) if usage and audit else None

    usage, audit = wait_until(find, timeout=10, desc="outbox rows")
    assert len(usage) == 1
    u = usage[0]["json"]
    assert u["chat_id"] == chat["id"]
    assert u["billing_outcome"] == "completed" and u["settlement_method"] == "actual"
    assert u["terminal_state"] == "completed"
    assert u["actual_credits_micro"] == 210
    assert u["usage"]["input_tokens"] == 120 and u["usage"]["output_tokens"] == 30
    assert u["effective_model"] == "gpt-4.1" and u["selected_model"] == "gpt-4.1"
    assert u["dedupe_key"]
    assert [a["json"]["event_type"] for a in audit] == ["turn_completed"]
    for b in usage + audit:
        wait_until(lambda b=b: _outbox_processed(db, b["id"]), timeout=15, desc=f"outbox body {b['id']} processed")
    assert db.query("SELECT * FROM toolkit_outbox_dead_letters") == []


def test_outbox_failed_turn_and_mutation_audit(api, db):
    chat = api.create_chat()
    failed = api.stream(chat["id"], "x [[error]]")
    rid = failed.started["request_id"]
    usage = wait_until(lambda: [b for b in db.outbox_bodies("mini_chat.usage_event.v1")
                                if b["json"]["request_id"] == rid], timeout=10, desc="usage event")
    assert usage[0]["json"]["billing_outcome"] == "failed"
    audit = [b["json"]["event_type"] for b in db.outbox_bodies("mini_chat.audit_event.v1")
             if b["json"].get("request_id") == rid]
    assert audit == ["turn_failed"]

    ok = api.turn(chat["id"], "fine")
    ok_rid = ok.started["request_id"]
    r = api.delete(f"/chats/{chat['id']}/turns/{ok_rid}")
    assert r.status_code == 204, r.text

    def mutation_audit():
        return [b["json"] for b in db.outbox_bodies("mini_chat.audit_event.v1")
                if "turn_delete" in (b["json"].get("event_type"), b["json"].get("event_kind"))
                and b["json"].get("chat_id") == chat["id"]]

    events = wait_until(mutation_audit, timeout=10, desc="turn_delete audit")
    ev = events[0]
    # DESIGN §3.9 "Audit Events for Turn Mutations": actor_user_id, chat_id, request_id, timestamp.
    assert ev["request_id"] == ok_rid
    assert ev["actor_user_id"] == "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    assert ev["timestamp"]
