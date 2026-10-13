"""Attachments: upload/get/delete, indexing, provider tools, citations, cleanup."""

from __future__ import annotations

import base64
import io
import struct
import uuid
import zipfile
import zlib

import pytest

from .conftest import RT_ATTACHMENT, RT_CHAT, assert_problem, field_reasons, outbox_payloads, ub, wait_until
from .mock_llm import json_response, text_stream

XLSX_MIME = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def png_bytes(w: int = 64, h: int = 48) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    raw = b"".join(
        b"\x00" + bytes(v for x in range(w) for v in ((x * 4) % 256, (y * 5) % 256, 128)) for y in range(h)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def xlsx_bytes() -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("[Content_Types].xml", "<Types/>")
        z.writestr("xl/workbook.xml", "<workbook/>")
    return buf.getvalue()


def provider_file_id(db, attachment_id: str) -> str:
    return db.execute("SELECT provider_file_id FROM attachments WHERE id = ?", (ub(attachment_id),)).fetchone()[0]


def chat_vector_store(db, chat_id: str):
    row = db.execute("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(chat_id),)).fetchone()
    return row[0] if row else None


@pytest.mark.smoke
def test_upload_document_ready_and_indexed(api, mock_llm, db, provider):
    a = api("A")
    chat = a.create_chat(model=provider["model"])
    content = f"The launch date is March 3. ({chat['id']})".encode()
    r = a.upload(chat["id"], "notes.txt", content, "text/plain")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready"
    assert att["kind"] == "document"
    assert att["filename"] == "notes.txt"
    assert att["content_type"] == "text/plain"
    assert att["size_bytes"] == len(content)
    for absent in ("error_code", "img_thumbnail", "doc_summary", "summary_updated_at", "provider_file_id"):
        assert absent not in att
    assert "file-mock" not in r.text and "vs_mock" not in r.text

    got = a.get(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert got.status_code == 200 and got.json() == att

    fid = provider_file_id(db, att["id"])
    vs = chat_vector_store(db, chat["id"])
    assert fid and vs
    upload = [x for x in mock_llm.find("POST", "/v1/files") if x.multipart.get("file", {}).get("data") == content]
    assert len(upload) == 1
    assert upload[0].multipart["purpose"]["data"] == b"assistants"
    assert upload[0].listener == provider["name"]
    assert upload[0].raw_path == provider["storage_prefix"] + "/files"
    assert upload[0].query == provider["query"]
    add = mock_llm.find("POST", f"/v1/vector_stores/{vs}/files")
    assert add and add[-1].json["file_id"] == fid
    assert add[-1].raw_path == f"{provider['storage_prefix']}/vector_stores/{vs}/files"
    assert add[-1].query == provider["query"]
    assert add[-1].json["attributes"] == {"attachment_id": att["id"]}

    # file_search is offered on the next turn, over the chat vector store
    a.send(chat["id"], "When is the launch?")
    body = mock_llm.chat_requests(chat["id"])[-1].json
    fs = [t for t in body["tools"] if t["type"] == "file_search"]
    assert fs == [{"type": "file_search", "vector_store_ids": [vs], "max_num_results": 5}]
    assert body["metadata"]["feature"] == "file_search"


def test_upload_image_has_thumbnail_and_is_sent_as_input_image(api, mock_llm, db, provider):
    a = api("A")
    chat = a.create_chat(model=provider["model"])
    data = png_bytes()
    r = a.upload(chat["id"], "photo.png", data, "image/png")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "image"
    thumb = att["img_thumbnail"]
    assert set(thumb) >= {"content_type", "width", "height", "data_base64"}
    assert thumb["width"] <= 128 and thumb["height"] <= 128
    assert len(base64.b64decode(thumb["data_base64"])) <= 131072
    # images are never added to the vector store
    assert chat_vector_store(db, chat["id"]) is None

    res = a.send(chat["id"], "What is in the picture?", attachment_ids=[att["id"]])
    body = mock_llm.chat_requests(chat["id"])[-1].json
    user_input = body["input"][-1]
    assert user_input["role"] == "user"
    assert {"type": "input_text", "text": "What is in the picture?"} in user_input["content"]
    assert {"type": "input_image", "file_id": provider_file_id(db, att["id"])} in user_input["content"]

    msgs = a.messages(chat["id"])
    user_msg = [m for m in msgs if m["role"] == "user"][0]
    assert user_msg["request_id"] == res.request_id
    assert len(user_msg["attachments"]) == 1
    summary = user_msg["attachments"][0]
    assert summary["attachment_id"] == att["id"]
    assert summary["kind"] == "image" and summary["filename"] == "photo.png" and summary["status"] == "ready"
    assert summary["img_thumbnail"]["data_base64"]
    assert [m for m in msgs if m["role"] == "assistant"][0]["attachments"] == []

    # images are not implicitly reused on later turns
    a.send(chat["id"], "and now?")
    later = mock_llm.chat_requests(chat["id"])[-1].json["input"][-1]
    assert "input_image" not in str(later)


def test_image_rejected_on_model_without_vision(api, mock_llm):
    a = api("A")
    chat = a.create_chat(model="gpt-text-only")
    att = a.upload(chat["id"], "p.png", png_bytes(), "image/png")
    assert att.status_code == 201, att.text
    r = a.post(f"/chats/{chat['id']}/messages:stream", json={"content": "see", "attachment_ids": [att.json()["id"]]})
    p = assert_problem(r, 400)
    assert "VISION_NOT_SUPPORTED" in str(p["context"]), p
    assert mock_llm.chat_requests(chat["id"]) == []


def test_code_interpreter_tool_for_xlsx(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    r = a.upload(chat["id"], "data.xlsx", xlsx_bytes(), XLSX_MIME)
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "document"
    a.send(chat["id"], "Sum column A")
    body = mock_llm.chat_requests(chat["id"])[-1].json
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"]
    assert ci == [{"type": "code_interpreter", "container": {"type": "auto", "file_ids": [provider_file_id(db, att["id"])]}}]
    assert body["include"] == ["code_interpreter_call.outputs"]
    assert "code_interpreter" in body["metadata"]["feature"]

    # XLSX on a model without code interpreter support is rejected
    other = a.create_chat(model="gpt-text-only")
    p = assert_problem(a.upload(other["id"], "data.xlsx", xlsx_bytes(), XLSX_MIME), 400)
    assert "CODE_INTERPRETER_UNAVAILABLE" in field_reasons(p)


def test_file_citations_map_to_attachment(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    att = a.upload(chat["id"], "report.txt", b"Revenue grew 12%.", "text/plain").json()
    fid = provider_file_id(db, att["id"])
    mock_llm.script_chat(
        chat["id"],
        text_stream(
            chunks=["Revenue grew 12%."],
            before=[
                ("response.file_search_call.searching", {"type": "response.file_search_call.searching"}),
                ("response.file_search_call.completed", {"type": "response.file_search_call.completed"}),
            ],
            annotations=[{"type": "file_citation", "file_id": fid, "filename": "report.txt", "index": 5}],
        ),
    )
    res = a.send(chat["id"], "How did revenue change?")
    tools = res.all("tool")
    assert [(t["phase"], t["name"]) for t in tools] == [("start", "file_search"), ("done", "file_search")]
    assert tools[1]["details"] == {"files_searched": 0}
    assert res.first("citations")["items"] == [
        {"source": "file", "title": "report.txt", "attachment_id": att["id"], "snippet": ""}
    ]
    assert fid not in res.body


def test_upload_validation_errors(api):
    a = api("A")
    chat = a.create_chat()
    cid = chat["id"]
    p = assert_problem(a.upload(cid, "evil.exe", b"MZ", "application/x-msdownload"), 400)
    assert "UNSUPPORTED_CONTENT_TYPE" in field_reasons(p)
    p = assert_problem(a.upload(cid, "unknown.bin", b"\x00\x01", "application/octet-stream"), 400)
    assert "UNSUPPORTED_CONTENT_TYPE" in field_reasons(p)
    # image above rag.uploaded_image_max_size_kb (5 MiB)
    big = png_bytes() + b"\x00" * (5 * 1024 * 1024 + 10)
    p = assert_problem(a.upload(cid, "big.png", big, "image/png"), 400)
    assert "FILE_TOO_LARGE" in field_reasons(p)
    # multipart without a file field / no boundary
    r = a.post(f"/chats/{cid}/attachments", files={"other": ("x.txt", b"x", "text/plain")})
    assert "MISSING_FILE" in field_reasons(assert_problem(r, 400))
    r = a.post(f"/chats/{cid}/attachments", content=b"xx", headers={"content-type": "multipart/form-data"})
    assert "BOUNDARY_REQUIRED" in field_reasons(assert_problem(r, 400))
    # unknown chat
    p = assert_problem(a.upload(str(uuid.uuid4()), "a.txt", b"x", "text/plain"), 404)
    assert p["context"]["resource_type"] == RT_CHAT


def test_attachment_ids_validation(api):
    a = api("A")
    chat = a.create_chat()
    other = a.create_chat()
    att = a.upload(chat["id"], "a.txt", b"aaa", "text/plain").json()
    foreign = a.upload(other["id"], "b.txt", b"bbb", "text/plain").json()
    for ids in ([att["id"], att["id"]], [foreign["id"]], [str(uuid.uuid4())]):
        r = a.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "attachment_ids": ids})
        assert "invalid_attachment" in field_reasons(assert_problem(r, 400)), ids
    assert a.messages(chat["id"]) == []


def test_delete_attachment_cleanup_and_locking(api, mock_llm, db):
    a, b = api("A"), api("B")
    chat = a.create_chat()
    used = a.upload(chat["id"], "used.txt", b"used doc", "text/plain").json()
    spare = a.upload(chat["id"], "spare.txt", b"spare doc", "text/plain").json()
    a.send(chat["id"], "look", attachment_ids=[used["id"]])

    # referenced by a message -> locked
    r = a.delete(f"/chats/{chat['id']}/attachments/{used['id']}")
    p = assert_problem(r, 409)
    assert p["context"].get("resource_name") == "attachment_locked", p

    # another user cannot see or delete it
    assert_problem(b.get(f"/chats/{chat['id']}/attachments/{spare['id']}"), 404)
    assert_problem(b.delete(f"/chats/{chat['id']}/attachments/{spare['id']}"), 404)

    fid = provider_file_id(db, spare["id"])
    assert a.delete(f"/chats/{chat['id']}/attachments/{spare['id']}").status_code == 204
    p = assert_problem(a.get(f"/chats/{chat['id']}/attachments/{spare['id']}"), 404)
    assert p["context"]["resource_type"] == RT_ATTACHMENT
    assert a.delete(f"/chats/{chat['id']}/attachments/{spare['id']}").status_code == 204  # idempotent

    cleanup = [
        m for m in outbox_payloads(db, "mini-chat.attachment_cleanup") if m["payload"].get("attachment_id") == spare["id"]
    ]
    assert len(cleanup) == 1, outbox_payloads(db)
    assert wait_until(lambda: mock_llm.find("DELETE", f"/v1/files/{fid}"), timeout=15)
    assert wait_until(
        lambda: db.execute("SELECT cleanup_status FROM attachments WHERE id = ?", (ub(spare["id"]),)).fetchone()[0] == "done"
    )
    # the chat vector store is not deleted by an attachment delete
    vs = chat_vector_store(db, chat["id"])
    assert vs and not mock_llm.find("DELETE", f"/v1/vector_stores/{vs}")


def test_indexing_failure_marks_attachment_failed(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    mock_llm.vs_file_status = "failed"
    r = a.upload(chat["id"], "bad.txt", b"cannot index", "text/plain")
    assert_problem(r, 503)
    assert r.headers.get("retry-after") == "10"
    row = db.execute(
        "SELECT id, status, error_code FROM attachments WHERE chat_id = ? AND filename = 'bad.txt'", (ub(chat["id"]),)
    ).fetchone()
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    # the failed row stays visible through the API
    att_id = str(uuid.UUID(bytes=bytes(row["id"])))
    got = a.get(f"/chats/{chat['id']}/attachments/{att_id}").json()
    assert got["status"] == "failed" and got["error_code"] == "indexing_failed"
    # a failed attachment cannot be referenced by a message
    r = a.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "attachment_ids": [att_id]})
    assert "invalid_attachment" in field_reasons(assert_problem(r, 400))


def test_provider_file_upload_failure_is_503(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script(json_response(500, {"error": {"message": "storage down"}}), path="/v1/files")
    r = a.upload(chat["id"], "x.txt", b"x", "text/plain")
    assert_problem(r, 503)
    assert r.headers.get("retry-after") == "10"
