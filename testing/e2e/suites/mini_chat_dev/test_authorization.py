"""Tenant and owner isolation on every chat-scoped endpoint (DESIGN section 3.8, section 2.1
"Tenant-Scoped Everything" / "Owner-Only Chat Content").

The chat is the authorized resource; sub-resources (messages, turns, attachments, reactions) are
reached through it. A scoped query that finds no row is 404 - never 403 - so a foreign caller
(another tenant, or another user of the same tenant) cannot even learn that the chat exists, and
nothing is changed or sent to the provider on its behalf.
"""

import uuid
from contextlib import closing
from pathlib import Path

import pytest

from .helpers import (PREFIX, TOKEN_A, TOKEN_A_REVIEWER, TOKEN_B, api, assert_problem, create_chat, db,
                      reserved_credits, stream, uuid_bytes)

pytestmark = pytest.mark.usefixtures("server")

CHAT_TYPE = "gts.cf.core.mini_chat.chat.v1~"
PDF = Path(__file__).resolve().parents[2] / "testdata" / "pdf" / "test_file_one_page_en.pdf"
QUOTA_URL = f"{PREFIX}/quota/status"


def _owned_chat(s) -> dict:
    """A chat of tenant A's user with a completed turn, a reaction and a ready attachment."""
    chat = create_chat(s, title="owned")
    rid = str(uuid.uuid4())
    res = stream(s, chat["id"], {"content": "hi", "request_id": rid})
    assert res.terminal and res.terminal[0] == "done", res.raw
    assistant = res.of("stream_started")[0]["message_id"]
    url = f"{PREFIX}/chats/{chat['id']}"
    assert s.put(f"{url}/messages/{assistant}/reaction", json={"reaction": "like"}).status_code == 200
    up = s.post(f"{url}/attachments", files={"file": (PDF.name, PDF.read_bytes(), "application/pdf")})
    assert up.status_code == 201, up.text
    return {"chat": chat, "rid": rid, "assistant": assistant, "attachment": up.json()["id"]}


def _snapshot(chat_id: str) -> dict:
    """Everything a foreign call could have changed, read from the gear DB."""
    cid = uuid_bytes(chat_id)
    with closing(db()) as conn:
        q = conn.execute
        return {
            "chat": tuple(q("SELECT title, model, updated_at, deleted_at FROM chats WHERE id = ?", (cid,)).fetchone()),
            "messages": [tuple(r) for r in q(
                "SELECT id, role, content, deleted_at FROM messages WHERE chat_id = ? ORDER BY created_at, id",
                (cid,))],
            "turns": [tuple(r) for r in q(
                "SELECT request_id, state, deleted_at, replaced_by_request_id FROM chat_turns WHERE chat_id = ?"
                " ORDER BY started_at, id", (cid,))],
            "attachments": [tuple(r) for r in q(
                "SELECT id, status, deleted_at, cleanup_status FROM attachments WHERE chat_id = ? ORDER BY id",
                (cid,))],
            "reactions": [tuple(r) for r in q(
                "SELECT message_id, user_id, reaction FROM message_reactions WHERE message_id IN"
                " (SELECT id FROM messages WHERE chat_id = ?)", (cid,))],
        }


def _foreign_calls(other, owned: dict):
    """(label, response) for every chat-scoped operation, aimed at tenant A's real ids."""
    chat_id, rid = owned["chat"]["id"], owned["rid"]
    url = f"{PREFIX}/chats/{chat_id}"
    turn = f"{url}/turns/{rid}"
    att = f"{url}/attachments/{owned['attachment']}"
    reaction = f"{url}/messages/{owned['assistant']}/reaction"
    sse = {"Accept": "text/event-stream"}
    yield "get_chat", other.get(url)
    yield "update_chat", other.patch(url, json={"title": "hijack"})
    yield "list_messages", other.get(f"{url}/messages")
    yield "stream_message", other.post(f"{url}/messages:stream", json={"content": "hi"}, headers=sse)
    # A's own request id must not be replayed to a foreign caller either.
    yield "stream_message_replay", other.post(f"{url}/messages:stream", json={"content": "hi", "request_id": rid},
                                              headers=sse)
    yield "get_turn", other.get(turn)
    yield "retry_turn", other.post(f"{turn}/retry", headers=sse)
    yield "edit_turn", other.patch(turn, json={"content": "edited"}, headers=sse)
    yield "delete_turn", other.delete(turn)
    yield "upload_attachment", other.post(f"{url}/attachments",
                                          files={"file": ("x.pdf", PDF.read_bytes(), "application/pdf")})
    yield "get_attachment", other.get(att)
    yield "delete_attachment", other.delete(att)
    yield "put_reaction", other.put(reaction, json={"reaction": "dislike"})
    yield "delete_reaction", other.delete(reaction)
    yield "delete_chat", other.delete(url)


@pytest.mark.parametrize("token", [TOKEN_B, TOKEN_A_REVIEWER], ids=["other_tenant", "same_tenant_other_user"])
def test_foreign_caller_gets_404_on_every_chat_endpoint(reset_mock, token):
    owner = api(TOKEN_A)
    owned = _owned_chat(owner)
    before = _snapshot(owned["chat"]["id"])
    provider_calls = len(reset_mock.requests())

    labels = []
    for label, r in _foreign_calls(api(token), owned):
        labels.append(label)
        assert_problem(r, 404, "not_found", resource_type=CHAT_TYPE)
    assert len(labels) == 15

    assert len(reset_mock.requests()) == provider_calls, "a foreign call reached the provider"
    assert _snapshot(owned["chat"]["id"]) == before
    # The owner still sees everything unchanged.
    url = f"{PREFIX}/chats/{owned['chat']['id']}"
    assert owner.get(url).json()["title"] == "owned"
    items = owner.get(f"{url}/messages").json()["items"]
    assert [m["my_reaction"] for m in items] == [None, "like"]
    assert owner.get(f"{url}/turns/{owned['rid']}").json()["state"] == "done"
    assert owner.get(f"{url}/attachments/{owned['attachment']}").status_code == 200
    # And the chat is absent from the foreign caller's list.
    ids = {c["id"] for c in api(token).get(f"{PREFIX}/chats", params={"limit": 100}).json()["items"]}
    assert owned["chat"]["id"] not in ids


@pytest.mark.parametrize("token", [TOKEN_B, TOKEN_A_REVIEWER], ids=["other_tenant", "same_tenant_other_user"])
def test_foreign_attachment_id_cannot_be_referenced_from_own_chat(reset_mock, token):
    owned = _owned_chat(api(TOKEN_A))
    calls = len(reset_mock.requests(path="/responses"))
    other = api(token)
    own_chat = create_chat(other)
    reserved = reserved_credits(own_chat["id"])
    r = other.post(f"{PREFIX}/chats/{own_chat['id']}/messages:stream",
                   json={"content": "read it", "attachment_ids": [owned["attachment"]]})
    assert_problem(r, 400, "invalid_argument", reason="invalid_attachment", field="attachment")
    assert len(reset_mock.requests(path="/responses")) == calls
    with closing(db()) as conn:
        turns = conn.execute("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?",
                             (uuid_bytes(own_chat["id"]),)).fetchone()["n"]
    assert turns == 0, "a turn was started"
    assert reserved_credits(own_chat["id"]) == reserved, "a reserve is still booked"
    with closing(db()) as conn:
        linked = conn.execute("SELECT COUNT(*) AS n FROM message_attachments WHERE attachment_id = ?",
                              (uuid_bytes(owned["attachment"]),)).fetchone()["n"]
    assert linked == 0


def test_quota_status_of_other_tenant_is_unaffected_by_usage(reset_mock):
    b_before = api(TOKEN_B).get(QUOTA_URL)
    assert b_before.status_code == 200, b_before.text
    a = api(TOKEN_A)
    a_before = a.get(QUOTA_URL).json()
    chat = create_chat(a)
    assert stream(a, chat["id"], {"content": "spend some credits"}).terminal[0] == "done"
    a_after = a.get(QUOTA_URL).json()

    def used(body):
        return {(t["tier"], p["period"]): p["used_credits_micro"] for t in body["tiers"] for p in t["periods"]}

    assert used(a_after)[("total", "daily")] > used(a_before)[("total", "daily")]
    assert api(TOKEN_B).get(QUOTA_URL).json() == b_before.json()


CHAT_ENDPOINTS = [
    ("get", "/chats"),
    ("post", "/chats"),
    ("get", "/chats/{c}"),
    ("patch", "/chats/{c}"),
    ("delete", "/chats/{c}"),
    ("get", "/chats/{c}/messages"),
    ("post", "/chats/{c}/messages:stream"),
    ("get", "/chats/{c}/turns/{r}"),
    ("post", "/chats/{c}/turns/{r}/retry"),
    ("patch", "/chats/{c}/turns/{r}"),
    ("delete", "/chats/{c}/turns/{r}"),
    ("post", "/chats/{c}/attachments"),
    ("get", "/chats/{c}/attachments/{r}"),
    ("delete", "/chats/{c}/attachments/{r}"),
    ("put", "/chats/{c}/messages/{r}/reaction"),
    ("delete", "/chats/{c}/messages/{r}/reaction"),
    ("get", "/models"),
    ("get", "/models/gpt-premium"),
    ("get", "/quota/status"),
]


@pytest.mark.parametrize("method,path", CHAT_ENDPOINTS, ids=[f"{m} {p}" for m, p in CHAT_ENDPOINTS])
def test_unauthenticated_is_401_on_every_endpoint(reset_mock, method, path):
    url = PREFIX + path.format(c=uuid.uuid4(), r=uuid.uuid4())
    if path.endswith("/attachments"):  # the gateway checks the declared content type first
        body = {"files": {"file": ("x.pdf", b"%PDF-1.4", "application/pdf")}}
    else:
        body = {"json": {"content": "hi", "title": "t", "reaction": "like"}}
    r = api(None).request(method.upper(), url, **body)
    assert r.status_code == 401, f"{method} {url}: {r.status_code} {r.text}"
    assert reset_mock.requests() == []
