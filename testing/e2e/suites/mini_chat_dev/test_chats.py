"""Chat CRUD, list (OData filter/order/paging), validation, isolation, auth."""

from __future__ import annotations

import time
import uuid

import httpx
import pytest

from .conftest import PREFIX, RT_CHAT, RT_ODATA, USERS, assert_problem, field_reasons, ub


@pytest.mark.smoke
def test_create_chat_returns_201_location_and_default_model(api):
    a = api("A")
    r = a.post("/chats", json={"title": "  Quarterly report  "})
    assert r.status_code == 201, r.text
    chat = r.json()
    assert r.headers["location"] == f"{PREFIX}/chats/{chat['id']}"
    uuid.UUID(chat["id"])
    assert chat["model"] == "gpt-4.1"  # first enabled is_default model
    assert chat["title"] == "Quarterly report"  # trimmed
    assert chat["is_temporary"] is False
    assert chat["message_count"] == 0
    assert chat["created_at"] and chat["updated_at"]
    for internal in ("user_id", "tenant_id", "deleted_at"):
        assert internal not in chat


def test_create_chat_without_title_omits_title(api):
    a = api("A")
    chat = a.create_chat()
    assert "title" not in chat
    assert "title" not in a.get(f"/chats/{chat['id']}").json()
    # explicit null is also untitled
    assert "title" not in a.create_chat(title=None)


def test_create_chat_with_explicit_model(api):
    chat = api("A").create_chat(model="gpt-4.1-mini")
    assert chat["model"] == "gpt-4.1-mini"


@pytest.mark.parametrize("model", ["no-such-model", "gpt-disabled"])
def test_create_chat_rejects_unknown_or_disabled_model(api, model):
    r = api("A").post("/chats", json={"model": model})
    p = assert_problem(r, 400)
    assert "INVALID_MODEL" in field_reasons(p)
    fv = [v for v in p["context"]["field_violations"] if v["reason"] == "INVALID_MODEL"]
    assert fv[0]["field"] == "model"


@pytest.mark.parametrize("title", ["", "   ", "x" * 256])
def test_create_chat_rejects_invalid_title(api, title):
    r = api("A").post("/chats", json={"title": title, "model": "no-such-model"})
    # the title is validated before the model lookup
    p = assert_problem(r, 400)
    assert field_reasons(p) == ["INVALID_TITLE"]


def test_create_chat_accepts_255_char_title(api):
    a = api("A")
    assert a.create_chat(title="t" * 255)["title"] == "t" * 255
    # the limit counts characters, not bytes
    assert a.create_chat(title="Ж" * 255)["title"] == "Ж" * 255
    assert a.create_chat(title="👋" * 255)["title"] == "👋" * 255
    assert field_reasons(assert_problem(a.post("/chats", json={"title": "Ж" * 256}), 400)) == ["INVALID_TITLE"]


def test_get_update_delete_lifecycle(api):
    a = api("A")
    chat = a.create_chat(title="Original")
    cid = chat["id"]

    got = a.get(f"/chats/{cid}")
    assert got.status_code == 200
    assert got.json() == chat

    time.sleep(0.01)
    r = a.patch(f"/chats/{cid}", json={"title": "  Renamed  ", "model": "gpt-4.1-mini"})
    assert r.status_code == 200, r.text
    upd = r.json()
    assert upd["title"] == "Renamed"
    assert upd["model"] == chat["model"]  # model is immutable
    assert upd["created_at"] == chat["created_at"]
    assert upd["updated_at"] > chat["updated_at"]
    assert a.get(f"/chats/{cid}").json()["title"] == "Renamed"

    assert a.delete(f"/chats/{cid}").status_code == 204
    p = assert_problem(a.get(f"/chats/{cid}"), 404)
    assert p["context"]["resource_type"] == RT_CHAT
    assert_problem(a.delete(f"/chats/{cid}"), 404)
    assert_problem(a.patch(f"/chats/{cid}", json={"title": "x"}), 404)
    assert cid not in [c["id"] for c in a.get("/chats", params={"limit": 100}).json()["items"]]


def test_update_chat_validation(api):
    a = api("A")
    cid = a.create_chat(title="v")["id"]
    for bad in ("", "   ", "y" * 256):
        p = assert_problem(a.patch(f"/chats/{cid}", json={"title": bad}), 400)
        assert field_reasons(p) == ["INVALID_TITLE"]
    # schema mismatch -> 422, malformed JSON -> 400, no JSON content type -> 415
    assert_problem(a.patch(f"/chats/{cid}", json={}), 422)
    assert_problem(a.patch(f"/chats/{cid}", json={"title": None}), 422)
    assert_problem(
        a.patch(f"/chats/{cid}", content=b"{not json", headers={"content-type": "application/json"}), 400
    )
    assert a.patch(f"/chats/{cid}", content=b'{"title":"x"}', headers={"content-type": "text/plain"}).status_code == 415
    assert a.get(f"/chats/{cid}").json()["title"] == "v"


def test_non_uuid_path_param_is_400(api):
    p = assert_problem(api("A").get("/chats/not-a-uuid"), 400)
    assert "invalid_path_params" in field_reasons(p)


def test_list_default_order_and_paging(api):
    a = api("B")
    ids = [a.create_chat(title=f"page-{i}")["id"] for i in range(5)]
    time.sleep(0.01)
    # the oldest chat gets activity -> moves to the top
    a.patch(f"/chats/{ids[0]}", json={"title": "page-0-renamed"})

    first = a.get("/chats", params={"limit": 2})
    assert first.status_code == 200, first.text
    body = first.json()
    assert body["page_info"]["limit"] == 2
    assert [c["id"] for c in body["items"]] == [ids[0], ids[4]]
    cursor = body["page_info"]["next_cursor"]
    assert cursor

    second = a.get("/chats", params={"limit": 2, "cursor": cursor}).json()
    assert [c["id"] for c in second["items"]] == [ids[3], ids[2]]
    assert second["page_info"]["prev_cursor"]

    full = a.get("/chats", params={"limit": 100}).json()["items"]
    stamps = [(c["updated_at"], c["id"]) for c in full]
    assert stamps == sorted(stamps, reverse=True)


def test_list_activity_from_send_moves_chat_to_top(api):
    a = api("B")
    older = a.create_chat(title="older")["id"]
    a.create_chat(title="newer")
    a.send(older, "bump")
    items = a.get("/chats", params={"limit": 1}).json()["items"]
    assert items[0]["id"] == older
    assert items[0]["message_count"] == 2


def test_list_limit_clamped_and_zero_rejected(api):
    a = api("A")
    assert a.get("/chats", params={"limit": 1000}).json()["page_info"]["limit"] == 100
    assert a.get("/chats").json()["page_info"]["limit"] == 20
    p = assert_problem(a.get("/chats", params={"limit": 0}), 400)
    assert p["context"]["resource_type"] == RT_ODATA
    assert "INVALID_LIMIT" in field_reasons(p)


def test_list_filter_and_orderby(api):
    a = api("A")
    marker = uuid.uuid4().hex[:8]
    c1 = a.create_chat(title=f"alpha-{marker}")["id"]
    c2 = a.create_chat(title=f"beta-{marker}")["id"]

    r = a.get("/chats", params={"$filter": f"title eq 'alpha-{marker}'"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [c1]

    r = a.get("/chats", params={"$filter": f"id eq {c2}"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [c2]

    r = a.get("/chats", params={"$orderby": "title asc", "limit": 100})
    assert r.status_code == 200, r.text
    titles = [c.get("title", "") for c in r.json()["items"]]
    assert titles == sorted(titles)

    # $select is accepted and ignored
    r = a.get("/chats", params={"$select": "id", "limit": 1})
    assert r.status_code == 200
    assert "model" in r.json()["items"][0]


@pytest.mark.parametrize(
    "params,reason",
    [
        ({"$filter": "nope eq 'x'"}, "INVALID_FILTER"),
        ({"$filter": "title eq"}, "INVALID_FILTER"),
        ({"$orderby": "model asc"}, "INVALID_ORDERBY_FIELD"),
        ({"cursor": "garbage!!"}, "INVALID_CURSOR"),
        ({"$skip": "1"}, "UNSUPPORTED_QUERY_PARAM"),
    ],
)
def test_list_rejects_malformed_query(api, params, reason):
    p = assert_problem(api("A").get("/chats", params=params), 400)
    assert p["context"]["resource_type"] == RT_ODATA
    assert reason in field_reasons(p), p


def test_chats_are_isolated_between_users_and_tenants(api, db):
    a, b, c = api("A"), api("B"), api("C")
    chat = a.create_chat(title="private")
    cid = chat["id"]
    for other in (b, c):
        for r in (
            other.get(f"/chats/{cid}"),
            other.patch(f"/chats/{cid}", json={"title": "stolen"}),
            other.delete(f"/chats/{cid}"),
            other.get(f"/chats/{cid}/messages"),
        ):
            p = assert_problem(r, 404)
            assert p["context"]["resource_type"] == RT_CHAT
        assert cid not in [x["id"] for x in other.get("/chats", params={"limit": 100}).json()["items"]]
        assert other.stream(cid, "hi").status == 404
    assert a.get(f"/chats/{cid}").json()["title"] == "private"

    row = db.execute("SELECT tenant_id, user_id FROM chats WHERE id = ?", (ub(cid),)).fetchone()
    assert bytes(row["tenant_id"]) == ub(USERS["A"]["tenant"])
    assert bytes(row["user_id"]) == ub(USERS["A"]["id"])


def test_missing_or_invalid_token_is_401(server):
    for headers in ({}, {"Authorization": "Bearer not-a-known-token"}):
        r = httpx.get(f"{server.base_url}{PREFIX}/chats", headers=headers)
        assert r.status_code == 401, r.text
        p = r.json()
        assert p["status"] == 401
        assert "context" in p
    r = httpx.post(f"{server.base_url}{PREFIX}/chats", json={})
    assert r.status_code == 401
