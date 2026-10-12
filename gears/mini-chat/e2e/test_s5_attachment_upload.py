"""S5 attachments: upload, indexing, thumbnails, limits, upload errors and GET.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "Get Attachment" and "Upload Attachment" (operations
  ``mini_chat.upload_attachment`` / ``mini_chat.get_attachment``,
  ``AttachmentDetailDto``, ``ImgThumbnailDto``): synchronous upload, 201 with
  ``status: ready`` (or ``uploaded`` when indexing still runs at the 25 s
  deadline), optional fields omitted when null, ``doc_summary`` /
  ``summary_updated_at`` always null, no provider identifiers; the filename
  defaults to ``upload`` and is truncated to 255 characters keeping the
  extension; ``application/octet-stream`` gets its MIME type from the
  extension; the upload error table (404 chat before the body, 400
  ``INVALID_MODEL`` before the body, ``BOUNDARY_REQUIRED`` /
  ``MULTIPART_ERROR`` / ``MISSING_FILE`` / ``MISSING_CONTENT_TYPE``,
  ``FILE_TOO_LARGE``, ``UNSUPPORTED_CONTENT_TYPE``,
  ``CODE_INTERPRETER_UNAVAILABLE``, images ``FEATURE_DISABLED``, 409
  ``provider_mismatch``, 429 ``document_limit`` / ``storage_limit``, 503 with
  ``Retry-After`` 10 / 5);
* §3.3 "Error Codes" (REST error mapping: resource types, field names,
  ``invalid_path_params``, ``context.retry_after_seconds``);
* §3.6 "File Upload" (``cpt-cf-mini-chat-seq-file-upload``): purposes and
  kinds, Files API upload with ``purpose=assistants``, the chat vector store
  (one per chat, created on the first document), vector store file with the
  single ``attachment_id`` attribute, indexing polls (250 ms doubling to 2 s,
  until 25 s), transient read errors keep polling, failure ->
  ``indexing_failed`` + best-effort provider file delete + 503, background
  indexing after the deadline, thumbnails (WebP, fit inside 128x128 keeping
  the aspect ratio, ``thumbnail.max_bytes``), images and XLSX never enter the
  vector store;
* §3.7 ``attachments`` (``storage_backend``, ``provider_file_id``,
  ``for_file_search`` / ``for_code_interpreter``, ``error_code`` values) and
  ``chat_vector_stores`` (``provider`` label, creation protocol, placeholder
  removal after a failed create, 409 ``provider_mismatch``);
* "Code Interpreter Tool Availability" (XLSX purpose table, kill switch and
  model capability gating at upload);
* "RAG Quality & Scale Controls", B.7 / B.8 (``rag.*`` and ``thumbnail.*``
  keys), §2.1 "Files API upload field mapping" and the provider filename
  convention ``{chat_id}_{attachment_id}.{ext}`` ("Cleanup on Chat Deletion",
  invariant 4).

The upload, get and delete endpoints are exercised through the API; rows the
API cannot produce (an attachment uploaded by another user into the caller's
chat, a vector store of another provider backend) are seeded in SQLite.
"""

from __future__ import annotations

import base64
import random
import re
import struct
import threading
import time
import uuid
import zlib
from typing import Any, Iterator

import httpx
import pytest

from conftest import REPO_ROOT, create_server, default_model_catalog
from mc_helpers import (
    RT_ATTACHMENT,
    RT_CHAT,
    assert_problem,
    db_uuid,
    is_uuid,
    new_uuid,
    retry_after_seconds,
    uuid_bytes,
    wait_until,
)
from mc_seed import db_ts, is_rfc3339, seed_attachment

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
PROVIDER_ID = "openai"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
TESTDATA = REPO_ROOT / "testing" / "e2e" / "testdata"

DETAIL_REQUIRED = {"id", "filename", "content_type", "size_bytes", "status", "kind", "created_at"}
DETAIL_FIELDS = DETAIL_REQUIRED | {"error_code", "doc_summary", "img_thumbnail", "summary_updated_at"}
THUMBNAIL_FIELDS = {"content_type", "width", "height", "data_base64"}
#: ``thumbnail.width`` / ``thumbnail.height`` / ``thumbnail.max_bytes`` defaults (B.8).
THUMBNAIL_BOX = (128, 128)
THUMBNAIL_MAX_BYTES = 131_072
#: Fixed indexing deadline of the upload request (§3.6), seconds.
INDEXING_DEADLINE_SECS = 25
UPLOAD_RETRY_AFTER = 10
CONCURRENCY_RETRY_AFTER = 5

#: A valid 1x1 lossy (VP8) WebP image.
TINY_LOSSY_WEBP = base64.b64decode("UklGRiIAAABXRUJQVlA4IBYAAAAwAQCdASoBAAEADsD+JaQAA3AAAAAA")

PDF_MIME = "application/pdf"
TXT_MIME = "text/plain"
DOCX_MIME = "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
XLSX_MIME = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
OCTET = "application/octet-stream"

#: Provider identifiers as the mock (and OpenAI) issue them.
PROVIDER_ID_RE = re.compile(r"\b(?:file-|vs_)[A-Za-z0-9]{12,}")


# --------------------------------------------------------------------------
# Test data
# --------------------------------------------------------------------------


def make_png(width: int, height: int, *, noise: bool = False, seed: int = 7) -> bytes:
    """An 8-bit RGB PNG; ``noise=True`` makes it (nearly) incompressible."""
    rng = random.Random(seed)
    rows = []
    for y in range(height):
        if noise:
            row = rng.randbytes(width * 3)
        else:
            row = bytes(
                value
                for x in range(width)
                for value in (x * 255 // max(1, width - 1), y * 255 // max(1, height - 1), 128)
            )
        rows.append(b"\x00" + row)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )


def load_testdata(relative: str) -> bytes:
    path = TESTDATA / relative
    if not path.is_file():
        pytest.skip(f"test data file missing: {path}")
    return path.read_bytes()


def text_document(size: int = 0, label: str = "doc") -> bytes:
    line = f"Mini-chat E2E {label}: quarterly revenue grew by 12 percent.\n".encode()
    if size <= 0:
        return line * 4
    return (line * (size // len(line) + 1))[:size]


def pdf_bytes() -> bytes:
    return load_testdata("pdf/test_file_one_page_en.pdf")


def xlsx_bytes() -> bytes:
    return load_testdata("xlsx/simple_data.xlsx")


def webp_dimensions(data: bytes) -> tuple[int, int]:
    """Canvas size of a WebP file (``VP8 ``, ``VP8L`` or ``VP8X`` first chunk)."""
    assert data[:4] == b"RIFF" and data[8:12] == b"WEBP", f"not a WebP file: {data[:16]!r}"
    assert int.from_bytes(data[4:8], "little") == len(data) - 8, "RIFF size does not match the data"
    fourcc = data[12:16]
    if fourcc == b"VP8 ":
        assert data[23:26] == b"\x9d\x01\x2a", "VP8 start code missing"
        width = struct.unpack("<H", data[26:28])[0] & 0x3FFF
        height = struct.unpack("<H", data[28:30])[0] & 0x3FFF
        return width, height
    if fourcc == b"VP8L":
        assert data[20] == 0x2F, "VP8L signature missing"
        bits = int.from_bytes(data[21:25], "little")
        return (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
    if fourcc == b"VP8X":
        return int.from_bytes(data[24:27], "little") + 1, int.from_bytes(data[27:30], "little") + 1
    raise AssertionError(f"unknown WebP chunk {fourcc!r}")


# --------------------------------------------------------------------------
# Multipart and API helpers
# --------------------------------------------------------------------------


def multipart(parts: list[dict[str, Any]], boundary: str | None = None) -> tuple[bytes, str]:
    """Build a ``multipart/form-data`` body.

    Each part: ``name``, ``data`` (bytes), optional ``filename`` and
    ``content_type`` (omitted from the part headers when ``None``).
    """
    boundary = boundary or f"mc-e2e-{uuid.uuid4().hex}"
    body = bytearray()
    for part in parts:
        disposition = f'form-data; name="{part["name"]}"'
        if part.get("filename") is not None:
            disposition += f'; filename="{part["filename"]}"'
        body += f"--{boundary}\r\nContent-Disposition: {disposition}\r\n".encode()
        if part.get("content_type") is not None:
            body += f"Content-Type: {part['content_type']}\r\n".encode()
        body += b"\r\n" + part["data"] + b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    return bytes(body), f"multipart/form-data; boundary={boundary}"


def _attachments_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/attachments"


def _attachment_path(chat_id: str, attachment_id: str) -> str:
    return f"{CHATS}/{chat_id}/attachments/{attachment_id}"


def upload_raw(
    server, chat_id: str, body: bytes, content_type: str, user: str = "A1", timeout: float = 60.0
) -> httpx.Response:
    return server.post(
        _attachments_path(chat_id), user, content=body, headers={"Content-Type": content_type}, timeout=timeout
    )


def upload(
    server,
    chat_id: str,
    data: bytes,
    filename: str | None = "report.pdf",
    content_type: str | None = PDF_MIME,
    user: str = "A1",
    timeout: float = 60.0,
) -> httpx.Response:
    body, ct = multipart([{"name": "file", "filename": filename, "content_type": content_type, "data": data}])
    return upload_raw(server, chat_id, body, ct, user=user, timeout=timeout)


def upload_ok(server, chat_id: str, data: bytes, filename: str = "report.pdf", content_type: str = PDF_MIME,
              user: str = "A1") -> dict[str, Any]:
    resp = upload(server, chat_id, data, filename, content_type, user=user)
    assert resp.status_code == 201, f"upload failed: HTTP {resp.status_code} {resp.text[:1000]}"
    return resp.json()


def upload_ok_raw(server, chat_id: str, parts: list[dict[str, Any]]) -> dict[str, Any]:
    body, ct = multipart(parts)
    resp = upload_raw(server, chat_id, body, ct)
    assert resp.status_code == 201, f"upload failed: HTTP {resp.status_code} {resp.text[:1000]}"
    return resp.json()


def _new_chat(server, user: str = "A1", model: str = DEFAULT_MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S5 attachments", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _attachment_row(server, attachment_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM attachments WHERE id = ?", (db_uuid(attachment_id),))
    assert len(rows) == 1, rows
    return rows[0]


def _chat_attachment_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query("SELECT * FROM attachments WHERE chat_id = ? ORDER BY created_at", (db_uuid(chat_id),))


def _vector_store_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (db_uuid(chat_id),))


def _assert_no_provider_ids(text: str, *known: str | None) -> None:
    for value in known:
        if value:
            assert value not in text, f"provider identifier {value!r} leaked: {text[:1000]}"
    assert PROVIDER_ID_RE.search(text) is None, f"provider identifier pattern leaked: {text[:1000]}"


def _wait_provider_file_delete(mock, provider_file_id: str) -> None:
    """Files API ``DELETE .../files/{id}`` (not a vector store file removal)."""
    wait_until(
        lambda: [r for r in mock.requests_for("files_delete") if r["path"].endswith(f"/files/{provider_file_id}")],
        timeout=15,
        description=f"provider file delete of {provider_file_id}",
    )


def assert_detail(
    body: dict[str, Any],
    *,
    filename: str,
    content_type: str,
    size_bytes: int,
    kind: str,
    status: str = "ready",
) -> None:
    """``AttachmentDetail`` shape: required fields, null optionals omitted or null."""
    assert isinstance(body, dict), body
    assert DETAIL_REQUIRED <= set(body), f"missing fields {DETAIL_REQUIRED - set(body)}: {body}"
    assert set(body) <= DETAIL_FIELDS, f"unexpected fields {set(body) - DETAIL_FIELDS}: {body}"
    assert is_uuid(body["id"]), body
    assert is_rfc3339(body["created_at"]), body
    assert body["filename"] == filename, body
    assert body["content_type"] == content_type, body
    assert body["size_bytes"] == size_bytes, body
    assert body["kind"] == kind, body
    assert body["status"] == status, body
    assert body.get("doc_summary") is None, body
    assert body.get("summary_updated_at") is None, body
    if status != "failed":
        assert body.get("error_code") is None, body
    if kind == "document" or status != "ready":
        assert body.get("img_thumbnail") is None, body


def assert_thumbnail(thumbnail: Any, source_size: tuple[int, int] | None = None) -> tuple[int, int, bytes]:
    """``ImgThumbnailDto``: WebP, fits inside 128x128, aspect ratio of ``source_size`` kept."""
    assert isinstance(thumbnail, dict), thumbnail
    assert set(thumbnail) == THUMBNAIL_FIELDS, thumbnail
    assert thumbnail["content_type"] == "image/webp", thumbnail
    width, height = thumbnail["width"], thumbnail["height"]
    assert isinstance(width, int) and isinstance(height, int), thumbnail
    assert 1 <= width <= THUMBNAIL_BOX[0] and 1 <= height <= THUMBNAIL_BOX[1], thumbnail
    raw = base64.b64decode(thumbnail["data_base64"], validate=True)
    assert len(raw) <= THUMBNAIL_MAX_BYTES, len(raw)
    assert webp_dimensions(raw) == (width, height), (webp_dimensions(raw), width, height)
    if source_size is not None:
        src_w, src_h = source_size
        scale = min(THUMBNAIL_BOX[0] / src_w, THUMBNAIL_BOX[1] / src_h)
        assert abs(width - src_w * scale) <= 1 and abs(height - src_h * scale) <= 1, (
            f"thumbnail {width}x{height} does not fit {src_w}x{src_h} into {THUMBNAIL_BOX}"
        )
    return width, height, raw


def assert_service_unavailable(resp: httpx.Response, retry_after: int) -> dict[str, Any]:
    problem = assert_problem(resp, 503, category="service_unavailable")
    assert retry_after_seconds(resp) == retry_after, resp.headers
    assert problem["context"].get("retry_after_seconds") == retry_after, problem
    return problem


def _policy(**config: Any) -> dict[str, Any]:
    return {"gears": {POLICY_PLUGIN: {"config": config}}}


def _gear(**config: Any) -> dict[str, Any]:
    return {"gears": {"mini-chat": {"config": config}}}


# --------------------------------------------------------------------------
# Module servers
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def _module_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


def _module_server(mock, servers: list[Any], name: str, overrides: dict[str, Any]):
    server = create_server(mock, overrides, name=name)
    servers.append(server)
    server.start()
    return server.wait_ready()


#: Small per-file limits, two documents per chat, one upload at a time.
LIMITS_RAG = {
    "max_documents_per_chat": 2,
    "uploaded_image_max_size_kb": 64,
    "uploaded_file_max_size_kb": 256,
    "max_concurrent_uploads": 1,
}


@pytest.fixture(scope="module")
def limits_server(mock, _module_servers):
    return _module_server(mock, _module_servers, "upload-limits", _gear(rag=LIMITS_RAG))


@pytest.fixture(scope="module")
def storage_server(mock, _module_servers):
    return _module_server(mock, _module_servers, "upload-storage", _gear(rag={"max_total_upload_mb_per_chat": 1}))


@pytest.fixture(scope="module")
def killswitch_server(mock, _module_servers):
    overrides = _policy(kill_switches={"disable_images": True, "disable_code_interpreter": True})
    return _module_server(mock, _module_servers, "upload-killswitch", overrides)


# --------------------------------------------------------------------------
# Documents: provider upload, vector store, DB rows, GET
# --------------------------------------------------------------------------


DOCUMENTS = [
    pytest.param("report.pdf", PDF_MIME, pdf_bytes, id="pdf"),
    pytest.param("notes.txt", TXT_MIME, text_document, id="txt"),
    pytest.param(
        "brief.docx", DOCX_MIME, lambda: load_testdata("docx/test_file_two_pages_international.docx"), id="docx"
    ),
]


@pytest.mark.parametrize(("filename", "content_type", "make"), DOCUMENTS)
def test_document_upload_indexes_into_the_chat_vector_store(mc, mock, filename, content_type, make):
    data = make()
    ident = mc.identity("A1")
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, data, filename, content_type)

    assert resp.status_code == 201, resp.text
    body = resp.json()
    assert_detail(body, filename=filename, content_type=content_type, size_bytes=len(data), kind="document")
    attachment_id = body["id"]

    uploads = mock.requests_for("files_upload")
    assert len(uploads) == 1, uploads
    fields = {f["name"]: f for f in uploads[0]["multipart"]}
    assert fields["purpose"]["value"] == "assistants", fields
    assert fields["file"]["size"] == len(data), fields
    provider_file_id = uploads[0]["response_id"]

    creates = mock.requests_for("vector_store_create")
    assert len(creates) == 1, creates
    vector_store_id = creates[0]["response_id"]
    adds = mock.requests_for("vector_store_add_file")
    assert len(adds) == 1, adds
    assert adds[0]["path"].endswith(f"/vector_stores/{vector_store_id}/files"), adds[0]["path"]
    assert adds[0]["json"]["file_id"] == provider_file_id, adds[0]["json"]
    assert adds[0]["json"].get("attributes") == {"attachment_id": attachment_id}, adds[0]["json"]
    assert provider_file_id in mock.vector_stores[vector_store_id]["files"]

    row = _attachment_row(mc, attachment_id)
    assert row["chat_id"] == chat_id and row["tenant_id"] == ident.tenant_id, row
    assert row["uploaded_by_user_id"] == ident.user_id, row
    assert (row["filename"], row["content_type"], row["size_bytes"]) == (filename, content_type, len(data)), row
    assert row["status"] == "ready" and row["error_code"] is None, row
    assert row["attachment_kind"] == "document", row
    assert row["storage_backend"] == PROVIDER_ID, row
    assert row["provider_file_id"] == provider_file_id, row
    assert (row["for_file_search"], row["for_code_interpreter"]) == (1, 0), row
    assert row["img_thumbnail"] is None and row["doc_summary"] is None, row
    assert row["deleted_at"] is None and row["cleanup_status"] is None, row

    stores = _vector_store_rows(mc, chat_id)
    assert len(stores) == 1, stores
    assert stores[0]["tenant_id"] == ident.tenant_id, stores
    assert stores[0]["vector_store_id"] == vector_store_id, stores
    assert stores[0]["provider"] == PROVIDER_ID, stores

    _assert_no_provider_ids(resp.text, provider_file_id, vector_store_id)

    got = mc.get(_attachment_path(chat_id, attachment_id), "A1")
    assert got.status_code == 200, got.text
    assert got.json() == body
    _assert_no_provider_ids(got.text, provider_file_id, vector_store_id)


def test_second_document_reuses_the_chat_vector_store(mc, mock):
    chat_id = _new_chat(mc)
    first = upload_ok(mc, chat_id, pdf_bytes(), "first.pdf")
    second = upload_ok(mc, chat_id, text_document(label="second"), "second.txt", TXT_MIME)

    creates = mock.requests_for("vector_store_create")
    assert len(creates) == 1, "one vector store per chat, created on the first document"
    vector_store_id = creates[0]["response_id"]
    adds = mock.requests_for("vector_store_add_file")
    assert [a["path"].endswith(f"/vector_stores/{vector_store_id}/files") for a in adds] == [True, True], adds
    assert [a["json"]["attributes"] for a in adds] == [
        {"attachment_id": first["id"]},
        {"attachment_id": second["id"]},
    ]
    assert len(_vector_store_rows(mc, chat_id)) == 1
    assert len(mock.vector_stores[vector_store_id]["files"]) == 2

    # Another chat gets its own store.
    other_chat = _new_chat(mc)
    upload_ok(mc, other_chat, pdf_bytes(), "other.pdf")
    creates = mock.requests_for("vector_store_create")
    assert len(creates) == 2, creates
    other_store = _vector_store_rows(mc, other_chat)
    assert len(other_store) == 1 and other_store[0]["vector_store_id"] == creates[1]["response_id"]
    assert other_store[0]["vector_store_id"] != vector_store_id


def test_provider_file_name_follows_the_chat_attachment_convention(mc, mock):
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, pdf_bytes(), "Quarterly Report.pdf")

    fields = {f["name"]: f for f in mock.requests_for("files_upload")[0]["multipart"]}
    assert fields["file"]["filename"] == f"{chat_id}_{body['id']}.pdf", fields["file"]


# --------------------------------------------------------------------------
# Images: thumbnail, no vector store
# --------------------------------------------------------------------------


def test_png_upload_is_ready_with_a_webp_thumbnail_and_no_vector_store(mc, mock):
    data = make_png(300, 150)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, data, "chart.png", "image/png")

    assert resp.status_code == 201, resp.text
    body = resp.json()
    assert_detail(body, filename="chart.png", content_type="image/png", size_bytes=len(data), kind="image")
    width, height, raw = assert_thumbnail(body.get("img_thumbnail"), source_size=(300, 150))
    assert (width, height) == (128, 64), body["img_thumbnail"]

    uploads = mock.requests_for("files_upload")
    assert len(uploads) == 1, uploads
    fields = {f["name"]: f for f in uploads[0]["multipart"]}
    assert fields["purpose"]["value"] == "assistants", fields
    assert fields["file"]["size"] == len(data), "only the original image is uploaded"
    assert mock.requests_for("vector_store_create") == []
    assert mock.requests_for("vector_store_add_file") == []
    assert _vector_store_rows(mc, chat_id) == []

    row = _attachment_row(mc, body["id"])
    assert row["attachment_kind"] == "image" and row["status"] == "ready", row
    assert (row["for_file_search"], row["for_code_interpreter"]) == (0, 0), row
    assert row["provider_file_id"] == uploads[0]["response_id"], row
    assert bytes(row["img_thumbnail"]) == raw
    assert (row["img_thumbnail_width"], row["img_thumbnail_height"]) == (width, height), row
    _assert_no_provider_ids(resp.text, uploads[0]["response_id"])

    got = mc.get(_attachment_path(chat_id, body["id"]), "A1")
    assert got.status_code == 200 and got.json() == body, got.text


def test_portrait_thumbnail_keeps_the_aspect_ratio(mc, mock):
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, make_png(100, 400), "tall.png", "image/png")

    width, height, _ = assert_thumbnail(body.get("img_thumbnail"), source_size=(100, 400))
    assert (width, height) == (32, 128), body["img_thumbnail"]


@pytest.mark.parametrize(
    ("filename", "content_type"),
    [("tiny.jpg", "image/jpeg"), ("tiny.webp", "image/webp"), ("tiny.gif", "image/gif"), ("tiny.png", "image/png")],
)
def test_every_image_type_is_an_image_attachment_with_thumbnail(mc, mock, filename, content_type):
    if content_type == "image/webp":
        # The repository's tiny.webp is not decodable (wrong RIFF size and a
        # VP8 token partition that ends early; libwebp rejects it too), so a
        # known-valid 1x1 lossy WebP is used instead.
        data = TINY_LOSSY_WEBP
        assert webp_dimensions(data) == (1, 1)
    else:
        data = load_testdata(f"images/{filename}")
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, data, filename, content_type)

    assert_detail(body, filename=filename, content_type=content_type, size_bytes=len(data), kind="image")
    assert_thumbnail(body.get("img_thumbnail"))
    assert mock.requests_for("vector_store_add_file") == []


def test_undecodable_image_is_ready_without_thumbnail(mc, mock):
    data = b"\x89PNG\r\n\x1a\n" + b"this is not a real png" * 10
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, data, "broken.png", "image/png")

    assert_detail(body, filename="broken.png", content_type="image/png", size_bytes=len(data), kind="image")
    assert body.get("img_thumbnail") is None, body
    assert body.get("error_code") is None, body
    row = _attachment_row(mc, body["id"])
    assert row["img_thumbnail"] is None and row["error_code"] is None and row["status"] == "ready", row


# --------------------------------------------------------------------------
# XLSX: code interpreter only
# --------------------------------------------------------------------------


def test_xlsx_upload_is_code_interpreter_only(mc, mock):
    data = xlsx_bytes()
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, data, "sales.xlsx", XLSX_MIME)

    assert_detail(body, filename="sales.xlsx", content_type=XLSX_MIME, size_bytes=len(data), kind="document")
    assert len(mock.requests_for("files_upload")) == 1
    assert mock.requests_for("vector_store_create") == []
    assert mock.requests_for("vector_store_add_file") == []
    row = _attachment_row(mc, body["id"])
    assert (row["for_file_search"], row["for_code_interpreter"]) == (0, 1), row
    assert row["status"] == "ready" and row["provider_file_id"], row
    assert _vector_store_rows(mc, chat_id) == []


def test_xlsx_on_model_without_code_interpreter_is_rejected(mc, mock):
    entry = next(e for e in mc.catalog if e["id"] == "gpt-text-only")
    assert entry["general_config"]["tool_support"]["code_interpreter"] is False
    chat_id = _new_chat(mc, model="gpt-text-only")

    resp = upload(mc, chat_id, xlsx_bytes(), "sales.xlsx", XLSX_MIME)

    assert_problem(
        resp,
        400,
        category="invalid_argument",
        field_name="file",
        field_reason="CODE_INTERPRETER_UNAVAILABLE",
        resource_type=RT_ATTACHMENT,
    )
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(mc, chat_id) == []


# --------------------------------------------------------------------------
# Filenames and MIME types
# --------------------------------------------------------------------------


def test_missing_filename_defaults_to_upload(mc, mock):
    chat_id = _new_chat(mc)
    data = text_document()

    body = upload_ok_raw(mc, chat_id, [{"name": "file", "content_type": TXT_MIME, "data": data}])

    assert body["filename"] == "upload", body
    assert body["content_type"] == TXT_MIME and body["size_bytes"] == len(data), body
    assert _attachment_row(mc, body["id"])["filename"] == "upload"


def test_long_filename_is_truncated_to_255_keeping_the_extension(mc, mock):
    chat_id = _new_chat(mc)
    filename = "q" * 296 + ".pdf"

    body = upload_ok(mc, chat_id, pdf_bytes(), filename)

    assert body["filename"] == "q" * 251 + ".pdf", (len(body["filename"]), body["filename"][-10:])
    assert len(body["filename"]) == 255
    assert _attachment_row(mc, body["id"])["filename"] == body["filename"]


@pytest.mark.parametrize(
    ("filename", "expected_type", "make", "kind"),
    [
        pytest.param("scan.pdf", PDF_MIME, pdf_bytes, "document", id="pdf"),
        pytest.param("sheet.xlsx", XLSX_MIME, xlsx_bytes, "document", id="xlsx"),
        pytest.param("photo.png", "image/png", lambda: make_png(40, 20), "image", id="png"),
    ],
)
def test_octet_stream_type_is_inferred_from_the_extension(mc, mock, filename, expected_type, make, kind):
    data = make()
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, data, filename, OCTET)

    assert_detail(body, filename=filename, content_type=expected_type, size_bytes=len(data), kind=kind)
    assert _attachment_row(mc, body["id"])["content_type"] == expected_type


def test_csv_is_accepted_as_plain_text(mc, mock):
    chat_id = _new_chat(mc)
    data = b"region,revenue\nnorth,120\nsouth,95\n"

    body = upload_ok(mc, chat_id, data, "revenue.csv", "text/csv")

    assert body["content_type"] == TXT_MIME, body
    assert body["kind"] == "document" and body["status"] == "ready", body


@pytest.mark.parametrize(
    ("filename", "content_type"),
    [
        pytest.param("setup.exe", "application/x-msdownload", id="msdownload"),
        pytest.param("blob.qqq", OCTET, id="octet-stream-unknown-extension"),
        pytest.param("noext", OCTET, id="octet-stream-no-extension"),
        pytest.param("movie.mp4", "video/mp4", id="video"),
    ],
)
def test_unsupported_content_type_is_rejected(mc, mock, filename, content_type):
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, b"MZ\x90\x00binary payload", filename, content_type)

    assert_problem(resp, 400, category="invalid_argument", field_reason="UNSUPPORTED_CONTENT_TYPE")
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(mc, chat_id) == []


# --------------------------------------------------------------------------
# Multipart errors
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "content_type",
    [
        pytest.param("multipart/form-data", id="multipart-without-boundary"),
        pytest.param("application/octet-stream", id="not-multipart"),
        pytest.param("application/json", id="json"),
    ],
)
def test_upload_without_multipart_boundary_is_boundary_required(mc, mock, content_type):
    chat_id = _new_chat(mc)

    resp = upload_raw(mc, chat_id, pdf_bytes(), content_type)

    assert_problem(
        resp, 400, category="invalid_argument", field_name="content_type", field_reason="BOUNDARY_REQUIRED"
    )
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(mc, chat_id) == []


def test_upload_without_file_part_is_missing_file(mc, mock):
    chat_id = _new_chat(mc)
    body, ct = multipart([{"name": "document", "filename": "report.pdf", "content_type": PDF_MIME,
                           "data": pdf_bytes()}, {"name": "note", "data": b"hello"}])

    resp = upload_raw(mc, chat_id, body, ct)

    assert_problem(resp, 400, category="invalid_argument", field_name="file", field_reason="MISSING_FILE")
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(mc, chat_id) == []


def test_file_part_without_content_type_is_missing_content_type(mc, mock):
    chat_id = _new_chat(mc)
    body, ct = multipart([{"name": "file", "filename": "report.pdf", "data": pdf_bytes()}])

    resp = upload_raw(mc, chat_id, body, ct)

    assert_problem(
        resp, 400, category="invalid_argument", field_name="content_type", field_reason="MISSING_CONTENT_TYPE"
    )
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(mc, chat_id) == []


def test_unreadable_multipart_body_is_multipart_error(mc, mock):
    chat_id = _new_chat(mc)
    boundary = "mc-e2e-broken"
    body = (
        f"--{boundary}\r\nContent-Disposition form-data name file\r\n\r\nno header colon\r\n--{boundary}--\r\n"
    ).encode()

    resp = upload_raw(mc, chat_id, body, f"multipart/form-data; boundary={boundary}")

    assert_problem(resp, 400, category="invalid_argument", field_name="multipart", field_reason="MULTIPART_ERROR")
    assert mock.requests_for("files_upload") == []


# --------------------------------------------------------------------------
# Chat resolution: 404 and INVALID_MODEL before the body
# --------------------------------------------------------------------------


@pytest.mark.parametrize("case", ["unknown", "other-user", "other-tenant", "deleted"])
def test_upload_into_unreachable_chat_is_chat_not_found(mc, mock, case):
    if case == "unknown":
        chat_id, user = new_uuid(), "A1"
    else:
        chat_id = _new_chat(mc, "A1")
        user = {"other-user": "A2", "other-tenant": "B1", "deleted": "A1"}[case]
        if case == "deleted":
            assert mc.delete(f"{CHATS}/{chat_id}", "A1").status_code == 204

    resp = upload(mc, chat_id, pdf_bytes(), user=user)

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)
    assert mock.requests_for("files_upload") == []
    assert mc.query("SELECT COUNT(*) AS n FROM attachments WHERE chat_id = ?", (db_uuid(chat_id),))[0]["n"] == 0


def test_upload_with_non_uuid_chat_id_is_invalid_path_params(mc, mock):
    resp = upload(mc, "not-a-uuid", pdf_bytes())

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")
    assert mock.requests_for("files_upload") == []


@pytest.mark.slow
def test_upload_into_chat_whose_model_left_the_catalog_is_invalid_model(start_server, mock):
    first = start_server(name="upload-catalog-full")
    chat_id = _new_chat(first, model="gpt-4.1-mini")
    home_dir = str(first.home_dir)
    first.stop()

    pruned = [e for e in default_model_catalog() if e["id"] != "gpt-4.1-mini"]
    second = start_server(
        {"server": {"home_dir": home_dir}, **_policy(model_catalog=pruned)}, name="upload-catalog-pruned"
    )
    assert second.db_path == first.db_path

    resp = upload(second, chat_id, pdf_bytes())

    assert_problem(resp, 400, category="invalid_argument", field_name="model", field_reason="INVALID_MODEL")
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(second, chat_id) == []


# --------------------------------------------------------------------------
# Kill switches
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_image_upload_with_disable_images_is_feature_disabled(killswitch_server, mock):
    server = killswitch_server
    chat_id = _new_chat(server)

    resp = upload(server, chat_id, make_png(32, 32), "photo.png", "image/png")

    assert_problem(
        resp, 400, category="failed_precondition", violation_subject="images", violation_type="FEATURE_DISABLED"
    )
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(server, chat_id) == []


@pytest.mark.slow
def test_xlsx_upload_with_disable_code_interpreter_is_rejected(killswitch_server, mock):
    server = killswitch_server
    chat_id = _new_chat(server)

    resp = upload(server, chat_id, xlsx_bytes(), "sales.xlsx", XLSX_MIME)

    assert_problem(
        resp,
        400,
        category="invalid_argument",
        field_name="file",
        field_reason="CODE_INTERPRETER_UNAVAILABLE",
        resource_type=RT_ATTACHMENT,
    )
    assert mock.requests_for("files_upload") == []
    assert _chat_attachment_rows(server, chat_id) == []

    # Documents are unaffected by both kill switches.
    body = upload_ok(server, chat_id, pdf_bytes(), "still-ok.pdf")
    assert body["status"] == "ready" and body["kind"] == "document", body


# --------------------------------------------------------------------------
# Size and per-chat limits
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_image_above_uploaded_image_max_size_is_file_too_large(limits_server, mock):
    server = limits_server
    chat_id = _new_chat(server)
    data = make_png(200, 200, noise=True)
    assert len(data) > LIMITS_RAG["uploaded_image_max_size_kb"] * 1024

    resp = upload(server, chat_id, data, "noise.png", "image/png")

    assert_problem(
        resp, 400, category="out_of_range", field_name="content_length", field_reason="FILE_TOO_LARGE"
    )
    assert [r for r in _chat_attachment_rows(server, chat_id) if r["status"] == "ready"] == []

    # A small image and a document of the same size are accepted (documents
    # have their own limit, uploaded_file_max_size_kb).
    upload_ok(server, chat_id, make_png(16, 16), "small.png", "image/png")
    upload_ok(server, chat_id, text_document(len(data)), "same-size.txt", TXT_MIME)


@pytest.mark.slow
def test_document_above_uploaded_file_max_size_is_file_too_large(limits_server, mock):
    server = limits_server
    chat_id = _new_chat(server)
    data = text_document(LIMITS_RAG["uploaded_file_max_size_kb"] * 1024 + 1)

    resp = upload(server, chat_id, data, "big.txt", TXT_MIME)

    assert_problem(
        resp, 400, category="out_of_range", field_name="content_length", field_reason="FILE_TOO_LARGE"
    )
    assert [r for r in _chat_attachment_rows(server, chat_id) if r["status"] == "ready"] == []
    assert mock.requests_for("vector_store_add_file") == []


@pytest.mark.slow
def test_document_count_above_max_documents_per_chat_is_document_limit(limits_server, mock):
    server = limits_server
    chat_id = _new_chat(server)
    first = upload_ok(server, chat_id, text_document(label="one"), "one.txt", TXT_MIME)
    upload_ok(server, chat_id, text_document(label="two"), "two.txt", TXT_MIME)
    adds_before = len(mock.requests_for("vector_store_add_file"))

    resp = upload(server, chat_id, text_document(label="three"), "three.txt", TXT_MIME)

    problem = assert_problem(resp, 429, category="resource_exhausted", violation_subject="document_limit")
    assert problem["context"]["violations"][0]["subject"] == "document_limit", problem
    assert len(mock.requests_for("vector_store_add_file")) == adds_before
    ready_docs = [r for r in _chat_attachment_rows(server, chat_id) if r["status"] == "ready"]
    assert len(ready_docs) == 2, ready_docs

    # Images do not count as documents.
    upload_ok(server, chat_id, make_png(16, 16), "img.png", "image/png")

    # Deleted documents do not count.
    assert server.delete(_attachment_path(chat_id, first["id"]), "A1").status_code == 204
    upload_ok(server, chat_id, text_document(label="four"), "four.txt", TXT_MIME)

    # The limit is per chat.
    other_chat = _new_chat(server)
    upload_ok(server, other_chat, text_document(label="other"), "other.txt", TXT_MIME)


@pytest.mark.slow
def test_total_size_above_max_total_upload_mb_per_chat_is_storage_limit(storage_server, mock):
    server = storage_server
    chat_id = _new_chat(server)
    upload_ok(server, chat_id, text_document(600_000, "first"), "first.txt", TXT_MIME)
    uploads_before = len(mock.requests_for("files_upload"))

    resp = upload(server, chat_id, text_document(600_000, "second"), "second.txt", TXT_MIME)

    problem = assert_problem(resp, 429, category="resource_exhausted", violation_subject="storage_limit")
    assert problem["context"]["violations"][0]["subject"] == "storage_limit", problem
    # The limits are enforced when the row is inserted, before the provider upload.
    assert len(mock.requests_for("files_upload")) == uploads_before
    ready = [r for r in _chat_attachment_rows(server, chat_id) if r["status"] == "ready"]
    assert sum(r["size_bytes"] for r in ready) == 600_000, ready

    # Images count towards the total as well.
    image = make_png(520, 300, noise=True)
    assert 600_000 + len(image) > 1024 * 1024 and len(image) < 1_000_000, len(image)
    resp = upload(server, chat_id, image, "noise.png", "image/png")
    assert_problem(resp, 429, category="resource_exhausted", violation_subject="storage_limit")
    assert len(mock.requests_for("files_upload")) == uploads_before

    # Another chat has its own budget.
    upload_ok(server, _new_chat(server), text_document(600_000, "other"), "other.txt", TXT_MIME)


@pytest.mark.slow
def test_upload_over_max_concurrent_uploads_is_service_unavailable(limits_server, mock):
    server = limits_server
    mock.set_indexing(initial="in_progress", polls=5, final="completed")
    slow_chat = _new_chat(server)
    outcome: dict[str, Any] = {}

    def slow_upload() -> None:
        outcome["resp"] = upload(server, slow_chat, text_document(label="slow"), "slow.txt", TXT_MIME)

    worker = threading.Thread(target=slow_upload, name="slow-upload", daemon=True)
    worker.start()
    try:
        wait_until(lambda: mock.requests_for("vector_store_add_file"), timeout=15, description="slow upload indexing")

        resp = upload(server, _new_chat(server), text_document(label="fast"), "fast.txt", TXT_MIME)

        assert_service_unavailable(resp, CONCURRENCY_RETRY_AFTER)
    finally:
        worker.join(timeout=40)
    assert not worker.is_alive()
    assert outcome["resp"].status_code == 201, outcome["resp"].text
    assert outcome["resp"].json()["status"] == "ready"

    # The slot is free again.
    upload_ok(server, _new_chat(server), text_document(label="after"), "after.txt", TXT_MIME)


# --------------------------------------------------------------------------
# Provider mismatch
# --------------------------------------------------------------------------


def _seed_vector_store(server, chat_id: str, tenant_id: str, provider: str, vector_store_id: str) -> None:
    conn = server.sqlite(readonly=False)
    try:
        conn.execute(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, file_count, "
            "created_at) VALUES (?, ?, ?, ?, ?, 0, ?)",
            (uuid_bytes(new_uuid()), uuid_bytes(tenant_id), uuid_bytes(chat_id), vector_store_id, provider, db_ts()),
        )
        conn.commit()
    finally:
        conn.close()


def test_document_upload_into_store_of_another_backend_is_provider_mismatch(mc, mock):
    ident = mc.identity("A1")
    chat_id = _new_chat(mc)
    _seed_vector_store(mc, chat_id, ident.tenant_id, "azure-legacy", "vs_seededOtherBackend0001")

    resp = upload(mc, chat_id, pdf_bytes())

    assert_problem(resp, 409, category="already_exists", resource_name="provider_mismatch")
    assert mock.requests_for("vector_store_create") == []
    assert mock.requests_for("vector_store_add_file") == []
    assert [r for r in _chat_attachment_rows(mc, chat_id) if r["status"] == "ready"] == []
    _assert_no_provider_ids(resp.text, "vs_seededOtherBackend0001")


# --------------------------------------------------------------------------
# Indexing: polling, deadline, failures
# --------------------------------------------------------------------------


def test_indexing_in_progress_is_polled_until_completed_within_the_request(mc, mock):
    mock.set_indexing(initial="in_progress", polls=3, final="completed")
    chat_id = _new_chat(mc)

    started = time.monotonic()
    body = upload_ok(mc, chat_id, pdf_bytes())
    elapsed = time.monotonic() - started

    assert body["status"] == "ready", body
    polls = mock.requests_for("vector_store_file_get")
    assert len(polls) == 4, f"3 in_progress reads then completed: {len(polls)}"
    provider_file_id = mock.requests_for("files_upload")[0]["response_id"]
    assert all(p["path"].endswith(f"/files/{provider_file_id}") for p in polls), [p["path"] for p in polls]
    # Waits 250 ms, 500 ms, 1 s and 2 s before the four reads.
    assert elapsed >= 3.5, elapsed
    assert _attachment_row(mc, body["id"])["status"] == "ready"


def test_transient_status_read_errors_keep_polling(mc, mock):
    mock.set_indexing(initial="in_progress", polls=1, final="completed")
    mock.fail_next("vector_store_file_get", 500, times=2)
    chat_id = _new_chat(mc)

    body = upload_ok(mc, chat_id, pdf_bytes())

    assert body["status"] == "ready", body
    assert len(mock.requests_for("vector_store_file_get")) == 4


@pytest.mark.slow
@pytest.mark.timeout(240)
def test_indexing_still_running_at_the_deadline_returns_uploaded_then_ready(mc, mock):
    # ~14 reads fit into the 25 s request; 20 in_progress reads outlast it.
    mock.set_indexing(initial="in_progress", polls=20, final="completed")
    chat_id = _new_chat(mc)

    started = time.monotonic()
    resp = upload(mc, chat_id, pdf_bytes(), timeout=60)
    elapsed = time.monotonic() - started

    assert resp.status_code == 201, resp.text
    body = resp.json()
    assert_detail(body, filename="report.pdf", content_type=PDF_MIME, size_bytes=len(pdf_bytes()),
                  kind="document", status="uploaded")
    assert INDEXING_DEADLINE_SECS - 1 <= elapsed < 30, elapsed
    row = _attachment_row(mc, body["id"])
    assert row["status"] == "uploaded" and row["provider_file_id"], row

    # Not ready yet: a message cannot reference it.
    resp = mc.post(
        f"{CHATS}/{chat_id}/messages:stream",
        "A1",
        json={"content": "summarize", "attachment_ids": [body["id"]]},
    )
    assert_problem(resp, 400, category="invalid_argument", field_name="attachment", field_reason="invalid_attachment")

    def ready() -> dict[str, Any] | None:
        got = mc.get(_attachment_path(chat_id, body["id"]), "A1")
        assert got.status_code == 200, got.text
        detail = got.json()
        assert detail["status"] in ("uploaded", "ready"), detail
        return detail if detail["status"] == "ready" else None

    detail = wait_until(ready, timeout=120, interval=1.0, description="background indexing to finish")
    assert {k: v for k, v in detail.items() if k != "status"} == {k: v for k, v in body.items() if k != "status"}
    assert _attachment_row(mc, body["id"])["status"] == "ready"


@pytest.mark.parametrize(
    ("initial", "polls", "final"),
    [
        pytest.param("failed", 0, "failed", id="failed-on-add"),
        pytest.param("in_progress", 1, "failed", id="failed-after-polling"),
        pytest.param("in_progress", 0, "cancelled", id="cancelled"),
    ],
)
def test_failed_indexing_is_service_unavailable_and_deletes_the_provider_file(mc, mock, initial, polls, final):
    mock.set_indexing(initial=initial, polls=polls, final=final)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, pdf_bytes())

    assert_service_unavailable(resp, UPLOAD_RETRY_AFTER)
    assert "indexing_failed" not in resp.text
    provider_file_id = mock.requests_for("files_upload")[0]["response_id"]
    _assert_no_provider_ids(resp.text, provider_file_id)
    rows = _chat_attachment_rows(mc, chat_id)
    assert len(rows) == 1, rows
    assert (rows[0]["status"], rows[0]["error_code"]) == ("failed", "indexing_failed"), rows[0]
    _wait_provider_file_delete(mock, provider_file_id)
    assert len(mock.requests_for("files_upload")) == 1, "provider writes are not retried"

    got = mc.get(_attachment_path(chat_id, rows[0]["id"]), "A1")
    assert got.status_code == 200, got.text
    detail = got.json()
    assert detail["status"] == "failed" and detail["error_code"] == "indexing_failed", detail
    assert detail.get("img_thumbnail") is None, detail
    _assert_no_provider_ids(got.text, provider_file_id)


def test_non_transient_status_read_error_fails_indexing(mc, mock):
    mock.set_indexing(initial="in_progress", polls=3, final="completed")
    mock.fail_next("vector_store_file_get", 400)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, pdf_bytes())

    assert_service_unavailable(resp, UPLOAD_RETRY_AFTER)
    rows = _chat_attachment_rows(mc, chat_id)
    assert (rows[0]["status"], rows[0]["error_code"]) == ("failed", "indexing_failed"), rows


def test_failed_add_to_vector_store_is_indexing_failed(mc, mock):
    mock.fail_next("vector_store_add_file", 500)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, pdf_bytes())

    assert_service_unavailable(resp, UPLOAD_RETRY_AFTER)
    rows = _chat_attachment_rows(mc, chat_id)
    assert (rows[0]["status"], rows[0]["error_code"]) == ("failed", "indexing_failed"), rows
    provider_file_id = mock.requests_for("files_upload")[0]["response_id"]
    _wait_provider_file_delete(mock, provider_file_id)
    assert len(mock.requests_for("vector_store_add_file")) == 1, "provider writes are not retried"


def test_provider_file_upload_failure_is_service_unavailable_and_upload_failed(mc, mock):
    mock.fail_next("files_upload", 500)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, pdf_bytes())

    assert_service_unavailable(resp, UPLOAD_RETRY_AFTER)
    assert len(mock.requests_for("files_upload")) == 1, "provider writes are not retried"
    assert mock.requests_for("vector_store_add_file") == []
    rows = _chat_attachment_rows(mc, chat_id)
    assert len(rows) == 1, rows
    assert (rows[0]["status"], rows[0]["error_code"]) == ("failed", "upload_failed"), rows[0]
    assert rows[0]["provider_file_id"] is None, rows[0]

    got = mc.get(_attachment_path(chat_id, rows[0]["id"]), "A1")
    assert got.status_code == 200, got.text
    detail = got.json()
    assert_detail(detail, filename="report.pdf", content_type=PDF_MIME, size_bytes=rows[0]["size_bytes"],
                  kind="document", status="failed")
    assert detail["error_code"] == "upload_failed", detail

    # A retry is a new upload with a new attachment.
    retried = upload_ok(mc, chat_id, pdf_bytes())
    assert retried["id"] != rows[0]["id"]


def test_failed_vector_store_create_removes_the_placeholder(mc, mock):
    mock.fail_next("vector_store_create", 500)
    chat_id = _new_chat(mc)

    resp = upload(mc, chat_id, pdf_bytes())

    assert_service_unavailable(resp, UPLOAD_RETRY_AFTER)
    assert mock.requests_for("vector_store_add_file") == []
    wait_until(lambda: _vector_store_rows(mc, chat_id) == [], timeout=10, description="placeholder removal")
    rows = _chat_attachment_rows(mc, chat_id)
    assert [r["status"] for r in rows] == ["failed"], rows

    body = upload_ok(mc, chat_id, pdf_bytes(), "second-try.pdf")
    assert body["status"] == "ready", body
    creates = mock.requests_for("vector_store_create")
    assert len(creates) == 2, creates
    stores = _vector_store_rows(mc, chat_id)
    assert len(stores) == 1 and stores[0]["vector_store_id"] == creates[1]["response_id"], stores


# --------------------------------------------------------------------------
# GET errors
# --------------------------------------------------------------------------


def test_get_unknown_attachment_is_attachment_not_found(mc, mock):
    chat_id = _new_chat(mc)

    resp = mc.get(_attachment_path(chat_id, new_uuid()), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)


def test_get_attachment_of_another_chat_is_attachment_not_found(mc, mock):
    chat_id = _new_chat(mc)
    other_chat = _new_chat(mc)
    body = upload_ok(mc, other_chat, pdf_bytes())

    resp = mc.get(_attachment_path(chat_id, body["id"]), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)


def test_get_attachment_uploaded_by_another_user_in_the_callers_chat_is_not_found(mc, mock):
    a2 = mc.identity("A2")
    chat_id = _new_chat(mc, "A1")
    foreign = seed_attachment(mc, chat_id, a2.tenant_id, a2.user_id, filename="a2.pdf")

    resp = mc.get(_attachment_path(chat_id, foreign), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)


def test_get_soft_deleted_attachment_is_not_found(mc, mock):
    a1 = mc.identity("A1")
    chat_id = _new_chat(mc, "A1")
    deleted = seed_attachment(mc, chat_id, a1.tenant_id, a1.user_id, deleted=True)

    resp = mc.get(_attachment_path(chat_id, deleted), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_ATTACHMENT)


@pytest.mark.parametrize("user", ["A2", "B1"])
def test_get_attachment_in_another_users_chat_is_not_found(mc, mock, user):
    chat_id = _new_chat(mc, "A1")
    body = upload_ok(mc, chat_id, pdf_bytes())

    resp = mc.get(_attachment_path(chat_id, body["id"]), user)

    assert_problem(resp, 404, category="not_found")
    assert body["filename"] not in resp.text


@pytest.mark.parametrize(
    "path",
    [
        pytest.param("{chat}/attachments/not-a-uuid", id="attachment-id"),
        pytest.param("not-a-uuid/attachments/{attachment}", id="chat-id"),
    ],
)
def test_get_attachment_with_non_uuid_path_parameter_is_invalid_path_params(mc, mock, path):
    chat_id = _new_chat(mc)
    resolved = path.format(chat=chat_id, attachment=new_uuid())

    resp = mc.get(f"{CHATS}/{resolved}", "A1")

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_get_failed_attachment_reports_its_error_code(mc, mock):
    a1 = mc.identity("A1")
    chat_id = _new_chat(mc)
    failed = seed_attachment(mc, chat_id, a1.tenant_id, a1.user_id, status="failed", provider_file_id=None,
                             filename="broken.pdf", size_bytes=2048)

    resp = mc.get(_attachment_path(chat_id, failed), "A1")

    assert resp.status_code == 200, resp.text
    detail = resp.json()
    assert_detail(detail, filename="broken.pdf", content_type=PDF_MIME, size_bytes=2048, kind="document",
                  status="failed")
    assert detail["error_code"] == "upload_failed", detail
    assert detail["id"] == failed
