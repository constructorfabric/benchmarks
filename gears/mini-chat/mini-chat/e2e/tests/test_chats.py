"""Chat CRUD, list query/pagination, ordering, model lock."""

import time
import uuid

from conftest import reason, violations


def test_create_get_update_delete_lifecycle(api):
    r = api.post("/chats", json={"title": "  My chat  "})
    assert r.status_code == 201
    chat = r.json()
    assert r.headers["location"] == f"/mini-chat/v1/chats/{chat['id']}"
    assert chat["title"] == "My chat"
    assert chat["model"] == "gpt-4.1"  # first enabled is_default model
    assert chat["is_temporary"] is False
    assert chat["message_count"] == 0
    assert "user_id" not in chat and "tenant_id" not in chat
    assert chat["created_at"].endswith("Z")

    got = api.get(f"/chats/{chat['id']}").json()
    assert got == chat

    r = api.patch(f"/chats/{chat['id']}", json={"title": "Renamed", "model": "gpt-4.1-mini"})
    assert r.status_code == 200
    upd = r.json()
    assert upd["title"] == "Renamed"
    assert upd["model"] == "gpt-4.1"  # model is immutable
    assert upd["updated_at"] >= chat["updated_at"]

    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    assert api.get(f"/chats/{chat['id']}").status_code == 404
    assert api.delete(f"/chats/{chat['id']}").status_code == 404
    assert api.patch(f"/chats/{chat['id']}", json={"title": "x"}).status_code == 404


def test_untitled_chat_omits_title(api):
    chat = api.create_chat(title=None)
    assert "title" not in chat
    assert "title" not in api.get(f"/chats/{chat['id']}").json()


def test_create_with_explicit_model(api):
    chat = api.create_chat(model="gpt-4.1-mini")
    assert chat["model"] == "gpt-4.1-mini"


def test_create_rejects_unknown_and_disabled_model(api):
    for model in ("nope", "disabled-model"):
        r = api.post("/chats", json={"model": model})
        assert r.status_code == 400, r.text
        v = violations(r.json())[0]
        assert v["field"] == "model" and v["reason"] == "INVALID_MODEL"


def test_title_validation(api):
    for bad in ("", "   ", "x" * 256):
        r = api.post("/chats", json={"title": bad})
        assert r.status_code == 400, bad
        assert violations(r.json())[0]["reason"] == "INVALID_TITLE"
    assert api.post("/chats", json={"title": "x" * 255}).status_code == 201
    chat = api.create_chat(title="ok")
    for bad in ("", "  ", "y" * 256):
        r = api.patch(f"/chats/{chat['id']}", json={"title": bad})
        assert r.status_code == 400
        assert reason(r.json()) == "INVALID_TITLE"
    # schema mismatch -> 422, malformed JSON -> 400, no content type -> 415
    assert api.patch(f"/chats/{chat['id']}", json={}).status_code == 422
    assert api.patch(f"/chats/{chat['id']}", json={"title": None}).status_code == 422
    r = api.patch(f"/chats/{chat['id']}", content=b"{bad", headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    r = api.patch(f"/chats/{chat['id']}", content=b'{"title":"a"}', headers={"Content-Type": "text/plain"})
    assert r.status_code == 415


def test_invalid_path_param(api):
    r = api.get("/chats/not-a-uuid")
    assert r.status_code == 400


def test_list_ordering_reflects_activity(api, mock):
    a = api.create_chat(title="order-a")
    time.sleep(0.01)
    b = api.create_chat(title="order-b")
    ids = [c["id"] for c in api.get("/chats?limit=100").json()["items"]]
    assert ids.index(b["id"]) < ids.index(a["id"])
    # sending a message bumps updated_at
    s = api.send(a["id"], "bump")
    assert s.terminal[0] == "done"
    ids = [c["id"] for c in api.get("/chats?limit=100").json()["items"]]
    assert ids.index(a["id"]) < ids.index(b["id"])
    got = api.get(f"/chats/{a['id']}").json()
    assert got["message_count"] == 2
    # rename bumps too
    api.patch(f"/chats/{b['id']}", json={"title": "order-b2"})
    ids = [c["id"] for c in api.get("/chats?limit=100").json()["items"]]
    assert ids.index(b["id"]) < ids.index(a["id"])


def test_list_pagination_filter_orderby(server):
    api = server.client("user-c")
    tag = uuid.uuid4().hex[:8]
    made = [api.create_chat(title=f"{tag}-{i}") for i in range(5)]
    seen, cursor = [], None
    while True:
        q = f"?limit=2&$filter=startswith(title,'{tag}')" + (f"&cursor={cursor}" if cursor else "")
        r = api.get("/chats" + q)
        assert r.status_code == 200, r.text
        page = r.json()
        assert page["page_info"]["limit"] == 2
        seen += [c["id"] for c in page["items"]]
        cursor = page["page_info"].get("next_cursor")
        if not cursor:
            break
    assert seen == [c["id"] for c in reversed(made)]

    r = api.get(f"/chats?$filter=title eq '{tag}-3'")
    assert [c["id"] for c in r.json()["items"]] == [made[3]["id"]]
    r = api.get(f"/chats?$filter=startswith(title,'{tag}')&$orderby=title asc")
    assert [c["title"] for c in r.json()["items"]] == [f"{tag}-{i}" for i in range(5)]
    # limit > 100 clamps
    r = api.get("/chats?limit=1000")
    assert r.status_code == 200 and r.json()["page_info"]["limit"] == 100


def test_list_rejects_malformed_query(api):
    cases = {
        "?limit=0": "INVALID_LIMIT",
        "?$filter=bogus eq 1": "INVALID_FILTER",
        "?$filter=title eq": "INVALID_FILTER",
        "?$orderby=bogus asc": "INVALID_ORDERBY_FIELD",
        "?cursor=garbage": "INVALID_CURSOR",
        "?$skip=3": "UNSUPPORTED_QUERY_PARAM",
    }
    for q, expected in cases.items():
        r = api.get("/chats" + q)
        assert r.status_code == 400, (q, r.text)
        body = r.json()
        assert body["context"]["resource_type"] == "gts.cf.core.odata.query.v1~", (q, body)
        assert reason(body) == expected, (q, body)


def test_deleted_chats_not_listed(api):
    c = api.create_chat(title="to-delete")
    api.delete(f"/chats/{c['id']}")
    ids = [x["id"] for x in api.get("/chats?limit=100").json()["items"]]
    assert c["id"] not in ids
