"""Attachment upload / get / delete, indexing lifecycle, tool wiring,
limits, cleanup and abandoned-upload recovery."""

import json
import time
import uuid

from conftest import reason, violations, wait_for
from testdata_helpers import PDF, TEXT, XLSX_CT, png

PROVIDER_ID_KEYS = ("provider_file_id", "vector_store_id", "file_id")


def _no_provider_ids(obj):
    text = json.dumps(obj)
    assert "file-" not in text and "vs_" not in text, text
    for k in PROVIDER_ID_KEYS:
        assert k not in text


def test_document_upload_lifecycle(api, mock):
    chat = api.create_chat()
    r = api.upload(chat["id"], "report.txt", TEXT, "text/plain")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "document"
    assert att["filename"] == "report.txt" and att["content_type"] == "text/plain"
    assert att["size_bytes"] == len(TEXT)
    assert "error_code" not in att and "img_thumbnail" not in att
    _no_provider_ids(att)
    got = api.get(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert got.status_code == 200 and got.json() == att

    files = [x for x in mock.paths("POST", "/v1/files") if x["path"].endswith("/files")]
    assert files
    vs_adds = [x for x in mock.requests() if x["method"] == "POST" and "/vector_stores/" in x["path"] and x["path"].endswith("/files")]
    assert any((x.get("json") or {}).get("attributes", {}).get("attachment_id") == att["id"] for x in vs_adds)

    # the chat now has file_search on every request
    s = api.send(chat["id"], "what about the report?", attachment_ids=[att["id"]])
    assert s.terminal[0] == "done", s.text
    req = mock.chat_requests(chat["id"])[-1]["json"]
    fs = [t for t in req.get("tools", []) if t["type"] == "file_search"]
    assert fs and fs[0]["vector_store_ids"] and fs[0]["max_num_results"] == 5
    assert "file_search" in req.get("instructions", "") or any("file_search" in json.dumps(i) for i in req["input"])
    msgs = api.messages(chat["id"])
    summ = msgs[0]["attachments"]
    assert summ == [{"attachment_id": att["id"], "kind": "document", "filename": "report.txt", "status": "ready"}]
    s = api.send(chat["id"], "no attachments this time")
    req = mock.chat_requests(chat["id"])[-1]["json"]
    assert any(t["type"] == "file_search" for t in req.get("tools", []))

    # referenced attachment cannot be deleted
    r = api.delete(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert r.status_code == 409 and r.json()["context"]["resource_name"] == "attachment_locked"


def test_delete_unreferenced_attachment_and_cleanup(api, mock):
    chat = api.create_chat()
    att = api.upload(chat["id"], "a.pdf", PDF, "application/pdf").json()
    assert att["status"] == "ready"
    assert api.delete(f"/chats/{chat['id']}/attachments/{att['id']}").status_code == 204
    assert api.delete(f"/chats/{chat['id']}/attachments/{att['id']}").status_code == 204  # idempotent
    r = api.get(f"/chats/{chat['id']}/attachments/{att['id']}")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.attachment.v1~"
    # provider file deleted asynchronously by the cleanup worker
    wait_for(lambda: [x for x in mock.requests() if x["method"] == "DELETE" and "/v1/files/" in x["path"]], timeout=20, msg="provider delete")


def test_image_upload_and_multimodal_input(api, mock):
    chat = api.create_chat()  # gpt-4.1 supports vision
    r = api.upload(chat["id"], "pic.png", png(), "image/png")
    assert r.status_code == 201, r.text
    img = r.json()
    assert img["kind"] == "image" and img["status"] == "ready"
    th = img["img_thumbnail"]
    assert th["content_type"] == "image/webp" and th["width"] <= 128 and th["height"] <= 128 and th["data_base64"]
    s = api.send(chat["id"], "describe", attachment_ids=[img["id"]])
    assert s.terminal[0] == "done", s.text
    req = mock.chat_requests(chat["id"])[-1]["json"]
    content = req["input"][-1]["content"]
    assert isinstance(content, list) and any(p["type"] == "input_image" for p in content)
    # the image is not part of later turns unless re-attached
    s = api.send(chat["id"], "next")
    req = mock.chat_requests(chat["id"])[-1]["json"]
    assert "input_image" not in json.dumps(req["input"][-1])
    # images do not enable file_search
    assert not any(t["type"] == "file_search" for t in req.get("tools", []))
    msgs = api.messages(chat["id"])
    a = msgs[0]["attachments"][0]
    assert a["kind"] == "image" and a["img_thumbnail"]["width"] > 0


def test_image_guards(api):
    chat = api.create_chat(model="gpt-4.1-mini")  # no vision
    r = api.upload(chat["id"], "pic.png", png(), "image/png")
    assert r.status_code == 201
    s = api.send(chat["id"], "describe", attachment_ids=[r.json()["id"]])
    assert s.status == 400 and reason(s.json) == "VISION_NOT_SUPPORTED"

    chat = api.create_chat()
    ids = [api.upload(chat["id"], f"p{i}.png", png(8, 8), "image/png").json()["id"] for i in range(3)]
    s = api.send(chat["id"], "many", attachment_ids=ids)
    assert s.status == 400 and reason(s.json) == "TOO_MANY_IMAGES"
    # duplicates and foreign ids
    s = api.send(chat["id"], "dup", attachment_ids=[ids[0], ids[0]])
    assert s.status == 400 and reason(s.json) == "invalid_attachment"
    other = api.create_chat()
    foreign = api.upload(other["id"], "f.png", png(8, 8), "image/png").json()["id"]
    s = api.send(chat["id"], "foreign", attachment_ids=[foreign])
    assert s.status == 400 and reason(s.json) == "invalid_attachment"


def test_upload_validation(api):
    chat = api.create_chat(model="gpt-4.1-mini")  # max_file_size_mb = 1, no code interpreter
    r = api.upload(chat["id"], "bin.exe", b"MZ", "application/x-msdownload")
    assert r.status_code == 400 and reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    r = api.upload(chat["id"], "mystery.zzz", b"abc", "application/octet-stream")
    assert r.status_code == 400 and reason(r.json()) == "UNSUPPORTED_CONTENT_TYPE"
    r = api.upload(chat["id"], "inferred.pdf", PDF, "application/octet-stream")
    assert r.status_code == 201 and r.json()["content_type"] == "application/pdf"
    r = api.upload(chat["id"], "big.txt", b"a" * (1024 * 1024 + 10), "text/plain")
    assert r.status_code == 400, r.text
    v = violations(r.json())[0]
    assert v["field"] == "content_length" and v["reason"] == "FILE_TOO_LARGE"
    r = api.upload(chat["id"], "sheet.xlsx", b"PK\x03\x04data", XLSX_CT)
    assert r.status_code == 400 and reason(r.json()) == "CODE_INTERPRETER_UNAVAILABLE"
    # multipart errors
    r = api.post(f"/chats/{chat['id']}/attachments", content=b"x", headers={"Content-Type": "multipart/form-data"})
    assert r.status_code == 400 and reason(r.json()) == "BOUNDARY_REQUIRED"
    r = api.post(f"/chats/{chat['id']}/attachments", files={"other": ("a.txt", b"x", "text/plain")})
    assert r.status_code == 400 and reason(r.json()) == "MISSING_FILE"
    # unknown chat
    r = api.upload(str(uuid.uuid4()), "a.txt", TEXT, "text/plain")
    assert r.status_code == 404 and r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"


def test_xlsx_with_code_interpreter(api, mock):
    chat = api.create_chat()  # gpt-4.1 has code interpreter
    r = api.upload(chat["id"], "data.xlsx", b"PK\x03\x04data", XLSX_CT)
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "ready" and att["kind"] == "document"
    s = api.send(chat["id"], "MOCK_CODE analyze", attachment_ids=[att["id"]])
    assert s.terminal[0] == "done", s.text
    req = mock.chat_requests(chat["id"])[-1]["json"]
    ci = [t for t in req.get("tools", []) if t["type"] == "code_interpreter"]
    assert ci and ci[0]["container"]["type"] == "auto" and ci[0]["container"]["file_ids"]
    assert "code_interpreter_call.outputs" in req.get("include", [])
    tools = s.all("tool")
    assert {"phase": "start", "name": "code_interpreter", "details": {}} in tools
    assert any(t["phase"] == "done" and t["details"].get("output") == "42" for t in tools)


def test_per_chat_document_limits(api):
    chat = api.create_chat()
    for i in range(3):
        assert api.upload(chat["id"], f"d{i}.txt", TEXT, "text/plain").status_code == 201
    r = api.upload(chat["id"], "d4.txt", TEXT, "text/plain")
    assert r.status_code == 429, r.text
    assert r.json()["context"]["violations"][0]["subject"] == "document_limit"
    chat = api.create_chat()
    r = api.upload(chat["id"], "huge.txt", b"a" * (2 * 1024 * 1024 + 100), "text/plain")
    assert r.status_code == 429, r.text
    assert r.json()["context"]["violations"][0]["subject"] == "storage_limit"


def test_storage_failure_returns_503(api, mock):
    chat = api.create_chat()
    mock.config(upload_status=500)
    r = api.upload(chat["id"], "x.txt", TEXT, "text/plain")
    assert r.status_code == 503 and r.headers["retry-after"] == "10"
    mock.config(upload_status=200, index_status="failed")
    r = api.upload(chat["id"], "y.txt", TEXT, "text/plain")
    assert r.status_code == 503 and r.headers["retry-after"] == "10"
    assert "indexing_failed" not in r.text


def test_indexing_failure_marks_row_failed(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    mock.config(index_status="failed")
    before = len([x for x in mock.requests() if x["method"] == "DELETE" and "/files/" in x["path"]])
    r = api.upload(chat["id"], "y.txt", TEXT, "text/plain")
    assert r.status_code == 503
    rows = server.query("SELECT id, status, error_code FROM attachments WHERE filename = 'y.txt' ORDER BY created_at DESC LIMIT 1")
    assert rows and rows[0]["status"] == "failed" and rows[0]["error_code"] == "indexing_failed"
    att_id = str(uuid.UUID(bytes=rows[0]["id"])) if isinstance(rows[0]["id"], bytes) else rows[0]["id"]
    got = api.get(f"/chats/{chat['id']}/attachments/{att_id}").json()
    assert got["status"] == "failed" and got["error_code"] == "indexing_failed"
    wait_for(lambda: len([x for x in mock.requests() if x["method"] == "DELETE" and "/files/" in x["path"]]) > before, timeout=15, msg="file delete")
    # a failed attachment cannot be referenced
    s = api.send(chat["id"], "x", attachment_ids=[att_id])
    assert s.status == 400 and reason(s.json) == "invalid_attachment"


def test_slow_indexing_completes_in_background(api, mock):
    chat = api.create_chat()
    mock.config(index_status="completed", index_delay_secs=29)
    t0 = time.time()
    r = api.upload(chat["id"], "slow.txt", TEXT, "text/plain")
    assert r.status_code == 201, r.text
    att = r.json()
    assert att["status"] == "uploaded"
    assert 20 < time.time() - t0 < 30
    s = api.send(chat["id"], "use it", attachment_ids=[att["id"]])
    assert s.status == 400 and reason(s.json) == "invalid_attachment"
    wait_for(lambda: api.get(f"/chats/{chat['id']}/attachments/{att['id']}").json()["status"] == "ready", timeout=30, msg="ready")


def test_attachment_scoped_to_owner(server):
    a = server.client("user-a")
    b = server.client("user-b")
    chat = a.create_chat()
    att = a.upload(chat["id"], "a.txt", TEXT, "text/plain").json()
    assert b.get(f"/chats/{chat['id']}/attachments/{att['id']}").status_code == 404
    assert b.delete(f"/chats/{chat['id']}/attachments/{att['id']}").status_code == 404
    assert b.upload(chat["id"], "b.txt", TEXT, "text/plain").status_code == 404
    # attachment of another chat of the same user is not visible through this chat
    other = a.create_chat()
    assert a.get(f"/chats/{other['id']}/attachments/{att['id']}").status_code == 404


def test_retry_carries_attachments(api, mock):
    chat = api.create_chat()
    img = api.upload(chat["id"], "p.png", png(8, 8), "image/png").json()
    doc = api.upload(chat["id"], "d.txt", TEXT, "text/plain").json()
    s = api.send(chat["id"], "look", attachment_ids=[img["id"], doc["id"]])
    rid = s.first("stream_started")["request_id"]
    s = api.retry(chat["id"], rid)
    assert s.terminal[0] == "done", s.text
    req = mock.chat_requests(chat["id"])[-1]["json"]
    assert "input_image" in json.dumps(req["input"][-1])
    msgs = api.messages(chat["id"])
    assert sorted(a["attachment_id"] for a in msgs[0]["attachments"]) == sorted([img["id"], doc["id"]])
    # edit keeps them too
    new_rid = s.first("stream_started")["request_id"]
    s = api.edit(chat["id"], new_rid, "look again")
    assert s.terminal[0] == "done"
    msgs = api.messages(chat["id"])
    assert len(msgs[0]["attachments"]) == 2
