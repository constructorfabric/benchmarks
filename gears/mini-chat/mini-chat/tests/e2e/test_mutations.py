"""Retry / edit / delete of the latest turn."""

from __future__ import annotations

import threading
import time
import uuid

from conftest import assert_problem, ok_stream, ub, wait_until
from test_idempotency import Background, _wait_running

TURN_RT = "gts.cf.core.mini_chat.turn.v1~"


def _turn_rows(server, chat_id):
    return server.query(
        "SELECT request_id, state, deleted_at, replaced_by_request_id, web_search_enabled FROM chat_turns "
        "WHERE chat_id = ? ORDER BY started_at",
        ub(chat_id),
    )


def test_retry_latest_turn(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"text": "first try"}, {"text": "second try"})
    ok_stream(api.send(chat["id"], "the question", request_id=rid))
    before = api.get(f"/chats/{chat['id']}").json()
    time.sleep(0.01)
    r = ok_stream(api.retry(chat["id"], rid))
    started = r.first("stream_started")
    new_rid = started["request_id"]
    assert new_rid != rid
    assert started["is_new_turn"] is True
    assert r.text == "second try"

    msgs = api.messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "the question"), ("assistant", "second try")]
    assert all(m["request_id"] == new_rid for m in msgs)
    assert_problem(api.turn(chat["id"], rid), 404, resource_type=TURN_RT)
    assert api.turn(chat["id"], new_rid).json()["state"] == "done"

    rows = _turn_rows(server, chat["id"])
    old = next(x for x in rows if x["request_id"] == ub(rid))
    assert old["deleted_at"] is not None
    assert old["replaced_by_request_id"] == ub(new_rid)
    old_msgs = server.query("SELECT deleted_at FROM messages WHERE request_id = ?", ub(rid))
    assert old_msgs and all(m["deleted_at"] is not None for m in old_msgs)

    body = mock.chat_requests(chat["id"])[-1]["json"]
    assert body["input"] == [{"role": "user", "content": [{"type": "input_text", "text": "the question"}]}]
    after = api.get(f"/chats/{chat['id']}").json()
    assert after["updated_at"] > before["updated_at"]
    assert after["message_count"] == 2
    ev = wait_until(lambda: server.audit_events(event_type="turn_retry", chat_id=chat["id"]), msg="retry audit")
    assert ev[0]["original_request_id"] == rid
    assert ev[0]["new_request_id"] == new_rid
    assert ev[0]["actor_user_id"] == api.user_id


def test_edit_latest_turn(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "orig", request_id=rid))
    mock.script({"text": "edited answer"})
    r = ok_stream(api.edit(chat["id"], rid, "changed question"))
    new_rid = r.first("stream_started")["request_id"]
    assert new_rid != rid
    msgs = api.messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "changed question"), ("assistant", "edited answer")]
    body = mock.chat_requests(chat["id"])[-1]["json"]
    assert body["input"][-1]["content"][0]["text"] == "changed question"
    wait_until(lambda: server.audit_events(event_type="turn_edit", chat_id=chat["id"]), msg="edit audit")


def test_edit_validation(api):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "orig", request_id=rid))
    assert_problem(api.edit(chat["id"], rid, ""), 400, reason="EMPTY_CONTENT")
    assert api.turn(chat["id"], rid).json()["state"] == "done"


def test_delete_latest_turn(api, server):
    chat = api.create_chat()
    rid1 = str(uuid.uuid4())
    rid2 = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "one", request_id=rid1))
    ok_stream(api.send(chat["id"], "two", request_id=rid2))
    r = api.delete(f"/chats/{chat['id']}/turns/{rid2}")
    assert r.status_code == 204
    assert_problem(api.turn(chat["id"], rid2), 404)
    msgs = api.messages(chat["id"])
    assert [m["request_id"] for m in msgs] == [rid1, rid1]
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2
    # the previous turn becomes the latest one again
    assert api.delete(f"/chats/{chat['id']}/turns/{rid1}").status_code == 204
    assert api.messages(chat["id"]) == []
    wait_until(lambda: server.audit_events(event_type="turn_delete", request_id=rid2), msg="delete audit")
    # deleted turn cannot be mutated again
    assert api.delete(f"/chats/{chat['id']}/turns/{rid2}").status_code in (404, 409)


def test_only_latest_turn_can_be_mutated(api):
    chat = api.create_chat()
    rid1 = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "one", request_id=rid1))
    ok_stream(api.send(chat["id"], "two"))
    assert_problem(api.retry(chat["id"], rid1), 409, category="aborted", reason="NOT_LATEST_TURN")
    assert_problem(api.edit(chat["id"], rid1, "x"), 409, reason="NOT_LATEST_TURN")
    assert_problem(api.delete(f"/chats/{chat['id']}/turns/{rid1}"), 409, reason="NOT_LATEST_TURN")
    assert len(api.messages(chat["id"])) == 4


def test_running_turn_cannot_be_mutated(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 4})
    bg = Background(api, chat["id"], request_id=rid)
    _wait_running(server, chat["id"])
    body = assert_problem(api.retry(chat["id"], rid), 400, category="failed_precondition")
    assert body["context"]["violations"][0]["subject"] == "turn_state"
    assert body["context"]["violations"][0]["type"] == "STATE"
    assert_problem(api.edit(chat["id"], rid, "x"), 400, category="failed_precondition")
    assert_problem(api.delete(f"/chats/{chat['id']}/turns/{rid}"), 400, category="failed_precondition")
    bg.join()


def test_mutation_unknown_turn_and_foreign_chat(api, other_user):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "x", request_id=rid))
    assert_problem(api.retry(chat["id"], str(uuid.uuid4())), 404, resource_type=TURN_RT)
    assert_problem(other_user.retry(chat["id"], rid), 404)
    assert_problem(other_user.delete(f"/chats/{chat['id']}/turns/{rid}"), 404)
    assert_problem(other_user.turn(chat["id"], rid), 404)


def test_retry_failed_turn_allowed(api, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"http_status": 500}, {"text": "recovered"})
    r = api.send(chat["id"], "x", request_id=rid)
    assert r.names[-1] == "error"
    r = ok_stream(api.retry(chat["id"], rid))
    assert r.text == "recovered"


def test_mutation_goes_through_preflight(api, mock):
    chat = api.create_chat(model="gpt-tiny")
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "short", request_id=rid))
    calls = len(mock.chat_requests(chat["id"]))
    r = api.edit(chat["id"], rid, "x" * 20_000)
    assert_problem(r, 400, reason="INPUT_TOO_LONG")
    # rejected mutation changes nothing
    assert api.turn(chat["id"], rid).json()["state"] == "done"
    assert [m["content"] for m in api.messages(chat["id"]) if m["role"] == "user"] == ["short"]
    assert len(mock.chat_requests(chat["id"])) == calls


def test_concurrent_mutations_resolve_deterministically(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "x", request_id=rid))
    mock.script(*[{"chunks": ["slow"], "chunk_delay_ms": 1500} for _ in range(3)])
    barrier = threading.Barrier(3)
    results = []

    def go():
        barrier.wait()
        results.append(api.retry(chat["id"], rid))

    threads = [threading.Thread(target=go) for _ in range(3)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    ok = [r for r in results if r.status == 200]
    assert len(ok) == 1, [(r.status, r.body) for r in results]
    for r in results:
        if r.status != 200:
            body = assert_problem(r, 409)
            assert body["context"]["reason"] in ("GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"), body
    live = server.query("SELECT request_id FROM chat_turns WHERE chat_id = ? AND deleted_at IS NULL", ub(chat["id"]))
    assert len(live) == 1


def test_retry_carries_attachments_and_tools(api, server, mock):
    chat = api.create_chat()
    up = api.upload(chat["id"], b"facts about rust", "facts.txt", "text/plain")
    assert up.status_code == 201, up.text
    att = up.json()["id"]
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "use the file", request_id=rid, attachment_ids=[att], web_search={"enabled": True}))
    r = ok_stream(api.retry(chat["id"], rid))
    new_rid = r.first("stream_started")["request_id"]
    msgs = api.messages(chat["id"])
    user = next(m for m in msgs if m["role"] == "user")
    assert [a["attachment_id"] for a in user["attachments"]] == [att]
    body = mock.chat_requests(chat["id"])[-1]["json"]
    types = sorted(t["type"] for t in body["tools"])
    assert types == ["file_search", "web_search"]
    row = server.query("SELECT web_search_enabled FROM chat_turns WHERE request_id = ?", ub(new_rid))[0]
    assert row["web_search_enabled"] == 1
    # edit keeps the attachment links too
    r = ok_stream(api.edit(chat["id"], new_rid, "use the file again"))
    user = next(m for m in api.messages(chat["id"]) if m["role"] == "user")
    assert user["content"] == "use the file again"
    assert [a["attachment_id"] for a in user["attachments"]] == [att]


def test_retry_resends_original_images(api, server, mock):
    from test_attachments import att_row, png

    chat = api.create_chat()
    img = api.upload(chat["id"], png(16, 16), "i.png", "image/png").json()
    file_id = att_row(server, img["id"])["provider_file_id"]
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "what is this", request_id=rid, attachment_ids=[img["id"]]))
    ok_stream(api.retry(chat["id"], rid))
    content = mock.chat_requests(chat["id"])[-1]["json"]["input"][-1]["content"]
    assert {"type": "input_image", "file_id": file_id} in content
    # deleted attachments are silently excluded from the copy
    new_rid = api.messages(chat["id"])[0]["request_id"]
    server.execute("UPDATE attachments SET deleted_at = '2026-01-01T00:00:00.000000001Z' WHERE id = ?", ub(img["id"]))
    ok_stream(api.retry(chat["id"], new_rid))
    user = next(m for m in api.messages(chat["id"]) if m["role"] == "user")
    assert user["attachments"] == []
    content = mock.chat_requests(chat["id"])[-1]["json"]["input"][-1]["content"]
    assert all(c["type"] != "input_image" for c in content)
