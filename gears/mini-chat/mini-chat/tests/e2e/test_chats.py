"""Chat CRUD, list (OData), ordering, model immutability, isolation."""

import time
import uuid

from mc import problem_reason


def test_create_get_update_delete_lifecycle(api):
    r = api.post("/chats", json={"title": "  First  ", "model": "gpt-standard"})
    assert r.status_code == 201, r.text
    chat = r.json()
    assert r.headers["location"] == f"/mini-chat/v1/chats/{chat['id']}"
    assert chat["title"] == "First"
    assert chat["model"] == "gpt-standard"
    assert chat["is_temporary"] is False
    assert chat["message_count"] == 0
    assert set(chat) == {"id", "model", "title", "is_temporary", "message_count", "created_at", "updated_at"}

    got = api.get(f"/chats/{chat['id']}").json()
    assert got == chat

    r = api.patch(f"/chats/{chat['id']}", json={"title": "Renamed", "model": "gpt-premium"})
    assert r.status_code == 200, r.text
    upd = r.json()
    assert upd["title"] == "Renamed"
    assert upd["model"] == "gpt-standard", "model is immutable"
    assert upd["updated_at"] >= chat["updated_at"]

    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    r = api.get(f"/chats/{chat['id']}")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"
    assert api.delete(f"/chats/{chat['id']}").status_code == 404


def test_create_defaults_and_untitled(api):
    r = api.post("/chats", json={})
    assert r.status_code == 201
    chat = r.json()
    assert chat["model"] == "gpt-premium"  # is_default
    assert "title" not in chat  # omitted, not null


def test_title_and_model_validation(api):
    for title in ["", "   ", "x" * 256]:
        r = api.post("/chats", json={"title": title})
        assert r.status_code == 400, title
        assert problem_reason(r.json()) == "INVALID_TITLE"
    assert api.post("/chats", json={"title": "y" * 255}).status_code == 201

    for model in ["no-such-model", "gpt-off"]:
        r = api.post("/chats", json={"model": model})
        assert r.status_code == 400
        assert problem_reason(r.json()) == "INVALID_MODEL"

    chat = api.create_chat()
    for title in ["", "  ", "z" * 256]:
        r = api.patch(f"/chats/{chat['id']}", json={"title": title})
        assert r.status_code == 400
        assert problem_reason(r.json()) == "INVALID_TITLE"
    # Body without title / null title -> 422.
    assert api.patch(f"/chats/{chat['id']}", json={}).status_code == 422
    assert api.patch(f"/chats/{chat['id']}", json={"title": None}).status_code == 422
    # Malformed JSON -> 400, missing content type -> 415.
    r = api.post("/chats", content=b"{bad", headers={"content-type": "application/json"})
    assert r.status_code == 400
    r = api.post("/chats", content=b"{}")
    assert r.status_code == 415
    # Non-UUID path parameter.
    r = api.get("/chats/not-a-uuid")
    assert r.status_code == 400
    assert problem_reason(r.json()) == "invalid_path_params"


def test_list_pagination_filter_order(api_a2):
    api = api_a2
    ids = [api.create_chat(title=f"page-{i}")["id"] for i in range(5)]
    r = api.get("/chats", params={"limit": 2})
    assert r.status_code == 200
    page = r.json()
    assert page["page_info"]["limit"] == 2
    assert len(page["items"]) == 2
    # Default order: most recently updated first.
    assert page["items"][0]["id"] == ids[-1]
    seen = [c["id"] for c in page["items"]]
    cursor = page["page_info"]["next_cursor"]
    while cursor:
        page = api.get("/chats", params={"limit": 2, "cursor": cursor}).json()
        seen += [c["id"] for c in page["items"]]
        cursor = page["page_info"]["next_cursor"]
    assert set(ids) <= set(seen)
    assert len(seen) == len(set(seen))

    r = api.get("/chats", params={"$filter": "title eq 'page-3'"})
    assert r.status_code == 200
    assert [c["id"] for c in r.json()["items"]] == [ids[3]]

    r = api.get("/chats", params={"$orderby": "title asc", "limit": 100})
    titles = [c.get("title") for c in r.json()["items"]]
    assert titles == sorted(titles, key=lambda t: (t is None, t))

    # Clamp limit above 100.
    assert api.get("/chats", params={"limit": 1000}).json()["page_info"]["limit"] == 100

    for params, reason in [
        ({"limit": 0}, "INVALID_LIMIT"),
        ({"$filter": "nope eq 1"}, None),
        ({"$filter": "title eq"}, None),
        ({"$orderby": "model asc"}, None),
        ({"cursor": "garbage"}, None),
        ({"$skip": "1"}, None),
    ]:
        r = api.get("/chats", params=params)
        assert r.status_code == 400, (params, r.text)
        assert r.json()["context"]["resource_type"] == "gts.cf.core.odata.query.v1~"
        if reason:
            assert problem_reason(r.json()) == reason


def test_ordering_reflects_activity(api):
    a = api.create_chat(title="older")
    time.sleep(0.01)
    b = api.create_chat(title="newer")
    items = api.get("/chats", params={"limit": 100}).json()["items"]
    order = [c["id"] for c in items]
    assert order.index(b["id"]) < order.index(a["id"])
    s = api.send(a["id"], "bump")
    assert s.terminal[0] == "done"
    items = api.get("/chats", params={"limit": 100}).json()["items"]
    order = [c["id"] for c in items]
    assert order.index(a["id"]) < order.index(b["id"])
    chat = api.get(f"/chats/{a['id']}").json()
    assert chat["message_count"] == 2


def test_owner_and_tenant_isolation(api, api_a2, api_b):
    chat = api.create_chat(title="private")
    cid = chat["id"]
    rid = str(uuid.uuid4())
    assert api.send(cid, "hi", request_id=rid).terminal[0] == "done"
    msg_id = api.messages(cid)["items"][1]["id"]
    for other in (api_a2, api_b):
        assert other.get(f"/chats/{cid}").status_code == 404
        assert other.patch(f"/chats/{cid}", json={"title": "x"}).status_code == 404
        assert other.delete(f"/chats/{cid}").status_code == 404
        assert other.get(f"/chats/{cid}/messages").status_code == 404
        assert other.get(f"/chats/{cid}/turns/{rid}").status_code == 404
        assert other.post(f"/chats/{cid}/turns/{rid}/retry").status_code == 404
        assert other.delete(f"/chats/{cid}/turns/{rid}").status_code == 404
        assert other.put(f"/chats/{cid}/messages/{msg_id}/reaction", json={"reaction": "like"}).status_code == 404
        r = other.send(cid, "intrude")
        assert r.status == 404
        assert other.upload(cid, "a.txt", b"hello").status_code == 404
        ids = [c["id"] for c in other.get("/chats", params={"limit": 100}).json()["items"]]
        assert cid not in ids
    # The owner still sees everything.
    assert api.get(f"/chats/{cid}").status_code == 200
    # A user of another tenant works with its own chats normally.
    own = api_b.create_chat()
    assert api_b.send(own["id"], "tenant b").terminal[0] == "done"
    assert api.get(f"/chats/{own['id']}").status_code == 404


def test_unauthenticated(env):
    import httpx

    r = httpx.get(f"{env.base}/chats")
    assert r.status_code == 401
    r = httpx.get(f"{env.base}/chats", headers={"Authorization": "Bearer nope"})
    assert r.status_code == 401
