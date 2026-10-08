"""Quota enforcement: reserve before execute, downgrade cascade, credits
accounting, status API, kill switches, web search / code interpreter quotas."""

import uuid

import pytest

from conftest import reason, start_server, violations, wait_for
from testdata_helpers import XLSX_CT, png


def _policy(cfg):
    return cfg["gears"]["static-mini-chat-model-policy-plugin"]["config"]


def _period(status, tier, period):
    for t in status["tiers"]:
        if t["tier"] == tier:
            for p in t["periods"]:
                if p["period"] == period:
                    return p
    raise KeyError((tier, period))


def _used(status, tier, period="daily"):
    return _period(status, tier, period)["used_credits_micro"]


# ── default limits: accounting on the shared server ────────────────────────


def test_credits_accounting_per_model_and_tier(server):
    api = server.client("user-c")
    before = api.quota()
    chat = api.create_chat()  # premium gpt-4.1: 3 / 15 micro-credits per token
    assert api.send(chat["id"], "MOCK_USAGE=100,20").terminal[0] == "done"
    after = api.quota()
    expected = 100 * 3 + 20 * 15
    for period in ("daily", "monthly"):
        assert _used(after, "premium", period) - _used(before, "premium", period) == expected
        assert _used(after, "total", period) - _used(before, "total", period) == expected
    mini = api.create_chat(model="gpt-4.1-mini")  # standard: 1 / 3
    assert api.send(mini["id"], "MOCK_USAGE=100,20").terminal[0] == "done"
    after2 = api.quota()
    assert _used(after2, "premium") == _used(after, "premium")
    assert _used(after2, "total") - _used(after, "total") == 100 + 60


def test_quota_status_shape(api):
    st = api.quota()
    assert st["warning_threshold_pct"] == 80
    assert [t["tier"] for t in st["tiers"]] == ["premium", "total"]
    p = _period(st, "premium", "daily")
    assert p["limit_credits_micro"] == 50_000_000
    assert p["remaining_credits_micro"] == p["limit_credits_micro"] - p["used_credits_micro"]
    assert set(p) >= {"period", "limit_credits_micro", "used_credits_micro", "remaining_credits_micro", "remaining_percentage", "next_reset", "warning", "exhausted"}
    assert p["next_reset"].endswith("T00:00:00Z")
    assert _period(st, "total", "monthly")["limit_credits_micro"] == 1_000_000_000


def test_failed_turn_settles_estimated(server):
    api = server.client("user-c")
    chat = api.create_chat(model="gpt-4.1-mini")
    before = _used(api.quota(), "total")
    s = api.send(chat["id"], "MOCK_FAILED")
    assert s.terminal[1]["code"] == "provider_error"
    charged = _used(api.quota(), "total") - before
    # estimated = input estimate + minimal generation floor (50) at 1/3 credits
    assert 150 <= charged < 4096 * 3


def test_failed_turn_with_usage_settles_actual(server):
    api = server.client("user-c")
    chat = api.create_chat(model="gpt-4.1-mini")
    before = _used(api.quota(), "total")
    s = api.send(chat["id"], "MOCK_FAILED MOCK_FAILED_USAGE MOCK_USAGE=7,3")
    assert s.terminal[1]["code"] == "provider_error"
    assert _used(api.quota(), "total") - before == 7 + 9


def test_overshoot_is_capped_and_turn_stays_done(server):
    api = server.client("user-c")
    chat = api.create_chat(model="gpt-4.1-mini")
    before = _used(api.quota(), "total")
    s = api.send(chat["id"], "MOCK_USAGE=1000000,1000000")
    assert s.terminal[0] == "done"
    charged = _used(api.quota(), "total") - before
    assert 0 < charged <= 20_000  # capped at the reserve (~4.1k tokens x 3)
    rid = s.first("stream_started")["request_id"]
    assert api.turn(chat["id"], rid).json()["state"] == "done"


def test_usage_and_audit_published_once(server):
    api = server.client("user-c")
    chat = api.create_chat(model="gpt-4.1-mini")
    rid = str(uuid.uuid4())
    assert api.send(chat["id"], "count me", request_id=rid).terminal[0] == "done"
    api.send(chat["id"], "count me", request_id=rid)  # replay: no new events
    wait_for(lambda: rid in server.log_text() and "mini-chat usage event" in server.log_text(), timeout=20, msg="usage event")
    import time

    time.sleep(1.5)
    log = server.log_text()
    usage_lines = [l for l in log.splitlines() if "mini-chat usage event" in l and rid in l]
    audit_lines = [l for l in log.splitlines() if "mini-chat audit event" in l and rid in l and "turn_completed" in l]
    assert len(usage_lines) == 1, usage_lines
    assert len(audit_lines) == 1, audit_lines
    # provider identifiers never reach the audit payload
    assert "resp_" not in audit_lines[0]


# ── tight token limits ─────────────────────────────────────────────────────


@pytest.fixture(scope="module")
def tight(mock):
    def ov(cfg):
        p = _policy(cfg)
        p["default_premium_limits"] = {"limit_daily_credits_micro": 5_000, "limit_monthly_credits_micro": 10_000_000}
        p["default_standard_limits"] = {"limit_daily_credits_micro": 20_000, "limit_monthly_credits_micro": 10_000_000}

    s = start_server("tight", ov)
    yield s
    s.cleanup()


def test_premium_exhausted_downgrades(tight, mock):
    api = tight.client("user-a")
    chat = api.create_chat()  # gpt-4.1 premium; premium reserve > 5000
    s = api.send(chat["id"], "MOCK_USAGE=1000,1000")
    assert s.terminal[0] == "done", s.text
    done = s.first("done")
    assert done["quota_decision"] == "downgrade"
    assert done["selected_model"] == "gpt-4.1"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert done["downgrade_from"] == "gpt-4.1"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    req = mock.chat_requests(chat["id"])[-1]["json"]
    assert req["model"] == "gpt-4.1-mini"
    st = api.quota()
    assert _used(st, "premium") == 0
    assert _used(st, "total") == 1000 + 3000  # standard multipliers
    assert api.messages(chat["id"])[1]["model"] == "gpt-4.1-mini"
    # chat model stays as selected
    assert api.get(f"/chats/{chat['id']}").json()["model"] == "gpt-4.1"
    # replay shows the stored downgrade without the reason
    rid = s.first("stream_started")["request_id"]
    rep = api.send(chat["id"], "x", request_id=rid).first("done")
    assert rep["quota_decision"] == "downgrade" and rep["effective_model"] == "gpt-4.1-mini"
    assert "downgrade_reason" not in rep


def test_total_exhausted_rejects_before_provider(tight, mock):
    api = tight.client("user-b")
    chat = api.create_chat(model="gpt-4.1-mini")
    # standard reserve ~ 4.1k tokens x 3 = ~12.4k credits; limit 20k
    s = api.send(chat["id"], "MOCK_USAGE=2000,2000")  # 2000 + 6000 = 8000
    assert s.terminal[0] == "done"
    n = len(mock.chat_requests(chat["id"]))
    s = api.send(chat["id"], "again")
    assert s.status == 429, s.text
    assert not s.is_sse
    v = s.json["context"]["violations"][0]
    assert v["subject"] == "tokens" and v["description"] == "quota_exceeded"
    assert len(mock.chat_requests(chat["id"])) == n  # no provider call
    assert [m["content"] for m in api.messages(chat["id"]) if m["role"] == "user"] == ["MOCK_USAGE=2000,2000"]
    st = api.quota()
    p = _period(st, "total", "daily")
    assert p["used_credits_micro"] == 8000 and p["remaining_credits_micro"] == 12000
    assert p["remaining_percentage"] == 60 and p["warning"] is False and p["exhausted"] is False
    # retry is rejected by the preflight and leaves the previous turn in place
    rid = api.messages(chat["id"])[0]["request_id"]
    r = api.post(f"/chats/{chat['id']}/turns/{rid}/retry")
    assert r.status_code == 429
    assert api.turn(chat["id"], rid).json()["state"] == "done"


def test_quota_warning_flags(tight):
    api = tight.client("user-c")
    chat = api.create_chat(model="gpt-4.1-mini")
    s = api.send(chat["id"], "MOCK_USAGE=1000,3000")  # 1000 + 9000 = 10000 -> 50% remaining
    assert s.terminal[0] == "done"
    warn = s.first("done")["quota_warnings"]
    total_daily = [w for w in warn if w["tier"] == "total" and w["period"] == "daily"][0]
    assert total_daily["remaining_percentage"] == 50 and total_daily["warning"] is False
    tiny = api.create_chat(model="gpt-4.1-mini-tiny-ctx")  # reserve ~ (input + 1024) x 3
    assert api.send(tiny["id"], "MOCK_USAGE=0,1000").terminal[0] == "done"
    assert _period(api.quota(), "total", "daily")["remaining_percentage"] == 35
    s = api.send(tiny["id"], "MOCK_USAGE=0,1000")
    assert s.terminal[0] == "done", s.text
    p = _period(api.quota(), "total", "daily")
    assert p["used_credits_micro"] == 16000
    assert p["remaining_percentage"] == 20 and p["warning"] is True and p["exhausted"] is False
    w = [w for w in s.first("done")["quota_warnings"] if w["tier"] == "total" and w["period"] == "daily"][0]
    assert w["warning"] is True


# ── policy switches and tool quotas ────────────────────────────────────────


@pytest.fixture(scope="module")
def switches(mock):
    def ov(cfg):
        _policy(cfg)["kill_switches"] = {
            "disable_web_search": True,
            "disable_images": True,
            "force_standard_tier": True,
            "disable_code_interpreter": True,
        }

    s = start_server("switches", ov)
    yield s
    s.cleanup()


def test_kill_switches(switches, mock):
    api = switches.client("user-a")
    chat = api.create_chat()
    s = api.send(chat["id"], "search", web_search={"enabled": True})
    assert s.status == 400
    v = violations(s.json)[0]
    assert v["subject"] == "web_search" and v["type"] == "FEATURE_DISABLED"
    r = api.upload(chat["id"], "p.png", png(8, 8), "image/png")
    assert r.status_code == 400
    v = violations(r.json())[0]
    assert v["subject"] == "images" and v["type"] == "FEATURE_DISABLED"
    r = api.upload(chat["id"], "s.xlsx", b"PK\x03\x04", XLSX_CT)
    assert r.status_code == 400 and reason(r.json()) == "CODE_INTERPRETER_UNAVAILABLE"
    s = api.send(chat["id"], "plain")
    assert s.terminal[0] == "done"
    done = s.first("done")
    assert done["quota_decision"] == "downgrade" and done["downgrade_reason"] == "force_standard_tier"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert mock.chat_requests(chat["id"])[-1]["json"]["model"] == "gpt-4.1-mini"


@pytest.fixture(scope="module")
def toolquota(mock):
    def ov(cfg):
        q = cfg["gears"]["mini-chat"]["config"].setdefault("quota", {})
        q["web_search_daily_quota"] = 2
        q["web_search_max_calls_per_message"] = 2

    s = start_server("toolquota", ov)
    yield s
    s.cleanup()


def test_web_search_limits(toolquota, mock):
    api = toolquota.client("user-a")
    chat = api.create_chat()
    s = api.send(chat["id"], "MOCK_WEB=3 x", web_search={"enabled": True})
    name, err = s.terminal
    assert name == "error" and err["code"] == "web_search_calls_exceeded"
    rid = s.first("stream_started")["request_id"]
    assert api.turn(chat["id"], rid).json()["error_code"] == "web_search_calls_exceeded"
    req = mock.chat_requests(chat["id"])[-1]["json"]
    ws = [t for t in req["tools"] if t["type"] == "web_search"]
    assert ws and req["max_tool_calls"] == 2
    assert "web_search" in req.get("instructions", "")
    # the two completed calls of the failed turn count toward the daily quota (2)
    s = api.send(chat["id"], "MOCK_WEB=1 y", web_search={"enabled": True})
    assert s.status == 429, s.text
    assert s.json["context"]["violations"][0]["subject"] == "web_search"
    # without web search the request is not limited
    assert api.send(chat["id"], "no search").terminal[0] == "done"

    other = toolquota.client("user-b")
    chat = other.create_chat()
    for i in range(2):
        s = other.send(chat["id"], f"MOCK_WEB=1 q{i}", web_search={"enabled": True})
        assert s.terminal[0] == "done", s.text
        assert s.first("citations")["items"][0]["source"] == "web"
    s = other.send(chat["id"], "MOCK_WEB=1 q3", web_search={"enabled": True})
    assert s.status == 429 and s.json["context"]["violations"][0]["subject"] == "web_search"


def test_disabled_model_downgrades(mock):
    srv = start_server("modeldisable")
    try:
        api = srv.client("user-a")
        chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
        srv.stop()
        for m in _policy(srv.cfg)["model_catalog"]:
            if m["id"] == "gpt-4.1-mini-tiny-ctx":
                m["enabled"] = False
        import yaml

        srv.config_path.write_text(yaml.safe_dump(srv.cfg))
        srv.start()
        s = api.send(chat["id"], "hi")
        assert s.terminal[0] == "done", s.text
        done = s.first("done")
        assert done["quota_decision"] == "downgrade" and done["downgrade_reason"] == "model_disabled"
        assert done["selected_model"] == "gpt-4.1-mini-tiny-ctx"
        # removed from the catalog entirely -> INVALID_MODEL
        srv.stop()
        cat = _policy(srv.cfg)["model_catalog"]
        _policy(srv.cfg)["model_catalog"] = [m for m in cat if m["id"] != "gpt-4.1-mini-tiny-ctx"]
        srv.config_path.write_text(yaml.safe_dump(srv.cfg))
        srv.start()
        s = api.send(chat["id"], "hi")
        assert s.status == 400 and reason(s.json) == "INVALID_MODEL"
        r = api.upload(chat["id"], "a.txt", b"x", "text/plain")
        assert r.status_code == 400 and reason(r.json()) == "INVALID_MODEL"
    finally:
        srv.cleanup()
