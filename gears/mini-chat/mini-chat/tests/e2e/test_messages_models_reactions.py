"""Messages API, Models API, Reactions API."""

from __future__ import annotations

import uuid

from harness import problem_reason


def test_list_messages_contract_filter_order_pagination(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    rids = [c.send(chat["id"], f"m{i}").started["request_id"] for i in range(3)]
    assert c.get(f"/chats/{chat['id']}").json()["message_count"] == 6
    items = c.messages(chat["id"])
    assert [m["content"] for m in items] == ["m0", "Echo: m0", "m1", "Echo: m1", "m2", "Echo: m2"]
    created = [m["created_at"] for m in items]
    assert created == sorted(created)
    for m in items:
        assert {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"} <= set(m)
        assert m["attachments"] == [] and m["my_reaction"] is None
        uuid.UUID(m["request_id"])
    assert [m["request_id"] for m in items[::2]] == rids
    # filtering by role, id and created_at
    r = c.get(f"/chats/{chat['id']}/messages", params={"$filter": "role eq 'assistant'"})
    assert [m["role"] for m in r.json()["items"]] == ["assistant"] * 3
    target = items[3]["id"]
    r = c.get(f"/chats/{chat['id']}/messages", params={"$filter": f"id eq {target}"})
    assert [m["id"] for m in r.json()["items"]] == [target]
    r = c.get(f"/chats/{chat['id']}/messages", params={"$filter": f"created_at gt {items[3]['created_at']}"})
    assert [m["content"] for m in r.json()["items"]] == ["m2", "Echo: m2"]
    # ordering
    r = c.get(f"/chats/{chat['id']}/messages", params={"$orderby": "created_at desc"})
    assert [m["content"] for m in r.json()["items"]][0] == "Echo: m2"
    # pagination
    p1 = c.get(f"/chats/{chat['id']}/messages", params={"limit": 4}).json()
    assert len(p1["items"]) == 4
    p2 = c.get(f"/chats/{chat['id']}/messages", params={"limit": 4, "cursor": p1["page_info"]["next_cursor"]}).json()
    assert [m["content"] for m in p2["items"]] == ["m2", "Echo: m2"]
    assert not p2["page_info"].get("next_cursor")
    # malformed input
    for params in ({"$filter": "content eq 'x'"}, {"limit": 0}, {"cursor": "zzz"}, {"$orderby": "bogus"}):
        r = c.get(f"/chats/{chat['id']}/messages", params=params)
        assert r.status_code == 400, params
    assert c.get(f"/chats/{uuid.uuid4()}/messages").status_code == 404


def test_models_api(env):
    c = env.a
    r = c.get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert "disabled-1" not in ids and set(ids) == {"premium-1", "gpt-4.1-mini", "std-novision", "tiny-ctx"}
    for m in items:
        assert set(m) <= {"model_id", "display_name", "tier", "multiplier_display", "description", "multimodal_capabilities", "context_window"}
        assert m["tier"] in ("standard", "premium")
    prem = [m for m in items if m["model_id"] == "premium-1"][0]
    assert prem == {
        "model_id": "premium-1",
        "display_name": "PREMIUM-1",
        "tier": "premium",
        "multiplier_display": "3x",
        "description": "premium-1 model",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128000,
    }
    mini = [m for m in items if m["model_id"] == "gpt-4.1-mini"][0]
    assert "description" not in mini  # empty description omitted
    raw = r.text
    for leak in ("prov-", "provider", "credit_multiplier", "is_default", "policy_version", "max_output"):
        assert leak not in raw
    assert c.get("/models/premium-1").json() == prem
    for missing in ("disabled-1", "nope"):
        r = c.get(f"/models/{missing}")
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


def test_reactions(env):
    c = env.a
    chat = c.create_chat()
    c.send(chat["id"], "hello")
    user_msg, asst = c.messages(chat["id"])
    path = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"
    r = c.put(path, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like" and "created_at" in body
    assert c.messages(chat["id"])[1]["my_reaction"] == "like"
    # idempotent upsert, change of reaction
    assert c.put(path, json={"reaction": "like"}).status_code == 200
    assert c.put(path, json={"reaction": "dislike"}).json()["reaction"] == "dislike"
    assert c.messages(chat["id"])[1]["my_reaction"] == "dislike"
    rows = env.server.query("SELECT count(*) AS n FROM message_reactions")
    assert rows[0]["n"] >= 1
    # validation
    r = c.put(path, json={"reaction": "love"})
    assert r.status_code == 400 and problem_reason(r.json()) == "INVALID_REACTION"
    assert c.put(path, json={}).status_code == 422
    r = c.put(f"/chats/{chat['id']}/messages/{user_msg['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 400
    v = r.json()["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("reaction_target", "STATE")
    r = c.put(f"/chats/{chat['id']}/messages/{uuid.uuid4()}/reaction", json={"reaction": "like"})
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.message.v1~"
    # other users cannot react in my chat
    assert env.a2.put(path, json={"reaction": "like"}).status_code == 404
    # delete (idempotent)
    assert c.delete(path).status_code == 204
    assert c.delete(path).status_code == 204
    assert c.messages(chat["id"])[1]["my_reaction"] is None
