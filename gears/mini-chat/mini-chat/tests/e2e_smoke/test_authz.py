"""Tenant / owner isolation: another user of the same tenant and a user of another tenant get 404
on every chat-scoped route (DESIGN §3.8, ADR-0004 not_found masking)."""

import pytest

from conftest import CHAT_RT, make_png, sse_request


@pytest.fixture(scope="module")
def owned(api):
    chat = api.create_chat(title="owned by A")
    turn = api.turn(chat["id"], "secret")
    att = api.upload(chat["id"], "doc.txt", b"private", "text/plain").json()
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    return {"chat": chat, "rid": turn.started["request_id"], "att": att, "asst": msgs[1]["id"]}


def routes(o):
    c = o["chat"]["id"]
    return [
        ("GET", f"/chats/{c}", None),
        ("PATCH", f"/chats/{c}", {"title": "hijack"}),
        ("GET", f"/chats/{c}/messages", None),
        ("GET", f"/chats/{c}/attachments/{o['att']['id']}", None),
        ("DELETE", f"/chats/{c}/attachments/{o['att']['id']}", None),
        ("GET", f"/chats/{c}/turns/{o['rid']}", None),
        ("DELETE", f"/chats/{c}/turns/{o['rid']}", None),
        ("PUT", f"/chats/{c}/messages/{o['asst']}/reaction", {"reaction": "like"}),
        ("DELETE", f"/chats/{c}/messages/{o['asst']}/reaction", None),
        ("DELETE", f"/chats/{c}", None),
    ]


@pytest.mark.parametrize("who", ["reviewer", "tenant_b"])
def test_foreign_rest_routes_are_404(request, who, owned, api):
    other = request.getfixturevalue(who)
    for method, path, body in routes(owned):
        r = other.request(method, path, json=body) if body is not None else other.request(method, path)
        assert r.status_code == 404, (method, path, r.status_code, r.text)
        assert r.json()["type"].endswith("cf.core.err.not_found.v1~"), r.text
    # Upload into the foreign chat.
    r = other.upload(owned["chat"]["id"], "x.txt", b"x", "text/plain")
    assert r.status_code == 404 and r.json()["context"]["resource_type"] == CHAT_RT, r.text
    # Nothing changed for the owner.
    c = owned["chat"]["id"]
    got = api.get(f"/chats/{c}").json()
    assert got["title"] == "owned by A" and got["message_count"] == 2
    assert api.get(f"/chats/{c}/attachments/{owned['att']['id']}").json()["status"] == "ready"
    assert api.get(f"/chats/{c}/turns/{owned['rid']}").json()["state"] == "done"


@pytest.mark.parametrize("who", ["reviewer", "tenant_b"])
def test_foreign_stream_routes_are_404(request, who, owned, mock):
    other = request.getfixturevalue(who)
    c = owned["chat"]["id"]
    before = len(mock.responses_calls())
    for method, path, body in (
        ("POST", f"/chats/{c}/messages:stream", {"content": "hi"}),
        ("POST", f"/chats/{c}/messages:stream", {"content": "hi", "request_id": owned["rid"]}),  # replay attempt
        ("POST", f"/chats/{c}/turns/{owned['rid']}/retry", None),
        ("PATCH", f"/chats/{c}/turns/{owned['rid']}", {"content": "edit"}),
    ):
        res = sse_request(other, method, path, body)
        assert res.status == 404, (method, path, res.status, res.problem, res.names)
        assert res.problem["context"]["resource_type"] == CHAT_RT, res.problem
    assert len(mock.responses_calls()) == before


def test_foreign_attachment_ids_rejected(api, reviewer):
    mine = api.create_chat()
    att = api.upload(mine["id"], "p.png", make_png(8, 8), "image/png").json()
    theirs = reviewer.create_chat()
    res = reviewer.stream(theirs["id"], "steal", attachment_ids=[att["id"]])
    assert res.status == 400, res.problem
    assert any(v.get("reason") == "invalid_attachment" for v in res.problem["context"]["field_violations"])
