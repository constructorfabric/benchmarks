"""Attachments: upload / get / delete, validation, limits, indexing lifecycle,
tool availability, cleanup and abandoned-upload recovery."""

from __future__ import annotations

import base64
import struct
import time
import uuid
import zlib

import httpx
import pytest

import prov
from harness import from_blob, problem_reason, ub, wait_until

XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def png(w: int = 40, h: int = 20) -> bytes:
    rows = b"".join(b"\x00" + b"".join(bytes([(x * 7) % 256, (y * 5) % 256, 128]) for x in range(w)) for y in range(h))

    def chunk(t: bytes, d: bytes) -> bytes:
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def att_row(env, aid):
    return env.server.query("SELECT * FROM attachments WHERE id = ?", (ub(aid),))[0]


def raw_multipart(c, chat_id, body: bytes, content_type: str) -> httpx.Response:
    return c.post(f"/chats/{chat_id}/attachments", content=body, headers={"content-type": content_type})


def test_upload_document_get_and_file_search_tool(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    r = c.upload(chat["id"], "notes.txt", b"quarterly numbers: 42", "text/plain")
    assert r.status_code == 201, r.text
    a = r.json()
    assert r.headers["location"].endswith(f"/attachments/{a['id']}")
    assert a["status"] == "ready" and a["kind"] == "document"
    assert a["filename"] == "notes.txt" and a["content_type"] == "text/plain" and a["size_bytes"] == 21
    for absent in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert absent not in a
    g = c.get(f"/chats/{chat['id']}/attachments/{a['id']}")
    assert g.status_code == 200 and g.json() == a
    # provider calls: file upload (purpose=assistants), vector store, add with attributes
    up = env.mock.requests("/v1/files", "POST")[0]
    assert up["multipart"]["purpose"] == "assistants"
    assert up["multipart"]["file"]["filename"] == f"{chat['id']}_{a['id']}.txt"
    assert len(env.mock.requests("/v1/vector_stores", "POST")) == 1
    add = [x for x in env.mock.requests(method="POST") if "/vector_stores/" in x["path"] and x["path"].endswith("/files")][0]
    assert add["json"]["attributes"] == {"attachment_id": a["id"]}
    row = att_row(env, a["id"])
    assert row["for_file_search"] == 1 and row["for_code_interpreter"] == 0
    assert row["provider_file_id"].startswith("file-")
    vs = env.server.query("SELECT vector_store_id, provider FROM chat_vector_stores WHERE chat_id = ?", (ub(chat["id"]),))[0]
    # a second document reuses the chat vector store
    assert c.upload(chat["id"], "more.md", b"# more", "text/markdown").status_code == 201
    assert len(env.mock.requests("/v1/vector_stores", "POST")) == 1
    # next message carries the file_search tool, the guard, and file citations are mapped
    env.mock.reset()
    env.mock.script([
        prov.sse(
            prov.created(),
            prov.ev("response.file_search_call.searching"),
            prov.ev("response.file_search_call.completed", results=[{"file_id": row["provider_file_id"]}]),
            prov.delta("It is 42."),
            prov.completed(
                "It is 42.",
                annotations=[
                    {"type": "file_citation", "file_id": row["provider_file_id"], "filename": "x", "index": 3},
                    {"type": "file_citation", "file_id": "file-unknownunknown123", "index": 3},
                ],
            ),
        )
    ])
    s = c.send(chat["id"], "what are the numbers?")
    body = env.mock.chat_requests()[0]["json"]
    fs = [t for t in body["tools"] if t["type"] == "file_search"][0]
    assert fs["vector_store_ids"] == [vs["vector_store_id"]] and fs["max_num_results"] == 5
    assert "file_search" in body["instructions"]
    assert "file_search" in body["metadata"]["feature"]
    tools = s.all("tool")
    assert tools[1].data == {"phase": "done", "name": "file_search", "details": {"files_searched": 1}}
    items = s.first("citations").data["items"]
    assert items == [{"source": "file", "title": "notes.txt", "attachment_id": a["id"], "snippet": ""}]
    assert row["provider_file_id"] not in str(s.events)


def test_upload_image_thumbnail_and_vision_input(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    r = c.upload(chat["id"], "photo.png", png(400, 200), "image/png")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["kind"] == "image" and a["status"] == "ready"
    th = a["img_thumbnail"]
    assert th["content_type"] == "image/webp" and (th["width"], th["height"]) == (128, 64)
    assert base64.b64decode(th["data_base64"])[8:12] == b"WEBP"
    assert not env.mock.requests("/v1/vector_stores", "POST")
    s = c.send(chat["id"], "what is in the picture?", attachment_ids=[a["id"]])
    assert s.names[-1] == "done"
    parts = env.mock.chat_requests()[0]["json"]["input"][-1]["content"]
    assert parts[1] == {"type": "input_image", "file_id": att_row(env, a["id"])["provider_file_id"]}
    msgs = c.messages(chat["id"])
    summ = msgs[0]["attachments"]
    assert summ == [{"attachment_id": a["id"], "kind": "image", "filename": "photo.png", "status": "ready", "img_thumbnail": th}]
    assert msgs[1]["attachments"] == []


def test_image_guards(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    ids = [c.upload(chat["id"], f"i{i}.png", png(4, 4), "image/png").json()["id"] for i in range(5)]
    r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "x", "attachment_ids": ids})
    assert r.status == 400 and r.body["type"].endswith("out_of_range.v1~")
    assert problem_reason(r.body) == "TOO_MANY_IMAGES"
    nov = c.create_chat(model="std-novision")
    img = c.upload(nov["id"], "a.png", png(4, 4), "image/png").json()
    r = c.stream_raw("POST", f"/chats/{nov['id']}/messages:stream", {"content": "x", "attachment_ids": [img["id"]]})
    assert r.status == 400 and problem_reason(r.body) == "VISION_NOT_SUPPORTED"
    assert env.mock.chat_requests() == []


def test_xlsx_code_interpreter(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    r = c.upload(chat["id"], "data.xlsx", b"PK\x03\x04fake-xlsx", XLSX)
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "ready" and a["kind"] == "document"
    row = att_row(env, a["id"])
    assert row["for_code_interpreter"] == 1 and row["for_file_search"] == 0
    assert not env.mock.requests("/v1/vector_stores", "POST")
    c.send(chat["id"], "sum column A")
    body = env.mock.chat_requests()[0]["json"]
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"][0]
    assert ci["container"] == {"type": "auto", "file_ids": [row["provider_file_id"]]}
    assert "code_interpreter_call.outputs" in body["include"]
    # a model without code interpreter support rejects code-interpreter-only files
    nov = c.create_chat(model="std-novision")
    r = c.upload(nov["id"], "data.xlsx", b"PK", XLSX)
    assert r.status_code == 400 and problem_reason(r.json()) == "CODE_INTERPRETER_UNAVAILABLE"


def test_upload_validation(env):
    c = env.a
    chat = c.create_chat()
    r = c.upload(chat["id"], "a.zip", b"PK", "application/zip")
    assert r.status_code == 400 and problem_reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    r = c.upload(chat["id"], "a.exe", b"MZ", "application/octet-stream")
    assert r.status_code == 400 and problem_reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    r = c.upload(chat["id"], "report.pdf", b"%PDF-1.4", "application/octet-stream")
    assert r.status_code == 201 and r.json()["content_type"] == "application/pdf"
    r = c.upload(chat["id"], "table.csv", b"a,b\n1,2", "text/csv")
    assert r.status_code == 201 and r.json()["content_type"] == "text/plain"
    long = "n" * 300 + ".txt"
    r = c.upload(chat["id"], long, b"x", "text/plain")
    assert r.status_code == 201 and len(r.json()["filename"]) == 255 and r.json()["filename"].endswith(".txt")
    # image above the image size limit (5 MiB)
    big = b"\x89PNG" + b"0" * (5 * 1024 * 1024 + 10)
    r = c.upload(chat["id"], "big.png", big, "image/png")
    assert r.status_code == 400 and r.json()["type"].endswith("out_of_range.v1~")
    assert problem_reason(r.json()) == "FILE_TOO_LARGE"
    # multipart errors
    r = raw_multipart(c, chat["id"], b"x", "multipart/form-data")
    assert r.status_code == 400 and problem_reason(r.json()) == "BOUNDARY_REQUIRED"
    b = b"--XX\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nv\r\n--XX--\r\n"
    r = raw_multipart(c, chat["id"], b, "multipart/form-data; boundary=XX")
    assert r.status_code == 400 and problem_reason(r.json()) == "MISSING_FILE"
    b = b"--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nv\r\n--XX--\r\n"
    r = raw_multipart(c, chat["id"], b, "multipart/form-data; boundary=XX")
    assert r.status_code == 400 and problem_reason(r.json()) == "MISSING_CONTENT_TYPE"
    b = b"--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nunterminated"
    r = raw_multipart(c, chat["id"], b, "multipart/form-data; boundary=XX")
    assert r.status_code == 400 and problem_reason(r.json()) == "MULTIPART_ERROR"
    # missing filename defaults to "upload"
    b = b"--XX\r\nContent-Disposition: form-data; name=\"file\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--XX--\r\n"
    r = raw_multipart(c, chat["id"], b, "multipart/form-data; boundary=XX")
    assert r.status_code == 201 and r.json()["filename"] == "upload"
    # unknown chat
    assert c.upload(str(uuid.uuid4()), "a.txt", b"x", "text/plain").status_code == 404


def test_get_delete_and_locking(env):
    c = env.a
    chat = c.create_chat()
    a = c.upload(chat["id"], "a.txt", b"aaa", "text/plain").json()
    b = c.upload(chat["id"], "b.txt", b"bbb", "text/plain").json()
    # foreign access is 404
    other_chat = c.create_chat()
    assert c.get(f"/chats/{other_chat['id']}/attachments/{a['id']}").status_code == 404
    assert env.a2.get(f"/chats/{chat['id']}/attachments/{a['id']}").status_code == 404
    assert env.b.delete(f"/chats/{chat['id']}/attachments/{a['id']}").status_code == 404
    # referenced by a submitted message -> locked
    c.send(chat["id"], "use a", attachment_ids=[a["id"]])
    r = c.delete(f"/chats/{chat['id']}/attachments/{a['id']}")
    assert r.status_code == 409 and r.json()["context"]["resource_name"] == "attachment_locked"
    # unreferenced -> deleted, idempotent, provider file removed asynchronously
    file_id = att_row(env, b["id"])["provider_file_id"]
    assert c.delete(f"/chats/{chat['id']}/attachments/{b['id']}").status_code == 204
    assert c.get(f"/chats/{chat['id']}/attachments/{b['id']}").status_code == 404
    assert c.delete(f"/chats/{chat['id']}/attachments/{b['id']}").status_code == 204
    wait_until(lambda: att_row(env, b["id"])["cleanup_status"] == "done", msg="cleanup done")
    deletes = [x for x in env.mock.requests(method="DELETE") if x["path"] == f"/v1/files/{file_id}"]
    assert len(deletes) == 1
    # deleted attachments cannot be referenced
    r = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "x", "attachment_ids": [b["id"]]})
    assert r.status == 400 and problem_reason(r.body) == "invalid_attachment"
    # another user's attachment id in my message is rejected
    r = env.a2.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "x", "attachment_ids": [a["id"]]})
    assert r.status == 404


def test_indexing_completes_within_request(env):
    c = env.a
    chat = c.create_chat()
    env.mock.config(vector_store_file_status="in_progress", vector_store_file_poll_statuses=["in_progress", "completed"])
    r = c.upload(chat["id"], "doc.md", b"# doc", "text/markdown")
    assert r.status_code == 201 and r.json()["status"] == "ready"
    assert len([x for x in env.mock.requests(method="GET") if "/vector_stores/" in x["path"]]) >= 2


def test_indexing_failure_within_request(env):
    c = env.a
    chat = c.create_chat()
    env.mock.config(vector_store_file_status="in_progress", vector_store_file_poll_statuses=["failed"])
    r = c.upload(chat["id"], "doc.md", b"# doc", "text/markdown")
    assert r.status_code == 503
    assert r.headers["retry-after"] == "10"
    assert "indexing_failed" not in r.text
    row = env.server.query("SELECT * FROM attachments WHERE chat_id = ?", (ub(chat["id"]),))[0]
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    fid = row["provider_file_id"]
    wait_until(lambda: [x for x in env.mock.requests(method="DELETE") if x["path"] == f"/v1/files/{fid}"], msg="file delete")
    g = c.get(f"/chats/{chat['id']}/attachments/{from_blob(row['id'])}").json()
    assert g["status"] == "failed" and g["error_code"] == "indexing_failed"
    # provider upload failure
    env.mock.config(file_upload_status=500)
    r = c.upload(chat["id"], "doc2.md", b"# doc", "text/markdown")
    assert r.status_code == 503
    rows = env.server.query("SELECT status, error_code FROM attachments WHERE chat_id = ? AND filename = 'doc2.md'", (ub(chat["id"]),))
    assert rows == [{"status": "failed", "error_code": "upload_failed"}]


@pytest.mark.timeout(240)
def test_background_indexing_success_and_failure(env):
    c = env.a
    chat = c.create_chat()
    env.mock.config(vector_store_file_status="in_progress", vector_store_file_poll_statuses=["in_progress"] * 18 + ["completed"])
    t0 = time.time()
    r = c.upload(chat["id"], "slow.md", b"# slow", "text/markdown")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "uploaded"
    assert time.time() - t0 < 29
    # not ready -> cannot be referenced yet
    s = c.stream_raw("POST", f"/chats/{chat['id']}/messages:stream", {"content": "x", "attachment_ids": [a["id"]]})
    assert s.status == 400 and problem_reason(s.body) == "invalid_attachment"
    wait_until(lambda: c.get(f"/chats/{chat['id']}/attachments/{a['id']}").json()["status"] == "ready", 60, 1, "background ready")
    # background failure: failed + cleanup through the outbox
    env.mock.reset()
    env.mock.config(vector_store_file_status="in_progress", vector_store_file_poll_statuses=["in_progress"] * 18 + ["failed"])
    r = c.upload(chat["id"], "bad.md", b"# bad", "text/markdown")
    assert r.status_code == 201 and r.json()["status"] == "uploaded"
    bad = r.json()
    wait_until(lambda: c.get(f"/chats/{chat['id']}/attachments/{bad['id']}").json()["status"] == "failed", 60, 1, "background failed")
    assert c.get(f"/chats/{chat['id']}/attachments/{bad['id']}").json()["error_code"] == "indexing_failed"
    wait_until(lambda: att_row(env, bad["id"])["cleanup_status"] == "done", 30, 0.5, "cleanup")
    fid = att_row(env, bad["id"])["provider_file_id"]
    assert [x for x in env.mock.requests(method="DELETE") if x["path"] == f"/v1/files/{fid}"]


def test_per_chat_limits_and_concurrency(make_env):
    e = make_env({"gears": {"mini-chat": {"config": {"rag": {"max_documents_per_chat": 2, "max_total_upload_mb_per_chat": 1, "max_concurrent_uploads": 1, "uploaded_file_max_size_kb": 600}}}}})
    c = e.a
    chat = c.create_chat()
    assert c.upload(chat["id"], "1.txt", b"1", "text/plain").status_code == 201
    assert c.upload(chat["id"], "2.txt", b"2", "text/plain").status_code == 201
    r = c.upload(chat["id"], "3.txt", b"3", "text/plain")
    assert r.status_code == 429
    v = r.json()["context"]["violations"][0]
    assert v["subject"] == "document_limit"
    chat2 = c.create_chat()
    assert c.upload(chat2["id"], "big1.png", b"\x89PNG" + b"0" * 500_000, "image/png").status_code == 201
    r = c.upload(chat2["id"], "big2.png", b"\x89PNG" + b"0" * 600_000, "image/png")
    assert r.status_code == 429 and r.json()["context"]["violations"][0]["subject"] == "storage_limit"
    r = c.upload(chat2["id"], "huge.txt", b"x" * 700_000, "text/plain")
    assert r.status_code == 400 and problem_reason(r.json()) == "FILE_TOO_LARGE"
    # failed uploads do not count toward the limits
    e.mock.config(file_upload_status=500)
    chat3 = c.create_chat()
    for i in range(3):
        assert c.upload(chat3["id"], f"f{i}.txt", b"x", "text/plain").status_code == 503
    e.mock.config(file_upload_status=200)
    assert c.upload(chat3["id"], "ok.txt", b"x", "text/plain").status_code == 201
    # concurrency limit: a slow upload holds the only permit
    e.mock.reset()
    e.mock.config(vector_store_file_status="in_progress", vector_store_file_poll_statuses=["in_progress"] * 6 + ["completed"])
    chat4 = c.create_chat()
    import threading

    res = {}
    t = threading.Thread(target=lambda: res.setdefault("slow", c.server.client(c.token).upload(chat4["id"], "slow.txt", b"x", "text/plain")))
    t.start()
    wait_until(lambda: e.mock.requests("/v1/files", "POST"), msg="slow upload started")
    r = c.upload(chat4["id"], "fast.txt", b"x", "text/plain")
    assert r.status_code == 503 and r.headers["retry-after"] == "5"
    t.join()
    assert res["slow"].status_code == 201


def test_abandoned_upload_recovery(env):
    c = env.a
    chat = c.create_chat()
    # pending row: the provider upload never answers and the client goes away
    env.mock.script([{"kind": "hang"}], path_suffix="/v1/files")
    import socket as _s

    body = b"--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"p.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--XX--\r\n"
    sock = _s.create_connection(("127.0.0.1", env.server.api_port))
    head = (
        f"POST /mini-chat/v1/chats/{chat['id']}/attachments HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {c.token}\r\n"
        f"Content-Type: multipart/form-data; boundary=XX\r\nContent-Length: {len(body)}\r\n\r\n"
    ).encode()
    sock.sendall(head + body)
    wait_until(lambda: env.server.query("SELECT id FROM attachments WHERE chat_id = ? AND filename = 'p.txt'", (ub(chat["id"]),)), msg="pending row")
    sock.close()
    row = env.server.query("SELECT * FROM attachments WHERE chat_id = ? AND filename = 'p.txt'", (ub(chat["id"]),))[0]
    assert row["status"] == "pending"
    # uploaded row with a provider file: adding to the vector store never answers
    env.mock.reset()
    env.mock.script([{"kind": "hang"}], path_suffix="~vector_stores/")
    sock = _s.create_connection(("127.0.0.1", env.server.api_port))
    body2 = body.replace(b"p.txt", b"u.txt")
    head = head.replace(f"Content-Length: {len(body)}".encode(), f"Content-Length: {len(body2)}".encode())
    sock.sendall(head + body2)
    wait_until(lambda: env.server.query("SELECT status FROM attachments WHERE chat_id = ? AND filename = 'u.txt' AND status = 'uploaded'", (ub(chat["id"]),)), msg="uploaded row")
    sock.close()
    time.sleep(1)
    old = "2020-01-01T00:00:00.000000001Z"
    env.server.execute("UPDATE attachments SET updated_at = ? WHERE chat_id = ? AND filename IN ('p.txt','u.txt')", (old, ub(chat["id"])))
    wait_until(
        lambda: all(r["status"] == "failed" for r in env.server.query("SELECT status FROM attachments WHERE chat_id = ? AND filename IN ('p.txt','u.txt')", (ub(chat["id"]),))),
        20,
        msg="reaped",
    )
    rows = {r["filename"]: r for r in env.server.query("SELECT * FROM attachments WHERE chat_id = ?", (ub(chat["id"]),))}
    assert rows["p.txt"]["error_code"] == "upload_abandoned" and rows["p.txt"]["cleanup_status"] is None
    assert rows["u.txt"]["error_code"] == "upload_abandoned" and rows["u.txt"]["deleted_at"] is None
    wait_until(lambda: att_row(env, from_blob(rows["u.txt"]["id"]))["cleanup_status"] == "done", msg="cleanup")
    fid = rows["u.txt"]["provider_file_id"]
    assert [x for x in env.mock.requests(method="DELETE") if x["path"] == f"/v1/files/{fid}"]
    g = c.get(f"/chats/{chat['id']}/attachments/{from_blob(rows['p.txt']['id'])}").json()
    assert g["status"] == "failed" and g["error_code"] == "upload_abandoned"
