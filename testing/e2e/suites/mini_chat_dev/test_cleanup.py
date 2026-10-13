"""Outbox-driven provider cleanup (DESIGN section 3.6 "Cleanup on Chat Deletion", section 4 Phase 2)."""

from contextlib import closing
from pathlib import Path

import pytest

from .helpers import PREFIX, api, assert_problem, create_chat, db, uuid_bytes, wait_until

pytestmark = pytest.mark.usefixtures("server")

TESTDATA = Path(__file__).resolve().parents[2] / "testdata"
PDF = TESTDATA / "pdf" / "test_file_one_page_en.pdf"
PNG = TESTDATA / "images" / "tiny.png"
CHAT_TYPE = "gts.cf.core.mini_chat.chat.v1~"


def _upload(s, chat_id: str, path: Path, content_type: str) -> dict:
    r = s.post(f"{PREFIX}/chats/{chat_id}/attachments", files={"file": (path.name, path.read_bytes(), content_type)})
    assert r.status_code == 201, r.text
    return r.json()


def _attachments(chat_id: str) -> dict[bytes, dict]:
    with closing(db()) as conn:
        rows = conn.execute("SELECT id, provider_file_id, cleanup_status FROM attachments WHERE chat_id = ?",
                            (uuid_bytes(chat_id),)).fetchall()
    return {r["id"]: dict(r) for r in rows}


def _vector_store(chat_id: str):
    with closing(db()) as conn:
        return conn.execute("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?",
                            (uuid_bytes(chat_id),)).fetchone()


def test_chat_delete_cleans_files_then_vector_store(reset_mock):
    s = api()
    chat = create_chat(s)
    pdf = _upload(s, chat["id"], PDF, "application/pdf")
    img = _upload(s, chat["id"], PNG, "image/png")
    assert pdf["status"] == "ready" and img["status"] == "ready"
    rows = _attachments(chat["id"])
    file_ids = {rows[uuid_bytes(a["id"])]["provider_file_id"] for a in (pdf, img)}
    assert len(file_ids) == 2 and None not in file_ids
    vs = _vector_store(chat["id"])["vector_store_id"]
    assert vs

    assert s.delete(f"{PREFIX}/chats/{chat['id']}").status_code == 204

    def deletes():
        files = [r for r in reset_mock.requests(method="DELETE", route="delete_file")
                 if any(r["path"].endswith(f"/files/{f}") for f in file_ids)]
        stores = [r for r in reset_mock.requests(method="DELETE", route="delete_vector_store")
                  if r["path"].endswith(f"/vector_stores/{vs}")]
        return (files, stores) if len(files) == 2 and stores else None

    files, stores = wait_until(deletes, timeout=30, message="provider file and vector store deletes")
    assert {r["path"].rsplit("/", 1)[-1] for r in files} == file_ids
    assert len(stores) == 1
    assert stores[0]["seq"] > max(r["seq"] for r in files), "vector store deleted after the files"

    wait_until(lambda: all(r["cleanup_status"] == "done" for r in _attachments(chat["id"]).values()),
               message="attachments cleanup_status = done")
    wait_until(lambda: _vector_store(chat["id"]) is None, message="chat_vector_stores row removed")
    assert_problem(s.get(f"{PREFIX}/chats/{chat['id']}"), 404, "not_found", resource_type=CHAT_TYPE)


def test_attachment_delete_cleans_provider_file(reset_mock):
    s = api()
    chat = create_chat(s)
    img = _upload(s, chat["id"], PNG, "image/png")
    fid = _attachments(chat["id"])[uuid_bytes(img["id"])]["provider_file_id"]

    assert s.delete(f"{PREFIX}/chats/{chat['id']}/attachments/{img['id']}").status_code == 204

    wait_until(lambda: [r for r in reset_mock.requests(method="DELETE", route="delete_file")
                        if r["path"].endswith(f"/files/{fid}")],
               timeout=30, message="provider file delete")
    wait_until(lambda: _attachments(chat["id"])[uuid_bytes(img["id"])]["cleanup_status"] == "done",
               message="attachment cleanup_status = done")
    assert s.get(f"{PREFIX}/chats/{chat['id']}").status_code == 200
