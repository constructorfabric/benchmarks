"""Attachments black-box: upload / get / delete of documents and images, size / type / per-chat limit
validation, the provider wire (Files API through OAGW, per-chat vector store), the asynchronous
indexing lifecycle, provider failures and the message links.

The server runs against the mock provider of ``conftest.py`` (Responses, Files and Vector Stores
APIs behind a provisioned OAGW upstream); every provider request is recorded. Limit tests run on the
``attachments_small`` stack (tiny limits, ``attachment_support.small_overrides``).

Source: DESIGN 3.6 "File Upload" (sequence and description), 3.3 "Get Attachment" / "Upload
Attachment" (error table), REST error mapping, 3.5 "Provider API Mapping" (``purpose =
assistants``), 2.2 kill switches, B.7 / B.8 (``rag.*``, ``thumbnail.*``), "Attachment Mutability
and Deletion", 3.7 ``attachments`` / ``chat_vector_stores`` / ``message_attachments``, PRD "Image
Upload" and the MIME allowlist. Cleanup, the reaper and the tool wiring live in
``test_attachment_cleanup.py`` and ``test_attachment_tools.py``.
"""

from __future__ import annotations

import concurrent.futures
import re
import time
import uuid
from types import SimpleNamespace

import pytest

from . import catalog
from .attachment_support import (
    ATTACHMENT_QUEUE,
    AttachmentSeeder,
    CHAT_CLEANUP_QUEUE,
    DOCX,
    PDF,
    PDF_BYTES,
    PPTX,
    SMALL_FILE_KB,
    SMALL_IMAGE_KB,
    SMALL_MAX_DOCUMENTS,
    XLSX,
    XLSX_BYTES,
    assert_exhausted,
    assert_resource_type,
    assert_unavailable,
    decode_thumbnail,
    delete_attachment,
    filler,
    get_attachment,
    list_messages,
    multipart_body,
    png_bytes,
    upload,
    upload_document,
    upload_image,
    upload_raw,
    uploaded,
    webp_size,
)
from .mock_openai_provider import (
    FILE,
    FILES,
    VS_FILE,
    VS_FILES,
    VECTOR_STORES,
    error_response,
    vs_file_response,
    vs_file_without_status,
)
from .support import (
    MiniChatApi,
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    assert_field_violation,
    assert_precondition,
    assert_problem,
    field_violation_reasons,
    parse_ts,
    send_message,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
BASE = "/mini-chat"
PROVIDER_SECRET = "provider-secret-detail-4711"


def new_chat(attach, model=PREMIUM, *, tenant=TENANT_A_ID, user=USER_A_ID):
    return attach.insert_chat(tenant=tenant, user=user, model=model)


def normalise(value: str) -> str:
    return value.replace("-", "").lower()


def assert_provider_filename(name: str, chat: str, attachment: str, ext: str) -> None:
    """``{chat_id}_{attachment_id}.{ext}`` with either UUID spelling."""
    stem, _, got_ext = name.rpartition(".")
    chat_part, _, attachment_part = stem.partition("_")
    assert normalise(chat_part) == normalise(chat), name
    assert normalise(attachment_part) == normalise(attachment), name
    assert got_ext == ext, name


def assert_no_provider_traffic(mock) -> None:
    assert not mock.storage_requests(), [(r.method, r.path) for r in mock.storage_requests()]


def assert_nothing_stored(attach, chat: str) -> None:
    assert not attach.live_rows_of(chat), [dict(r) for r in attach.live_rows_of(chat)]


@pytest.fixture()
def sm(small):
    """An api client and a seeder of the small stack (limits: ``attachment_support.small_overrides``)."""
    api = MiniChatApi(small.server.base_url)
    try:
        yield SimpleNamespace(api=api, attach=AttachmentSeeder(small.server.db_path),
                              mock=small.mock, server=small.server)
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# the 201 contract
# --------------------------------------------------------------------------- #


def test_a_document_upload_is_201_with_location_and_the_attachment_detail(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload(api, chat, "Quarterly Report.pdf", PDF, PDF_BYTES)

    assert resp.status_code == 201, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    body = resp.json()
    assert resp.headers["location"] == f"{BASE}/v1/chats/{chat}/attachments/{body['id']}"
    uuid.UUID(body["id"])
    assert body["filename"] == "Quarterly Report.pdf"
    assert body["content_type"] == PDF
    assert body["size_bytes"] == len(PDF_BYTES)
    assert body["status"] == "ready"
    assert body["kind"] == "document"
    parse_ts(body["created_at"])
    for key in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert body.get(key) is None, f"`{key}` is omitted for a ready document: {body}"
    assert "provider_file_id" not in body and "storage_backend" not in body
    assert not re.search(r"file[-_][A-Za-z0-9]{6,}|vs_[A-Za-z0-9]{6,}", resp.text), resp.text


def test_the_upload_stores_the_row_with_its_routing_flags(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "notes.pdf")

    row = attach.row(attachment)
    assert row["status"] == "ready" and row["attachment_kind"] == "document"
    assert row["filename"] == "notes.pdf" and row["content_type"] == PDF
    assert row["size_bytes"] == len(PDF_BYTES)
    assert (row["for_file_search"], row["for_code_interpreter"]) == (1, 0)
    assert row["storage_backend"] == "openai"
    assert row["provider_file_id"] and row["provider_file_id"].startswith("file-")
    assert row["error_code"] is None and row["cleanup_status"] is None
    assert row["deleted_at"] is None and row["img_thumbnail"] is None
    assert row["secondary_status"] == "not_attempted" and row["secondary_file_id"] is None
    assert attach.all_links(chat) == [], "an upload links nothing to a message"


def test_a_document_goes_through_the_files_api_then_the_chat_vector_store(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "contract.pdf")
    row = attach.row(attachment)

    uploads = mock_provider.file_uploads()
    assert len(uploads) == 1
    upload_request = uploads[0]
    assert upload_request.path == "/v1/files"
    assert upload_request.header("content-type", "").startswith("multipart/form-data")
    assert upload_request.part("purpose")["text"] == "assistants", "purpose=assistants for documents"
    file_part = upload_request.part("file")
    assert file_part["content_type"] == PDF
    assert file_part["data"] == PDF_BYTES, "the bytes are forwarded unchanged"
    assert_provider_filename(file_part["filename"], chat, attachment, "pdf")

    creates = mock_provider.vector_store_creates()
    assert len(creates) == 1 and creates[0].path == "/v1/vector_stores"
    store = attach.vector_store_of(chat)
    adds = mock_provider.vector_store_adds()
    assert len(adds) == 1
    assert adds[0].path == f"/v1/vector_stores/{store}/files"
    assert adds[0].json["file_id"] == row["provider_file_id"]
    assert adds[0].json["attributes"] == {"attachment_id": attachment}, \
        "the single attribute of a vector store file is the attachment id"
    stores = attach.stores_of(chat)
    assert len(stores) == 1 and stores[0]["provider"] == "openai"
    order = [(r.method, r.route) for r in mock_provider.storage_requests()]
    assert order[0] == ("POST", "POST /v1/files")
    assert order[1] == ("POST", "POST /v1/vector_stores")
    assert order[2] == ("POST", "POST /v1/vector_stores/{vs_id}/files")


def test_the_second_document_of_a_chat_reuses_its_vector_store(api, attach, mock_provider):
    chat = new_chat(attach)
    for name in ("one.pdf", "two.pdf", "three.pdf"):
        upload_document(api, chat, name)
    assert len(mock_provider.file_uploads()) == 3
    assert len(mock_provider.vector_store_creates()) == 1, "created lazily, once"
    assert len(mock_provider.vector_store_adds()) == 3
    assert len(attach.stores_of(chat)) == 1


def test_every_chat_gets_its_own_vector_store(api, attach, mock_provider):
    first, second = new_chat(attach), new_chat(attach)
    upload_document(api, first)
    upload_document(api, second)
    assert attach.vector_store_of(first) != attach.vector_store_of(second)
    stores = {r.path.split("/")[3] for r in mock_provider.vector_store_adds()}
    assert stores == {attach.vector_store_of(first), attach.vector_store_of(second)}


def test_an_xlsx_goes_to_the_code_interpreter_only(api, attach, mock_provider):
    chat = new_chat(attach)
    body = uploaded(api, chat, "sales.xlsx", XLSX, XLSX_BYTES)

    assert body["status"] == "ready" and body["kind"] == "document" and body["content_type"] == XLSX
    assert [r.route for r in mock_provider.storage_requests()] == ["POST /v1/files"], \
        "no vector store, no add: an XLSX is not indexed"
    row = attach.row(body["id"])
    assert (row["for_code_interpreter"], row["for_file_search"]) == (1, 0)
    assert attach.stores_of(chat) == []
    upload_request = mock_provider.file_uploads()[0]
    assert upload_request.part("purpose")["text"] == "assistants"
    assert upload_request.part("file")["content_type"] == XLSX


def test_an_image_is_uploaded_with_purpose_assistants_and_never_indexed(api, attach, mock_provider):
    chat = new_chat(attach)
    data = png_bytes(400, 200)
    body = uploaded(api, chat, "photo.png", "image/png", data)

    assert body["status"] == "ready" and body["kind"] == "image"
    assert [r.route for r in mock_provider.storage_requests()] == ["POST /v1/files"]
    upload_request = mock_provider.file_uploads()[0]
    assert upload_request.part("purpose")["text"] == "assistants"
    assert upload_request.part("file")["data"] == data
    assert_provider_filename(upload_request.part("file")["filename"], chat, body["id"], "png")
    row = attach.row(body["id"])
    assert (row["for_file_search"], row["for_code_interpreter"]) == (0, 0)
    assert attach.stores_of(chat) == []


# --------------------------------------------------------------------------- #
# thumbnails
# --------------------------------------------------------------------------- #


def test_an_image_gets_a_webp_thumbnail_that_fits_inside_the_box(api, attach, mock_provider):
    chat = new_chat(attach)
    body = uploaded(api, chat, "wide.png", "image/png", png_bytes(400, 200))

    thumbnail = body["img_thumbnail"]
    assert thumbnail["content_type"] == "image/webp"
    data = decode_thumbnail(thumbnail)
    assert (thumbnail["width"], thumbnail["height"]) == (128, 64), "aspect ratio kept, no cropping"
    assert webp_size(data) == (128, 64)
    assert len(data) <= 131_072
    row = attach.row(body["id"])
    assert bytes(row["img_thumbnail"]) == data
    assert (row["img_thumbnail_width"], row["img_thumbnail_height"]) == (128, 64)
    assert len(mock_provider.file_uploads()[0].parts) == 2, "the thumbnail never goes to the provider"


def test_the_thumbnail_box_follows_the_thumbnail_config(sm):
    chat = new_chat(sm.attach)
    body = uploaded(sm.api, chat, "wide.png", "image/png", png_bytes(400, 200))
    assert (body["img_thumbnail"]["width"], body["img_thumbnail"]["height"]) == (32, 16)


def test_a_thumbnail_that_cannot_be_made_leaves_the_image_ready_without_it(api, attach, mock_provider):
    chat = new_chat(attach)
    body = uploaded(api, chat, "broken.png", "image/png", b"this is not an image at all")
    assert body["status"] == "ready"
    assert body.get("img_thumbnail") is None and body.get("error_code") is None
    row = attach.row(body["id"])
    assert row["img_thumbnail"] is None and row["error_code"] is None


# --------------------------------------------------------------------------- #
# content types
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("content_type,kind", [
    (PDF, "document"), (DOCX, "document"), (PPTX, "document"), (XLSX, "document"),
    ("text/plain", "document"), ("text/markdown", "document"), ("text/html", "document"),
    ("application/json", "document"),
    ("image/png", "image"), ("image/jpeg", "image"), ("image/webp", "image"), ("image/gif", "image"),
])
def test_every_allowlisted_type_is_accepted_with_its_kind(api, attach, mock_provider, content_type,
                                                          kind):
    chat = new_chat(attach)
    body = uploaded(api, chat, "file.bin", content_type, b"some bytes")
    assert body["kind"] == kind and body["status"] == "ready" and body["content_type"] == content_type


@pytest.mark.parametrize("content_type", [
    "application/zip", "application/x-msdownload", "video/mp4", "image/svg+xml", "image/tiff",
    "audio/mpeg", "application/octet-stream"])
def test_an_unsupported_type_is_400_unsupported_content_type_and_nothing_is_stored(
        api, attach, mock_provider, content_type):
    chat = new_chat(attach)
    resp = upload(api, chat, "payload.dat", content_type, b"bytes")
    problem = assert_problem(resp, 400, "invalid_argument")
    assert "UNSUPPORTED_CONTENT_TYPE" in field_violation_reasons(problem)
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)
    assert_nothing_stored(attach, chat)


@pytest.mark.parametrize("filename,inferred", [
    ("report.pdf", PDF), ("memo.docx", DOCX), ("deck.pptx", PPTX), ("numbers.xlsx", XLSX),
    ("shot.png", "image/png"), ("shot.jpg", "image/jpeg"), ("README.md", "text/markdown")])
def test_an_octet_stream_part_gets_its_type_from_the_file_extension(api, attach, mock_provider,
                                                                    filename, inferred):
    chat = new_chat(attach)
    body = uploaded(api, chat, filename, "application/octet-stream", b"opaque bytes")
    assert body["content_type"] == inferred


@pytest.mark.parametrize("filename", ["blob.bin", "no_extension", "archive.exe"])
def test_an_octet_stream_part_with_an_unknown_extension_is_unsupported(api, attach, mock_provider,
                                                                       filename):
    chat = new_chat(attach)
    resp = upload(api, chat, filename, "application/octet-stream", b"opaque bytes")
    problem = assert_problem(resp, 400, "invalid_argument")
    assert "UNSUPPORTED_CONTENT_TYPE" in field_violation_reasons(problem)
    assert_no_provider_traffic(mock_provider)


@pytest.mark.parametrize("filename,content_type", [
    ("main.rs", "text/x-rust"), ("main.go", "text/x-go"), ("schema.sql", "application/sql"),
    ("schema.sql", "text/x-sql"), ("lib.rs", "application/octet-stream"),
    ("util.go", "application/octet-stream"), ("query.sql", "application/octet-stream")])
def test_rust_go_and_sql_source_is_a_document_uploaded_as_plain_text(api, attach, mock_provider,
                                                                     filename, content_type):
    chat = new_chat(attach)
    body = uploaded(api, chat, filename, content_type, b"source code")
    assert body["kind"] == "document" and body["status"] == "ready"
    uploads = mock_provider.file_uploads()
    assert len(uploads) == 1
    file_part = uploads[0].part("file")
    assert file_part["content_type"] == "text/plain" and file_part["data"] == b"source code"


def test_csv_is_accepted_as_plain_text(api, attach, mock_provider):
    chat = new_chat(attach)
    body = uploaded(api, chat, "data.csv", "text/csv", b"a,b\n1,2\n")
    assert body["content_type"] == "text/plain" and body["kind"] == "document"
    assert mock_provider.file_uploads()[0].part("file")["content_type"] == "text/plain"
    inferred = uploaded(api, new_chat(attach), "more.csv", "application/octet-stream", b"a,b\n")
    assert inferred["content_type"] == "text/plain"


def test_a_missing_filename_defaults_to_upload(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload(api, chat, None, "text/plain", b"anonymous")
    assert resp.status_code == 201, resp.text
    assert resp.json()["filename"] == "upload"


def test_a_long_filename_is_truncated_to_255_characters_keeping_the_extension(api, attach,
                                                                              mock_provider):
    chat = new_chat(attach)
    body = uploaded(api, chat, "a" * 300 + ".pdf", PDF, PDF_BYTES)
    assert len(body["filename"]) == 255 and body["filename"].endswith(".pdf")
    assert attach.row(body["id"])["filename"] == body["filename"]


# --------------------------------------------------------------------------- #
# multipart errors
# --------------------------------------------------------------------------- #

GOOD_PART = {"name": "file", "filename": "a.pdf", "content_type": PDF, "data": PDF_BYTES}


@pytest.mark.parametrize("content_type", [
    None, "multipart/form-data", "multipart/form-data; charset=utf-8"])
def test_a_request_without_a_multipart_boundary_is_400_boundary_required(api, attach, mock_provider,
                                                                         content_type):
    chat = new_chat(attach)
    resp = upload_raw(api, chat, body=multipart_body([GOOD_PART]), content_type=content_type)
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "BOUNDARY_REQUIRED")
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)


def test_an_unreadable_multipart_body_is_400_multipart_error(api, attach, mock_provider):
    chat = new_chat(attach)
    boundary = "----e2e-attachment-boundary-9bC4qZ"
    header = f"multipart/form-data; boundary={boundary}"
    for body in (f"--{boundary}\r\nContent-Disp".encode(),
                 (f"--{boundary}\r\nContent-Disposition form-data; name=\"file\"\r\n\r\ndata\r\n"
                  f"--{boundary}--\r\n").encode()):
        resp = upload_raw(api, chat, body=body, content_type=header)
        problem = assert_problem(resp, 400, "invalid_argument")
        assert_field_violation(problem, "multipart", "MULTIPART_ERROR")
        assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)
    assert_nothing_stored(attach, chat)


@pytest.mark.parametrize("parts", [
    [{"name": "note", "data": b"just text"}],
    [],
    [{"name": "upload", "filename": "a.pdf", "content_type": PDF, "data": PDF_BYTES}],
])
def test_a_body_without_a_file_field_is_400_missing_file(api, attach, mock_provider, parts):
    chat = new_chat(attach)
    resp = upload_raw(api, chat, body=multipart_body(parts),
                      content_type="multipart/form-data; boundary=----e2e-attachment-boundary-9bC4qZ")
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "file", "MISSING_FILE")
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)


def test_a_file_part_without_a_content_type_is_400_missing_content_type(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload(api, chat, "a.pdf", None, PDF_BYTES)
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "MISSING_CONTENT_TYPE")
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)


def test_other_form_fields_are_ignored(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload_raw(
        api, chat, body=multipart_body([{"name": "note", "data": b"ignored"}, GOOD_PART]),
        content_type="multipart/form-data; boundary=----e2e-attachment-boundary-9bC4qZ")
    assert resp.status_code == 201, resp.text


# --------------------------------------------------------------------------- #
# chat resolution and authentication
# --------------------------------------------------------------------------- #


def test_an_unknown_foreign_or_deleted_chat_is_404_before_the_body_is_read(api, attach,
                                                                           mock_provider):
    other_user = new_chat(attach, user=USER_A_REVIEWER_ID)
    other_tenant = new_chat(attach, tenant=TENANT_B_ID, user=USER_B_ID)
    deleted = attach.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=PREMIUM, deleted=True)
    for chat in (str(uuid.uuid4()), other_user, other_tenant, deleted):
        # not even multipart: the chat is resolved first
        resp = upload_raw(api, chat, body=b"not a multipart body", content_type=None)
        problem = assert_problem(resp, 404, "not_found")
        assert_resource_type(problem, "chat")
    assert_no_provider_traffic(mock_provider)


def test_a_non_uuid_chat_id_is_400_invalid_path_params(api, mock_provider):
    resp = upload(api, "not-a-uuid", "a.pdf", PDF, PDF_BYTES)
    problem = assert_problem(resp, 400, "invalid_argument")
    assert "invalid_path_params" in field_violation_reasons(problem)


def test_the_attachment_routes_need_a_token(api, attach, mock_provider):
    chat = new_chat(attach)
    row = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID)
    assert upload(api, chat, "a.pdf", PDF, PDF_BYTES, user=None).status_code == 401
    assert get_attachment(api, chat, row, user=None).status_code == 401
    assert delete_attachment(api, chat, row, user=None).status_code == 401
    assert_no_provider_traffic(mock_provider)
    assert attach.row(row)["deleted_at"] is None


def test_the_upload_is_scoped_to_the_chat_owner(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES, user="a_reviewer")
    assert_problem(resp, 404, "not_found")
    assert_problem(upload(api, chat, "a.pdf", PDF, PDF_BYTES, user="b"), 404, "not_found")
    assert_no_provider_traffic(mock_provider)


# --------------------------------------------------------------------------- #
# capabilities of the chat's model
# --------------------------------------------------------------------------- #


def test_an_image_for_a_chat_whose_model_has_no_vision_is_400_vision_not_supported(api, attach,
                                                                                   mock_provider):
    chat = new_chat(attach, STANDARD)
    resp = upload(api, chat, "photo.png", "image/png", png_bytes(8, 8))
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content_type", "VISION_NOT_SUPPORTED")
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)
    assert upload(api, chat, "a.pdf", PDF, PDF_BYTES).status_code == 201, "documents are fine"


def test_an_xlsx_for_a_chat_whose_model_has_no_code_interpreter_is_refused(api, attach,
                                                                           mock_provider):
    chat = new_chat(attach, STANDARD)
    resp = upload(api, chat, "sales.xlsx", XLSX, XLSX_BYTES)
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "file", "CODE_INTERPRETER_UNAVAILABLE")
    assert_resource_type(problem, "attachment")
    assert_no_provider_traffic(mock_provider)
    assert_nothing_stored(attach, chat)


def test_the_kill_switches_refuse_images_and_spreadsheets_but_not_documents(killed):
    api = MiniChatApi(killed.server.base_url)
    attach = killed.seeder()
    try:
        chat = new_chat(attach)
        image = upload(api, chat, "photo.png", "image/png", png_bytes(8, 8))
        problem = assert_problem(image, 400, "failed_precondition")
        assert_precondition(problem, "images", "FEATURE_DISABLED")
        sheet = upload(api, chat, "sales.xlsx", XLSX, XLSX_BYTES)
        problem = assert_problem(sheet, 400, "invalid_argument")
        assert_field_violation(problem, "file", "CODE_INTERPRETER_UNAVAILABLE")
        assert_resource_type(problem, "attachment")
        assert not killed.mock.file_uploads(), "the refused files never reach the provider"
        assert upload(api, chat, "a.pdf", PDF, PDF_BYTES).status_code == 201
        assert len(killed.mock.file_uploads()) == 1
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# size limits and per-chat limits (the small stack)
# --------------------------------------------------------------------------- #


def test_a_document_over_the_document_limit_is_400_file_too_large(sm):
    chat = new_chat(sm.attach)
    limit = SMALL_FILE_KB * 1024
    assert upload(sm.api, chat, "exact.pdf", PDF, filler(limit)).status_code == 201

    uploads_before = len(sm.mock.file_uploads())
    resp = upload(sm.api, chat, "over.pdf", PDF, filler(limit + 1))
    problem = assert_problem(resp, 400, "out_of_range")
    assert_field_violation(problem, "content_length", "FILE_TOO_LARGE")
    assert_resource_type(problem, "attachment")
    assert len(sm.mock.file_uploads()) == uploads_before, "nothing reaches the provider"
    assert len(sm.attach.live_rows_of(chat)) == 1


def test_an_image_over_the_image_limit_is_400_file_too_large(sm):
    chat = new_chat(sm.attach)
    limit = SMALL_IMAGE_KB * 1024
    assert upload(sm.api, chat, "exact.webp", "image/webp", filler(limit)).status_code == 201
    over = upload(sm.api, chat, "over.webp", "image/webp", filler(limit + 1))
    problem = assert_problem(over, 400, "out_of_range")
    assert_field_violation(problem, "content_length", "FILE_TOO_LARGE")
    # the document limit is its own: a 40 KiB document is fine
    assert upload(sm.api, chat, "doc.pdf", PDF, filler(40 * 1024)).status_code == 201


def test_the_document_count_limit_is_429_document_limit(sm):
    chat = new_chat(sm.attach)
    ids = [upload_document(sm.api, chat, f"doc{i}.pdf") for i in range(SMALL_MAX_DOCUMENTS)]

    uploads_before = len(sm.mock.file_uploads())
    resp = upload(sm.api, chat, "extra.pdf", PDF, PDF_BYTES)
    problem = assert_exhausted(resp, "document_limit")
    assert_resource_type(problem, "attachment")
    assert len(sm.mock.file_uploads()) == uploads_before
    assert len(sm.attach.live_rows_of(chat)) == SMALL_MAX_DOCUMENTS

    # images are not documents, the limit is per chat, a deleted document frees its slot
    assert upload(sm.api, chat, "pic.png", "image/png", png_bytes(8, 8)).status_code == 201
    assert upload(sm.api, new_chat(sm.attach), "o.pdf", PDF, PDF_BYTES).status_code == 201
    assert delete_attachment(sm.api, chat, ids[0]).status_code == 204
    assert upload(sm.api, chat, "again.pdf", PDF, PDF_BYTES).status_code == 201


def test_failed_documents_do_not_count_toward_the_document_limit(sm):
    chat = new_chat(sm.attach)
    for i in range(SMALL_MAX_DOCUMENTS):
        sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="failed",
                             error_code="upload_failed", filename=f"f{i}.pdf")
    assert upload(sm.api, chat, "fresh.pdf", PDF, PDF_BYTES).status_code == 201


def test_the_total_size_limit_is_429_storage_limit(sm):
    chat = new_chat(sm.attach)
    # 100 bytes below 1 MB (whether a MB counts 10^6 or 2^20 bytes): 10 more fit, 60 000 do not
    sm.attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image",
                         size=1_000_000 - 100)
    assert upload(sm.api, chat, "small.pdf", PDF, filler(10)).status_code == 201
    uploads_before = len(sm.mock.file_uploads())
    resp = upload(sm.api, chat, "bigger.pdf", PDF, filler(60_000))
    problem = assert_exhausted(resp, "storage_limit")
    assert_resource_type(problem, "attachment")
    assert len(sm.mock.file_uploads()) == uploads_before
    # failed and deleted attachments do not count
    other = new_chat(sm.attach)
    sm.attach.insert_row(tenant=TENANT_A_ID, chat=other, user=USER_A_ID, status="failed",
                         size=2_000_000)
    sm.attach.insert_row(tenant=TENANT_A_ID, chat=other, user=USER_A_ID, deleted=True,
                         size=2_000_000)
    assert upload(sm.api, other, "fits.pdf", PDF, filler(1000)).status_code == 201


def test_a_vector_store_of_another_provider_backend_is_409_provider_mismatch(api, attach,
                                                                             mock_provider):
    chat = new_chat(attach)
    attach.insert_vector_store(tenant=TENANT_A_ID, chat=chat, vector_store_id="vs_foreign_backend",
                               provider="azure-elsewhere")
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    problem = assert_problem(resp, 409, "already_exists")
    assert problem["context"]["resource_name"] == "provider_mismatch"
    assert not mock_provider.vector_store_adds(), "the foreign store is never written"
    assert "vs_foreign_backend" not in resp.text


def test_an_upload_beyond_the_concurrency_limit_is_503_with_retry_after_5(sm):
    chat = new_chat(sm.attach)
    # the provider holds every file upload back for a while: two uploads fill the two slots
    from .mock_openai_provider import ProviderResponse

    slow = [ProviderResponse(json_body={"id": f"file-slow-{n}", "object": "file"}, delay=6.0)
            for n in range(2)]
    sm.mock.script(*FILES, *slow)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        running = [pool.submit(upload, sm.api, chat, f"slow{n}.xlsx", XLSX, XLSX_BYTES)
                   for n in range(2)]
        sm.mock.wait_for_request(*FILES, count=2, timeout=10.0)
        third = upload(sm.api, chat, "third.xlsx", XLSX, XLSX_BYTES)
        results = [f.result(timeout=60) for f in running]
    assert_unavailable(third, 5)
    assert len(sm.mock.file_uploads()) == 2, "the rejected upload never reached the provider"
    assert [r.status_code for r in results] == [201, 201]
    # the slots are free again
    assert upload(sm.api, chat, "later.xlsx", XLSX, XLSX_BYTES).status_code == 201


# --------------------------------------------------------------------------- #
# provider failures
# --------------------------------------------------------------------------- #


def test_a_failed_provider_upload_is_503_with_retry_after_10_and_a_failed_row(api, attach,
                                                                              mock_provider):
    chat = new_chat(attach)
    mock_provider.fail(FILES, 500, PROVIDER_SECRET)
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)

    problem = assert_unavailable(resp, 10)
    assert problem["detail"] == "Service temporarily unavailable"
    assert PROVIDER_SECRET not in resp.text
    rows = attach.rows_of(chat)
    assert len(rows) == 1 and rows[0]["status"] == "failed"
    assert rows[0]["error_code"] == "upload_failed" and rows[0]["provider_file_id"] is None
    assert [r.route for r in mock_provider.storage_requests()] == ["POST /v1/files"]
    # the failed row stays visible
    got = get_attachment_body(api, chat, str(uuid_of(rows[0])))
    assert got["status"] == "failed" and got["error_code"] == "upload_failed"
    # a retry is a new upload with a new attachment
    assert upload(api, chat, "a.pdf", PDF, PDF_BYTES).status_code == 201
    assert len(attach.rows_of(chat)) == 2


def uuid_of(row):
    from .support import uuid_text

    return uuid_text(row["id"])


def get_attachment_body(api, chat, attachment):
    resp = get_attachment(api, chat, attachment)
    assert resp.status_code == 200, resp.text
    return resp.json()


def test_a_failed_vector_store_creation_fails_the_upload_with_503(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.fail(VECTOR_STORES, 500, PROVIDER_SECRET)
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    problem = assert_unavailable(resp, 10)
    assert problem["detail"] == "Service temporarily unavailable"
    assert PROVIDER_SECRET not in resp.text
    rows = attach.rows_of(chat)
    assert rows[0]["status"] == "failed" and rows[0]["error_code"]
    assert attach.stores_of(chat) == []
    assert not mock_provider.vector_store_adds()


def test_a_failed_add_to_the_vector_store_is_503_indexing_failed_and_deletes_the_file_once(
        api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.fail(VS_FILES, 500, PROVIDER_SECRET)
    mock_provider.fail(FILE, 500, PROVIDER_SECRET)  # the best effort delete fails too
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)

    problem = assert_unavailable(resp, 10)
    assert problem["detail"] == "Service temporarily unavailable"
    assert PROVIDER_SECRET not in resp.text and "indexing_failed" not in resp.text
    row = attach.rows_of(chat)[0]
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    deleted = wait_for(lambda: mock_provider.deleted_file_ids(), "the inline delete")
    assert deleted == [row["provider_file_id"]]
    time.sleep(1.5)
    assert mock_provider.deleted_file_ids() == deleted, "the inline delete is not retried"


# --------------------------------------------------------------------------- #
# the indexing wait of a document
# --------------------------------------------------------------------------- #


def test_an_in_progress_file_is_polled_until_it_is_completed(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.index_statuses("in_progress", "in_progress", "completed")
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    assert resp.status_code == 201, resp.text
    assert resp.json()["status"] == "ready"
    polls = mock_provider.vector_store_polls()
    assert len(polls) == 3, "polled until completed, then stopped"
    row = attach.row(resp.json()["id"])
    store = attach.vector_store_of(chat)
    assert all(p.path == f"/v1/vector_stores/{store}/files/{row['provider_file_id']}" for p in polls)


def test_the_default_provider_flow_polls_once_and_the_document_is_ready(api, attach, mock_provider):
    chat = new_chat(attach)
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)  # the mock: add -> in_progress, poll -> completed
    assert resp.status_code == 201 and resp.json()["status"] == "ready"
    assert len(mock_provider.vector_store_polls()) == 1


def test_a_poll_answer_without_status_counts_as_in_progress(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.set_default(*VS_FILES, vs_file_response("in_progress"))
    mock_provider.script(*VS_FILE, vs_file_without_status(), vs_file_without_status())
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    assert resp.status_code == 201 and resp.json()["status"] == "ready"
    assert len(mock_provider.vector_store_polls()) == 3


def test_transient_read_errors_keep_the_polling_going(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.set_default(*VS_FILES, vs_file_response("in_progress"))
    mock_provider.script(*VS_FILE, error_response(500), error_response(503))
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    assert resp.status_code == 201 and resp.json()["status"] == "ready"
    assert len(mock_provider.vector_store_polls()) == 3


@pytest.mark.parametrize("status", ["failed", "cancelled", "expired"])
def test_a_failed_cancelled_or_unknown_status_fails_the_upload_and_deletes_the_file(
        api, attach, mock_provider, status):
    chat = new_chat(attach)
    mock_provider.index_statuses("in_progress", status)
    sequence = attach.outbox_sequenced(ATTACHMENT_QUEUE)
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)

    problem = assert_unavailable(resp, 10)
    assert problem["detail"] == "Service temporarily unavailable"
    assert "indexing_failed" not in resp.text
    row = attach.rows_of(chat)[0]
    assert row["status"] == "failed" and row["error_code"] == "indexing_failed"
    assert wait_for(lambda: mock_provider.deleted_file_ids(), "the inline delete") == \
        [row["provider_file_id"]]
    assert len(mock_provider.vector_store_polls()) == 2, "polling stops at the failure"
    assert attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence, \
        "no outbox message: the inline delete is the whole cleanup"


def test_a_non_transient_read_error_fails_the_upload(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.set_default(*VS_FILES, vs_file_response("in_progress"))
    mock_provider.script(*VS_FILE, error_response(404, "No such file"))
    resp = upload(api, chat, "a.pdf", PDF, PDF_BYTES)
    assert_unavailable(resp, 10)
    row = attach.rows_of(chat)[0]
    assert (row["status"], row["error_code"]) == ("failed", "indexing_failed")


def test_images_and_spreadsheets_are_never_polled(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.index_forever()
    assert uploaded(api, chat, "p.png", "image/png", png_bytes(10, 10))["status"] == "ready"
    assert uploaded(api, chat, "s.xlsx", XLSX, XLSX_BYTES)["status"] == "ready"
    assert not mock_provider.vector_store_polls() and not mock_provider.vector_store_adds()


def test_a_failed_document_is_not_usable_and_can_be_deleted(api, attach, mock_provider):
    chat = new_chat(attach)
    mock_provider.index_statuses("failed")
    assert upload(api, chat, "a.pdf", PDF, PDF_BYTES).status_code == 503
    failed = uuid_of(attach.rows_of(chat)[0])

    from .support import reject_message

    resp = reject_message(api, chat, {"content": "q", "attachment_ids": [failed]})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "attachment", "invalid_attachment")
    assert delete_attachment(api, chat, failed).status_code == 204


# The 25 s request deadline is fixed (not configurable): these two tests really wait for it.


def test_indexing_still_running_at_the_deadline_returns_201_uploaded_then_ready(api, attach,
                                                                               mock_provider):
    chat = new_chat(attach)
    mock_provider.index_forever()
    started = time.monotonic()
    resp = upload(api, chat, "slow.pdf", PDF, PDF_BYTES, timeout=60.0)
    elapsed = time.monotonic() - started

    assert resp.status_code == 201, resp.text
    body = resp.json()
    assert body["status"] == "uploaded"
    assert 23.0 <= elapsed <= 29.5, f"the upload waits for the 25 s deadline: {elapsed:.1f}s"
    assert body.get("error_code") is None
    row = attach.row(body["id"])
    assert row["status"] == "uploaded" and row["provider_file_id"]
    assert len(attach.stores_of(chat)) == 1
    assert get_attachment_body(api, chat, body["id"])["status"] == "uploaded"

    # a message can not reference it yet
    from .support import reject_message

    rejected = reject_message(api, chat, {"content": "q", "attachment_ids": [body["id"]]})
    assert_field_violation(assert_problem(rejected, 400, "invalid_argument"),
                           "attachment", "invalid_attachment")

    # the background task makes it ready once the provider is done
    mock_provider.index_status_now("completed")
    ready = attach.wait_status(body["id"], "ready", timeout=40.0)
    assert ready["error_code"] is None and ready["cleanup_status"] is None
    assert get_attachment_body(api, chat, body["id"])["status"] == "ready"
    assert send_message(api, chat, "what does it say?", attachment_ids=[body["id"]]).terminal.event \
        == "done"


def test_a_background_indexing_failure_fails_the_row_and_the_outbox_deletes_the_file(api, attach,
                                                                                    mock_provider):
    chat = new_chat(attach)
    mock_provider.index_forever()
    resp = upload(api, chat, "slow.pdf", PDF, PDF_BYTES, timeout=60.0)
    assert resp.status_code == 201 and resp.json()["status"] == "uploaded"
    attachment = resp.json()["id"]
    provider_file = attach.provider_file_of(attachment)
    sequence = attach.outbox_sequenced(ATTACHMENT_QUEUE)

    mock_provider.index_status_now("failed")
    row = attach.wait_status(attachment, "failed", timeout=40.0)
    assert row["error_code"] == "indexing_failed"
    assert row["deleted_at"] is None, "failed, not soft-deleted"
    assert attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence + 1, \
        "one attachment_indexing_failed message in the same transaction"
    done = attach.wait_cleanup(attachment, "done")
    assert done["status"] == "failed" and done["error_code"] == "indexing_failed"
    assert mock_provider.deleted_file_ids() == [provider_file], "deleted once, by the outbox"
    body = get_attachment_body(api, chat, attachment)
    assert body["status"] == "failed" and body["error_code"] == "indexing_failed"
    assert provider_file not in str(body)


# --------------------------------------------------------------------------- #
# GET
# --------------------------------------------------------------------------- #


def test_get_returns_the_attachment_detail(api, attach, mock_provider):
    chat = new_chat(attach)
    created = uploaded(api, chat, "Quarterly Report.pdf", PDF, PDF_BYTES)
    resp = get_attachment(api, chat, created["id"])
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["id"] == created["id"] and body["filename"] == "Quarterly Report.pdf"
    assert body["content_type"] == PDF and body["size_bytes"] == len(PDF_BYTES)
    assert body["status"] == "ready" and body["kind"] == "document"
    assert parse_ts(body["created_at"]) == parse_ts(created["created_at"])
    allowed = {"id", "filename", "content_type", "size_bytes", "status", "kind", "created_at",
               "error_code", "doc_summary", "img_thumbnail", "summary_updated_at"}
    assert set(body) <= allowed, set(body) - allowed
    for key in ("error_code", "doc_summary", "img_thumbnail", "summary_updated_at"):
        assert body.get(key) is None


def test_get_of_a_ready_image_carries_the_thumbnail(api, attach, mock_provider):
    chat = new_chat(attach)
    created = uploaded(api, chat, "photo.png", "image/png", png_bytes(400, 200))
    body = get_attachment_body(api, chat, created["id"])
    assert body["kind"] == "image"
    assert body["img_thumbnail"] == created["img_thumbnail"]
    assert (body["img_thumbnail"]["width"], body["img_thumbnail"]["height"]) == (128, 64)


def test_get_reports_every_status_and_the_error_code_only_of_a_failed_row(api, attach):
    chat = new_chat(attach)
    for status in ("pending", "uploaded", "ready", "failed"):
        row = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status=status,
                                error_code="indexing_failed" if status == "failed" else None)
        body = get_attachment_body(api, chat, row)
        assert body["status"] == status
        assert body.get("error_code") == ("indexing_failed" if status == "failed" else None)


def test_the_thumbnail_is_only_shown_for_a_ready_image(api, attach):
    from .support import THUMBNAIL_BYTES

    chat = new_chat(attach)
    ready = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image",
                              thumbnail=THUMBNAIL_BYTES)
    assert get_attachment_body(api, chat, ready)["img_thumbnail"]["content_type"] == "image/webp"
    for status in ("pending", "uploaded", "failed"):
        row = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, kind="image",
                                status=status, thumbnail=THUMBNAIL_BYTES)
        assert get_attachment_body(api, chat, row).get("img_thumbnail") is None, status


def test_get_is_404_for_unknown_foreign_and_deleted_attachments(api, attach):
    chat, other_chat = new_chat(attach), new_chat(attach)
    in_other_chat = attach.insert_row(tenant=TENANT_A_ID, chat=other_chat, user=USER_A_ID)
    by_other_user = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_REVIEWER_ID)
    deleted = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, deleted=True)
    for attachment in (str(uuid.uuid4()), in_other_chat, by_other_user, deleted):
        problem = assert_problem(get_attachment(api, chat, attachment), 404, "not_found")
        assert_resource_type(problem, "attachment")


def test_get_in_a_chat_the_caller_cannot_see_is_404_for_the_chat(api, attach):
    other_user = new_chat(attach, user=USER_A_REVIEWER_ID)
    other_tenant = new_chat(attach, tenant=TENANT_B_ID, user=USER_B_ID)
    deleted = attach.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=PREMIUM, deleted=True)
    for chat, owner, tenant in ((other_user, USER_A_REVIEWER_ID, TENANT_A_ID),
                                (other_tenant, USER_B_ID, TENANT_B_ID),
                                (deleted, USER_A_ID, TENANT_A_ID)):
        row = attach.insert_row(tenant=tenant, chat=chat, user=owner)
        for caller in (get_attachment(api, chat, row), delete_attachment(api, chat, row)):
            assert_resource_type(assert_problem(caller, 404, "not_found"), "chat")
        assert attach.row(row)["deleted_at"] is None


def test_a_non_uuid_attachment_id_is_400_invalid_path_params(api, attach):
    chat = new_chat(attach)
    for call in (get_attachment, delete_attachment):
        problem = assert_problem(call(api, chat, "not-a-uuid"), 400, "invalid_argument")
        assert "invalid_path_params" in field_violation_reasons(problem)


# --------------------------------------------------------------------------- #
# DELETE
# --------------------------------------------------------------------------- #


def test_delete_soft_deletes_and_returns_204_without_a_body(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    sequence = attach.outbox_sequenced(ATTACHMENT_QUEUE)

    resp = delete_attachment(api, chat, attachment)
    assert resp.status_code == 204 and resp.content == b""
    row = attach.row(attachment)
    assert row["deleted_at"] is not None and row["status"] == "ready"
    assert row["cleanup_status"] in ("pending", "done"), "provider cleanup is outstanding or done"
    assert attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence + 1
    assert_resource_type(assert_problem(get_attachment(api, chat, attachment), 404, "not_found"),
                         "attachment")


def test_deleting_twice_is_idempotent_and_enqueues_once(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    sequence = attach.outbox_sequenced(ATTACHMENT_QUEUE)
    assert delete_attachment(api, chat, attachment).status_code == 204
    first = attach.row(attachment)["deleted_at"]
    assert delete_attachment(api, chat, attachment).status_code == 204
    assert delete_attachment(api, chat, attachment).status_code == 204
    assert attach.row(attachment)["deleted_at"] == first
    assert attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence + 1


def test_a_repeated_delete_by_another_user_is_404_not_204(api, attach):
    chat = new_chat(attach)
    deleted = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_REVIEWER_ID, deleted=True)
    live = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_REVIEWER_ID)
    for attachment in (deleted, live):
        assert_resource_type(assert_problem(delete_attachment(api, chat, attachment), 404,
                                            "not_found"), "attachment")
    assert attach.row(live)["deleted_at"] is None


def test_delete_is_404_for_unknown_and_other_chat_attachments(api, attach):
    chat, other = new_chat(attach), new_chat(attach)
    foreign = attach.insert_row(tenant=TENANT_A_ID, chat=other, user=USER_A_ID)
    for attachment in (str(uuid.uuid4()), foreign):
        assert_resource_type(assert_problem(delete_attachment(api, chat, attachment), 404,
                                            "not_found"), "attachment")
    assert attach.row(foreign)["deleted_at"] is None


def test_an_attachment_referenced_by_a_submitted_message_is_409_attachment_locked(api, attach,
                                                                                  mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    sent = send_message(api, chat, "see the file", attachment_ids=[attachment])
    assert sent.terminal.event == "done"

    sequence = attach.outbox_sequenced(ATTACHMENT_QUEUE)
    resp = delete_attachment(api, chat, attachment)
    problem = assert_problem(resp, 409, "already_exists")
    assert problem["context"]["resource_name"] == "attachment_locked"
    assert_resource_type(problem, "attachment")
    row = attach.row(attachment)
    assert row["deleted_at"] is None and row["cleanup_status"] is None
    assert attach.outbox_sequenced(ATTACHMENT_QUEUE) == sequence
    assert get_attachment(api, chat, attachment).status_code == 200


def test_an_unsent_attachment_can_be_removed_while_a_sent_one_is_locked(api, attach, mock_provider):
    chat = new_chat(attach)
    locked, free = upload_document(api, chat, "sent.pdf"), upload_document(api, chat, "free.pdf")
    send_message(api, chat, "q", attachment_ids=[locked])
    assert delete_attachment(api, chat, free).status_code == 204
    assert delete_attachment(api, chat, locked).status_code == 409
    from .support import reject_message

    resp = reject_message(api, chat, {"content": "again", "attachment_ids": [free]})
    assert_field_violation(assert_problem(resp, 400, "invalid_argument"), "attachment",
                           "invalid_attachment")


def test_delete_returns_without_waiting_for_the_provider(api, attach, mock_provider):
    from .mock_openai_provider import ProviderResponse

    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    mock_provider.script(*FILE, ProviderResponse(json_body={"deleted": True}, delay=4.0))
    started = time.monotonic()
    assert delete_attachment(api, chat, attachment).status_code == 204
    assert time.monotonic() - started < 3.0, "the response does not wait for the provider cleanup"
    attach.wait_cleanup(attachment, "done", timeout=40.0)


def test_deleting_an_attachment_never_touches_the_vector_store(api, attach, mock_provider):
    chat = new_chat(attach)
    attachment = upload_document(api, chat, "a.pdf")
    assert delete_attachment(api, chat, attachment).status_code == 204
    attach.wait_cleanup(attachment, "done")
    assert len(mock_provider.file_deletes()) == 1
    assert not mock_provider.vector_store_deletes(), "the store lives until the chat goes"
    assert len(attach.stores_of(chat)) == 1


# --------------------------------------------------------------------------- #
# the message list and the message links
# --------------------------------------------------------------------------- #


def test_the_message_list_summarises_the_attachments_of_a_submitted_message(api, attach,
                                                                            mock_provider):
    chat = new_chat(attach)
    doc, image = upload_document(api, chat, "notes.pdf"), upload_image(api, chat, "photo.png")
    sent = send_message(api, chat, "look at these", attachment_ids=[doc, image])
    assert sent.terminal.event == "done"

    items = list_messages(api, chat)
    user = next(m for m in items if m["role"] == "user")
    assistant = next(m for m in items if m["role"] == "assistant")
    summaries = {s["attachment_id"]: s for s in user["attachments"]}
    assert set(summaries) == {doc, image}
    assert summaries[doc]["kind"] == "document" and summaries[doc]["filename"] == "notes.pdf"
    assert summaries[doc]["status"] == "ready" and summaries[doc].get("img_thumbnail") is None
    assert summaries[image]["kind"] == "image"
    assert summaries[image]["img_thumbnail"]["content_type"] == "image/webp"
    assert assistant["attachments"] == []
    assert not re.search(r"file[-_][A-Za-z0-9]{6,}", str(user)), user


def test_the_attachments_are_linked_once_to_the_user_message(api, attach, mock_provider):
    chat = new_chat(attach)
    first, second = upload_document(api, chat, "a.pdf"), upload_document(api, chat, "b.pdf")
    sent = send_message(api, chat, "q", attachment_ids=[first, second])
    user_message = attach.user_message_of(chat, sent.request_id)["id"]
    from .support import uuid_text

    links = attach.all_links(chat)
    assert links == sorted((uuid_text(user_message), a) for a in (first, second))


def test_a_message_can_not_reference_foreign_pending_duplicate_or_unknown_attachments(
        api, attach, mock_provider):
    from .support import reject_message

    chat, other = new_chat(attach), new_chat(attach)
    ready = upload_document(api, chat, "ok.pdf")
    in_other_chat = upload_document(api, other, "o.pdf")
    by_other_user = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_REVIEWER_ID)
    pending = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, status="pending")
    uploaded_row = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                     status="uploaded")
    deleted = attach.insert_row(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, deleted=True)
    before = attach.counts(chat)
    for ids in ([in_other_chat], [by_other_user], [pending], [uploaded_row], [deleted],
                [str(uuid.uuid4())], [ready, ready], [ready, pending]):
        resp = reject_message(api, chat, {"content": "q", "attachment_ids": ids})
        problem = assert_problem(resp, 400, "invalid_argument")
        assert_field_violation(problem, "attachment", "invalid_attachment")
    assert attach.counts(chat) == before, "a rejection leaves nothing behind"
    assert not mock_provider.requests_to("POST", "/v1/responses")
    assert send_message(api, chat, "q", attachment_ids=[ready]).terminal.event == "done"


# --------------------------------------------------------------------------- #
# the outbox queues
# --------------------------------------------------------------------------- #


def test_the_cleanup_queues_are_registered_with_the_default_partition_count(attach):
    queues = attach.outbox_queues()
    assert queues.get(ATTACHMENT_QUEUE) == 4, queues
    assert queues.get(CHAT_CLEANUP_QUEUE) == 4, queues
