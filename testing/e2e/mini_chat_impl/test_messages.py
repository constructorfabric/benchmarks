"""Messages API (acceptance: Messages API)."""

import uuid

import pytest

from mchelpers import RT_CHAT, RT_ODATA, assert_problem, nonce, parse_ts

MSG_REQUIRED = {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}


@pytest.fixture()
def two_turn_chat(api):
    c = api.create_chat()
    n = nonce()
    s1 = api.stream(c["id"], f"first question {n}")
    assert s1.done
    s2 = api.stream(c["id"], f"second question {n}")
    assert s2.done
    return c, s1, s2, n


# Acceptance: Messages API — chronological order across turns
def test_list_messages_chronological(api, two_turn_chat):
    c, s1, s2, n = two_turn_chat
    items = api.messages(c["id"])
    assert [m["role"] for m in items] == ["user", "assistant", "user", "assistant"]
    assert items[0]["content"] == f"first question {n}"
    assert items[1]["content"] == "Hello world"
    assert items[2]["content"] == f"second question {n}"
    ts = [parse_ts(m["created_at"]) for m in items]
    assert ts == sorted(ts)


# Acceptance: Messages API — response contract: identity, attachments, reaction fields
def test_message_contract_fields(api, two_turn_chat):
    c, s1, s2, n = two_turn_chat
    items = api.messages(c["id"])
    for m in items:
        assert MSG_REQUIRED <= set(m), m
        assert m["attachments"] == []
        assert m["my_reaction"] is None
        assert m["request_id"]
        uuid.UUID(m["request_id"])
        uuid.UUID(m["id"])
    # user and assistant of one turn share the request id, which equals stream_started.request_id
    assert items[0]["request_id"] == items[1]["request_id"] == s1.request_id
    assert items[2]["request_id"] == items[3]["request_id"] == s2.request_id
    assert items[0]["request_id"] != items[2]["request_id"]
    # assistant fields
    a1 = items[1]
    assert a1["id"] == s1.message_id
    assert a1["model"] == s1.done["effective_model"]
    assert a1["input_tokens"] == 10 and a1["output_tokens"] == 5
    # user messages carry no model
    assert items[0].get("model") is None


# Acceptance: Messages API — token fields omitted when zero
def test_token_fields_omitted_when_zero(api):
    c = api.create_chat()
    assert api.stream(c["id"], "zero usage [[usage:0:0]] " + nonce()).done
    a = api.messages(c["id"])[-1]
    assert a["role"] == "assistant"
    assert a.get("input_tokens") is None and a.get("output_tokens") is None


# Acceptance: Messages API — filtering by id and role
def test_filter_messages(api, two_turn_chat):
    c, s1, s2, n = two_turn_chat
    r = api.get(f"/v1/chats/{c['id']}/messages", params={"$filter": f"id eq '{s2.message_id}'"})
    assert r.status_code == 200, r.text
    items = r.json()["items"]
    assert [m["id"] for m in items] == [s2.message_id]
    r = api.get(f"/v1/chats/{c['id']}/messages", params={"$filter": "role eq 'assistant'"})
    assert r.status_code == 200, r.text
    assert [m["role"] for m in r.json()["items"]] == ["assistant", "assistant"]


# Acceptance: Messages API — ordering override
def test_orderby_desc(api, two_turn_chat):
    c, *_ = two_turn_chat
    asc = [m["id"] for m in api.messages(c["id"])]
    r = api.get(f"/v1/chats/{c['id']}/messages", params={"$orderby": "created_at desc"})
    assert r.status_code == 200, r.text
    assert [m["id"] for m in r.json()["items"]] == list(reversed(asc))


# Acceptance: Messages API — cursor pagination
def test_messages_pagination(api, two_turn_chat):
    c, *_ = two_turn_chat
    seen = []
    cursor = None
    for _ in range(10):
        params = {"limit": 1}
        if cursor:
            params["cursor"] = cursor
        r = api.get(f"/v1/chats/{c['id']}/messages", params=params)
        assert r.status_code == 200, r.text
        body = r.json()
        assert body["page_info"]["limit"] == 1
        seen += [m["id"] for m in body["items"]]
        cursor = body["page_info"].get("next_cursor")
        if not cursor:
            break
    assert len(seen) == 4 and len(set(seen)) == 4
    assert seen == [m["id"] for m in api.messages(c["id"])]


# Acceptance: Messages API — limit validation and malformed OData
def test_messages_bad_query(api, two_turn_chat):
    c, *_ = two_turn_chat
    r = api.get(f"/v1/chats/{c['id']}/messages", params={"limit": 0})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_LIMIT", resource_type=RT_ODATA)
    r = api.get(f"/v1/chats/{c['id']}/messages", params={"limit": 500})
    assert r.status_code == 200 and r.json()["page_info"]["limit"] == 100
    for params in ({"$filter": "content eq 'x'"}, {"$orderby": "content asc"}, {"cursor": "bogus-cursor"}, {"$filter": "role eq"}):
        r = api.get(f"/v1/chats/{c['id']}/messages", params=params)
        assert_problem(r, 400, "invalid_argument", resource_type=RT_ODATA)


# Acceptance: Messages API — unknown chat
def test_messages_unknown_chat(api):
    assert_problem(api.get(f"/v1/chats/{uuid.uuid4()}/messages"), 404, "not_found", resource_type=RT_CHAT)


# Acceptance: Messages API — message count and ordering across turns and a delete
def test_message_count_two_turns_then_delete(api, two_turn_chat):
    c, s1, s2, n = two_turn_chat
    assert api.chat(c["id"])["message_count"] == 4
    r = api.delete(f"/v1/chats/{c['id']}/turns/{s2.request_id}")
    assert r.status_code == 204, r.text
    assert api.chat(c["id"])["message_count"] == 2
    items = api.messages(c["id"])
    assert [m["request_id"] for m in items] == [s1.request_id, s1.request_id]
    assert [m["role"] for m in items] == ["user", "assistant"]
