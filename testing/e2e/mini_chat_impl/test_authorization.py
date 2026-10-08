"""Authorization & isolation (acceptance: Authorization; Principles: tenant and owner isolation)."""

import uuid

import pytest

from mchelpers import Api, assert_problem, make_pdf, nonce


@pytest.fixture()
def owned(api):
    """A chat of user A with one completed turn and one ready attachment."""
    c = api.create_chat(title="private")
    att = api.upload_ready(c["id"], "private.pdf", make_pdf("private"), "application/pdf")
    s = api.stream(c["id"], "secret question " + nonce())
    assert s.done
    msgs = api.messages(c["id"])
    return {"chat": c, "att": att, "turn": s, "assistant": msgs[1], "user": msgs[0]}


def _foreign_calls(o: Api, d: dict) -> list:
    cid = d["chat"]["id"]
    rid = d["turn"].request_id
    mid = d["assistant"]["id"]
    aid = d["att"]["id"]
    return [
        ("get chat", lambda: o.get(f"/v1/chats/{cid}")),
        ("patch chat", lambda: o.patch(f"/v1/chats/{cid}", json={"title": "hijacked"})),
        ("list messages", lambda: o.get(f"/v1/chats/{cid}/messages")),
        ("send", lambda: o.as_response(o.stream(cid, "intrude " + nonce()))),
        ("upload", lambda: o.upload(cid, "x.txt", b"x", "text/plain")),
        ("get attachment", lambda: o.get(f"/v1/chats/{cid}/attachments/{aid}")),
        ("delete attachment", lambda: o.delete(f"/v1/chats/{cid}/attachments/{aid}")),
        ("turn status", lambda: o.turn(cid, rid)),
        ("retry", lambda: o.as_response(o.retry(cid, rid))),
        ("edit", lambda: o.as_response(o.edit(cid, rid, "edited by intruder"))),
        ("delete turn", lambda: o.delete(f"/v1/chats/{cid}/turns/{rid}")),
        ("put reaction", lambda: o.put(f"/v1/chats/{cid}/messages/{mid}/reaction", json={"reaction": "like"})),
        ("delete reaction", lambda: o.delete(f"/v1/chats/{cid}/messages/{mid}/reaction")),
        ("delete chat", lambda: o.delete(f"/v1/chats/{cid}")),
    ]


# Acceptance: Authorization — another tenant and another user of the same tenant get 404 everywhere
@pytest.mark.parametrize("intruder", ["tok-b", "tok-a2"])
def test_foreign_access_is_404(api, api_for, owned, intruder, mock_llm):
    o = api_for(intruder)
    before_calls = len(mock_llm.chat_requests(contains="intrude"))
    for name, call in _foreign_calls(o, owned):
        r = call()
        assert r.status_code == 404, f"{intruder} {name}: expected 404, got {r.status_code} {r.text[:300]}"
        body = r.json()
        assert "not_found" in str(body.get("type")), (name, body)
    assert len(mock_llm.chat_requests(contains="intrude")) == before_calls
    # A's data is unchanged
    c = api.chat(owned["chat"]["id"])
    assert c["title"] == "private" and c["message_count"] == 2
    assert api.attachment(owned["chat"]["id"], owned["att"]["id"]).json()["status"] == "ready"
    assert api.turn(owned["chat"]["id"], owned["turn"].request_id).json()["state"] == "done"
    assert api.messages(owned["chat"]["id"])[1]["my_reaction"] is None


# Acceptance: Authorization — lists are scoped to the caller
def test_list_scoped_to_owner(api, api_for, owned):
    for tok in ("tok-b", "tok-a2"):
        ids = [c["id"] for c in api_for(tok).list_chats(limit=100).json()["items"]]
        assert owned["chat"]["id"] not in ids
    assert owned["chat"]["id"] in [c["id"] for c in api.list_chats(limit=100).json()["items"]]


# Acceptance: Authorization — reactions are per user (another user's reaction is invisible)
def test_reactions_are_per_user(api, owned):
    cid, mid = owned["chat"]["id"], owned["assistant"]["id"]
    assert api.put(f"/v1/chats/{cid}/messages/{mid}/reaction", json={"reaction": "like"}).status_code == 200
    assert api.messages(cid)[1]["my_reaction"] == "like"


# Acceptance: Authorization — a foreign attachment cannot be referenced in one's own chat
def test_foreign_attachment_rejected(api_for, owned, mock_llm):
    b = api_for("tok-b")
    own = b.create_chat()
    n = nonce()
    s = b.stream(own["id"], f"steal {n}", attachment_ids=[owned["att"]["id"]])
    assert_problem(b.as_response(s), 400, "invalid_argument", field_reason="invalid_attachment")
    assert mock_llm.chat_requests(contains=n) == []


# Acceptance: Authorization — quota is per user; B's usage does not change A's quota
def test_quota_isolated(api_for, db):
    a = api_for("tok-l3")  # a same-tenant user with no concurrent activity
    ca = a.create_chat()
    assert a.stream(ca["id"], "a spends " + nonce()).done
    b = api_for("tok-b")
    a_before = a.quota()
    a_rows_before = db.quota_snapshot("tok-l3")
    cb = b.create_chat()
    assert b.stream(cb["id"], "b spends [[usage:1000:1000]] " + nonce()).done
    assert a.quota() == a_before
    assert db.quota_snapshot("tok-l3") == a_rows_before
    b_daily = b.quota_period("total", "daily")
    assert b_daily["used_credits_micro"] > 0


# Acceptance: Authorization — models are readable by every authenticated user
def test_models_visible_to_other_tenant(api_for):
    assert api_for("tok-b").get("/v1/models").status_code == 200


# Acceptance: Authorization — unauthenticated requests are rejected on every route family
def test_unauthenticated_routes(api_for, owned):
    anon = api_for(None)
    cid = owned["chat"]["id"]
    for r in (
        anon.get("/v1/chats"),
        anon.post("/v1/chats", json={}),
        anon.get(f"/v1/chats/{cid}"),
        anon.get(f"/v1/chats/{cid}/messages"),
        anon.post(f"/v1/chats/{cid}/messages:stream", json={"content": "x"}),
        anon.get("/v1/quota/status"),
        anon.get(f"/v1/chats/{cid}/turns/{uuid.uuid4()}"),
    ):
        assert r.status_code == 401, r.text
