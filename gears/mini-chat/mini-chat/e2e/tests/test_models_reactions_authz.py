"""Models API, reactions API, authorization and tenant isolation."""

import uuid

from conftest import reason, violations

INTERNAL_MODEL_KEYS = (
    "provider",
    "provider_id",
    "provider_model_id",
    "provider_display_name",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "credits_micro",
    "policy_version",
    "max_output_tokens",
    "is_default",
    "enabled",
    "system_prompt",
    "estimation_budgets",
    "general_config",
)


def test_models_list_only_enabled_without_internals(api):
    r = api.get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert ids == ["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-mini-tiny-ctx"]
    assert "disabled-model" not in ids
    m = items[0]
    assert m == {
        "model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "tier": "premium",
        "multiplier_display": "3x",
        "description": "Most capable model",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128000,
    }
    for item in items:
        for k in INTERNAL_MODEL_KEYS:
            assert k not in item
    assert "description" not in items[2]  # absent when not configured


def test_get_model(api):
    r = api.get("/models/gpt-4.1-mini")
    assert r.status_code == 200 and r.json()["tier"] == "standard"
    for mid in ("disabled-model", "nope"):
        r = api.get(f"/models/{mid}")
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


def test_reactions(api):
    chat = api.create_chat()
    s = api.send(chat["id"], "hi")
    user_msg, asst = api.messages(chat["id"])
    path = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"
    r = api.put(path, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like" and body["created_at"]
    assert api.put(path, json={"reaction": "like"}).status_code == 200  # idempotent
    assert api.messages(chat["id"])[1]["my_reaction"] == "like"
    r = api.put(path, json={"reaction": "dislike"})
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    assert api.messages(chat["id"])[1]["my_reaction"] == "dislike"
    assert api.messages(chat["id"])[0]["my_reaction"] is None
    assert api.delete(path).status_code == 204
    assert api.delete(path).status_code == 204
    assert api.messages(chat["id"])[1]["my_reaction"] is None

    # invalid value -> 400, schema mismatch -> 422
    r = api.put(path, json={"reaction": "love"})
    assert r.status_code == 400 and reason(r.json()) == "INVALID_REACTION"
    assert api.put(path, json={}).status_code == 422
    # user message -> failed precondition
    upath = f"/chats/{chat['id']}/messages/{user_msg['id']}/reaction"
    for r in (api.put(upath, json={"reaction": "like"}), api.delete(upath)):
        assert r.status_code == 400
        v = violations(r.json())[0]
        assert v["subject"] == "reaction_target" and v["type"] == "STATE"
    # unknown message
    r = api.put(f"/chats/{chat['id']}/messages/{uuid.uuid4()}/reaction", json={"reaction": "like"})
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.message.v1~"
    # unknown chat
    r = api.put(f"/chats/{uuid.uuid4()}/messages/{asst['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"


def test_owner_and_tenant_isolation(server):
    a = server.client("user-a")
    b = server.client("user-b")
    x = server.client("user-x")  # other tenant
    chat = a.create_chat(title="private")
    a.send(chat["id"], "secret")
    asst = a.messages(chat["id"])[1]
    rid = asst["request_id"]
    for other in (b, x):
        assert other.get(f"/chats/{chat['id']}").status_code == 404
        assert other.patch(f"/chats/{chat['id']}", json={"title": "pwn"}).status_code == 404
        assert other.get(f"/chats/{chat['id']}/messages").status_code == 404
        s = other.send(chat["id"], "inject")
        assert s.status == 404
        assert other.put(f"/chats/{chat['id']}/messages/{asst['id']}/reaction", json={"reaction": "like"}).status_code == 404
        assert other.delete(f"/chats/{chat['id']}/messages/{asst['id']}/reaction").status_code == 404
        assert other.turn(chat["id"], rid).status_code == 404
        assert other.delete(f"/chats/{chat['id']}").status_code == 404
        ids = [c["id"] for c in other.get("/chats?limit=100").json()["items"]]
        assert chat["id"] not in ids
    assert a.get(f"/chats/{chat['id']}").json()["title"] == "private"
    assert len(a.messages(chat["id"])) == 2
    # quota status is per user
    xq = x.quota()
    for tier in xq["tiers"]:
        for p in tier["periods"]:
            assert p["used_credits_micro"] == 0


def test_unauthenticated(server):
    anon = server.client(None)
    for method, path in (("GET", "/chats"), ("POST", "/chats"), ("GET", "/models"), ("GET", "/quota/status")):
        r = anon.req(method, path)
        assert r.status_code == 401
    bad = server.client("not-a-token")
    assert bad.get("/chats").status_code == 401


def test_nil_tenant_is_denied(server):
    nil = server.client("user-nil")
    for method, path, body in (
        ("GET", "/chats", None),
        ("POST", "/chats", {}),
        ("GET", f"/chats/{uuid.uuid4()}", None),
        ("GET", "/quota/status", None),
    ):
        r = nil.req(method, path, json=body) if body is not None else nil.req(method, path)
        assert r.status_code == 403, (path, r.text)
        assert r.json()["context"]["reason"] == "AUTHZ_DENIED"
