"""Chats CRUD and list (OData) — DESIGN §3.3 Create/List/Get/Update/Delete Chat."""

import time
import uuid

import pytest

from conftest import CHAT_RT, ODATA_RT, assert_problem


def uniq(prefix="c"):
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


CHAT_KEYS = {"id", "model", "title", "is_temporary", "message_count", "created_at", "updated_at"}


# ------------------------------------------------------------------ create
def test_create_default_model_and_location(api):
    r = api.post_chat({})
    assert r.status_code == 201, r.text
    body = r.json()
    assert r.headers["Location"] == f"/mini-chat/v1/chats/{body['id']}"
    assert body["model"] == "gpt-4.1"
    assert "title" not in body  # omitted when absent, not null
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    assert "user_id" not in body and "tenant_id" not in body
    assert set(body) <= CHAT_KEYS
    uuid.UUID(body["id"])


def test_create_explicit_model_and_trimmed_title(api):
    body = api.create_chat(title="  Hello  ", model="gpt-4.1-mini")
    assert body["model"] == "gpt-4.1-mini"
    assert body["title"] == "Hello"


def test_create_null_title_is_untitled(api):
    body = api.create_chat(title=None)
    assert "title" not in body


@pytest.mark.parametrize("model", ["no-such-model", "disabled-model"])
def test_create_invalid_model(api, model):
    r = api.post_chat({"model": model})
    assert_problem(r, 400, "invalid_argument", field="model", reason="INVALID_MODEL")


@pytest.mark.parametrize("title", ["", "   ", "x" * 256])
def test_create_invalid_title(api, title):
    r = api.post_chat({"title": title})
    assert_problem(r, 400, "invalid_argument", field="title", reason="INVALID_TITLE")


def test_create_title_255_chars_ok(api):
    assert api.create_chat(title="y" * 255)["title"] == "y" * 255


def test_create_malformed_json_and_content_type(api):
    r = api.post("/chats", data="{bad", headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument", reason="json_syntax_error")
    r = api.post("/chats", data='{"title":"x"}', headers={"Content-Type": "text/plain"})
    assert_problem(r, 415, "invalid_argument", reason="missing_json_content_type")


# ------------------------------------------------------------------ get
def test_get_chat(api, chat):
    r = api.get(f"/chats/{chat['id']}")
    assert r.status_code == 200
    assert r.json() == chat


def test_get_unknown_and_bad_id(api):
    r = api.get(f"/chats/{uuid.uuid4()}")
    assert_problem(r, 404, "not_found", resource_type=CHAT_RT)
    r = api.get("/chats/not-a-uuid")
    assert_problem(r, 400, "invalid_argument", reason="invalid_path_params")


def test_unauthenticated(api):
    import requests

    from conftest import BASE

    r = requests.get(f"{BASE}/chats", timeout=10)
    assert r.status_code == 401, r.text
    r = requests.get(f"{BASE}/chats", headers={"Authorization": "Bearer nope"}, timeout=10)
    assert r.status_code == 401, r.text


# ------------------------------------------------------------------ patch
def test_patch_title_keeps_model_and_bumps_updated_at(api, chat):
    time.sleep(0.01)
    r = api.patch(f"/chats/{chat['id']}", json={"title": "  Renamed ", "model": "gpt-4.1-mini"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["title"] == "Renamed"
    assert body["model"] == chat["model"]
    assert body["updated_at"] > chat["updated_at"]
    assert body["created_at"] == chat["created_at"]
    assert api.get(f"/chats/{chat['id']}").json() == body


@pytest.mark.parametrize("title", ["", "  ", "z" * 256])
def test_patch_invalid_title(api, chat, title):
    r = api.patch(f"/chats/{chat['id']}", json={"title": title})
    assert_problem(r, 400, "invalid_argument", field="title", reason="INVALID_TITLE")


@pytest.mark.parametrize("body", [{}, {"title": None}, {"title": 5}])
def test_patch_schema_mismatch_is_422(api, chat, body):
    r = api.patch(f"/chats/{chat['id']}", json=body)
    assert_problem(r, 422, "invalid_argument", reason="invalid_json_body")


def test_patch_unknown_chat(api):
    r = api.patch(f"/chats/{uuid.uuid4()}", json={"title": "x"})
    assert_problem(r, 404, "not_found", resource_type=CHAT_RT)


# ------------------------------------------------------------------ delete
def test_delete_chat(api, chat):
    r = api.delete(f"/chats/{chat['id']}")
    assert r.status_code == 204 and r.content == b""
    assert_problem(api.get(f"/chats/{chat['id']}"), 404, "not_found", resource_type=CHAT_RT)
    assert_problem(api.delete(f"/chats/{chat['id']}"), 404, "not_found", resource_type=CHAT_RT)
    assert_problem(api.patch(f"/chats/{chat['id']}", json={"title": "x"}), 404, "not_found")
    listed = api.get("/chats", params={"$filter": f"id eq {chat['id']}"})
    assert listed.status_code == 200, listed.text
    assert listed.json()["items"] == []


# ------------------------------------------------------------------ list
def _list(api, **params):
    r = api.get("/chats", params=params)
    assert r.status_code == 200, r.text
    return r.json()


def test_list_shape_and_default_order(api):
    tag = uniq("ord")
    ids = [api.create_chat(title=f"{tag}-{i}")["id"] for i in range(3)]
    page = _list(api, limit=100)
    assert set(page["page_info"]) >= {"limit", "next_cursor", "prev_cursor"}
    assert page["page_info"]["limit"] == 100
    listed = [c["id"] for c in page["items"] if c.get("title", "").startswith(tag)]
    assert listed == list(reversed(ids)), "default order must be updated_at desc"
    stamps = [c["updated_at"] for c in page["items"]]
    assert stamps == sorted(stamps, reverse=True)
    for item in page["items"]:
        assert set(item) <= CHAT_KEYS

    # Renaming the oldest one moves it to the top (ordering reflects recent activity).
    time.sleep(0.01)
    api.patch(f"/chats/{ids[0]}", json={"title": f"{tag}-renamed"})
    listed = [c["id"] for c in _list(api, limit=100)["items"] if c.get("title", "").startswith(tag)]
    assert listed[0] == ids[0]


def test_list_filter_orderby(api):
    tag = uniq("flt")
    a = api.create_chat(title=f"{tag}-b")
    b = api.create_chat(title=f"{tag}-a")
    page = _list(api, **{"$filter": f"title eq '{tag}-b'"})
    assert [c["id"] for c in page["items"]] == [a["id"]]
    page = _list(api, **{"$filter": f"title eq '{tag}-a' or title eq '{tag}-b'", "$orderby": "title asc"})
    assert [c["id"] for c in page["items"]] == [b["id"], a["id"]]
    page = _list(api, **{"$filter": f"title eq '{tag}-a' or title eq '{tag}-b'", "$orderby": "title desc"})
    assert [c["id"] for c in page["items"]] == [a["id"], b["id"]]
    page = _list(api, **{"$filter": f"id eq {b['id']}"})
    assert [c["id"] for c in page["items"]] == [b["id"]]
    page = _list(api, limit=100, **{"$filter": f"updated_at ge {b['updated_at']}"})
    assert b["id"] in [c["id"] for c in page["items"]] and a["id"] not in [c["id"] for c in page["items"]]


def test_list_filter_timestamp_exact(api, chat):
    ids_eq = [c["id"] for c in _list(api, limit=100, **{"$filter": f"updated_at eq {chat['updated_at']}"})["items"]]
    ids_gt = [c["id"] for c in _list(api, limit=100, **{"$filter": f"updated_at gt {chat['updated_at']}"})["items"]]
    assert chat["id"] in ids_eq and chat["id"] not in ids_gt


def test_list_cursor_pagination(api):
    tag = uniq("pg")
    ids = [api.create_chat(title=f"{tag}-{i}")["id"] for i in range(5)]
    flt = " or ".join(f"title eq '{tag}-{i}'" for i in range(5))
    seen = []
    page = _list(api, limit=2, **{"$filter": flt})
    pages = 0
    while True:
        pages += 1
        assert len(page["items"]) <= 2
        seen += [c["id"] for c in page["items"]]
        cur = page["page_info"].get("next_cursor")
        if not cur:
            break
        page = _list(api, limit=2, cursor=cur, **{"$filter": flt})  # cursor must match the filter
        assert pages < 10
    assert pages == 3
    assert seen == list(reversed(ids))


def test_list_limit_clamped(api):
    assert _list(api, limit=1000)["page_info"]["limit"] == 100
    assert _list(api)["page_info"]["limit"] == 20


@pytest.mark.parametrize(
    "params,reason",
    [
        ({"limit": 0}, "INVALID_LIMIT"),
        ({"$filter": "title eq"}, "INVALID_FILTER"),
        ({"$filter": "nosuchfield eq 'x'"}, None),
        ({"$orderby": "nosuchfield asc"}, "INVALID_ORDERBY_FIELD"),
        ({"cursor": "garbage!!"}, "INVALID_CURSOR"),
        ({"$skip": "1"}, "UNSUPPORTED_QUERY_PARAM"),
    ],
)
def test_list_bad_odata(api, params, reason):
    r = api.get("/chats", params=params)
    assert_problem(r, 400, "invalid_argument", reason=reason, resource_type=ODATA_RT)


def test_list_cursor_with_orderby_rejected(api):
    tag = uniq("cw")
    for i in range(3):
        api.create_chat(title=f"{tag}-{i}")
    page = _list(api, limit=1)
    cur = page["page_info"]["next_cursor"]
    assert cur
    r = api.get("/chats", params={"cursor": cur, "$orderby": "title asc"})
    assert_problem(r, 400, "invalid_argument", reason="ORDER_WITH_CURSOR", resource_type=ODATA_RT)


def test_select_is_accepted_and_ignored(api, chat):
    page = _list(api, **{"$select": "id", "$filter": f"id eq {chat['id']}"})
    assert page["items"] and set(page["items"][0]) >= {"id", "model", "created_at"}


def test_list_isolated_per_user(api, reviewer, tenant_b, chat):
    for other in (reviewer, tenant_b):
        r = other.get("/chats", params={"$filter": f"id eq {chat['id']}"})
        assert r.status_code == 200, r.text
        assert r.json()["items"] == []
