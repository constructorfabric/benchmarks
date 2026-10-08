"""US4 black-box: document / image attachments against the mock Files + Vector Stores API."""

import base64
import struct
import zlib

from conftest import field_reasons, reason, ub, us, wait_until

MINI_CHAT_PATCH = {
    "rag": {
        "uploaded_file_max_size_kb": 64,
        "uploaded_image_max_size_kb": 64,
        "max_documents_per_chat": 3,
        "max_images_per_message": 2,
    }
}

PROVIDER_ID_MARKERS = ("file-", "vs_")


def png(w=64, h=48, rgb=(200, 30, 30)):
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def _no_provider_ids(text):
    for m in PROVIDER_ID_MARKERS:
        assert m not in text, f"provider identifier leaked: {m} in {text[:300]}"


# ------------------------------------------------------------------ documents
def test_document_upload_creates_file_and_vector_store(api, mock, db):
    c = api.create_chat()  # gpt-4.1: file_search supported
    data = b"The secret launch code is 1234.\n" * 10
    r = api.upload(c["id"], "notes.txt", data, "text/plain")
    assert r.status_code == 201, r.text
    _no_provider_ids(r.text)
    att = r.json()
    assert att["status"] == "ready"
    assert att["kind"] == "document"
    assert att["filename"] == "notes.txt"
    assert att["content_type"] == "text/plain"
    assert att["size_bytes"] == len(data)
    for absent in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert absent not in att

    # provider calls: file upload (multipart, purpose=assistants), vector store, link
    up = mock.calls("POST", "/v1/files")
    assert len(up) == 1
    assert up[0]["headers"]["content-type"].startswith("multipart/form-data")
    assert b'name="purpose"' in up[0]["body"] and b"assistants" in up[0]["body"]
    assert data in up[0]["body"]
    assert len(mock.calls("POST", "/v1/vector_stores")) >= 1
    vs = mock.vector_stores[0]
    link = mock.calls("POST", f"/v1/vector_stores/{vs}/files")
    assert len(link) == 1
    assert link[0]["json"] == {"file_id": mock.files[0], "attributes": {"attachment_id": att["id"]}}

    row = db.one("SELECT * FROM attachments WHERE id = ?", ub(att["id"]))
    assert row["status"] == "ready"
    assert row["provider_file_id"] == mock.files[0]
    assert row["attachment_kind"] == "document"
    assert row["for_file_search"] == 1
    assert row["img_thumbnail"] is None
    store = db.one("SELECT * FROM chat_vector_stores WHERE chat_id = ?", ub(c["id"]))
    assert store["vector_store_id"] == vs
    assert store["file_count"] == 0  # reserved column (DESIGN: never updated)

    assert api.get(f"/chats/{c['id']}/attachments/{att['id']}").json() == att

    # a second document reuses the chat's vector store
    r2 = api.upload(c["id"], "more.md", b"# more\n", "text/markdown")
    assert r2.status_code == 201, r2.text
    assert len(mock.vector_stores) == 1
    assert len(mock.calls("POST", f"/v1/vector_stores/{vs}/files")) == 2


def test_document_enables_file_search_and_file_citations(api, mock, db):
    c = api.create_chat()
    doc = api.upload(c["id"], "facts.txt", b"Mars has two moons." * 5, "text/plain").json()
    fid, vs = mock.files[0], mock.vector_stores[0]
    ann = {"type": "file_citation", "file_id": fid, "filename": "facts.txt", "index": 0}
    mock.push({
        "text": "Mars has two moons.",
        "pre_events": [
            ["response.file_search_call.searching", {"type": "response.file_search_call.searching", "item_id": "fs_1"}],
            ["response.file_search_call.completed", {"type": "response.file_search_call.completed", "item_id": "fs_1"}],
        ],
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "Mars has two moons.", "annotations": [ann]}]}],
    })
    res = api.send(c["id"], "How many moons does Mars have?", attachment_ids=[doc["id"]])
    assert res.status == 200 and res.names[-1] == "done", res
    _no_provider_ids(res.text)

    body = mock.chat_requests()[0]["json"]
    assert {"type": "file_search", "vector_store_ids": [vs], "max_num_results": 5} in body["tools"]
    assert body["metadata"]["feature"] == "file_search"
    assert "file_search" in body["instructions"]
    assert body["max_tool_calls"] == 10

    tools = res.all("tool")
    assert [t["phase"] for t in tools] == ["start", "done"] and tools[0]["name"] == "file_search"
    cit = res.first("citations")
    assert cit and cit["items"][0]["source"] == "file"
    assert cit["items"][0]["attachment_id"] == doc["id"]

    msgs = api.messages(c["id"])
    assert msgs[0]["attachments"] == [
        {"attachment_id": doc["id"], "kind": "document", "filename": "facts.txt", "status": "ready"}
    ]
    turn = db.turn(c["id"], res.request_id)
    assert turn["file_search_completed_count"] == 1


def test_documents_in_chat_enable_file_search_without_explicit_ids(api, mock):
    c = api.create_chat()
    api.upload(c["id"], "a.txt", b"alpha", "text/plain")
    api.send(c["id"], "anything in my files?")
    body = mock.chat_requests()[0]["json"]
    assert any(t["type"] == "file_search" for t in body.get("tools", []))


def test_model_without_file_search_gets_no_tool(api, mock):
    c = api.create_chat(model="gpt-4.1-mini")  # tool_support.file_search = false
    doc = api.upload(c["id"], "a.txt", b"alpha", "text/plain")
    assert doc.status_code == 201, doc.text
    api.send(c["id"], "hi", attachment_ids=[doc.json()["id"]])
    body = mock.chat_requests()[0]["json"]
    assert not any(t["type"] == "file_search" for t in body.get("tools", []))


# ------------------------------------------------------------------ images
def test_image_upload_thumbnail_and_multimodal_input(api, mock, db):
    c = api.create_chat()
    img_bytes = png(64, 48)
    r = api.upload(c["id"], "pic.png", img_bytes, "image/png")
    assert r.status_code == 201, r.text
    _no_provider_ids(r.text)
    att = r.json()
    assert att["kind"] == "image" and att["status"] == "ready"
    assert att["content_type"] == "image/png" and att["size_bytes"] == len(img_bytes)
    th = att["img_thumbnail"]
    assert th["content_type"] == "image/webp"
    assert 0 < th["width"] <= 128 and 0 < th["height"] <= 128
    assert abs(th["width"] / th["height"] - 64 / 48) < 0.05  # aspect ratio preserved
    raw = base64.b64decode(th["data_base64"])
    assert raw[:4] == b"RIFF" and raw[8:12] == b"WEBP"
    # images are not added to a vector store
    assert mock.calls("POST", "/v1/vector_stores") == []
    assert len(mock.files) == 1
    row = db.one("SELECT * FROM attachments WHERE id = ?", ub(att["id"]))
    assert row["attachment_kind"] == "image"
    assert bytes(row["img_thumbnail"]) == raw
    assert (row["img_thumbnail_width"], row["img_thumbnail_height"]) == (th["width"], th["height"])

    res = api.send(c["id"], "what is in the picture?", attachment_ids=[att["id"]])
    assert res.names[-1] == "done"
    body = mock.chat_requests()[0]["json"]
    assert body["input"][-1] == {
        "role": "user",
        "content": [
            {"type": "input_text", "text": "what is in the picture?"},
            {"type": "input_image", "file_id": mock.files[0]},
        ],
    }
    assert "tools" not in body or not any(t["type"] == "file_search" for t in body["tools"])
    summary = api.messages(c["id"])[0]["attachments"][0]
    assert summary["attachment_id"] == att["id"] and summary["kind"] == "image"
    assert summary["img_thumbnail"] == th

    # images of earlier turns are not implicitly re-sent
    api.send(c["id"], "and now?")
    last = mock.chat_requests()[-1]["json"]["input"]
    assert last[-1] == {"role": "user", "content": "and now?"}
    assert all(isinstance(m["content"], str) for m in last)


def test_octet_stream_image_type_inferred_from_extension(api):
    c = api.create_chat()
    r = api.upload(c["id"], "photo.png", png(8, 8), "application/octet-stream")
    assert r.status_code == 201, r.text
    assert r.json()["content_type"] == "image/png" and r.json()["kind"] == "image"


def test_too_many_images_per_message(api, mock):
    c = api.create_chat()
    ids = [api.upload(c["id"], f"p{i}.png", png(4, 4), "image/png").json()["id"] for i in range(3)]
    r = api.send(c["id"], "three images", attachment_ids=ids)
    assert r.status == 400, r
    assert field_reasons(r.problem) == ["TOO_MANY_IMAGES"]
    assert mock.chat_requests() == []


# ------------------------------------------------------------------ validation
def test_upload_validation(api, mock):
    c = api.create_chat()
    r = api.upload(c["id"], "x.exe", b"MZ", "application/x-msdownload")
    assert r.status_code == 400 and field_reasons(r.json()) == ["UNSUPPORTED_CONTENT_TYPE"]
    r = api.upload(c["id"], "big.txt", b"a" * (65 * 1024), "text/plain")
    assert r.status_code == 400, r.text
    assert field_reasons(r.json()) == ["FILE_TOO_LARGE"]
    assert r.json()["type"].endswith("cf.core.err.out_of_range.v1~")
    r = api.upload(c["id"], "big.png", png(4, 4) + b"\0" * (65 * 1024), "image/png")
    assert r.status_code == 400 and field_reasons(r.json()) == ["FILE_TOO_LARGE"]
    r = api.post(f"/chats/{c['id']}/attachments", files={"other": ("a.txt", b"x", "text/plain")})
    assert r.status_code == 400 and field_reasons(r.json()) == ["MISSING_FILE"]
    assert mock.files == []


def test_per_chat_document_limit(api, mock):
    c = api.create_chat()
    for i in range(3):
        assert api.upload(c["id"], f"d{i}.txt", b"doc", "text/plain").status_code == 201
    r = api.upload(c["id"], "d4.txt", b"doc", "text/plain")
    assert r.status_code == 429, r.text
    v = r.json()["context"]["violations"][0]
    assert v["subject"] == "document_limit"
    # images do not count against the document limit
    assert api.upload(c["id"], "p.png", png(4, 4), "image/png").status_code == 201


def test_provider_upload_failure(api, mock, db):
    c = api.create_chat()
    mock.file_upload_status = 500
    r = api.upload(c["id"], "a.txt", b"x", "text/plain")
    assert r.status_code == 503, r.text
    assert r.headers["retry-after"] == "10"
    assert r.json()["type"].endswith("cf.core.err.service_unavailable.v1~")
    _no_provider_ids(r.text)
    row = db.one("SELECT * FROM attachments WHERE chat_id = ?", ub(c["id"]))
    assert row["status"] == "failed" and row["error_code"] == "upload_failed"
    assert row["provider_file_id"] is None


def test_vector_store_indexing_failure(api, mock, db):
    c = api.create_chat()
    mock.vs_status = "failed"
    r = api.upload(c["id"], "a.txt", b"x", "text/plain")
    assert r.status_code == 503, r.text
    assert r.headers["retry-after"] == "10"
    _no_provider_ids(r.text)
    row = db.one("SELECT * FROM attachments WHERE chat_id = ?", ub(c["id"]))
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    # the row stays visible with the stable error code
    got = api.get(f"/chats/{c['id']}/attachments/{us(row['id'])}").json()
    assert got["status"] == "failed" and got["error_code"] == "indexing_failed"
    assert "img_thumbnail" not in got
    # the failed document's provider file is handed to the outbox cleanup
    fid = mock.files[0]
    wait_until(lambda: mock.calls("DELETE", f"/files/{fid}"), timeout=20, desc="provider file cleanup")
    # a failed attachment cannot be referenced by a message
    r = api.send(c["id"], "use it", attachment_ids=[us(row["id"])])
    assert r.status == 400, r
    assert mock.chat_requests() == []


def test_send_rejects_foreign_or_unknown_attachments(api, api_a2, mock):
    c1, c2 = api.create_chat(), api.create_chat()
    other_chat_att = api.upload(c2["id"], "a.txt", b"x", "text/plain").json()["id"]
    r = api.send(c1["id"], "hi", attachment_ids=[other_chat_att])
    assert r.status in (400, 404), r
    r = api.send(c1["id"], "hi", attachment_ids=[other_chat_att, other_chat_att])
    assert r.status in (400, 404), r
    assert mock.chat_requests() == []
    # another user cannot see the attachment
    assert api_a2.get(f"/chats/{c2['id']}/attachments/{other_chat_att}").status_code == 404
    assert api.get(f"/chats/{c1['id']}/attachments/{other_chat_att}").status_code == 404


# ------------------------------------------------------------------ deletion
def test_delete_unreferenced_attachment(api, api_a2, mock, db):
    c = api.create_chat()
    att = api.upload(c["id"], "a.txt", b"delete me", "text/plain").json()
    fid = mock.files[0]
    assert api_a2.delete(f"/chats/{c['id']}/attachments/{att['id']}").status_code == 404
    r = api.delete(f"/chats/{c['id']}/attachments/{att['id']}")
    assert r.status_code == 204
    r = api.get(f"/chats/{c['id']}/attachments/{att['id']}")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.attachment.v1~"
    assert db.one("SELECT deleted_at FROM attachments WHERE id = ?", ub(att["id"]))["deleted_at"] is not None
    # provider cleanup happens asynchronously through the outbox
    wait_until(lambda: mock.calls("DELETE", f"/files/{fid}"), timeout=20, desc="provider file delete")
    wait_until(
        lambda: db.one("SELECT cleanup_status FROM attachments WHERE id = ?", ub(att["id"]))["cleanup_status"] == "done",
        timeout=20,
        desc="cleanup_status done",
    )
    # repeated delete is not an error for the owner
    assert api.delete(f"/chats/{c['id']}/attachments/{att['id']}").status_code in (204, 404)
    # deleted documents no longer trigger file_search
    api.send(c["id"], "anything?")
    body = mock.chat_requests()[-1]["json"]
    assert not any(t["type"] == "file_search" for t in body.get("tools", []))


def test_delete_referenced_attachment_is_locked(api, mock, db):
    c = api.create_chat()
    doc = api.upload(c["id"], "a.txt", b"referenced", "text/plain").json()
    img = api.upload(c["id"], "p.png", png(4, 4), "image/png").json()
    api.send(c["id"], "use them", attachment_ids=[doc["id"], img["id"]])
    for att in (doc, img):
        r = api.delete(f"/chats/{c['id']}/attachments/{att['id']}")
        assert r.status_code == 409, r.text
        p = r.json()
        assert p["type"].endswith("cf.core.err.already_exists.v1~")
        assert p["context"]["resource_name"] == "attachment_locked"
        assert db.one("SELECT deleted_at FROM attachments WHERE id = ?", ub(att["id"]))["deleted_at"] is None
    assert reason(p) is None
    assert us(db.rows("SELECT message_id FROM message_attachments WHERE attachment_id = ?", ub(doc["id"]))[0]["message_id"])
