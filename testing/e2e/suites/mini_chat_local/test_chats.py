"""Chat CRUD, chat list (OData, cursor pagination, ordering by activity) and chat deletion,
black-box against the real server and its SQLite database.

Source: DESIGN 3.3 "Create Chat" / "List Chats" / "Get Chat" / "Update Chat Title" /
"Delete Chat" and the REST error table, 3.6 / section 4 "Cleanup on Chat Deletion",
3.7 ``chats`` / ``attachments``, ADR-0004, ADR-0009, OpenAPI ``ChatDetailDto``,
``CreateChatReq``, ``UpdateChatReq``, ``Page_ChatDetailDto``, ``PageInfo``.

The server and its database are shared by the whole session, so every test tags the chats
it creates (``unique_tag``) and lists them through ``$filter=contains(title,'<tag>')``.
"""

from __future__ import annotations

import re
import time
import uuid
from datetime import timedelta

import pytest

from . import catalog
from .support import (
    DAY,
    MIXED_WIDTH_INSTANTS,
    RESOURCE_TYPES,
    TENANT_A_ID,
    USER_A_ID,
    assert_problem,
    iso,
    list_all,
    parse_ts,
    qs,
    unique_tag,
    uuid_blob,
    wire_ts,
)

CHAT_KEYS = {"id", "model", "title", "is_temporary", "message_count", "created_at", "updated_at"}
ODATA_TYPE = "gts.cf.core.odata.query.v1~"


def create(api, user="a", **body):
    resp = api.post("/v1/chats", json=body, user=user)
    assert resp.status_code == 201, resp.text
    return resp.json()


def tagged(tag):
    return {"_filter": f"contains(title,'{tag}')"}


def ids(items):
    return [i["id"] for i in items]


def field_violations(problem):
    return problem["context"]["field_violations"]


def has_violation(problem, field, reason):
    return any(v.get("field") == field and v.get("reason") == reason
               for v in field_violations(problem))


# --------------------------------------------------------------------------- #
# POST /v1/chats
# --------------------------------------------------------------------------- #


@pytest.mark.smoke
def test_create_returns_201_with_location_and_the_chat_detail(api):
    tag = unique_tag()
    resp = api.post("/v1/chats", json={"title": f"{tag} Q3 Financial Analysis"})

    assert resp.status_code == 201, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    body = resp.json()
    uuid.UUID(body["id"])
    assert resp.headers["location"] == f"/mini-chat/v1/chats/{body['id']}"
    assert set(body) == CHAT_KEYS
    assert body["title"] == f"{tag} Q3 Financial Analysis"
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    parse_ts(body["created_at"])
    parse_ts(body["updated_at"])


def test_create_never_exposes_user_or_tenant_identity(api):
    body = create(api, title=unique_tag())
    assert "user_id" not in body and "tenant_id" not in body
    assert USER_A_ID not in str(body)


@pytest.mark.parametrize("payload", [{}, {"title": None}, {"model": None}])
def test_create_without_a_title_omits_the_field(api, payload):
    body = create(api, **payload)
    assert "title" not in body, body
    assert set(body) == CHAT_KEYS - {"title"}


def test_create_persists_the_chat_for_the_caller(api, seed):
    body = create(api, title=f"{unique_tag()} mine")
    row = seed.chat(body["id"])
    assert row is not None
    assert bytes(row["tenant_id"]) == uuid_blob(TENANT_A_ID)
    assert bytes(row["user_id"]) == uuid_blob(USER_A_ID)
    assert row["model"] == body["model"]
    assert row["title"] == body["title"]
    assert row["deleted_at"] is None
    assert not row["is_temporary"]


def test_create_ignores_identity_fields_supplied_by_the_client(api, seed):
    other = str(uuid.uuid4())
    body = create(api, title=unique_tag(), user_id=other, tenant_id=other, is_temporary=True)
    row = seed.chat(body["id"])
    assert bytes(row["user_id"]) == uuid_blob(USER_A_ID)
    assert bytes(row["tenant_id"]) == uuid_blob(TENANT_A_ID)
    assert not row["is_temporary"]


def test_create_without_a_model_uses_the_default_catalog_model(api):
    assert create(api)["model"] == catalog.PREMIUM_ID
    assert create(api, model=None)["model"] == catalog.PREMIUM_ID


def test_create_with_an_explicit_enabled_model_stores_it(api, seed):
    body = create(api, model=catalog.STANDARD_ID)
    assert body["model"] == catalog.STANDARD_ID
    assert seed.chat(body["id"])["model"] == catalog.STANDARD_ID


@pytest.mark.parametrize("model", [catalog.DISABLED_ID, "does-not-exist", "", "MOCK-PREMIUM"])
def test_create_with_an_unknown_or_disabled_model_is_400_invalid_model(api, model):
    resp = api.post("/v1/chats", json={"model": model, "title": "x"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "model", "INVALID_MODEL"), problem
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_create_trims_the_title(api):
    tag = unique_tag()
    assert create(api, title=f"  \t {tag} Hello  world \n ")["title"] == f"{tag} Hello  world"


@pytest.mark.parametrize("length", [1, 255])
def test_create_accepts_titles_of_1_to_255_characters(api, length):
    assert api.post("/v1/chats", json={"title": "a" * length}).status_code == 201


def test_create_accepts_255_multibyte_characters_and_a_padded_255(api):
    assert api.post("/v1/chats", json={"title": "é" * 255}).status_code == 201
    assert api.post("/v1/chats", json={"title": "  " + "a" * 255 + "  "}).status_code == 201


@pytest.mark.parametrize("title", ["", "   ", "\n\t \r", "a" * 256, "  " + "a" * 256 + "  ",
                                   "é" * 256])
def test_create_rejects_invalid_titles_with_invalid_title(api, title):
    resp = api.post("/v1/chats", json={"title": title})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "title", "INVALID_TITLE"), problem
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_an_invalid_title_wins_over_an_invalid_model(api):
    # DESIGN: the title is validated before authorization and the model lookup.
    resp = api.post("/v1/chats", json={"title": "  ", "model": "does-not-exist"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "title", "INVALID_TITLE"), problem


@pytest.mark.parametrize("payload", ['{"title": 5}', '{"model": 5}', '{"title": ["a"]}', "[]"])
def test_create_body_that_does_not_match_the_schema_is_422(api, payload):
    resp = api.post("/v1/chats", content=payload, headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 422, "invalid_argument")
    assert has_violation(problem, "body", "invalid_json_body"), problem


def test_create_malformed_json_is_400(api):
    resp = api.post("/v1/chats", content='{"title": ', headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "body", "json_syntax_error"), problem


@pytest.mark.parametrize("content_type", ["text/plain", "application/x-www-form-urlencoded"])
def test_create_without_a_json_content_type_is_415(api, content_type):
    resp = api.post("/v1/chats", content="{}", headers={"Content-Type": content_type})
    problem = assert_problem(resp, 415, "invalid_argument")
    assert has_violation(problem, "body", "missing_json_content_type"), problem


def test_a_created_chat_is_readable_and_listed_first(api):
    tag = unique_tag()
    created = create(api, title=f"{tag} round trip")
    assert api.get(f"/v1/chats/{created['id']}").json() == created
    assert list_all(api, "/v1/chats", **tagged(tag)) == [created]


# --------------------------------------------------------------------------- #
# GET /v1/chats/{id}
# --------------------------------------------------------------------------- #


def test_get_returns_the_chat_detail_with_the_live_message_count(api, seed):
    tag = unique_tag()
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} counted",
                            created=100)
    other = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} other", created=100)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=1)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", created=2)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=3, deleted=True)
    seed.insert_message(tenant=TENANT_A_ID, chat=other, role="user", created=1)

    resp = api.get(f"/v1/chats/{chat}")

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert set(body) == CHAT_KEYS
    assert body["id"] == chat
    assert body["model"] == "mock-premium"
    assert body["title"] == f"{tag} counted"
    assert body["is_temporary"] is False
    assert body["message_count"] == 2, "soft-deleted messages and other chats' messages are not counted"
    assert parse_ts(body["created_at"]) == parse_ts(iso(100))
    assert parse_ts(body["updated_at"]) == parse_ts(iso(100))


def test_get_omits_the_title_of_an_untitled_chat(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    assert "title" not in api.get(f"/v1/chats/{chat}").json()


def test_get_unknown_chat_is_404_for_the_chat_resource(api):
    problem = assert_problem(api.get(f"/v1/chats/{uuid.uuid4()}"), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_get_soft_deleted_chat_is_404(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, deleted=True)
    problem = assert_problem(api.get(f"/v1/chats/{chat}"), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_get_keeps_a_model_the_catalog_no_longer_lists(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model="gpt-retired")
    assert api.get(f"/v1/chats/{chat}").json()["model"] == "gpt-retired"


# --------------------------------------------------------------------------- #
# PATCH /v1/chats/{id}
# --------------------------------------------------------------------------- #


def test_patch_renames_the_chat_and_bumps_updated_at(api, seed):
    tag = unique_tag()
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} old", created=100)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=1)

    resp = api.patch(f"/v1/chats/{chat}", json={"title": f"{tag} Renamed Chat"})

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert set(body) == CHAT_KEYS
    assert body["title"] == f"{tag} Renamed Chat"
    assert body["message_count"] == 1
    assert parse_ts(body["created_at"]) == parse_ts(iso(100)), "created_at never changes"
    assert parse_ts(body["updated_at"]) > parse_ts(iso(100)) + timedelta(days=1), body
    assert api.get(f"/v1/chats/{chat}").json() == body
    assert seed.chat(chat)["title"] == f"{tag} Renamed Chat"


def test_patch_trims_the_title_and_titles_an_untitled_chat(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    body = api.patch(f"/v1/chats/{chat}", json={"title": "  Fresh  "}).json()
    assert body["title"] == "Fresh"


def test_patch_never_changes_the_model_and_ignores_unknown_fields(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="old",
                            model=catalog.PREMIUM_ID)

    resp = api.patch(f"/v1/chats/{chat}", json={
        "title": "Renamed", "model": catalog.STANDARD_ID, "is_temporary": True,
        "user_id": str(uuid.uuid4()), "whatever": [1, 2]})

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["title"] == "Renamed"
    assert body["model"] == catalog.PREMIUM_ID
    assert body["is_temporary"] is False
    row = seed.chat(chat)
    assert row["model"] == catalog.PREMIUM_ID
    assert not row["is_temporary"]
    assert bytes(row["user_id"]) == uuid_blob(USER_A_ID)


@pytest.mark.parametrize("payload", ["{}", '{"title": null}', '{"title": 42}', '{"title": ["x"]}',
                                     '{"model": "mock-standard"}', "null", "[]"])
def test_patch_body_that_does_not_match_the_schema_is_422(api, seed, payload):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="Keep")
    resp = api.patch(f"/v1/chats/{chat}", content=payload,
                     headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 422, "invalid_argument")
    assert has_violation(problem, "body", "invalid_json_body"), problem
    assert seed.chat(chat)["title"] == "Keep"


def test_patch_malformed_json_is_400_and_a_non_json_content_type_is_415(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="Keep")
    resp = api.patch(f"/v1/chats/{chat}", content='{"title": "x',
                     headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "body", "json_syntax_error"), problem

    resp = api.patch(f"/v1/chats/{chat}", content='{"title": "x"}',
                     headers={"Content-Type": "text/plain"})
    problem = assert_problem(resp, 415, "invalid_argument")
    assert has_violation(problem, "body", "missing_json_content_type"), problem
    assert seed.chat(chat)["title"] == "Keep"


@pytest.mark.parametrize("title", ["", "  \t ", "a" * 256, " " + "a" * 256 + " "])
def test_patch_with_an_invalid_title_is_400_and_changes_nothing(api, seed, title):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="Keep", created=100)
    resp = api.patch(f"/v1/chats/{chat}", json={"title": title})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "title", "INVALID_TITLE"), problem
    row = seed.chat(chat)
    assert row["title"] == "Keep"
    assert parse_ts(row["updated_at"]) == parse_ts(iso(100)), "a rejected rename does not bump"


@pytest.mark.parametrize("length", [1, 255])
def test_patch_accepts_titles_of_1_to_255_characters(api, seed, length):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    resp = api.patch(f"/v1/chats/{chat}", json={"title": "a" * length})
    assert resp.status_code == 200, resp.text


def test_patch_unknown_or_deleted_chat_is_404(api, seed):
    deleted = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="Gone", deleted=True)
    for chat in (str(uuid.uuid4()), deleted):
        problem = assert_problem(api.patch(f"/v1/chats/{chat}", json={"title": "x"}), 404,
                                 "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    assert seed.chat(deleted)["title"] == "Gone"


# --------------------------------------------------------------------------- #
# GET /v1/chats: envelope, ordering, pagination
# --------------------------------------------------------------------------- #


def seed_chats(seed, tag, count, *, step=1, titles=None):
    """``count`` chats whose ``updated_at`` increases with the index; returns their ids."""
    out = []
    for n in range(1, count + 1):
        title = titles[n - 1] if titles else f"c{n:03d}"
        out.append(seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID,
                                    title=f"{tag} {title}", created=n * step))
    return out


def test_list_body_is_a_page_of_chat_details(api, seed):
    tag = unique_tag()
    (chat,) = seed_chats(seed, tag, 1)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=1)

    resp = api.get("/v1/chats" + qs(**tagged(tag)))

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    page = resp.json()
    assert set(page) == {"items", "page_info"}
    assert set(page["page_info"]) <= {"limit", "next_cursor", "prev_cursor"}
    assert page["page_info"]["limit"] == 20
    (item,) = page["items"]
    assert set(item) == CHAT_KEYS
    assert item == api.get(f"/v1/chats/{chat}").json(), "same shape as GET /chats/{id}"
    assert item["message_count"] == 1


def test_list_returns_only_live_chats(api, seed):
    tag = unique_tag()
    (live,) = seed_chats(seed, tag, 1)
    seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} gone", deleted=True)
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [live]


def test_list_default_order_is_updated_at_descending(api, seed):
    tag = unique_tag()
    oldest = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} 1", updated=100)
    newest = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} 2", updated=300)
    middle = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} 3", updated=200)
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [newest, middle, oldest]


def test_list_orders_by_updated_at_not_created_at(api, seed):
    tag = unique_tag()
    active = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} a", created=10,
                              updated=900)
    idle = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} b", created=500,
                            updated=500)
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [active, idle]


def test_list_ties_on_updated_at_are_ordered_deterministically_across_pages(api, seed):
    tag = unique_tag()
    created = [seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} {n}",
                                created=50) for n in range(5)]
    full = ids(list_all(api, "/v1/chats", page_size=100, **tagged(tag)))
    assert sorted(full) == sorted(created)
    assert ids(list_all(api, "/v1/chats", page_size=2, **tagged(tag))) == full


def test_list_limit_defaults_to_20_and_is_clamped_to_100(api, seed):
    tag = unique_tag()
    seed_chats(seed, tag, 105)

    default = api.get("/v1/chats" + qs(**tagged(tag))).json()
    assert len(default["items"]) == 20
    assert default["page_info"]["limit"] == 20
    assert default["page_info"].get("next_cursor")

    resp = api.get("/v1/chats" + qs(limit=1000, **tagged(tag)))
    assert resp.status_code == 200, "a limit above 100 is clamped, not rejected: " + resp.text
    clamped = resp.json()
    assert len(clamped["items"]) == 100
    assert clamped["page_info"]["limit"] == 100

    one = api.get("/v1/chats" + qs(limit=1, **tagged(tag))).json()
    assert len(one["items"]) == 1 and one["page_info"]["limit"] == 1


def assert_odata_problem(resp, field, reason):
    problem = assert_problem(resp, 400, "invalid_argument")
    assert problem["context"]["resource_type"] == ODATA_TYPE, problem
    assert has_violation(problem, field, reason), problem
    return problem


def test_list_limit_zero_is_400_invalid_limit(api):
    assert_odata_problem(api.get("/v1/chats" + qs(limit=0)), "$top", "INVALID_LIMIT")


@pytest.mark.parametrize("limit", ["abc", "-1", "1.5"])
def test_list_non_numeric_or_negative_limit_is_a_400_odata_error(api, limit):
    problem = assert_problem(api.get("/v1/chats" + qs(limit=limit)), 400, "invalid_argument")
    assert problem["context"]["resource_type"] == ODATA_TYPE


def test_list_cursor_pagination_walks_forward_and_backward(api, seed):
    tag = unique_tag()
    c1, c2, c3, c4, c5 = seed_chats(seed, tag, 5)  # default order: c5 c4 c3 c2 c1
    flt = tagged(tag)

    p1 = api.get("/v1/chats" + qs(limit=2, **flt)).json()
    assert ids(p1["items"]) == [c5, c4]
    assert p1["page_info"]["limit"] == 2
    assert not p1["page_info"].get("prev_cursor")
    next1 = p1["page_info"]["next_cursor"]
    assert next1

    p2 = api.get("/v1/chats" + qs(limit=2, cursor=next1, **flt)).json()
    assert ids(p2["items"]) == [c3, c2]
    prev2, next2 = p2["page_info"]["prev_cursor"], p2["page_info"]["next_cursor"]
    assert prev2 and next2

    p3 = api.get("/v1/chats" + qs(limit=2, cursor=next2, **flt)).json()
    assert ids(p3["items"]) == [c1]
    assert not p3["page_info"].get("next_cursor")
    prev3 = p3["page_info"]["prev_cursor"]
    assert prev3

    back = api.get("/v1/chats" + qs(limit=2, cursor=prev3, **flt)).json()
    assert ids(back["items"]) == [c3, c2]
    back = api.get("/v1/chats" + qs(limit=2, cursor=prev2, **flt)).json()
    assert ids(back["items"]) == [c5, c4]


@pytest.mark.parametrize("cursor", ["not-a-cursor!!", "e30", "AAAA"])
def test_list_malformed_cursor_is_400_invalid_cursor(api, cursor):
    assert_odata_problem(api.get("/v1/chats" + qs(cursor=cursor)), "cursor", "INVALID_CURSOR")


def test_list_cursor_combined_with_orderby_is_400_order_with_cursor(api, seed):
    tag = unique_tag()
    seed_chats(seed, tag, 3)
    cursor = api.get("/v1/chats" + qs(limit=1, **tagged(tag))).json()["page_info"]["next_cursor"]
    resp = api.get("/v1/chats" + qs(limit=1, cursor=cursor, _orderby="title asc", **tagged(tag)))
    assert_odata_problem(resp, "cursor", "ORDER_WITH_CURSOR")


def test_list_cursor_of_another_filter_is_400_filter_mismatch(api, seed):
    tag = unique_tag()
    seed_chats(seed, tag, 3)
    first = api.get("/v1/chats" + qs(limit=1, _filter=f"title ne '{tag} zzz'")).json()
    cursor = first["page_info"]["next_cursor"]
    resp = api.get("/v1/chats" + qs(limit=1, cursor=cursor, _filter=f"title ne '{tag} yyy'"))
    assert_odata_problem(resp, "cursor", "FILTER_MISMATCH")


# ---- $orderby ---------------------------------------------------------------


def titled_trio(seed, tag):
    gamma = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Gamma notes",
                             updated=100)
    alpha = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Alpha plan",
                             updated=300)
    beta = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Beta plan",
                            updated=200)
    return gamma, alpha, beta


def test_list_orderby_title_ascending_and_descending(api, seed):
    tag = unique_tag()
    gamma, alpha, beta = titled_trio(seed, tag)
    asc = list_all(api, "/v1/chats", _orderby="title asc", **tagged(tag))
    assert ids(asc) == [alpha, beta, gamma]
    desc = list_all(api, "/v1/chats", _orderby="title desc", **tagged(tag))
    assert ids(desc) == [gamma, beta, alpha]


def test_list_orderby_updated_at_ascending(api, seed):
    tag = unique_tag()
    gamma, alpha, beta = titled_trio(seed, tag)
    asc = list_all(api, "/v1/chats", _orderby="updated_at asc", **tagged(tag))
    assert ids(asc) == [gamma, beta, alpha]


def test_list_orderby_with_cursor_pagination(api, seed):
    tag = unique_tag()
    gamma, alpha, beta = titled_trio(seed, tag)
    p1 = api.get("/v1/chats" + qs(limit=2, _orderby="title asc", **tagged(tag))).json()
    assert ids(p1["items"]) == [alpha, beta]
    cursor = p1["page_info"]["next_cursor"]
    p2 = api.get("/v1/chats" + qs(limit=2, cursor=cursor, **tagged(tag))).json()
    assert ids(p2["items"]) == [gamma]


@pytest.mark.parametrize("orderby", ["model asc", "message_count desc", "user_id asc",
                                     "created_at asc", "nope", "title sideways"])
def test_list_orderby_unknown_field_is_400_invalid_orderby_field(api, orderby):
    resp = api.get("/v1/chats" + qs(_orderby=orderby))
    assert_odata_problem(resp, "$orderby", "INVALID_ORDERBY_FIELD")


# ---- $filter ----------------------------------------------------------------


def filter_fixture(seed, tag):
    rows = [("Alpha plan", 1), ("Beta plan", 2), ("Gamma notes", 3), ("Delta", 4)]
    return [seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} {title}",
                             updated=n * DAY, created=n * DAY)
            for title, n in rows]


def filtered(api, tag, expr):
    # the tag clause confines the result to this test's chats
    resp = api.get("/v1/chats" + qs(_filter=f"contains(title,'{tag}') and ({expr})"))
    assert resp.status_code == 200, f"{expr}: {resp.text}"
    return ids(resp.json()["items"])


def test_list_filter_by_title_operators(api, seed):
    tag = unique_tag()
    alpha, beta, gamma, delta = filter_fixture(seed, tag)
    assert filtered(api, tag, f"title eq '{tag} Beta plan'") == [beta]
    assert filtered(api, tag, f"title ne '{tag} Beta plan'") == [delta, gamma, alpha]
    assert filtered(api, tag, "contains(title,'plan')") == [beta, alpha]
    assert filtered(api, tag, f"startswith(title,'{tag} Gam')") == [gamma]
    assert filtered(api, tag, "endswith(title,'notes')") == [gamma]
    assert filtered(api, tag, f"title in ('{tag} Alpha plan','{tag} Delta')") == [delta, alpha]
    assert filtered(api, tag, "title eq 'nothing'") == []


def test_list_filter_by_updated_at_operators(api, seed):
    tag = unique_tag()
    alpha, beta, gamma, delta = filter_fixture(seed, tag)  # 2025-01-02 .. 2025-01-05
    mid = "2025-01-03T12:00:00Z"
    assert filtered(api, tag, f"updated_at gt {mid}") == [delta, gamma]
    assert filtered(api, tag, f"updated_at ge {mid}") == [delta, gamma]
    assert filtered(api, tag, f"updated_at lt {mid}") == [beta, alpha]
    assert filtered(api, tag, f"updated_at le {mid}") == [beta, alpha]
    assert filtered(api, tag, f"updated_at gt 2025-01-02T12:00:00Z and updated_at lt {mid}") == [beta]


def test_list_filter_by_id_operators(api, seed):
    tag = unique_tag()
    alpha, beta, gamma, delta = filter_fixture(seed, tag)
    assert filtered(api, tag, f"id eq {beta}") == [beta]
    assert filtered(api, tag, f"id ne {beta}") == [delta, gamma, alpha]
    assert filtered(api, tag, f"id in ({alpha},{gamma})") == [gamma, alpha]


def test_list_filter_combines_with_or_and_ordering(api, seed):
    tag = unique_tag()
    alpha, beta, _gamma, delta = filter_fixture(seed, tag)
    assert filtered(api, tag, f"title eq '{tag} Delta' or title eq '{tag} Alpha plan'") == [
        delta, alpha]
    asc = list_all(api, "/v1/chats", _orderby="title asc",
                   _filter=f"contains(title,'{tag}') and contains(title,'plan')")
    assert ids(asc) == [alpha, beta]


@pytest.mark.parametrize("expr", [
    "model eq 'mock-premium'", "user_id eq 5", "message_count gt 1", "tenant_id eq 5",
    "title eq", "title eqq 'x'", "title eq 'unterminated", "updated_at gt notadate",
    "((title eq 'x')", "id eq not-a-uuid"])
def test_list_filter_with_an_unknown_field_or_bad_syntax_is_400_invalid_filter(api, expr):
    assert_odata_problem(api.get("/v1/chats" + qs(_filter=expr)), "$filter", "INVALID_FILTER")


# ---- $select / unsupported options -----------------------------------------


@pytest.mark.parametrize("select", ["id", "id,title", "title,model"])
def test_list_select_is_accepted_and_ignored(api, seed, select):
    tag = unique_tag()
    seed_chats(seed, tag, 2)
    full = api.get("/v1/chats" + qs(**tagged(tag)))
    resp = api.get("/v1/chats" + qs(_select=select, **tagged(tag)))
    assert resp.status_code == 200, resp.text
    assert resp.json() == full.json(), "the full item is always returned"


@pytest.mark.parametrize("select", ["", " ", "id,id", ","])
def test_list_select_with_bad_syntax_is_400_invalid_select(api, select):
    assert_odata_problem(api.get("/v1/chats" + qs(_select=select)), "$select", "INVALID_SELECT")


@pytest.mark.parametrize("option", ["$skip", "$count", "$expand", "$bogus"])
def test_list_unsupported_odata_options_are_400(api, option):
    resp = api.get("/v1/chats?" + option + "=1")
    assert_odata_problem(resp, option, "UNSUPPORTED_QUERY_PARAM")


# --------------------------------------------------------------------------- #
# ordering reflects activity
# --------------------------------------------------------------------------- #


def test_renaming_a_chat_moves_it_to_the_top_of_the_list(api, seed):
    tag = unique_tag()
    oldest, middle, newest = seed_chats(seed, tag, 3, step=100)
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [newest, middle, oldest]

    resp = api.patch(f"/v1/chats/{oldest}", json={"title": f"{tag} old, renamed"})
    assert resp.status_code == 200, resp.text

    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [oldest, newest, middle]


def test_a_newly_created_chat_is_listed_first(api, seed):
    tag = unique_tag()
    (old,) = seed_chats(seed, tag, 1)
    created = create(api, title=f"{tag} fresh")
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [created["id"], old]


def test_a_rejected_rename_does_not_reorder_the_list(api, seed):
    tag = unique_tag()
    old, new = seed_chats(seed, tag, 2, step=100)
    assert api.patch(f"/v1/chats/{old}", json={"title": " "}).status_code == 400
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [new, old]


# --------------------------------------------------------------------------- #
# DELETE /v1/chats/{id}
# --------------------------------------------------------------------------- #


def wait_for_outbox_payload(seed, chat_id, timeout=3.0):
    """The chat-cleanup payload of ``chat_id`` while it is still held by the outbox body table."""
    deadline = time.monotonic() + timeout
    while True:
        for payload in seed.outbox_payloads():
            if isinstance(payload, dict) and payload.get("chat_id") == chat_id:
                return payload
        if time.monotonic() >= deadline:
            return None
        time.sleep(0.01)


def test_delete_returns_204_without_a_body_and_soft_deletes_the_chat(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="Bye")
    resp = api.delete(f"/v1/chats/{chat}")
    assert resp.status_code == 204, resp.text
    assert resp.content == b""
    row = seed.chat(chat)
    assert row is not None, "soft delete keeps the row"
    assert row["deleted_at"] is not None
    assert row["title"] == "Bye"


def test_delete_leaves_messages_reactions_and_attachment_rows_untouched(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    user_msg = seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=1)
    assistant = seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", created=2)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=assistant, reaction="like")
    attachment = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                        filename="doc.pdf")
    seed.link_attachment(tenant=TENANT_A_ID, chat=chat, message=user_msg, attachment=attachment)

    assert api.delete(f"/v1/chats/{chat}").status_code == 204

    messages = seed.messages_of(chat)
    assert len(messages) == 2 and all(m["deleted_at"] is None for m in messages)
    assert len(seed.reactions(assistant)) == 1
    assert seed.attachment(attachment)["deleted_at"] is None


def test_deleted_chat_is_hidden_from_get_patch_delete_and_list(api, seed):
    tag = unique_tag()
    keep, gone = seed_chats(seed, tag, 2, step=100)

    assert api.delete(f"/v1/chats/{gone}").status_code == 204

    for resp in (api.get(f"/v1/chats/{gone}"),
                 api.patch(f"/v1/chats/{gone}", json={"title": "x"}),
                 api.delete(f"/v1/chats/{gone}")):
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    assert ids(list_all(api, "/v1/chats", **tagged(tag))) == [keep]


def test_deleting_an_unknown_chat_is_404(api):
    problem = assert_problem(api.delete(f"/v1/chats/{uuid.uuid4()}"), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_delete_enqueues_a_chat_cleanup_message_with_the_documented_payload(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    assert api.delete(f"/v1/chats/{chat}").status_code == 204

    payload = wait_for_outbox_payload(seed, chat)
    assert payload is not None, "no chat-cleanup message was enqueued for the deleted chat"
    assert payload["tenant_id"] == TENANT_A_ID
    assert payload["chat_id"] == chat
    assert payload["reason"] == "chat_soft_delete"
    assert uuid.UUID(payload["system_request_id"]).version == 4
    assert parse_ts(payload["chat_deleted_at"]) == parse_ts(seed.chat(chat)["deleted_at"])


def test_a_second_delete_does_not_enqueue_another_message(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    assert api.delete(f"/v1/chats/{chat}").status_code == 204
    wait_for_outbox_payload(seed, chat)
    assert api.delete(f"/v1/chats/{chat}").status_code == 404
    time.sleep(0.3)
    assert sum(1 for p in seed.outbox_payloads()
               if isinstance(p, dict) and p.get("chat_id") == chat) <= 1


def test_delete_marks_every_attachment_of_the_chat_for_cleanup(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    other = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    mine = [seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                   filename=f"{status}.pdf", status=status)
            for status in ("ready", "uploaded", "pending", "failed")]
    mine.append(seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                       filename="photo.png", kind="image", thumbnail=True))
    foreign = seed.insert_attachment(tenant=TENANT_A_ID, chat=other, user=USER_A_ID,
                                     filename="other.pdf")

    assert api.delete(f"/v1/chats/{chat}").status_code == 204

    for attachment in mine:
        row = seed.attachment(attachment)
        # `pending` right after the delete; a cleanup worker may already have moved it on.
        assert row["cleanup_status"] in ("pending", "done", "failed"), dict(row)
        assert row["deleted_at"] is None, "attachment rows stay visible (ADR-0009)"
    assert seed.attachment(foreign)["cleanup_status"] is None


def test_delete_does_not_reopen_attachments_whose_cleanup_already_finished(api, seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    done = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                  filename="done.pdf", cleanup_status="done", deleted=True)
    failed = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                    filename="failed.pdf", cleanup_status="failed", deleted=True)

    assert api.delete(f"/v1/chats/{chat}").status_code == 204

    assert seed.attachment(done)["cleanup_status"] == "done"
    assert seed.attachment(failed)["cleanup_status"] == "failed"


def test_delete_does_not_touch_other_chats(api, seed):
    tag = unique_tag()
    gone, kept = seed_chats(seed, tag, 2, step=100)
    api.delete(f"/v1/chats/{gone}")
    row = seed.chat(kept)
    assert row["deleted_at"] is None
    assert parse_ts(row["updated_at"]) == parse_ts(iso(200))


# --------------------------------------------------------------------------- #
# path parameters and authentication
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("method,path", [("GET", "/v1/chats/not-a-uuid"),
                                         ("PATCH", "/v1/chats/not-a-uuid"),
                                         ("DELETE", "/v1/chats/not-a-uuid"),
                                         ("GET", "/v1/chats/12345/messages")])
def test_non_uuid_chat_id_is_400_invalid_path_params(api, method, path):
    kwargs = {"json": {"title": "x"}} if method == "PATCH" else {}
    problem = assert_problem(api.request(method, path, **kwargs), 400, "invalid_argument")
    assert any(v.get("reason") == "invalid_path_params" for v in field_violations(problem)), problem


@pytest.mark.parametrize("method,path", [("POST", "/v1/chats"), ("GET", "/v1/chats"),
                                         ("GET", f"/v1/chats/{uuid.uuid4()}"),
                                         ("DELETE", f"/v1/chats/{uuid.uuid4()}"),
                                         ("GET", f"/v1/chats/{uuid.uuid4()}/messages")])
def test_missing_bearer_token_is_401(api, method, path):
    kwargs = {"json": {}} if method == "POST" else {}
    resp = api.request(method, path, user=None, **kwargs)
    problem = assert_problem(resp, 401, "unauthenticated")
    assert problem["context"].get("reason") == "MISSING_BEARER"


# ---- $orderby=title over untitled chats (NULL-free sort key) -----------------


def mixed_title_chats(seed, tag):
    """Three untitled and two titled chats; returns ``(untitled ids, alpha, beta)``."""
    untitled = [seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, created=n * 100)
                for n in range(1, 4)]
    alpha = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Alpha", created=400)
    beta = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Beta", created=500)
    return untitled, alpha, beta


def only_ids(chat_ids):
    """Filter confining a list to exactly these chats (untitled chats carry no tag)."""
    return {"_filter": "id in (" + ",".join(chat_ids) + ")"}


def walk_backward(api, last_page, **query):
    """Ids of every page before ``last_page`` (via ``prev_cursor``) plus ``last_page`` itself."""
    collected = ids(last_page["items"])
    prev = last_page["page_info"].get("prev_cursor")
    for _ in range(100):
        if not prev:
            return collected
        resp = api.get("/v1/chats" + qs(limit=1, cursor=prev, **query))
        assert resp.status_code == 200, resp.text
        page = resp.json()
        collected = ids(page["items"]) + collected
        prev = page["page_info"].get("prev_cursor")
    raise AssertionError("backward walk did not terminate")


@pytest.mark.parametrize("direction", ["asc", "desc"])
def test_orderby_title_walk_visits_untitled_and_titled_chats_exactly_once(api, seed, direction):
    tag = unique_tag()
    untitled, alpha, beta = mixed_title_chats(seed, tag)
    everything = untitled + [alpha, beta]
    flt = only_ids(everything)

    full = ids(list_all(api, "/v1/chats", page_size=100, _orderby=f"title {direction}", **flt))
    assert sorted(full) == sorted(everything), "every chat exactly once"
    # untitled chats sort as the empty title: first ascending, last descending
    untitled_block = set(full[:3] if direction == "asc" else full[2:])
    assert untitled_block == set(untitled)
    assert (full[3:] if direction == "asc" else full[:2]) == (
        [alpha, beta] if direction == "asc" else [beta, alpha])

    # forward over next cursors, one chat per page
    walked = ids(list_all(api, "/v1/chats", page_size=1, _orderby=f"title {direction}", **flt))
    assert walked == full

    # backward over prev cursors from the last page
    cursor = None
    last = None
    for _ in range(100):
        params = dict(limit=1, _orderby=f"title {direction}", **flt)
        if cursor:
            params = dict(limit=1, cursor=cursor, **flt)
        last = api.get("/v1/chats" + qs(**params)).json()
        cursor = last["page_info"].get("next_cursor")
        if not cursor:
            break
    assert walk_backward(api, last, **flt) == full


# ---- timestamps: exact $filter and chronological order ----------------------


def mixed_width_chats(seed, tag):
    """Chats whose ``updated_at`` share one second with different fraction widths; chat ``n``
    is the n-th instant ascending. Seeded out of order. Returns ids ascending by time."""
    created = {}
    for index in [3, 0, 4, 2, 1]:
        secs, micros = MIXED_WIDTH_INSTANTS[index]
        created[index] = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID,
                                          title=f"{tag} t{index}", created=secs,
                                          created_micros=micros)
    return [created[i] for i in range(5)]


def api_stamps(api, tag, chat_ids):
    """``updated_at`` text of each chat exactly as the API renders it."""
    items = {i["id"]: i for i in list_all(api, "/v1/chats", **tagged(tag))}
    return [items[c]["updated_at"] for c in chat_ids]


def test_updated_at_filters_are_exact_on_the_timestamps_the_api_returns(api, seed):
    tag = unique_tag()
    chat_ids = mixed_width_chats(seed, tag)
    stamps = api_stamps(api, tag, chat_ids)
    for stamp, (secs, micros) in zip(stamps, MIXED_WIDTH_INSTANTS):
        assert parse_ts(stamp) == parse_ts(wire_ts(secs, micros))

    def newest_first(indexes):
        return [chat_ids[i] for i in sorted(indexes, reverse=True)]

    everyone = range(5)
    for i, stamp in enumerate(stamps):
        assert filtered(api, tag, f"updated_at eq {stamp}") == newest_first([i]), f"eq {stamp}"
        assert filtered(api, tag, f"updated_at ne {stamp}") == newest_first(
            [j for j in everyone if j != i]), f"ne {stamp}"
        assert filtered(api, tag, f"updated_at gt {stamp}") == newest_first(
            [j for j in everyone if j > i]), f"gt {stamp}"
        assert filtered(api, tag, f"updated_at ge {stamp}") == newest_first(
            [j for j in everyone if j >= i]), f"ge {stamp}"
        assert filtered(api, tag, f"updated_at lt {stamp}") == newest_first(
            [j for j in everyone if j < i]), f"lt {stamp}"
        assert filtered(api, tag, f"updated_at le {stamp}") == newest_first(
            [j for j in everyone if j <= i]), f"le {stamp}"
    assert filtered(api, tag, f"updated_at in ({stamps[0]},{stamps[2]},{stamps[4]})") == (
        newest_first([0, 2, 4]))


def test_same_second_timestamps_of_different_fraction_widths_are_ordered_chronologically(
        api, seed):
    tag = unique_tag()
    chat_ids = mixed_width_chats(seed, tag)
    descending = list(reversed(chat_ids))

    assert ids(list_all(api, "/v1/chats", page_size=100, **tagged(tag))) == descending
    for direction, expected in (("asc", chat_ids), ("desc", descending)):
        for page_size in (100, 1):
            walked = list_all(api, "/v1/chats", page_size=page_size,
                              _orderby=f"updated_at {direction}", **tagged(tag))
            assert ids(walked) == expected, f"updated_at {direction}, pages of {page_size}"


def test_stored_timestamps_use_the_canonical_utc_encoding(api, seed):
    tag = unique_tag()
    body = create(api, title=f"{tag} canonical")
    row = seed.chat(body["id"])
    for column in ("created_at", "updated_at"):
        assert re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{3}|\.\d{6})?\+00:00",
                            row[column]), f"{column} = {row[column]!r}"
        assert parse_ts(row[column]) == parse_ts(body[column])
