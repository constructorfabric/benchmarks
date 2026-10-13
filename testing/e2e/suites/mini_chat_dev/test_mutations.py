"""Turn mutations: retry / edit / delete of the latest terminal turn."""

from __future__ import annotations

import threading
import uuid

from .conftest import RT_TURN, assert_problem, field_reasons, outbox_payloads, ub
from .mock_llm import held_stream, json_response, text_stream


def test_retry_creates_new_turn_and_soft_deletes_old(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    first = a.send(chat["id"], "original question")
    old_rid = first.request_id

    mock_llm.script_chat(chat["id"], text_stream(chunks=["second answer"]))
    res = a.retry(chat["id"], old_rid)
    assert res.status == 200, res.body
    assert res.names[0] == "stream_started" and res.names[-1] == "done"
    new_rid = res.request_id
    assert new_rid != old_rid and uuid.UUID(new_rid).version == 4
    assert res.started["is_new_turn"] is True
    assert res.text == "second answer"

    reqs = mock_llm.chat_requests(chat["id"])
    assert len(reqs) == 2
    assert reqs[1].json["input"][-1] == {"role": "user", "content": "original question"}
    assert len(reqs[1].json["input"]) == 1  # the replaced turn is not in the history

    msgs = a.messages(chat["id"])
    assert [(m["role"], m["content"]) for m in msgs] == [("user", "original question"), ("assistant", "second answer")]
    assert {m["request_id"] for m in msgs} == {new_rid}
    assert a.get(f"/chats/{chat['id']}").json()["message_count"] == 2

    assert_problem(a.turn(chat["id"], old_rid), 404)
    assert a.turn(chat["id"], new_rid).json()["state"] == "done"
    old = db.execute("SELECT deleted_at, replaced_by_request_id FROM chat_turns WHERE request_id = ?", (ub(old_rid),)).fetchone()
    assert old["deleted_at"] is not None and bytes(old["replaced_by_request_id"]) == ub(new_rid)

    # the replaced request_id cannot be replayed
    p = a.stream(chat["id"], "original question", request_id=old_rid)
    assert p.status == 409 and p.problem["context"]["reason"] == "request_id_conflict"
    # the old turn is no longer the latest
    p = assert_problem(a.post(f"/chats/{chat['id']}/turns/{old_rid}/retry"), 409)
    assert p["context"]["reason"] == "NOT_LATEST_TURN"

    audits = [m["payload"] for m in outbox_payloads(db, "mini-chat.audit") if m["payload"].get("event_type") == "turn_retry"]
    mine = [x for x in audits if x.get("chat_id") == chat["id"]]
    assert len(mine) == 1
    assert mine[0]["original_request_id"] == old_rid and mine[0]["new_request_id"] == new_rid


def test_edit_replaces_content_and_regenerates(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    a.send(chat["id"], "first")
    second = a.send(chat["id"], "typo questoin")
    res = a.edit(chat["id"], second.request_id, "  fixed question  ")
    assert res.status == 200, res.body
    assert res.names[-1] == "done"
    new_rid = res.request_id
    assert new_rid != second.request_id

    body = mock_llm.chat_requests(chat["id"])[-1].json
    assert body["input"][-1]["content"].strip() == "fixed question"
    assert [m["content"] for m in body["input"] if m["role"] == "user"][0] == "first"
    msgs = a.messages(chat["id"])
    users = [m["content"].strip() for m in msgs if m["role"] == "user"]
    assert users == ["first", "fixed question"]
    assert len(msgs) == 4

    # empty edit content is rejected before anything changes
    p = assert_problem(a.patch(f"/chats/{chat['id']}/turns/{new_rid}", json={"content": "   "}), 400)
    assert field_reasons(p) == ["EMPTY_CONTENT"]
    assert a.turn(chat["id"], new_rid).json()["state"] == "done"


def test_delete_last_turn(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    first = a.send(chat["id"], "keep")
    second = a.send(chat["id"], "drop")
    # only the latest turn can be deleted
    p = assert_problem(a.delete(f"/chats/{chat['id']}/turns/{first.request_id}"), 409)
    assert p["context"]["reason"] == "NOT_LATEST_TURN"

    r = a.delete(f"/chats/{chat['id']}/turns/{second.request_id}")
    assert r.status_code == 204 and r.content == b""
    assert [m["content"] for m in a.messages(chat["id"]) if m["role"] == "user"] == ["keep"]
    assert a.get(f"/chats/{chat['id']}").json()["message_count"] == 2
    p = assert_problem(a.turn(chat["id"], second.request_id), 404)
    assert p["context"]["resource_type"] == RT_TURN
    # deleting it again: not the latest any more
    p = assert_problem(a.delete(f"/chats/{chat['id']}/turns/{second.request_id}"), 409)
    assert p["context"]["reason"] == "NOT_LATEST_TURN"
    # the previous turn is the latest again and can be retried
    assert a.retry(chat["id"], first.request_id).names[-1] == "done"
    # history sent to the provider excludes the deleted turn
    a.send(chat["id"], "after delete")
    contents = [m["content"] for m in mock_llm.chat_requests(chat["id"])[-1].json["input"] if m["role"] == "user"]
    assert "drop" not in contents


def test_mutation_of_running_turn_is_rejected(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release))
    live = a.open_stream(chat["id"], "still running")
    try:
        rid = live.read_until("delta")[0][1]["request_id"]
        for r in (
            a.post(f"/chats/{chat['id']}/turns/{rid}/retry"),
            a.patch(f"/chats/{chat['id']}/turns/{rid}", json={"content": "edit"}),
            a.delete(f"/chats/{chat['id']}/turns/{rid}"),
        ):
            p = assert_problem(r, 400)
            v = p["context"]["violations"][0]
            assert v["subject"] == "turn_state" and v["type"] == "STATE"
    finally:
        release.set()
    live.read_all()
    live.close()


def test_mutations_are_owner_scoped(api):
    a, b, c = api("A"), api("B"), api("C")
    chat = a.create_chat()
    res = a.send(chat["id"], "mine")
    for other in (b, c):
        assert_problem(other.post(f"/chats/{chat['id']}/turns/{res.request_id}/retry"), 404)
        assert_problem(other.patch(f"/chats/{chat['id']}/turns/{res.request_id}", json={"content": "x"}), 404)
        assert_problem(other.delete(f"/chats/{chat['id']}/turns/{res.request_id}"), 404)
    assert a.turn(chat["id"], res.request_id).json()["state"] == "done"


def test_retry_after_failure_and_concurrent_retries(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], json_response(500, {"error": {"message": "x"}}))
    failed = a.stream(chat["id"], "flaky")
    assert failed.names[-1] == "error"
    a.wait_turn(chat["id"], failed.request_id, ("error",))

    results = []
    barrier = threading.Barrier(2)

    def go():
        barrier.wait()
        results.append(a.retry(chat["id"], failed.request_id))

    threads = [threading.Thread(target=go) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    ok = [r for r in results if r.status == 200]
    rejected = [r for r in results if r.status != 200]
    assert len(ok) == 1 and len(rejected) == 1, [(r.status, r.body[:200]) for r in results]
    assert ok[0].names[-1] == "done"
    assert rejected[0].status in (400, 409)
    if rejected[0].status == 409:
        assert rejected[0].problem["context"]["reason"] in ("NOT_LATEST_TURN", "GENERATION_IN_PROGRESS")
    users = [m for m in a.messages(chat["id"]) if m["role"] == "user"]
    assert len(users) == 1


def test_retry_carries_attachments(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    att = a.upload(chat["id"], "doc.txt", b"retry doc", "text/plain").json()
    first = a.send(chat["id"], "about the doc", attachment_ids=[att["id"]])
    res = a.retry(chat["id"], first.request_id)
    assert res.names[-1] == "done"
    user = [m for m in a.messages(chat["id"]) if m["role"] == "user"][0]
    assert user["request_id"] == res.request_id
    assert [x["attachment_id"] for x in user["attachments"]] == [att["id"]]
    body = mock_llm.chat_requests(chat["id"])[-1].json
    assert any(t["type"] == "file_search" for t in body["tools"])
    # still locked: referenced by the new user message
    p = assert_problem(a.delete(f"/chats/{chat['id']}/attachments/{att['id']}"), 409)
    assert p["context"].get("resource_name") == "attachment_locked"


def test_edit_runs_preflight_quota(api, db):
    """A mutation goes through the send preflight: an exhausted quota rejects the edit
    and leaves the previous turn untouched."""
    from .test_quota import TOTAL_DAILY, clear_quota, seed

    c = api("C")
    clear_quota(db, "C")
    try:
        chat = c.create_chat(model="gpt-4.1-mini")
        res = c.send(chat["id"], "before")
        seed(db, "C", "daily", "total", spent=TOTAL_DAILY)
        r = c.patch(f"/chats/{chat['id']}/turns/{res.request_id}", json={"content": "after"})
        p = assert_problem(r, 429)
        assert p["context"]["violations"][0]["subject"] == "tokens"
        assert_problem(c.post(f"/chats/{chat['id']}/turns/{res.request_id}/retry"), 429)
        assert c.turn(chat["id"], res.request_id).json()["state"] == "done"
        assert [m["content"] for m in c.messages(chat["id"])][0] == "before"
    finally:
        clear_quota(db, "C")
