"""Attachments: upload / get / delete, provider Files + Vector Stores calls, file_search and image
input, deletion guard and chat-deletion cleanup.

DESIGN §3.3 Upload/Get Attachment, §3.6 File Upload, §4 Attachment Deletion, Cleanup on Chat Deletion.
"""

import base64
import struct
import time
import uuid

import pytest

from conftest import (
    ATTACHMENT_RT,
    CHAT_RT,
    assert_no_provider_ids,
    assert_problem,
    make_png,
    wait_until,
)

FILES_RE = r"^(/openai)?(/v1)?/files$"
DETAIL_KEYS = {"id", "filename", "content_type", "size_bytes", "status", "kind", "error_code", "doc_summary",
               "img_thumbnail", "summary_updated_at", "created_at"}


def upload_doc(api, chat_id, name="notes.txt", data=b"hello world\n", ctype="text/plain"):
    r = api.upload(chat_id, name, data, ctype)
    assert r.status_code == 201, r.text
    return r.json()


def provider_ids(db, attachment_id):
    row = db.attachment(attachment_id)
    return row["provider_file_id"]


def vector_store_id(db, chat_id):
    rows = db.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?",
                    (uuid.UUID(chat_id).bytes,))
    return rows[0]["vector_store_id"] if rows else None


def webp_size(data):
    """(width, height) of a WebP image (VP8 / VP8L / VP8X)."""
    assert data[:4] == b"RIFF" and data[8:12] == b"WEBP", data[:16]
    fourcc = data[12:16]
    if fourcc == b"VP8 ":
        w, h = struct.unpack("<HH", data[26:30])
        return w & 0x3FFF, h & 0x3FFF
    if fourcc == b"VP8L":
        b = data[21:25]
        bits = int.from_bytes(b, "little")
        return (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
    if fourcc == b"VP8X":
        w = int.from_bytes(data[24:27], "little") + 1
        h = int.from_bytes(data[27:30], "little") + 1
        return w, h
    raise AssertionError(f"unknown webp chunk {fourcc!r}")


# ------------------------------------------------------------------ upload: documents
def test_upload_document_ready(api, chat, fresh_mock, db):
    att = upload_doc(api, chat["id"])
    assert set(att) <= DETAIL_KEYS
    assert att["status"] == "ready" and att["kind"] == "document"
    assert att["filename"] == "notes.txt" and att["content_type"] == "text/plain"
    assert att["size_bytes"] == len(b"hello world\n")
    for k in ("error_code", "img_thumbnail", "doc_summary", "summary_updated_at"):
        assert k not in att
    assert_no_provider_ids(att)
    uuid.UUID(att["id"])

    files = fresh_mock.calls("POST", FILES_RE)
    stores = fresh_mock.calls("POST", r"/vector_stores$")
    adds = fresh_mock.calls("POST", r"/vector_stores/[^/]+/files$")
    assert len(files) == 1 and len(stores) == 1 and len(adds) == 1
    fid = provider_ids(db, att["id"])
    vid = vector_store_id(db, chat["id"])
    assert fid and fid.startswith("file-") and vid and vid.startswith("vs_")
    assert adds[0]["path"].endswith(f"/vector_stores/{vid}/files")
    assert adds[0]["body"]["file_id"] == fid
    row = db.attachment(att["id"])
    assert row["for_file_search"] and not row["for_code_interpreter"]

    # GET returns the same detail.
    r = api.get(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert r.status_code == 200 and r.json() == att

    # A second document reuses the chat vector store.
    fresh_mock.reset()
    upload_doc(api, chat["id"], name="more.md", ctype="text/markdown")
    assert fresh_mock.calls("POST", r"/vector_stores$") == []
    assert len(fresh_mock.calls("POST", r"/vector_stores/[^/]+/files$")) == 1


def test_octet_stream_inferred_from_extension(api, chat):
    att = upload_doc(api, chat["id"], name="readme.md", ctype="application/octet-stream")
    assert att["content_type"] == "text/markdown"
    r = api.upload(chat["id"], "blob.unknownext", b"zzz", "application/octet-stream")
    assert_problem(r, 400, "invalid_argument", reason="UNSUPPORTED_CONTENT_TYPE")


def test_upload_validation_errors(api, chat, fresh_mock):
    r = api.upload(chat["id"], "x.bin", b"\x00\x01", "application/x-msdownload")
    assert_problem(r, 400, "invalid_argument", reason="UNSUPPORTED_CONTENT_TYPE")
    r = api.post(f"/chats/{chat['id']}/attachments", data=b"abc",
                 headers={"Content-Type": "multipart/form-data"})
    assert_problem(r, 400, "invalid_argument", field="content_type", reason="BOUNDARY_REQUIRED")
    r = api.upload(chat["id"], "a.txt", b"abc", "text/plain", field="notfile")
    assert_problem(r, 400, "invalid_argument", field="file", reason="MISSING_FILE")
    assert fresh_mock.calls("POST", FILES_RE) == []


def test_upload_unknown_chat(api):
    r = api.upload(str(uuid.uuid4()), "a.txt", b"abc", "text/plain")
    assert_problem(r, 404, "not_found", resource_type=CHAT_RT)


def test_get_attachment_errors(api, chat):
    other = api.create_chat()
    att = upload_doc(api, other["id"])
    assert_problem(api.get(f"/chats/{chat['id']}/attachments/{att['id']}"), 404, "not_found",
                   resource_type=ATTACHMENT_RT)
    assert_problem(api.get(f"/chats/{chat['id']}/attachments/{uuid.uuid4()}"), 404, "not_found",
                   resource_type=ATTACHMENT_RT)
    assert_problem(api.get(f"/chats/{chat['id']}/attachments/nope"), 400, "invalid_argument",
                   reason="invalid_path_params")


# ------------------------------------------------------------------ upload: images
def test_upload_image_with_thumbnail(api, chat, fresh_mock, db):
    png = make_png(300, 150)
    r = api.upload(chat["id"], "pic.png", png, "image/png")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "image"
    assert att["content_type"] == "image/png" and att["size_bytes"] == len(png)
    thumb = att["img_thumbnail"]
    assert thumb["content_type"] == "image/webp"
    assert thumb["width"] <= 128 and thumb["height"] <= 128
    assert (thumb["width"], thumb["height"]) == (128, 64)  # aspect ratio preserved
    raw = base64.b64decode(thumb["data_base64"])
    assert len(raw) <= 131072
    assert webp_size(raw) == (thumb["width"], thumb["height"])
    assert_no_provider_ids(att)
    # Images are uploaded to Files but never indexed.
    assert len(fresh_mock.calls("POST", FILES_RE)) == 1
    assert fresh_mock.calls("POST", r"/vector_stores") == []
    assert vector_store_id(db, chat["id"]) is None
    assert api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json() == att


def test_undecodable_image_is_ready_without_thumbnail(api, chat):
    r = api.upload(chat["id"], "broken.png", b"\x89PNG\r\n\x1a\nnot really", "image/png")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and "img_thumbnail" not in att and "error_code" not in att


# ------------------------------------------------------------------ upload: provider failures
def test_provider_upload_failure(api, chat, fresh_mock, db):
    fresh_mock.config(file_upload_status=500)
    r = api.upload(chat["id"], "fail.txt", b"data", "text/plain")
    body = assert_problem(r, 503, "service_unavailable")
    assert r.headers.get("Retry-After") == "10"
    assert body["context"].get("retry_after_seconds") == 10
    assert_no_provider_ids(body)
    rows = db.query("SELECT * FROM attachments WHERE chat_id = ? AND filename = 'fail.txt'",
                    (uuid.UUID(chat["id"]).bytes,))
    assert len(rows) == 1 and rows[0]["status"] == "failed" and rows[0]["error_code"]
    got = api.get(f"/chats/{chat['id']}/attachments/{rows[0]['id']}").json()
    assert got["status"] == "failed" and got["error_code"] == rows[0]["error_code"]
    assert_no_provider_ids(got)


def test_indexing_failure(api, chat, fresh_mock, db):
    fresh_mock.config(index_status="failed")
    r = api.upload(chat["id"], "idx.txt", b"data", "text/plain")
    assert_problem(r, 503, "service_unavailable")
    assert r.headers.get("Retry-After") == "10"
    row = db.one("SELECT * FROM attachments WHERE chat_id = ? AND filename = 'idx.txt'",
                 (uuid.UUID(chat["id"]).bytes,))
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    # Best-effort delete of the provider file.
    assert fresh_mock.wait_for("DELETE", rf"/files/{row['provider_file_id']}$", timeout=10)


@pytest.mark.slow
def test_indexing_in_progress_at_deadline(api, chat, fresh_mock):
    """Indexing still in_progress 25 s after the upload started -> 201 `uploaded`, then background
    indexing makes it `ready`."""
    fresh_mock.config(index_status="in_progress")
    started = time.time()
    r = api.upload(chat["id"], "slow.txt", b"slow data", "text/plain")
    elapsed = time.time() - started
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "uploaded", att
    assert 20 <= elapsed < 30, elapsed
    assert api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()["status"] == "uploaded"
    # Not usable until ready.
    res = api.stream(chat["id"], "use it", attachment_ids=[att["id"]])
    assert_problem(res.problem, 400, "invalid_argument", field="attachment", reason="invalid_attachment")
    fresh_mock.config(index_status="completed")
    wait_until(lambda: api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()["status"] == "ready",
               timeout=30, interval=1, desc="background indexing completes")
    api.turn(chat["id"], "use it now", attachment_ids=[att["id"]])


# ------------------------------------------------------------------ attachments in turns
def test_document_attachment_in_turn(api, chat, fresh_mock, db):
    att = upload_doc(api, chat["id"], name="report.txt")
    vid = vector_store_id(db, chat["id"])
    fresh_mock.reset()
    res = api.turn(chat["id"], "summarize [[file_search]]", attachment_ids=[att["id"]])
    body = fresh_mock.responses_calls()[-1]["body"]
    fs = [t for t in body["tools"] if t["type"] == "file_search"]
    assert len(fs) == 1 and fs[0]["vector_store_ids"] == [vid], body["tools"]
    assert body["metadata"]["feature"] == "file_search"
    tools = res.all("tool")
    assert {"phase": "start", "name": "file_search", "details": {}} in tools
    done_tools = [t for t in tools if t["phase"] == "done" and t["name"] == "file_search"]
    assert done_tools and done_tools[0]["details"] == {"files_searched": 0}
    cites = res.first("citations")
    assert cites, res.names
    item = cites["items"][0]
    assert item["source"] == "file" and item["attachment_id"] == att["id"]
    assert item["title"] == "report.txt" and item["snippet"] == "" and "span" not in item and "url" not in item
    assert_no_provider_ids(res.raw)
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert msgs[0]["attachments"] == [{"attachment_id": att["id"], "kind": "document",
                                       "filename": "report.txt", "status": "ready"}]
    assert msgs[1]["attachments"] == []

    # Retrieval covers the chat vector store even without attachment_ids on later turns.
    fresh_mock.reset()
    api.turn(chat["id"], "follow-up")
    body = fresh_mock.responses_calls()[-1]["body"]
    assert any(t["type"] == "file_search" for t in body["tools"])


def test_image_attachment_in_turn(api, chat, fresh_mock, db):
    r = api.upload(chat["id"], "pic.png", make_png(40, 40), "image/png")
    att = r.json()
    fid = provider_ids(db, att["id"])
    fresh_mock.reset()
    api.turn(chat["id"], "what is this", attachment_ids=[att["id"]])
    body = fresh_mock.responses_calls()[-1]["body"]
    content = body["input"][-1]["content"]
    assert isinstance(content, list), content
    assert {"type": "input_text", "text": "what is this"} in content
    assert {"type": "input_image", "file_id": fid} in [{k: p.get(k) for k in ("type", "file_id")}
                                                        for p in content if p.get("type") == "input_image"]
    assert not any(t["type"] == "file_search" for t in body.get("tools") or [])
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    summ = msgs[0]["attachments"][0]
    assert summ["attachment_id"] == att["id"] and summ["kind"] == "image" and summ["status"] == "ready"
    assert summ["img_thumbnail"]["content_type"] == "image/webp"
    # Images are not reused implicitly on the next turn.
    fresh_mock.reset()
    api.turn(chat["id"], "and now")
    content = fresh_mock.responses_calls()[-1]["body"]["input"][-1]["content"]
    assert isinstance(content, str) or not any(p.get("type") == "input_image" for p in content)


def test_image_on_text_only_model(api, fresh_mock):
    chat = api.create_chat(model="text-only")
    att = api.upload(chat["id"], "pic.png", make_png(10, 10), "image/png").json()
    fresh_mock.reset()
    res = api.stream(chat["id"], "see", attachment_ids=[att["id"]])
    assert_problem(res.problem, 400, "invalid_argument", reason="VISION_NOT_SUPPORTED")
    assert fresh_mock.responses_calls() == []
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 0


def test_invalid_attachment_ids(api, chat, fresh_mock):
    att = upload_doc(api, chat["id"])
    other_chat = api.create_chat()
    foreign = upload_doc(api, other_chat["id"])
    fresh_mock.reset()
    for ids in ([att["id"], att["id"]], [str(uuid.uuid4())], [foreign["id"]]):
        res = api.stream(chat["id"], "x", attachment_ids=ids)
        assert_problem(res.problem, 400, "invalid_argument", field="attachment", reason="invalid_attachment")
    assert fresh_mock.responses_calls() == []
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 0


# ------------------------------------------------------------------ deletion
def test_delete_attachment(api, chat, fresh_mock, db):
    att = upload_doc(api, chat["id"])
    fid = provider_ids(db, att["id"])
    url = f"/chats/{chat['id']}/attachments/{att['id']}"
    r = api.delete(url)
    assert r.status_code == 204 and r.content == b""
    assert api.delete(url).status_code == 204  # idempotent
    assert_problem(api.get(url), 404, "not_found", resource_type=ATTACHMENT_RT)
    assert fresh_mock.wait_for("DELETE", rf"/files/{fid}$", timeout=15), "provider file not deleted"
    wait_until(lambda: db.attachment(att["id"])["cleanup_status"] == "done", timeout=10, desc="cleanup done")
    assert len(fresh_mock.calls("DELETE", rf"/files/{fid}$")) == 1
    # A deleted attachment can no longer be referenced.
    res = api.stream(chat["id"], "x", attachment_ids=[att["id"]])
    assert_problem(res.problem, 400, "invalid_argument", reason="invalid_attachment")


def test_delete_referenced_attachment_locked(api, chat):
    att = upload_doc(api, chat["id"])
    api.turn(chat["id"], "with doc", attachment_ids=[att["id"]])
    r = api.delete(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert_problem(r, 409, "already_exists", resource_name="attachment_locked")
    assert api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()["status"] == "ready"


def test_delete_attachment_unknown(api, chat):
    r = api.delete(f"/chats/{chat['id']}/attachments/{uuid.uuid4()}")
    assert_problem(r, 404, "not_found", resource_type=ATTACHMENT_RT)


def test_retry_copies_attachments(api, chat, fresh_mock, db):
    att = upload_doc(api, chat["id"], name="keep.txt")
    first = api.turn(chat["id"], "with doc", attachment_ids=[att["id"]])
    from conftest import sse_request

    res = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{first.started['request_id']}/retry")
    assert res.terminal_name == "done", res.events
    msgs = api.get(f"/chats/{chat['id']}/messages").json()["items"]
    assert [a["attachment_id"] for a in msgs[0]["attachments"]] == [att["id"]]
    assert msgs[0]["request_id"] == res.started["request_id"]


# ------------------------------------------------------------------ chat deletion cleanup
def test_chat_delete_cleans_provider_resources(api, fresh_mock, db):
    chat = api.create_chat(title="cleanup")
    doc1 = upload_doc(api, chat["id"], name="a.txt")
    doc2 = upload_doc(api, chat["id"], name="b.txt")
    img = api.upload(chat["id"], "i.png", make_png(20, 20), "image/png").json()
    api.turn(chat["id"], "use", attachment_ids=[doc1["id"]])
    fids = [provider_ids(db, a["id"]) for a in (doc1, doc2, img)]
    vid = vector_store_id(db, chat["id"])
    assert all(fids) and vid
    fresh_mock.reset()

    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    for fid in fids:
        assert fresh_mock.wait_for("DELETE", rf"/files/{fid}$", timeout=20), f"file {fid} not deleted"
    assert fresh_mock.wait_for("DELETE", rf"/vector_stores/{vid}$", timeout=20), "vector store not deleted"
    reqs = [r for r in fresh_mock.requests() if r["method"] == "DELETE"]
    vs_index = max(i for i, r in enumerate(reqs) if r["path"].endswith(f"/vector_stores/{vid}"))
    file_idx = [i for i, r in enumerate(reqs) if "/files/" in r["path"]]
    assert max(file_idx) < vs_index, "vector store must be deleted after all files"

    def cleaned():
        rows = db.query("SELECT cleanup_status FROM attachments WHERE chat_id = ?", (uuid.UUID(chat["id"]).bytes,))
        return all(r["cleanup_status"] == "done" for r in rows) and vector_store_id(db, chat["id"]) is None

    wait_until(cleaned, timeout=15, desc="attachments cleanup_status=done and chat_vector_stores row removed")
    assert db.query("SELECT * FROM toolkit_outbox_dead_letters") == []


# ------------------------------------------------------------------ more upload rules
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def test_filename_default_and_truncation(api, chat):
    long_name = "n" * 300 + ".txt"
    att = upload_doc(api, chat["id"], name=long_name)
    assert len(att["filename"]) == 255 and att["filename"].endswith(".txt"), att["filename"]


def test_file_part_without_content_type(api, chat):
    r = api.post(f"/chats/{chat['id']}/attachments", files={"file": ("a.txt", b"abc")})
    # requests sends no Content-Type for a part without an explicit type.
    assert_problem(r, 400, "invalid_argument", field="content_type", reason="MISSING_CONTENT_TYPE")


def test_xlsx_is_code_interpreter_only(api, chat, fresh_mock, db):
    r = api.upload(chat["id"], "sheet.xlsx", b"PK\x03\x04fake-xlsx", XLSX)
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "document"
    row = db.attachment(att["id"])
    assert row["for_code_interpreter"] and not row["for_file_search"]
    assert fresh_mock.calls("POST", r"/vector_stores") == []
    fresh_mock.reset()
    api.turn(chat["id"], "analyze", attachment_ids=[att["id"]])
    body = fresh_mock.responses_calls()[-1]["body"]
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"]
    assert ci, body["tools"]
    assert row["provider_file_id"] in str(ci[0])
    assert not any(t["type"] == "file_search" for t in body["tools"])
    assert "code_interpreter_call.outputs" in (body.get("include") or [])


def test_too_many_images(api, chat, fresh_mock):
    ids = [api.upload(chat["id"], f"p{i}.png", make_png(4, 4), "image/png").json()["id"] for i in range(5)]
    fresh_mock.reset()
    res = api.stream(chat["id"], "five images", attachment_ids=ids)
    assert_problem(res.problem, 400, "out_of_range", field="image_count", reason="TOO_MANY_IMAGES")
    assert fresh_mock.responses_calls() == []
    api.turn(chat["id"], "four images", attachment_ids=ids[:4])
