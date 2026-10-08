"""Attachments: upload, get, delete, indexing, thumbnails (DESIGN sections 3.3, 3.6, 4)."""

import base64
import json
import os
import uuid
from contextlib import closing
from pathlib import Path

import pytest

from .helpers import (PREFIX, TOKEN_A_REVIEWER, api, assert_problem, captured_outbox, create_chat, db, stream,
                      uuid_bytes, wait_until)

pytestmark = pytest.mark.usefixtures("server")

TESTDATA = Path(__file__).resolve().parents[2] / "testdata"
PDF = TESTDATA / "pdf" / "test_file_one_page_en.pdf"
PNG = TESTDATA / "images" / "tiny.png"
XLSX = TESTDATA / "xlsx" / "simple_data.xlsx"
XLSX_MIME = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
ATTACHMENT_TYPE = "gts.cf.core.mini_chat.attachment.v1~"
#: Capture table of the ``outbox_capture`` fixture (conftest.py).
CAPTURE = "e2e_attachment_outbox_capture"
#: Provider identifiers must never reach API bodies.
PROVIDER_ID_PREFIXES = ("file-", "vs_", "assistant-")


def _cleanup_events(attachment_id: str) -> list[dict]:
    out = []
    for queue, _, body in captured_outbox(CAPTURE):
        if body.get("attachment_id") == attachment_id:
            assert queue == "mini-chat.attachment_cleanup"
            out.append(body)
    return out


def _upload(s, chat_id: str, filename: str, data: bytes, content_type: str):
    return s.post(f"{PREFIX}/chats/{chat_id}/attachments",
                  files={"file": (filename, data, content_type)})


def _upload_file(s, chat_id: str, path: Path, content_type: str) -> dict:
    r = _upload(s, chat_id, path.name, path.read_bytes(), content_type)
    assert r.status_code == 201, r.text
    return r.json()


def _row(attachment_id: str):
    with closing(db()) as conn:
        return conn.execute(
            "SELECT status, error_code, provider_file_id, for_file_search, for_code_interpreter, attachment_kind,"
            " deleted_at, cleanup_status, img_thumbnail FROM attachments WHERE id = ?",
            (uuid_bytes(attachment_id),)).fetchone()


def _vector_store(chat_id: str):
    with closing(db()) as conn:
        return conn.execute("SELECT vector_store_id, provider FROM chat_vector_stores WHERE chat_id = ?",
                            (uuid_bytes(chat_id),)).fetchone()


def _assert_no_provider_ids(body) -> None:
    text = json.dumps(body)
    for p in PROVIDER_ID_PREFIXES:
        assert f'"{p}' not in text, f"provider id leaked: {text}"


def _last_provider_request(mock) -> dict:
    return mock.requests(path="/responses")[-1]["json"]


def _send(s, chat_id: str, attachment_ids: list[str]) -> str:
    rid = str(uuid.uuid4())
    res = stream(s, chat_id, {"content": "what is in the file?", "request_id": rid,
                              "attachment_ids": attachment_ids})
    assert res.terminal and res.terminal[0] == "done", res.raw
    return rid


# ── upload ──────────────────────────────────────────────────────────────────


def test_pdf_upload_is_indexed_and_ready(reset_mock):
    s = api()
    chat = create_chat(s)
    body = _upload_file(s, chat["id"], PDF, "application/pdf")

    assert body["status"] == "ready"
    assert body["kind"] == "document"
    assert body["content_type"] == "application/pdf"
    assert body["filename"] == PDF.name
    assert body["size_bytes"] == PDF.stat().st_size
    for absent in ("doc_summary", "summary_updated_at", "img_thumbnail", "error_code"):
        assert absent not in body, body
    _assert_no_provider_ids(body)

    files = reset_mock.requests(method="POST", route="files")
    assert len(files) == 1
    part = next(p for p in files[0]["multipart"] if p["filename"])
    assert part["filename"] == f"{chat['id']}_{body['id']}.pdf"
    assert part["size"] == PDF.stat().st_size
    stores = reset_mock.requests(method="POST", route="vector_stores")
    assert len(stores) == 1
    vs = _vector_store(chat["id"])
    assert vs["vector_store_id"].startswith("vs_") and vs["provider"] == "openai"
    attach = reset_mock.requests(method="POST", path=f"/vector_stores/{vs['vector_store_id']}/files")
    assert len(attach) == 1
    row = _row(body["id"])
    assert attach[0]["json"]["file_id"] == row["provider_file_id"]
    assert attach[0]["json"]["attributes"] == {"attachment_id": body["id"]}
    assert row["status"] == "ready" and row["for_file_search"] == 1 and row["for_code_interpreter"] == 0

    got = s.get(f"{PREFIX}/chats/{chat['id']}/attachments/{body['id']}")
    assert got.status_code == 200, got.text
    assert got.json() == body

    # The second document reuses the chat's vector store.
    _upload_file(s, chat["id"], PDF, "application/octet-stream")
    assert len(reset_mock.requests(method="POST", route="vector_stores")) == 1


def test_png_upload_has_webp_thumbnail(reset_mock):
    s = api()
    chat = create_chat(s)
    body = _upload_file(s, chat["id"], PNG, "image/png")
    assert body["status"] == "ready" and body["kind"] == "image"
    thumb = body["img_thumbnail"]
    assert thumb["content_type"] == "image/webp"
    assert 1 <= thumb["width"] <= 128 and 1 <= thumb["height"] <= 128
    raw = base64.b64decode(thumb["data_base64"])
    assert raw[:4] == b"RIFF" and raw[8:12] == b"WEBP"
    _assert_no_provider_ids(body)
    assert reset_mock.requests(route="vector_stores") == [] and reset_mock.requests(route="vector_store_file_status") == []
    assert _row(body["id"])["for_file_search"] == 0


def test_xlsx_upload_is_for_code_interpreter(reset_mock):
    s = api()
    chat = create_chat(s)
    body = _upload_file(s, chat["id"], XLSX, XLSX_MIME)
    assert body["status"] == "ready" and body["kind"] == "document"
    row = _row(body["id"])
    assert row["for_code_interpreter"] == 1 and row["for_file_search"] == 0
    assert reset_mock.requests(route="vector_stores") == [] and reset_mock.requests(route="vector_store_file_status") == []


def test_unsupported_type_is_400(reset_mock):
    s = api()
    chat = create_chat(s)
    r = _upload(s, chat["id"], "setup.exe", b"MZ\x90\x00", "application/x-msdownload")
    assert_problem(r, 400, "invalid_argument", reason="UNSUPPORTED_CONTENT_TYPE")
    r = _upload(s, chat["id"], "setup.exe", b"MZ\x90\x00", "application/octet-stream")
    assert_problem(r, 400, "invalid_argument", reason="UNSUPPORTED_CONTENT_TYPE")
    assert reset_mock.requests(route="files") == []


def test_image_over_limit_is_file_too_large(reset_mock):
    s = api()
    chat = create_chat(s)
    big = os.urandom(5 * 1024 * 1024 + 1)  # uploaded_image_max_size_kb = 5120
    r = _upload(s, chat["id"], "big.png", big, "image/png")
    assert_problem(r, 400, "out_of_range", reason="FILE_TOO_LARGE", field="content_length")
    assert reset_mock.requests(route="files") == []
    with closing(db()) as conn:
        n = conn.execute("SELECT COUNT(*) FROM attachments WHERE chat_id = ?", (uuid_bytes(chat["id"]),)).fetchone()[0]
    assert n == 0


def test_multipart_errors_are_400():
    s = api()
    chat = create_chat(s)
    url = f"{PREFIX}/chats/{chat['id']}/attachments"
    r = s.post(url, data=b"x", headers={"Content-Type": "multipart/form-data"})
    assert_problem(r, 400, "invalid_argument", reason="BOUNDARY_REQUIRED", field="content_type")
    r = s.post(url, files={"other": ("a.txt", b"x", "text/plain")})
    assert_problem(r, 400, "invalid_argument", reason="MISSING_FILE", field="file")


def test_upload_to_unknown_chat_or_removed_model(reset_mock):
    s = api()
    r = _upload(s, str(uuid.uuid4()), "a.pdf", b"%PDF", "application/pdf")
    assert_problem(r, 404, "not_found", resource_type="gts.cf.core.mini_chat.chat.v1~")
    chat = create_chat(s)
    with closing(db()) as conn:
        conn.execute("UPDATE chats SET model = 'gone-model' WHERE id = ?", (uuid_bytes(chat["id"]),))
        conn.commit()
    r = _upload(s, chat["id"], "a.pdf", b"%PDF", "application/pdf")
    assert_problem(r, 400, "invalid_argument", reason="INVALID_MODEL", field="model")
    assert reset_mock.requests(route="files") == []


def test_indexing_failure_is_503_and_row_failed(reset_mock):
    s = api()
    chat = create_chat(s)
    reset_mock.enqueue("vector_store_file_status", {"file_status": "failed"})
    r = _upload(s, chat["id"], PDF.name, PDF.read_bytes(), "application/pdf")
    assert_problem(r, 503, "service_unavailable")
    assert r.headers.get("Retry-After") == "10"
    assert "indexing_failed" not in r.text
    with closing(db()) as conn:
        row = conn.execute("SELECT status, error_code, provider_file_id FROM attachments WHERE chat_id = ?",
                           (uuid_bytes(chat["id"]),)).fetchone()
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    # Best-effort provider delete of the indexed file.
    wait_until(lambda: reset_mock.requests(method="DELETE", path=f"/files/{row['provider_file_id']}"),
               message="provider file delete")


@pytest.mark.timeout(120)
def test_slow_indexing_returns_uploaded_then_ready(reset_mock):
    s = api()
    chat = create_chat(s)
    reset_mock.enqueue("vector_store_file_status", {"file_status": "in_progress"}, repeat=0)
    r = _upload(s, chat["id"], PDF.name, PDF.read_bytes(), "application/pdf")
    assert r.status_code == 201, r.text
    body = r.json()
    assert body["status"] == "uploaded"
    url = f"{PREFIX}/chats/{chat['id']}/attachments/{body['id']}"
    assert s.get(url).json()["status"] == "uploaded"
    reset_mock.reset()  # the next status read reports completed
    wait_until(lambda: s.get(url).json()["status"] == "ready", timeout=30, interval=0.5,
               message="background indexing to finish")


# ── send path ───────────────────────────────────────────────────────────────


def test_message_with_document_uses_file_search(reset_mock):
    s = api()
    chat = create_chat(s)
    att = _upload_file(s, chat["id"], PDF, "application/pdf")
    rid = _send(s, chat["id"], [att["id"]])

    req = _last_provider_request(reset_mock)
    vs = _vector_store(chat["id"])["vector_store_id"]
    fs = [t for t in req["tools"] if t["type"] == "file_search"]
    assert fs and fs[0]["vector_store_ids"] == [vs], req["tools"]
    assert "Use file_search only when" in req["instructions"]

    msgs = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    user = next(m for m in msgs if m["role"] == "user" and m["request_id"] == rid)
    assert user["attachments"] == [{"attachment_id": att["id"], "kind": "document",
                                    "filename": PDF.name, "status": "ready"}]


def test_image_turn_sends_input_image(reset_mock):
    s = api()
    chat = create_chat(s)
    att = _upload_file(s, chat["id"], PNG, "image/png")
    _send(s, chat["id"], [att["id"]])
    file_id = _row(att["id"])["provider_file_id"]
    text = json.dumps(_last_provider_request(reset_mock)["input"])
    assert '"input_image"' in text and file_id in text, text


def test_xlsx_enables_code_interpreter(reset_mock):
    s = api()
    chat = create_chat(s)
    att = _upload_file(s, chat["id"], XLSX, XLSX_MIME)
    _send(s, chat["id"], [att["id"]])
    file_id = _row(att["id"])["provider_file_id"]
    ci = [t for t in _last_provider_request(reset_mock)["tools"] if t["type"] == "code_interpreter"]
    assert ci and file_id in ci[0]["container"]["file_ids"], ci


# ── get / delete ────────────────────────────────────────────────────────────


def test_get_unknown_or_foreign_is_404(reset_mock):
    s = api()
    chat = create_chat(s)
    att = _upload_file(s, chat["id"], PNG, "image/png")
    url = f"{PREFIX}/chats/{chat['id']}/attachments"
    assert_problem(s.get(f"{url}/{uuid.uuid4()}"), 404, "not_found", resource_type=ATTACHMENT_TYPE)
    assert_problem(api(TOKEN_A_REVIEWER).get(f"{url}/{att['id']}"), 404, "not_found",
                   resource_type="gts.cf.core.mini_chat.chat.v1~")


def test_delete_locked_then_unreferenced(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s)
    used = _upload_file(s, chat["id"], PDF, "application/pdf")
    _send(s, chat["id"], [used["id"]])
    url = f"{PREFIX}/chats/{chat['id']}/attachments"
    assert_problem(s.delete(f"{url}/{used['id']}"), 409, "already_exists", resource_name="attachment_locked")
    assert _row(used["id"])["deleted_at"] is None

    free = _upload_file(s, chat["id"], PNG, "image/png")
    r = s.delete(f"{url}/{free['id']}")
    assert r.status_code == 204, r.text
    row = _row(free["id"])
    # The cleanup handler may already have finished (`done`).
    assert row["deleted_at"] is not None and row["cleanup_status"] in ("pending", "done")
    events = wait_until(lambda: _cleanup_events(free["id"]), message="cleanup outbox event")
    assert len(events) == 1
    ev = events[0]
    assert ev["event_type"] == "attachment_deleted"
    assert ev["chat_id"] == chat["id"]
    assert ev["provider_file_id"] == row["provider_file_id"]
    assert ev["storage_backend"] == "openai"
    assert ev["attachment_kind"] == "image"
    assert ev["vector_store_id"] is None and ev["secondary_ref"] is None

    # Idempotent: 204 again, no second event; GET is 404.
    assert s.delete(f"{url}/{free['id']}").status_code == 204
    assert len(_cleanup_events(free["id"])) == 1
    assert_problem(s.get(f"{url}/{free['id']}"), 404, "not_found", resource_type=ATTACHMENT_TYPE)
