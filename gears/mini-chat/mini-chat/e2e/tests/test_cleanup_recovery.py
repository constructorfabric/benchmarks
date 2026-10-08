"""Chat-deletion cleanup, upload reaper, orphan watchdog, thread summary."""

import threading
import time
import uuid

import pytest

from conftest import start_server, wait_for
from testdata_helpers import PDF, TEXT, png


def b(u):
    return uuid.UUID(str(u)).bytes


def _deletes(mock, part):
    return [r for r in mock.requests() if r["method"] == "DELETE" and part in r["path"]]


def test_chat_deletion_cleans_provider_resources(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    d1 = api.upload(chat["id"], "a.txt", TEXT, "text/plain").json()
    d2 = api.upload(chat["id"], "b.pdf", PDF, "application/pdf").json()
    img = api.upload(chat["id"], "c.png", png(8, 8), "image/png").json()
    rows = server.query("SELECT id, provider_file_id FROM attachments WHERE chat_id = ?", b(chat["id"]))
    file_ids = {r["provider_file_id"] for r in rows}
    assert len(file_ids) == 3
    vs = server.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", b(chat["id"]))[0]["vector_store_id"]
    mock.config(delete_fail_count=2)  # first deletes fail -> retried
    assert api.delete(f"/chats/{chat['id']}").status_code == 204

    def done():
        deleted = {r["path"].rsplit("/", 1)[1] for r in _deletes(mock, "/files/") if "/vector_stores/" not in r["path"]}
        return file_ids <= deleted and any(r["path"].endswith(f"/vector_stores/{vs}") for r in _deletes(mock, "/vector_stores/"))

    wait_for(done, timeout=60, msg="provider cleanup")
    wait_for(
        lambda: all(r["cleanup_status"] == "done" for r in server.query("SELECT cleanup_status FROM attachments WHERE chat_id = ?", b(chat["id"]))),
        timeout=30,
        msg="cleanup_status done",
    )
    # chat-level reads are gone
    for a in (d1, d2, img):
        assert api.get(f"/chats/{chat['id']}/attachments/{a['id']}").status_code == 404


def test_upload_reaper_fails_abandoned_uploads(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    att = api.upload(chat["id"], "stale.txt", TEXT, "text/plain").json()
    fid = server.query("SELECT provider_file_id FROM attachments WHERE id = ?", b(att["id"]))[0]["provider_file_id"]
    # simulate an upload whose request died after the provider upload
    conn = server.db()
    conn.execute(
        "UPDATE attachments SET status = 'uploaded', updated_at = '2020-01-01T00:00:00.000000000Z' WHERE id = ?",
        (b(att["id"]),),
    )
    conn.commit()
    conn.close()
    wait_for(lambda: api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()["status"] == "failed", timeout=20, msg="reaped")
    got = api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()
    assert got["error_code"] == "upload_abandoned"
    wait_for(lambda: any(r["path"].endswith(f"/files/{fid}") for r in _deletes(mock, "/files/")), timeout=20, msg="file delete")


def test_orphan_watchdog_finalizes_stuck_turn(mock):
    srv = start_server("orphan")
    try:
        api = srv.client("user-a")
        chat = api.create_chat(model="gpt-4.1-mini")
        rid = str(uuid.uuid4())

        def hang():
            try:
                srv.client("user-a").send(chat["id"], "MOCK_HANG", request_id=rid)
            except Exception:
                pass

        t = threading.Thread(target=hang, daemon=True)
        t.start()
        wait_for(lambda: api.turn(chat["id"], rid).status_code == 200, msg="turn running")
        before = api.quota()
        # crash the process mid-turn
        srv.proc.kill()
        srv.proc.wait(timeout=10)
        conn = srv.db()
        conn.execute(
            "UPDATE chat_turns SET last_progress_at = '2020-01-01T00:00:00.000000000Z' WHERE request_id = ?",
            (b(rid),),
        )
        conn.commit()
        conn.close()
        srv.start()
        st = wait_for(lambda: (lambda j: j if j["state"] != "running" else None)(api.turn(chat["id"], rid).json()), timeout=30, msg="orphan finalized")
        assert st["state"] == "error" and st["error_code"] == "orphan_timeout"
        # the reserve (counted as used while running) is replaced by the estimated debit
        tot = lambda q: [p for t in q["tiers"] if t["tier"] == "total" for p in t["periods"] if p["period"] == "daily"][0]["used_credits_micro"]
        assert 0 < tot(api.quota()) < tot(before)
        row = srv.query("SELECT reserved_credits_micro, spent_credits_micro FROM quota_usage WHERE bucket = 'total' AND period_type = 'daily'")
        assert row and row[0]["reserved_credits_micro"] == 0 and row[0]["spent_credits_micro"] > 0
        # the chat is usable again
        assert api.send(chat["id"], "after crash").terminal[0] == "done"
        wait_for(lambda: "turn_failed" in srv.log_text() and rid in srv.log_text(), timeout=20, msg="audit")
    finally:
        srv.cleanup()


# ── thread summary ─────────────────────────────────────────────────────────

BIG = "lorem ipsum dolor sit amet consectetur " * 60  # ~2400 chars


def _fill_until_summary(api, server, mock, chat_id, max_turns=8):
    for i in range(max_turns):
        s = api.send(chat_id, f"MOCK_ECHO turn {i} {BIG}")
        assert s.terminal[0] == "done", s.text
        rows = server.query("SELECT summary_text FROM thread_summaries WHERE chat_id = ?", b(chat_id))
        if rows:
            return i
        if mock.summary_requests(chat_id):
            wait_for(lambda: server.query("SELECT 1 FROM thread_summaries WHERE chat_id = ?", b(chat_id)), timeout=30, msg="summary row")
            return i
    raise AssertionError("summary never triggered")


def test_thread_summary_generation_and_use(server, mock):
    api = server.client("user-b")
    chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
    _fill_until_summary(api, server, mock, chat["id"])
    row = server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", b(chat["id"]))[0]
    assert row["summary_text"] == "Mock summary of the conversation."
    compressed = server.query("SELECT COUNT(*) AS n FROM messages WHERE chat_id = ? AND is_compressed = 1", b(chat["id"]))[0]["n"]
    assert compressed >= 2
    # summary request is a system task, not billed to the user, no chat turn
    sreq = mock.summary_requests(chat["id"])[-1]["json"]
    assert sreq["metadata"]["request_type"] == "summary" and sreq.get("stream") in (None, False)
    turns_before = server.query("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?", b(chat["id"]))[0]["n"]
    # next turn uses the summary and reports it
    s = api.send(chat["id"], "MOCK_ECHO after summary")
    assert s.terminal[0] == "done"
    st = s.first("stream_started")
    assert st.get("thread_summary_applied", {}).get("token_estimate", 0) > 0
    req = mock.chat_requests(chat["id"])[-1]["json"]
    import json as _json

    text = _json.dumps(req["input"]) + req.get("instructions", "")
    assert "Mock summary of the conversation." in text
    assert "turn 0 " not in text  # compressed messages are not sent
    assert server.query("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?", b(chat["id"]))[0]["n"] == turns_before + 1
    # history still shows every message
    assert len(api.all_messages(chat["id"])) == 2 * (turns_before + 1)
    def system_usage():
        return [
            l
            for l in server.log_text().splitlines()
            if "mini-chat usage event" in l and chat["id"] in l and "billing_outcome=system_task" in l
        ]

    lines = wait_for(system_usage, timeout=10, msg="system usage event")
    assert "requester_type=system" in lines[0] and "actual_credits_micro=0" in lines[0]


def test_thread_summary_retries_after_failure(server, mock):
    api = server.client("user-b")
    chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
    mock.config(summary_fail_count=2)
    _fill_until_summary(api, server, mock, chat["id"])
    wait_for(lambda: server.query("SELECT 1 FROM thread_summaries WHERE chat_id = ?", b(chat["id"])), timeout=60, msg="summary after retries")
    assert len(mock.summary_requests(chat["id"])) >= 3


def test_mutation_invalidates_covering_summary(server, mock):
    api = server.client("user-b")
    chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
    _fill_until_summary(api, server, mock, chat["id"])
    wait_for(lambda: server.query("SELECT 1 FROM thread_summaries WHERE chat_id = ?", b(chat["id"])), timeout=30, msg="summary")
    row = server.query("SELECT summarized_up_to_message_id FROM thread_summaries WHERE chat_id = ?", b(chat["id"]))[0]
    frontier = row["summarized_up_to_message_id"]
    frontier = str(uuid.UUID(bytes=frontier)) if isinstance(frontier, bytes) else frontier
    # delete turns from the end until the summary frontier's turn is deleted
    msgs = api.all_messages(chat["id"])
    covered_rid = [m["request_id"] for m in msgs if m["id"] == frontier][0]
    for m in reversed([m for m in msgs if m["role"] == "user"]):
        assert api.delete(f"/chats/{chat['id']}/turns/{m['request_id']}").status_code == 204
        if m["request_id"] == covered_rid:
            break
    assert server.query("SELECT 1 FROM thread_summaries WHERE chat_id = ?", b(chat["id"])) == []
    assert server.query("SELECT COUNT(*) AS n FROM messages WHERE chat_id = ? AND is_compressed = 1", b(chat["id"]))[0]["n"] == 0
