"""Attachments: upload / get / delete, validation, limits, indexing, tools, cleanup."""

from __future__ import annotations

import base64
import struct
import time
import uuid
import zlib

import pytest

from conftest import _fresh, assert_problem, ok_stream, ub, wait_until

ATT_RT = "gts.cf.core.mini_chat.attachment.v1~"
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def png(w: int, h: int, noise: bool = False) -> bytes:
    rows = []
    for y in range(h):
        if noise:
            row = bytes((x * 31 + y * 17 + (x * y) % 251) % 256 for x in range(w * 3))
        else:
            row = bytes([200, 30, 30] * w)
        rows.append(b"\x00" + row)
    raw = b"".join(rows)

    def chunk(t: bytes, d: bytes) -> bytes:
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 0 if noise else 6))
        + chunk(b"IEND", b"")
    )


def att_row(server, att_id):
    rows = server.query("SELECT * FROM attachments WHERE id = ?", ub(att_id))
    return rows[0] if rows else None


def chat_atts(server, chat_id):
    return server.query("SELECT * FROM attachments WHERE chat_id = ? ORDER BY created_at", ub(chat_id))


def test_upload_document(api, server, mock):
    chat = api.create_chat()
    r = api.upload(chat["id"], b"hello document", "notes.txt", "text/plain")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["filename"] == "notes.txt"
    assert a["content_type"] == "text/plain"
    assert a["size_bytes"] == len(b"hello document")
    assert a["status"] == "ready"
    assert a["kind"] == "document"
    for absent in ("error_code", "img_thumbnail", "doc_summary", "summary_updated_at"):
        assert absent not in a
    g = api.get(f"/chats/{chat['id']}/attachments/{a['id']}")
    assert g.status_code == 200
    assert g.json() == a

    files = mock.requests("POST", "/v1/files")
    assert len(files) == 1
    assert files[0]["json"]["purpose"] == "assistants"
    # provider-side name follows {chat_id}_{attachment_id}.{ext}
    assert files[0]["json"]["filename"] == f"{chat['id']}_{a['id']}.txt"
    vs_create = mock.requests("POST", "/v1/vector_stores")
    vs_create = [x for x in vs_create if x["path"] == "/v1/vector_stores"]
    assert len(vs_create) == 1
    adds = [x for x in mock.requests("POST", "/files") if "/vector_stores/" in x["path"]]
    assert len(adds) == 1
    assert adds[0]["json"]["attributes"] == {"attachment_id": a["id"]}

    row = att_row(server, a["id"])
    assert row["status"] == "ready"
    assert row["provider_file_id"] == adds[0]["json"]["file_id"]
    assert row["storage_backend"] == "openai"
    assert row["attachment_kind"] == "document"
    assert (row["for_file_search"], row["for_code_interpreter"]) == (1, 0)
    assert row["uploaded_by_user_id"] == ub(api.user_id)
    assert row["cleanup_status"] is None
    vs = server.query("SELECT vector_store_id, provider FROM chat_vector_stores WHERE chat_id = ?", ub(chat["id"]))
    assert len(vs) == 1 and vs[0]["vector_store_id"] and vs[0]["provider"] == "openai"

    # a second document reuses the chat vector store
    assert api.upload(chat["id"], b"second", "b.md", "text/markdown").status_code == 201
    assert len([x for x in mock.requests("POST", "/v1/vector_stores") if x["path"] == "/v1/vector_stores"]) == 1


def test_upload_image_with_thumbnail(api, server, mock):
    chat = api.create_chat()
    r = api.upload(chat["id"], png(400, 200), "pic.png", "image/png")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["kind"] == "image" and a["status"] == "ready"
    th = a["img_thumbnail"]
    assert th["content_type"] == "image/webp"
    assert (th["width"], th["height"]) == (128, 64)
    data = base64.b64decode(th["data_base64"])
    assert data[:4] == b"RIFF" and data[8:12] == b"WEBP"
    row = att_row(server, a["id"])
    assert (row["for_file_search"], row["for_code_interpreter"]) == (0, 0)
    assert row["img_thumbnail"] == data
    assert [x for x in mock.requests("POST", "/v1/vector_stores")] == []


def test_upload_small_image_not_upscaled(api):
    chat = api.create_chat()
    a = api.upload(chat["id"], png(20, 10), "s.png", "image/png").json()
    assert (a["img_thumbnail"]["width"], a["img_thumbnail"]["height"]) == (20, 10)


def test_corrupt_image_is_ready_without_thumbnail(api):
    chat = api.create_chat()
    r = api.upload(chat["id"], b"\x89PNG not really", "bad.png", "image/png")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "ready"
    assert "img_thumbnail" not in a
    assert "error_code" not in a


def test_xlsx_inferred_and_code_interpreter_tool(api, server, mock):
    chat = api.create_chat()
    r = api.upload(chat["id"], b"PK\x03\x04fake", "sheet.xlsx", "application/octet-stream")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["content_type"] == XLSX
    assert a["status"] == "ready"
    row = att_row(server, a["id"])
    assert (row["for_file_search"], row["for_code_interpreter"]) == (0, 1)
    assert not [x for x in mock.requests("POST", "/v1/vector_stores")]

    mock.script(
        {
            "events_before": [
                {"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"},
                {
                    "type": "response.output_item.done",
                    "item": {"type": "code_interpreter_call", "id": "ci_1", "outputs": [{"type": "logs", "logs": "sum=42"}]},
                },
            ],
            "text": "The sum is 42.",
        }
    )
    rid = str(uuid.uuid4())
    resp = ok_stream(api.send(chat["id"], "sum column A", request_id=rid))
    assert resp.all("tool") == [
        {"phase": "start", "name": "code_interpreter", "details": {}},
        {"phase": "done", "name": "code_interpreter", "details": {"output": "sum=42"}},
    ]
    body = mock.chat_requests(chat["id"])[-1]["json"]
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"]
    assert ci == [{"type": "code_interpreter", "container": {"type": "auto", "file_ids": [row["provider_file_id"]]}}]
    assert body["include"] == ["code_interpreter_call.outputs"]
    assert not [t for t in body["tools"] if t["type"] == "file_search"]
    turn = server.query("SELECT code_interpreter_completed_count FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert turn["code_interpreter_completed_count"] == 1


def test_filename_default_and_truncation(api):
    chat = api.create_chat()
    r = api.upload(chat["id"], b"data", None, "text/plain")
    assert r.status_code == 201, r.text
    assert r.json()["filename"] == "upload"
    long_name = "n" * 300 + ".txt"
    r = api.upload(chat["id"], b"data", long_name, "text/plain")
    assert r.status_code == 201, r.text
    name = r.json()["filename"]
    assert len(name) == 255
    assert name.endswith(".txt")


def test_csv_is_stored_as_text(api):
    chat = api.create_chat()
    r = api.upload(chat["id"], b"a,b\n1,2\n", "t.csv", "text/csv")
    assert r.status_code == 201, r.text
    assert r.json()["content_type"] == "text/plain"


def test_unsupported_types(api, server):
    chat = api.create_chat()
    for name, ct in (("a.zip", "application/zip"), ("a.bin", "application/octet-stream"), ("a.exe", "application/x-msdownload")):
        r = api.upload(chat["id"], b"xx", name, ct)
        assert_problem(r, 400, category="invalid_argument", reason="UNSUPPORTED_CONTENT_TYPE", resource_type=ATT_RT)
    assert chat_atts(server, chat["id"]) == []


def test_multipart_errors(api):
    chat = api.create_chat()
    path = f"/chats/{chat['id']}/attachments"
    r = api.post(path, content=b"x", headers={"Content-Type": "multipart/form-data"})
    assert_problem(r, 400, reason="BOUNDARY_REQUIRED", field="content_type")
    # a non-multipart content type is rejected (by the api-gateway content-type filter)
    r = api.post(path, json={"file": "x"})
    assert_problem(r, 400, category="invalid_argument")
    r = api.post(path, files={"other": ("a.txt", b"x", "text/plain")})
    assert_problem(r, 400, reason="MISSING_FILE", field="file")
    r = api.upload(chat["id"], b"x", "a.txt", None)
    assert_problem(r, 400, reason="MISSING_CONTENT_TYPE", field="content_type")
    r = api.post(path, content=b"--zz\r\ngarbage", headers={"Content-Type": "multipart/form-data; boundary=zz"})
    assert_problem(r, 400, reason="MULTIPART_ERROR", field="multipart")


def test_upload_unknown_or_foreign_chat(api, other_user):
    r = api.upload(str(uuid.uuid4()), b"x", "a.txt", "text/plain")
    assert_problem(r, 404, resource_type="gts.cf.core.mini_chat.chat.v1~")
    chat = api.create_chat()
    r = other_user.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert_problem(r, 404, resource_type="gts.cf.core.mini_chat.chat.v1~")


def test_upload_model_gone_from_catalog(api, server):
    chat = api.create_chat()
    server.execute("UPDATE chats SET model = 'removed-model' WHERE id = ?", ub(chat["id"]))
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert_problem(r, 400, reason="INVALID_MODEL", field="model")
    r = api.send(chat["id"], "hi")
    assert_problem(r, 400, reason="INVALID_MODEL")


def test_code_interpreter_unavailable_for_model(api):
    chat = api.create_chat(model="gpt-novision")
    r = api.upload(chat["id"], b"PK", "s.xlsx", XLSX)
    assert_problem(r, 400, category="invalid_argument", reason="CODE_INTERPRETER_UNAVAILABLE")


def test_get_attachment_isolation(api, other_user):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"x", "a.txt", "text/plain").json()
    other_chat = api.create_chat()
    assert_problem(api.get(f"/chats/{other_chat['id']}/attachments/{a['id']}"), 404, resource_type=ATT_RT)
    assert_problem(other_user.get(f"/chats/{chat['id']}/attachments/{a['id']}"), 404)
    assert_problem(api.get(f"/chats/{chat['id']}/attachments/{uuid.uuid4()}"), 404, resource_type=ATT_RT)


def test_delete_attachment_and_cleanup(api, server, mock):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"to delete", "d.txt", "text/plain").json()
    file_id = att_row(server, a["id"])["provider_file_id"]
    r = api.delete(f"/chats/{chat['id']}/attachments/{a['id']}")
    assert r.status_code == 204
    assert_problem(api.get(f"/chats/{chat['id']}/attachments/{a['id']}"), 404)
    assert api.delete(f"/chats/{chat['id']}/attachments/{a['id']}").status_code == 204
    wait_until(lambda: mock.requests("DELETE", f"/v1/files/{file_id}"), msg="provider file delete")
    row = wait_until(lambda: (lambda x: x if x["cleanup_status"] == "done" else None)(att_row(server, a["id"])), msg="cleanup done")
    assert row["deleted_at"] is not None
    time.sleep(0.5)
    assert len(mock.requests("DELETE", f"/v1/files/{file_id}")) == 1


def test_delete_attachment_locked_and_foreign(api, other_user):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"ref", "r.txt", "text/plain").json()
    ok_stream(api.send(chat["id"], "see attached", attachment_ids=[a["id"]]))
    r = api.delete(f"/chats/{chat['id']}/attachments/{a['id']}")
    assert_problem(r, 409, category="already_exists", reason="attachment_locked")
    assert_problem(other_user.delete(f"/chats/{chat['id']}/attachments/{a['id']}"), 404)


def test_message_attachment_summary(api, server):
    chat = api.create_chat()
    doc = api.upload(chat["id"], b"doc", "d.txt", "text/plain").json()
    img = api.upload(chat["id"], png(64, 64), "i.png", "image/png").json()
    ok_stream(api.send(chat["id"], "both", attachment_ids=[doc["id"], img["id"]]))
    user = api.messages(chat["id"])[0]
    summ = {a["attachment_id"]: a for a in user["attachments"]}
    assert set(summ) == {doc["id"], img["id"]}
    assert summ[doc["id"]]["kind"] == "document" and summ[doc["id"]]["filename"] == "d.txt"
    assert summ[doc["id"]]["status"] == "ready"
    assert "img_thumbnail" not in summ[doc["id"]] or summ[doc["id"]]["img_thumbnail"] is None
    assert summ[img["id"]]["kind"] == "image"
    assert summ[img["id"]]["img_thumbnail"]["content_type"] == "image/webp"
    links = server.query("SELECT attachment_id FROM message_attachments WHERE chat_id = ?", ub(chat["id"]))
    assert {bytes(x["attachment_id"]) for x in links} == {ub(doc["id"]), ub(img["id"])}


def test_image_input_and_vision_guard(api, server, mock):
    chat = api.create_chat()
    img = api.upload(chat["id"], png(32, 32), "i.png", "image/png").json()
    file_id = att_row(server, img["id"])["provider_file_id"]
    ok_stream(api.send(chat["id"], "describe", attachment_ids=[img["id"]]))
    content = mock.chat_requests(chat["id"])[-1]["json"]["input"][-1]["content"]
    assert content == [{"type": "input_text", "text": "describe"}, {"type": "input_image", "file_id": file_id}]

    nv = api.create_chat(model="gpt-novision")
    img2 = api.upload(nv["id"], png(32, 32), "i.png", "image/png").json()
    r = api.send(nv["id"], "describe", attachment_ids=[img2["id"]])
    assert_problem(r, 400, category="invalid_argument", reason="VISION_NOT_SUPPORTED")
    assert mock.chat_requests(nv["id"]) == []


def test_attachment_reference_validation(api, other_user, server):
    chat = api.create_chat()
    other_chat = api.create_chat()
    foreign = api.upload(other_chat["id"], b"x", "x.txt", "text/plain").json()
    r = api.send(chat["id"], "x", attachment_ids=[foreign["id"]])
    assert_problem(r, 400, reason="invalid_attachment")
    mine = api.upload(chat["id"], b"x", "x.txt", "text/plain").json()
    server.execute("UPDATE attachments SET status = 'uploaded' WHERE id = ?", ub(mine["id"]))
    r = api.send(chat["id"], "x", attachment_ids=[mine["id"]])
    assert_problem(r, 400, reason="invalid_attachment")
    assert api.messages(chat["id"]) == []


def test_file_search_tool_and_file_citations(api, server, mock):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"The capital is Paris.", "geo.txt", "text/plain").json()
    row = att_row(server, a["id"])
    vs_id = server.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", ub(chat["id"]))[0][0]
    mock.script(
        {
            "events_before": [
                {"type": "response.file_search_call.searching", "item_id": "fs_1"},
                {"type": "response.file_search_call.completed", "item_id": "fs_1", "results": [{"file_id": row["provider_file_id"]}, {"file_id": "x"}]},
            ],
            "text": "Paris.",
            "annotations": [
                {"type": "file_citation", "file_id": row["provider_file_id"], "index": 0, "filename": "geo.txt"},
                {"type": "file_citation", "file_id": "file-unknown0000000000", "index": 0},
            ],
        }
    )
    rid = str(uuid.uuid4())
    r = ok_stream(api.send(chat["id"], "capital?", request_id=rid))
    assert r.all("tool") == [
        {"phase": "start", "name": "file_search", "details": {}},
        {"phase": "done", "name": "file_search", "details": {"files_searched": 2}},
    ]
    items = r.first("citations")["items"]
    assert items == [{"source": "file", "title": "geo.txt", "attachment_id": a["id"], "snippet": ""}]
    body = mock.chat_requests(chat["id"])[-1]["json"]
    fs = [t for t in body["tools"] if t["type"] == "file_search"]
    assert fs == [{"type": "file_search", "vector_store_ids": [vs_id], "max_num_results": 5}]
    assert "file_search" in body["metadata"]["feature"]
    assert row["provider_file_id"] not in str(r.events)
    turn = server.query("SELECT file_search_completed_count FROM chat_turns WHERE request_id = ?", ub(rid))[0]
    assert turn["file_search_completed_count"] == 1


def test_provider_upload_failure(api, server, mock):
    chat = api.create_chat()
    mock.config(files_upload_status=500)
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    body = assert_problem(r, 503, category="service_unavailable")
    assert r.headers["retry-after"] == "10"
    assert "upload_failed" not in str(body)
    rows = chat_atts(server, chat["id"])
    assert len(rows) == 1
    assert (rows[0]["status"], rows[0]["error_code"]) == ("failed", "upload_failed")
    att_id = str(uuid.UUID(bytes=rows[0]["id"]))
    g = api.get(f"/chats/{chat['id']}/attachments/{att_id}").json()
    assert g["status"] == "failed" and g["error_code"] == "upload_failed"


def test_indexing_failure_marks_failed_and_deletes_file(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_status="failed")
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    body = assert_problem(r, 503)
    assert r.headers["retry-after"] == "10"
    assert body.get("detail") == "Service temporarily unavailable"
    assert "indexing_failed" not in str(body)
    row = chat_atts(server, chat["id"])[0]
    assert (row["status"], row["error_code"]) == ("failed", "indexing_failed")
    wait_until(lambda: mock.requests("DELETE", f"/v1/files/{row['provider_file_id']}"), msg="best-effort delete")


def test_vector_store_add_failure(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_add_status=500)
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert_problem(r, 503)
    row = chat_atts(server, chat["id"])[0]
    assert (row["status"], row["error_code"]) == ("failed", "indexing_failed")


def test_vector_store_create_failure(api, server, mock):
    chat = api.create_chat()
    mock.config(vector_store_create_status=500)
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert_problem(r, 503)
    assert r.headers["retry-after"] == "10"
    row = chat_atts(server, chat["id"])[0]
    assert row["status"] == "failed"
    assert server.query("SELECT id FROM chat_vector_stores WHERE chat_id = ?", ub(chat["id"])) == []
    mock.config(vector_store_create_status=200)
    assert api.upload(chat["id"], b"x", "a.txt", "text/plain").status_code == 201


def test_provider_mismatch(api, server):
    chat = api.create_chat()
    assert api.upload(chat["id"], b"x", "a.txt", "text/plain").status_code == 201
    server.execute("UPDATE chat_vector_stores SET provider = 'azure-other' WHERE chat_id = ?", ub(chat["id"]))
    r = api.upload(chat["id"], b"y", "b.txt", "text/plain")
    assert_problem(r, 409, category="already_exists", reason="provider_mismatch")
    # images are not stored in the vector store and still work
    assert api.upload(chat["id"], png(8, 8), "i.png", "image/png").status_code == 201


def test_indexing_polls_until_completed(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_poll_statuses=["in_progress", "in_progress", "completed"])
    t0 = time.time()
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert r.status_code == 201, r.text
    assert r.json()["status"] == "ready"
    assert time.time() - t0 < 10
    polls = [x for x in mock.requests("GET") if "/vector_stores/" in x["path"]]
    assert len(polls) >= 1


def test_missing_status_counts_as_in_progress(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_poll_statuses=["in_progress", "completed"])
    r = api.upload(chat["id"], b"x", "a.txt", "text/plain")
    assert r.json()["status"] == "ready"


def test_background_indexing_completes(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_poll_statuses=["in_progress"] * 17 + ["completed"])
    t0 = time.time()
    r = api.upload(chat["id"], b"slow doc", "slow.txt", "text/plain")
    elapsed = time.time() - t0
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "uploaded"
    assert 20 <= elapsed < 29, elapsed
    # a message cannot reference it yet
    assert_problem(api.send(chat["id"], "x", attachment_ids=[a["id"]]), 400, reason="invalid_attachment")
    wait_until(
        lambda: api.get(f"/chats/{chat['id']}/attachments/{a['id']}").json()["status"] == "ready",
        timeout=60,
        interval=1,
        msg="background ready",
    )
    ok_stream(api.send(chat["id"], "x", attachment_ids=[a["id"]]))


def test_background_indexing_failure(api, server, mock):
    chat = api.create_chat()
    mock.config(vs_file_poll_statuses=["in_progress"] * 17 + ["failed"])
    r = api.upload(chat["id"], b"slow doc", "slow.txt", "text/plain")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "uploaded"
    detail = wait_until(
        lambda: (lambda d: d if d["status"] == "failed" else None)(api.get(f"/chats/{chat['id']}/attachments/{a['id']}").json()),
        timeout=60,
        interval=1,
        msg="background failure",
    )
    assert detail["error_code"] == "indexing_failed"
    row = att_row(server, a["id"])
    wait_until(lambda: mock.requests("DELETE", f"/v1/files/{row['provider_file_id']}"), msg="outbox delete")
    wait_until(lambda: att_row(server, a["id"])["cleanup_status"] == "done", msg="cleanup done")


def test_upload_reaper(api, server, mock):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"x", "a.txt", "text/plain").json()
    b = api.upload(chat["id"], b"y", "b.txt", "text/plain").json()
    file_a = att_row(server, a["id"])["provider_file_id"]
    old = "2000-01-01T00:00:00.000000001Z"
    server.execute("UPDATE attachments SET status = 'uploaded', updated_at = ? WHERE id = ?", old, ub(a["id"]))
    server.execute(
        "UPDATE attachments SET status = 'pending', provider_file_id = NULL, updated_at = ? WHERE id = ?", old, ub(b["id"])
    )
    for att in (a, b):
        d = wait_until(
            lambda: (lambda x: x if x["status"] == "failed" else None)(api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()),
            timeout=20,
            msg="reaped",
        )
        assert d["error_code"] == "upload_abandoned"
    wait_until(lambda: mock.requests("DELETE", f"/v1/files/{file_a}"), msg="abandoned file deleted")
    assert att_row(server, b["id"])["cleanup_status"] is None
    assert att_row(server, a["id"])["deleted_at"] is None
    # fresh rows are not reaped
    c = api.upload(chat["id"], b"z", "c.txt", "text/plain").json()
    server.execute("UPDATE attachments SET status = 'uploaded' WHERE id = ?", ub(c["id"]))
    time.sleep(3)
    assert att_row(server, c["id"])["status"] == "uploaded"


def test_chat_delete_cleans_provider_resources(api, server, mock):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"x", "a.txt", "text/plain").json()
    b = api.upload(chat["id"], png(8, 8), "b.png", "image/png").json()
    files = {att_row(server, x["id"])["provider_file_id"] for x in (a, b)}
    vs_id = server.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", ub(chat["id"]))[0][0]
    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    wait_until(lambda: mock.requests("DELETE", f"/v1/vector_stores/{vs_id}"), timeout=20, msg="vector store delete")
    deleted = {x["path"].rsplit("/", 1)[1] for x in mock.requests("DELETE", "/v1/files/")}
    assert files <= deleted
    for x in (a, b):
        assert att_row(server, x["id"])["cleanup_status"] == "done"
    wait_until(
        lambda: server.query("SELECT id FROM chat_vector_stores WHERE chat_id = ?", ub(chat["id"])) == [], msg="vs row removed"
    )


def test_chat_cleanup_retries_failed_deletes(api, server, mock):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"x", "a.txt", "text/plain").json()
    mock.config(files_delete_status=500)
    assert api.delete(f"/chats/{chat['id']}").status_code == 204
    wait_until(lambda: (att_row(server, a["id"])["cleanup_attempts"] or 0) >= 1, timeout=20, msg="attempt counted")
    row = att_row(server, a["id"])
    assert row["last_cleanup_error"]
    mock.config(files_delete_status=200)
    wait_until(lambda: att_row(server, a["id"])["cleanup_status"] == "done", timeout=60, interval=0.5, msg="retried cleanup")


# ───────────────────────── limits (dedicated server) ─────────────────────────


@pytest.fixture
def limited(servers):
    srv = servers("limits")
    return srv, _fresh(srv)


def test_file_too_large(limited, mock):
    srv, api = limited
    chat = api.create_chat()
    r = api.upload(chat["id"], b"a" * (600 * 1024 + 1), "big.txt", "text/plain")
    assert_problem(r, 400, category="out_of_range", reason="FILE_TOO_LARGE", field="content_length")
    assert api.upload(chat["id"], b"a" * (600 * 1024), "ok.txt", "text/plain").status_code == 201
    r = api.upload(chat["id"], png(200, 200, noise=True), "big.png", "image/png")
    assert_problem(r, 400, reason="FILE_TOO_LARGE")
    assert not mock.requests("POST", "/v1/files") or len(mock.requests("POST", "/v1/files")) == 1


def test_document_limit(limited):
    srv, api = limited
    chat = api.create_chat()
    for i in range(2):
        assert api.upload(chat["id"], b"x", f"{i}.txt", "text/plain").status_code == 201
    r = api.upload(chat["id"], b"x", "3.txt", "text/plain")
    assert_problem(r, 429, category="resource_exhausted", reason="document_limit")
    # images do not count as documents
    assert api.upload(chat["id"], png(8, 8), "i.png", "image/png").status_code == 201
    # deleting a document frees a slot
    first = srv.query("SELECT id FROM attachments WHERE chat_id = ? AND attachment_kind = 'document'", ub(chat["id"]))[0]["id"]
    api.delete(f"/chats/{chat['id']}/attachments/{uuid.UUID(bytes=first)}")
    assert api.upload(chat["id"], b"x", "4.txt", "text/plain").status_code == 201


def test_storage_limit(limited, mock):
    srv, api = limited
    chat = api.create_chat()
    assert api.upload(chat["id"], b"a" * (590 * 1024), "a.txt", "text/plain").status_code == 201
    r = api.upload(chat["id"], b"b" * (590 * 1024), "b.txt", "text/plain")
    assert_problem(r, 429, reason="storage_limit")
    # failed uploads do not count toward the limit
    mock.config(files_upload_status=500)
    chat2 = api.create_chat()
    assert api.upload(chat2["id"], b"a" * (590 * 1024), "a.txt", "text/plain").status_code == 503
    mock.config(files_upload_status=200)
    assert api.upload(chat2["id"], b"b" * (590 * 1024), "b.txt", "text/plain").status_code == 201


def test_too_many_images(limited, mock):
    srv, api = limited
    chat = api.create_chat()
    ids = [api.upload(chat["id"], png(8, 8), f"{i}.png", "image/png").json()["id"] for i in range(3)]
    r = api.send(chat["id"], "look", attachment_ids=ids)
    assert_problem(r, 400, category="out_of_range", reason="TOO_MANY_IMAGES", field="image_count")
    assert mock.chat_requests(chat["id"]) == []
    ok_stream(api.send(chat["id"], "look", attachment_ids=ids[:2]))


def test_code_interpreter_call_limit(api, server, mock):
    chat = api.create_chat()
    assert api.upload(chat["id"], b"PK", "s.xlsx", XLSX).status_code == 201
    starts = [{"type": "response.code_interpreter_call.in_progress", "item_id": f"ci_{i}"} for i in range(11)]
    mock.script({"events_before": starts, "text": "never"})
    rid = str(uuid.uuid4())
    r = api.send(chat["id"], "loop", request_id=rid)
    assert r.names[-1] == "error"
    assert r.first("error")["code"] == "code_interpreter_calls_exceeded"
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error" and t["error_code"] == "code_interpreter_calls_exceeded"


def test_azure_storage_kind_paths(servers, mock):
    srv = servers("azure")
    api = _fresh(srv)
    chat = api.create_chat()
    r = api.upload(chat["id"], b"azure doc", "a.txt", "text/plain")
    assert r.status_code == 201, r.text
    files = [x for x in mock.requests("POST") if x["path"].endswith("/files") and "/vector_stores/" not in x["path"]]
    assert files and files[-1]["path"] == "/openai/files"
    assert files[-1]["query"].get("api-version") == "2025-04-01-preview"
    vs = [x for x in mock.requests("POST") if x["path"] == "/openai/vector_stores"]
    assert vs and vs[-1]["query"].get("api-version") == "2025-04-01-preview"
    ok_stream(api.send(chat["id"], "q"))
    assert mock.chat_requests(chat["id"])[-1]["path"] == "/v1/responses"
