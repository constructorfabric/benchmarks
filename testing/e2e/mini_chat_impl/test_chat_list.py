"""Chat list: OData filtering, ordering, pagination (acceptance: Chat CRUD)."""

import time

import pytest

from mchelpers import RT_ODATA, assert_problem, nonce, parse_ts


@pytest.fixture()
def three_chats(api_for):
    """User tok-l1 is reserved for the pagination test (exactly 3 chats)."""
    a = api_for("tok-l1")
    existing = a.list_chats(limit=100).json()["items"]
    for c in existing:  # keep the test re-runnable on a reused server
        a.delete(f"/v1/chats/{c['id']}")
    chats = []
    for t in ("Alpha", "Beta", "Gamma"):
        chats.append(a.create_chat(title=t))
        time.sleep(0.02)
    return a, chats


# Acceptance: Chat CRUD — list shape, default order updated_at desc, default limit 20
def test_list_default_order_and_shape(three_chats):
    a, chats = three_chats
    r = a.list_chats()
    assert r.status_code == 200, r.text
    body = r.json()
    assert set(body) >= {"items", "page_info"}
    assert body["page_info"]["limit"] == 20
    ids = [c["id"] for c in body["items"]]
    assert ids == [chats[2]["id"], chats[1]["id"], chats[0]["id"]]
    for c in body["items"]:
        assert {"id", "model", "is_temporary", "message_count", "created_at", "updated_at"} <= set(c)
        assert "user_id" not in c and "tenant_id" not in c
    ups = [parse_ts(c["updated_at"]) for c in body["items"]]
    assert ups == sorted(ups, reverse=True)


# Acceptance: Chat CRUD — cursor pagination without duplicates
def test_list_pagination_with_cursor(three_chats):
    a, chats = three_chats
    p1 = a.list_chats(limit=2).json()
    assert len(p1["items"]) == 2
    assert p1["page_info"]["limit"] == 2
    cur = p1["page_info"]["next_cursor"]
    assert cur
    p2 = a.list_chats(limit=2, cursor=cur).json()
    assert len(p2["items"]) == 1
    ids = [c["id"] for c in p1["items"]] + [c["id"] for c in p2["items"]]
    assert len(set(ids)) == 3
    assert set(ids) == {c["id"] for c in chats}
    assert not p2["page_info"].get("next_cursor")


# Acceptance: Chat CRUD — limit above 100 is clamped, limit=0 is rejected
def test_list_limit_clamp_and_zero(api):
    r = api.list_chats(limit=1000)
    assert r.status_code == 200, r.text
    assert r.json()["page_info"]["limit"] == 100
    r = api.list_chats(limit=0)
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_LIMIT", resource_type=RT_ODATA)


# Acceptance: Chat CRUD — $filter / $orderby on supported fields
def test_list_filter_and_orderby(three_chats):
    a, chats = three_chats
    r = a.list_chats(**{"$filter": "title eq 'Beta'"})
    assert r.status_code == 200, r.text
    assert [c["id"] for c in r.json()["items"]] == [chats[1]["id"]]
    # OData allows the GUID literal unquoted or quoted (DESIGN uses the quoted form for messages).
    results = []
    for form in (f"id eq '{chats[0]['id']}'", f"id eq {chats[0]['id']}"):
        r = a.list_chats(**{"$filter": form})
        results.append((form, r.status_code, r.json().get("items") if r.status_code == 200 else r.text[:200]))
        if r.status_code == 200 and [c["id"] for c in r.json()["items"]] == [chats[0]["id"]]:
            break
    else:
        raise AssertionError(f"filter by id did not return exactly the chat: {results}")
    r = a.list_chats(**{"$orderby": "title asc"})
    assert r.status_code == 200, r.text
    assert [c["title"] for c in r.json()["items"]] == ["Alpha", "Beta", "Gamma"]
    r = a.list_chats(**{"$orderby": "title desc"})
    assert [c["title"] for c in r.json()["items"]] == ["Gamma", "Beta", "Alpha"]


# Acceptance: Chat CRUD — malformed / unsupported OData is 400 with the OData resource type
@pytest.mark.parametrize(
    "params",
    [
        {"$filter": "model eq 'x'"},
        {"$filter": "title eq"},
        {"$filter": "((("},
        {"$orderby": "model asc"},
        {"cursor": "not-a-valid-cursor-" + "x" * 8},
        {"$skip": "1"},
    ],
)
def test_list_bad_odata(api, params):
    r = api.list_chats(**params)
    assert_problem(r, 400, "invalid_argument", resource_type=RT_ODATA)


# Acceptance: Chat CRUD — unknown filter field reason
def test_list_unknown_filter_field_reason(api):
    r = api.list_chats(**{"$filter": "model eq 'x'"})
    body = assert_problem(r, 400, "invalid_argument", resource_type=RT_ODATA)
    reasons = [fv.get("reason") for fv in body["context"].get("field_violations", [])]
    assert "INVALID_FILTER" in reasons, body


# Acceptance: Chat CRUD — malformed cursor reason
def test_list_bad_cursor_reason(api):
    r = api.list_chats(cursor="%%%garbage%%%")
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_CURSOR", resource_type=RT_ODATA)


# Acceptance: Chat CRUD — $select is validated and ignored
def test_list_select_ignored(api):
    api.create_chat(title="select")
    r = api.list_chats(**{"$select": "id"})
    assert r.status_code == 200, r.text
    item = r.json()["items"][0]
    assert {"id", "model", "created_at", "updated_at", "message_count"} <= set(item)


# Acceptance: Chat CRUD — chat ordering reflects most recent activity (message, rename)
def test_list_ordering_reflects_activity(api_for):
    a = api_for("tok-l2")
    ca = a.create_chat(title="A " + nonce())
    time.sleep(0.05)
    cb = a.create_chat(title="B " + nonce())
    ids = [c["id"] for c in a.list_chats(limit=100).json()["items"]]
    assert ids.index(cb["id"]) < ids.index(ca["id"])
    before = a.chat(ca["id"])["updated_at"]
    time.sleep(0.05)
    assert a.stream(ca["id"], "bump " + nonce()).done
    assert parse_ts(a.chat(ca["id"])["updated_at"]) > parse_ts(before)
    ids = [c["id"] for c in a.list_chats(limit=100).json()["items"]]
    assert ids[0] == ca["id"]
    time.sleep(0.05)
    assert a.patch(f"/v1/chats/{cb['id']}", json={"title": "B renamed"}).status_code == 200
    ids = [c["id"] for c in a.list_chats(limit=100).json()["items"]]
    assert ids[0] == cb["id"]
