"""Message list (DESIGN section 3.3, List Messages)."""

import uuid

import pytest

from ._seed import insert_message, ts
from .helpers import PREFIX, TOKEN_A, api, assert_problem, create_chat

pytestmark = pytest.mark.usefixtures("server")

ODATA_TYPE = "gts.cf.core.odata.query.v1~"


def test_empty_list_shape():
    s = api()
    chat = create_chat(s)
    r = s.get(f"{PREFIX}/chats/{chat['id']}/messages")
    assert r.status_code == 200, r.text
    assert r.json() == {"items": [], "page_info": {"limit": 20, "next_cursor": None, "prev_cursor": None}}


def test_select_is_accepted_and_ignored():
    s = api()
    chat = create_chat(s)
    insert_message(chat["id"], "user", ts(0))
    r = s.get(f"{PREFIX}/chats/{chat['id']}/messages", params={"$select": "id,content"})
    assert r.status_code == 200, r.text
    item = r.json()["items"][0]
    assert {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"} <= set(item)


@pytest.mark.parametrize("params,reason", [
    ({"$skip": 1}, "UNSUPPORTED_QUERY_PARAM"),
    ({"limit": 0}, "INVALID_LIMIT"),
    ({"cursor": "garbage!"}, "INVALID_CURSOR"),
    ({"$filter": "content eq 'x'"}, "INVALID_FILTER"),
    ({"$orderby": "content asc"}, "INVALID_ORDERBY_FIELD"),
])
def test_bad_query_is_400(params, reason):
    s = api()
    chat = create_chat(s)
    r = s.get(f"{PREFIX}/chats/{chat['id']}/messages", params=params)
    assert_problem(r, 400, "invalid_argument", reason=reason, resource_type=ODATA_TYPE)


def test_messages_are_chronological_and_paginate_within_one_second():
    s = api()
    chat = create_chat(s)
    ids = [insert_message(chat["id"], "user" if i % 2 == 0 else "assistant", ts(0, i)) for i in range(5)]

    first = s.get(f"{PREFIX}/chats/{chat['id']}/messages", params={"limit": 3}).json()
    assert [m["id"] for m in first["items"]] == ids[:3]
    cursor = first["page_info"]["next_cursor"]
    second = s.get(f"{PREFIX}/chats/{chat['id']}/messages", params={"limit": 3, "cursor": cursor}).json()
    assert [m["id"] for m in second["items"]] == ids[3:]
    assert second["page_info"]["next_cursor"] is None

    assistants = s.get(f"{PREFIX}/chats/{chat['id']}/messages", params={"$filter": "role eq 'assistant'"}).json()
    assert [m["id"] for m in assistants["items"]] == [ids[1], ids[3]]
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["message_count"] == 5


def test_message_dto_fields():
    s = api(TOKEN_A)
    chat = create_chat(s)
    request_id = str(uuid.uuid4())
    insert_message(chat["id"], "user", ts(0), content="question", request_id=request_id)
    insert_message(chat["id"], "assistant", ts(1), content="answer", request_id=request_id,
                   input_tokens=12, output_tokens=5, model="gpt-premium")

    items = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    user, assistant = items
    assert user["role"] == "user" and user["content"] == "question"
    assert user["request_id"] == request_id == assistant["request_id"]
    assert user["attachments"] == [] and user["my_reaction"] is None
    for absent in ("model", "input_tokens", "output_tokens"):
        assert absent not in user
    assert assistant["model"] == "gpt-premium"
    assert (assistant["input_tokens"], assistant["output_tokens"]) == (12, 5)
    assert assistant["my_reaction"] is None


def test_unknown_chat_is_404():
    r = api().get(f"{PREFIX}/chats/{uuid.uuid4()}/messages")
    assert_problem(r, 404, "not_found", resource_type="gts.cf.core.mini_chat.chat.v1~")
