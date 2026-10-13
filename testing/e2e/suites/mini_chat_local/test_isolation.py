"""Tenant and owner isolation of every chat endpoint, black-box.

Source: DESIGN 2.1 "Tenant-Scoped Everything" / "Owner-Only Chat Content", 3.8 (404 masking
of foreign resources, fail-closed, defence-in-depth owner predicate), the REST error table
and the acceptance criterion "Every operation is scoped to its owner; cross-tenant or
foreign access is rejected consistently".

Users (generated server config): ``a`` and ``a_reviewer`` belong to tenant A, ``b`` to tenant B.
"""

from __future__ import annotations

import uuid

import pytest

from .support import (
    RESOURCE_TYPES,
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    assert_problem,
    list_all,
    qs,
    unique_tag,
    uuid_blob,
)

FOREIGN_USERS = ["a_reviewer", "b"]  # another user of the same tenant / a user of another tenant


@pytest.fixture()
def owned(seed):
    """A chat of user ``a`` with a user and an assistant message and a reaction of ``a``."""
    tag = unique_tag()
    chat = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} Private", created=100)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", content="secret question",
                        created=1)
    assistant = seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant",
                                    content="secret answer", created=2)
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_ID, message=assistant, reaction="like")
    return {"tag": tag, "chat": chat, "assistant": assistant}


def requests_for(owned):
    chat, msg = owned["chat"], owned["assistant"]
    return [
        ("GET", f"/v1/chats/{chat}", None),
        ("PATCH", f"/v1/chats/{chat}", {"title": "hijacked"}),
        ("GET", f"/v1/chats/{chat}/messages", None),
        ("PUT", f"/v1/chats/{chat}/messages/{msg}/reaction", {"reaction": "dislike"}),
        ("DELETE", f"/v1/chats/{chat}/messages/{msg}/reaction", None),
        ("DELETE", f"/v1/chats/{chat}", None),  # last: it would delete the chat for the owner
    ]


def send(api, method, path, body, user):
    kwargs = {"json": body} if body is not None else {}
    return api.request(method, path, user=user, **kwargs)


@pytest.mark.parametrize("user", FOREIGN_USERS)
def test_a_foreign_user_gets_404_on_every_chat_endpoint_and_changes_nothing(api, seed, owned, user):
    for method, path, body in requests_for(owned):
        resp = send(api, method, path, body, user)
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"], (method, path)
        assert "secret" not in resp.text and "Private" not in resp.text

    row = seed.chat(owned["chat"])
    assert row["title"] == f"{owned['tag']} Private", "not renamed"
    assert row["deleted_at"] is None, "not deleted"
    (reaction,) = seed.reactions(owned["assistant"])
    assert (bytes(reaction["user_id"]), reaction["reaction"]) == (uuid_blob(USER_A_ID), "like")
    assert not [p for p in seed.outbox_payloads()
                if isinstance(p, dict) and p.get("chat_id") == owned["chat"]]
    # The owner still sees everything.
    assert api.get(f"/v1/chats/{owned['chat']}").status_code == 200
    assert len(api.get(f"/v1/chats/{owned['chat']}/messages").json()["items"]) == 2


@pytest.mark.parametrize("user", FOREIGN_USERS)
def test_foreign_and_unknown_chats_are_indistinguishable(api, owned, user):
    unknown = str(uuid.uuid4())
    msg = owned["assistant"]
    pairs = [
        ("GET", "/v1/chats/{}", None),
        ("PATCH", "/v1/chats/{}", {"title": "x"}),
        ("DELETE", "/v1/chats/{}", None),
        ("GET", "/v1/chats/{}/messages", None),
        ("PUT", "/v1/chats/{}/messages/" + msg + "/reaction", {"reaction": "like"}),
    ]
    for method, template, body in pairs:
        foreign = assert_problem(send(api, method, template.format(owned["chat"]), body, user),
                                 404, "not_found")
        missing = assert_problem(send(api, method, template.format(unknown), body, user),
                                 404, "not_found")
        for key in ("type", "title", "status", "detail"):
            assert foreign[key] == missing[key], (method, template, key)
        assert foreign["context"]["resource_type"] == missing["context"]["resource_type"]


def test_list_shows_only_the_callers_own_live_chats(api, seed):
    tag = unique_tag()
    a1 = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} a1", updated=10)
    a2 = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} a2", updated=20)
    seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, title=f"{tag} a-gone", updated=30,
                     deleted=True)
    r1 = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, title=f"{tag} r1",
                          updated=40)
    b1 = seed.insert_chat(tenant=TENANT_B_ID, user=USER_B_ID, title=f"{tag} b1", updated=50)
    flt = {"_filter": f"contains(title,'{tag}')"}

    assert [c["id"] for c in list_all(api, "/v1/chats", user="a", **flt)] == [a2, a1]
    assert [c["id"] for c in list_all(api, "/v1/chats", user="a_reviewer", **flt)] == [r1]
    assert [c["id"] for c in list_all(api, "/v1/chats", user="b", **flt)] == [b1]


def test_a_chat_created_by_another_tenant_is_invisible_and_owned_by_its_creator(api, seed):
    tag = unique_tag()
    created = api.post("/v1/chats", json={"title": f"{tag} B only"}, user="b").json()

    row = seed.chat(created["id"])
    assert bytes(row["tenant_id"]) == uuid_blob(TENANT_B_ID)
    assert bytes(row["user_id"]) == uuid_blob(USER_B_ID)
    for user in ("a", "a_reviewer"):
        assert api.get(f"/v1/chats/{created['id']}", user=user).status_code == 404
        assert list_all(api, "/v1/chats", user=user,
                        _filter=f"contains(title,'{tag}')") == []
    assert api.get(f"/v1/chats/{created['id']}", user="b").status_code == 200


def test_a_foreign_message_id_cannot_be_reached_through_the_callers_own_chat(api, seed):
    mine = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID)
    theirs = seed.insert_chat(tenant=TENANT_B_ID, user=USER_B_ID)
    their_msg = seed.insert_message(tenant=TENANT_B_ID, chat=theirs, role="assistant", created=1)

    put = api.put(f"/v1/chats/{mine}/messages/{their_msg}/reaction", json={"reaction": "like"})
    delete = api.delete(f"/v1/chats/{mine}/messages/{their_msg}/reaction")
    for resp in (put, delete):
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["message"]
    assert seed.reactions(their_msg) == []
    assert api.get(f"/v1/chats/{mine}/messages").json()["items"] == []


def test_a_foreign_users_reaction_row_never_surfaces_as_my_reaction(api, seed, owned):
    seed.insert_reaction(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, message=owned["assistant"],
                         reaction="dislike")
    items = api.get(f"/v1/chats/{owned['chat']}/messages").json()["items"]
    assert [m["my_reaction"] for m in items if m["role"] == "assistant"] == ["like"]


@pytest.mark.parametrize("user", FOREIGN_USERS)
def test_a_foreign_filter_cannot_widen_the_list(api, seed, owned, user):
    resp = api.get("/v1/chats" + qs(_filter=f"contains(title,'{owned['tag']}')"), user=user)
    assert resp.status_code == 200, resp.text
    assert resp.json()["items"] == []
