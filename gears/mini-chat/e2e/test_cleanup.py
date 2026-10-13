"""Chat deletion → outbox-driven provider cleanup (files, vector store), retries and failure handling.

Acceptance criteria covered:
* Cleanup & Recovery — "Chat deletion triggers reliable background cleanup of provider-side resources"
* Attachments — "Cleanup and abandoned-upload recovery behave correctly under failure" (provider delete failures)
"""

from __future__ import annotations

from helpers import (
    attachment_row,
    chat_row,
    make_png,
    new_chat,
    outbox_mentions,
    send_ok,
    sleep,
    stream_script,
    bg_send,
    tenant_id,
    turn_row,
    upload_ok,
    vector_store_row,
    wait_running,
    wait_until,
)


def _setup(srv):
    cid = new_chat(srv, "gpt-4.1-mini")
    doc = upload_ok(srv, cid, "doc.pdf")
    img = upload_ok(srv, cid, "pic.png", make_png(), "image/png")
    files = {a["id"]: attachment_row(srv, a["id"])["provider_file_id"] for a in (doc, img)}
    vs = vector_store_row(srv, cid)["vector_store_id"]
    return cid, files, vs


def test_chat_deletion_cleans_provider_resources(fresh):
    cid, files, vs = _setup(fresh)
    r = fresh.req("DELETE", f"/chats/{cid}")
    assert r.status_code == 204, r.text
    # Same transaction: attachments marked pending, cleanup message enqueued.
    for att_id in files:
        assert attachment_row(fresh, att_id)["cleanup_status"] in ("pending", "done")
    msgs = wait_until(lambda: outbox_mentions(fresh, cid, "chat_cleanup") or outbox_mentions(fresh, "chat_soft_delete"), timeout=5)
    assert msgs, "no chat cleanup outbox message"
    p = msgs[0]["payload"]
    assert p["chat_id"] == cid and p["tenant_id"] == tenant_id("a1")
    assert p["reason"] == "chat_soft_delete"
    assert p.get("system_request_id") and p.get("chat_deleted_at")
    # Provider files and the vector store are deleted.
    for att_id, fid in files.items():
        assert wait_until(lambda fid=fid: fresh.mock_requests(f"/files/{fid}", "DELETE"), timeout=20), f"file {fid} not deleted"
        assert wait_until(lambda a=att_id: attachment_row(fresh, a)["cleanup_status"] == "done", timeout=20)
    assert wait_until(lambda: fresh.mock_requests(f"/vector_stores/{vs}", "DELETE"), timeout=20), "vector store not deleted"
    assert wait_until(lambda: vector_store_row(fresh, cid) is None, timeout=20), "chat_vector_stores row kept"
    # Vector store deleted only after the files.
    reqs = fresh.mock_requests(method="DELETE")
    vs_idx = [i for i, r in enumerate(reqs) if r["path"].endswith(f"/vector_stores/{vs}")][0]
    file_idx = [i for i, r in enumerate(reqs) if "/files/" in r["path"]]
    assert max(file_idx) < vs_idx
    assert chat_row(fresh, cid)["deleted_at"] is not None


def test_cleanup_treats_404_as_success(fresh):
    cid, files, vs = _setup(fresh)
    fresh.mock_config(file_delete_status=404, vector_store_delete_status=404)
    assert fresh.req("DELETE", f"/chats/{cid}").status_code == 204
    for att_id in files:
        assert wait_until(lambda a=att_id: attachment_row(fresh, a)["cleanup_status"] == "done", timeout=20)
    assert wait_until(lambda: vector_store_row(fresh, cid) is None, timeout=20)


def test_cleanup_retries_then_fails_terminally(fresh):
    """Failing provider deletes: attempts counted with the last error, terminal failed at max_attempts (5)."""
    cid, files, vs = _setup(fresh)
    fresh.mock_config(file_delete_status=500)
    assert fresh.req("DELETE", f"/chats/{cid}").status_code == 204
    att_id = next(iter(files))
    first_fail = wait_until(lambda: (lambda r: r if int(r["cleanup_attempts"]) >= 1 else None)(attachment_row(fresh, att_id)), timeout=20)
    assert first_fail, attachment_row(fresh, att_id)
    assert first_fail["last_cleanup_error"]
    final = wait_until(lambda: (lambda r: r if r["cleanup_status"] == "failed" else None)(attachment_row(fresh, att_id)), timeout=90, interval=0.5)
    assert final, attachment_row(fresh, att_id)
    assert int(final["cleanup_attempts"]) == 5
    fid = files[att_id]
    assert len(fresh.mock_requests(f"/files/{fid}", "DELETE")) == 5
    # With no attachment left pending, the vector store is deleted.
    assert wait_until(lambda: fresh.mock_requests(f"/vector_stores/{vs}", "DELETE"), timeout=60, interval=0.5)


def test_cleanup_recovers_after_transient_failure(fresh):
    cid, files, vs = _setup(fresh)
    fresh.mock_config(file_delete_status=503)
    assert fresh.req("DELETE", f"/chats/{cid}").status_code == 204
    att_id = next(iter(files))
    assert wait_until(lambda: int(attachment_row(fresh, att_id)["cleanup_attempts"]) >= 1, timeout=20)
    fresh.mock_config(file_delete_status=200)
    for a in files:
        assert wait_until(lambda a=a: attachment_row(fresh, a)["cleanup_status"] in ("done", "failed"), timeout=90, interval=0.5)
    statuses = {attachment_row(fresh, a)["cleanup_status"] for a in files}
    assert "done" in statuses
    assert wait_until(lambda: vector_store_row(fresh, cid) is None, timeout=60, interval=0.5)


def test_chat_without_attachments_deletes_cleanly(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "hi")
    assert fresh.req("DELETE", f"/chats/{cid}").status_code == 204
    assert wait_until(lambda: outbox_mentions(fresh, cid), timeout=5)
    import time

    time.sleep(1)
    assert fresh.mock_requests(method="DELETE") == []


def test_running_turn_in_deleted_chat_completes_and_is_billed(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_script(stream_script("still", sleep(1500), " running"))
    bg = bg_send(fresh, cid, "long")
    wait_running(fresh, cid)
    assert fresh.req("DELETE", f"/chats/{cid}").status_code == 204
    bg.wait()
    assert bg.events[-1].event == "done"
    rid = bg.events[0].data["request_id"]
    assert turn_row(fresh, cid, rid)["state"] == "completed"
    from helpers import wait_usage_event

    assert wait_usage_event(fresh, rid)["billing_outcome"] == "completed"
