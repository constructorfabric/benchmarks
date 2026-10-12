#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Helpers of the slice S5 attachment tests (upload, get, delete, indexing, tools, cleanup,
//! upload reaper). They use only the harness API of S1-S4 (`request`, `request_raw`, `provider`,
//! `db`, `outbox_payloads`, `outbox_outcomes`, `enqueue_raw_outbox`); the S5-only API
//! (`TestEnvBuilder::indexing_timings`, `TestEnv::run_upload_reaper_once`, HARNESS.md "S5
//! additions") is used directly by `attachments_indexing.rs` and `upload_reaper.rs` only, so the
//! other binaries compile before the author stage adds it.
//!
//! A test file that needs them declares `mod common; mod attachments;` (plus `mod billing;` for the
//! dead-letter reader).
//!
//! # Scripting the fake provider
//!
//! `FakeProvider` routes by `path.contains(pattern)`; the first pattern (in insertion order) whose
//! queue is not empty answers, an unscripted non-chat request gets `200 {}`. The upload helpers
//! therefore enqueue patterns that are specific to one provider call:
//!
//! | call | path | pattern |
//! |---|---|---|
//! | `POST` file upload | `/llm.test/v1/files` | `/v1/files` |
//! | `POST` vector store create | `/llm.test/v1/vector_stores` | `/v1/vector_stores` |
//! | `POST` vector store add file | `/llm.test/v1/vector_stores/{vs}/files` | `/vector_stores/{vs}/files` |
//! | `GET` vector store file status | `/llm.test/v1/vector_stores/{vs}/files/{file}` | `/vector_stores/{vs}/files/{file}` |
//! | `DELETE` file | `/llm.test/v1/files/{file}` | `/v1/files/{file}` |
//! | `DELETE` vector store | `/llm.test/v1/vector_stores/{vs}` | `/v1/vector_stores/{vs}` |
//!
//! Scripts are queued just before the call that consumes them (one upload at a time), so the
//! first-match rule never lets a later call steal an earlier script.

use std::fmt::Write as _;
use std::io::Cursor;

use axum::body::Body;
use base64::Engine as _;
use http::{Method, Request, StatusCode};
use mini_chat::test_support::{RecordedRequest, ScriptedResponse, TestEnv, TestResponse, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::common::{assert_not_found, assert_problem, exec_sql, expect_status, get, uuid_hex};

pub const ATTACHMENT_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";

pub const ATTACHMENT_CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";
pub const CHAT_CLEANUP_QUEUE: &str = "mini-chat.chat_cleanup";

/// The alias of the harness provider (its host); every proxied path starts with it.
pub const ALIAS: &str = "llm.test";

pub const PDF: &str = "application/pdf";
pub const TEXT: &str = "text/plain";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
pub const PNG: &str = "image/png";
pub const JPEG: &str = "image/jpeg";

/// An old instant for seeded rows (SQL literal text without quotes).
pub const OLD: &str = "2026-01-01T00:00:00Z";

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[must_use]
pub fn attachments_path(chat: &str) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments")
}

#[must_use]
pub fn attachment_path(chat: &str, attachment: &str) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{attachment}")
}

/// `POST` of a file upload.
#[must_use]
pub fn files_path() -> String {
    format!("/{ALIAS}/v1/files")
}

#[must_use]
pub fn file_path(file: &str) -> String {
    format!("/{ALIAS}/v1/files/{file}")
}

#[must_use]
pub fn vector_stores_path() -> String {
    format!("/{ALIAS}/v1/vector_stores")
}

#[must_use]
pub fn vector_store_path(vs: &str) -> String {
    format!("/{ALIAS}/v1/vector_stores/{vs}")
}

#[must_use]
pub fn vector_store_files_path(vs: &str) -> String {
    format!("/{ALIAS}/v1/vector_stores/{vs}/files")
}

#[must_use]
pub fn vector_store_file_path(vs: &str, file: &str) -> String {
    format!("/{ALIAS}/v1/vector_stores/{vs}/files/{file}")
}

// ---------------------------------------------------------------------------
// Multipart bodies and requests
// ---------------------------------------------------------------------------

pub const BOUNDARY: &str = "----mini-chat-test-boundary";

/// One part of a multipart body.
#[derive(Debug, Clone)]
pub struct Part<'a> {
    pub name: &'a str,
    pub filename: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub data: &'a [u8],
}

impl<'a> Part<'a> {
    /// The `file` part of an upload.
    #[must_use]
    pub fn file(filename: &'a str, content_type: &'a str, data: &'a [u8]) -> Self {
        Self {
            name: "file",
            filename: Some(filename),
            content_type: Some(content_type),
            data,
        }
    }

    /// A plain text field.
    #[must_use]
    pub fn field(name: &'a str, value: &'a str) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            data: value.as_bytes(),
        }
    }
}

/// The multipart body of `parts` closed by the final boundary.
#[must_use]
pub fn multipart_body(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = multipart_open(parts);
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// The multipart body of `parts` WITHOUT the closing boundary (a truncated body).
#[must_use]
pub fn multipart_open(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = part.filename {
            write!(disposition, "; filename=\"{filename}\"").unwrap();
        }
        body.extend_from_slice(disposition.as_bytes());
        body.extend_from_slice(b"\r\n");
        if let Some(content_type) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(part.data);
        body.extend_from_slice(b"\r\n");
    }
    body
}

/// A `POST` upload request with an explicit content type header and raw body.
#[must_use]
pub fn upload_request_raw(chat: &str, content_type: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(attachments_path(chat));
    if let Some(content_type) = content_type {
        builder = builder.header(http::header::CONTENT_TYPE, content_type);
    }
    builder.body(Body::from(body)).unwrap()
}

#[must_use]
pub fn multipart_content_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// Uploads `parts` as one multipart request.
pub async fn upload_parts(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    parts: &[Part<'_>],
) -> TestResponse {
    env.request_raw(
        user,
        upload_request_raw(chat, Some(&multipart_content_type()), multipart_body(parts)),
    )
    .await
}

/// Uploads one file.
pub async fn upload(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> TestResponse {
    upload_parts(env, user, chat, &[Part::file(filename, content_type, data)]).await
}

/// Uploads a small text document named `report.pdf`.
pub async fn upload_pdf(env: &TestEnv, user: &TestUser, chat: &str) -> TestResponse {
    upload(env, user, chat, "report.pdf", PDF, &pdf_bytes(2048)).await
}

/// Asserts `201` and returns the `AttachmentDetail`.
pub fn expect_created(resp: &TestResponse) -> Value {
    expect_status(resp, StatusCode::CREATED);
    resp.json()
}

/// The id of a created attachment.
#[must_use]
pub fn detail_id(detail: &Value) -> Uuid {
    Uuid::parse_str(detail["id"].as_str().expect("`id` is a string")).expect("`id` is a UUID")
}

pub async fn get_attachment(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    attachment: &str,
) -> TestResponse {
    get(env, user, &attachment_path(chat, attachment)).await
}

pub async fn delete_attachment(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    attachment: &str,
) -> TestResponse {
    env.request(
        user,
        Method::DELETE,
        &attachment_path(chat, attachment),
        None,
    )
    .await
}

/// Deletes the attachment and asserts `204`.
pub async fn delete_ok(env: &TestEnv, user: &TestUser, chat: &str, attachment: Uuid) {
    let resp = delete_attachment(env, user, chat, &attachment.to_string()).await;
    expect_status(&resp, StatusCode::NO_CONTENT);
    assert!(resp.body.is_empty(), "204 carries no body");
}

// ---------------------------------------------------------------------------
// File content generators
// ---------------------------------------------------------------------------

/// `len` bytes that look like a document (the fake provider does not parse them).
#[must_use]
pub fn pdf_bytes(len: usize) -> Vec<u8> {
    let mut bytes = b"%PDF-1.4 mini-chat test document ".to_vec();
    bytes.resize(len.max(bytes.len()), b'x');
    bytes
}

fn gradient(width: u32, height: u32) -> image::RgbImage {
    image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            u8::try_from(x % 251).unwrap(),
            u8::try_from(y % 241).unwrap(),
            u8::try_from((x * 7 + y * 13) % 239).unwrap(),
        ])
    })
}

fn encode(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(gradient(width, height))
        .write_to(&mut Cursor::new(&mut out), format)
        .expect("the test image encodes");
    out
}

/// A valid PNG of `width` x `height`.
#[must_use]
pub fn png(width: u32, height: u32) -> Vec<u8> {
    encode(image::ImageFormat::Png, width, height)
}

/// A valid JPEG of `width` x `height`.
#[must_use]
pub fn jpeg(width: u32, height: u32) -> Vec<u8> {
    encode(image::ImageFormat::Jpeg, width, height)
}

/// The dimensions of a WebP thumbnail (decoded, so the bytes must be a valid WebP).
#[must_use]
pub fn webp_dimensions(bytes: &[u8]) -> (u32, u32) {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .expect("the thumbnail is a valid WebP image");
    (decoded.width(), decoded.height())
}

/// Asserts the shape of an `img_thumbnail` object (see [`thumbnail_bytes`]).
pub fn assert_thumbnail(thumbnail: &Value) {
    assert!(!thumbnail_bytes(thumbnail).is_empty());
}

/// Asserts that `value` is an RFC 3339 timestamp string.
pub fn assert_timestamp(value: &Value) {
    let text = value.as_str().expect("a timestamp string");
    OffsetDateTime::parse(text, &Rfc3339)
        .unwrap_or_else(|error| panic!("`{text}` is not an RFC 3339 timestamp: {error}"));
}

/// Decodes the `img_thumbnail` object of an attachment (summary or detail) and asserts its shape.
/// Returns the WebP bytes.
#[must_use]
pub fn thumbnail_bytes(thumbnail: &Value) -> Vec<u8> {
    let object = thumbnail.as_object().expect("`img_thumbnail` is an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["content_type", "data_base64", "height", "width"],
        "{thumbnail}"
    );
    assert_eq!(thumbnail["content_type"], json!("image/webp"));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(thumbnail["data_base64"].as_str().expect("base64 string"))
        .expect("`data_base64` is standard base64");
    assert!(
        bytes.len() <= 131_072,
        "the thumbnail is at most 128 KiB: {}",
        bytes.len()
    );
    assert_eq!(&bytes[..4], b"RIFF", "a WebP container");
    assert_eq!(&bytes[8..12], b"WEBP", "a WebP container");
    let (width, height) = webp_dimensions(&bytes);
    assert_eq!(thumbnail["width"], json!(width), "{thumbnail}");
    assert_eq!(thumbnail["height"], json!(height), "{thumbnail}");
    bytes
}

// ---------------------------------------------------------------------------
// Provider objects (OpenAI JSON shapes) and scripts
// ---------------------------------------------------------------------------

#[must_use]
pub fn file_object(id: &str) -> Value {
    json!({
        "id": id, "object": "file", "bytes": 1, "created_at": 1_700_000_000,
        "filename": "upload", "purpose": "assistants", "status": "processed"
    })
}

#[must_use]
pub fn vector_store_object(id: &str) -> Value {
    json!({
        "id": id, "object": "vector_store", "name": "chat", "status": "completed",
        "usage_bytes": 0, "created_at": 1_700_000_000,
        "file_counts": { "in_progress": 0, "completed": 0, "failed": 0, "cancelled": 0, "total": 0 }
    })
}

/// A `vector_store.file` object; `status` `None` omits the member.
#[must_use]
pub fn vector_store_file(vs: &str, file: &str, status: Option<&str>) -> Value {
    let mut object = json!({
        "id": file, "object": "vector_store.file", "vector_store_id": vs,
        "created_at": 1_700_000_000, "usage_bytes": 0, "last_error": null
    });
    if let Some(status) = status {
        object["status"] = json!(status);
    }
    object
}

#[must_use]
pub fn deleted_object(id: &str, object: &str) -> Value {
    json!({ "id": id, "object": object, "deleted": true })
}

#[must_use]
pub fn ok(body: Value) -> ScriptedResponse {
    ScriptedResponse::Json { status: 200, body }
}

/// A provider error response with an OpenAI-style body.
#[must_use]
pub fn provider_error(status: u16) -> ScriptedResponse {
    ScriptedResponse::Error {
        status,
        body: json!({ "error": { "message": "provider failure", "type": "server_error" } }),
        headers: Vec::new(),
    }
}

/// The provider answers the next file upload with `file_id`.
pub fn script_file(env: &TestEnv, file_id: &str) {
    env.provider()
        .enqueue("/v1/files", ok(file_object(file_id)));
}

/// The provider answers the next vector store create with `vs`.
pub fn script_store_create(env: &TestEnv, vs: &str) {
    env.provider()
        .enqueue("/v1/vector_stores", ok(vector_store_object(vs)));
}

/// The provider answers the next "add file to vector store" call.
pub fn script_store_add(env: &TestEnv, vs: &str, file: &str, status: Option<&str>) {
    env.provider().enqueue(
        &format!("/vector_stores/{vs}/files"),
        ok(vector_store_file(vs, file, status)),
    );
}

/// The provider answers the next status read of the vector store file.
pub fn script_store_status(env: &TestEnv, vs: &str, file: &str, status: &str) {
    env.provider().enqueue(
        &format!("/vector_stores/{vs}/files/{file}"),
        ok(vector_store_file(vs, file, Some(status))),
    );
}

/// The provider fails the next status read of the vector store file with `status`.
pub fn script_store_status_error(env: &TestEnv, vs: &str, file: &str, status: u16) {
    env.provider().enqueue(
        &format!("/vector_stores/{vs}/files/{file}"),
        provider_error(status),
    );
}

/// Scripts the whole provider side of ONE successful document upload: the file, the vector store
/// (only when `create_store`: the first document of the chat) and the add-file call, which reports
/// `completed` at once.
pub fn script_document(env: &TestEnv, file: &str, vs: &str, create_store: bool) {
    script_file(env, file);
    if create_store {
        script_store_create(env, vs);
    }
    script_store_add(env, vs, file, Some("completed"));
}

/// Scripts the provider side of an image (or XLSX) upload: only the file.
pub fn script_plain_file(env: &TestEnv, file: &str) {
    script_file(env, file);
}

/// Uploads a PDF to a chat whose provider side is scripted by [`script_document`]; asserts `201 ready`
/// and returns the detail.
pub async fn upload_ready_document(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    filename: &str,
    file: &str,
    vs: &str,
    create_store: bool,
) -> Value {
    script_document(env, file, vs, create_store);
    let detail = expect_created(&upload(env, user, chat, filename, PDF, &pdf_bytes(2048)).await);
    assert_eq!(detail["status"], json!("ready"), "{detail}");
    detail
}

/// Uploads a PNG; asserts `201 ready` and returns the detail.
pub async fn upload_ready_image(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    filename: &str,
    file: &str,
) -> Value {
    script_plain_file(env, file);
    let detail = expect_created(&upload(env, user, chat, filename, PNG, &png(64, 64)).await);
    assert_eq!(detail["status"], json!("ready"), "{detail}");
    detail
}

// ---------------------------------------------------------------------------
// Provider request inspection
// ---------------------------------------------------------------------------

fn is_chat_path(path: &str) -> bool {
    ["/responses", "/chat/completions", "/messages"]
        .iter()
        .any(|marker| path.contains(marker))
}

/// Every proxied request that is not an LLM chat request, in order.
#[must_use]
pub fn calls(env: &TestEnv) -> Vec<RecordedRequest> {
    env.provider()
        .requests()
        .into_iter()
        .filter(|request| !is_chat_path(&request.path))
        .collect()
}

/// `"METHOD path"` of every non-chat request, in order.
#[must_use]
pub fn call_lines(env: &TestEnv) -> Vec<String> {
    calls(env)
        .iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect()
}

/// The non-chat requests with `method` whose path is exactly `path`.
#[must_use]
pub fn calls_to(env: &TestEnv, method: &str, path: &str) -> Vec<RecordedRequest> {
    calls(env)
        .into_iter()
        .filter(|request| request.method.eq_ignore_ascii_case(method) && request.path == path)
        .collect()
}

/// The paths of every `DELETE` request, in order.
#[must_use]
pub fn deletes(env: &TestEnv) -> Vec<String> {
    calls(env)
        .into_iter()
        .filter(|request| request.method.eq_ignore_ascii_case("DELETE"))
        .map(|request| request.path)
        .collect()
}

/// Position of the first request `"METHOD path"` in the request log.
#[must_use]
pub fn position_of(env: &TestEnv, method: &str, path: &str) -> Option<usize> {
    call_lines(env)
        .iter()
        .position(|line| *line == format!("{method} {path}"))
}

/// A header of a recorded request (case-insensitive name).
#[must_use]
pub fn request_header(request: &RecordedRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// True when `haystack` contains `needle` as a byte substring.
#[must_use]
pub fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Problem assertions
// ---------------------------------------------------------------------------

/// Asserts `503 service_unavailable` with `Retry-After: seconds` (header and
/// `context.retry_after_seconds`).
pub fn assert_unavailable(resp: &TestResponse, seconds: u64) -> Value {
    let problem = assert_problem(resp, 503, Some("service_unavailable"));
    assert_eq!(
        resp.headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some(seconds.to_string().as_str()),
        "{problem}"
    );
    assert_eq!(
        problem["context"]["retry_after_seconds"],
        json!(seconds),
        "{problem}"
    );
    problem
}

/// Asserts `404 not_found` naming the attachment resource type.
pub fn assert_attachment_not_found(resp: &TestResponse) -> Value {
    assert_not_found(resp, ATTACHMENT_RESOURCE_TYPE)
}

/// Asserts that `text` carries no provider identifier (`file-...`, `vs_...`, `assistant-...`).
pub fn assert_no_provider_ids(text: &str) {
    for marker in ["file-", "vs_", "assistant-", "file_"] {
        assert!(
            !text.contains(marker),
            "a provider identifier (`{marker}`) leaked: {text}"
        );
    }
}

/// Asserts that the object has exactly the members `keys`.
pub fn assert_keys(value: &Value, keys: &[&str]) {
    let mut actual: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("an object is expected: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    let mut expected = keys.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "{value}");
}

/// The members of a ready document / XLSX `AttachmentDetail`.
pub const DOCUMENT_DETAIL_KEYS: [&str; 7] = [
    "id",
    "filename",
    "content_type",
    "size_bytes",
    "status",
    "kind",
    "created_at",
];

// ---------------------------------------------------------------------------
// Database rows
// ---------------------------------------------------------------------------

async fn rows(env: &TestEnv, sql: String) -> Vec<QueryResult> {
    env.db()
        .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await
        .expect("row query must succeed")
}

/// An `attachments` row as the tests see it.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: Uuid,
    pub chat_hex: String,
    pub tenant_hex: String,
    pub uploaded_by_hex: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub storage_backend: String,
    pub provider_file_id: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub kind: String,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub thumbnail_len: Option<i64>,
    pub thumbnail_width: Option<i64>,
    pub thumbnail_height: Option<i64>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i64,
    pub last_cleanup_error: Option<String>,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: String,
    pub secondary_provider_kind: Option<String>,
}

const ROW_COLUMNS: &str = "hex(id) AS id_hex, hex(chat_id) AS chat_hex, hex(tenant_id) AS tenant_hex, \
    hex(uploaded_by_user_id) AS uploaded_by_hex, filename, content_type, \
    CAST(size_bytes AS INTEGER) AS size_bytes, storage_backend, provider_file_id, status, \
    error_code, attachment_kind, CAST(for_file_search AS INTEGER) AS for_fs, \
    CAST(for_code_interpreter AS INTEGER) AS for_ci, \
    CAST(LENGTH(img_thumbnail) AS INTEGER) AS thumb_len, \
    CAST(img_thumbnail_width AS INTEGER) AS thumb_w, \
    CAST(img_thumbnail_height AS INTEGER) AS thumb_h, cleanup_status, \
    CAST(cleanup_attempts AS INTEGER) AS attempts, last_cleanup_error, \
    CAST(updated_at AS TEXT) AS updated_text, CAST(deleted_at AS TEXT) AS deleted_text, \
    secondary_file_id, secondary_status, secondary_provider_kind";

fn parse_row(row: &QueryResult) -> AttachmentRow {
    let text = |column: &str| -> Option<String> { row.try_get("", column).unwrap() };
    let int = |column: &str| -> Option<i64> { row.try_get("", column).unwrap() };
    AttachmentRow {
        id: Uuid::parse_str(&text("id_hex").unwrap()).unwrap(),
        chat_hex: text("chat_hex").unwrap(),
        tenant_hex: text("tenant_hex").unwrap(),
        uploaded_by_hex: text("uploaded_by_hex").unwrap(),
        filename: text("filename").unwrap(),
        content_type: text("content_type"),
        size_bytes: int("size_bytes"),
        storage_backend: text("storage_backend").unwrap(),
        provider_file_id: text("provider_file_id"),
        status: text("status").unwrap(),
        error_code: text("error_code"),
        kind: text("attachment_kind").unwrap(),
        for_file_search: int("for_fs").unwrap() != 0,
        for_code_interpreter: int("for_ci").unwrap() != 0,
        thumbnail_len: int("thumb_len"),
        thumbnail_width: int("thumb_w"),
        thumbnail_height: int("thumb_h"),
        cleanup_status: text("cleanup_status"),
        cleanup_attempts: int("attempts").unwrap_or(0),
        last_cleanup_error: text("last_cleanup_error"),
        updated_at: text("updated_text").unwrap(),
        deleted_at: text("deleted_text"),
        secondary_file_id: text("secondary_file_id"),
        secondary_status: text("secondary_status").unwrap_or_default(),
        secondary_provider_kind: text("secondary_provider_kind"),
    }
}

/// The row of `attachment`, if it exists.
pub async fn attachment_row(env: &TestEnv, attachment: Uuid) -> Option<AttachmentRow> {
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM attachments WHERE hex(id) = '{}'",
        uuid_hex(attachment)
    );
    rows(env, sql).await.first().map(parse_row)
}

/// The row of `attachment` (panics when it does not exist).
pub async fn row(env: &TestEnv, attachment: Uuid) -> AttachmentRow {
    attachment_row(env, attachment)
        .await
        .unwrap_or_else(|| panic!("no attachments row {attachment}"))
}

/// Every row of the chat, oldest first.
pub async fn chat_rows(env: &TestEnv, chat: &str) -> Vec<AttachmentRow> {
    let sql = format!(
        "SELECT {ROW_COLUMNS} FROM attachments WHERE hex(chat_id) = '{}' ORDER BY created_at, id",
        uuid_hex(Uuid::parse_str(chat).unwrap())
    );
    rows(env, sql).await.iter().map(parse_row).collect()
}

/// The number of `attachments` rows (all chats).
pub async fn row_count(env: &TestEnv) -> i64 {
    crate::common::scalar_i64(env, "SELECT COUNT(*) AS n FROM attachments").await
}

/// A `chat_vector_stores` row.
#[derive(Debug, Clone)]
pub struct StoreRow {
    pub vector_store_id: Option<String>,
    pub provider: String,
    pub file_count: i64,
}

/// The vector store rows of the chat.
pub async fn store_rows(env: &TestEnv, chat: &str) -> Vec<StoreRow> {
    let sql = format!(
        "SELECT vector_store_id, provider, CAST(file_count AS INTEGER) AS file_count \
         FROM chat_vector_stores WHERE hex(chat_id) = '{}'",
        uuid_hex(Uuid::parse_str(chat).unwrap())
    );
    rows(env, sql)
        .await
        .iter()
        .map(|row| StoreRow {
            vector_store_id: row.try_get("", "vector_store_id").unwrap(),
            provider: row.try_get("", "provider").unwrap(),
            file_count: row.try_get("", "file_count").unwrap(),
        })
        .collect()
}

/// The links of `message_attachments` of the chat as `(message, attachment)`.
pub async fn link_rows(env: &TestEnv, chat: &str) -> Vec<(String, String)> {
    let sql = format!(
        "SELECT hex(message_id) AS m, hex(attachment_id) AS a FROM message_attachments \
         WHERE hex(chat_id) = '{}' ORDER BY m, a",
        uuid_hex(Uuid::parse_str(chat).unwrap())
    );
    rows(env, sql)
        .await
        .iter()
        .map(|row| (row.try_get("", "m").unwrap(), row.try_get("", "a").unwrap()))
        .collect()
}

/// Soft-deletes the chat with SQL (no outbox message, no attachment marking).
pub async fn soft_delete_chat_sql(env: &TestEnv, chat: &str) {
    exec_sql(
        env,
        &format!(
            "UPDATE chats SET deleted_at = '2026-01-02T00:00:00Z' WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(chat).unwrap())
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Seeded rows
// ---------------------------------------------------------------------------

/// Builder of an `attachments` row inserted with SQL.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat mirror of the table columns"
)]
#[derive(Debug, Clone)]
pub struct RowSeed {
    tenant: Uuid,
    chat: Uuid,
    uploaded_by: Uuid,
    status: String,
    kind: String,
    content_type: String,
    filename: String,
    provider_file_id: Option<String>,
    updated_at: String,
    cleanup_status: Option<String>,
    deleted: bool,
    for_file_search: bool,
    for_code_interpreter: bool,
    error_code: Option<String>,
    thumbnail: Option<(Vec<u8>, i32, i32)>,
    size_bytes: i64,
    secondary_file_id: Option<String>,
    secondary_status: String,
    cleanup_attempts: i32,
}

impl RowSeed {
    /// A document row of `user` in `chat`: `uploaded`, no provider file, old.
    #[must_use]
    pub fn new(user: &TestUser, chat: &str) -> Self {
        Self {
            tenant: user.tenant_id,
            chat: Uuid::parse_str(chat).unwrap(),
            uploaded_by: user.user_id,
            status: "uploaded".to_owned(),
            kind: "document".to_owned(),
            content_type: TEXT.to_owned(),
            filename: "doc.txt".to_owned(),
            provider_file_id: None,
            updated_at: OLD.to_owned(),
            cleanup_status: None,
            deleted: false,
            for_file_search: true,
            for_code_interpreter: false,
            error_code: None,
            thumbnail: None,
            size_bytes: 10,
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
            cleanup_attempts: 0,
        }
    }

    #[must_use]
    pub fn status(mut self, status: &str) -> Self {
        status.clone_into(&mut self.status);
        self
    }

    #[must_use]
    pub fn image(mut self) -> Self {
        "image".clone_into(&mut self.kind);
        PNG.clone_into(&mut self.content_type);
        "cat.png".clone_into(&mut self.filename);
        self.for_file_search = false;
        self
    }

    #[must_use]
    pub fn filename(mut self, filename: &str) -> Self {
        filename.clone_into(&mut self.filename);
        self
    }

    #[must_use]
    pub fn provider_file(mut self, id: &str) -> Self {
        self.provider_file_id = Some(id.to_owned());
        self
    }

    #[must_use]
    pub fn updated_at(mut self, at: &str) -> Self {
        at.clone_into(&mut self.updated_at);
        self
    }

    #[must_use]
    pub fn cleanup(mut self, status: &str) -> Self {
        self.cleanup_status = Some(status.to_owned());
        self
    }

    #[must_use]
    pub fn cleanup_attempts(mut self, attempts: i32) -> Self {
        self.cleanup_attempts = attempts;
        self
    }

    #[must_use]
    pub fn deleted(mut self) -> Self {
        self.deleted = true;
        self
    }

    #[must_use]
    pub fn uploaded_by(mut self, user: Uuid) -> Self {
        self.uploaded_by = user;
        self
    }

    #[must_use]
    pub fn error_code(mut self, code: &str) -> Self {
        self.error_code = Some(code.to_owned());
        self
    }

    #[must_use]
    pub fn thumbnail(mut self, bytes: &[u8], width: i32, height: i32) -> Self {
        self.thumbnail = Some((bytes.to_vec(), width, height));
        self
    }

    #[must_use]
    pub fn secondary(mut self, file_id: &str) -> Self {
        self.secondary_file_id = Some(file_id.to_owned());
        "uploaded".clone_into(&mut self.secondary_status);
        self
    }

    /// Inserts the row and returns its id.
    pub async fn insert(self, env: &TestEnv) -> Uuid {
        let id = Uuid::new_v4();
        let literal = |value: &Option<String>| {
            value
                .as_ref()
                .map_or_else(|| "NULL".to_owned(), |text| format!("'{text}'"))
        };
        let thumbnail = self.thumbnail.as_ref().map_or_else(
            || ("NULL".to_owned(), "NULL".to_owned(), "NULL".to_owned()),
            |(bytes, width, height)| {
                (
                    format!("X'{}'", hex_of(bytes)),
                    width.to_string(),
                    height.to_string(),
                )
            },
        );
        let deleted = if self.deleted {
            "'2026-01-01T00:00:01Z'"
        } else {
            "NULL"
        };
        let secondary_kind = if self.secondary_file_id.is_some() {
            "'anthropic'"
        } else {
            "NULL"
        };
        exec_sql(
            env,
            &format!(
                "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
                 content_type, size_bytes, storage_backend, provider_file_id, status, error_code, \
                 attachment_kind, for_file_search, for_code_interpreter, img_thumbnail, \
                 img_thumbnail_width, img_thumbnail_height, cleanup_status, cleanup_attempts, \
                 secondary_file_id, secondary_status, secondary_provider_kind, created_at, \
                 updated_at, deleted_at) VALUES ({}, {}, {}, {}, '{}', '{}', {}, 'openai', {}, '{}', \
                 {}, '{}', {}, {}, {}, {}, {}, {}, {}, {}, '{}', {secondary_kind}, '{OLD}', '{}', {deleted})",
                crate::common::blob(id),
                crate::common::blob(self.tenant),
                crate::common::blob(self.chat),
                crate::common::blob(self.uploaded_by),
                self.filename,
                self.content_type,
                self.size_bytes,
                literal(&self.provider_file_id),
                self.status,
                literal(&self.error_code),
                self.kind,
                i32::from(self.for_file_search),
                i32::from(self.for_code_interpreter),
                thumbnail.0,
                thumbnail.1,
                thumbnail.2,
                literal(&self.cleanup_status),
                self.cleanup_attempts,
                literal(&self.secondary_file_id),
                self.secondary_status,
                self.updated_at,
            ),
        )
        .await;
        id
    }
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02X}").unwrap();
        out
    })
}

/// Inserts the vector store row of a chat (`vector_store_id` `None` = creation in progress).
pub async fn seed_store(env: &TestEnv, user: &TestUser, chat: &str, vs: Option<&str>) {
    let vs = vs.map_or_else(|| "NULL".to_owned(), |id| format!("'{id}'"));
    exec_sql(
        env,
        &format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, \
             file_count, created_at) VALUES ({}, {}, {}, {vs}, 'openai', 0, '{OLD}')",
            crate::common::blob(Uuid::new_v4()),
            crate::common::blob(user.tenant_id),
            crate::common::blob(Uuid::parse_str(chat).unwrap()),
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Waiting (the background work runs on real time)
// ---------------------------------------------------------------------------

const WAIT: std::time::Duration = std::time::Duration::from_secs(15);
const STEP: std::time::Duration = std::time::Duration::from_millis(20);

/// Polls the row until `done` accepts it (at most 15 s) and returns it.
pub async fn wait_row(
    env: &TestEnv,
    attachment: Uuid,
    what: &str,
    done: impl Fn(&AttachmentRow) -> bool,
) -> AttachmentRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let current = attachment_row(env, attachment).await;
        if let Some(current) = &current
            && done(current)
        {
            return current.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "attachment {attachment}: timed out waiting for {what}; last row: {current:?}"
        );
        tokio::time::sleep(STEP).await;
    }
}

/// Polls `GET` of the attachment until its `status` is `status` and returns the detail.
pub async fn wait_status(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    attachment: Uuid,
    status: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let resp = get_attachment(env, user, chat, &attachment.to_string()).await;
        if resp.status == StatusCode::OK && resp.json()["status"] == json!(status) {
            return resp.json();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "attachment {attachment}: timed out waiting for status `{status}`; last: {} {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        );
        tokio::time::sleep(STEP).await;
    }
}

/// Polls the request log until at least `count` requests `"METHOD path"` were made.
pub async fn wait_calls(env: &TestEnv, method: &str, path: &str, count: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let seen = calls_to(env, method, path).len();
        if seen >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {count} x {method} {path}; seen {seen}: {:?}",
            call_lines(env)
        );
        tokio::time::sleep(STEP).await;
    }
}

/// Waits until `queue` has at least `count` handler outcomes (at most 20 s: a retry waits for the
/// shared outbox backoff) and returns them all, in order.
pub async fn wait_outcomes(env: &TestEnv, queue: &str, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let outcomes = env.outbox_outcomes(queue).await;
        if outcomes.len() >= count {
            return outcomes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queue `{queue}` has {outcomes:?}, expected {count} handler outcomes"
        );
        tokio::time::sleep(STEP).await;
    }
}

/// Waits until every message the gear enqueued on `queue` was handled to completion (`ok` or
/// `reject`; retries do not count) and returns the outcomes.
pub async fn wait_queue_handled(env: &TestEnv, queue: &str) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let enqueued = env.outbox_payloads(queue).await.len();
        let outcomes = env.outbox_outcomes(queue).await;
        let finished = outcomes
            .iter()
            .filter(|outcome| outcome.as_str() != "retry")
            .count();
        if finished >= enqueued {
            return outcomes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queue `{queue}`: {enqueued} enqueued, outcomes {outcomes:?}"
        );
        tokio::time::sleep(STEP).await;
    }
}

/// Waits until the attachment cleanup and chat cleanup queues handled everything enqueued so far
/// (the "drain" of the cleanup queues).
pub async fn drain_cleanup(env: &TestEnv) {
    wait_queue_handled(env, ATTACHMENT_CLEANUP_QUEUE).await;
    wait_queue_handled(env, CHAT_CLEANUP_QUEUE).await;
}

/// A short pause for "nothing happens" assertions on background work.
pub async fn settle(millis: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

/// RFC 3339 `Z` literal of `now_utc() + secs`.
#[must_use]
pub fn instant(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}
