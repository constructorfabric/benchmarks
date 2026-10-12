"""Message reactions black-box: ``PUT`` / ``DELETE /v1/chats/{id}/messages/{msg_id}/reaction``.

Source: DESIGN 3.3 "Message Reaction API" (set / remove rules and errors), the REST error
table (INVALID_REACTION is checked before authorization; a body without ``reaction`` is 422;
``reaction_target`` is a ``failed_precondition`` for ``PUT`` and ``DELETE``), 3.7
``message_reactions`` (unique ``(message_id, user_id)``, tenant and user from the security
context), OpenAPI ``SetReactionReq`` / ``MiniChatReactionDto``.

Messages are seeded directly in the database (sending messages is a later slice).
"""

from __future__ import annotations

import uuid
from types import SimpleNamespace

import pytest

from .support import (
    RESOURCE_TYPES,
    TENANT_A_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    assert_problem,
    parse_ts,
    uuid_blob,
)


@pytest.fixture()
def world(seed):
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title="reactions")
    return SimpleNamespace(
        chat=chat,
        user_msg=seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", created=1),
        assistant=seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", created=2),
        system=seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="system", created=3),
    )


def path(chat, message):
    return f"/v1/chats/{chat}/messages/{message}/reaction"


def my_reaction(api, chat, message):
    items = api.get(f"/v1/chats/{chat}/messages?limit=100").json()["items"]
    return [m for m in items if m["id"] == message][0]["my_reaction"]


def has_violation(problem, field, reason):
    return any(v.get("field") == field and v.get("reason") == reason
               for v in problem["context"]["field_violations"])


def assert_reaction_target(resp):
    problem = assert_problem(resp, 400, "failed_precondition")
    assert any(v.get("subject") == "reaction_target" and v.get("type") == "STATE"
               for v in problem["context"]["violations"]), problem


# --------------------------------------------------------------------------- #
# PUT
# --------------------------------------------------------------------------- #


@pytest.mark.smoke
def test_put_like_on_an_assistant_message_returns_the_reaction(api, world):
    resp = api.put(path(world.chat, world.assistant), json={"reaction": "like"})

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    body = resp.json()
    assert set(body) == {"message_id", "reaction", "created_at"}
    assert body["message_id"] == world.assistant
    assert body["reaction"] == "like"
    parse_ts(body["created_at"])


def test_put_dislike_is_accepted_too(api, world):
    resp = api.put(path(world.chat, world.assistant), json={"reaction": "dislike"})
    assert resp.status_code == 200, resp.text
    assert resp.json()["reaction"] == "dislike"


def test_put_stores_one_row_for_the_caller_and_shows_it_as_my_reaction(api, seed, world):
    assert my_reaction(api, world.chat, world.assistant) is None
    api.put(path(world.chat, world.assistant), json={"reaction": "dislike"})

    (row,) = seed.reactions(world.assistant)
    assert bytes(row["user_id"]) == uuid_blob(USER_A_ID), "user_id comes from the security context"
    assert bytes(row["tenant_id"]) == uuid_blob(TENANT_A_ID)
    assert row["reaction"] == "dislike"
    assert my_reaction(api, world.chat, world.assistant) == "dislike"


def test_put_replaces_an_existing_reaction(api, seed, world):
    api.put(path(world.chat, world.assistant), json={"reaction": "like"})

    resp = api.put(path(world.chat, world.assistant), json={"reaction": "dislike"})

    assert resp.status_code == 200, resp.text
    assert resp.json()["reaction"] == "dislike"
    rows = seed.reactions(world.assistant)
    assert len(rows) == 1, "unique (message_id, user_id): replaced, not added"
    assert rows[0]["reaction"] == "dislike"
    assert my_reaction(api, world.chat, world.assistant) == "dislike"


def test_put_is_idempotent_for_the_same_value(api, seed, world):
    first = api.put(path(world.chat, world.assistant), json={"reaction": "like"})
    second = api.put(path(world.chat, world.assistant), json={"reaction": "like"})
    assert first.status_code == 200 and second.status_code == 200, second.text
    assert second.json()["reaction"] == "like"
    assert len(seed.reactions(world.assistant)) == 1


@pytest.mark.parametrize("target", ["user_msg", "system"])
def test_put_on_a_user_or_system_message_is_400_reaction_target(api, seed, world, target):
    message = getattr(world, target)
    resp = api.put(path(world.chat, message), json={"reaction": "like"})
    assert_reaction_target(resp)
    assert seed.reactions(message) == []


@pytest.mark.parametrize("value", ["love", "", "LIKE", "Like", "thumbs_up"])
def test_put_with_an_invalid_reaction_value_is_400_invalid_reaction(api, seed, world, value):
    resp = api.put(path(world.chat, world.assistant), json={"reaction": value})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "reaction", "INVALID_REACTION"), problem
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]
    assert seed.reactions(world.assistant) == []


def test_the_reaction_value_is_validated_before_any_lookup(api):
    # Neither the chat nor the message exists: the value check comes first (and before authz).
    resp = api.put(path(uuid.uuid4(), uuid.uuid4()), json={"reaction": "love"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "reaction", "INVALID_REACTION"), problem


@pytest.mark.parametrize("payload", ["{}", '{"reaction": null}', '{"reaction": 5}',
                                     '{"reaction": true}', '{"reaction": ["like"]}',
                                     '{"Reaction": "like"}', "null", "[]"])
def test_put_body_that_does_not_match_the_schema_is_422(api, seed, world, payload):
    resp = api.put(path(world.chat, world.assistant), content=payload,
                   headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 422, "invalid_argument")
    assert has_violation(problem, "body", "invalid_json_body"), problem
    assert seed.reactions(world.assistant) == []


def test_put_malformed_json_is_400_and_a_non_json_content_type_is_415(api, seed, world):
    uri = path(world.chat, world.assistant)
    resp = api.put(uri, content='{"reaction": "li', headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert has_violation(problem, "body", "json_syntax_error"), problem

    resp = api.put(uri, content='{"reaction": "like"}', headers={"Content-Type": "text/plain"})
    problem = assert_problem(resp, 415, "invalid_argument")
    assert has_violation(problem, "body", "missing_json_content_type"), problem
    assert seed.reactions(world.assistant) == []


def test_put_on_an_unknown_message_is_404_for_the_message_resource(api, world):
    resp = api.put(path(world.chat, uuid.uuid4()), json={"reaction": "like"})
    problem = assert_problem(resp, 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]


def test_put_on_a_soft_deleted_message_is_404_for_the_message_resource(api, seed, world):
    deleted = seed.insert_message(tenant=TENANT_A_ID, chat=world.chat, role="assistant",
                                  created=9, deleted=True)
    problem = assert_problem(api.put(path(world.chat, deleted), json={"reaction": "like"}), 404,
                             "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]


def test_put_on_a_message_of_another_chat_is_404_for_the_message_resource(api, seed, world):
    other = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    elsewhere = seed.insert_message(tenant=TENANT_A_ID, chat=other, role="assistant", created=1)
    resp = api.put(path(world.chat, elsewhere), json={"reaction": "like"})
    problem = assert_problem(resp, 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]
    assert seed.reactions(elsewhere) == []


def test_put_in_an_unknown_or_deleted_chat_is_404_for_the_chat_resource(api, seed, world):
    deleted_chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, deleted=True)
    deleted_msg = seed.insert_message(tenant=TENANT_A_ID, chat=deleted_chat, role="assistant",
                                      created=1)
    for chat, message in ((uuid.uuid4(), world.assistant), (deleted_chat, deleted_msg)):
        problem = assert_problem(api.put(path(chat, message), json={"reaction": "like"}), 404,
                                 "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    assert seed.reactions(deleted_msg) == []


# --------------------------------------------------------------------------- #
# DELETE
# --------------------------------------------------------------------------- #


def test_delete_removes_the_reaction_and_returns_204_without_a_body(api, seed, world):
    api.put(path(world.chat, world.assistant), json={"reaction": "like"})

    resp = api.delete(path(world.chat, world.assistant))

    assert resp.status_code == 204, resp.text
    assert resp.content == b""
    assert seed.reactions(world.assistant) == []
    assert my_reaction(api, world.chat, world.assistant) is None


def test_delete_is_idempotent_whether_or_not_a_reaction_existed(api, world):
    uri = path(world.chat, world.assistant)
    assert api.delete(uri).status_code == 204, "nothing to remove"
    api.put(uri, json={"reaction": "dislike"})
    assert api.delete(uri).status_code == 204
    assert api.delete(uri).status_code == 204


@pytest.mark.parametrize("target", ["user_msg", "system"])
def test_delete_on_a_user_or_system_message_is_400_reaction_target(api, world, target):
    assert_reaction_target(api.delete(path(world.chat, getattr(world, target))))


def test_delete_on_an_unknown_message_is_404_for_the_message_resource(api, world):
    problem = assert_problem(api.delete(path(world.chat, uuid.uuid4())), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]


def test_delete_on_a_message_of_another_chat_is_404_and_keeps_its_reaction(api, seed, world):
    other = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    elsewhere = seed.insert_message(tenant=TENANT_A_ID, chat=other, role="assistant", created=1)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=elsewhere, reaction="like")

    problem = assert_problem(api.delete(path(world.chat, elsewhere)), 404, "not_found")

    assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]
    assert len(seed.reactions(elsewhere)) == 1


def test_delete_in_an_unknown_chat_is_404_for_the_chat_resource(api, world):
    problem = assert_problem(api.delete(path(uuid.uuid4(), world.assistant)), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]


def test_delete_removes_only_the_callers_reaction(api, seed, world):
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, message=world.assistant,
                         reaction="dislike")
    api.put(path(world.chat, world.assistant), json={"reaction": "like"})
    assert len(seed.reactions(world.assistant)) == 2

    assert api.delete(path(world.chat, world.assistant)).status_code == 204

    (row,) = seed.reactions(world.assistant)
    assert bytes(row["user_id"]) == uuid_blob(USER_A_REVIEWER_ID)
    assert row["reaction"] == "dislike"


def test_put_replaces_only_the_callers_reaction(api, seed, world):
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, message=world.assistant,
                         reaction="dislike")
    api.put(path(world.chat, world.assistant), json={"reaction": "like"})

    by_user = {bytes(r["user_id"]): r["reaction"] for r in seed.reactions(world.assistant)}
    assert by_user == {uuid_blob(USER_A_ID): "like", uuid_blob(USER_A_REVIEWER_ID): "dislike"}
    assert my_reaction(api, world.chat, world.assistant) == "like"


# --------------------------------------------------------------------------- #
# path parameters and authentication
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("which", ["chat", "message"])
@pytest.mark.parametrize("method", ["PUT", "DELETE"])
def test_non_uuid_path_parameters_are_400_invalid_path_params(api, world, which, method):
    uri = (f"/v1/chats/not-a-uuid/messages/{world.assistant}/reaction" if which == "chat"
           else f"/v1/chats/{world.chat}/messages/not-a-uuid/reaction")
    kwargs = {"json": {"reaction": "like"}} if method == "PUT" else {}
    problem = assert_problem(api.request(method, uri, **kwargs), 400, "invalid_argument")
    assert any(v.get("reason") == "invalid_path_params"
               for v in problem["context"]["field_violations"]), problem


@pytest.mark.parametrize("method", ["PUT", "DELETE"])
def test_missing_bearer_token_is_401(api, world, method):
    kwargs = {"json": {"reaction": "like"}} if method == "PUT" else {}
    resp = api.request(method, path(world.chat, world.assistant), user=None, **kwargs)
    problem = assert_problem(resp, 401, "unauthenticated")
    assert problem["context"].get("reason") == "MISSING_BEARER"
