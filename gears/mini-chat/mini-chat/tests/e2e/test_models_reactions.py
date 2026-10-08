"""Models API (read-only catalog view) and reactions."""

from __future__ import annotations

import uuid

from conftest import assert_problem, ok_stream, ub

MODEL_RT = "gts.cf.core.mini_chat.model.v1~"
MSG_RT = "gts.cf.core.mini_chat.message.v1~"
INTERNAL_FIELDS = (
    "provider_model_id",
    "provider_id",
    "system_prompt",
    "thread_summary_prompt",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "estimation_budgets",
    "general_config",
    "enabled",
)


def test_list_models_enabled_only(api):
    r = api.get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert "gpt-disabled" not in ids
    assert {"gpt-premium", "gpt-standard", "gpt-novision", "gpt-tiny", "gpt-4.1-mini"} <= set(ids)
    prem = next(m for m in items if m["model_id"] == "gpt-premium")
    assert prem["display_name"] == "GPT-PREMIUM"
    assert prem["tier"] == "premium"
    assert prem["multiplier_display"] == "1x"
    assert prem["description"] == "Test model gpt-premium"
    assert prem["multimodal_capabilities"] == ["VISION_INPUT"]
    assert prem["context_window"] == 128000
    std = next(m for m in items if m["model_id"] == "gpt-standard")
    assert std["tier"] == "standard"
    for m in items:
        for f in INTERNAL_FIELDS:
            assert f not in m, (f, m)


def test_get_model(api):
    r = api.get("/models/gpt-standard")
    assert r.status_code == 200
    m = r.json()
    assert m["model_id"] == "gpt-standard"
    for f in INTERNAL_FIELDS:
        assert f not in m
    assert_problem(api.get("/models/gpt-disabled"), 404, category="not_found", resource_type=MODEL_RT)
    assert_problem(api.get("/models/nope"), 404, resource_type=MODEL_RT)


def test_models_require_auth(server):
    assert server.anonymous().get("/models").status_code == 401


def _chat_with_answer(api):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "hi"))
    user, asst = api.messages(chat["id"])
    return chat, user, asst


def test_set_and_remove_reaction(api, server):
    chat, user, asst = _chat_with_answer(api)
    path = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"
    r = api.put(path, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"]
    assert body["reaction"] == "like"
    assert body["created_at"]
    assert api.messages(chat["id"])[1]["my_reaction"] == "like"
    # upsert
    r = api.put(path, json={"reaction": "dislike"})
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    rows = server.query("SELECT reaction, user_id FROM message_reactions WHERE message_id = ?", ub(asst["id"]))
    assert len(rows) == 1 and rows[0]["reaction"] == "dislike" and rows[0]["user_id"] == ub(api.user_id)
    # idempotent same value
    assert api.put(path, json={"reaction": "dislike"}).status_code == 200
    assert len(server.query("SELECT id FROM message_reactions WHERE message_id = ?", ub(asst["id"]))) == 1
    # remove (idempotent)
    assert api.delete(path).status_code == 204
    assert api.delete(path).status_code == 204
    assert api.messages(chat["id"])[1]["my_reaction"] is None
    assert server.query("SELECT id FROM message_reactions WHERE message_id = ?", ub(asst["id"])) == []


def test_reaction_validation(api, other_user):
    chat, user, asst = _chat_with_answer(api)
    path = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"
    assert_problem(api.put(path, json={"reaction": "love"}), 400, category="invalid_argument", reason="INVALID_REACTION")
    assert_problem(api.put(path, json={}), 422)
    upath = f"/chats/{chat['id']}/messages/{user['id']}/reaction"
    body = assert_problem(api.put(upath, json={"reaction": "like"}), 400, category="failed_precondition")
    assert body["context"]["violations"][0] == {**body["context"]["violations"][0], "subject": "reaction_target", "type": "STATE"}
    assert_problem(api.delete(upath), 400, category="failed_precondition")
    missing = f"/chats/{chat['id']}/messages/{uuid.uuid4()}/reaction"
    assert_problem(api.put(missing, json={"reaction": "like"}), 404, resource_type=MSG_RT)
    assert_problem(api.delete(missing), 404, resource_type=MSG_RT)
    assert_problem(api.put(f"/chats/{uuid.uuid4()}/messages/{asst['id']}/reaction", json={"reaction": "like"}), 404)
    assert_problem(other_user.put(path, json={"reaction": "like"}), 404)
    assert_problem(other_user.delete(path), 404)


def test_messages_list_odata(api):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "one"))
    ok_stream(api.send(chat["id"], "two"))
    msgs = api.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant", "user", "assistant"]
    assert [m["created_at"] for m in msgs] == sorted(m["created_at"] for m in msgs)
    users = api.messages(chat["id"], **{"$filter": "role eq 'user'"})
    assert [m["content"] for m in users] == ["one", "two"]
    desc = api.messages(chat["id"], **{"$orderby": "created_at desc"})
    assert [m["id"] for m in desc] == [m["id"] for m in reversed(msgs)]
    one = api.messages(chat["id"], **{"$filter": f"id eq {msgs[1]['id']}"})
    assert [m["id"] for m in one] == [msgs[1]["id"]]
    page = api.get(f"/chats/{chat['id']}/messages", params={"limit": 3}).json()
    assert len(page["items"]) == 3
    nxt = api.get(f"/chats/{chat['id']}/messages", params={"limit": 3, "cursor": page["page_info"]["next_cursor"]}).json()
    assert [m["id"] for m in page["items"] + nxt["items"]] == [m["id"] for m in msgs]
    assert api.get(f"/chats/{chat['id']}/messages", params={"limit": 0}).status_code == 400
    assert api.get(f"/chats/{chat['id']}/messages", params={"$filter": "content eq 'x'"}).status_code == 400
    for m in msgs:
        assert set(m) >= {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}
