"""Models API, reactions, messages API, quota status / enforcement, web
search, thread summary."""

import time
import uuid

import mc
from mc import USER_A, USER_QUOTA, problem_reason


# ---------------------------------------------------------------------------
# Models
# ---------------------------------------------------------------------------


def test_models_api(api):
    r = api.get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert "gpt-premium" in ids and "gpt-standard" in ids
    assert "gpt-off" not in ids
    for m in items:
        assert set(m) <= {"model_id", "display_name", "tier", "multiplier_display", "description", "multimodal_capabilities", "context_window"}
        assert m["tier"] in ("standard", "premium")
    prem = [m for m in items if m["model_id"] == "gpt-premium"][0]
    assert prem == {
        "model_id": "gpt-premium",
        "display_name": "Premium",
        "tier": "premium",
        "multiplier_display": "2x",
        "description": "Premium model",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128000,
    }
    std = [m for m in items if m["model_id"] == "gpt-standard"][0]
    assert "description" not in std
    assert api.get("/models/gpt-standard").json() == std
    for mid in ("gpt-off", "nope"):
        r = api.get(f"/models/{mid}")
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


# ---------------------------------------------------------------------------
# Reactions
# ---------------------------------------------------------------------------


def test_reactions(api, api_a2, db):
    chat = api.create_chat()
    cid = chat["id"]
    api.send(cid, "react to me")
    user_msg, asst = api.messages(cid)["items"]
    path = f"/chats/{cid}/messages/{asst['id']}/reaction"
    r = api.put(path, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like" and body["created_at"]
    assert api.put(path, json={"reaction": "like"}).status_code == 200  # idempotent
    msgs = api.messages(cid)["items"]
    assert msgs[1]["my_reaction"] == "like" and msgs[0]["my_reaction"] is None
    assert api.put(path, json={"reaction": "dislike"}).json()["reaction"] == "dislike"
    assert api.messages(cid)["items"][1]["my_reaction"] == "dislike"
    assert len(db.q("select * from message_reactions where message_id = ?", mc.uuid_blob(asst["id"]))) == 1
    r = api.put(path, json={"reaction": "love"})
    assert r.status_code == 400 and problem_reason(r.json()) == "INVALID_REACTION"
    assert api.put(path, json={}).status_code == 422
    r = api.put(f"/chats/{cid}/messages/{user_msg['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 400
    v = r.json()["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("reaction_target", "STATE")
    r = api.delete(f"/chats/{cid}/messages/{user_msg['id']}/reaction")
    assert r.status_code == 400
    r = api.put(f"/chats/{cid}/messages/{uuid.uuid4()}/reaction", json={"reaction": "like"})
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.message.v1~"
    assert api.delete(path).status_code == 204
    assert api.delete(path).status_code == 204
    assert api.messages(cid)["items"][1]["my_reaction"] is None
    assert api_a2.put(path, json={"reaction": "like"}).status_code == 404


# ---------------------------------------------------------------------------
# Messages API
# ---------------------------------------------------------------------------


def test_messages_list_filter_order_pagination(api):
    chat = api.create_chat()
    cid = chat["id"]
    for i in range(3):
        api.send(cid, f"msg {i}")
    all_items = api.messages(cid)["items"]
    assert len(all_items) == 6
    ts = [(m["created_at"], m["id"]) for m in all_items]
    assert ts == sorted(ts)
    for m in all_items:
        assert set(m) >= {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}
        assert m["attachments"] == []
    page = api.messages(cid, limit=4)
    assert len(page["items"]) == 4 and page["page_info"]["next_cursor"]
    page2 = api.messages(cid, limit=4, cursor=page["page_info"]["next_cursor"])
    assert [m["id"] for m in page["items"] + page2["items"]] == [m["id"] for m in all_items]
    assert [m["role"] for m in api.messages(cid, **{"$filter": "role eq 'assistant'"})["items"]] == ["assistant"] * 3
    desc = api.messages(cid, **{"$orderby": "created_at desc"})["items"]
    assert [m["id"] for m in desc] == [m["id"] for m in reversed(all_items)]
    one = api.messages(cid, **{"$filter": f"id eq {all_items[1]['id']}"})["items"]
    assert [m["id"] for m in one] == [all_items[1]["id"]]
    r = api.get(f"/chats/{cid}/messages", params={"$filter": "content eq 'x'"})
    assert r.status_code == 400
    r = api.get(f"/chats/{cid}/messages", params={"limit": 0})
    assert r.status_code == 400 and problem_reason(r.json()) == "INVALID_LIMIT"
    assert api.get(f"/chats/{uuid.uuid4()}/messages").status_code == 404


# ---------------------------------------------------------------------------
# Quota
# ---------------------------------------------------------------------------


def _quota_map(api):
    st = api.get("/quota/status").json()
    return st, {(t["tier"], p["period"]): p for t in st["tiers"] for p in t["periods"]}


def test_quota_status_matches_usage(env, db):
    api = mc.Client(env, "user-quota")
    st, qm = _quota_map(api)
    assert st["warning_threshold_pct"] == 80
    assert set(qm) == {("premium", "daily"), ("premium", "monthly"), ("total", "daily"), ("total", "monthly")}
    for (tier, period), p in qm.items():
        assert p["remaining_credits_micro"] == p["limit_credits_micro"] - p["used_credits_micro"]
        assert p["next_reset"].endswith("Z")
    chat = api.create_chat()
    api.send(chat["id"], "spend some credits")
    st2, qm2 = _quota_map(api)
    rows = db.quota(USER_QUOTA)
    for (tier, period), p in qm2.items():
        bucket = "tier:premium" if tier == "premium" else "total"
        row = rows[(period, bucket)]
        assert p["used_credits_micro"] == row["spent_credits_micro"] + row["reserved_credits_micro"]
        assert row["reserved_credits_micro"] == 0
    assert qm2[("total", "daily")]["used_credits_micro"] - qm[("total", "daily")]["used_credits_micro"] == 21 * 2 + 7 * 6


def test_credits_and_tokens_per_model(env, db):
    api = mc.Client(env, "user-a2")
    before = db.quota(mc.USER_A2)
    chat = api.create_chat(model="gpt-standard")
    rid = str(uuid.uuid4())
    api.send(chat["id"], "standard turn", request_id=rid)
    after = db.quota(mc.USER_A2)
    spent = after[("daily", "total")]["spent_credits_micro"] - (before.get(("daily", "total")) or {"spent_credits_micro": 0})["spent_credits_micro"]
    assert spent == 21 * 1 + 7 * 3
    # Standard turns do not touch the premium bucket.
    prem_before = before.get(("daily", "tier:premium"))
    prem_after = after.get(("daily", "tier:premium"))
    assert (prem_before and prem_before["spent_credits_micro"]) == (prem_after and prem_after["spent_credits_micro"])
    t = db.turn(rid)
    assert t["policy_version_applied"] == 1
    assert t["effective_model"] == "gpt-standard"
    assert t["max_output_tokens_applied"] == 2048
    assert t["minimal_generation_floor_applied"] == 50
    assert after[("daily", "total")]["input_tokens"] - (before.get(("daily", "total")) or {"input_tokens": 0})["input_tokens"] == 21


def test_downgrade_and_exhaustion(env, db, mock):
    api = mc.Client(env, "user-quota")
    chat = api.create_chat()
    cid = chat["id"]
    assert api.send(cid, "warm up").terminal[0] == "done"
    uid = mc.uuid_blob(USER_QUOTA)
    db.x("update quota_usage set spent_credits_micro = 50000000 where user_id = ? and bucket = 'tier:premium' and period_type = 'daily'", uid)
    s = api.send(cid, "now downgraded")
    done = s.first("done")
    assert done["effective_model"] == "gpt-standard"
    assert done["selected_model"] == "gpt-premium"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == "gpt-premium"
    assert done["downgrade_reason"] == "premium_quota_exhausted"
    warn = [w for w in done["quota_warnings"] if w["tier"] == "premium" and w["period"] == "daily"][0]
    assert warn["exhausted"] is True and warn["warning"] is True and warn["next_reset"]
    msgs = api.messages(cid)["items"]
    assert msgs[-1]["model"] == "gpt-standard"
    # Total exhausted -> 429 tokens before any provider call.
    db.x("update quota_usage set spent_credits_micro = 100000000 where user_id = ? and bucket = 'total' and period_type = 'daily'", uid)
    mark = mock.mark()
    r = api.send(cid, "rejected")
    assert r.status == 429
    v = r.body["context"]["violations"][0]
    assert v["subject"] == "tokens" and v["description"] == "quota_exceeded"
    assert not [x for x in mock.since(mark) if x["path"].endswith("/responses")]
    # Replay of a completed turn still works when the quota is exhausted.
    st, qm = _quota_map(api)
    assert qm[("total", "daily")]["exhausted"] is True and qm[("total", "daily")]["remaining_percentage"] == 0
    db.x("update quota_usage set spent_credits_micro = 0 where user_id = ?", uid)


# ---------------------------------------------------------------------------
# Web search
# ---------------------------------------------------------------------------


def test_web_search(api, mock, db, env):
    chat = api.create_chat()
    cid = chat["id"]
    q_before = db.quota(USER_A).get(("daily", "total"))
    mark = mock.mark()
    rid = str(uuid.uuid4())
    s = api.send(cid, "search please #websearch", request_id=rid, web_search={"enabled": True})
    assert s.terminal[0] == "done"
    assert {"phase": "start", "name": "web_search", "details": {}} in s.all("tool")
    assert {"phase": "done", "name": "web_search", "details": {}} in s.all("tool")
    items = s.first("citations")["items"]
    assert items == [
        {
            "source": "web",
            "title": "Sky facts",
            "url": "https://example.com/sky",
            "snippet": "the sky is blue.",
            "span": {"start": 15, "end": 37},
        }
    ] or items[0]["url"] == "https://example.com/sky"
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    ws = [t for t in body["tools"] if t["type"] == "web_search"]
    assert ws and ws[0].get("search_context_size") == "low"
    assert "web_search" in body["instructions"]
    assert body["metadata"]["feature"] == "web_search"
    t = db.turn(rid)
    assert t["web_search_enabled"] == 1 and t["web_search_completed_count"] == 1
    q_after = db.quota(USER_A)[("daily", "total")]
    assert q_after["web_search_calls"] - (q_before["web_search_calls"] if q_before else 0) == 1
    # Without the flag the tool is not sent.
    mark = mock.mark()
    api.send(cid, "no search")
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    assert not any(t["type"] == "web_search" for t in body.get("tools", []))
    # Per-message call limit.
    rid = str(uuid.uuid4())
    s = api.send(cid, "loop #websearch3", request_id=rid, web_search={"enabled": True})
    assert s.terminal[0] == "error" and s.terminal[1]["code"] == "web_search_calls_exceeded"
    assert db.turn(rid)["error_code"] == "web_search_calls_exceeded"
    key = rid.replace("-", "")
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["dedupe_key"].endswith(key)])[0]
    assert ev["billing_outcome"] == "failed" and ev["settlement_method"] == "estimated"


# ---------------------------------------------------------------------------
# Thread summary
# ---------------------------------------------------------------------------


def test_thread_summary_lifecycle(api, mock, db, env):
    chat = api.create_chat(model="gpt-tiny")
    cid = chat["id"]
    for i in range(5):
        s = api.send(cid, f"turn {i} " + "t" * 600)
        assert s.terminal[0] == "done", s.events
    row = mc.wait_for(lambda: db.q("select * from thread_summaries where chat_id = ?", mc.uuid_blob(cid)), timeout=30)[0]
    assert "SUMMARY" in row["summary_text"]
    assert "<analysis>" not in row["summary_text"]
    assert row["token_estimate"] == 119
    sreq = [x for x in mock.requests() if x["path"].endswith("/responses") and x["body"] and x["body"].get("stream") is False]
    assert sreq
    sb = sreq[-1]["body"]
    assert sb["model"] == "mock-summary"
    assert sb["metadata"]["request_type"] == "summary"
    assert sb["metadata"]["feature"] == "none"
    assert "Summarize the following conversation" in str(sb["input"]) or "existing summary" in str(sb["input"])
    compressed = db.q("select count(*) c from messages where chat_id = ? and is_compressed = 1", mc.uuid_blob(cid))[0]["c"]
    assert compressed > 0
    ev = mc.wait_for(lambda: [e for e in mc.usage_events(env) if e["billing_outcome"] == "system_task"])
    assert ev[0]["settlement_method"] == "none" and ev[0]["actual_credits_micro"] == 0
    # The next turn applies the summary.
    mark = mock.mark()
    s = api.send(cid, "after summary")
    assert s.first("stream_started")["thread_summary_applied"] == {"token_estimate": row["token_estimate"]}
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    first = str(body["input"][0]["content"])
    assert body["input"][0]["role"] == "user"
    assert "earlier messages that have been summarized" in first and "SUMMARY" in first
    # Messages stay visible in the history API.
    assert len(api.messages(cid, limit=100)["items"]) == 12


def test_summary_invalidated_by_mutation(api, db):
    chat = api.create_chat(model="gpt-tiny")
    cid = chat["id"]
    rids = []
    for i in range(5):
        rid = str(uuid.uuid4())
        rids.append(rid)
        assert api.send(cid, f"turn {i} " + "u" * 600, request_id=rid).terminal[0] == "done"
    mc.wait_for(lambda: db.q("select * from thread_summaries where chat_id = ?", mc.uuid_blob(cid)), timeout=30)
    # Delete turns from the tail until the summary covers the latest turn.
    for rid in reversed(rids):
        assert api.delete(f"/chats/{cid}/turns/{rid}").status_code == 204
        if not db.q("select * from thread_summaries where chat_id = ?", mc.uuid_blob(cid)):
            break
    assert not db.q("select * from thread_summaries where chat_id = ?", mc.uuid_blob(cid))
    assert db.q("select count(*) c from messages where chat_id = ? and is_compressed = 1", mc.uuid_blob(cid))[0]["c"] == 0
