"""Turn retry / edit / delete."""

import threading
import uuid

from conftest import reason, violations, wait_for


def _send_ok(api, chat_id, text="hi", **kw):
    s = api.send(chat_id, text, **kw)
    assert s.terminal[0] == "done", s.text
    return s.first("stream_started")["request_id"]


def test_retry_latest_turn(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    _send_ok(api, chat["id"], "first")
    rid = _send_ok(api, chat["id"], "second")
    s = api.retry(chat["id"], rid)
    assert s.status == 200 and s.is_sse
    st = s.first("stream_started")
    new_rid = st["request_id"]
    assert new_rid != rid and st["is_new_turn"] is True
    assert uuid.UUID(new_rid).version == 4
    assert s.terminal[0] == "done"
    msgs = api.messages(chat["id"])
    assert [m["content"] for m in msgs if m["role"] == "user"] == ["first", "second"]
    assert msgs[-1]["request_id"] == new_rid and msgs[-2]["request_id"] == new_rid
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 4
    assert api.turn(chat["id"], rid).status_code == 404
    assert api.turn(chat["id"], new_rid).json()["state"] == "done"
    rows = server.query("SELECT replaced_by_request_id, deleted_at FROM chat_turns WHERE request_id = ?", uuid.UUID(rid).bytes)
    if not rows:
        rows = server.query("SELECT replaced_by_request_id, deleted_at FROM chat_turns WHERE request_id = ?", rid)
    assert rows and rows[0]["deleted_at"] is not None
    # old request id: replay rejected; mutation of the old one rejected
    s = api.send(chat["id"], "x", request_id=rid)
    assert s.status == 409 and s.json["context"]["reason"] == "request_id_conflict"
    r = api.post(f"/chats/{chat['id']}/turns/{rid}/retry")
    assert r.status_code == 409 and r.json()["context"]["reason"] == "NOT_LATEST_TURN"
    # the provider got the same user text again
    last = mock.chat_requests(chat["id"])[-1]["json"]
    assert last["input"][-1]["content"] in ("second", [{"type": "input_text", "text": "second"}])


def test_edit_latest_turn(api, mock):
    chat = api.create_chat()
    rid = _send_ok(api, chat["id"], "original")
    s = api.edit(chat["id"], rid, "MOCK_ECHO edited text")
    assert s.terminal[0] == "done", s.text
    new_rid = s.first("stream_started")["request_id"]
    assert new_rid != rid
    msgs = api.messages(chat["id"])
    assert [m["content"] for m in msgs] == ["MOCK_ECHO edited text", "MOCK_ECHO edited text"]
    # edit validation
    for bad in ("", "  "):
        s = api.edit(chat["id"], new_rid, bad)
        assert s.status == 400 and reason(s.json) == "EMPTY_CONTENT"
    s = api.stream(f"/chats/{chat['id']}/turns/{new_rid}", {}, method="PATCH")
    assert s.status == 422
    # the old turn is no longer latest
    s = api.edit(chat["id"], rid, "again")
    assert s.status == 409 and s.json["context"]["reason"] == "NOT_LATEST_TURN"


def test_delete_turns(api):
    chat = api.create_chat()
    r1 = _send_ok(api, chat["id"], "one")
    r2 = _send_ok(api, chat["id"], "two")
    # not the latest
    r = api.delete(f"/chats/{chat['id']}/turns/{r1}")
    assert r.status_code == 409 and r.json()["context"]["reason"] == "NOT_LATEST_TURN"
    assert api.delete(f"/chats/{chat['id']}/turns/{r2}").status_code == 204
    assert [m["content"] for m in api.messages(chat["id"]) if m["role"] == "user"] == ["one"]
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2
    # already deleted
    r = api.delete(f"/chats/{chat['id']}/turns/{r2}")
    assert r.status_code == 409 and r.json()["context"]["reason"] == "NOT_LATEST_TURN"
    assert api.turn(chat["id"], r2).status_code == 404
    # previous turn is now the latest
    assert api.delete(f"/chats/{chat['id']}/turns/{r1}").status_code == 204
    assert api.messages(chat["id"]) == []
    # unknown turn
    r = api.delete(f"/chats/{chat['id']}/turns/{uuid.uuid4()}")
    assert r.status_code in (404, 409)
    r = api.delete(f"/chats/{chat['id']}/turns/not-a-uuid")
    assert r.status_code == 400


def test_retry_failed_and_cancelled_turns(api):
    chat = api.create_chat()
    s = api.send(chat["id"], "MOCK_FAILED")
    rid = s.first("stream_started")["request_id"]
    s = api.retry(chat["id"], rid)
    # the retried text still makes the mock fail: still a new turn
    assert s.status == 200 and s.terminal[1]["code"] == "provider_error"
    new_rid = s.first("stream_started")["request_id"]
    s = api.edit(chat["id"], new_rid, "fixed now")
    assert s.terminal[0] == "done"
    assert [m["content"] for m in api.messages(chat["id"])] == ["fixed now", "Hello from the mock provider."]


def test_mutation_scoped_to_owner(server):
    a = server.client("user-a")
    b = server.client("user-b")
    chat = a.create_chat()
    rid = _send_ok(a, chat["id"])
    assert b.post(f"/chats/{chat['id']}/turns/{rid}/retry").status_code == 404
    assert b.patch(f"/chats/{chat['id']}/turns/{rid}", json={"content": "x"}).status_code == 404
    assert b.delete(f"/chats/{chat['id']}/turns/{rid}").status_code == 404
    assert b.turn(chat["id"], rid).status_code == 404
    assert a.turn(chat["id"], rid).json()["state"] == "done"


def test_concurrent_mutations_resolve_deterministically(server):
    api = server.client("user-a")
    chat = api.create_chat()
    rid = _send_ok(api, chat["id"])
    results = []

    def run(kind):
        c = server.client("user-a")
        if kind == "retry":
            results.append(c.stream(f"/chats/{chat['id']}/turns/{rid}/retry"))
        else:
            results.append(c.edit(chat["id"], rid, "MOCK_SLOW edit"))

    threads = [threading.Thread(target=run, args=(k,)) for k in ("retry", "edit", "retry", "edit")]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=90)
    ok = [r for r in results if r.status == 200]
    rejected = [r for r in results if r.status != 200]
    assert len(ok) == 1, [(r.status, r.text[:200]) for r in results]
    for r in rejected:
        assert r.status in (409, 400), r.text
        if r.status == 409:
            assert r.json["context"]["reason"] in ("NOT_LATEST_TURN", "GENERATION_IN_PROGRESS")
        else:
            assert violations(r.json)[0]["subject"] == "turn_state"
    # exactly one live turn remains and it is the winner's
    msgs = api.messages(chat["id"])
    assert len(msgs) == 2
    assert msgs[0]["request_id"] == ok[0].first("stream_started")["request_id"]


def test_retry_reuses_web_search_flag(api, mock):
    chat = api.create_chat()
    rid = _send_ok(api, chat["id"], "search please", web_search={"enabled": True})
    first = mock.chat_requests(chat["id"])[-1]["json"]
    assert any(t["type"] == "web_search" for t in first.get("tools", []))
    s = api.retry(chat["id"], rid)
    assert s.terminal[0] == "done"
    again = mock.chat_requests(chat["id"])[-1]["json"]
    assert any(t["type"] == "web_search" for t in again.get("tools", []))
    rid2 = s.first("stream_started")["request_id"]
    s = api.edit(chat["id"], rid2, "edited search")
    assert s.terminal[0] == "done"
    edited = mock.chat_requests(chat["id"])[-1]["json"]
    assert any(t["type"] == "web_search" for t in edited.get("tools", []))
    # without web search the tool is absent
    _send_ok(api, chat["id"], "plain")
    plain = mock.chat_requests(chat["id"])[-1]["json"]
    assert not any(t["type"] == "web_search" for t in plain.get("tools", []))


def test_mutation_after_chat_deleted(api):
    chat = api.create_chat()
    rid = _send_ok(api, chat["id"])
    api.delete(f"/chats/{chat['id']}")
    assert api.post(f"/chats/{chat['id']}/turns/{rid}/retry").status_code == 404
