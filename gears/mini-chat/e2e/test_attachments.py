"""Attachments: upload / get / delete, validation, limits, indexing lifecycle, tools, reaper.

Acceptance criteria covered:
* Attachments — "Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation"
* Attachments — "Asynchronous indexing lifecycle, including provider failure and timeout handling"
* Attachments — "Attachments are correctly made available to the relevant provider tools"
* Attachments — "Cleanup and abandoned-upload recovery behave correctly under failure"
"""

from __future__ import annotations

import base64
import time
import uuid

import pytest

from helpers import (
    MIME_XLSX,
    RT_ATTACHMENT,
    RT_CHAT,
    as_uuid,
    assert_not_found,
    assert_problem,
    attachment_row,
    chat_row,
    input_images,
    list_messages,
    make_pdf,
    make_png,
    make_xlsx,
    new_chat,
    no_provider_ids,
    outbox_mentions,
    raw_multipart,
    rows,
    send_ok,
    shift_ts,
    tenant_id,
    tool,
    ub,
    upload,
    upload_ok,
    user_id,
    vector_store_row,
    wait_until,
)

DETAIL_KEYS = {"id", "filename", "content_type", "size_bytes", "status", "kind", "created_at"}


def _assert_detail(d: dict) -> None:
    assert DETAIL_KEYS <= set(d), d
    for k, v in d.items():
        assert v is not None, f"null field {k} must be omitted"
    assert "provider_file_id" not in d and "vector_store_id" not in d and "storage_backend" not in d
    no_provider_ids(d)


def _files_posts(srv):
    return [r for r in srv.mock_requests("/files", "POST") if "/vector_stores/" not in r["path"]]


def _vs_creates(srv):
    return srv.mock_requests("/vector_stores", "POST")


def _vs_adds(srv):
    return [r for r in srv.mock_requests(method="POST") if "/vector_stores/" in r["path"] and r["path"].endswith("/files")]


# ── documents ─────────────────────────────────────────────────────────────
def test_document_upload_lifecycle(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    pdf = make_pdf("quarterly")
    r = upload(fresh, cid, "Q3 Report.pdf", pdf, "application/pdf")
    assert r.status_code == 201, r.text
    d = r.json()
    _assert_detail(d)
    assert d["status"] == "ready" and d["kind"] == "document"
    assert d["filename"] == "Q3 Report.pdf"
    assert d["content_type"] == "application/pdf"
    assert d["size_bytes"] == len(pdf)
    assert "img_thumbnail" not in d and "doc_summary" not in d and "error_code" not in d
    # Provider side: Files API upload (purpose=assistants), vector store created once and file added.
    ups = _files_posts(fresh)
    assert len(ups) == 1 and ups[0]["body"].get("purpose") == "assistants"
    assert ups[0]["body"]["file"]["size"] == len(pdf)
    assert len(_vs_creates(fresh)) == 1
    adds = _vs_adds(fresh)
    assert len(adds) == 1
    row = attachment_row(fresh, d["id"])
    assert row["provider_file_id"] and row["provider_file_id"].startswith("file-")
    assert adds[0]["body"]["file_id"] == row["provider_file_id"]
    assert (adds[0]["body"].get("attributes") or {}).get("attachment_id") == d["id"]
    vs = vector_store_row(fresh, cid)
    assert vs and vs["vector_store_id"] and vs["vector_store_id"] in adds[0]["path"]
    assert row["attachment_kind"] == "document" and row["for_file_search"] in (1, True)
    assert as_uuid(row["uploaded_by_user_id"]) == user_id("a1")
    # GET returns the same detail.
    g = fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}")
    assert g.status_code == 200, g.text
    assert g.json() == d
    # A second document reuses the chat vector store.
    upload_ok(fresh, cid, "second.pdf")
    assert len(_vs_creates(fresh)) == 1
    assert len(rows(fresh, "SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(cid),))) == 1


def test_image_upload_with_thumbnail(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    png = make_png(200, 100)
    r = upload(fresh, cid, "photo.png", png, "image/png")
    assert r.status_code == 201, r.text
    d = r.json()
    _assert_detail(d)
    assert d["kind"] == "image" and d["status"] == "ready" and d["content_type"] == "image/png"
    th = d["img_thumbnail"]
    assert th["content_type"] == "image/webp"
    assert 0 < th["width"] <= 128 and 0 < th["height"] <= 128
    raw = base64.b64decode(th["data_base64"])
    assert raw[:4] == b"RIFF" and raw[8:12] == b"WEBP"
    assert len(raw) <= 131072
    assert _vs_creates(fresh) == [] and _vs_adds(fresh) == [], "images are never indexed"
    assert len(_files_posts(fresh)) == 1
    g = fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}").json()
    assert g["img_thumbnail"] == th
    # The message attachment summary carries the thumbnail for ready images.
    send_ok(fresh, cid, "see image", attachment_ids=[d["id"]])
    summ = list_messages(fresh, cid)[0]["attachments"]
    assert summ == [{"attachment_id": d["id"], "kind": "image", "filename": "photo.png", "status": "ready", "img_thumbnail": th}]


def test_document_summary_in_message(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    d = upload_ok(fresh, cid, "a.pdf")
    send_ok(fresh, cid, "use doc", attachment_ids=[d["id"]])
    msgs = list_messages(fresh, cid)
    assert len(msgs[0]["attachments"]) == 1
    s = msgs[0]["attachments"][0]
    assert s["attachment_id"] == d["id"] and s["kind"] == "document" and s["filename"] == "a.pdf" and s["status"] == "ready"
    assert "img_thumbnail" not in s or s["img_thumbnail"] is None
    assert msgs[1]["attachments"] == [], "assistant messages have no attachments"


@pytest.mark.parametrize(
    "filename,ctype,expected",
    [
        ("doc.pdf", "application/octet-stream", "application/pdf"),
        ("notes.txt", "text/plain", "text/plain"),
        ("table.csv", "text/csv", "text/plain"),
        ("readme.md", "text/markdown", None),
        ("page.html", "text/html", None),
        ("data.json", "application/json", None),
        ("w.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", None),
        ("s.pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation", None),
    ],
)
def test_supported_document_types(fresh, filename, ctype, expected):
    cid = new_chat(fresh, "gpt-4.1-mini")
    r = upload(fresh, cid, filename, b"some document text\n", ctype)
    assert r.status_code == 201, r.text
    d = r.json()
    assert d["kind"] == "document"
    if expected:
        assert d["content_type"] == expected


TINY_GIF = base64.b64decode("R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==")
TINY_WEBP = base64.b64decode("UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==")
TINY_JPEG = base64.b64decode(
    "/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////"
    "wgALCAABAAEBAREA/8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABPxA="
)


@pytest.mark.parametrize(
    "filename,ctype,data",
    [("img.jpg", "image/jpeg", TINY_JPEG), ("img.webp", "image/webp", TINY_WEBP), ("img.gif", "image/gif", TINY_GIF)],
)
def test_supported_image_types_kind(fresh, filename, ctype, data):
    """JPEG / WEBP / GIF are accepted image types (classified as images)."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    r = upload(fresh, cid, filename, data, ctype)
    if r.status_code == 201:
        assert r.json()["kind"] == "image"
        assert r.json()["content_type"] == ctype
    else:
        # A thumbnail decoder may refuse tiny hand-made images; the type itself must never be unsupported.
        assert r.status_code == 400, r.text
        assert "UNSUPPORTED_CONTENT_TYPE" not in r.text


def test_unsupported_type_rejected(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    for name, ctype in (("a.exe", "application/x-msdownload"), ("blob.bin", "application/octet-stream"), ("v.mp4", "video/mp4")):
        r = upload(fresh, cid, name, b"MZ\x00\x00", ctype)
        assert_problem(r, 400, field_reason="UNSUPPORTED_CONTENT_TYPE")
    assert _files_posts(fresh) == []


def test_filename_truncated_keeping_extension(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    long_name = "n" * 300 + ".txt"
    d = upload_ok(fresh, cid, long_name, b"text", "text/plain")
    assert len(d["filename"]) == 255
    assert d["filename"].endswith(".txt")


def test_filename_defaults_to_upload(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    boundary = "xBOUNDARYx"
    body = (
        f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"\r\nContent-Type: text/plain\r\n\r\n"
        "plain text body\r\n"
        f"--{boundary}--\r\n"
    ).encode()
    r = raw_multipart(fresh, cid, body, f"multipart/form-data; boundary={boundary}")
    assert r.status_code == 201, r.text
    assert r.json()["filename"] == "upload"


def test_multipart_validation(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    # No boundary.
    r = raw_multipart(fresh, cid, b"whatever", "multipart/form-data")
    assert_problem(r, 400, field_reason="BOUNDARY_REQUIRED")
    # No file field.
    b = "bnd123"
    body = f"--{b}\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nvalue\r\n--{b}--\r\n".encode()
    r = raw_multipart(fresh, cid, body, f"multipart/form-data; boundary={b}")
    assert_problem(r, 400, field_reason="MISSING_FILE")
    # File part without content type.
    body = f"--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello\r\n--{b}--\r\n".encode()
    r = raw_multipart(fresh, cid, body, f"multipart/form-data; boundary={b}")
    assert_problem(r, 400, field_reason="MISSING_CONTENT_TYPE")
    # Unreadable multipart body.
    body = f"--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nunterminated".encode()
    r = raw_multipart(fresh, cid, body, f"multipart/form-data; boundary={b}")
    assert_problem(r, 400, field_reason="MULTIPART_ERROR")
    assert _files_posts(fresh) == []


def test_upload_into_unknown_foreign_or_deleted_chat(fresh):
    assert_not_found(upload(fresh, str(uuid.uuid4()), "a.pdf", make_pdf(), "application/pdf"), RT_CHAT)
    cid_b = new_chat(fresh, "gpt-4.1-mini", user="b")
    assert_not_found(upload(fresh, cid_b, "a.pdf", make_pdf(), "application/pdf", user="a1"), RT_CHAT)
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.req("DELETE", f"/chats/{cid}")
    assert_not_found(upload(fresh, cid, "a.pdf", make_pdf(), "application/pdf"), RT_CHAT)
    assert _files_posts(fresh) == []


def test_upload_model_removed_from_catalog(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.execute("UPDATE chats SET model = 'ghost-model' WHERE id = ?", (ub(cid),))
    r = upload(fresh, cid, "a.pdf", make_pdf(), "application/pdf")
    assert_problem(r, 400, field_reason="INVALID_MODEL")
    assert _files_posts(fresh) == []


# ── size and per-chat limits (limits server) ──────────────────────────────
def test_file_too_large(lim):
    cid = new_chat(lim, "gpt-4.1-mini")
    r = upload(lim, cid, "big.pdf", b"%PDF" + b"0" * (800 * 1024), "application/pdf")  # > 700 KB
    assert_problem(r, 400, field_reason="FILE_TOO_LARGE", field="content_length")
    r = upload(lim, cid, "big.png", b"\x89PNG" + b"0" * (100 * 1024), "image/png")  # > 64 KB images
    assert_problem(r, 400, field_reason="FILE_TOO_LARGE")
    assert _files_posts(lim) == []
    # Model limit: max_file_size_mb (25) caps the gear limit; a small file passes.
    upload_ok(lim, cid, "ok.pdf", b"%PDF" + b"0" * 1024)


def test_document_count_limit(lim):
    cid = new_chat(lim, "gpt-4.1-mini")
    for i in range(3):
        upload_ok(lim, cid, f"d{i}.pdf")
    r = upload(lim, cid, "d3.pdf", make_pdf(), "application/pdf")
    j = assert_problem(r, 429)
    assert "document_limit" in str(j["context"]), j
    # Deleting one frees a slot.
    first = rows(lim, "SELECT id FROM attachments WHERE chat_id = ? AND deleted_at IS NULL ORDER BY created_at LIMIT 1", (ub(cid),))[0]
    assert lim.req("DELETE", f"/chats/{cid}/attachments/{as_uuid(first['id'])}").status_code == 204
    upload_ok(lim, cid, "d4.pdf")


def test_storage_limit(lim):
    cid = new_chat(lim, "gpt-4.1-mini")
    blob = b"%PDF" + b"1" * (600 * 1024)
    statuses = []
    for i in range(3):
        r = upload(lim, cid, f"s{i}.pdf", blob, "application/pdf")
        statuses.append(r.status_code)
        if r.status_code == 429:
            j = assert_problem(r, 429)
            assert "storage_limit" in str(j["context"]), j
            break
    assert statuses[0] == 201 and statuses[-1] == 429, statuses


def test_failed_uploads_do_not_count(lim):
    cid = new_chat(lim, "gpt-4.1-mini")
    lim.mock_config(file_upload_status=500)
    for i in range(3):
        assert upload(lim, cid, f"f{i}.pdf", make_pdf(), "application/pdf").status_code == 503
    lim.mock_config(file_upload_status=200)
    for i in range(3):
        upload_ok(lim, cid, f"ok{i}.pdf")


# ── kill switches / capabilities ──────────────────────────────────────────
def test_image_upload_disabled_by_kill_switch(ks):
    cid = new_chat(ks, "gpt-4.1-mini")
    r = upload(ks, cid, "p.png", make_png(), "image/png")
    assert_problem(r, 400, subject="images", vtype="FEATURE_DISABLED")
    assert _files_posts(ks) == []


def test_xlsx_requires_code_interpreter(fresh, ks):
    cid = new_chat(fresh, "std-novision")
    r = upload(fresh, cid, "t.xlsx", make_xlsx(), MIME_XLSX)
    assert_problem(r, 400, field_reason="CODE_INTERPRETER_UNAVAILABLE", resource_type=RT_ATTACHMENT)
    cid2 = new_chat(ks, "gpt-4.1-mini")
    r = upload(ks, cid2, "t.xlsx", make_xlsx(), MIME_XLSX)
    assert_problem(r, 400, field_reason="CODE_INTERPRETER_UNAVAILABLE")


# ── provider failures / indexing ──────────────────────────────────────────
def test_provider_upload_failure(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_config(file_upload_status=500)
    r = upload(fresh, cid, "x.pdf", make_pdf(), "application/pdf")
    j = assert_problem(r, 503)
    assert r.headers.get("retry-after") == "10"
    no_provider_ids(r.text)
    rs = rows(fresh, "SELECT * FROM attachments WHERE chat_id = ?", (ub(cid),))
    assert len(rs) == 1 and rs[0]["status"] == "failed" and rs[0]["error_code"]
    g = fresh.req("GET", f"/chats/{cid}/attachments/{as_uuid(rs[0]['id'])}")
    assert g.status_code == 200, g.text
    assert g.json()["status"] == "failed" and g.json()["error_code"] == rs[0]["error_code"]
    no_provider_ids(g.json())


def test_indexing_failure(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_config(vector_store_file_status="failed")
    r = upload(fresh, cid, "x.pdf", make_pdf(), "application/pdf")
    assert_problem(r, 503)
    assert r.headers.get("retry-after") == "10"
    rs = rows(fresh, "SELECT * FROM attachments WHERE chat_id = ?", (ub(cid),))
    assert rs[0]["status"] == "failed" and rs[0]["error_code"] == "indexing_failed"
    g = fresh.req("GET", f"/chats/{cid}/attachments/{as_uuid(rs[0]['id'])}").json()
    assert g["status"] == "failed" and g["error_code"] == "indexing_failed"
    # Not ready → not usable in a message, and no file_search tool.
    fresh.mock_reset()
    r, _ = fresh.stream(cid, "x", attachment_ids=[g["id"]])
    assert_problem(r, 400, field_reason="invalid_attachment")


def test_indexing_still_running_returns_uploaded_then_ready(fresh):
    """Indexing in progress at the 25 s deadline: 201 uploaded; background polling makes it ready."""
    cid = new_chat(fresh, "gpt-4.1-mini")
    fresh.mock_config(vector_store_file_status="in_progress")
    t0 = time.monotonic()
    r = upload(fresh, cid, "slow.pdf", make_pdf(), "application/pdf")
    elapsed = time.monotonic() - t0
    assert r.status_code == 201, r.text
    d = r.json()
    assert d["status"] == "uploaded"
    assert 20 <= elapsed < 30, elapsed
    assert fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}").json()["status"] == "uploaded"
    # Not ready yet → rejected as a message attachment.
    r2, _ = fresh.stream(cid, "x", attachment_ids=[d["id"]])
    assert_problem(r2, 400, field_reason="invalid_attachment")
    fresh.mock_config(vector_store_file_status="completed")
    ok = wait_until(lambda: fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}").json()["status"] == "ready", timeout=60, interval=1)
    assert ok, fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}").text


# ── get / delete ──────────────────────────────────────────────────────────
def test_get_attachment_not_found_cases(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    other = new_chat(fresh, "gpt-4.1-mini")
    d = upload_ok(fresh, cid)
    assert_not_found(fresh.req("GET", f"/chats/{cid}/attachments/{uuid.uuid4()}"), RT_ATTACHMENT)
    assert_not_found(fresh.req("GET", f"/chats/{other}/attachments/{d['id']}"), RT_ATTACHMENT)
    assert_not_found(fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}", "b"), RT_CHAT)
    # Uploaded by another user (in the caller's chat) is indistinguishable from unknown.
    fresh.execute("UPDATE attachments SET uploaded_by_user_id = ? WHERE id = ?", (ub(user_id("a2")), ub(d["id"])))
    assert_not_found(fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}"), RT_ATTACHMENT)
    assert_not_found(fresh.req("DELETE", f"/chats/{cid}/attachments/{d['id']}"), RT_ATTACHMENT)
    assert_problem(fresh.req("GET", f"/chats/{cid}/attachments/not-a-uuid"), 400)


def test_delete_attachment_and_cleanup(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    d = upload_ok(fresh, cid)
    file_id = attachment_row(fresh, d["id"])["provider_file_id"]
    r = fresh.req("DELETE", f"/chats/{cid}/attachments/{d['id']}")
    assert r.status_code == 204, r.text
    row = attachment_row(fresh, d["id"])
    assert row["deleted_at"] is not None
    assert row["cleanup_status"] in ("pending", "done")
    assert_not_found(fresh.req("GET", f"/chats/{cid}/attachments/{d['id']}"), RT_ATTACHMENT)
    msgs = wait_until(lambda: outbox_mentions(fresh, d["id"]), timeout=5)
    assert msgs and any("attachment_deleted" in str(m["payload"]) for m in msgs)
    n_msgs = len(msgs)
    # The provider file is deleted by the attachment cleanup handler.
    assert wait_until(lambda: [x for x in fresh.mock_requests(f"/files/{file_id}", "DELETE")], timeout=15), "no provider file DELETE"
    assert wait_until(lambda: attachment_row(fresh, d["id"])["cleanup_status"] == "done", timeout=15)
    # Repeated delete: 204, no new outbox message.
    assert fresh.req("DELETE", f"/chats/{cid}/attachments/{d['id']}").status_code == 204
    time.sleep(0.5)
    assert len(outbox_mentions(fresh, d["id"])) == n_msgs


def test_delete_referenced_attachment_locked(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    d = upload_ok(fresh, cid)
    send_ok(fresh, cid, "uses it", attachment_ids=[d["id"]])
    r = fresh.req("DELETE", f"/chats/{cid}/attachments/{d['id']}")
    assert_problem(r, 409, resource_name="attachment_locked")
    assert attachment_row(fresh, d["id"])["deleted_at"] is None


# ── tools ─────────────────────────────────────────────────────────────────
def test_file_search_tool_after_upload(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    send_ok(fresh, cid, "before upload")
    assert tool(fresh.chat_requests()[-1], "file_search") is None, "no file_search without documents"
    upload_ok(fresh, cid, "kb.pdf")
    vs_id = vector_store_row(fresh, cid)["vector_store_id"]
    fresh.mock_reset()
    send_ok(fresh, cid, "question about the doc")
    body = fresh.chat_requests()[-1]
    fs = tool(body, "file_search")
    assert fs is not None, body.get("tools")
    assert fs["vector_store_ids"] == [vs_id]
    assert fs.get("max_num_results") == 5
    assert body.get("max_tool_calls") == 10
    assert len(body["instructions"]) > len("You are a helpful assistant."), "file search guard appended"


def test_file_search_not_sent_without_support(fresh, ks):
    cid = new_chat(fresh, "std-novision")
    upload_ok(fresh, cid, "kb.pdf")
    fresh.mock_reset()
    send_ok(fresh, cid, "q")
    assert tool(fresh.chat_requests()[-1], "file_search") is None
    # Kill switch disable_file_search.
    cid2 = new_chat(ks, "gpt-4.1-mini")
    upload_ok(ks, cid2, "kb.pdf")
    ks.mock_reset()
    send_ok(ks, cid2, "q")
    assert tool(ks.chat_requests()[-1], "file_search") is None


def test_images_only_do_not_enable_file_search(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    upload_ok(fresh, cid, "p.png", make_png(), "image/png")
    fresh.mock_reset()
    send_ok(fresh, cid, "q")
    assert tool(fresh.chat_requests()[-1], "file_search") is None


def test_image_sent_only_on_its_turn(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    img = upload_ok(fresh, cid, "p.png", make_png(), "image/png")
    file_id = attachment_row(fresh, img["id"])["provider_file_id"]
    fresh.mock_reset()
    send_ok(fresh, cid, "what is in the image?", attachment_ids=[img["id"]])
    body = fresh.chat_requests()[-1]
    imgs = input_images(body)
    assert [i.get("file_id") for i in imgs] == [file_id]
    last = body["input"][-1]
    assert last.get("role") == "user"
    assert any(p.get("type") == "input_text" and p.get("text") == "what is in the image?" for p in last["content"])
    send_ok(fresh, cid, "and now?")
    assert input_images(fresh.chat_requests()[-1]) == [], "images are never implicitly reused"
    # Explicit re-attachment includes it again.
    send_ok(fresh, cid, "again", attachment_ids=[img["id"]])
    assert [i.get("file_id") for i in input_images(fresh.chat_requests()[-1])] == [file_id]


def test_code_interpreter_tool_for_xlsx(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    x = upload_ok(fresh, cid, "sheet.xlsx", make_xlsx(), MIME_XLSX)
    assert x["kind"] == "document"
    file_id = attachment_row(fresh, x["id"])["provider_file_id"]
    fresh.mock_reset()
    send_ok(fresh, cid, "sum column A")
    body = fresh.chat_requests()[-1]
    ci = tool(body, "code_interpreter")
    assert ci is not None, body.get("tools")
    assert ci["container"]["type"] == "auto"
    assert file_id in ci["container"]["file_ids"]
    assert "code_interpreter_call.outputs" in (body.get("include") or [])
    assert body.get("max_tool_calls") == 10


# ── abandoned uploads ─────────────────────────────────────────────────────
def _insert_attachment(srv, cid, status, provider_file_id=None, age_secs=600):
    sample = chat_row(srv, cid)["created_at"]
    old = shift_ts(sample, -age_secs)
    att_id = str(uuid.uuid4())
    srv.execute(
        "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, content_type, size_bytes, "
        "storage_backend, provider_file_id, status, attachment_kind, for_file_search, created_at, updated_at) "
        "VALUES (?, ?, ?, ?, ?, 'application/pdf', 10, 'openai', ?, ?, 'document', 1, ?, ?)",
        (ub(att_id), ub(tenant_id("a1")), ub(cid), ub(user_id("a1")), f"{status}.pdf", provider_file_id, status, old, old),
    )
    return att_id


def test_upload_reaper_marks_abandoned_uploads(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    pending = _insert_attachment(fresh, cid, "pending")
    uploaded = _insert_attachment(fresh, cid, "uploaded", provider_file_id="file-abandonedabc123456789")
    young = _insert_attachment(fresh, cid, "pending", age_secs=0)

    def reaped():
        a, b = attachment_row(fresh, pending), attachment_row(fresh, uploaded)
        return a["status"] == "failed" and b["status"] == "failed"

    assert wait_until(reaped, timeout=15), (attachment_row(fresh, pending), attachment_row(fresh, uploaded))
    a = attachment_row(fresh, pending)
    assert a["error_code"] == "upload_abandoned" and a["deleted_at"] is None
    assert a["cleanup_status"] is None, "no provider file → no cleanup"
    b = attachment_row(fresh, uploaded)
    assert b["error_code"] == "upload_abandoned"
    assert b["cleanup_status"] in ("pending", "done")
    assert attachment_row(fresh, young)["status"] == "pending", "fresh uploads are not reaped"
    # Visible via GET as failed.
    g = fresh.req("GET", f"/chats/{cid}/attachments/{pending}").json()
    assert g["status"] == "failed" and g["error_code"] == "upload_abandoned"
    # The provider file of the abandoned upload is deleted.
    assert wait_until(lambda: fresh.mock_requests("/files/file-abandonedabc123456789", "DELETE"), timeout=15)
    assert wait_until(lambda: attachment_row(fresh, uploaded)["cleanup_status"] == "done", timeout=15)


def test_reaper_skips_rows_owned_by_chat_cleanup(fresh):
    cid = new_chat(fresh, "gpt-4.1-mini")
    att = _insert_attachment(fresh, cid, "pending")
    fresh.execute("UPDATE attachments SET cleanup_status = 'pending' WHERE id = ?", (ub(att),))
    time.sleep(3)
    assert attachment_row(fresh, att)["status"] == "pending"
