"""Attachments: upload / get / delete, limits, indexing lifecycle, tool
availability, citations, images, cleanup and the upload reaper."""

import base64
import struct
import time
import uuid
import zlib

import mc
from mc import problem_reason

XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def make_png(w: int = 16, h: int = 8) -> bytes:
    def chunk(t: bytes, d: bytes) -> bytes:
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + bytes([(x * 13) % 256, (y * 29) % 256, 120] * 1)[:3] * 1 + b"" for y in range(h) for x in range(1)) if False else b""
    rows = []
    for y in range(h):
        row = b"\x00" + b"".join(bytes([(x * 13) % 256, (y * 29) % 256, 120]) for x in range(w))
        rows.append(row)
    raw = b"".join(rows)
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def test_document_upload_ready_and_provider_calls(api, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    r = api.upload(cid, "report.md", b"# Report\nhello world", "text/markdown")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "ready"
    assert a["kind"] == "document"
    assert a["filename"] == "report.md"
    assert a["content_type"] == "text/markdown"
    assert a["size_bytes"] == len(b"# Report\nhello world")
    for k in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert k not in a
    assert set(a) == {"id", "filename", "content_type", "size_bytes", "status", "kind", "created_at"}
    reqs = mock.since(mark)
    upload = [x for x in reqs if x["path"] == "/v1/files" and x["method"] == "POST"][0]
    assert upload["body"]["fields"]["purpose"] == "assistants"
    assert upload["body"]["filename"] == f"{cid}_{a['id']}.md"
    assert [x for x in reqs if x["path"] == "/v1/vector_stores" and x["method"] == "POST"]
    add = [x for x in reqs if x["path"].endswith("/files") and "/vector_stores/" in x["path"]][0]
    assert add["body"]["attributes"] == {"attachment_id": a["id"]}
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(a["id"]))[0]
    assert row["for_file_search"] == 1 and row["for_code_interpreter"] == 0
    assert row["provider_file_id"].startswith("file-")
    vs = db.q("select * from chat_vector_stores where chat_id = ?", mc.uuid_blob(cid))
    assert len(vs) == 1 and vs[0]["vector_store_id"].startswith("vs_") and vs[0]["provider"] == "mock"
    # GET returns the same projection; no provider ids leak.
    g = api.get(f"/chats/{cid}/attachments/{a['id']}")
    assert g.status_code == 200 and g.json() == a
    assert "file-" not in g.text and "vs_" not in g.text

    # A second document reuses the chat vector store.
    mark = mock.mark()
    assert api.upload(cid, "two.txt", b"second").status_code == 201
    assert not [x for x in mock.since(mark) if x["path"] == "/v1/vector_stores" and x["method"] == "POST"]


def test_file_search_tool_and_citations(api, mock):
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    s = api.send(cid, "no docs yet")
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    assert not any(t["type"] == "file_search" for t in body.get("tools", []))
    a = api.upload(cid, "Q3 Report.pdf", b"%PDF-1.4 fake", "application/pdf").json()
    mark = mock.mark()
    s = api.send(cid, "what does it say #filecite")
    assert s.terminal[0] == "done"
    names = s.names()
    assert names.index("citations") == len(names) - 2
    tools = s.all("tool")
    assert tools[0] == {"phase": "start", "name": "file_search", "details": {}}
    assert tools[1] == {"phase": "done", "name": "file_search", "details": {"files_searched": 0}}
    items = s.first("citations")["items"]
    assert items == [{"source": "file", "title": "Q3 Report.pdf", "attachment_id": a["id"], "snippet": ""}]
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    fs = [t for t in body["tools"] if t["type"] == "file_search"][0]
    assert len(fs["vector_store_ids"]) == 1 and fs["max_num_results"] == 5
    assert body["max_tool_calls"] == 2
    assert "file_search" in body["metadata"]["feature"]
    assert "file_search" in body["instructions"]  # tool guard appended


def test_image_upload_thumbnail_and_multimodal_input(api, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    png = make_png(64, 32)
    mark = mock.mark()
    r = api.upload(cid, "pic.png", png, "image/png")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["kind"] == "image" and a["status"] == "ready"
    th = a["img_thumbnail"]
    assert th["content_type"] == "image/webp"
    assert th["width"] <= 128 and th["height"] <= 128
    assert base64.b64decode(th["data_base64"])[:4] == b"RIFF"
    # Images are never added to the vector store.
    assert not [x for x in mock.since(mark) if "/vector_stores" in x["path"]]
    s = api.send(cid, "describe", attachment_ids=[a["id"]])
    assert s.terminal[0] == "done"
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    content = body["input"][-1]["content"]
    assert isinstance(content, list)
    assert {"type": "input_text", "text": "describe"} in content
    img = [c for c in content if c["type"] == "input_image"]
    assert len(img) == 1 and img[0]["file_id"].startswith("file-")
    msgs = api.messages(cid)["items"]
    summ = msgs[0]["attachments"][0]
    assert summ["attachment_id"] == a["id"] and summ["kind"] == "image" and summ["status"] == "ready"
    assert summ["img_thumbnail"]["content_type"] == "image/webp"
    # The image is not reused implicitly on later turns.
    mark = mock.mark()
    api.send(cid, "again")
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    assert "input_image" not in str(body["input"][-1])


def test_image_guards(api):
    chat = api.create_chat(model="gpt-standard")
    a = api.upload(chat["id"], "p.png", make_png(), "image/png").json()
    s = api.send(chat["id"], "look", attachment_ids=[a["id"]])
    assert s.status == 400 and problem_reason(s.body) == "VISION_NOT_SUPPORTED"
    chat2 = api.create_chat()
    ids = [api.upload(chat2["id"], f"p{i}.png", make_png(), "image/png").json()["id"] for i in range(3)]
    s = api.send(chat2["id"], "look", attachment_ids=ids)
    assert s.status == 400 and problem_reason(s.body) == "TOO_MANY_IMAGES"


def test_upload_validation_and_limits(api, db):
    chat = api.create_chat()
    cid = chat["id"]
    r = api.upload(cid, "x.exe", b"MZ", "application/x-msdownload")
    assert r.status_code == 400 and problem_reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    # octet-stream is inferred from the extension.
    r = api.upload(cid, "inferred.txt", b"abc", "application/octet-stream")
    assert r.status_code == 201 and r.json()["content_type"] == "text/plain"
    r = api.upload(cid, "unknown.bin", b"abc", "application/octet-stream")
    assert r.status_code == 400 and problem_reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    # Size limits (documents 512 KiB, images 256 KiB in the test config).
    r = api.upload(cid, "big.txt", b"a" * (512 * 1024 + 1))
    assert r.status_code == 400 and problem_reason(r.json()) == "FILE_TOO_LARGE"
    r = api.upload(cid, "big.png", make_png() + b"\x00" * (256 * 1024), "image/png")
    assert r.status_code == 400 and problem_reason(r.json()) == "FILE_TOO_LARGE"
    # Multipart errors.
    r = api.post(f"/chats/{cid}/attachments", content=b"x", headers={"content-type": "multipart/form-data"})
    assert r.status_code == 400 and problem_reason(r.json()) == "BOUNDARY_REQUIRED"
    r = api.post(f"/chats/{cid}/attachments", files={"other": ("a.txt", b"x", "text/plain")})
    assert r.status_code == 400 and problem_reason(r.json()) == "MISSING_FILE"
    # Filename defaults / truncation.
    long_name = "n" * 300 + ".txt"
    r = api.upload(cid, long_name, b"abc")
    assert r.status_code == 201 and len(r.json()["filename"]) == 255 and r.json()["filename"].endswith(".txt")
    # Code-interpreter-only file on a model without code interpreter.
    std = api.create_chat(model="gpt-standard")
    r = api.upload(std["id"], "s.xlsx", b"PK\x03\x04", XLSX)
    assert r.status_code == 400 and problem_reason(r.json()) == "CODE_INTERPRETER_UNAVAILABLE"
    # Unknown chat.
    r = api.upload(str(uuid.uuid4()), "a.txt", b"x")
    assert r.status_code == 404 and r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"


def test_per_chat_limits(api):
    chat = api.create_chat()
    cid = chat["id"]
    for i in range(3):
        assert api.upload(cid, f"d{i}.txt", b"doc").status_code == 201
    r = api.upload(cid, "d4.txt", b"doc")
    assert r.status_code == 429
    assert r.json()["context"]["violations"][0]["subject"] == "document_limit"
    chat2 = api.create_chat()
    assert api.upload(chat2["id"], "a.txt", b"a" * 500 * 1024).status_code == 201
    assert api.upload(chat2["id"], "b.txt", b"b" * 500 * 1024).status_code == 201
    r = api.upload(chat2["id"], "c.txt", b"c" * 100 * 1024)
    assert r.status_code == 429
    assert r.json()["context"]["violations"][0]["subject"] == "storage_limit"
    # Images count toward the storage limit too.
    r = api.upload(chat2["id"], "c.png", make_png() + b"\x00" * (100 * 1024), "image/png")
    assert r.status_code == 429
    assert r.json()["context"]["violations"][0]["subject"] == "storage_limit"


def test_indexing_failure_and_slow_indexing(api, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    r = api.upload(cid, "bad.txt", b"INDEX_FAIL please")
    assert r.status_code == 503
    assert r.headers.get("retry-after") == "10"
    assert "indexing_failed" not in r.text
    rows = db.q("select * from attachments where chat_id = ?", mc.uuid_blob(cid))
    assert len(rows) == 1 and rows[0]["status"] == "failed" and rows[0]["error_code"] == "indexing_failed"
    att_id = mc.blob_uuid(rows[0]["id"])
    g = api.get(f"/chats/{cid}/attachments/{att_id}").json()
    assert g["status"] == "failed" and g["error_code"] == "indexing_failed"
    # The provider file is deleted (best effort).
    mc.wait_for(lambda: [x for x in mock.since(mark) if x["method"] == "DELETE" and x["path"].startswith("/v1/files/")])
    # Indexing that finishes after a couple of polls ends ready.
    r = api.upload(cid, "late.txt", b"INDEX_LATE content")
    assert r.status_code == 201 and r.json()["status"] == "ready"
    # A document still in progress at the deadline is returned as uploaded.
    r = api.upload(cid, "slow.txt", b"INDEX_SLOW content")
    assert r.status_code == 201, r.text
    assert r.json()["status"] == "uploaded"
    s = api.send(cid, "use it", attachment_ids=[r.json()["id"]])
    assert s.status == 400 and problem_reason(s.body) == "invalid_attachment"


def test_provider_upload_failure(api, db):
    chat = api.create_chat()
    r = api.upload(chat["id"], "x.txt", b"UPLOAD_FAIL now")
    assert r.status_code == 503 and r.headers.get("retry-after") == "10"
    row = db.q("select * from attachments where chat_id = ?", mc.uuid_blob(chat["id"]))[0]
    assert row["status"] == "failed" and row["error_code"] == "upload_failed"


def test_attachment_visibility_and_delete(api, api_a2, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    a = api.upload(cid, "free.txt", b"free").json()
    b = api.upload(cid, "used.txt", b"used").json()
    assert api.send(cid, "with b", attachment_ids=[b["id"]]).terminal[0] == "done"
    # Another user cannot see or delete it.
    assert api_a2.get(f"/chats/{cid}/attachments/{a['id']}").status_code == 404
    assert api_a2.delete(f"/chats/{cid}/attachments/{a['id']}").status_code == 404
    # Wrong chat -> 404 attachment type.
    other = api.create_chat()
    r = api.get(f"/chats/{other['id']}/attachments/{a['id']}")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.attachment.v1~"
    # Referenced attachment is locked.
    r = api.delete(f"/chats/{cid}/attachments/{b['id']}")
    assert r.status_code == 409 and r.json()["context"]["resource_name"] == "attachment_locked"
    mark = mock.mark()
    assert api.delete(f"/chats/{cid}/attachments/{a['id']}").status_code == 204
    assert api.get(f"/chats/{cid}/attachments/{a['id']}").status_code == 404
    assert api.delete(f"/chats/{cid}/attachments/{a['id']}").status_code == 204  # idempotent
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(a["id"]))[0]
    assert row["deleted_at"] is not None
    mc.wait_for(lambda: db.q("select cleanup_status from attachments where id = ?", mc.uuid_blob(a["id"]))[0]["cleanup_status"] == "done")
    deletes = [x for x in mock.since(mark) if x["method"] == "DELETE" and x["path"].startswith("/v1/files/")]
    assert len(deletes) == 1 and deletes[0]["path"] == f"/v1/files/{row['provider_file_id']}"
    # Deleted attachments cannot be referenced.
    s = api.send(cid, "x", attachment_ids=[a["id"]])
    assert s.status == 400 and problem_reason(s.body) == "invalid_attachment"
    # Other users' attachments cannot be referenced either.
    chat_b = api_a2.create_chat()
    foreign = api_a2.upload(chat_b["id"], "f.txt", b"f").json()
    s = api.send(cid, "x", attachment_ids=[foreign["id"]])
    assert s.status == 400 and problem_reason(s.body) == "invalid_attachment"


def test_chat_delete_cleans_provider_resources(api, mock, db):
    chat = api.create_chat()
    cid = chat["id"]
    a = api.upload(cid, "doc.txt", b"to be removed").json()
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(a["id"]))[0]
    vs = db.q("select * from chat_vector_stores where chat_id = ?", mc.uuid_blob(cid))[0]["vector_store_id"]
    mark = mock.mark()
    assert api.delete(f"/chats/{cid}").status_code == 204
    mc.wait_for(lambda: not db.q("select * from chat_vector_stores where chat_id = ?", mc.uuid_blob(cid)), timeout=30)
    reqs = mock.since(mark)
    assert [x for x in reqs if x["method"] == "DELETE" and x["path"] == f"/v1/files/{row['provider_file_id']}"]
    assert [x for x in reqs if x["method"] == "DELETE" and x["path"] == f"/v1/vector_stores/{vs}"]
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(a["id"]))[0]
    assert row["cleanup_status"] == "done"
    assert vs not in mock.state()["vector_stores"]


def test_upload_reaper(api, db, mock):
    chat = api.create_chat()
    cid = chat["id"]
    r = api.upload(cid, "stale.txt", b"INDEX_SLOW stale")
    assert r.json()["status"] == "uploaded"
    att = r.json()["id"]
    row = db.q("select * from attachments where id = ?", mc.uuid_blob(att))[0]
    mark = mock.mark()
    # Simulate an abandoned upload (no heartbeat for a long time).
    for _ in range(3):
        db.x("update attachments set updated_at = '2020-01-01T00:00:00Z' where id = ?", mc.uuid_blob(att))
        time.sleep(0.2)
    def reaped():
        db.x(
            "update attachments set updated_at = '2020-01-01T00:00:00Z' where id = ? and status = 'uploaded'",
            mc.uuid_blob(att),
        )
        return db.q("select status from attachments where id = ?", mc.uuid_blob(att))[0]["status"] == "failed"
    mc.wait_for(reaped, timeout=30)
    g = api.get(f"/chats/{cid}/attachments/{att}").json()
    assert g["status"] == "failed" and g["error_code"] == "upload_abandoned"
    mc.wait_for(lambda: [x for x in mock.since(mark) if x["method"] == "DELETE" and x["path"] == f"/v1/files/{row['provider_file_id']}"], timeout=30)


def test_code_interpreter_file_and_tool(api, mock):
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    r = api.upload(cid, "sheet.xlsx", b"PK\x03\x04fake", XLSX)
    assert r.status_code == 201 and r.json()["status"] == "ready"
    assert not [x for x in mock.since(mark) if "/vector_stores" in x["path"]]
    s = api.send(cid, "compute #ci")
    assert s.terminal[0] == "done"
    tools = s.all("tool")
    assert {"phase": "start", "name": "code_interpreter", "details": {}} in tools
    assert {"phase": "done", "name": "code_interpreter", "details": {"output": "42"}} in tools
    body = [x for x in mock.since(mark) if x["path"].endswith("/responses")][0]["body"]
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"][0]
    assert ci["container"]["file_ids"][0].startswith("file-")
    assert "code_interpreter_call.outputs" in body.get("include", [])
    # Mid-turn limit.
    s = api.send(cid, "too many #ci11")
    assert s.terminal == ("error", s.terminal[1]) and s.terminal[1]["code"] == "code_interpreter_calls_exceeded"
