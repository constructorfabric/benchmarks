"""Chat CRUD, list OData, ordering by activity, model immutability."""

import time
import uuid

from harness import Client, parse_sse


def field_reason(problem):
    return [(v.get("field"), v.get("reason")) for v in problem["context"].get("field_violations", [])]


def test_create_defaults_and_location(client):
    r = client.req("POST", "/v1/chats", json={})
    assert r.status_code == 201
    body = r.json()
    assert body["model"] == "gpt-premium"  # first enabled is_default model
    assert "title" not in body  # omitted when null
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    assert r.headers["location"] == f"/mini-chat/v1/chats/{body['id']}"
    assert "user_id" not in body and "tenant_id" not in body
    assert body["created_at"].endswith("Z")


def test_create_with_title_and_model(client):
    c = client.create_chat(title="  My chat  ", model="gpt-standard")
    assert c["title"] == "My chat"
    assert c["model"] == "gpt-standard"


def test_create_null_title(client):
    r = client.req("POST", "/v1/chats", json={"title": None})
    assert r.status_code == 201
    assert "title" not in r.json()


def test_create_invalid_model(client):
    for m in ("nope", "gpt-disabled"):
        r = client.req("POST", "/v1/chats", json={"model": m})
        assert r.status_code == 400, r.text
        assert ("model", "INVALID_MODEL") in field_reason(r.json())


def test_create_invalid_title(client):
    for t in ("", "   ", "x" * 256):
        r = client.req("POST", "/v1/chats", json={"title": t})
        assert r.status_code == 400
        assert ("title", "INVALID_TITLE") in field_reason(r.json())
    # 255 characters is fine
    r = client.req("POST", "/v1/chats", json={"title": "y" * 255})
    assert r.status_code == 201


def test_title_validated_before_model(client):
    r = client.req("POST", "/v1/chats", json={"title": " ", "model": "nope"})
    assert ("title", "INVALID_TITLE") in field_reason(r.json())


def test_body_errors(client):
    r = client.req("POST", "/v1/chats", content=b"{bad json", headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    assert ("body", "json_syntax_error") in field_reason(r.json())
    r = client.req("POST", "/v1/chats", content=b"{}", headers={"Content-Type": "text/plain"})
    assert r.status_code == 415
    r = client.req("POST", "/v1/chats", json={"title": 5})
    assert r.status_code == 422
    assert ("body", "invalid_json_body") in field_reason(r.json())


def test_get_update_delete_lifecycle(client):
    c = client.create_chat(title="a", model="gpt-standard")
    r = client.req("GET", f"/v1/chats/{c['id']}")
    assert r.status_code == 200 and r.json()["title"] == "a"
    time.sleep(0.01)
    r = client.req("PATCH", f"/v1/chats/{c['id']}", json={"title": " Renamed ", "model": "gpt-premium"})
    assert r.status_code == 200
    u = r.json()
    assert u["title"] == "Renamed"
    assert u["model"] == "gpt-standard"  # model is immutable; unknown field ignored
    assert u["updated_at"] > c["updated_at"]
    r = client.req("PATCH", f"/v1/chats/{c['id']}", json={"title": None})
    assert r.status_code == 422
    r = client.req("PATCH", f"/v1/chats/{c['id']}", json={})
    assert r.status_code == 422
    r = client.req("PATCH", f"/v1/chats/{c['id']}", json={"title": "  "})
    assert r.status_code == 400
    assert client.req("DELETE", f"/v1/chats/{c['id']}").status_code == 204
    r = client.req("GET", f"/v1/chats/{c['id']}")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"
    assert client.req("DELETE", f"/v1/chats/{c['id']}").status_code == 404
    assert client.req("PATCH", f"/v1/chats/{c['id']}", json={"title": "x"}).status_code == 404


def test_unknown_chat_and_bad_path(client):
    r = client.req("GET", f"/v1/chats/{uuid.uuid4()}")
    assert r.status_code == 404
    r = client.req("GET", "/v1/chats/not-a-uuid")
    assert r.status_code == 400
    assert any(reason == "invalid_path_params" for _, reason in field_reason(r.json()))


def test_isolation_between_users_and_tenants(client, client_a2, client_b1):
    c = client.create_chat(title="private")
    for other in (client_a2, client_b1):
        assert other.req("GET", f"/v1/chats/{c['id']}").status_code == 404
        assert other.req("PATCH", f"/v1/chats/{c['id']}", json={"title": "x"}).status_code == 404
        assert other.req("DELETE", f"/v1/chats/{c['id']}").status_code == 404
        assert other.req("GET", f"/v1/chats/{c['id']}/messages").status_code == 404
        r = other.stream(c["id"], "hi")
        assert r.status_code == 404
        ids = [x["id"] for x in other.req("GET", "/v1/chats", params={"limit": 100}).json()["items"]]
        assert c["id"] not in ids
    assert client.req("GET", f"/v1/chats/{c['id']}").status_code == 200


def test_list_pagination_filter_order(stack):
    cl = Client(stack, "token-b1")
    made = []
    for i in range(5):
        made.append(cl.create_chat(title=f"page-{i}"))
        time.sleep(0.005)
    r = cl.req("GET", "/v1/chats", params={"limit": 2})
    assert r.status_code == 200
    page = r.json()
    assert page["page_info"]["limit"] == 2
    assert len(page["items"]) == 2
    # updated_at desc: newest first
    assert page["items"][0]["id"] == made[-1]["id"]
    seen = [x["id"] for x in page["items"]]
    cursor = page["page_info"]["next_cursor"]
    while cursor:
        r = cl.req("GET", "/v1/chats", params={"limit": 2, "cursor": cursor})
        assert r.status_code == 200, r.text
        p = r.json()
        seen += [x["id"] for x in p["items"]]
        cursor = p["page_info"]["next_cursor"]
    assert len(seen) == len(set(seen))
    assert set(x["id"] for x in made) <= set(seen)
    # filter + orderby
    r = cl.req("GET", "/v1/chats", params={"$filter": "contains(title, 'page-3')"})
    assert [x["id"] for x in r.json()["items"]] == [made[3]["id"]]
    r = cl.req("GET", "/v1/chats", params={"$filter": f"id eq '{made[1]['id']}'"})
    assert [x["id"] for x in r.json()["items"]] == [made[1]["id"]]
    r = cl.req("GET", "/v1/chats", params={"$orderby": "title asc", "$filter": "startswith(title, 'page-')"})
    titles = [x["title"] for x in r.json()["items"]]
    assert titles == sorted(titles)
    r = cl.req("GET", "/v1/chats", params={"$filter": f"updated_at gt {made[2]['updated_at']}"})
    ids = {x["id"] for x in r.json()["items"]}
    assert made[2]["id"] not in ids and made[3]["id"] in ids
    # limit clamping and validation
    r = cl.req("GET", "/v1/chats", params={"limit": 1000})
    assert r.status_code == 200 and r.json()["page_info"]["limit"] == 100
    for params, reason in (
        ({"limit": 0}, "INVALID_LIMIT"),
        ({"$filter": "nope eq 1"}, None),
        ({"$filter": "title eq"}, None),
        ({"$orderby": "nope desc"}, None),
        ({"cursor": "garbage"}, "INVALID_CURSOR"),
        ({"$skip": "1"}, "UNSUPPORTED_QUERY_PARAM"),
    ):
        r = cl.req("GET", "/v1/chats", params=params)
        assert r.status_code == 400, (params, r.text)
        prob = r.json()
        assert prob["context"]["resource_type"] == "gts.cf.core.odata.query.v1~"
        if reason:
            assert reason in [v["reason"] for v in prob["context"]["field_violations"]]
    # $select accepted and ignored
    r = cl.req("GET", "/v1/chats", params={"$select": "id"})
    assert r.status_code == 200 and "model" in r.json()["items"][0]


def test_ordering_reflects_activity(client, mock):
    a = client.create_chat(title="older", model="gpt-standard")
    b = client.create_chat(title="newer", model="gpt-standard")
    ids = [x["id"] for x in client.req("GET", "/v1/chats").json()["items"]]
    assert ids.index(b["id"]) < ids.index(a["id"])
    client.send(a["id"], "bump")
    ids = [x["id"] for x in client.req("GET", "/v1/chats").json()["items"]]
    assert ids.index(a["id"]) < ids.index(b["id"])
    client.req("PATCH", f"/v1/chats/{b['id']}", json={"title": "renamed"})
    ids = [x["id"] for x in client.req("GET", "/v1/chats").json()["items"]]
    assert ids.index(b["id"]) < ids.index(a["id"])


def test_message_count(client, mock):
    c = client.create_chat(model="gpt-standard")
    client.send(c["id"], "one")
    client.send(c["id"], "two")
    r = client.req("GET", f"/v1/chats/{c['id']}")
    assert r.json()["message_count"] == 4
    items = client.req("GET", "/v1/chats", params={"$filter": f"id eq '{c['id']}'"}).json()["items"]
    assert items[0]["message_count"] == 4


def test_delete_chat_db_and_cleanup_event(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    assert client.req("DELETE", f"/v1/chats/{c['id']}").status_code == 204
    rows = stack.query("select deleted_at from chats where id = ?", (uuid.UUID(c["id"]).bytes,))
    assert rows and rows[0]["deleted_at"] is not None
