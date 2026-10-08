"""Owner and tenant scoping of every operation.

Acceptance criteria covered:
* Authorization — "Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently"
* Principles — "Tenant and owner isolation enforced on every resource"
"""

from __future__ import annotations

import pytest

from helpers import (
    RT_ATTACHMENT,
    RT_CHAT,
    RT_TURN,
    assert_not_found,
    assert_not_found_any,
    attachment_row,
    get_chat,
    list_messages,
    make_pdf,
    new_chat,
    send_ok,
    turn_row,
    upload,
    upload_ok,
)


@pytest.fixture
def owned(fresh):
    """A chat of user a1 with one completed turn and one attachment."""
    cid = new_chat(fresh, "gpt-4.1-mini", title="owner chat")
    att = upload_ok(fresh, cid, "secret.pdf")
    st, _, _ = send_ok(fresh, cid, "private question")
    fresh.mock_reset()
    return fresh, cid, st, att


@pytest.mark.parametrize("intruder", ["a2", "b"])
def test_foreign_access_is_not_found_everywhere(owned, intruder):
    srv, cid, st, att = owned
    rid, msg = st["request_id"], st["message_id"]
    calls = [
        ("GET", f"/chats/{cid}", None),
        ("PATCH", f"/chats/{cid}", {"title": "hijacked"}),
        ("GET", f"/chats/{cid}/messages", None),
        ("GET", f"/chats/{cid}/turns/{rid}", None),
        ("POST", f"/chats/{cid}/turns/{rid}/retry", None),
        ("PATCH", f"/chats/{cid}/turns/{rid}", {"content": "edited"}),
        ("DELETE", f"/chats/{cid}/turns/{rid}", None),
        ("GET", f"/chats/{cid}/attachments/{att['id']}", None),
        ("DELETE", f"/chats/{cid}/attachments/{att['id']}", None),
        ("PUT", f"/chats/{cid}/messages/{msg}/reaction", {"reaction": "like"}),
        ("DELETE", f"/chats/{cid}/messages/{msg}/reaction", None),
        ("POST", f"/chats/{cid}/messages:stream", {"content": "intrude"}),
        ("DELETE", f"/chats/{cid}", None),
    ]
    for method, path, body in calls:
        kw = {"json": body} if body is not None else {}
        r = srv.req(method, path, intruder, **kw)
        assert "text/event-stream" not in r.headers.get("content-type", ""), (method, path)
        if "/turns/" in path:
            assert_not_found_any(r, RT_CHAT, RT_TURN)
        elif "/attachments/" in path:
            assert_not_found_any(r, RT_CHAT, RT_ATTACHMENT)
        else:
            assert_not_found(r, RT_CHAT)
    r = upload(srv, cid, "evil.pdf", make_pdf(), "application/pdf", user=intruder)
    assert_not_found(r, RT_CHAT)
    # Nothing changed for the owner and nothing reached the provider.
    assert srv.responses_requests() == [] and srv.mock_requests("/files", "POST") == []
    c = get_chat(srv, cid)
    assert c["title"] == "owner chat" and c["message_count"] == 2
    assert turn_row(srv, cid, rid)["deleted_at"] is None
    assert attachment_row(srv, att["id"])["deleted_at"] is None
    assert list_messages(srv, cid)[1]["my_reaction"] is None


def test_list_isolation(fresh):
    a1 = new_chat(fresh, "gpt-4.1-mini", user="a1")
    a2 = new_chat(fresh, "gpt-4.1-mini", user="a2")
    b = new_chat(fresh, "gpt-4.1-mini", user="b")

    def all_ids(user):
        ids, cursor = [], None
        while True:
            params = {"limit": 100}
            if cursor:
                params["cursor"] = cursor
            j = fresh.req("GET", "/chats", user, params=params).json()
            ids += [c["id"] for c in j["items"]]
            cursor = j["page_info"].get("next_cursor")
            if not cursor:
                return set(ids)

    ids_a1, ids_a2, ids_b = all_ids("a1"), all_ids("a2"), all_ids("b")
    assert a1 in ids_a1 and a2 not in ids_a1 and b not in ids_a1
    assert a2 in ids_a2 and a1 not in ids_a2 and b not in ids_a2
    assert b in ids_b and a1 not in ids_b and a2 not in ids_b
    # $filter cannot reach foreign chats either.
    r = fresh.req("GET", "/chats", "b", params={"$filter": f"id eq '{a1}'"})
    assert r.status_code == 200 and r.json()["items"] == []


def test_foreign_attachment_ids_cannot_be_used(fresh):
    cid_b = new_chat(fresh, "gpt-4.1-mini", user="b")
    att_b = upload_ok(fresh, cid_b, "b.pdf", user="b")
    cid_a = new_chat(fresh, "gpt-4.1-mini", user="a1")
    fresh.mock_reset()
    r, _ = fresh.stream(cid_a, "use foreign", attachment_ids=[att_b["id"]])
    assert r.status_code == 400
    assert fresh.chat_requests() == []
    # Foreign attachment is invisible through the owner's chat path as well.
    assert_not_found(fresh.req("GET", f"/chats/{cid_a}/attachments/{att_b['id']}"), RT_ATTACHMENT)


def test_quota_status_is_per_user(qs):
    from helpers import ensure_quota_rows

    ensure_quota_rows(qs, "a1")

    def used(user):
        j = qs.req("GET", "/quota/status", user).json()
        return {(t["tier"], p["period"]): p["used_credits_micro"] for t in j["tiers"] for p in t["periods"]}

    before_a2, before_b = used("a2"), used("b")
    cid = new_chat(qs, "gpt-4.1", user="a1")
    send_ok(qs, cid, "spend as a1")
    assert used("a2") == before_a2
    assert used("b") == before_b


def test_reaction_rows_scoped_to_user(owned):
    srv, cid, st, att = owned
    r = srv.req("PUT", f"/chats/{cid}/messages/{st['message_id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 200
    assert_not_found(srv.req("GET", f"/chats/{cid}/messages", "b"), RT_CHAT)
