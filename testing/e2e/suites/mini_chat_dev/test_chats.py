"""Chat CRUD and list (DESIGN section 3.3)."""

import uuid

import pytest

from .helpers import PREFIX, TOKEN_A, TOKEN_A_REVIEWER, TOKEN_B, api, assert_problem, create_chat

pytestmark = pytest.mark.usefixtures("server")

CHAT_TYPE = "gts.cf.core.mini_chat.chat.v1~"
ODATA_TYPE = "gts.cf.core.odata.query.v1~"


def test_crud_round_trip():
    s = api(TOKEN_A)
    r = s.post(f"{PREFIX}/chats", json={"title": "  Round trip  "})
    assert r.status_code == 201, r.text
    chat = r.json()
    assert r.headers["Location"] == f"{PREFIX}/chats/{chat['id']}"
    assert chat["title"] == "Round trip"
    assert chat["model"] == "gpt-premium"
    assert chat["is_temporary"] is False
    assert chat["message_count"] == 0
    assert set(chat) == {"id", "model", "title", "is_temporary", "message_count", "created_at", "updated_at"}

    got = s.get(f"{PREFIX}/chats/{chat['id']}")
    assert got.status_code == 200, got.text
    assert got.json() == chat

    renamed = s.patch(f"{PREFIX}/chats/{chat['id']}", json={"title": "Renamed", "model": "gpt-standard"})
    assert renamed.status_code == 200, renamed.text
    body = renamed.json()
    assert body["title"] == "Renamed"
    assert body["model"] == "gpt-premium"
    assert body["created_at"] == chat["created_at"]
    assert body["updated_at"] > chat["updated_at"]

    listed = s.get(f"{PREFIX}/chats", params={"limit": 100}).json()
    assert body in listed["items"]

    d = s.delete(f"{PREFIX}/chats/{chat['id']}")
    assert d.status_code == 204, d.text
    assert d.content == b""
    assert_problem(s.get(f"{PREFIX}/chats/{chat['id']}"), 404, "not_found", resource_type=CHAT_TYPE)
    assert_problem(s.delete(f"{PREFIX}/chats/{chat['id']}"), 404, "not_found", resource_type=CHAT_TYPE)
    ids = {c["id"] for c in s.get(f"{PREFIX}/chats", params={"limit": 100}).json()["items"]}
    assert chat["id"] not in ids


def test_untitled_chat_omits_title_and_explicit_model_is_kept():
    chat = create_chat(api(), model="gpt-standard")
    assert "title" not in chat
    assert chat["model"] == "gpt-standard"


@pytest.mark.parametrize("title", ["", "   ", "x" * 256])
def test_invalid_title_is_400(title):
    s = api()
    assert_problem(s.post(f"{PREFIX}/chats", json={"title": title}), 400, "invalid_argument",
                   reason="INVALID_TITLE", field="title")
    chat = create_chat(s)
    assert_problem(s.patch(f"{PREFIX}/chats/{chat['id']}", json={"title": title}), 400,
                   "invalid_argument", reason="INVALID_TITLE")


@pytest.mark.parametrize("model", ["gpt-disabled", "no-such-model"])
def test_invalid_model_is_400(model):
    r = api().post(f"{PREFIX}/chats", json={"model": model})
    assert_problem(r, 400, "invalid_argument", reason="INVALID_MODEL", field="model")


def test_patch_without_title_is_422():
    s = api()
    chat = create_chat(s)
    for body in ({}, {"title": None}, {"title": 5}):
        r = s.patch(f"{PREFIX}/chats/{chat['id']}", json=body)
        assert_problem(r, 422, "invalid_argument", reason="invalid_json_body")


def test_json_body_without_json_content_type_is_415():
    s = api()
    r = s.post(f"{PREFIX}/chats", data='{"title": "x"}', headers={"Content-Type": "text/plain"})
    assert_problem(r, 415, "invalid_argument", reason="missing_json_content_type")
    chat = create_chat(s)
    r = s.patch(f"{PREFIX}/chats/{chat['id']}", data='{"title": "x"}', headers={"Content-Type": "text/plain"})
    assert_problem(r, 415, "invalid_argument", reason="missing_json_content_type")


def test_malformed_json_is_400():
    r = api().post(f"{PREFIX}/chats", data="{", headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument", reason="json_syntax_error")


def test_non_uuid_path_is_400():
    s = api()
    for method in ("get", "delete"):
        r = getattr(s, method)(f"{PREFIX}/chats/not-a-uuid")
        assert_problem(r, 400, "invalid_argument", reason="invalid_path_params")
    r = s.patch(f"{PREFIX}/chats/not-a-uuid", json={"title": "x"})
    assert_problem(r, 400, "invalid_argument", reason="invalid_path_params")


@pytest.mark.parametrize("token", [TOKEN_B, TOKEN_A_REVIEWER])
def test_other_tenant_or_user_gets_404(token):
    chat = create_chat(api(TOKEN_A), title="private")
    other = api(token)
    url = f"{PREFIX}/chats/{chat['id']}"
    assert_problem(other.get(url), 404, "not_found", resource_type=CHAT_TYPE)
    assert_problem(other.patch(url, json={"title": "hijack"}), 404, "not_found", resource_type=CHAT_TYPE)
    assert_problem(other.delete(url), 404, "not_found", resource_type=CHAT_TYPE)
    assert_problem(other.get(f"{url}/messages"), 404, "not_found", resource_type=CHAT_TYPE)
    ids = {c["id"] for c in other.get(f"{PREFIX}/chats", params={"limit": 100}).json()["items"]}
    assert chat["id"] not in ids
    assert api(TOKEN_A).get(url).json()["title"] == "private"


def test_ordering_after_rename():
    s = api()
    first = create_chat(s, title="first")
    second = create_chat(s, title="second")
    ids = [c["id"] for c in s.get(f"{PREFIX}/chats").json()["items"]]
    assert ids.index(second["id"]) < ids.index(first["id"])

    assert s.patch(f"{PREFIX}/chats/{first['id']}", json={"title": "first renamed"}).status_code == 200

    ids = [c["id"] for c in s.get(f"{PREFIX}/chats").json()["items"]]
    assert ids[0] == first["id"]
    assert ids.index(first["id"]) < ids.index(second["id"])


def test_list_pagination_filter_and_limits():
    s = api(TOKEN_A_REVIEWER)
    tag = uuid.uuid4().hex[:8]
    created = [create_chat(s, title=f"{tag}-{i}")["id"] for i in range(3)]
    flt = {"$filter": f"startswith(title,'{tag}')"}

    page1 = s.get(f"{PREFIX}/chats", params={**flt, "limit": 2})
    assert page1.status_code == 200, page1.text
    body = page1.json()
    assert [c["id"] for c in body["items"]] == created[::-1][:2]
    assert body["page_info"]["limit"] == 2
    page2 = s.get(f"{PREFIX}/chats", params={**flt, "limit": 2, "cursor": body["page_info"]["next_cursor"]}).json()
    assert [c["id"] for c in page2["items"]] == [created[0]]
    assert page2["page_info"]["next_cursor"] is None

    assert s.get(f"{PREFIX}/chats", params={"limit": 1000}).json()["page_info"]["limit"] == 100
    assert s.get(f"{PREFIX}/chats", params={"$select": "id,title"}).status_code == 200
    by_title = s.get(f"{PREFIX}/chats", params={**flt, "$orderby": "title asc"}).json()
    assert [c["id"] for c in by_title["items"]] == created


@pytest.mark.parametrize("params,reason", [
    ({"$filter": "model eq 'gpt-premium'"}, "INVALID_FILTER"),
    ({"$filter": "title eq"}, "INVALID_FILTER"),
    ({"$orderby": "created_at desc"}, "INVALID_ORDERBY_FIELD"),
    ({"limit": 0}, "INVALID_LIMIT"),
    ({"cursor": "not-a-cursor"}, "INVALID_CURSOR"),
    ({"$skip": 5}, "UNSUPPORTED_QUERY_PARAM"),
])
def test_bad_list_query_is_400_odata(params, reason):
    r = api().get(f"{PREFIX}/chats", params=params)
    assert_problem(r, 400, "invalid_argument", reason=reason, resource_type=ODATA_TYPE)
