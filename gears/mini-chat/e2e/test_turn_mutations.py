"""Retry / edit / delete of the latest turn.

Acceptance criteria covered:
* Turn Mutations — "Retry / edit / delete act only on the latest, terminal turn"
* Turn Mutations — "A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id"
* Turn Mutations — "Concurrent mutations resolve deterministically"
* Turn Mutations — "Mutated turns correctly carry forward attachment and tool-usage history"
"""

from __future__ import annotations

import threading
import uuid

from helpers import (
    RT_CHAT,
    RT_TURN,
    QuotaRestorer,
    as_uuid,
    assert_not_found,
    assert_ok_stream,
    assert_problem,
    audit_event_types,
    audit_events,
    bg_send,
    ensure_quota_rows,
    get_chat,
    input_images,
    input_pairs,
    list_messages,
    make_png,
    new_chat,
    send_ok,
    set_quota,
    sleep,
    stream_script,
    tool,
    turn_row,
    turn_rows,
    turn_status,
    ub,
    upload_ok,
    user_id,
    wait_running,
    wait_until,
)


def _retry(srv, cid, rid, user="a1"):
    return srv.sse("POST", f"/chats/{cid}/turns/{rid}/retry", user)


def _edit(srv, cid, rid, content, user="a1"):
    return srv.sse("PATCH", f"/chats/{cid}/turns/{rid}", user, json={"content": content})


def _delete(srv, cid, rid, user="a1"):
    return srv.req("DELETE", f"/chats/{cid}/turns/{rid}", user)


def _chat_with_turns(srv, n=2, model="gpt-4.1-mini"):
    cid = new_chat(srv, model)
    rids = []
    for i in range(n):
        st, _, _ = send_ok(srv, cid, f"question {i + 1}")
        rids.append(st["request_id"])
    return cid, rids


# ── retry ─────────────────────────────────────────────────────────────────
def test_retry_latest_turn(fresh):
    cid, (r1, r2) = _chat_with_turns(fresh)
    before_updated = get_chat(fresh, cid)["updated_at"]
    fresh.mock_reset()
    fresh.mock_script(stream_script("Retried", " answer"))
    r, events = _retry(fresh, cid, r2)
    assert_ok_stream(r, events)
    new_rid = events[0].data["request_id"]
    assert new_rid not in (r1, r2)
    assert events[0].data["is_new_turn"] is True
    assert uuid.UUID(new_rid).version == 4
    old = turn_row(fresh, cid, r2)
    assert old["deleted_at"] is not None
    assert as_uuid(old["replaced_by_request_id"]) == new_rid
    new = turn_row(fresh, cid, new_rid)
    assert new["state"] == "completed" and new["deleted_at"] is None
    # The provider received the original content as the current user message.
    body = fresh.chat_requests()[-1]
    assert input_pairs(body)[-1] == ("user", "question 2")
    msgs = list_messages(fresh, cid)
    assert [m["content"] for m in msgs] == ["question 1", "Hello from mock", "question 2", "Retried answer"]
    assert msgs[2]["request_id"] == msgs[3]["request_id"] == new_rid
    assert get_chat(fresh, cid)["message_count"] == 4
    assert get_chat(fresh, cid)["updated_at"] != before_updated
    # Old request id: 404 on status, 409 request_id_conflict on send.
    assert_not_found(turn_status(fresh, cid, r2), RT_TURN)
    r, _ = fresh.stream(cid, "x", request_id=r2)
    assert_problem(r, 409, reason="request_id_conflict")
    # Audit event for the mutation.
    assert wait_until(lambda: any("turn_retry" in audit_event_types(p) for p in audit_events(fresh)), timeout=5)


def test_retry_failed_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    from helpers import http_error

    fresh.mock_script(http_error(500))
    r, events = fresh.stream(cid, "flaky")
    assert events[-1].event == "error"
    rid = events[0].data["request_id"]
    r, events = _retry(fresh, cid, rid)
    assert_ok_stream(r, events)
    assert [m["content"] for m in list_messages(fresh, cid)] == ["flaky", "Hello from mock"]


def test_retry_non_latest_turn_rejected(fresh):
    cid, (r1, r2) = _chat_with_turns(fresh)
    fresh.mock_reset()
    for resp in (_retry(fresh, cid, r1)[0], _edit(fresh, cid, r1, "new")[0], _delete(fresh, cid, r1)):
        assert_problem(resp, 409, reason="NOT_LATEST_TURN")
    assert fresh.chat_requests() == []
    assert turn_row(fresh, cid, r1)["deleted_at"] is None


def test_mutation_of_deleted_turn_is_not_latest(fresh):
    cid, (r1, r2) = _chat_with_turns(fresh)
    assert _delete(fresh, cid, r2).status_code == 204
    assert_problem(_delete(fresh, cid, r2), 409, reason="NOT_LATEST_TURN")
    assert_problem(_retry(fresh, cid, r2)[0], 409, reason="NOT_LATEST_TURN")


def test_mutation_of_running_turn_rejected(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    rid = str(uuid.uuid4())
    fresh.mock_script(stream_script("slow", sleep(3000), "."))
    bg = bg_send(fresh, cid, "x", request_id=rid)
    wait_running(fresh, cid)
    for resp in (_delete(fresh, cid, rid), _retry(fresh, cid, rid)[0], _edit(fresh, cid, rid, "e")[0]):
        assert_problem(resp, 400, subject="turn_state", vtype="STATE")
    bg.wait()
    assert turn_row(fresh, cid, rid)["state"] == "completed"
    assert turn_row(fresh, cid, rid)["deleted_at"] is None


def test_running_newer_turn_makes_target_not_latest(fresh):
    cid, (r1,) = _chat_with_turns(fresh, 1)
    fresh.mock_script(stream_script("slow", sleep(2500), "."))
    bg = bg_send(fresh, cid, "newer")
    wait_running(fresh, cid)
    assert_problem(_retry(fresh, cid, r1)[0], 409, reason="NOT_LATEST_TURN")
    bg.wait()


def test_mutation_of_foreign_requester_forbidden(fresh):
    cid, (r1,) = _chat_with_turns(fresh, 1)
    fresh.execute("UPDATE chat_turns SET requester_user_id = ? WHERE chat_id = ?", (ub(user_id("a2")), ub(cid)))
    for resp in (_retry(fresh, cid, r1)[0], _edit(fresh, cid, r1, "x")[0], _delete(fresh, cid, r1)):
        j = assert_problem(resp, 403)
    assert turn_row(fresh, cid, r1)["deleted_at"] is None


def test_mutation_unknown_turn_or_chat_404(fresh):
    cid, _ = _chat_with_turns(fresh, 1)
    unknown = str(uuid.uuid4())
    assert_not_found(_retry(fresh, cid, unknown)[0], RT_TURN)
    assert_not_found(_edit(fresh, cid, unknown, "x")[0], RT_TURN)
    assert_not_found(_delete(fresh, cid, unknown), RT_TURN)
    other_chat = str(uuid.uuid4())
    assert_not_found(_retry(fresh, other_chat, unknown)[0], RT_CHAT)
    assert_problem(fresh.req("DELETE", f"/chats/{cid}/turns/not-a-uuid"), 400)


# ── edit ──────────────────────────────────────────────────────────────────
def test_edit_latest_turn(fresh):
    cid, (r1, r2) = _chat_with_turns(fresh)
    fresh.mock_reset()
    r, events = _edit(fresh, cid, r2, "new")
    assert_ok_stream(r, events)
    new_rid = events[0].data["request_id"]
    assert new_rid != r2
    body = fresh.chat_requests()[-1]
    pairs = input_pairs(body)
    assert pairs[-1] == ("user", "new")
    assert ("user", "question 2") not in pairs, "the replaced turn is not part of the context"
    assert ("user", "question 1") in pairs
    msgs = list_messages(fresh, cid)
    assert [m["content"] for m in msgs] == ["question 1", "Hello from mock", "new", "Hello from mock"]
    old = turn_row(fresh, cid, r2)
    assert old["deleted_at"] is not None and as_uuid(old["replaced_by_request_id"]) == new_rid
    assert wait_until(lambda: any("turn_edit" in audit_event_types(p) for p in audit_events(fresh)), timeout=5)


def test_edit_empty_content_rejected_turn_unchanged(fresh):
    cid, (r1,) = _chat_with_turns(fresh, 1)
    fresh.mock_reset()
    r, events = _edit(fresh, cid, r1, "   ")
    assert events == []
    assert_problem(r, 400, field_reason="EMPTY_CONTENT")
    assert turn_row(fresh, cid, r1)["deleted_at"] is None
    assert fresh.chat_requests() == []
    r = fresh.req("PATCH", f"/chats/{cid}/turns/{r1}", json={})
    assert r.status_code == 422


def test_edit_over_context_budget_does_not_block_chat(fresh):
    """Context budget is enforced on edit; the chat is not left with a running turn."""
    cid = new_chat(fresh, "tiny-ctx")
    st, _, _ = send_ok(fresh, cid, "short")
    fresh.mock_reset()
    r, events = _edit(fresh, cid, st["request_id"], "z" * 12000)
    j = assert_problem(r, 400)
    from helpers import field_reasons

    assert set(field_reasons(j)) & {"INPUT_TOO_LONG", "CONTEXT_BUDGET_EXCEEDED"}, j
    assert fresh.chat_requests() == []
    assert not [t for t in turn_rows(fresh, cid) if t["state"] == "running"]
    # The chat still accepts messages.
    send_ok(fresh, cid, "still works")


# ── delete ────────────────────────────────────────────────────────────────
def test_delete_latest_turn(fresh):
    cid, (r1, r2) = _chat_with_turns(fresh)
    r = _delete(fresh, cid, r2)
    assert r.status_code == 204, r.text
    assert r.content == b""
    assert turn_row(fresh, cid, r2)["deleted_at"] is not None
    assert turn_row(fresh, cid, r2)["replaced_by_request_id"] is None
    assert_not_found(turn_status(fresh, cid, r2), RT_TURN)
    assert [m["content"] for m in list_messages(fresh, cid)] == ["question 1", "Hello from mock"]
    assert get_chat(fresh, cid)["message_count"] == 2
    # The previous turn is latest again and can be deleted too.
    assert _delete(fresh, cid, r1).status_code == 204
    assert list_messages(fresh, cid) == []
    assert wait_until(lambda: any("turn_delete" in audit_event_types(p) for p in audit_events(fresh)), timeout=5)
    # Deleted turns are excluded from the next context.
    fresh.mock_reset()
    send_ok(fresh, cid, "fresh start")
    assert input_pairs(fresh.chat_requests()[-1]) == [("user", "fresh start")]


# ── pipeline: quota / attachments / tools ─────────────────────────────────
def test_retry_quota_rejection_leaves_turn_unchanged(qs):
    ensure_quota_rows(qs, "a2")
    cid = new_chat(qs, "gpt-4.1-mini", user="a2")
    st, _, _ = send_ok(qs, cid, "q", user="a2")
    saver = QuotaRestorer(qs, "a2")
    try:
        set_quota(qs, "a2", "daily", "total", spent_credits_micro=100_000_000)
        qs.mock_reset()
        r, events = _retry(qs, cid, st["request_id"], "a2")
        assert events == []
        assert_problem(r, 429, subject="tokens", description="quota_exceeded")
        r, _ = _edit(qs, cid, st["request_id"], "edited", "a2")
        assert_problem(r, 429, subject="tokens", description="quota_exceeded")
        assert qs.chat_requests() == []
        old = turn_row(qs, cid, st["request_id"])
        assert old["deleted_at"] is None and old["state"] == "completed"
        assert len(turn_rows(qs, cid)) == 1
    finally:
        saver.restore()


def test_retry_reserves_quota_like_send(qs):
    ensure_quota_rows(qs, "a2")
    cid = new_chat(qs, "gpt-4.1-mini", user="a2")
    st, _, _ = send_ok(qs, cid, "q", user="a2")
    qs.mock_script(stream_script("slow", sleep(2000), "."))
    from helpers import BgStream, quota_val, running_turn

    reserved_before = quota_val(qs, "a2", "daily", "total", "reserved_credits_micro")
    bg = BgStream(qs, "POST", f"/chats/{cid}/turns/{st['request_id']}/retry", "a2")
    wait_running(qs, cid)
    row = wait_until(lambda: (lambda r: r if r and r["reserved_credits_micro"] is not None else None)(running_turn(qs, cid)), timeout=5)
    assert row, "retry turn has no reserve"
    assert quota_val(qs, "a2", "daily", "total", "reserved_credits_micro") == reserved_before + int(row["reserved_credits_micro"])
    bg.wait()
    assert quota_val(qs, "a2", "daily", "total", "reserved_credits_micro") == reserved_before


def test_retry_carries_attachments_and_images(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    doc = upload_ok(fresh, cid, "notes.pdf")
    img = upload_ok(fresh, cid, "pic.png", make_png(), "image/png")
    st, _, _ = send_ok(fresh, cid, "look at these", attachment_ids=[doc["id"], img["id"]])
    file_id = fresh.query("SELECT provider_file_id FROM attachments WHERE id = ?", (ub(img["id"]),))[0][0]
    fresh.mock_reset()
    r, events = _retry(fresh, cid, st["request_id"])
    assert_ok_stream(r, events)
    body = fresh.chat_requests()[-1]
    assert [i.get("file_id") for i in input_images(body)] == [file_id], "images of the original message are re-sent"
    assert tool(body, "file_search") is not None
    msgs = list_messages(fresh, cid)
    assert {a["attachment_id"] for a in msgs[0]["attachments"]} == {doc["id"], img["id"]}
    # Edit also carries the attachments forward.
    rid2 = events[0].data["request_id"]
    r, events = _edit(fresh, cid, rid2, "edited text")
    assert_ok_stream(r, events)
    msgs = list_messages(fresh, cid)
    assert msgs[0]["content"] == "edited text"
    assert {a["attachment_id"] for a in msgs[0]["attachments"]} == {doc["id"], img["id"]}


def test_retry_excludes_deleted_attachment(fresh):
    """Attachments deleted since the original turn are silently excluded from the copy."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    a1 = upload_ok(fresh, cid, "a.pdf")
    a2 = upload_ok(fresh, cid, "b.pdf")
    st, _, _ = send_ok(fresh, cid, "two docs", attachment_ids=[a1["id"], a2["id"]])
    # A referenced attachment cannot be deleted through the API (attachment_locked); soft-delete it in the DB.
    created = fresh.query("SELECT created_at FROM attachments WHERE id = ?", (ub(a2["id"]),))[0][0]
    fresh.execute("UPDATE attachments SET deleted_at = ? WHERE id = ?", (created, ub(a2["id"])))
    r, events = _retry(fresh, cid, st["request_id"])
    assert_ok_stream(r, events)
    msgs = list_messages(fresh, cid)
    assert [a["attachment_id"] for a in msgs[0]["attachments"]] == [a1["id"]]
    new_user_msg = msgs[0]["id"]
    linked = fresh.query("SELECT attachment_id FROM message_attachments WHERE message_id = ?", (ub(new_user_msg),))
    assert [as_uuid(r[0]) for r in linked] == [a1["id"]]


def test_retry_reuses_web_search_flag(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    st, _, _ = send_ok(fresh, cid, "search the web", web_search={"enabled": True})
    first_body = fresh.chat_requests()[-1]
    assert tool(first_body, "web_search") is not None
    assert turn_row(fresh, cid, st["request_id"])["web_search_enabled"] in (1, True)
    fresh.mock_reset()
    r, events = _retry(fresh, cid, st["request_id"])
    assert_ok_stream(r, events)
    body = fresh.chat_requests()[-1]
    assert tool(body, "web_search") is not None, "retry reuses web_search_enabled of the original turn"
    new_rid = events[0].data["request_id"]
    assert turn_row(fresh, cid, new_rid)["web_search_enabled"] in (1, True)


def test_retry_reapplies_image_guards(fresh):
    """Images of the original message go through the image guards again; a rejection leaves the turn unchanged."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    img = upload_ok(fresh, cid, "pic.png", make_png(), "image/png")
    st, _, _ = send_ok(fresh, cid, "what is this", attachment_ids=[img["id"]])
    # The chat's model loses vision (simulated by pointing the chat at a non-vision model).
    fresh.execute("UPDATE chats SET model = 'std-novision' WHERE id = ?", (ub(cid),))
    fresh.mock_reset()
    r, events = _retry(fresh, cid, st["request_id"])
    assert events == []
    assert_problem(r, 400, field_reason="VISION_NOT_SUPPORTED")
    assert fresh.chat_requests() == []
    old = turn_row(fresh, cid, st["request_id"])
    assert old["deleted_at"] is None and old["state"] == "completed"


# ── concurrency ───────────────────────────────────────────────────────────
def test_concurrent_retries_resolve_deterministically(fresh):
    cid, (r1,) = _chat_with_turns(fresh, 1)
    fresh.mock_reset()
    fresh.mock_script(stream_script("one", sleep(1500), " winner"))
    results = []
    barrier = threading.Barrier(2)

    def go():
        barrier.wait()
        results.append(_retry(fresh, cid, r1))

    ts = [threading.Thread(target=go) for _ in range(2)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(30)
    codes = sorted(r.status_code for r, _ in results)
    assert codes == [200, 409], codes
    loser = [r for r, _ in results if r.status_code == 409][0]
    j = assert_problem(loser, 409)
    assert j["context"].get("reason") in ("GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"), j
    winner_events = [e for r, e in results if r.status_code == 200][0]
    assert winner_events[-1].event == "done"
    live = [t for t in turn_rows(fresh, cid) if t["deleted_at"] is None]
    assert len(live) == 1, "exactly one replacement turn"
    assert len(fresh.chat_requests()) == 1


def test_concurrent_delete_and_retry(fresh):
    cid, (r1,) = _chat_with_turns(fresh, 1)
    fresh.mock_reset()
    fresh.mock_script(stream_script("slow", sleep(1000), "."))
    out = {}
    barrier = threading.Barrier(2)

    def do_retry():
        barrier.wait()
        out["retry"] = _retry(fresh, cid, r1)[0].status_code

    def do_delete():
        barrier.wait()
        out["delete"] = _delete(fresh, cid, r1).status_code

    ts = [threading.Thread(target=do_retry), threading.Thread(target=do_delete)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(30)
    # Exactly one mutation wins; the other sees a non-latest (or running) turn.
    assert (out["retry"] == 200) != (out["delete"] == 204), out
    assert out["retry"] in (200, 409, 400) and out["delete"] in (204, 409, 400), out
