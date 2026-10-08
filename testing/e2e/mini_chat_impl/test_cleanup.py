"""Chat deletion cleanup (acceptance: Cleanup & Recovery — provider-side resources)."""

import pytest

from mchelpers import MIME_PNG, BackgroundStream, make_pdf, make_png, nonce, ub, wait_for


def _deletes(mock_llm, fragment):
    return mock_llm.requests(path_contains=fragment, method="DELETE")


# Acceptance: Cleanup — chat deletion removes provider files, then the vector store
@pytest.mark.timeout(90)
def test_chat_deletion_cleans_provider_resources(api, db, mock_llm):
    c = api.create_chat()
    doc = api.upload_ready(c["id"], "doc.pdf", make_pdf("cleanup"), "application/pdf")
    img = api.upload_ready(c["id"], "img.png", make_png(), MIME_PNG)
    doc_fid = db.attachment_row(doc["id"])["provider_file_id"]
    img_fid = db.attachment_row(img["id"])["provider_file_id"]
    vs = db.one("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),))["vector_store_id"]
    assert vs
    r = api.delete(f"/v1/chats/{c['id']}")
    assert r.status_code == 204
    # soft delete marks the attachments for cleanup in the same transaction
    for a_id in (doc["id"], img["id"]):
        assert db.attachment_row(a_id)["cleanup_status"] in ("pending", "done")
    wait_for(lambda: _deletes(mock_llm, f"/files/{doc_fid}") and _deletes(mock_llm, f"/files/{img_fid}"), timeout=60, desc="file deletes")
    wait_for(lambda: _deletes(mock_llm, f"/vector_stores/{vs}"), timeout=60, desc="vector store delete")
    file_seq = max(r["seq"] for r in _deletes(mock_llm, f"/files/{doc_fid}") + _deletes(mock_llm, f"/files/{img_fid}"))
    vs_seq = min(r["seq"] for r in _deletes(mock_llm, f"/vector_stores/{vs}"))
    assert vs_seq > file_seq, "the vector store is deleted only after the attachment files"
    wait_for(
        lambda: db.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),)) == [],
        timeout=30,
        desc="chat_vector_stores row removed",
    )
    for a_id in (doc["id"], img["id"]):
        wait_for(lambda: db.attachment_row(a_id)["cleanup_status"] == "done", timeout=30, desc="cleanup done")
    st = mock_llm.state()
    assert st["files"][doc_fid]["deleted"] and st["vector_stores"][vs]["deleted"]


# Acceptance: Cleanup — retried after a provider failure, vector store still removed afterwards
@pytest.mark.timeout(150)
def test_chat_deletion_cleanup_retries(api, db, mock_llm):
    c = api.create_chat()
    doc = api.upload_ready(c["id"], "retry.pdf", make_pdf("retry"), "application/pdf")
    fid = db.attachment_row(doc["id"])["provider_file_id"]
    vs = db.one("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),))["vector_store_id"]
    mock_llm.configure(file_delete_fail_count=1)
    assert api.delete(f"/v1/chats/{c['id']}").status_code == 204
    wait_for(lambda: db.attachment_row(doc["id"])["cleanup_status"] == "done", timeout=120, interval=1, desc="cleanup done after retry")
    assert int(db.attachment_row(doc["id"])["cleanup_attempts"] or 0) >= 1
    assert len(_deletes(mock_llm, f"/files/{fid}")) >= 2
    wait_for(lambda: _deletes(mock_llm, f"/vector_stores/{vs}"), timeout=60, interval=1, desc="vector store delete")


# Acceptance: Cleanup — deleting a chat does not cancel its running turn
@pytest.mark.timeout(60)
def test_chat_deletion_keeps_running_turn(api, db):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "slow [[slow:3:1]] " + nonce()).wait_started()
    assert api.delete(f"/v1/chats/{c['id']}").status_code == 204
    res = bg.join(30)
    assert res is not None and res.events[-1][0] == "done", res.events if res else None
    row = db.turn_row(c["id"], bg.request_id)
    assert row["state"] == "completed"
