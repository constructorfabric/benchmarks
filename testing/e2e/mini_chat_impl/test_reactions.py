"""Reactions API (acceptance: Reactions API)."""

import uuid

import pytest

from mchelpers import RT_CHAT, RT_MESSAGE, assert_problem, nonce, parse_ts, ub, user_id


@pytest.fixture()
def turn(api):
    c = api.create_chat()
    s = api.stream(c["id"], "react to me " + nonce())
    assert s.done
    msgs = api.messages(c["id"])
    return c, msgs[0], msgs[1]


def _reaction_path(chat_id, msg_id):
    return f"/v1/chats/{chat_id}/messages/{msg_id}/reaction"


# Acceptance: Reactions — set like then dislike (upsert, one row)
def test_set_and_change_reaction(api, db, turn):
    c, user_msg, asst = turn
    r = api.put(_reaction_path(c["id"], asst["id"]), json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like"
    parse_ts(body["created_at"])
    assert api.messages(c["id"])[1]["my_reaction"] == "like"
    r = api.put(_reaction_path(c["id"], asst["id"]), json={"reaction": "dislike"})
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    assert api.messages(c["id"])[1]["my_reaction"] == "dislike"
    rows = db.query("SELECT * FROM message_reactions WHERE message_id = ?", (ub(asst["id"]),))
    assert len(rows) == 1
    assert rows[0]["reaction"] == "dislike"
    assert bytes(rows[0]["user_id"]) == ub(user_id("tok-a"))
    # idempotent repeat
    assert api.put(_reaction_path(c["id"], asst["id"]), json={"reaction": "dislike"}).status_code == 200
    assert len(db.query("SELECT * FROM message_reactions WHERE message_id = ?", (ub(asst["id"]),))) == 1
    # user message stays null
    assert api.messages(c["id"])[0]["my_reaction"] is None


# Acceptance: Reactions — remove is idempotent
def test_remove_reaction(api, turn):
    c, user_msg, asst = turn
    assert api.put(_reaction_path(c["id"], asst["id"]), json={"reaction": "like"}).status_code == 200
    r = api.delete(_reaction_path(c["id"], asst["id"]))
    assert r.status_code == 204 and r.content == b""
    assert api.messages(c["id"])[1]["my_reaction"] is None
    assert api.delete(_reaction_path(c["id"], asst["id"])).status_code == 204


# Acceptance: Reactions — invalid value (checked before authorization) and schema errors
def test_invalid_reaction(api, turn):
    c, user_msg, asst = turn
    r = api.put(_reaction_path(c["id"], asst["id"]), json={"reaction": "love"})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_REACTION", field="reaction")
    r = api.put(_reaction_path(uuid.uuid4(), uuid.uuid4()), json={"reaction": "love"})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_REACTION")
    r = api.put(_reaction_path(c["id"], asst["id"]), json={})
    assert_problem(r, 422, "invalid_argument")


# Acceptance: Reactions — only assistant messages
def test_reaction_on_user_message(api, turn):
    c, user_msg, asst = turn
    r = api.put(_reaction_path(c["id"], user_msg["id"]), json={"reaction": "like"})
    assert_problem(r, 400, "failed_precondition", violation_subject="reaction_target", violation_type="STATE")
    r = api.delete(_reaction_path(c["id"], user_msg["id"]))
    assert_problem(r, 400, "failed_precondition", violation_subject="reaction_target", violation_type="STATE")


# Acceptance: Reactions — unknown message / unknown chat
def test_reaction_not_found(api, turn):
    c, user_msg, asst = turn
    assert_problem(api.put(_reaction_path(c["id"], uuid.uuid4()), json={"reaction": "like"}), 404, "not_found", resource_type=RT_MESSAGE)
    assert_problem(api.delete(_reaction_path(c["id"], uuid.uuid4())), 404, "not_found", resource_type=RT_MESSAGE)
    assert_problem(api.put(_reaction_path(uuid.uuid4(), asst["id"]), json={"reaction": "like"}), 404, "not_found", resource_type=RT_CHAT)
    # a message of another chat is not found in this chat
    other = api.create_chat()
    assert_problem(api.put(_reaction_path(other["id"], asst["id"]), json={"reaction": "like"}), 404, "not_found", resource_type=RT_MESSAGE)
    assert_problem(api.put(_reaction_path(c["id"], "nope"), json={"reaction": "like"}), 400, "invalid_argument", field_reason="invalid_path_params")
