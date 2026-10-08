"""Attachments: upload / get / delete, limits, indexing lifecycle, provider tools
(acceptance: Attachments; Streaming: preflight validation)."""

import base64
import json
import time
import uuid

import pytest

from mchelpers import (
    MIME_PDF,
    MIME_PNG,
    MIME_XLSX,
    RT_ATTACHMENT,
    RT_CHAT,
    as_uuid,
    assert_problem,
    find_tool,
    make_pdf,
    make_png,
    make_xlsx,
    nonce,
    ub,
    wait_for,
)

DETAIL_REQUIRED = {"id", "filename", "content_type", "size_bytes", "status", "kind", "created_at"}
DETAIL_ALLOWED = DETAIL_REQUIRED | {"error_code", "doc_summary", "img_thumbnail", "summary_updated_at"}


def _no_provider_ids(mock_llm, payload):
    text = json.dumps(payload)
    for fid in mock_llm.issued_file_ids():
        assert fid not in text, f"provider file id leaked: {fid}"
    for vs in mock_llm.issued_vector_store_ids():
        assert vs not in text, f"vector store id leaked: {vs}"


# Acceptance: Attachments — document upload lifecycle (pending -> uploaded -> ready), no provider ids
def test_upload_pdf_ready(api, mock_llm, db):
    c = api.create_chat()
    data = make_pdf("ready " + nonce())
    r = api.upload(c["id"], "Report.pdf", data, MIME_PDF)
    assert r.status_code == 201, r.text
    a = r.json()
    assert DETAIL_REQUIRED <= set(a) and set(a) <= DETAIL_ALLOWED, a
    assert a["status"] == "ready" and a["kind"] == "document"
    assert a["filename"] == "Report.pdf" and a["content_type"] == MIME_PDF
    assert a["size_bytes"] == len(data)
    assert not a.get("error_code") and not a.get("img_thumbnail") and not a.get("doc_summary")
    _no_provider_ids(mock_llm, a)
    # provider side: Files API with purpose=assistants, vector store add with attachment_id attribute
    row = db.attachment_row(a["id"])
    pfid = row["provider_file_id"]
    assert pfid and pfid in mock_llm.issued_file_ids()
    up = [q for q in mock_llm.requests(path_contains="/files", method="POST") if q.get("file_id") == pfid]
    assert up and up[0]["form"].get("purpose") == "assistants"
    adds = [q for q in mock_llm.requests(path_contains="/vector_stores/", method="POST") if (q.get("json") or {}).get("file_id") == pfid]
    assert adds, "document must be added to the chat vector store"
    attrs = adds[0]["json"].get("attributes") or {}
    assert str(attrs.get("attachment_id")).replace("-", "") == a["id"].replace("-", "")
    vs_rows = db.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),))
    assert len(vs_rows) == 1 and vs_rows[0]["vector_store_id"] in mock_llm.issued_vector_store_ids()
    assert row["for_file_search"] in (1, True) and row["attachment_kind"] == "document"
    # GET returns the same projection
    g = api.attachment(c["id"], a["id"])
    assert g.status_code == 200 and g.json()["status"] == "ready" and g.json()["id"] == a["id"]
    _no_provider_ids(mock_llm, g.json())


# Acceptance: Attachments — one vector store per chat, reused for later documents
def test_single_vector_store_per_chat(api, db, mock_llm):
    c = api.create_chat()
    api.upload_ready(c["id"], "one.txt", b"first document", "text/plain")
    api.upload_ready(c["id"], "two.txt", b"second document", "text/plain")
    rows = db.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),))
    assert len(rows) == 1


# Acceptance: Attachments — image upload: ready, kind image, WebP thumbnail, not indexed
def test_upload_image_with_thumbnail(api, mock_llm, db):
    c = api.create_chat()
    png = make_png(300, 150)
    a = api.upload_ready(c["id"], "photo.png", png, MIME_PNG)
    assert a["kind"] == "image" and a["content_type"] == MIME_PNG
    th = a.get("img_thumbnail")
    assert th, "a valid PNG gets a thumbnail"
    assert th["content_type"] == "image/webp"
    assert 0 < th["width"] <= 128 and 0 < th["height"] <= 128
    assert th["width"] >= th["height"]  # aspect ratio preserved (2:1)
    raw = base64.b64decode(th["data_base64"])
    assert raw[:4] == b"RIFF" and raw[8:12] == b"WEBP"
    pfid = db.attachment_row(a["id"])["provider_file_id"]
    adds = [q for q in mock_llm.requests(path_contains="/vector_stores/", method="POST") if (q.get("json") or {}).get("file_id") == pfid]
    assert adds == [], "images are never indexed"
    _no_provider_ids(mock_llm, a)


# Acceptance: Attachments — XLSX is code-interpreter only: ready, not indexed
def test_upload_xlsx_ready_not_indexed(api, mock_llm, db):
    c = api.create_chat()
    a = api.upload_ready(c["id"], "data.xlsx", make_xlsx(), MIME_XLSX)
    assert a["kind"] == "document"
    row = db.attachment_row(a["id"])
    assert row["for_code_interpreter"] in (1, True) and row["for_file_search"] in (0, False)
    adds = [q for q in mock_llm.requests(path_contains="/vector_stores/", method="POST") if (q.get("json") or {}).get("file_id") == row["provider_file_id"]]
    assert adds == []


# Acceptance: Attachments — XLSX on a model without code interpreter
def test_upload_xlsx_without_code_interpreter(api):
    c = api.create_chat(model="std-novision")
    r = api.upload(c["id"], "data.xlsx", make_xlsx(), MIME_XLSX)
    assert_problem(r, 400, "invalid_argument", field_reason="CODE_INTERPRETER_UNAVAILABLE")


# Acceptance: Attachments — type validation (allowlist, octet-stream inference, CSV remap)
def test_upload_type_validation(api):
    c = api.create_chat()
    r = api.upload(c["id"], "evil.exe", b"MZ\x90\x00", "application/x-msdownload")
    assert_problem(r, 400, "invalid_argument", field_reason="UNSUPPORTED_CONTENT_TYPE")
    r = api.upload(c["id"], "blob.xyz", b"???", "application/octet-stream")
    assert_problem(r, 400, "invalid_argument", field_reason="UNSUPPORTED_CONTENT_TYPE")
    r = api.upload(c["id"], "inferred.pdf", make_pdf(), "application/octet-stream")
    assert r.status_code == 201, r.text
    assert r.json()["content_type"] == MIME_PDF
    r = api.upload(c["id"], "table.csv", b"a,b\n1,2\n", "text/csv")
    assert r.status_code == 201, r.text
    assert r.json()["content_type"] == "text/plain"


# Acceptance: Attachments — multipart validation reasons
def test_upload_multipart_errors(api):
    c = api.create_chat()
    # no boundary
    r = api.upload_raw(c["id"], b"whatever", "multipart/form-data")
    assert_problem(r, 400, "invalid_argument", field_reason="BOUNDARY_REQUIRED")
    # no `file` field
    body = b'--XB\r\nContent-Disposition: form-data; name="other"\r\n\r\nvalue\r\n--XB--\r\n'
    r = api.upload_raw(c["id"], body, "multipart/form-data; boundary=XB")
    assert_problem(r, 400, "invalid_argument", field_reason="MISSING_FILE")
    # `file` part without a content type
    body = b'--XB\r\nContent-Disposition: form-data; name="file"; filename="a.txt"\r\n\r\nhello\r\n--XB--\r\n'
    r = api.upload_raw(c["id"], body, "multipart/form-data; boundary=XB")
    assert_problem(r, 400, "invalid_argument", field_reason="MISSING_CONTENT_TYPE")
    # unreadable multipart body
    r = api.upload_raw(c["id"], b"--XB\r\nContent-Disposition: garbage", "multipart/form-data; boundary=XB")
    assert r.status_code == 400, r.text


# Acceptance: Attachments — filename defaults to "upload" and is truncated to 255 keeping the extension
def test_upload_filename_rules(api):
    c = api.create_chat()
    body = b'--XB\r\nContent-Disposition: form-data; name="file"\r\nContent-Type: text/plain\r\n\r\nno name\r\n--XB--\r\n'
    r = api.upload_raw(c["id"], body, "multipart/form-data; boundary=XB")
    assert r.status_code == 201, r.text
    assert r.json()["filename"] == "upload"
    long_name = "n" * 300 + ".txt"
    r = api.upload(c["id"], long_name, b"long name", "text/plain")
    assert r.status_code == 201, r.text
    fn = r.json()["filename"]
    assert len(fn) == 255 and fn.endswith(".txt")


# Acceptance: Attachments — size limits (documents and images)
def test_upload_too_large(api):
    c = api.create_chat()
    r = api.upload(c["id"], "big.txt", b"a" * (2048 * 1024 + 10), "text/plain")
    assert_problem(r, 400, "out_of_range", field_reason="FILE_TOO_LARGE")
    r = api.upload(c["id"], "big.png", make_png(8, 8) + b"\x00" * (256 * 1024 + 10), MIME_PNG)
    assert_problem(r, 400, "out_of_range", field_reason="FILE_TOO_LARGE")


# Acceptance: Attachments — per-chat document count limit
def test_document_count_limit(api):
    c = api.create_chat()
    for i in range(3):
        api.upload_ready(c["id"], f"d{i}.txt", f"doc {i}".encode(), "text/plain")
    r = api.upload(c["id"], "d3.txt", b"one too many", "text/plain")
    assert_problem(r, 429, "resource_exhausted", violation_subject="document_limit")


# Acceptance: Attachments — per-chat total size limit
def test_storage_limit(api):
    c = api.create_chat()
    blob = b"s" * (1900 * 1024)
    api.upload_ready(c["id"], "s1.txt", blob, "text/plain")
    api.upload_ready(c["id"], "s2.txt", blob, "text/plain")
    r = api.upload(c["id"], "s3.txt", blob, "text/plain")
    assert_problem(r, 429, "resource_exhausted", violation_subject="storage_limit")


# Acceptance: Attachments — upload into unknown / foreign chat
def test_upload_unknown_chat(api, api_for):
    r = api.upload(str(uuid.uuid4()), "a.txt", b"x", "text/plain")
    assert_problem(r, 404, "not_found", resource_type=RT_CHAT)
    c = api.create_chat()
    other = api_for("tok-b")
    assert_problem(other.upload(c["id"], "a.txt", b"x", "text/plain"), 404, "not_found", resource_type=RT_CHAT)


# Acceptance: Attachments — provider failure: 503 + Retry-After 10, row failed with error_code
def test_provider_upload_failure(api, mock_llm, db):
    c = api.create_chat()
    mock_llm.configure(files_fail=True)
    r = api.upload(c["id"], "fail.pdf", make_pdf(), MIME_PDF)
    assert_problem(r, 503, "service_unavailable")
    assert r.headers.get("retry-after") == "10"
    rows = db.attachments_of_chat(c["id"])
    assert len(rows) == 1
    assert rows[0]["status"] == "failed" and rows[0]["error_code"]
    g = api.attachment(c["id"], as_uuid(rows[0]["id"]))
    assert g.status_code == 200
    assert g.json()["status"] == "failed" and g.json()["error_code"]


# Acceptance: Attachments — indexing failure: 503, indexing_failed, provider file deleted
def test_indexing_failed(api, mock_llm, db):
    c = api.create_chat()
    mock_llm.configure(index_status="failed")
    r = api.upload(c["id"], "bad.pdf", make_pdf(), MIME_PDF)
    assert_problem(r, 503, "service_unavailable")
    assert r.headers.get("retry-after") == "10"
    assert "indexing_failed" not in r.text
    row = db.attachments_of_chat(c["id"])[0]
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    pfid = row["provider_file_id"]
    if pfid:
        wait_for(
            lambda: mock_llm.requests(path_contains=f"/files/{pfid}", method="DELETE"),
            timeout=20,
            desc="provider file delete after indexing failure",
        )
    g = api.attachment(c["id"], as_uuid(row["id"])).json()
    assert g["status"] == "failed" and g["error_code"] == "indexing_failed"


# Acceptance: Attachments — indexing still in progress at the deadline -> 201 uploaded, then ready
@pytest.mark.slow
@pytest.mark.timeout(150)
def test_indexing_in_progress_then_ready(api, mock_llm):
    c = api.create_chat()
    mock_llm.configure(index_status="in_progress")
    t0 = time.time()
    r = api.upload(c["id"], "slow.pdf", make_pdf(), MIME_PDF, timeout=60)
    elapsed = time.time() - t0
    assert r.status_code == 201, r.text
    a = r.json()
    assert a["status"] == "uploaded"
    assert 20 <= elapsed <= 30, f"upload waited {elapsed:.1f}s (deadline is 25 s after start)"
    # a not-ready attachment cannot be referenced
    s = api.stream(c["id"], "use it " + nonce(), attachment_ids=[a["id"]])
    assert_problem(api.as_response(s), 400, "invalid_argument", field_reason="invalid_attachment")
    mock_llm.configure(poll_status="completed")
    wait_for(lambda: api.attachment(c["id"], a["id"]).json()["status"] == "ready", timeout=90, interval=1, desc="background indexing")


# Acceptance: Streaming preflight / Attachments — not-ready attachment: 400, no stream, no turn, no provider call
def test_not_ready_attachment_rejected_before_provider(api, mock_llm, db):
    c = api.create_chat()
    mock_llm.configure(files_fail=True)
    api.upload(c["id"], "broken.pdf", make_pdf(), MIME_PDF)
    mock_llm.configure(files_fail=None)
    failed_id = as_uuid(db.attachments_of_chat(c["id"])[0]["id"])
    n = nonce()
    s = api.stream(c["id"], f"use failed {n}", attachment_ids=[failed_id])
    assert not s.is_sse
    assert_problem(api.as_response(s), 400, "invalid_argument", field_reason="invalid_attachment", field="attachment")
    assert db.turns(c["id"]) == []
    assert mock_llm.chat_requests(contains=n) == []
    assert api.messages(c["id"]) == []


# Acceptance: Streaming preflight — duplicate, foreign, unknown attachment ids
def test_invalid_attachment_ids(api, mock_llm):
    c = api.create_chat()
    other_chat = api.create_chat()
    a = api.upload_ready(c["id"], "ok.txt", b"ok", "text/plain")
    foreign = api.upload_ready(other_chat["id"], "foreign.txt", b"foreign", "text/plain")
    n = nonce()
    for ids in ([a["id"], a["id"]], [foreign["id"]], [str(uuid.uuid4())]):
        s = api.stream(c["id"], f"bad ids {n}", attachment_ids=ids)
        assert_problem(api.as_response(s), 400, "invalid_argument", field_reason="invalid_attachment")
    assert mock_llm.chat_requests(contains=n) == []


# Acceptance: Streaming preflight — image guards (count, vision)
def test_image_guards(api, mock_llm):
    c = api.create_chat()
    imgs = [api.upload_ready(c["id"], f"i{i}.png", make_png(10 + i, 10), MIME_PNG)["id"] for i in range(3)]
    n = nonce()
    s = api.stream(c["id"], f"three images {n}", attachment_ids=imgs)
    assert_problem(api.as_response(s), 400, "out_of_range", field_reason="TOO_MANY_IMAGES", field="image_count")
    nv = api.create_chat(model="std-novision")
    img = api.upload_ready(nv["id"], "v.png", make_png(), MIME_PNG)
    s = api.stream(nv["id"], f"vision {n}", attachment_ids=[img["id"]])
    assert_problem(api.as_response(s), 400, "invalid_argument", field_reason="VISION_NOT_SUPPORTED")
    assert mock_llm.chat_requests(contains=n) == []


# Acceptance: Attachments — images are sent as input_image for that turn only; message lists the attachment
def test_image_in_provider_request(api, mock_llm, db):
    c = api.create_chat()
    img = api.upload_ready(c["id"], "pic.png", make_png(), MIME_PNG)
    pfid = db.attachment_row(img["id"])["provider_file_id"]
    n = nonce()
    s = api.stream(c["id"], f"describe {n}", attachment_ids=[img["id"]])
    assert s.done
    req = mock_llm.chat_requests(contains=n)[-1]["json"]
    user_items = [it for it in req["input"] if isinstance(it, dict) and it.get("role") == "user"]
    parts = user_items[-1]["content"]
    assert isinstance(parts, list)
    assert {"type": "input_image", "file_id": pfid} in [{k: p.get(k) for k in ("type", "file_id")} for p in parts]
    # the next turn does not implicitly re-send the image
    m = nonce()
    assert api.stream(c["id"], f"follow-up {m}").done
    nxt = mock_llm.chat_requests(contains=m)[-1]
    last_user = [it for it in nxt["json"]["input"] if isinstance(it, dict) and it.get("role") == "user"][-1]
    assert pfid not in json.dumps(last_user)
    # message attachments
    user_msg = [x for x in api.messages(c["id"]) if x["role"] == "user"][0]
    assert len(user_msg["attachments"]) == 1
    summ = user_msg["attachments"][0]
    assert summ["attachment_id"] == img["id"] and summ["kind"] == "image" and summ["filename"] == "pic.png"
    assert summ["status"] == "ready"
    assert summ.get("img_thumbnail") and summ["img_thumbnail"]["content_type"] == "image/webp"


# Acceptance: Attachments — file_search tool only with a ready document; file citations mapped
def test_file_search_tool_and_file_citation(api, mock_llm, db):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"no docs yet {n}").done
    assert find_tool(mock_llm.chat_requests(contains=n)[-1], "file_search") is None
    doc = api.upload_ready(c["id"], "Q3 Report.pdf", make_pdf("q3"), MIME_PDF)
    vs = db.one("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(c["id"]),))["vector_store_id"]
    m = nonce()
    s = api.stream(c["id"], f"cite the doc [[filecite]] {m}")
    tool = find_tool(mock_llm.chat_requests(contains=m)[-1], "file_search")
    assert tool is not None
    assert tool.get("vector_store_ids") == [vs]
    assert tool.get("max_num_results") == 5
    tools = [(t["phase"], t["name"]) for t in s.of("tool")]
    assert ("start", "file_search") in tools and ("done", "file_search") in tools
    done_tool = [t for t in s.of("tool") if t["name"] == "file_search" and t["phase"] == "done"][0]
    assert done_tool["details"].get("files_searched") == 0
    cits = s.of("citations")
    assert len(cits) == 1
    item = [i for i in cits[0]["items"] if i["source"] == "file"][0]
    assert item["attachment_id"] == doc["id"]
    assert item["title"] == "Q3 Report.pdf"
    assert item["snippet"] == ""
    assert item.get("span") is None
    assert s.done
    _no_provider_ids(mock_llm, s.events)


# Acceptance: Attachments — code_interpreter tool with ready XLSX file ids
def test_code_interpreter_tool_in_request(api, mock_llm, db):
    c = api.create_chat()
    n = nonce()
    assert api.stream(c["id"], f"no sheet {n}").done
    assert find_tool(mock_llm.chat_requests(contains=n)[-1], "code_interpreter") is None
    x = api.upload_ready(c["id"], "data.xlsx", make_xlsx(), MIME_XLSX)
    pfid = db.attachment_row(x["id"])["provider_file_id"]
    m = nonce()
    assert api.stream(c["id"], f"analyse {m}").done
    req = mock_llm.chat_requests(contains=m)[-1]
    tool = find_tool(req, "code_interpreter")
    assert tool is not None
    assert tool["container"]["type"] == "auto"
    assert tool["container"]["file_ids"] == [pfid]
    assert "code_interpreter_call.outputs" in (req["json"].get("include") or [])
    assert find_tool(req, "file_search") is None, "XLSX is not indexed: no file_search"


# Acceptance: Attachments — delete lifecycle, idempotent repeat, provider cleanup
def test_delete_attachment(api, mock_llm, db):
    c = api.create_chat()
    a = api.upload_ready(c["id"], "del.txt", b"delete me", "text/plain")
    pfid = db.attachment_row(a["id"])["provider_file_id"]
    r = api.delete(f"/v1/chats/{c['id']}/attachments/{a['id']}")
    assert r.status_code == 204, r.text
    assert_problem(api.attachment(c["id"], a["id"]), 404, "not_found", resource_type=RT_ATTACHMENT)
    r = api.delete(f"/v1/chats/{c['id']}/attachments/{a['id']}")
    assert r.status_code == 204, "a repeated delete is idempotent"
    wait_for(lambda: mock_llm.requests(path_contains=f"/files/{pfid}", method="DELETE"), timeout=30, desc="provider file delete")
    wait_for(lambda: db.attachment_row(a["id"])["cleanup_status"] == "done", timeout=30, desc="cleanup_status done")
    assert db.attachment_row(a["id"])["deleted_at"] is not None
    assert len(mock_llm.requests(path_contains=f"/files/{pfid}", method="DELETE")) >= 1


# Acceptance: Attachments — referenced attachment is locked
def test_delete_referenced_attachment_locked(api):
    c = api.create_chat()
    a = api.upload_ready(c["id"], "lock.txt", b"locked", "text/plain")
    assert api.stream(c["id"], "use " + nonce(), attachment_ids=[a["id"]]).done
    r = api.delete(f"/v1/chats/{c['id']}/attachments/{a['id']}")
    assert_problem(r, 409, "already_exists", resource_name="attachment_locked")
    assert api.attachment(c["id"], a["id"]).status_code == 200


# Acceptance: Attachments — get: unknown, other chat, other user
def test_get_attachment_scoping(api, api_for):
    c1 = api.create_chat()
    c2 = api.create_chat()
    a = api.upload_ready(c1["id"], "scoped.txt", b"scoped", "text/plain")
    assert_problem(api.attachment(c1["id"], str(uuid.uuid4())), 404, "not_found", resource_type=RT_ATTACHMENT)
    assert_problem(api.attachment(c2["id"], a["id"]), 404, "not_found", resource_type=RT_ATTACHMENT)
    other = api_for("tok-a2")
    assert other.attachment(c1["id"], a["id"]).status_code == 404
    assert_problem(api.get(f"/v1/chats/{c1['id']}/attachments/not-a-uuid"), 400, "invalid_argument", field_reason="invalid_path_params")


# Acceptance: Attachments — abandoned upload recovery (upload reaper)
@pytest.mark.timeout(60)
def test_upload_reaper_marks_abandoned(api, mock_llm, db):
    c = api.create_chat()
    a = api.upload_ready(c["id"], "abandoned.txt", b"abandoned", "text/plain")
    row = db.attachment_row(a["id"])
    pfid = row["provider_file_id"]
    # Simulate a dropped request: the row stayed `uploaded` and is older than stale_after_secs.
    from mchelpers import shift_timestamp_old

    n = db.execute(
        "UPDATE attachments SET status = 'uploaded', updated_at = ? WHERE id = ?",
        (shift_timestamp_old(row["updated_at"]), ub(a["id"])),
    )
    assert n == 1
    wait_for(lambda: db.attachment_row(a["id"])["status"] == "failed", timeout=30, desc="reaper marks the row failed")
    row = db.attachment_row(a["id"])
    assert row["error_code"] == "upload_abandoned"
    assert row["deleted_at"] is None
    g = api.attachment(c["id"], a["id"]).json()
    assert g["status"] == "failed" and g["error_code"] == "upload_abandoned"
    wait_for(lambda: mock_llm.requests(path_contains=f"/files/{pfid}", method="DELETE"), timeout=30, desc="abandoned provider file delete")


# Acceptance: Attachments — cleanup retried after a provider failure
@pytest.mark.timeout(120)
def test_attachment_cleanup_retries(api, mock_llm, db):
    c = api.create_chat()
    a = api.upload_ready(c["id"], "retry.txt", b"retry cleanup", "text/plain")
    pfid = db.attachment_row(a["id"])["provider_file_id"]
    mock_llm.configure(file_delete_fail_count=1)
    assert api.delete(f"/v1/chats/{c['id']}/attachments/{a['id']}").status_code == 204
    wait_for(lambda: db.attachment_row(a["id"])["cleanup_status"] == "done", timeout=100, interval=1, desc="cleanup done after retry")
    row = db.attachment_row(a["id"])
    assert int(row["cleanup_attempts"] or 0) >= 1
    assert len(mock_llm.requests(path_contains=f"/files/{pfid}", method="DELETE")) >= 2


# Acceptance: Attachments — kill switches: images and code interpreter
def test_kill_switch_images_and_code_interpreter(ks_api_for):
    a = ks_api_for("tok-a")
    c = a.create_chat()
    r = a.upload(c["id"], "pic.png", make_png(), MIME_PNG)
    assert_problem(r, 400, "failed_precondition", violation_subject="images", violation_type="FEATURE_DISABLED")
    r = a.upload(c["id"], "data.xlsx", make_xlsx(), MIME_XLSX)
    assert_problem(r, 400, "invalid_argument", field_reason="CODE_INTERPRETER_UNAVAILABLE")
    # documents still work
    assert a.upload(c["id"], "ok.txt", b"fine", "text/plain").status_code == 201
