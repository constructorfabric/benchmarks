"""Chat CRUD, list filtering/ordering/pagination, activity ordering, model immutability."""

from __future__ import annotations

import time
import uuid

from harness import problem_reason


def test_create_get_update_delete_lifecycle(env):
    c = env.a
    r = c.post("/chats", json={"title": "  First chat  ", "model": "gpt-4.1-mini"})
    assert r.status_code == 201, r.text
    chat = r.json()
    assert r.headers["location"].endswith(f"/mini-chat/v1/chats/{chat['id']}")
    assert chat["title"] == "First chat"
    assert chat["model"] == "gpt-4.1-mini"
    assert chat["is_temporary"] is False
    assert chat["message_count"] == 0
    assert set(chat) == {"id", "model", "title", "is_temporary", "message_count", "created_at", "updated_at"}

    got = c.get(f"/chats/{chat['id']}")
    assert got.status_code == 200
    assert got.json()["id"] == chat["id"]

    upd = c.patch(f"/chats/{chat['id']}", json={"title": "Renamed"})
    assert upd.status_code == 200, upd.text
    assert upd.json()["title"] == "Renamed"
    assert upd.json()["updated_at"] >= chat["updated_at"]

    d = c.delete(f"/chats/{chat['id']}")
    assert d.status_code == 204
    assert d.content == b""
    assert c.get(f"/chats/{chat['id']}").status_code == 404
    again = c.delete(f"/chats/{chat['id']}")
    assert again.status_code == 404
    assert again.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"
    ids = [x["id"] for x in c.get("/chats", params={"limit": 100}).json()["items"]]
    assert chat["id"] not in ids


def test_create_defaults_untitled_and_default_model(env):
    chat = env.a.create_chat()
    assert "title" not in chat  # omitted when there is no title
    assert chat["model"] == "premium-1"  # is_default catalog entry


def test_create_validation(env):
    c = env.a
    for title in ["", "   ", "x" * 256]:
        r = c.post("/chats", json={"title": title})
        assert r.status_code == 400, r.text
        fv = r.json()["context"]["field_violations"][0]
        assert (fv["field"], fv["reason"]) == ("title", "INVALID_TITLE")
    assert c.post("/chats", json={"title": "y" * 255}).status_code == 201
    for model in ["no-such-model", "disabled-1"]:
        r = c.post("/chats", json={"model": model})
        assert r.status_code == 400
        fv = r.json()["context"]["field_violations"][0]
        assert (fv["field"], fv["reason"]) == ("model", "INVALID_MODEL")
    r = c.post("/chats", content=b"{not json", headers={"content-type": "application/json"})
    assert r.status_code == 400
    assert problem_reason(r.json()) == "json_syntax_error"
    r = c.post("/chats", json={"title": 5})
    assert r.status_code == 422
    assert problem_reason(r.json()) == "invalid_json_body"


def test_update_validation_and_model_is_immutable(env):
    c = env.a
    chat = c.create_chat(title="t", model="gpt-4.1-mini")
    r = c.patch(f"/chats/{chat['id']}", json={"title": "   "})
    assert r.status_code == 400
    assert problem_reason(r.json()) == "INVALID_TITLE"
    r = c.patch(f"/chats/{chat['id']}", json={})
    assert r.status_code == 422
    # Unknown fields (including `model`) are ignored: the model never changes.
    r = c.patch(f"/chats/{chat['id']}", json={"title": "new", "model": "premium-1"})
    assert r.status_code == 200
    assert r.json()["model"] == "gpt-4.1-mini"
    assert c.get(f"/chats/{chat['id']}").json()["model"] == "gpt-4.1-mini"
    assert c.patch(f"/chats/{uuid.uuid4()}", json={"title": "x"}).status_code == 404
    r = c.get("/chats/not-a-uuid")
    assert r.status_code == 400
    assert problem_reason(r.json()) == "invalid_path_params"


def test_list_filter_order_pagination(make_env):
    e = make_env({})
    c = e.a
    created = []
    for i in range(5):
        created.append(c.create_chat(title=f"page-{i}"))
        time.sleep(0.01)
    c.create_chat(title="other")
    # default order: updated_at desc
    items = c.get("/chats").json()["items"]
    assert [x["title"] for x in items[:5]] == ["other", "page-4", "page-3", "page-2", "page-1"]
    # pagination with cursor
    p1 = c.get("/chats", params={"limit": 2}).json()
    assert len(p1["items"]) == 2 and p1["page_info"]["limit"] == 2
    cur = p1["page_info"]["next_cursor"]
    assert cur
    p2 = c.get("/chats", params={"limit": 2, "cursor": cur}).json()
    assert [x["title"] for x in p2["items"]] == ["page-3", "page-2"]
    seen = [x["id"] for x in p1["items"] + p2["items"]]
    assert len(set(seen)) == 4
    # filtering
    r = c.get("/chats", params={"$filter": "startswith(title,'page-')"})
    assert r.status_code == 200, r.text
    assert sorted(x["title"] for x in r.json()["items"]) == [f"page-{i}" for i in range(5)]
    r = c.get("/chats", params={"$filter": f"id eq {created[2]['id']}"})
    assert [x["title"] for x in r.json()["items"]] == ["page-2"]
    ref = created[2]["updated_at"]
    r = c.get("/chats", params={"$filter": f"updated_at gt {ref} and startswith(title,'page-')"})
    assert sorted(x["title"] for x in r.json()["items"]) == ["page-3", "page-4"]
    r = c.get("/chats", params={"$filter": f"updated_at le {ref} and startswith(title,'page-')"})
    assert sorted(x["title"] for x in r.json()["items"]) == ["page-0", "page-1", "page-2"]
    r = c.get("/chats", params={"$filter": f"updated_at eq {ref}"})
    assert [x["title"] for x in r.json()["items"]] == ["page-2"]
    # ordering
    r = c.get("/chats", params={"$orderby": "title asc"})
    titles = [x["title"] for x in r.json()["items"]]
    assert titles == sorted(titles)
    # limit is clamped to 100
    r = c.get("/chats", params={"limit": 1000})
    assert r.status_code == 200
    assert r.json()["page_info"]["limit"] == 100
    # malformed input
    for params in (
        {"limit": 0},
        {"cursor": "garbage"},
        {"$filter": "nonsense eq"},
        {"$filter": "model eq 'x'"},
        {"$orderby": "model asc"},
        {"$skip": "1"},
    ):
        r = c.get("/chats", params=params)
        assert r.status_code == 400, (params, r.text)
        assert r.json()["type"].endswith("invalid_argument.v1~")


def test_ordering_reflects_most_recent_activity(env):
    c = env.a
    older = c.create_chat(title="older-activity")
    time.sleep(0.01)
    newer = c.create_chat(title="newer-activity")
    items = c.get("/chats", params={"limit": 100}).json()["items"]
    ids = [x["id"] for x in items]
    assert ids.index(newer["id"]) < ids.index(older["id"])
    r = c.send(older["id"], "bump")
    assert r.names[-1] == "done"
    items = c.get("/chats", params={"limit": 100}).json()["items"]
    ids = [x["id"] for x in items]
    assert ids.index(older["id"]) < ids.index(newer["id"])
    assert c.get(f"/chats/{older['id']}").json()["updated_at"] > older["updated_at"]
