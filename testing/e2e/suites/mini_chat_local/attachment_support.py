"""Helpers of the S6 black-box tests (attachments).

Kept apart from ``support.py`` (S1 - S4) and ``mutation_support.py`` (S5) so the earlier suites stay
untouched:

* ``upload`` / ``upload_raw`` / ``multipart_body`` call ``POST /v1/chats/{id}/attachments``;
* ``png_bytes`` / ``webp_size`` build and inspect images with the standard library only;
* ``AttachmentSeeder`` extends ``MutationSeeder`` with full ``attachments`` rows (every column the
  lifecycle, the reaper and the cleanup handlers read) and the S6 readers;
* ``provider_request_tools`` & co. read the Responses request the mock provider recorded.

The mock provider side (``MockOpenAIProvider.file_uploads()``, ``index_statuses()``, ``fail()`` ...)
lives in ``mock_openai_provider.py``.
"""

from __future__ import annotations

import base64
import struct
import time
import uuid
import zlib
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Sequence, Tuple

import httpx

from .mutation_support import MutationSeeder
from .support import (
    MiniChatApi,
    bearer,
    stored_ts,
    uuid_blob,
    uuid_text,
    wait_for,
)

ATTACHMENT_QUEUE = "mini-chat.attachment_cleanup"
CHAT_CLEANUP_QUEUE = "mini-chat.chat_cleanup"

PDF = "application/pdf"
DOCX = "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
PPTX = "application/vnd.openxmlformats-officedocument.presentationml.presentation"
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
PDF_BYTES = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n"
XLSX_BYTES = b"PK\x03\x04 pretend spreadsheet"

BOUNDARY = "----e2e-attachment-boundary-9bC4qZ"

# the provider's fake file ids, e.g. ``file-mock-3`` / ``vs_mock_1``
PROVIDER_FILE_PREFIX = "file-"


# --------------------------------------------------------------------------- #
# file contents
# --------------------------------------------------------------------------- #


def png_bytes(width: int = 40, height: int = 20, rgb: Tuple[int, int, int] = (200, 30, 30)) -> bytes:
    """A valid truecolor PNG of ``width`` x ``height`` pixels (standard library only)."""
    row = b"\x00" + bytes(rgb) * width
    raw = row * height

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def webp_size(data: bytes) -> Tuple[int, int]:
    """``(width, height)`` of a WebP file (``VP8 `` lossy, ``VP8L`` lossless or ``VP8X``)."""
    assert data[:4] == b"RIFF" and data[8:12] == b"WEBP", f"not a WebP file: {data[:16]!r}"
    kind = data[12:16]
    if kind == b"VP8X":
        width = 1 + int.from_bytes(data[24:27], "little")
        height = 1 + int.from_bytes(data[27:30], "little")
        return width, height
    if kind == b"VP8L":
        bits = int.from_bytes(data[21:25], "little")
        return (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
    if kind == b"VP8 ":
        return (int.from_bytes(data[26:28], "little") & 0x3FFF,
                int.from_bytes(data[28:30], "little") & 0x3FFF)
    raise AssertionError(f"unknown WebP chunk {kind!r}")


def filler(size: int) -> bytes:
    return (b"abcdefghijklmnopqrstuvwxyz" * (size // 26 + 1))[:size]


def decode_thumbnail(thumbnail: Dict[str, Any]) -> bytes:
    return base64.b64decode(thumbnail["data_base64"])


# --------------------------------------------------------------------------- #
# the three endpoints
# --------------------------------------------------------------------------- #


def attachments_path(chat_id: str, attachment_id: Optional[str] = None) -> str:
    base = f"/v1/chats/{chat_id}/attachments"
    return base if attachment_id is None else f"{base}/{attachment_id}"


def multipart_body(parts: Sequence[Dict[str, Any]], boundary: str = BOUNDARY) -> bytes:
    """A multipart body: ``parts`` are ``{name, filename?, content_type?, data}`` dicts."""
    out = b""
    for part in parts:
        out += f"--{boundary}\r\n".encode()
        disposition = f'Content-Disposition: form-data; name="{part["name"]}"'
        if part.get("filename") is not None:
            disposition += f'; filename="{part["filename"]}"'
        out += disposition.encode() + b"\r\n"
        if part.get("content_type") is not None:
            out += f'Content-Type: {part["content_type"]}\r\n'.encode()
        out += b"\r\n" + part["data"] + b"\r\n"
    return out + f"--{boundary}--\r\n".encode()


def create_chat(api: MiniChatApi, model: str = "mock-premium", *, user: str = "a") -> str:
    resp = api.post("/v1/chats", json={"model": model}, user=user)
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def upload(api: MiniChatApi, chat_id: str, filename: Optional[str], content_type: Optional[str],
           data: bytes, *, user: Optional[str] = "a", timeout: float = 60.0) -> httpx.Response:
    """Upload ``data`` as the ``file`` part of a multipart request. The upload may wait for the
    provider's indexing for up to 25 s, hence the generous default timeout."""
    part: Dict[str, Any] = {"name": "file", "filename": filename, "content_type": content_type,
                            "data": data}
    return upload_raw(api, chat_id, body=multipart_body([part]),
                      content_type=f"multipart/form-data; boundary={BOUNDARY}", user=user,
                      timeout=timeout)


def upload_raw(api: MiniChatApi, chat_id: str, *, body: bytes, content_type: Optional[str],
               user: Optional[str] = "a", timeout: float = 60.0,
               path: Optional[str] = None) -> httpx.Response:
    """``POST .../attachments`` with an explicit body and ``Content-Type`` (``None``: no header)."""
    headers: Dict[str, str] = {}
    if user is not None:
        headers.update(bearer(user))
    if content_type is not None:
        headers["Content-Type"] = content_type
    return api._client.request(  # noqa: SLF001 - the suite's own client
        "POST", path or api.url(attachments_path(chat_id)), headers=headers, content=body,
        timeout=httpx.Timeout(timeout, connect=5.0))


def uploaded(api: MiniChatApi, chat_id: str, filename: str, content_type: str, data: bytes,
             **kwargs: Any) -> Dict[str, Any]:
    """Upload and assert ``201``; returns the body."""
    resp = upload(api, chat_id, filename, content_type, data, **kwargs)
    assert resp.status_code == 201, f"upload failed: {resp.status_code} {resp.text}"
    return resp.json()


def upload_document(api: MiniChatApi, chat_id: str, filename: str = "report.pdf",
                    **kwargs: Any) -> str:
    """A ready PDF document; returns its id."""
    body = uploaded(api, chat_id, filename, PDF, PDF_BYTES, **kwargs)
    assert body["status"] == "ready", body
    return body["id"]


def upload_spreadsheet(api: MiniChatApi, chat_id: str, filename: str = "sales.xlsx",
                       **kwargs: Any) -> str:
    body = uploaded(api, chat_id, filename, XLSX, XLSX_BYTES, **kwargs)
    assert body["status"] == "ready", body
    return body["id"]


def upload_image(api: MiniChatApi, chat_id: str, filename: str = "photo.png",
                 width: int = 40, height: int = 20, **kwargs: Any) -> str:
    body = uploaded(api, chat_id, filename, "image/png", png_bytes(width, height), **kwargs)
    assert body["status"] == "ready", body
    return body["id"]


def get_attachment(api: MiniChatApi, chat_id: str, attachment_id: str, *,
                   user: Optional[str] = "a") -> httpx.Response:
    return api.get(attachments_path(chat_id, attachment_id), user=user)


def delete_attachment(api: MiniChatApi, chat_id: str, attachment_id: str, *,
                      user: Optional[str] = "a") -> httpx.Response:
    return api.delete(attachments_path(chat_id, attachment_id), user=user)


def list_messages(api: MiniChatApi, chat_id: str, *, user: str = "a") -> List[Dict[str, Any]]:
    resp = api.get(f"/v1/chats/{chat_id}/messages", user=user)
    assert resp.status_code == 200, resp.text
    return resp.json()["items"]


# --------------------------------------------------------------------------- #
# assertions
# --------------------------------------------------------------------------- #


def assert_unavailable(resp: httpx.Response, retry_after: int) -> Dict[str, Any]:
    """503 ``service_unavailable`` with ``Retry-After`` and the generic detail."""
    from .support import assert_problem

    problem = assert_problem(resp, 503, "service_unavailable")
    assert resp.headers.get("retry-after") == str(retry_after), dict(resp.headers)
    assert problem["context"].get("retry_after_seconds") == retry_after, problem
    return problem


def assert_exhausted(resp: httpx.Response, subject: str) -> Dict[str, Any]:
    """429 ``resource_exhausted`` whose first violation names ``subject``."""
    from .support import assert_problem

    problem = assert_problem(resp, 429, "resource_exhausted")
    violations = problem["context"].get("violations", [])
    assert violations and violations[0]["subject"] == subject, problem
    return problem


def assert_resource_type(problem: Dict[str, Any], kind: str) -> None:
    assert problem["context"].get("resource_type") == f"gts.cf.core.mini_chat.{kind}.v1~", problem


def now_stored(delta_secs: float = 0.0) -> str:
    """Canonical stored text (see ``support.stored_ts``) of the wall clock plus ``delta_secs``."""
    moment = datetime.fromtimestamp(time.time() + delta_secs, tz=timezone.utc)
    micros = moment.microsecond
    if micros == 0:
        fraction = ""
    elif micros % 1000 == 0:
        fraction = f".{micros // 1000:03d}"
    else:
        fraction = f".{micros:06d}"
    return f"{moment.strftime('%Y-%m-%dT%H:%M:%S')}{fraction}+00:00"


# --------------------------------------------------------------------------- #
# the Responses request the provider recorded
# --------------------------------------------------------------------------- #


def responses_requests(mock: Any) -> List[Any]:
    return mock.requests_to("POST", "/v1/responses")


def last_responses_request(mock: Any) -> Any:
    requests = responses_requests(mock)
    assert requests, "the provider was never called for a chat turn"
    return requests[-1]


def user_content(request: Any) -> List[Dict[str, Any]]:
    """The content parts of the last ``user`` item of ``input``."""
    items = request.json.get("input") if isinstance(request.json, dict) else None
    users = [i for i in (items or []) if isinstance(i, dict) and i.get("role") == "user"]
    assert users, f"no user item in {request.json}"
    content = users[-1]["content"]
    return content if isinstance(content, list) else [{"type": "input_text", "text": content}]


def input_image_ids(request: Any) -> List[str]:
    """``file_id`` of every ``input_image`` part of the last user item, in order."""
    return [p["file_id"] for p in user_content(request) if p.get("type") == "input_image"]


def all_input_images(request: Any) -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []
    for item in (request.json.get("input") or []):
        content = item.get("content") if isinstance(item, dict) else None
        if isinstance(content, list):
            out += [p for p in content if isinstance(p, dict) and p.get("type") == "input_image"]
    return out


def tool_set(request: Any) -> "set[str]":
    return set(request.tool_types())


# --------------------------------------------------------------------------- #
# seeding and reading the attachments tables
# --------------------------------------------------------------------------- #


class AttachmentSeeder(MutationSeeder):
    """``MutationSeeder`` plus whole ``attachments`` rows and the S6 readers."""

    def insert_row(self, *, tenant: str, chat: str, user: str, filename: str = "a.pdf",
                   kind: str = "document", status: str = "ready",
                   content_type: Optional[str] = None, provider_file_id: Optional[str] = "auto",
                   size: int = 1234, for_file_search: Optional[bool] = None,
                   for_code_interpreter: bool = False, cleanup_status: Optional[str] = None,
                   cleanup_attempts: int = 0, error_code: Optional[str] = None,
                   created: Optional[str] = None, updated: Optional[str] = None,
                   deleted: bool = False, secondary_file_id: Optional[str] = None,
                   storage_backend: str = "openai", thumbnail: Optional[bytes] = None,
                   attachment_id: Optional[str] = None) -> str:
        """One ``attachments`` row; ``created`` / ``updated`` are stored timestamp texts (default
        2025-01-01, i.e. long stale for the upload reaper)."""
        attachment_id = attachment_id or str(uuid.uuid4())
        if provider_file_id == "auto":
            provider_file_id = f"file-seed-{attachment_id}"
        if for_file_search is None:
            for_file_search = kind == "document" and not for_code_interpreter
        if content_type is None:
            content_type = ("image/png" if kind == "image"
                            else XLSX if for_code_interpreter else PDF)
        self._write(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, "
            "content_type, size_bytes, storage_backend, provider_file_id, status, error_code, "
            "attachment_kind, for_file_search, for_code_interpreter, img_thumbnail, "
            "img_thumbnail_width, img_thumbnail_height, cleanup_status, cleanup_attempts, "
            "created_at, updated_at, deleted_at, secondary_file_id, secondary_status, "
            "secondary_provider_kind) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (uuid_blob(attachment_id), uuid_blob(tenant), uuid_blob(chat), uuid_blob(user),
             filename, content_type, size, storage_backend, provider_file_id, status, error_code,
             kind, 1 if for_file_search else 0, 1 if for_code_interpreter else 0, thumbnail,
             64 if thumbnail else None, 48 if thumbnail else None, cleanup_status,
             cleanup_attempts, created or stored_ts(0), updated or created or stored_ts(0),
             stored_ts(1) if deleted else None, secondary_file_id,
             "uploaded" if secondary_file_id else "not_attempted",
             "anthropic" if secondary_file_id else None))
        return attachment_id

    def update_row(self, attachment_id: str, **columns: Any) -> None:
        """``UPDATE attachments SET <column> = <value> ... WHERE id = ?`` (values are bound)."""
        assignments = ", ".join(f"{name} = ?" for name in columns)
        self._write(f"UPDATE attachments SET {assignments} WHERE id = ?",
                    (*columns.values(), uuid_blob(attachment_id)))

    # ---- readers -------------------------------------------------------------- #

    def row(self, attachment_id: str) -> Any:
        found = self.attachment(attachment_id)
        assert found is not None, f"no attachment {attachment_id}"
        return found

    def rows_of(self, chat: str) -> List[Any]:
        return self.query("SELECT * FROM attachments WHERE chat_id = ? ORDER BY created_at, id",
                          (uuid_blob(chat),))

    def live_rows_of(self, chat: str) -> List[Any]:
        """Rows that are neither failed nor soft-deleted."""
        return [r for r in self.rows_of(chat) if r["status"] != "failed" and r["deleted_at"] is None]

    def provider_file_of(self, attachment_id: str) -> str:
        file_id = self.row(attachment_id)["provider_file_id"]
        assert file_id, f"attachment {attachment_id} has no provider file"
        return file_id

    def stores_of(self, chat: str) -> List[Any]:
        return self.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (uuid_blob(chat),))

    def vector_store_of(self, chat: str) -> str:
        stores = self.stores_of(chat)
        assert len(stores) == 1, f"one vector store expected, got {len(stores)}"
        return stores[0]["vector_store_id"]

    def all_links(self, chat: str) -> List[Tuple[str, str]]:
        rows = self.query("SELECT message_id, attachment_id FROM message_attachments "
                          "WHERE chat_id = ?", (uuid_blob(chat),))
        return sorted((uuid_text(r["message_id"]), uuid_text(r["attachment_id"])) for r in rows)

    def message_ids(self, chat: str, role: str) -> List[str]:
        return [uuid_text(r["id"]) for r in self.ordered_messages(chat) if r["role"] == role]

    def chat_deleted(self, chat: str) -> bool:
        found = self.chat(chat)
        return found is not None and found["deleted_at"] is not None

    # ---- waiting -------------------------------------------------------------- #

    def wait_row(self, attachment_id: str, what: str, predicate: Any, timeout: float = 30.0) -> Any:
        return wait_for(lambda: (lambda r: r if predicate(r) else None)(self.row(attachment_id)),
                        f"{what} of {attachment_id}", timeout=timeout)

    def wait_status(self, attachment_id: str, status: str, timeout: float = 30.0) -> Any:
        return self.wait_row(attachment_id, f"status {status}", lambda r: r["status"] == status,
                             timeout)

    def wait_cleanup(self, attachment_id: str, status: str, timeout: float = 40.0) -> Any:
        return self.wait_row(attachment_id, f"cleanup_status {status}",
                             lambda r: r["cleanup_status"] == status, timeout)


# --------------------------------------------------------------------------- #
# server configurations of the S6 stacks
# --------------------------------------------------------------------------- #

SMALL_FILE_KB = 64
SMALL_IMAGE_KB = 32
SMALL_MAX_DOCUMENTS = 3
SMALL_MAX_IMAGES = 2
SMALL_MAX_CONCURRENT_UPLOADS = 2
SMALL_CLEANUP_ATTEMPTS = 2
CODE_INTERPRETER_DAILY_QUOTA = 3
REAPER_STALE_AFTER_SECS = 60


def small_overrides() -> Dict[str, Any]:
    """The ``attachments_small`` stack: the quota stack's limits plus tiny attachment limits, the
    upload reaper on (scan every second, stale after the minimum 60 s), two cleanup attempts and
    a daily code interpreter quota of three calls."""
    import copy

    from . import catalog

    overrides = copy.deepcopy(catalog.quota_overrides())
    config = overrides["gears"]["mini-chat"]["config"]
    config["quota"]["code_interpreter_daily_quota"] = CODE_INTERPRETER_DAILY_QUOTA
    config["rag"] = {
        "max_documents_per_chat": SMALL_MAX_DOCUMENTS,
        "max_total_upload_mb_per_chat": 1,
        "uploaded_file_max_size_kb": SMALL_FILE_KB,
        "uploaded_image_max_size_kb": SMALL_IMAGE_KB,
        "max_images_per_message": SMALL_MAX_IMAGES,
        "max_concurrent_uploads": SMALL_MAX_CONCURRENT_UPLOADS,
    }
    config["thumbnail"] = {"width": 32, "height": 32}
    config["upload_reaper"] = {"enabled": True, "scan_interval_secs": 1,
                               "stale_after_secs": REAPER_STALE_AFTER_SECS}
    config["cleanup_worker"] = {"max_attempts": SMALL_CLEANUP_ATTEMPTS}
    return overrides


def killed_overrides() -> Dict[str, Any]:
    """The ``attachments_killed`` stack: ``disable_images``, ``disable_code_interpreter`` and
    ``disable_file_search`` on, a one-call daily code interpreter quota."""
    import copy

    from . import catalog

    overrides = copy.deepcopy(catalog.quota_overrides(kill_switches={
        "disable_images": True, "disable_code_interpreter": True, "disable_file_search": True}))
    overrides["gears"]["mini-chat"]["config"]["quota"]["code_interpreter_daily_quota"] = 1
    return overrides
