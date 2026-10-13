"""Config variant `limits`: rag.uploaded_file_max_size_kb=4, kill switches disable_web_search and
disable_images, premium daily limit 5000 credits (below one premium reserve -> downgrade), and the
e2e-local.yaml OAGW proxy timeout of 2 s."""

import uuid

from conftest import assert_problem, make_png, sse_request

VARIANT = "limits"


def test_file_too_large(api, chat, fresh_mock):
    r = api.upload(chat["id"], "big.txt", b"a" * (5 * 1024), "text/plain")
    assert_problem(r, 400, "out_of_range", field="content_length", reason="FILE_TOO_LARGE")
    assert fresh_mock.calls("POST", r"^(/openai)?(/v1)?/files$") == []
    r = api.upload(chat["id"], "ok.txt", b"a" * 4000, "text/plain")
    assert r.status_code == 201, r.text


def test_images_kill_switch(api, chat, fresh_mock):
    r = api.upload(chat["id"], "p.png", make_png(4, 4), "image/png")
    assert_problem(r, 400, "failed_precondition", violation={"subject": "images", "type": "FEATURE_DISABLED"})
    assert fresh_mock.requests() == []


def test_web_search_kill_switch(api, chat, fresh_mock):
    res = api.stream(chat["id"], "search [[web_search]]", web_search={"enabled": True})
    assert res.events == []
    assert_problem(res.problem, 400, "failed_precondition",
                   violation={"subject": "web_search", "type": "FEATURE_DISABLED"})
    assert fresh_mock.responses_calls() == []
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 0
    # Without web search the turn proceeds.
    api.turn(chat["id"], "plain", web_search={"enabled": False})


def test_premium_exhausted_downgrades(api, fresh_mock, db):
    chat = api.create_chat()
    assert chat["model"] == "gpt-4.1"
    rid = str(uuid.uuid4())
    res = api.turn(chat["id"], "downgrade me", request_id=rid)
    done = res.terminal
    assert done["selected_model"] == "gpt-4.1"
    assert done["effective_model"] == "gpt-4.1-mini"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == "gpt-4.1"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    assert fresh_mock.responses_calls()[-1]["body"]["model"] == "gpt-4.1-mini"
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert msgs[1]["model"] == "gpt-4.1-mini"
    assert api.get(f"/chats/{chat['id']}").json()["model"] == "gpt-4.1"  # chat model is locked
    assert db.turn(chat["id"], rid)["effective_model"] == "gpt-4.1-mini"
    premium = [r for r in db.quota_rows() if r["bucket"] == "tier:premium"]
    assert all(r["spent_credits_micro"] == 0 and r["reserved_credits_micro"] == 0 for r in premium), premium
    # Replay rebuilds quota_decision / downgrade_from, omits downgrade_reason.
    replay = api.stream(chat["id"], "x", request_id=rid)
    rd = replay.terminal
    assert replay.started["is_new_turn"] is False
    assert rd["quota_decision"] == "downgrade" and rd["downgrade_from"] == "gpt-4.1"
    assert rd["effective_model"] == "gpt-4.1-mini" and "downgrade_reason" not in rd
    # Retry goes through the same preflight.
    rr = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{rid}/retry")
    assert rr.terminal_name == "done" and rr.terminal["quota_decision"] == "downgrade"


def test_quota_status_premium_limit(api):
    q = api.get("/quota/status").json()
    prem = next(t for t in q["tiers"] if t["tier"] == "premium")
    assert next(p for p in prem["periods"] if p["period"] == "daily")["limit_credits_micro"] == 5000


def test_provider_idle_timeout_mid_stream(api, chat):
    """OAGW proxy timeout (2 s) elapses while the provider is silent mid-stream. DESIGN 'Streaming
    error codes': `provider_timeout` = gateway timeout; `provider_error` = provider stream failed.
    Either is accepted (see README known discrepancies); the turn must end `failed`."""
    res = api.stream(chat["id"], "stall [[stall]]")
    assert res.status == 200
    assert res.terminal_name == "error", res.events
    assert res.terminal["code"] in ("provider_timeout", "provider_error"), res.terminal
    st = api.get(f"/chats/{chat['id']}/turns/{res.started['request_id']}").json()
    assert st["state"] == "error" and st["error_code"] == res.terminal["code"]
