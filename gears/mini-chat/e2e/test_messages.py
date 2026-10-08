"""Messages API: listing, OData, response contract and counting.

Acceptance criteria covered:
* Messages API — "List messages with filtering, ordering, and pagination"
* Messages API — "Message response contract: identity, attachments, and reaction fields are always consistently present"
* Messages API — "Message count and chronological ordering are tracked correctly across turns"
"""

from __future__ import annotations

import uuid

from helpers import (
    RT_CHAT,
    RT_ODATA,
    api_ts,
    assert_not_found,
    assert_problem,
    get_chat,
    http_error,
    list_messages,
    new_chat,
    send_ok,
    stream_script,
    ub,
)


def _two_turns(srv):
    cid = new_chat(srv, "gpt-4.1-mini")
    s1, d1, _ = send_ok(srv, cid, "first question")
    s2, d2, _ = send_ok(srv, cid, "second question")
    return cid, s1, s2


def test_chronological_order_and_count(fresh):
    """Two completed turns: user, assistant, user, assistant; message_count == 4."""
    cid, s1, s2 = _two_turns(fresh)
    msgs = list_messages(fresh, cid)
    assert [m["role"] for m in msgs] == ["user", "assistant", "user", "assistant"]
    assert [m["content"] for m in msgs][0] == "first question"
    assert msgs[2]["content"] == "second question"
    assert msgs[1]["content"] == "Hello from mock"
    assert get_chat(fresh, cid)["message_count"] == 4
    # request_id correlation: user and assistant of a turn share the stream_started request_id.
    assert msgs[0]["request_id"] == msgs[1]["request_id"] == s1["request_id"]
    assert msgs[2]["request_id"] == msgs[3]["request_id"] == s2["request_id"]
    # Assistant message ids are the pre-allocated stream_started message ids.
    assert msgs[1]["id"] == s1["message_id"] and msgs[3]["id"] == s2["message_id"]
    assert len(set(m["id"] for m in msgs)) == 4
    created = [api_ts(m["created_at"]) for m in msgs]
    assert created == sorted(created), "default order is created_at asc"


def test_message_contract_fields(fresh):
    """Consistent fields: request_id, attachments [], my_reaction null; assistant has model and token counts."""
    cid, s1, _ = _two_turns(fresh)
    msgs = list_messages(fresh, cid)
    for m in msgs:
        assert {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"} <= set(m), m
        assert m["request_id"] is not None
        uuid.UUID(m["request_id"])
        assert m["attachments"] == []
        assert m["my_reaction"] is None
    user, asst = msgs[0], msgs[1]
    assert "model" not in user or user["model"] is None
    assert "input_tokens" not in user, "zero token counts are omitted"
    assert "output_tokens" not in user
    assert asst["model"] == "gpt-4.1-mini"
    assert asst["input_tokens"] == 100 and asst["output_tokens"] == 50


def test_null_request_id_fails_with_500(fresh):
    """A stored message with a null request_id fails the request (never serialized as null)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "q")
    fresh.execute("UPDATE messages SET request_id = NULL WHERE chat_id = ? AND role = 'user'", (ub(cid),))
    r = fresh.req("GET", f"/chats/{cid}/messages")
    assert_problem(r, 500)


def test_filter_by_id_and_role(fresh):
    cid, s1, s2 = _two_turns(fresh)
    asst_id = s2["message_id"]
    r = fresh.req("GET", f"/chats/{cid}/messages", params={"$filter": f"id eq '{asst_id}'"})
    assert r.status_code == 200, r.text
    items = r.json()["items"]
    assert [m["id"] for m in items] == [asst_id]
    r = fresh.req("GET", f"/chats/{cid}/messages", params={"$filter": "role eq 'user'"})
    assert r.status_code == 200, r.text
    assert [m["content"] for m in r.json()["items"]] == ["first question", "second question"]


def test_orderby_desc(fresh):
    cid, _, _ = _two_turns(fresh)
    r = fresh.req("GET", f"/chats/{cid}/messages", params={"$orderby": "created_at desc"})
    assert r.status_code == 200, r.text
    roles = [m["role"] for m in r.json()["items"]]
    contents = [m["content"] for m in r.json()["items"]]
    assert roles == ["assistant", "user", "assistant", "user"]
    assert contents[1] == "second question" and contents[3] == "first question"


def test_pagination(fresh):
    cid, _, _ = _two_turns(fresh)
    seen: list[dict] = []
    cursor = None
    pages = 0
    while True:
        params = {"limit": 3}
        if cursor:
            params["cursor"] = cursor
        r = fresh.req("GET", f"/chats/{cid}/messages", params=params)
        assert r.status_code == 200, r.text
        j = r.json()
        assert j["page_info"]["limit"] == 3
        seen.extend(j["items"])
        pages += 1
        cursor = j["page_info"].get("next_cursor")
        if not cursor:
            break
    assert pages == 2
    assert [m["role"] for m in seen] == ["user", "assistant", "user", "assistant"]
    r = fresh.req("GET", f"/chats/{cid}/messages", params={"limit": 500})
    assert r.status_code == 200 and r.json()["page_info"]["limit"] == 100


def test_invalid_odata_rejected(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    for params in ({"limit": 0}, {"$filter": "nope eq 1"}, {"$orderby": "content asc"}, {"cursor": "garbage"}):
        r = fresh.req("GET", f"/chats/{cid}/messages", params=params)
        assert_problem(r, 400, resource_type=RT_ODATA)


def test_messages_of_unknown_chat_404(fresh):
    assert_not_found(fresh.req("GET", f"/chats/{uuid.uuid4()}/messages"), RT_CHAT)


def test_count_failed_turn_keeps_user_message_only(fresh):
    """A failed turn keeps only the user message."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "ok turn")
    fresh.mock_script(http_error(500, "boom"))
    r, events = fresh.stream(cid, "failing turn")
    assert r.status_code == 200 and events[-1].event == "error"
    msgs = list_messages(fresh, cid)
    assert [m["role"] for m in msgs] == ["user", "assistant", "user"]
    assert get_chat(fresh, cid)["message_count"] == 3


def test_count_after_mutations(fresh):
    """Turn mutations soft-delete the replaced turn's messages."""
    cid, s1, s2 = _two_turns(fresh)
    r, events = fresh.sse("POST", f"/chats/{cid}/turns/{s2['request_id']}/retry")
    assert r.status_code == 200 and events[-1].event == "done", r.text[:500]
    assert get_chat(fresh, cid)["message_count"] == 4
    new_rid = events[0].data["request_id"]
    r = fresh.req("DELETE", f"/chats/{cid}/turns/{new_rid}")
    assert r.status_code == 204, r.text
    assert get_chat(fresh, cid)["message_count"] == 2
    assert [m["content"] for m in list_messages(fresh, cid)] == ["first question", "Hello from mock"]


def test_incomplete_response_persists_message(fresh):
    """response.incomplete completes the turn; the assistant message is persisted even when empty."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script(terminal="incomplete", usage={"input_tokens": 10, "output_tokens": 0}))
    started, done, _ = send_ok(fresh, cid, "q")
    msgs = list_messages(fresh, cid)
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    assert msgs[1]["content"] == ""
    assert msgs[1]["id"] == started["message_id"]
