"""Chat CRUD, listing (OData filter/order/pagination) and activity ordering.

Acceptance criteria covered:
* Chat CRUD — "Create / get / list / update / delete lifecycle for chats, including model and title validation"
* Chat CRUD — "List endpoint supports filtering, ordering, and pagination, with validation of malformed input"
* Chat CRUD — "Chat ordering reflects most recent activity"
"""

from __future__ import annotations

import time
import uuid

from helpers import (
    RT_CHAT,
    RT_ODATA,
    assert_not_found,
    assert_problem,
    chat_row,
    get_chat,
    new_chat,
    api_ts,
    send_ok,
)

CHAT_KEYS = {"id", "model", "is_temporary", "message_count", "created_at", "updated_at"}


def _assert_chat_detail(c: dict) -> None:
    assert CHAT_KEYS <= set(c), c
    assert "user_id" not in c and "tenant_id" not in c, c
    assert c["is_temporary"] is False
    assert "title" not in c or isinstance(c["title"], str), "title must be omitted, never null"
    uuid.UUID(c["id"])


# ── create ────────────────────────────────────────────────────────────────
def test_create_chat_default_model_location_and_shape(fresh):
    """Create lifecycle: default model, 201 + Location, ChatDetail shape, untitled chat omits title."""
    r = fresh.req("POST", "/chats", json={})
    assert r.status_code == 201, r.text
    c = r.json()
    _assert_chat_detail(c)
    assert c["model"] == "gpt-4.1", "absent model resolves to the enabled is_default entry"
    assert "title" not in c
    assert c["message_count"] == 0
    assert r.headers.get("location") == f"/mini-chat/v1/chats/{c['id']}"
    # Persisted with the owner identity.
    row = chat_row(fresh, c["id"])
    assert row["deleted_at"] is None


def test_create_chat_with_model_and_trimmed_title(fresh):
    c = fresh.create_chat(model="gpt-4.1-mini", title="  My chat  ")
    assert c["model"] == "gpt-4.1-mini"
    assert c["title"] == "My chat"
    assert get_chat(fresh, c["id"])["title"] == "My chat"


def test_create_chat_null_title_is_untitled(fresh):
    r = fresh.req("POST", "/chats", json={"title": None})
    assert r.status_code == 201, r.text
    assert "title" not in r.json()


def test_create_chat_rejects_disabled_and_unknown_model(fresh):
    for model in ("disabled-model", "no-such-model"):
        r = fresh.req("POST", "/chats", json={"model": model})
        assert_problem(r, 400, field_reason="INVALID_MODEL", field="model")


def test_create_chat_title_validation(fresh):
    for title in ("", "   ", "x" * 256):
        r = fresh.req("POST", "/chats", json={"title": title})
        assert_problem(r, 400, field_reason="INVALID_TITLE", field="title")
    # Exactly 255 characters is accepted.
    r = fresh.req("POST", "/chats", json={"title": "y" * 255})
    assert r.status_code == 201, r.text
    assert len(r.json()["title"]) == 255


def test_create_chat_title_validated_before_model(fresh):
    """The title is validated before the model lookup."""
    r = fresh.req("POST", "/chats", json={"title": "   ", "model": "no-such-model"})
    assert_problem(r, 400, field_reason="INVALID_TITLE")


# ── get / update / delete ────────────────────────────────────────────────
def test_get_unknown_chat_404(fresh):
    r = fresh.req("GET", f"/chats/{uuid.uuid4()}")
    assert_not_found(r, RT_CHAT)


def test_update_title_model_immutable_and_updated_at_bumped(fresh):
    c = fresh.create_chat(model="gpt-4.1-mini", title="Before")
    time.sleep(0.05)
    r = fresh.req("PATCH", f"/chats/{c['id']}", json={"title": "  Renamed ", "model": "gpt-4.1", "unknown": 1})
    assert r.status_code == 200, r.text
    u = r.json()
    _assert_chat_detail(u)
    assert u["title"] == "Renamed"
    assert u["model"] == "gpt-4.1-mini", "PATCH must never change the chat model"
    assert api_ts(u["updated_at"]) >= api_ts(c["updated_at"])
    assert u["updated_at"] != c["updated_at"]
    g = get_chat(fresh, c["id"])
    assert g["title"] == "Renamed" and g["model"] == "gpt-4.1-mini"


def test_update_title_validation(fresh):
    cid = new_chat(fresh)
    for title in ("", "  ", "z" * 256):
        assert_problem(fresh.req("PATCH", f"/chats/{cid}", json={"title": title}), 400, field_reason="INVALID_TITLE")
    # Missing / null / wrong-typed title: schema mismatch → 422.
    for body in ({}, {"title": None}, {"title": 5}):
        r = fresh.req("PATCH", f"/chats/{cid}", json=body)
        assert r.status_code == 422, r.text


def test_update_unknown_chat_404(fresh):
    assert_not_found(fresh.req("PATCH", f"/chats/{uuid.uuid4()}", json={"title": "x"}), RT_CHAT)


def test_delete_chat_twice(fresh):
    cid = new_chat(fresh)
    r = fresh.req("DELETE", f"/chats/{cid}")
    assert r.status_code == 204, r.text
    assert chat_row(fresh, cid)["deleted_at"] is not None, "soft delete"
    assert_not_found(fresh.req("DELETE", f"/chats/{cid}"), RT_CHAT)
    assert_not_found(fresh.req("GET", f"/chats/{cid}"), RT_CHAT)
    assert_not_found(fresh.req("PATCH", f"/chats/{cid}", json={"title": "x"}), RT_CHAT)
    assert_not_found(fresh.req("GET", f"/chats/{cid}/messages"), RT_CHAT)
    r, _ = fresh.stream(cid, "hi")
    assert_not_found(r, RT_CHAT)
    assert fresh.chat_requests() == []
    ids = [c["id"] for c in _all_chats(fresh)]
    assert cid not in ids


# ── list ───────────────────────────────────────────────────────────────────
def _all_chats(srv, user="a1", **params) -> list[dict]:
    items: list[dict] = []
    cursor = None
    for _ in range(200):
        p = {"limit": 100, **params}
        if cursor:
            p["cursor"] = cursor
        r = srv.req("GET", "/chats", user, params=p)
        assert r.status_code == 200, r.text
        j = r.json()
        items.extend(j["items"])
        cursor = j["page_info"].get("next_cursor")
        if not cursor:
            break
    return items


def test_list_shape_default_order_and_pagination(fresh):
    ids = [new_chat(fresh, title=f"page-{i}") for i in range(3)]
    r = fresh.req("GET", "/chats", params={"limit": 2})
    assert r.status_code == 200, r.text
    j = r.json()
    assert set(j) >= {"items", "page_info"}
    assert j["page_info"]["limit"] == 2
    assert len(j["items"]) == 2
    for c in j["items"]:
        _assert_chat_detail(c)
    # Newest activity first: the last created chat leads the list.
    assert j["items"][0]["id"] == ids[2] and j["items"][1]["id"] == ids[1]
    assert j["page_info"]["next_cursor"], "a next page exists"
    r2 = fresh.req("GET", "/chats", params={"limit": 2, "cursor": j["page_info"]["next_cursor"]})
    assert r2.status_code == 200, r2.text
    j2 = r2.json()
    assert j2["items"][0]["id"] == ids[0], "the cursor continues after the first page"
    assert not ({c["id"] for c in j2["items"]} & {c["id"] for c in j["items"]})
    # Walking all pages yields every chat once, ordered by updated_at desc, id desc.
    walked: list[dict] = []
    cursor = None
    while True:
        p = {"limit": 2}
        if cursor:
            p["cursor"] = cursor
        page = fresh.req("GET", "/chats", params=p).json()
        walked.extend(page["items"])
        cursor = page["page_info"].get("next_cursor")
        if not cursor:
            break
    wids = [c["id"] for c in walked]
    assert len(wids) == len(set(wids))
    assert set(ids) <= set(wids)
    keys = [(api_ts(c["updated_at"]), c["id"]) for c in walked]
    assert keys == sorted(keys, reverse=True)


def test_list_limit_clamped_and_zero_rejected(fresh):
    new_chat(fresh)
    r = fresh.req("GET", "/chats", params={"limit": 1000})
    assert r.status_code == 200, r.text
    assert r.json()["page_info"]["limit"] == 100
    assert len(r.json()["items"]) <= 100
    r = fresh.req("GET", "/chats", params={"limit": 0})
    assert_problem(r, 400, field_reason="INVALID_LIMIT", resource_type=RT_ODATA)


def test_list_filter_and_orderby(fresh):
    tag = uuid.uuid4().hex[:8]
    a = new_chat(fresh, title=f"flt-{tag}-a")
    b = new_chat(fresh, title=f"flt-{tag}-b")
    r = fresh.req("GET", "/chats", params={"$filter": f"title eq 'flt-{tag}-a'"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [a]
    r = fresh.req("GET", "/chats", params={"$filter": f"id eq '{b}'"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [b]
    r = fresh.req("GET", "/chats", params={"$filter": f"title eq 'flt-{tag}-a' or title eq 'flt-{tag}-b'", "$orderby": "title asc"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [a, b]
    r = fresh.req("GET", "/chats", params={"$filter": f"title eq 'flt-{tag}-a' or title eq 'flt-{tag}-b'", "$orderby": "title desc"})
    assert [c["id"] for c in r.json()["items"]] == [b, a]
    # $select is accepted and ignored (full items).
    r = fresh.req("GET", "/chats", params={"$filter": f"id eq '{a}'", "$select": "id"})
    assert r.status_code == 200, r.text
    _assert_chat_detail(r.json()["items"][0])


def test_list_malformed_query_rejected(fresh):
    bad = [
        {"$filter": "bogus eq 1"},
        {"$filter": "title eq"},
        {"$orderby": "nonexistent desc"},
        {"cursor": "not-a-valid-cursor"},
        {"limit": 0},
    ]
    for params in bad:
        r = fresh.req("GET", "/chats", params=params)
        j = assert_problem(r, 400, resource_type=RT_ODATA)
        assert j["context"]["resource_type"] != RT_CHAT


def test_list_order_reflects_latest_activity(fresh):
    """Chat ordering reflects most recent activity (send bumps updated_at)."""
    older = new_chat(fresh, "gpt-4.1-mini")
    newer = new_chat(fresh, "gpt-4.1-mini")
    first = fresh.req("GET", "/chats", params={"limit": 2}).json()["items"]
    assert first[0]["id"] == newer
    before = get_chat(fresh, older)["updated_at"]
    send_ok(fresh, older, "bump me")
    items = fresh.req("GET", "/chats", params={"limit": 2}).json()["items"]
    assert items[0]["id"] == older, "the chat with the latest message is listed first"
    assert get_chat(fresh, older)["updated_at"] != before
    # Rename also bumps activity.
    fresh.req("PATCH", f"/chats/{newer}", json={"title": "renamed"})
    assert fresh.req("GET", "/chats", params={"limit": 1}).json()["items"][0]["id"] == newer


def test_message_count_on_chat(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "one")
    assert get_chat(fresh, cid)["message_count"] == 2
    listed = [c for c in _all_chats(fresh, **{"$filter": f"id eq '{cid}'"})]
    assert listed and listed[0]["message_count"] == 2
