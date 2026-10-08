"""Chat CRUD, list (OData), ordering, model immutability, isolation."""

from __future__ import annotations

import time
import uuid

from conftest import TENANT_A, assert_problem, ok_stream, ub

CHAT_RT = "gts.cf.core.mini_chat.chat.v1~"


def test_create_chat_defaults(api, server):
    r = api.post("/chats", json={})
    assert r.status_code == 201, r.text
    body = r.json()
    assert r.headers["location"] == f"/mini-chat/v1/chats/{body['id']}"
    assert body["model"] == "gpt-premium"  # catalog default
    assert "title" not in body
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    assert body["created_at"] and body["updated_at"]
    rows = server.query("SELECT tenant_id, user_id, model, deleted_at FROM chats WHERE id = ?", ub(body["id"]))
    assert len(rows) == 1
    assert rows[0]["tenant_id"] == ub(TENANT_A)
    assert rows[0]["user_id"] == ub(api.user_id)
    assert rows[0]["model"] == "gpt-premium"
    assert rows[0]["deleted_at"] is None


def test_create_chat_with_model_and_title(api):
    body = api.create_chat(model="gpt-standard", title="  My chat  ")
    assert body["model"] == "gpt-standard"
    assert body["title"] == "My chat"


def test_create_chat_invalid_model(api):
    for model in ("no-such-model", "gpt-disabled"):
        r = api.post("/chats", json={"model": model})
        assert_problem(r, 400, category="invalid_argument", reason="INVALID_MODEL", field="model")


def test_create_chat_invalid_title(api):
    for title in ("", "   ", "x" * 256):
        r = api.post("/chats", json={"title": title})
        assert_problem(r, 400, category="invalid_argument", reason="INVALID_TITLE", field="title")
    ok = api.create_chat(title="y" * 255)
    assert len(ok["title"]) == 255


def test_create_chat_title_checked_before_model(api):
    r = api.post("/chats", json={"title": " ", "model": "no-such-model"})
    assert_problem(r, 400, reason="INVALID_TITLE")


def test_get_chat_and_isolation(api, other_user, tenant_b_user):
    chat = api.create_chat(title="mine")
    r = api.get(f"/chats/{chat['id']}")
    assert r.status_code == 200
    assert r.json()["id"] == chat["id"]
    assert r.json()["title"] == "mine"
    for caller in (other_user, tenant_b_user):
        r = caller.get(f"/chats/{chat['id']}")
        assert_problem(r, 404, category="not_found", resource_type=CHAT_RT)
        assert_problem(caller.patch(f"/chats/{chat['id']}", json={"title": "x"}), 404)
        assert_problem(caller.delete(f"/chats/{chat['id']}"), 404)
        assert_problem(caller.get(f"/chats/{chat['id']}/messages"), 404, resource_type=CHAT_RT)
    assert api.get(f"/chats/{uuid.uuid4()}").status_code == 404


def test_invalid_path_param(api):
    r = api.get("/chats/not-a-uuid")
    assert_problem(r, 400)


def test_update_title(api):
    chat = api.create_chat(title="old")
    time.sleep(0.01)
    r = api.patch(f"/chats/{chat['id']}", json={"title": "  new  "})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["title"] == "new"
    assert body["updated_at"] > chat["updated_at"]
    assert api.get(f"/chats/{chat['id']}").json()["title"] == "new"


def test_update_validation(api):
    chat = api.create_chat()
    assert_problem(api.patch(f"/chats/{chat['id']}", json={}), 422)
    assert_problem(api.patch(f"/chats/{chat['id']}", json={"title": None}), 422)
    assert_problem(api.patch(f"/chats/{chat['id']}", json={"title": "  "}), 400, reason="INVALID_TITLE")
    assert_problem(
        api.patch(f"/chats/{chat['id']}", content=b"{bad json", headers={"content-type": "application/json"}), 400
    )
    r = api.patch(f"/chats/{chat['id']}", content=b'{"title":"x"}', headers={"content-type": "text/plain"})
    assert_problem(r, 415)


def test_model_is_immutable(api, server):
    chat = api.create_chat(model="gpt-standard")
    r = api.patch(f"/chats/{chat['id']}", json={"title": "t", "model": "gpt-premium"})
    assert r.status_code == 200, r.text
    assert r.json()["model"] == "gpt-standard"
    rows = server.query("SELECT model FROM chats WHERE id = ?", ub(chat["id"]))
    assert rows[0]["model"] == "gpt-standard"


def test_delete_chat(api, server):
    chat = api.create_chat()
    r = api.delete(f"/chats/{chat['id']}")
    assert r.status_code == 204
    assert_problem(api.get(f"/chats/{chat['id']}"), 404, resource_type=CHAT_RT)
    assert_problem(api.delete(f"/chats/{chat['id']}"), 404)
    rows = server.query("SELECT deleted_at FROM chats WHERE id = ?", ub(chat["id"]))
    assert rows[0]["deleted_at"] is not None
    ids = [c["id"] for c in api.get("/chats").json()["items"]]
    assert chat["id"] not in ids


def test_list_default_order_and_isolation(api, other_user):
    created = [api.create_chat(title=f"c{i}") for i in range(3)]
    other_user.create_chat(title="foreign")
    r = api.get("/chats")
    assert r.status_code == 200
    body = r.json()
    ids = [c["id"] for c in body["items"]]
    assert ids == [c["id"] for c in reversed(created)]
    assert body["page_info"]["limit"] == 20


def test_list_pagination(api):
    created = [api.create_chat(title=f"p{i}") for i in range(5)]
    seen = []
    cursor = None
    pages = 0
    while True:
        params = {"limit": 2}
        if cursor:
            params["cursor"] = cursor
        r = api.get("/chats", params=params)
        assert r.status_code == 200, r.text
        body = r.json()
        assert len(body["items"]) <= 2
        seen.extend(c["id"] for c in body["items"])
        cursor = body["page_info"].get("next_cursor")
        pages += 1
        if not cursor:
            break
        assert pages < 10
    assert seen == [c["id"] for c in reversed(created)]
    assert pages == 3


def test_list_limit_validation(api):
    assert_problem(api.get("/chats", params={"limit": 0}), 400, reason="INVALID_LIMIT")
    r = api.get("/chats", params={"limit": 1000})
    assert r.status_code == 200
    assert r.json()["page_info"]["limit"] == 100
    assert api.get("/chats", params={"limit": "abc"}).status_code == 400
    assert api.get("/chats", params={"cursor": "garbage!!"}).status_code == 400


def test_list_filter_and_orderby(api):
    a = api.create_chat(title="alpha")
    b = api.create_chat(title="beta")
    api.create_chat()
    r = api.get("/chats", params={"$filter": "title eq 'alpha'"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [a["id"]]
    r = api.get("/chats", params={"$filter": f"id eq {b['id']}"})
    assert [c["id"] for c in r.json()["items"]] == [b["id"]]
    r = api.get("/chats", params={"$filter": f"updated_at ge {a['updated_at']}"})
    assert {c["id"] for c in r.json()["items"]} >= {a["id"], b["id"]}
    r = api.get("/chats", params={"$orderby": "updated_at asc"})
    items = r.json()["items"]
    assert items[0]["id"] == a["id"]
    r = api.get("/chats", params={"$orderby": "title desc", "$filter": "title ne null"})
    assert r.status_code == 200, r.text
    titles = [c["title"] for c in r.json()["items"]]
    assert titles == sorted(titles, reverse=True)


def test_list_invalid_odata(api):
    for params in (
        {"$filter": "title eq"},
        {"$filter": "nosuchfield eq 1"},
        {"$orderby": "nosuchfield asc"},
        {"$filter": "model eq 'gpt-premium'"},
    ):
        r = api.get("/chats", params=params)
        assert_problem(r, 400)


def test_ordering_reflects_recent_activity(api, server):
    first = api.create_chat(title="first")
    second = api.create_chat(title="second")
    ids = [c["id"] for c in api.get("/chats").json()["items"]]
    assert ids[:2] == [second["id"], first["id"]]
    ok_stream(api.send(first["id"], "bump"))
    ids = [c["id"] for c in api.get("/chats").json()["items"]]
    assert ids[:2] == [first["id"], second["id"]]
    detail = api.get(f"/chats/{first['id']}").json()
    assert detail["updated_at"] > first["updated_at"]
    # rename bumps as well
    api.patch(f"/chats/{second['id']}", json={"title": "renamed"})
    ids = [c["id"] for c in api.get("/chats").json()["items"]]
    assert ids[0] == second["id"]


def test_message_count(api):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "one"))
    ok_stream(api.send(chat["id"], "two"))
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 4
    listed = next(c for c in api.get("/chats").json()["items"] if c["id"] == chat["id"])
    assert listed["message_count"] == 4


def test_unauthenticated(server):
    anon = server.anonymous()
    assert anon.get("/chats").status_code == 401
    assert anon.post("/chats", json={}).status_code == 401


def test_list_filter_contains_title(api):
    a = api.create_chat(title="Project Phoenix notes")
    api.create_chat(title="groceries")
    r = api.get("/chats", params={"$filter": "contains(title, 'Phoenix')"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [a["id"]]
