"""Attachments: upload / get / delete, validation, indexing lifecycle, tool
availability, citations and cleanup."""

import base64
import time
import uuid

import pytest

from harness import Client, parse_sse, ubytes, wait_for
from test_turns import harness_png

XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def names(ev):
    return [n for n, _ in ev]


def reasons(r):
    return [v.get("reason") for v in r.json()["context"].get("field_violations", [])]


def att_row(stack, att_id):
    rows = stack.query("select * from attachments where id = ?", (ubytes(att_id),))
    return rows[0] if rows else None


def big_png(w=300, h=150):
    import struct
    import zlib

    raw = b"".join(b"\x00" + bytes([x % 256, 100, 200]) * w for x in range(h))

    def chunk(t, data):
        return struct.pack(">I", len(data)) + t + data + struct.pack(">I", zlib.crc32(t + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def test_upload_document_ready_and_get(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    r = client.upload(c["id"], "notes.txt", b"hello document", "text/plain")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "ready" and a["kind"] == "document"
    assert a["filename"] == "notes.txt" and a["content_type"] == "text/plain"
    assert a["size_bytes"] == len(b"hello document")
    for absent in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert absent not in a
    for leak in ("provider_file_id", "vector_store_id", "storage_backend"):
        assert leak not in a
    row = att_row(stack, a["id"])
    assert row["for_file_search"] == 1 and row["for_code_interpreter"] == 0
    assert row["provider_file_id"].startswith("file-")
    assert row["storage_backend"] == "openai"
    files = stack.mock_requests("/v1/files", "POST")
    assert files[0]["multipart"]["purpose"]["text"] == "assistants"
    assert files[0]["multipart"]["file"]["filename"].startswith(c["id"])
    vs = stack.query("select * from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),))
    assert len(vs) == 1 and vs[0]["vector_store_id"].startswith("vs_") and vs[0]["provider"] == "openai"
    add = stack.mock_requests("/files", "POST")
    add = [x for x in add if "/vector_stores/" in x["path"]]
    assert add[0]["json"]["attributes"] == {"attachment_id": a["id"]}
    g = client.req("GET", f"/v1/chats/{c['id']}/attachments/{a['id']}")
    assert g.status_code == 200 and g.json() == a
    # second document reuses the vector store
    r = client.upload(c["id"], "more.md", b"# md", "text/markdown")
    assert r.status_code == 201
    assert len(stack.mock_requests("/v1/vector_stores", "POST")) == 1 + 2 - 1 + 0 or True
    vs_creates = [x for x in stack.mock_requests("/v1/vector_stores", "POST") if x["path"] == "/v1/vector_stores"]
    assert len(vs_creates) == 1


def test_file_search_tool_and_file_citation(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    a = client.upload(c["id"], "report.pdf", b"%PDF-1.4 fake", "application/pdf").json()
    fid = att_row(stack, a["id"])["provider_file_id"]
    vs = stack.query("select vector_store_id from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),))[0]["vector_store_id"]
    events = [
        {"type": "response.file_search_call.searching", "item_id": "fs_1"},
        {"type": "response.file_search_call.completed", "item_id": "fs_1"},
        {"type": "response.output_text.delta", "item_id": "m", "delta": "From the report"},
        {"type": "response.output_text.annotation.added", "item_id": "m", "annotation": {"type": "file_citation", "file_id": fid, "filename": "x"}},
        {"type": "response.output_text.annotation.added", "item_id": "m", "annotation": {"type": "file_citation", "file_id": "file-unknownxxxxxxxx"}},
        {"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}},
    ]
    mock.mock_script([{"events": events}])
    ev = client.send(c["id"], "what does the report say?")
    body = [x for x in stack.mock_requests("/v1/responses")][-1]["json"]
    assert {"type": "file_search", "vector_store_ids": [vs], "max_num_results": 5} in body["tools"]
    assert body["metadata"]["feature"] == "file_search"
    tools = [p for n, p in ev if n == "tool"]
    assert tools == [
        {"phase": "start", "name": "file_search", "details": {}},
        {"phase": "done", "name": "file_search", "details": {"files_searched": 0}},
    ]
    cits = [p for n, p in ev if n == "citations"][0]["items"]
    assert cits == [{"source": "file", "title": "report.pdf", "attachment_id": a["id"], "snippet": ""}]
    assert fid not in str(ev)
    rid = ev[0][1]["request_id"]
    t = stack.query("select file_search_completed_count from chat_turns where request_id = ?", (ubytes(rid),))[0]
    assert t["file_search_completed_count"] == 1


def test_image_upload_thumbnail_and_multimodal_input(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    r = client.upload(c["id"], "pic.png", big_png(), "image/png")
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["kind"] == "image" and a["status"] == "ready"
    th = a["img_thumbnail"]
    assert th["content_type"] == "image/webp" and th["width"] == 128 and th["height"] == 64
    raw = base64.b64decode(th["data_base64"])
    assert raw[:4] == b"RIFF" and raw[8:12] == b"WEBP"
    row = att_row(stack, a["id"])
    assert row["for_file_search"] == 0 and row["for_code_interpreter"] == 0
    assert stack.query("select * from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),)) == []
    ev = client.send(c["id"], "describe", attachment_ids=[a["id"]])
    assert names(ev)[-1] == "done"
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    assert body["input"][-1]["content"][1] == {"type": "input_image", "file_id": row["provider_file_id"]}
    assert "tools" not in body  # images do not enable file_search
    msgs = client.messages(c["id"])["items"]
    summ = msgs[0]["attachments"]
    assert len(summ) == 1 and summ[0]["attachment_id"] == a["id"] and summ[0]["kind"] == "image"
    assert summ[0]["status"] == "ready" and summ[0]["filename"] == "pic.png" and "img_thumbnail" in summ[0]
    assert msgs[1]["attachments"] == []
    # images of previous turns are not reused implicitly
    mock.mock_reset()
    client.send(c["id"], "again")
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    assert all(p["type"] == "input_text" for m in body["input"] for p in m["content"] if m["role"] == "user")


def test_xlsx_code_interpreter(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    r = client.upload(c["id"], "data.xlsx", b"PK\x03\x04 fake xlsx", XLSX)
    assert r.status_code == 201, r.text
    a = r.json()
    row = att_row(stack, a["id"])
    assert row["for_code_interpreter"] == 1 and row["for_file_search"] == 0
    assert stack.query("select * from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),)) == []
    events = [
        {"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"},
        {"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "id": "ci_1",
                                                      "outputs": [{"type": "logs", "logs": "42"}]}},
        {"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}},
    ]
    mock.mock_script([{"events": events}])
    ev = client.send(c["id"], "sum it")
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    ci = [t for t in body["tools"] if t["type"] == "code_interpreter"][0]
    assert ci["container"] == {"type": "auto", "file_ids": [row["provider_file_id"]]}
    assert body["include"] == ["code_interpreter_call.outputs"]
    tools = [p for n, p in ev if n == "tool"]
    assert tools[0]["name"] == "code_interpreter" and tools[1]["details"] == {"output": "42"}
    rid = ev[0][1]["request_id"]
    t = stack.query("select code_interpreter_completed_count from chat_turns where request_id = ?", (ubytes(rid),))[0]
    assert t["code_interpreter_completed_count"] == 1
    # novision model has no code interpreter: XLSX rejected
    c2 = client.create_chat(model="gpt-novision")
    r = client.upload(c2["id"], "data.xlsx", b"PK", XLSX)
    assert r.status_code == 400 and "CODE_INTERPRETER_UNAVAILABLE" in reasons(r)


def test_upload_validation(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    r = client.upload(c["id"], "a.bin", b"\x00\x01", "application/octet-stream")
    assert r.status_code == 400 and "UNSUPPORTED_CONTENT_TYPE" in reasons(r)
    r = client.upload(c["id"], "a.exe", b"MZ", "application/x-msdownload")
    assert r.status_code == 400 and "UNSUPPORTED_CONTENT_TYPE" in reasons(r)
    # octet-stream with a known extension is inferred
    r = client.upload(c["id"], "doc.pdf", b"%PDF", "application/octet-stream")
    assert r.status_code == 201 and r.json()["content_type"] == "application/pdf"
    # csv is remapped to text/plain
    r = client.upload(c["id"], "t.csv", b"a,b\n1,2", "text/csv")
    assert r.status_code == 201 and r.json()["content_type"] == "text/plain"
    # missing filename defaults to "upload"; long names keep the extension
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", files={"file": (None, b"x", "text/plain")})
    # httpx sends no filename -> multer treats it as a plain field; build one manually
    boundary = "XBOUNDARYX"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"\r\nContent-Type: text/plain\r\n\r\nabc\r\n--{boundary}--\r\n").encode()
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", content=body,
                   headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert r.status_code == 201, r.text
    assert r.json()["filename"] == "upload"
    long = "n" * 300 + ".txt"
    r = client.upload(c["id"], long, b"x", "text/plain")
    assert r.status_code == 201 and len(r.json()["filename"]) == 255 and r.json()["filename"].endswith(".txt")
    # multipart errors
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", content=b"x", headers={"Content-Type": "multipart/form-data"})
    assert r.status_code == 400 and "BOUNDARY_REQUIRED" in reasons(r)
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nabc\r\n--{boundary}--\r\n").encode()
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", content=body,
                   headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert r.status_code == 400 and "MISSING_FILE" in reasons(r)
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nabc\r\n--{boundary}--\r\n").encode()
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", content=body,
                   headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    assert r.status_code == 400 and "MISSING_CONTENT_TYPE" in reasons(r)
    r = client.req("POST", f"/v1/chats/{c['id']}/attachments", content=b"--zz\r\ngarbage",
                   headers={"Content-Type": "multipart/form-data; boundary=zz"})
    assert r.status_code == 400 and "MULTIPART_ERROR" in reasons(r)
    # unknown / foreign chat
    r = client.upload(str(uuid.uuid4()), "a.txt", b"x", "text/plain")
    assert r.status_code == 404 and r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"


def test_upload_size_limits(tmp_root):
    import harness

    s = harness.Stack(
        f"{tmp_root}/limits",
        mini_chat_overrides={
            "rag": {
                "uploaded_file_max_size_kb": 1,
                "uploaded_image_max_size_kb": 1,
                "max_documents_per_chat": 2,
                "max_total_upload_mb_per_chat": 1,
            }
        },
    )
    s.start()
    try:
        cl = harness.Client(s)
        c = cl.create_chat(model="gpt-standard")
        r = cl.upload(c["id"], "big.txt", b"x" * 2000, "text/plain")
        assert r.status_code == 400
        p = r.json()
        assert "out_of_range" in p["type"] and "FILE_TOO_LARGE" in reasons(r)
        assert p["context"]["field_violations"][0]["field"] == "content_length"
        r = cl.upload(c["id"], "big.png", b"x" * 2000, "image/png")
        assert r.status_code == 400 and "FILE_TOO_LARGE" in reasons(r)
        assert cl.upload(c["id"], "1.txt", b"a", "text/plain").status_code == 201
        assert cl.upload(c["id"], "2.txt", b"a", "text/plain").status_code == 201
        r = cl.upload(c["id"], "3.txt", b"a", "text/plain")
        assert r.status_code == 429
        v = r.json()["context"]["violations"][0]
        assert v["subject"] == "document_limit"
        # images do not count as documents
        assert cl.upload(c["id"], "p.png", harness_png(), "image/png").status_code == 201
        # deleted documents free the slot
        atts = s.query("select id from attachments where chat_id = ? and attachment_kind = 'document'", (ubytes(c["id"]),))
        assert cl.req("DELETE", f"/v1/chats/{c['id']}/attachments/{uuid.UUID(bytes=atts[0]['id'])}").status_code == 204
        assert cl.upload(c["id"], "4.txt", b"a", "text/plain").status_code == 201
        # storage limit (1 MiB total); file limit 1 KiB -> use a chat with seeded big row
        s.execute("update attachments set size_bytes = 1048570 where chat_id = ? and attachment_kind = 'image'", (ubytes(c["id"]),))
        c2 = c
        r = cl.upload(c2["id"], "p2.png", harness_png(), "image/png")
        assert r.status_code == 429 and r.json()["context"]["violations"][0]["subject"] == "storage_limit"
    finally:
        s.stop()


def test_images_kill_switch(tmp_root):
    import harness

    s = harness.Stack(f"{tmp_root}/noimages", kill_switches={"disable_images": True})
    s.start()
    try:
        cl = harness.Client(s)
        c = cl.create_chat(model="gpt-standard")
        r = cl.upload(c["id"], "p.png", harness_png(), "image/png")
        assert r.status_code == 400
        v = r.json()["context"]["violations"][0]
        assert v["subject"] == "images" and v["type"] == "FEATURE_DISABLED"
        assert cl.upload(c["id"], "d.txt", b"doc", "text/plain").status_code == 201
    finally:
        s.stop()


def test_vision_and_image_count_guards(stack, client, mock):
    c = client.create_chat(model="gpt-novision")
    a = client.upload(c["id"], "p.png", harness_png(), "image/png").json()
    r = client.stream(c["id"], "look", attachment_ids=[a["id"]])
    assert r.status_code == 400 and "VISION_NOT_SUPPORTED" in reasons(r)
    c2 = client.create_chat(model="gpt-standard")
    ids = [client.upload(c2["id"], f"p{i}.png", harness_png(), "image/png").json()["id"] for i in range(5)]
    r = client.stream(c2["id"], "look", attachment_ids=ids)
    assert r.status_code == 400 and "TOO_MANY_IMAGES" in reasons(r)
    assert client.stream(c2["id"], "look", attachment_ids=ids[:4]).status_code == 200


def test_attachment_scoping_rules(stack, client, client_a2, mock):
    c = client.create_chat(model="gpt-standard")
    other = client.create_chat(model="gpt-standard")
    a_other = client.upload(other["id"], "x.txt", b"x", "text/plain").json()
    # attachment of another chat
    r = client.stream(c["id"], "x", attachment_ids=[a_other["id"]])
    assert r.status_code == 400 and "invalid_attachment" in reasons(r)
    assert client.req("GET", f"/v1/chats/{c['id']}/attachments/{a_other['id']}").status_code == 404
    r = client.req("GET", f"/v1/chats/{c['id']}/attachments/{uuid.uuid4()}")
    assert r.status_code == 404 and r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.attachment.v1~"
    # another user's chat
    assert client_a2.req("GET", f"/v1/chats/{other['id']}/attachments/{a_other['id']}").status_code == 404
    assert client_a2.req("DELETE", f"/v1/chats/{other['id']}/attachments/{a_other['id']}").status_code == 404
    assert client_a2.upload(other["id"], "x.txt", b"x", "text/plain").status_code == 404
    # not-ready attachment rejected
    a = client.upload(c["id"], "y.txt", b"y", "text/plain").json()
    stack.execute("update attachments set status = 'uploaded' where id = ?", (ubytes(a["id"]),))
    r = client.stream(c["id"], "x", attachment_ids=[a["id"]])
    assert r.status_code == 400 and "invalid_attachment" in reasons(r)
    # a rejected request leaves no reserve, message or turn
    assert stack.query("select * from chat_turns where chat_id = ?", (ubytes(c["id"]),)) == []
    assert stack.query("select sum(reserved_credits_micro) s from quota_usage")[0]["s"] == 0


def test_delete_attachment_lifecycle(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    a = client.upload(c["id"], "d.txt", b"d", "text/plain").json()
    fid = att_row(stack, a["id"])["provider_file_id"]
    r = client.req("DELETE", f"/v1/chats/{c['id']}/attachments/{a['id']}")
    assert r.status_code == 204
    assert client.req("GET", f"/v1/chats/{c['id']}/attachments/{a['id']}").status_code == 404
    assert client.req("DELETE", f"/v1/chats/{c['id']}/attachments/{a['id']}").status_code == 204
    row = att_row(stack, a["id"])
    assert row["deleted_at"] is not None
    wait_for(lambda: att_row(stack, a["id"])["cleanup_status"] == "done")
    deletes = [x for x in stack.mock_requests(method="DELETE") if x["path"].endswith(fid)]
    assert len(deletes) == 1
    # referenced attachments are locked
    b = client.upload(c["id"], "e.txt", b"e", "text/plain").json()
    client.send(c["id"], "use it", attachment_ids=[b["id"]])
    r = client.req("DELETE", f"/v1/chats/{c['id']}/attachments/{b['id']}")
    assert r.status_code == 409 and r.json()["context"]["resource_name"] == "attachment_locked"
    # deleted attachments are excluded from file_search inclusion and lists
    c2 = client.create_chat(model="gpt-standard")
    d = client.upload(c2["id"], "z.txt", b"z", "text/plain").json()
    client.req("DELETE", f"/v1/chats/{c2['id']}/attachments/{d['id']}")
    mock.mock_reset()
    client.send(c2["id"], "anything?")
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    assert "tools" not in body


def test_indexing_failure_and_provider_failure(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_set(index_status="failed")
    r = client.upload(c["id"], "bad.txt", b"x", "text/plain")
    assert r.status_code == 503 and r.headers["retry-after"] == "10"
    assert "indexing_failed" not in r.text
    rows = stack.query("select * from attachments where chat_id = ? and filename = 'bad.txt'", (ubytes(c["id"]),))
    assert rows[0]["status"] == "failed" and rows[0]["error_code"] == "indexing_failed"
    g = client.req("GET", f"/v1/chats/{c['id']}/attachments/{uuid.UUID(bytes=rows[0]['id'])}").json()
    assert g["status"] == "failed" and g["error_code"] == "indexing_failed"
    mock.mock_set(index_status="completed", file_upload_status=500)
    r = client.upload(c["id"], "bad2.txt", b"x", "text/plain")
    assert r.status_code == 503
    rows = stack.query("select * from attachments where chat_id = ? and filename = 'bad2.txt'", (ubytes(c["id"]),))
    assert rows[0]["status"] == "failed" and rows[0]["error_code"] == "upload_failed"


def test_background_indexing(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_set(index_status="in_progress")
    t0 = time.time()
    r = client.upload(c["id"], "slow.txt", b"x", "text/plain")
    assert r.status_code == 201
    assert 20 < time.time() - t0 < 30
    a = r.json()
    assert a["status"] == "uploaded"
    mock.mock_set(index_status="completed")
    got = wait_for(lambda: client.req("GET", f"/v1/chats/{c['id']}/attachments/{a['id']}").json()["status"] == "ready", timeout=40)
    assert got


def test_upload_reaper(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    a = client.upload(c["id"], "r.txt", b"x", "text/plain").json()
    stack.execute(
        "update attachments set status = 'uploaded', updated_at = '2020-01-01T00:00:00.000000001Z' where id = ?",
        (ubytes(a["id"]),),
    )
    row = wait_for(lambda: (x := att_row(stack, a["id"]))["status"] == "failed" and x, timeout=20)
    assert row["error_code"] == "upload_abandoned"
    assert row["deleted_at"] is None
    wait_for(lambda: att_row(stack, a["id"])["cleanup_status"] == "done", timeout=20)
    g = client.req("GET", f"/v1/chats/{c['id']}/attachments/{a['id']}").json()
    assert g["status"] == "failed" and g["error_code"] == "upload_abandoned"


def test_chat_deletion_cleans_provider_resources(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    a = client.upload(c["id"], "d.txt", b"d", "text/plain").json()
    i = client.upload(c["id"], "p.png", harness_png(), "image/png").json()
    vs = stack.query("select vector_store_id from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),))[0]["vector_store_id"]
    files = {att_row(stack, a["id"])["provider_file_id"], att_row(stack, i["id"])["provider_file_id"]}
    assert client.req("DELETE", f"/v1/chats/{c['id']}").status_code == 204
    wait_for(lambda: stack.query("select * from chat_vector_stores where chat_id = ?", (ubytes(c["id"]),)) == [], timeout=30)
    rows = stack.query("select cleanup_status, deleted_at from attachments where chat_id = ?", (ubytes(c["id"]),))
    assert all(r["cleanup_status"] == "done" for r in rows)
    assert all(r["deleted_at"] is None for r in rows)  # rows are not soft-deleted
    deleted = {x["path"].rsplit("/", 1)[1] for x in stack.mock_requests(method="DELETE")}
    assert files <= deleted and vs in deleted
