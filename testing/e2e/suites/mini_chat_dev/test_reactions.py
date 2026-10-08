"""Message reactions (DESIGN section 3.3, Message Reaction API) and turn status."""

import uuid

import pytest

from ._seed import insert_message, insert_turn, ts
from .helpers import PREFIX, TOKEN_A_REVIEWER, api, assert_problem, create_chat

pytestmark = pytest.mark.usefixtures("server")

MESSAGE_TYPE = "gts.cf.core.mini_chat.message.v1~"
CHAT_TYPE = "gts.cf.core.mini_chat.chat.v1~"
TURN_TYPE = "gts.cf.core.mini_chat.turn.v1~"


def reaction_url(chat_id, msg_id):
    return f"{PREFIX}/chats/{chat_id}/messages/{msg_id}/reaction"


def test_missing_message_is_404_message():
    s = api()
    chat = create_chat(s)
    url = reaction_url(chat["id"], uuid.uuid4())
    assert_problem(s.put(url, json={"reaction": "like"}), 404, "not_found", resource_type=MESSAGE_TYPE)
    assert_problem(s.delete(url), 404, "not_found", resource_type=MESSAGE_TYPE)


def test_missing_chat_is_404_chat():
    url = reaction_url(uuid.uuid4(), uuid.uuid4())
    assert_problem(api().put(url, json={"reaction": "like"}), 404, "not_found", resource_type=CHAT_TYPE)


def test_invalid_reaction_value_is_400_before_lookup():
    s = api()
    url = reaction_url(uuid.uuid4(), uuid.uuid4())
    assert_problem(s.put(url, json={"reaction": "love"}), 400, "invalid_argument",
                   reason="INVALID_REACTION", field="reaction")
    assert_problem(s.put(url, json={}), 422, "invalid_argument", reason="invalid_json_body")
    assert_problem(s.put(url, data='{"reaction":"like"}', headers={"Content-Type": "text/plain"}),
                   415, "invalid_argument", reason="missing_json_content_type")
    assert_problem(s.put(reaction_url("nope", uuid.uuid4()), json={"reaction": "like"}), 400,
                   "invalid_argument", reason="invalid_path_params")


def test_set_replace_list_and_remove():
    s = api()
    chat = create_chat(s)
    msg = insert_message(chat["id"], "assistant", ts(0))
    url = reaction_url(chat["id"], msg)

    r = s.put(url, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    assert r.json()["message_id"] == msg and r.json()["reaction"] == "like"
    assert set(r.json()) == {"message_id", "reaction", "created_at"}
    r = s.put(url, json={"reaction": "dislike"})
    assert r.json()["reaction"] == "dislike"

    items = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    assert items[0]["my_reaction"] == "dislike"

    assert s.delete(url).status_code == 204
    assert s.delete(url).status_code == 204
    items = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    assert items[0]["my_reaction"] is None

    # Another user of the tenant cannot see the chat at all.
    assert_problem(api(TOKEN_A_REVIEWER).put(url, json={"reaction": "like"}), 404, "not_found",
                   resource_type=CHAT_TYPE)


def test_reaction_on_user_message_is_failed_precondition():
    s = api()
    chat = create_chat(s)
    msg = insert_message(chat["id"], "user", ts(0))
    url = reaction_url(chat["id"], msg)
    for r in (s.put(url, json={"reaction": "like"}), s.delete(url)):
        p = assert_problem(r, 400, "failed_precondition", violation_type="STATE")
        assert {v["subject"] for v in p["context"]["violations"]} == {"reaction_target"}


def test_turn_status():
    s = api()
    chat = create_chat(s)
    done_req, err_req, gone_req = (str(uuid.uuid4()) for _ in range(3))
    assistant = str(uuid.uuid4())
    insert_turn(chat["id"], done_req, "completed", assistant_message_id=assistant)
    insert_turn(chat["id"], err_req, "failed", error_code="provider_error", assistant_message_id=assistant)
    insert_turn(chat["id"], gone_req, "completed", deleted=True)
    base = f"{PREFIX}/chats/{chat['id']}/turns"

    done = s.get(f"{base}/{done_req}")
    assert done.status_code == 200, done.text
    assert done.json()["state"] == "done"
    assert done.json()["assistant_message_id"] == assistant
    assert "error_code" not in done.json()
    assert set(done.json()) == {"request_id", "state", "assistant_message_id", "updated_at"}

    err = s.get(f"{base}/{err_req}").json()
    assert err["state"] == "error" and err["error_code"] == "provider_error"
    assert "assistant_message_id" not in err

    assert_problem(s.get(f"{base}/{gone_req}"), 404, "not_found", resource_type=TURN_TYPE)
    assert_problem(s.get(f"{base}/{uuid.uuid4()}"), 404, "not_found", resource_type=TURN_TYPE)
    assert_problem(s.get(f"{base}/not-a-uuid"), 400, "invalid_argument", reason="invalid_path_params")
    assert_problem(api(TOKEN_A_REVIEWER).get(f"{base}/{done_req}"), 404, "not_found", resource_type=CHAT_TYPE)
