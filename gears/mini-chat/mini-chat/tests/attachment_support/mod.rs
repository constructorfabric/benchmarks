//! S6 (attachments) harness on top of the S1 fakes, the S2 chat harness, the S3 stream pieces and
//! the S4 quota pieces.
//!
//! Kept apart from `common`, `chat_support`, `stream_support`, `quota_support` and
//! `mutation_support` so the S1 - S5 test binaries (which never declare `mod attachment_support;`)
//! compile independently of the S6 production items. Everything here relies only on the items
//! listed in `S6-test-interface.md`. Test files declare `mod common; mod chat_support;
//! mod stream_support; mod quota_support; mod attachment_support;`.
//!
//! * [`StorageOagw`] is a second in-process `ServiceGatewayClientV1`: the Files API and Vector
//!   Store API of the provider (upload, delete, create store, add file, poll file, delete store).
//!   It records every call (multipart parts parsed) and answers from scripts or defaults. The chat
//!   provider (Responses SSE) stays the S3 `FakeOagw`, so the two kinds of provider traffic are
//!   recorded apart.
//! * [`AttachmentHarness`] wires the real `ChatsService`, `StreamService`, `TurnsService`,
//!   `TurnMutationService`, `AttachmentService` and `UploadReaper` over the S2 database, with one
//!   running outbox serving the usage, audit, attachment-cleanup and chat-cleanup queues (the
//!   cleanup queues run the real handlers or recording stand-ins).
//! * [`AttRow`] seeds `attachments` rows with every column the lifecycle, the reaper and the
//!   cleanup handlers read.

#![allow(
    dead_code,
    unused_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::integer_division,
    clippy::struct_excessive_bools,
    clippy::cast_possible_truncation
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body as AxumBody, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use mini_chat::api::rest::{
    register_attachment_routes, register_chat_routes, register_stream_routes,
    register_turn_mutation_routes,
};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::attachments::{AttachmentService, IndexingTimings};
use mini_chat::domain::chats::ChatsService;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::messages::MessagesService;
use mini_chat::domain::reactions::ReactionsService;
use mini_chat::domain::streaming::StreamService;
use mini_chat::domain::turn_mutations::TurnMutationService;
use mini_chat::domain::turns::TurnsService;
use mini_chat::domain::upload_reaper::UploadReaper;
use mini_chat::infra::db::entity::{
    attachments, chat_turns, chat_vector_stores, message_attachments, messages, quota_usage,
};
use mini_chat::infra::db::timestamp::{to_db, to_db_opt};
use mini_chat::infra::llm::LlmProviders;
use mini_chat::infra::llm::storage::ProviderStorage;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat::infra::outbox::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AttachmentCleanupHandler, AttachmentCleanupOutbox,
    AuditOutbox, AuditOutboxHandler, CHAT_CLEANUP_PAYLOAD_TYPE, ChatCleanupHandler,
    ChatCleanupOutbox, UsageOutbox, UsageOutboxHandler,
};
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::Body;
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::{
    DeadLetterFilter, DeadLetterScope, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle,
    OutboxMessage, OutboxProfile, Partitions,
};
use toolkit_db::secure::{
    SecureEntityExt, SecureInsertExt, SecureOnConflict, SecureUpdateExt, secure_insert,
};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::chat_support::{
    AttachmentSeed, ChatHarness, ChatHarnessOptions, ChatSeed, MessageSeed, USER_A2,
    assert_field_violation, assert_not_found, assert_precondition, assert_violation_reason,
    from_db, page_items, parse_ts, ts, uid,
};
pub use crate::common::{
    FakePdp, PdpMode, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem, ctx, ctx_a,
};
pub use crate::quota_support::{
    LimitsSpec, PREMIUM, PREMIUM_IN, PREMIUM_OUT, QuotaPolicy, RecordingAudit, STANDARD,
    STANDARD_IN, STANDARD_OUT, UsageSeed, assert_exhausted, audit_gateway_for, find_model,
    period_start_today, quota_models, ref_credits, ref_estimate, ref_reserve,
};
pub use crate::stream_support::{
    FakeOagw, LiveSse, Opts, Piece, ProviderReply, RawStream, Sent, assert_400_reason,
    assert_conflict, code_interpreter_pieces, contains_provider_identifier, ev, eventually,
    file_annotation, file_search_pieces, hex, merge, model_entries, norm_id, responses_stream,
    second_user, stream_config, stream_model_json, tenant_b_user, web_search_pieces,
};

/// `PROVIDER_ALIAS` of the stream-test provider entry: the first path segment of every call.
pub use crate::stream_support::PROVIDER_ALIAS;

/// Queue of the attachment cleanup (`outbox.cleanup_queue_name` default).
pub const ATTACHMENT_CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";
/// Queue of the chat cleanup (`outbox.chat_cleanup_queue_name` default).
pub const CHAT_CLEANUP_QUEUE: &str = crate::chat_support::CHAT_CLEANUP_QUEUE;

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
pub const PDF: &str = "application/pdf";

/// Bytes of a small PDF-looking document (the content is never inspected).
pub const PDF_BYTES: &[u8] = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n";

// ── multipart bodies ─────────────────────────────────────────────────────────

/// One multipart part (also what the recorded provider upload is parsed into).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl Part {
    /// The `file` part of an upload.
    pub fn file(filename: Option<&str>, content_type: Option<&str>, data: &[u8]) -> Self {
        Self {
            name: "file".to_owned(),
            filename: filename.map(str::to_owned),
            content_type: content_type.map(str::to_owned),
            data: data.to_vec(),
        }
    }

    /// A plain text field.
    pub fn field(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            filename: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

pub const BOUNDARY: &str = "----s6-test-boundary-7MA4YWxkTrZu0gW";

/// `Content-Type` header value of a multipart body with [`BOUNDARY`].
pub fn multipart_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// A well-formed multipart body of `parts`.
pub fn multipart_body(boundary: &str, parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = &part.filename {
            disposition.push_str("; filename=\"");
            disposition.push_str(filename);
            disposition.push('"');
        }
        out.extend_from_slice(disposition.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(content_type) = &part.content_type {
            out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&part.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// The `boundary` parameter of a multipart `Content-Type` value.
pub fn boundary_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let at = lower.find("boundary=")?;
    let rest = &content_type[at + "boundary=".len()..];
    let value = rest.split(';').next()?.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_owned())
}

fn header_param(line: &str, key: &str) -> Option<String> {
    let marker = format!("{key}=\"");
    let at = line.find(&marker)? + marker.len();
    let end = line[at..].find('"')?;
    Some(line[at..at + end].to_owned())
}

/// Parses a multipart body (as sent to the provider Files API) into its parts.
pub fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<Part> {
    let Some(boundary) = boundary_of(content_type) else {
        return Vec::new();
    };
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut cursor = find(body, &delimiter, 0);
    while let Some(start) = cursor {
        let after = start + delimiter.len();
        if body[after..].starts_with(b"--") {
            break;
        }
        let head_start = if body[after..].starts_with(b"\r\n") {
            after + 2
        } else {
            after
        };
        let next = find(body, &delimiter, head_start);
        let Some(next_start) = next else { break };
        // the CRLF before the next delimiter belongs to the delimiter
        let chunk_end = next_start.saturating_sub(2).max(head_start);
        let chunk = &body[head_start..chunk_end];
        if let Some(split) = find(chunk, b"\r\n\r\n", 0) {
            let head = String::from_utf8_lossy(&chunk[..split]).into_owned();
            let data = chunk[split + 4..].to_vec();
            let mut name = String::new();
            let mut filename = None;
            let mut content_type = None;
            for line in head.split("\r\n") {
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("content-disposition:") {
                    name = header_param(line, "name").unwrap_or_default();
                    filename = header_param(line, "filename");
                } else if lower.starts_with("content-type:") {
                    content_type = Some(line["content-type:".len()..].trim().to_owned());
                }
            }
            parts.push(Part {
                name,
                filename,
                content_type,
                data,
            });
        }
        cursor = Some(next_start);
    }
    parts
}

// ── file contents ────────────────────────────────────────────────────────────

/// An encoded image of `width` x `height` pixels.
pub fn encoded_image(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
    let buffer = image::ImageBuffer::from_fn(width, height, |x, y| {
        image::Rgb([
            u8::try_from(x % 256).unwrap(),
            u8::try_from(y % 256).unwrap(),
            128_u8,
        ])
    });
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(buffer)
        .write_to(&mut out, format)
        .expect("image encodes");
    out.into_inner()
}

pub fn png(width: u32, height: u32) -> Vec<u8> {
    encoded_image(width, height, image::ImageFormat::Png)
}

pub fn jpeg(width: u32, height: u32) -> Vec<u8> {
    encoded_image(width, height, image::ImageFormat::Jpeg)
}

/// `size` bytes of filler.
pub fn filler(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect()
}

// ── the fake provider storage (Files API, Vector Store API) ──────────────────

/// Which provider storage operation a call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StorageOp {
    /// `POST {prefix}/files`.
    UploadFile,
    /// `DELETE {prefix}/files/{id}`.
    DeleteFile,
    /// `POST {prefix}/vector_stores`.
    CreateVectorStore,
    /// `POST {prefix}/vector_stores/{id}/files`.
    AddVectorStoreFile,
    /// `GET {prefix}/vector_stores/{id}/files/{file_id}`.
    GetVectorStoreFile,
    /// `DELETE {prefix}/vector_stores/{id}`.
    DeleteVectorStore,
    /// Anything else (recorded, answered 404).
    Other,
}

/// What the fake answers to one storage call.
#[derive(Clone, Debug)]
pub enum StorageReply {
    /// The provider answers with this status and JSON body.
    Http(u16, Value),
    /// The gateway fails the call itself (`service_unavailable`).
    Unavailable,
}

impl StorageReply {
    pub fn ok(body: Value) -> Self {
        Self::Http(200, body)
    }

    /// A provider error answer with a secret-looking message (it must never reach a client).
    pub fn error(status: u16, message: &str) -> Self {
        Self::Http(
            status,
            json!({"error": {"message": message, "type": "server_error", "code": "mock_error"}}),
        )
    }

    /// `{"id": id, "status": status}` of a vector store file.
    pub fn vs_file(status: &str) -> Self {
        Self::Http(
            200,
            json!({"id": "file-vsfile", "object": "vector_store.file", "status": status}),
        )
    }

    /// A vector store file answer without a `status` field.
    pub fn vs_file_without_status() -> Self {
        Self::Http(
            200,
            json!({"id": "file-vsfile", "object": "vector_store.file"}),
        )
    }

    pub fn not_found() -> Self {
        Self::error(404, "No such resource")
    }
}

/// One call the fake storage received.
#[derive(Clone, Debug)]
pub struct StorageCall {
    pub op: StorageOp,
    pub method: http::Method,
    pub uri: http::Uri,
    pub headers: HeaderMap,
    pub raw: Bytes,
    pub subject_id: Uuid,
    pub tenant_id: Uuid,
    pub at: Instant,
}

impl StorageCall {
    /// `{path}` of the proxied URI (`/{alias}/v1/files`).
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    pub fn query(&self) -> &str {
        self.uri.query().unwrap_or_default()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.raw).unwrap_or(Value::Null)
    }

    pub fn content_type(&self) -> String {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    pub fn parts(&self) -> Vec<Part> {
        parse_multipart(&self.content_type(), &self.raw)
    }

    pub fn part(&self, name: &str) -> Option<Part> {
        self.parts().into_iter().find(|p| p.name == name)
    }

    /// The last path segment (a file id for file calls, the store id for a store delete).
    pub fn last_segment(&self) -> String {
        self.path()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    /// The vector store id of a `vector_stores/{id}/...` call.
    pub fn vector_store_id(&self) -> String {
        let segments: Vec<&str> = self.path().trim_matches('/').split('/').collect();
        segments
            .iter()
            .position(|s| *s == "vector_stores")
            .and_then(|i| segments.get(i + 1))
            .map(|s| (*s).to_owned())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct StorageState {
    queues: HashMap<StorageOp, VecDeque<StorageReply>>,
    sticky: HashMap<StorageOp, StorageReply>,
    holds: HashSet<StorageOp>,
    calls: Vec<StorageCall>,
    files: usize,
    stores: usize,
}

/// In-process fake of the provider Files and Vector Store APIs behind OAGW.
///
/// Defaults: an upload answers `{"id": "file-s6fake<n>", ...}`, a store creation
/// `{"id": "vs_s6fake<n>"}`, adding a file and polling it `status: completed`, deletes
/// `{"deleted": true}`. Scripted answers (`push`, FIFO per operation) win over `set_default`
/// (sticky), which wins over the default. `hold` pauses every call of an operation until
/// `release`.
pub struct StorageOagw {
    state: Mutex<StorageState>,
    paused: AtomicUsize,
}

fn classify(method: &http::Method, path: &str) -> StorageOp {
    let mut segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    if segments.is_empty() {
        return StorageOp::Other;
    }
    segments.remove(0); // the OAGW alias
    if segments.first() == Some(&"openai") {
        segments.remove(0);
    }
    if segments.first() == Some(&"v1") {
        segments.remove(0);
    }
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["files"]) => StorageOp::UploadFile,
        ("DELETE", ["files", _]) => StorageOp::DeleteFile,
        ("POST", ["vector_stores"]) => StorageOp::CreateVectorStore,
        ("POST", ["vector_stores", _, "files"]) => StorageOp::AddVectorStoreFile,
        ("GET", ["vector_stores", _, "files", _]) => StorageOp::GetVectorStoreFile,
        ("DELETE", ["vector_stores", _]) => StorageOp::DeleteVectorStore,
        _ => StorageOp::Other,
    }
}

impl StorageOagw {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(StorageState::default()),
            paused: AtomicUsize::new(0),
        })
    }

    // ── scripting ────────────────────────────────────────────────────────────

    /// Queue one answer for `op` (FIFO).
    pub fn push(&self, op: StorageOp, reply: StorageReply) {
        self.state
            .lock()
            .unwrap()
            .queues
            .entry(op)
            .or_default()
            .push_back(reply);
    }

    /// Queue `reply` `times` times.
    pub fn push_n(&self, op: StorageOp, reply: StorageReply, times: usize) {
        for _ in 0..times {
            self.push(op, reply.clone());
        }
    }

    /// Every call of `op` is answered with `reply` once the one-shot queue is empty.
    pub fn set_default(&self, op: StorageOp, reply: StorageReply) {
        self.state.lock().unwrap().sticky.insert(op, reply);
    }

    pub fn clear_default(&self, op: StorageOp) {
        self.state.lock().unwrap().sticky.remove(&op);
    }

    /// The next polls of a vector store file answer these statuses (then `completed` again).
    pub fn index_statuses(&self, statuses: &[&str]) {
        for status in statuses {
            self.push(StorageOp::GetVectorStoreFile, StorageReply::vs_file(status));
        }
    }

    /// The vector store file is `in_progress` for every poll and for the add call.
    pub fn never_finishes_indexing(&self) {
        self.set_default(
            StorageOp::GetVectorStoreFile,
            StorageReply::vs_file("in_progress"),
        );
        self.set_default(
            StorageOp::AddVectorStoreFile,
            StorageReply::vs_file("in_progress"),
        );
    }

    /// The add call answers `in_progress` (the default would answer `completed`).
    pub fn adding_answers_in_progress(&self) {
        self.set_default(
            StorageOp::AddVectorStoreFile,
            StorageReply::vs_file("in_progress"),
        );
    }

    /// Pause every call of `op` until [`Self::release`].
    pub fn hold(&self, op: StorageOp) {
        self.state.lock().unwrap().holds.insert(op);
    }

    pub fn release(&self) {
        self.state.lock().unwrap().holds.clear();
    }

    /// Wait (10 s) until `n` calls sit on a hold.
    pub async fn wait_paused(&self, n: usize) {
        for _ in 0..1000 {
            if self.paused.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} paused storage call(s), got {}",
            self.paused.load(Ordering::SeqCst)
        );
    }

    // ── recording ────────────────────────────────────────────────────────────

    pub fn calls(&self) -> Vec<StorageCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn calls_to(&self, op: StorageOp) -> Vec<StorageCall> {
        self.calls().into_iter().filter(|c| c.op == op).collect()
    }

    pub fn count(&self, op: StorageOp) -> usize {
        self.calls_to(op).len()
    }

    pub fn assert_no_calls(&self) {
        let calls = self.calls();
        assert!(
            calls.is_empty(),
            "the provider storage must not be called, got {:?}",
            calls
                .iter()
                .map(|c| format!("{} {}", c.method, c.uri))
                .collect::<Vec<_>>()
        );
    }

    /// Operations in call order.
    pub fn ops(&self) -> Vec<StorageOp> {
        self.calls().iter().map(|c| c.op).collect()
    }

    /// Wait (10 s) until `op` was called `n` times; returns the calls.
    pub async fn wait_calls(&self, op: StorageOp, n: usize) -> Vec<StorageCall> {
        for _ in 0..1000 {
            let calls = self.calls_to(op);
            if calls.len() >= n {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} {op:?} call(s), got {:?}",
            self.calls()
                .iter()
                .map(|c| format!("{} {}", c.method, c.uri))
                .collect::<Vec<_>>()
        );
    }

    /// Give the background work time to (not) call `op`, then assert the count.
    pub async fn assert_count_stays(&self, op: StorageOp, n: usize) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(self.count(op), n, "{op:?} calls: {:?}", self.ops());
    }

    fn default_reply(&self, op: StorageOp, call: &StorageCall) -> StorageReply {
        let mut state = self.state.lock().unwrap();
        match op {
            StorageOp::UploadFile => {
                state.files += 1;
                let parts = call.parts();
                let file = parts.iter().find(|p| p.name == "file");
                StorageReply::ok(json!({
                    "id": format!("file-s6fake{:010}", state.files),
                    "object": "file",
                    "bytes": file.map_or(0, |p| p.data.len()),
                    "created_at": 0,
                    "filename": file.and_then(|p| p.filename.clone()).unwrap_or_default(),
                    "purpose": parts.iter().find(|p| p.name == "purpose").map(Part::text)
                        .unwrap_or_default(),
                    "status": "processed",
                }))
            }
            StorageOp::DeleteFile => StorageReply::ok(
                json!({"id": call.last_segment(), "object": "file", "deleted": true}),
            ),
            StorageOp::CreateVectorStore => {
                state.stores += 1;
                StorageReply::ok(json!({
                    "id": format!("vs_s6fake{:010}", state.stores),
                    "object": "vector_store",
                    "status": "completed",
                }))
            }
            StorageOp::AddVectorStoreFile => StorageReply::ok(json!({
                "id": call.json()["file_id"],
                "object": "vector_store.file",
                "status": "completed",
                "vector_store_id": call.vector_store_id(),
            })),
            StorageOp::GetVectorStoreFile => StorageReply::ok(json!({
                "id": call.last_segment(),
                "object": "vector_store.file",
                "status": "completed",
                "vector_store_id": call.vector_store_id(),
            })),
            StorageOp::DeleteVectorStore => StorageReply::ok(json!({
                "id": call.last_segment(), "object": "vector_store.deleted", "deleted": true,
            })),
            StorageOp::Other => StorageReply::not_found(),
        }
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for StorageOagw {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        _req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("create_upstream"))
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("get_upstream"))
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Err(unsupported("list_upstreams"))
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("update_upstream"))
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        Err(unsupported("delete_upstream"))
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        _req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unsupported("create_route"))
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        Err(unsupported("get_route"))
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Err(unsupported("list_routes"))
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unsupported("update_route"))
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Err(unsupported("delete_route"))
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unsupported("resolve_proxy_target"))
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let raw = body
            .into_bytes()
            .await
            .map_err(|e| CanonicalError::internal(format!("fake storage body: {e}")).create())?;
        let op = classify(&parts.method, parts.uri.path());
        let call = StorageCall {
            op,
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            raw,
            subject_id: ctx.subject_id(),
            tenant_id: ctx.subject_tenant_id(),
            at: Instant::now(),
        };
        self.state.lock().unwrap().calls.push(call.clone());

        if self.state.lock().unwrap().holds.contains(&op) {
            self.paused.fetch_add(1, Ordering::SeqCst);
            while self.state.lock().unwrap().holds.contains(&op) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.paused.fetch_sub(1, Ordering::SeqCst);
        }
        let scripted = {
            let mut state = self.state.lock().unwrap();
            let queued = state.queues.get_mut(&op).and_then(VecDeque::pop_front);
            queued.or_else(|| state.sticky.get(&op).cloned())
        };
        let reply = scripted.unwrap_or_else(|| self.default_reply(op, &call));
        match reply {
            StorageReply::Http(status, body) => {
                let mut resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("response");
                resp.extensions_mut().insert(ErrorSource::Upstream);
                Ok(resp)
            }
            StorageReply::Unavailable => Err(CanonicalError::service_unavailable().create()),
        }
    }
}

fn unsupported(what: &str) -> CanonicalError {
    CanonicalError::internal(format!("fake storage oagw: {what} is not scripted")).create()
}

// ── policy with a failure switch ─────────────────────────────────────────────

/// The S4 policy plugin behind a switch: `fail_with` makes every call answer an internal error
/// (the model-policy plugin failure of the upload handler).
pub struct FlakyPolicy {
    pub inner: Arc<QuotaPolicy>,
    failure: Mutex<Option<String>>,
}

impl FlakyPolicy {
    pub fn new(inner: Arc<QuotaPolicy>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failure: Mutex::new(None),
        })
    }

    pub fn fail_with(&self, message: &str) {
        *self.failure.lock().unwrap() = Some(message.to_owned());
    }

    pub fn recover(&self) {
        *self.failure.lock().unwrap() = None;
    }

    fn check(&self) -> Result<(), ModelPolicyPluginError> {
        match self.failure.lock().unwrap().clone() {
            Some(message) => Err(ModelPolicyPluginError::Internal(message)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FlakyPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        self.check()?;
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.check()?;
        self.inner
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.check()?;
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.inner.publish_usage(payload).await
    }
}

// ── recording stand-ins for the cleanup queues ───────────────────────────────

/// Payloads delivered to a recording cleanup handler.
#[derive(Default)]
pub struct CleanupLog {
    delivered: Mutex<Vec<Value>>,
}

impl CleanupLog {
    pub fn received(&self) -> Vec<Value> {
        self.delivered.lock().unwrap().clone()
    }

    /// Wait (15 s) until at least `n` payloads arrived.
    pub async fn wait_for(&self, n: usize) -> Vec<Value> {
        for _ in 0..300 {
            let got = self.received();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("expected {n} cleanup message(s), got {:?}", self.received());
    }

    /// Give the outbox time to deliver, then assert exactly `n` payloads arrived.
    pub async fn assert_count_stays(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(self.received().len(), n, "{:?}", self.received());
    }
}

struct RecordingCleanup(Arc<CleanupLog>);

#[async_trait]
impl LeasedMessageHandler for RecordingCleanup {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: Value = serde_json::from_slice(&msg.payload)
            .unwrap_or_else(|e| panic!("cleanup payload must be JSON: {e}"));
        self.0.delivered.lock().unwrap().push(payload);
        MessageResult::Ok
    }
}

/// Who serves a cleanup queue of the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    /// The real handler (it calls the fake provider storage and updates the rows).
    Real,
    /// A stand-in that records the payload and acknowledges it.
    Recording,
    /// The queue is not registered: every enqueue fails (`QueueNotRegistered`).
    Missing,
}

// ── options ──────────────────────────────────────────────────────────────────

/// Test timings of the indexing wait: the 25 s request deadline, the 20 s rounds and the 10 min
/// limit of DESIGN "File Upload" shrunk so a test needs a fraction of a second.
pub fn fast_timings() -> IndexingTimings {
    IndexingTimings {
        request_deadline: Duration::from_millis(400),
        first_poll_delay: Duration::from_millis(10),
        max_poll_delay: Duration::from_millis(40),
        background_round: Duration::from_millis(250),
        background_max: Duration::from_millis(2_000),
        background_max_poll_delay: Duration::from_millis(40),
    }
}

pub struct AttachmentOptions {
    pub models: Vec<Value>,
    pub pdp: Arc<FakePdp>,
    /// Merged into the gear configuration.
    pub config: Value,
    pub limits: LimitsSpec,
    pub timings: IndexingTimings,
    pub attachment_cleanup: Cleanup,
    pub chat_cleanup: Cleanup,
}

impl Default for AttachmentOptions {
    fn default() -> Self {
        Self {
            models: quota_models(),
            pdp: FakePdp::allow(),
            config: json!({}),
            limits: LimitsSpec::generous(),
            timings: fast_timings(),
            attachment_cleanup: Cleanup::Real,
            chat_cleanup: Cleanup::Real,
        }
    }
}

// ── seeds ────────────────────────────────────────────────────────────────────

/// An `attachments` row to insert directly (every column the lifecycle reads).
#[derive(Clone, Debug)]
pub struct AttRow {
    pub id: Uuid,
    pub tenant: Uuid,
    pub chat: Uuid,
    pub user: Uuid,
    pub filename: String,
    pub content_type: String,
    pub kind: String,
    pub status: String,
    pub provider_file_id: Option<String>,
    pub size_bytes: i64,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub error_code: Option<String>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
    pub secondary_file_id: Option<String>,
    pub storage_backend: String,
}

impl AttRow {
    fn base(chat: Uuid, filename: &str, content_type: &str, kind: &str) -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            tenant: TENANT_A,
            chat,
            user: USER_A,
            filename: filename.to_owned(),
            content_type: content_type.to_owned(),
            kind: kind.to_owned(),
            status: "ready".to_owned(),
            provider_file_id: Some(format!("file-seed{}", id.simple())),
            size_bytes: 1234,
            for_file_search: false,
            for_code_interpreter: false,
            error_code: None,
            cleanup_status: None,
            cleanup_attempts: 0,
            created_at: ts(0),
            updated_at: ts(0),
            deleted_at: None,
            secondary_file_id: None,
            storage_backend: "openai".to_owned(),
        }
    }

    /// A ready PDF document routed to `file_search`.
    pub fn document(chat: Uuid, filename: &str) -> Self {
        Self {
            for_file_search: true,
            ..Self::base(chat, filename, PDF, "document")
        }
    }

    /// A ready XLSX routed to the code interpreter only.
    pub fn xlsx(chat: Uuid, filename: &str) -> Self {
        Self {
            for_code_interpreter: true,
            ..Self::base(chat, filename, XLSX, "document")
        }
    }

    /// A ready PNG image.
    pub fn image(chat: Uuid, filename: &str) -> Self {
        Self::base(chat, filename, "image/png", "image")
    }

    pub fn id(mut self, n: u128) -> Self {
        self.id = uid(n);
        self.provider_file_id = Some(format!("file-seed{}", self.id.simple()));
        self
    }

    pub fn status(mut self, status: &str) -> Self {
        status.clone_into(&mut self.status);
        self
    }

    pub fn provider_file(mut self, id: Option<&str>) -> Self {
        self.provider_file_id = id.map(str::to_owned);
        self
    }

    pub fn size(mut self, bytes: i64) -> Self {
        self.size_bytes = bytes;
        self
    }

    pub fn user(mut self, user: Uuid) -> Self {
        self.user = user;
        self
    }

    pub fn tenant(mut self, tenant: Uuid) -> Self {
        self.tenant = tenant;
        self
    }

    pub fn cleanup(mut self, status: &str, attempts: i32) -> Self {
        self.cleanup_status = Some(status.to_owned());
        self.cleanup_attempts = attempts;
        self
    }

    pub fn error(mut self, code: &str) -> Self {
        self.error_code = Some(code.to_owned());
        self
    }

    /// `updated_at` (and `created_at`) at `when`.
    pub fn touched(mut self, when: OffsetDateTime) -> Self {
        self.created_at = when;
        self.updated_at = when;
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted_at = Some(ts(1));
        self
    }

    pub fn secondary(mut self, file: &str) -> Self {
        self.secondary_file_id = Some(file.to_owned());
        self
    }

    pub fn backend(mut self, label: &str) -> Self {
        label.clone_into(&mut self.storage_backend);
        self
    }
}

// ── the harness ──────────────────────────────────────────────────────────────

pub struct AttachmentHarness {
    /// The S2 harness: database, seeds and row readers (its own routes are not used).
    pub chat: ChatHarness,
    /// Chat, message, stream, turn, turn-mutation and attachment routes.
    pub router: Router,
    pub openapi: Arc<OpenApiRegistryImpl>,
    /// The scripted chat provider (Responses SSE): answers and recorded model requests.
    pub oagw: Arc<FakeOagw>,
    /// The scripted provider storage (Files API, Vector Store API).
    pub storage: Arc<StorageOagw>,
    pub policy: Arc<QuotaPolicy>,
    pub flaky: Arc<FlakyPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub config: Arc<MiniChatConfig>,
    pub pdp: Arc<FakePdp>,
    pub attachments: Arc<AttachmentService>,
    pub reaper: Arc<UploadReaper>,
    /// Payloads of the attachment-cleanup queue when it runs [`Cleanup::Recording`].
    pub attachment_log: Arc<CleanupLog>,
    /// Payloads of the chat-cleanup queue when it runs [`Cleanup::Recording`].
    pub chat_log: Arc<CleanupLog>,
    /// A handler instance for direct `handle` calls (same dependencies as the queue's).
    pub attachment_handler: AttachmentCleanupHandler,
    pub chat_handler: ChatCleanupHandler,
    pub cleanup_port: Arc<AttachmentCleanupOutbox>,
    outbox: OutboxHandle,
}

impl AttachmentHarness {
    pub async fn new() -> Self {
        Self::with(AttachmentOptions::default()).await
    }

    pub async fn with_config(config: Value) -> Self {
        Self::with(AttachmentOptions {
            config,
            ..Default::default()
        })
        .await
    }

    pub async fn with_models(models: Vec<Value>) -> Self {
        Self::with(AttachmentOptions {
            models,
            ..Default::default()
        })
        .await
    }

    pub async fn with(opts: AttachmentOptions) -> Self {
        let chat = crate::chat_support::chat_harness_with(ChatHarnessOptions {
            models: model_entries(&opts.models),
            pdp: opts.pdp.clone(),
            register_cleanup_queue: false,
            ..Default::default()
        })
        .await;
        let oagw = FakeOagw::new();
        let storage = StorageOagw::new();
        let policy = QuotaPolicy::new(&opts.models, 1, opts.limits);
        let flaky = FlakyPolicy::new(policy.clone());
        let audit = Arc::new(RecordingAudit::default());
        let mut cfg = json!({"orphan_watchdog": {"enabled": false}});
        merge(&mut cfg, &opts.config);
        let config = stream_config(&cfg);

        let gateway = Arc::new(ModelPolicyGateway::from_client(flaky.clone()));
        let audit_plugin: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        let audit_gateway = audit_gateway_for(&config.vendor, Some(audit_plugin));
        let partitions = u16::try_from(config.outbox.num_partitions).expect("partitions");

        let storage_client: Arc<dyn ServiceGatewayClientV1> = storage.clone();
        let provider_storage = Arc::new(ProviderStorage::new(storage_client.clone(), &config));
        let attachment_log = Arc::new(CleanupLog::default());
        let chat_log = Arc::new(CleanupLog::default());

        // The S2 harness already runs an outbox on this database; SQLite answers `database is
        // locked` to a registration that races its workers, so starting ours is retried.
        let mut attempts = 0;
        let outbox = loop {
            let mut builder = Outbox::builder(chat.db.db())
                .profile(OutboxProfile::low_latency())
                .queue(&config.outbox.queue_name, Partitions::of(partitions))
                .leased(UsageOutboxHandler::new(gateway.clone()))
                .done()
                .queue(&config.outbox.audit_queue_name, Partitions::of(partitions))
                .leased(AuditOutboxHandler::new(audit_gateway.clone()))
                .done();
            builder = match opts.attachment_cleanup {
                Cleanup::Real => builder
                    .queue(
                        &config.outbox.cleanup_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(AttachmentCleanupHandler::new(
                        chat.db.clone(),
                        provider_storage.clone(),
                        config.clone(),
                    ))
                    .done(),
                Cleanup::Recording => builder
                    .queue(
                        &config.outbox.cleanup_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(RecordingCleanup(attachment_log.clone()))
                    .done(),
                Cleanup::Missing => builder,
            };
            builder = match opts.chat_cleanup {
                Cleanup::Real => builder
                    .queue(
                        &config.outbox.chat_cleanup_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(ChatCleanupHandler::new(
                        chat.db.clone(),
                        provider_storage.clone(),
                        config.clone(),
                    ))
                    .done(),
                Cleanup::Recording => builder
                    .queue(
                        &config.outbox.chat_cleanup_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(RecordingCleanup(chat_log.clone()))
                    .done(),
                Cleanup::Missing => builder,
            };
            match builder.start().await {
                Ok(outbox) => break outbox,
                Err(error) if attempts < 40 => {
                    attempts += 1;
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("S6 outbox starts: {error:?}"),
            }
        };

        let usage_port = Arc::new(UsageOutbox::new(
            outbox.outbox().clone(),
            config.outbox.queue_name.clone(),
            partitions,
        ));
        let audit_port = Arc::new(AuditOutbox::new(
            outbox.outbox().clone(),
            config.outbox.audit_queue_name.clone(),
            partitions,
        ));
        let chat_cleanup_port = Arc::new(ChatCleanupOutbox::new(
            outbox.outbox().clone(),
            config.outbox.chat_cleanup_queue_name.clone(),
            partitions,
        ));
        let cleanup_port = Arc::new(AttachmentCleanupOutbox::new(
            outbox.outbox().clone(),
            config.outbox.cleanup_queue_name.clone(),
            partitions,
        ));

        let enforcer = opts.pdp.enforcer();
        let llm_client: Arc<dyn ServiceGatewayClientV1> = oagw.clone();
        let llm = Arc::new(LlmProviders::new(llm_client, &config));
        let stream = Arc::new(
            StreamService::new(
                chat.db.clone(),
                gateway.clone(),
                llm,
                config.clone(),
                enforcer.clone(),
            )
            .with_outbox(usage_port, audit_port.clone()),
        );
        let turns = Arc::new(TurnsService::new(chat.db.clone(), enforcer.clone()));
        let mutations = Arc::new(
            TurnMutationService::new(chat.db.clone(), stream.clone(), enforcer.clone())
                .with_audit(audit_port),
        );
        let chats = Arc::new(ChatsService::new(
            chat.db.clone(),
            gateway.clone(),
            chat_cleanup_port,
            enforcer.clone(),
        ));
        let messages = Arc::new(MessagesService::new(chat.db.clone(), enforcer.clone()));
        let reactions = Arc::new(ReactionsService::new(chat.db.clone(), enforcer.clone()));
        let attachments = Arc::new(
            AttachmentService::new(
                chat.db.clone(),
                gateway.clone(),
                provider_storage.clone(),
                cleanup_port.clone(),
                config.clone(),
                enforcer,
            )
            .with_timings(opts.timings),
        );
        let reaper = Arc::new(
            UploadReaper::new(chat.db.clone(), config.clone()).with_cleanup(cleanup_port.clone()),
        );

        let openapi = Arc::new(OpenApiRegistryImpl::new());
        let prefix = chat.prefix.clone();
        let router = register_chat_routes(
            Router::new(),
            openapi.as_ref(),
            chats,
            messages,
            reactions,
            &prefix,
        );
        let router = register_stream_routes(router, openapi.as_ref(), stream, turns, &prefix);
        let router = register_turn_mutation_routes(router, openapi.as_ref(), mutations, &prefix);
        let router =
            register_attachment_routes(router, openapi.as_ref(), attachments.clone(), &prefix);

        let attachment_handler = AttachmentCleanupHandler::new(
            chat.db.clone(),
            provider_storage.clone(),
            config.clone(),
        );
        let chat_handler =
            ChatCleanupHandler::new(chat.db.clone(), provider_storage.clone(), config.clone());
        Self {
            chat,
            router,
            openapi,
            oagw,
            storage,
            policy,
            flaky,
            audit,
            config,
            pdp: opts.pdp,
            attachments,
            reaper,
            attachment_log,
            chat_log,
            attachment_handler,
            chat_handler,
            cleanup_port,
            outbox,
        }
    }

    /// A reaper over the same database and attachment-cleanup queue with its own configuration.
    pub fn reaper_with(&self, config: Value) -> Arc<UploadReaper> {
        let mut cfg = json!({"orphan_watchdog": {"enabled": false}});
        merge(&mut cfg, &config);
        Arc::new(
            UploadReaper::new(self.chat.db.clone(), stream_config(&cfg))
                .with_cleanup(self.cleanup_port.clone()),
        )
    }

    // ── URIs ─────────────────────────────────────────────────────────────────

    pub fn prefix(&self) -> &str {
        &self.chat.prefix
    }

    pub fn attachments_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/attachments", self.chat.prefix)
    }

    pub fn attachment_uri(&self, chat: Uuid, attachment: Uuid) -> String {
        format!("{}/{attachment}", self.attachments_uri(chat))
    }

    pub fn stream_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages:stream", self.chat.prefix)
    }

    pub fn turn_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/v1/chats/{chat}/turns/{request_id}", self.chat.prefix)
    }

    // ── chats ────────────────────────────────────────────────────────────────

    /// A chat of `USER_A` on `gpt-premium`.
    pub async fn seed_chat(&self) -> Uuid {
        self.seed_chat_on(PREMIUM).await
    }

    pub async fn seed_chat_on(&self, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(model).at(0))
            .await
    }

    pub async fn seed_chat_of(&self, user: Uuid, tenant: Uuid, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::new(tenant, user).with_model(model).at(0))
            .await
    }

    /// A soft-deleted chat of `USER_A`.
    pub async fn seed_deleted_chat(&self) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(PREMIUM).at(0).deleted())
            .await
    }

    // ── uploads ──────────────────────────────────────────────────────────────

    async fn send_bytes(
        &self,
        method: &str,
        uri: &str,
        ctx: Option<&SecurityContext>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Resp {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let mut req = builder.body(AxumBody::from(body)).expect("request");
        if let Some(ctx) = ctx {
            req.extensions_mut().insert(ctx.clone());
        }
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), 16 * 1024 * 1024)
            .await
            .expect("body");
        Resp {
            status,
            headers,
            raw: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// `POST /v1/chats/{chat}/attachments` as `ctx` with this `Content-Type` header and body.
    pub async fn upload_raw(
        &self,
        chat: Uuid,
        ctx: &SecurityContext,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Resp {
        self.send_bytes(
            "POST",
            &self.attachments_uri(chat),
            Some(ctx),
            content_type,
            body,
        )
        .await
    }

    /// An upload whose body is a well-formed multipart of `parts`.
    pub async fn upload_parts(&self, chat: Uuid, ctx: &SecurityContext, parts: &[Part]) -> Resp {
        let body = multipart_body(BOUNDARY, parts);
        self.upload_raw(chat, ctx, Some(&multipart_type()), body)
            .await
    }

    /// Upload `data` as `filename` (`content_type`) as `USER_A`.
    pub async fn upload(
        &self,
        chat: Uuid,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> Resp {
        self.upload_as(&ctx_a(), chat, filename, content_type, data)
            .await
    }

    pub async fn upload_as(
        &self,
        ctx: &SecurityContext,
        chat: Uuid,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> Resp {
        self.upload_parts(
            chat,
            ctx,
            &[Part::file(Some(filename), Some(content_type), data)],
        )
        .await
    }

    /// Upload and assert `201`; returns the response body.
    pub async fn uploaded(
        &self,
        chat: Uuid,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> Value {
        let resp = self.upload(chat, filename, content_type, data).await;
        assert_eq!(resp.status.as_u16(), 201, "upload failed: {}", resp.raw);
        resp.json()
    }

    /// Upload a ready document; returns its id.
    pub async fn doc(&self, chat: Uuid, filename: &str) -> Uuid {
        let body = self.uploaded(chat, filename, PDF, PDF_BYTES).await;
        assert_eq!(body["status"], json!("ready"), "{body}");
        id_of(&body)
    }

    /// Upload a ready XLSX (code interpreter); returns its id.
    pub async fn xlsx(&self, chat: Uuid, filename: &str) -> Uuid {
        let body = self
            .uploaded(chat, filename, XLSX, b"PK\x03\x04 xlsx")
            .await;
        assert_eq!(body["status"], json!("ready"), "{body}");
        id_of(&body)
    }

    /// Upload a ready PNG image; returns its id.
    pub async fn image(&self, chat: Uuid, filename: &str) -> Uuid {
        let body = self
            .uploaded(chat, filename, "image/png", &png(40, 20))
            .await;
        assert_eq!(body["status"], json!("ready"), "{body}");
        id_of(&body)
    }

    pub async fn get_attachment(
        &self,
        chat: Uuid,
        attachment: Uuid,
        ctx: &SecurityContext,
    ) -> Resp {
        self.send_bytes(
            "GET",
            &self.attachment_uri(chat, attachment),
            Some(ctx),
            None,
            vec![],
        )
        .await
    }

    /// `GET` as `USER_A`.
    pub async fn get(&self, chat: Uuid, attachment: Uuid) -> Resp {
        self.get_attachment(chat, attachment, &ctx_a()).await
    }

    pub async fn delete_attachment(
        &self,
        chat: Uuid,
        attachment: Uuid,
        ctx: &SecurityContext,
    ) -> Resp {
        self.send_bytes(
            "DELETE",
            &self.attachment_uri(chat, attachment),
            Some(ctx),
            None,
            vec![],
        )
        .await
    }

    /// `DELETE` as `USER_A`.
    pub async fn delete(&self, chat: Uuid, attachment: Uuid) -> Resp {
        self.delete_attachment(chat, attachment, &ctx_a()).await
    }

    /// Any request to the harness router (paths, methods and bodies the helpers do not cover).
    pub async fn raw(
        &self,
        method: &str,
        uri: &str,
        ctx: Option<&SecurityContext>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Resp {
        self.send_bytes(method, uri, ctx, content_type, body).await
    }

    /// `DELETE /v1/chats/{id}` as `USER_A`.
    pub async fn delete_chat(&self, chat: Uuid) -> Resp {
        self.send_bytes(
            "DELETE",
            &format!("{}/v1/chats/{chat}", self.chat.prefix),
            Some(&ctx_a()),
            None,
            vec![],
        )
        .await
    }

    pub async fn list_messages(&self, chat: Uuid) -> Vec<Value> {
        let resp = self
            .send_bytes(
                "GET",
                &format!("{}/v1/chats/{chat}/messages", self.chat.prefix),
                Some(&ctx_a()),
                None,
                vec![],
            )
            .await;
        assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
        page_items(&resp.json())
    }

    // ── streaming ────────────────────────────────────────────────────────────

    async fn open_json(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<&Value>,
    ) -> RawStream {
        let mut builder = Request::builder().method(method).uri(uri);
        let payload = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                AxumBody::from(body.to_string())
            }
            None => AxumBody::empty(),
        };
        let mut req = builder.body(payload).expect("request");
        req.extensions_mut().insert(ctx.clone());
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        RawStream::from_response(resp)
    }

    /// `POST messages:stream` with a JSON body; the body is not read.
    pub async fn open(&self, chat: Uuid, ctx: &SecurityContext, body: &Value) -> RawStream {
        self.open_json("POST", &self.stream_uri(chat), ctx, Some(body))
            .await
    }

    pub async fn send(&self, chat: Uuid, body: &Value) -> Sent {
        self.open(chat, &ctx_a(), body).await.collect().await
    }

    pub async fn say(&self, chat: Uuid, text: &str) -> Sent {
        self.send(chat, &json!({"content": text})).await
    }

    /// A message that references `attachments`.
    pub async fn say_with(&self, chat: Uuid, text: &str, attachments: &[Uuid]) -> Sent {
        self.send(
            chat,
            &json!({"content": text, "attachment_ids": attachments}),
        )
        .await
    }

    /// Send as `USER_A` and expect a JSON rejection (no SSE stream).
    pub async fn reject(&self, chat: Uuid, body: Value) -> Resp {
        let raw = self.open(chat, &ctx_a(), &body).await;
        assert!(
            !raw.is_sse(),
            "expected a JSON rejection, got an SSE stream ({})",
            raw.content_type()
        );
        raw.into_resp().await
    }

    /// A rejected message that references `attachments`.
    pub async fn reject_with(&self, chat: Uuid, text: &str, attachments: &[Uuid]) -> Resp {
        self.reject(
            chat,
            json!({"content": text, "attachment_ids": attachments}),
        )
        .await
    }

    /// `POST /turns/{request_id}/retry` as `USER_A`, the whole SSE answer.
    pub async fn retry(&self, chat: Uuid, request_id: Uuid) -> Sent {
        self.open_json(
            "POST",
            &format!("{}/retry", self.turn_uri(chat, request_id)),
            &ctx_a(),
            None,
        )
        .await
        .collect()
        .await
    }

    pub async fn reject_retry(&self, chat: Uuid, request_id: Uuid) -> Resp {
        let raw = self
            .open_json(
                "POST",
                &format!("{}/retry", self.turn_uri(chat, request_id)),
                &ctx_a(),
                None,
            )
            .await;
        assert!(!raw.is_sse(), "expected a JSON rejection of the retry");
        raw.into_resp().await
    }

    /// `PATCH /turns/{request_id}` with `{"content": text}`, the whole SSE answer.
    pub async fn edit(&self, chat: Uuid, request_id: Uuid, text: &str) -> Sent {
        self.open_json(
            "PATCH",
            &self.turn_uri(chat, request_id),
            &ctx_a(),
            Some(&json!({"content": text})),
        )
        .await
        .collect()
        .await
    }

    pub async fn reject_edit(&self, chat: Uuid, request_id: Uuid, text: &str) -> Resp {
        let raw = self
            .open_json(
                "PATCH",
                &self.turn_uri(chat, request_id),
                &ctx_a(),
                Some(&json!({"content": text})),
            )
            .await;
        assert!(!raw.is_sse(), "expected a JSON rejection of the edit");
        raw.into_resp().await
    }

    // ── reaper and handlers ──────────────────────────────────────────────────

    /// One reaper scan with the application clock `now`.
    pub async fn sweep(&self, now: OffsetDateTime) -> usize {
        self.reaper.sweep_at(now).await.expect("reaper sweep")
    }

    /// Deliver `payload` to the real attachment-cleanup handler as the `attempts`-th retry
    /// (`0` = first delivery).
    pub async fn deliver_attachment_cleanup(
        &self,
        payload: &Value,
        attempts: i16,
    ) -> MessageResult {
        self.attachment_handler
            .handle(&outbox_message(
                ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
                payload.to_string().into_bytes(),
                attempts,
            ))
            .await
    }

    pub async fn deliver_attachment_cleanup_raw(&self, raw: &[u8], attempts: i16) -> MessageResult {
        self.attachment_handler
            .handle(&outbox_message(
                ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
                raw.to_vec(),
                attempts,
            ))
            .await
    }

    /// Deliver a chat-cleanup payload to the real chat-cleanup handler.
    pub async fn deliver_chat_cleanup(&self, payload: &Value, attempts: i16) -> MessageResult {
        self.chat_handler
            .handle(&outbox_message(
                CHAT_CLEANUP_PAYLOAD_TYPE,
                payload.to_string().into_bytes(),
                attempts,
            ))
            .await
    }

    pub async fn deliver_chat_cleanup_raw(&self, raw: &[u8], attempts: i16) -> MessageResult {
        self.chat_handler
            .handle(&outbox_message(
                CHAT_CLEANUP_PAYLOAD_TYPE,
                raw.to_vec(),
                attempts,
            ))
            .await
    }

    /// An `attachment_deleted` payload of a seeded row (DESIGN "Attachment Deletion", Phase 1).
    pub async fn attachment_cleanup_payload(&self, attachment: Uuid, event_type: &str) -> Value {
        let row = self.chat.attachment_row(attachment).await;
        json!({
            "event_type": event_type,
            "tenant_id": row.tenant_id,
            "chat_id": row.chat_id,
            "attachment_id": row.id,
            "provider_file_id": row.provider_file_id,
            "vector_store_id": null,
            "storage_backend": row.storage_backend,
            "attachment_kind": row.attachment_kind,
            "deleted_at": "2025-01-01T00:00:01Z",
            "secondary_ref": row.secondary_file_id.as_ref().map(|file| json!({
                "file_id": file,
                "provider_kind": "anthropic",
                "upstream_alias": PROVIDER_ALIAS,
            })),
        })
    }

    /// Dead-lettered messages of `queue`.
    pub async fn dead_letters(&self, queue: &str) -> u64 {
        let conn = self.chat.db.conn().expect("db connection");
        self.outbox
            .outbox()
            .dead_letter_count(
                &conn,
                &DeadLetterFilter::from_scope(DeadLetterScope::default().queue(queue)).any_status(),
            )
            .await
            .expect("dead letter count")
    }

    // ── seeds ────────────────────────────────────────────────────────────────

    pub async fn seed(&self, row: AttRow) -> Uuid {
        let conn = self.chat.db.conn().expect("db connection");
        let thumbnail =
            (row.kind == "image").then(|| crate::chat_support::THUMBNAIL_BYTES.to_vec());
        secure_insert::<attachments::Entity>(
            attachments::ActiveModel {
                id: Set(row.id),
                tenant_id: Set(row.tenant),
                chat_id: Set(row.chat),
                uploaded_by_user_id: Set(row.user),
                filename: Set(row.filename),
                content_type: Set(Some(row.content_type)),
                size_bytes: Set(Some(row.size_bytes)),
                storage_backend: Set(row.storage_backend),
                provider_file_id: Set(row.provider_file_id),
                status: Set(row.status),
                error_code: Set(row.error_code),
                attachment_kind: Set(row.kind),
                for_file_search: Set(row.for_file_search),
                for_code_interpreter: Set(row.for_code_interpreter),
                doc_summary: Set(None),
                img_thumbnail_width: Set(thumbnail.as_ref().map(|_| 64)),
                img_thumbnail_height: Set(thumbnail.as_ref().map(|_| 48)),
                img_thumbnail: Set(thumbnail),
                summary_model: Set(None),
                summary_updated_at: Set(None),
                cleanup_status: Set(row.cleanup_status),
                cleanup_attempts: Set(row.cleanup_attempts),
                last_cleanup_error: Set(None),
                cleanup_updated_at: Set(None),
                created_at: Set(to_db(row.created_at)),
                updated_at: Set(to_db(row.updated_at)),
                deleted_at: Set(to_db_opt(row.deleted_at)),
                secondary_file_id: Set(row.secondary_file_id.clone()),
                secondary_status: Set(if row.secondary_file_id.is_some() {
                    "uploaded".to_owned()
                } else {
                    "not_attempted".to_owned()
                }),
                secondary_provider_kind: Set(row.secondary_file_id.map(|_| "anthropic".to_owned())),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed attachment");
        row.id
    }

    /// The `chat_vector_stores` row of `chat` (`provider` is the storage label).
    pub async fn seed_store(&self, chat: Uuid, vector_store_id: &str, provider: &str) {
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<chat_vector_stores::Entity>(
            chat_vector_stores::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                vector_store_id: Set(Some(vector_store_id.to_owned())),
                provider: Set(provider.to_owned()),
                file_count: Set(0),
                created_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed vector store");
    }

    /// Link `attachment` to a message of `chat` that was submitted at `secs` (a user message
    /// is inserted).
    pub async fn submit_with(&self, chat: Uuid, attachment: Uuid) -> Uuid {
        let message = self
            .chat
            .seed_message(MessageSeed::user(chat).request(Some(Uuid::new_v4())).at(5))
            .await;
        self.chat.link_attachment(chat, message, attachment).await;
        message
    }

    // ── readers (unscoped: they assert what the API did) ─────────────────────

    pub async fn row(&self, attachment: Uuid) -> attachments::Model {
        self.chat.attachment_row(attachment).await
    }

    pub async fn rows_of(&self, chat: Uuid) -> Vec<attachments::Model> {
        let mut rows: Vec<_> = self
            .chat
            .all_attachments()
            .await
            .into_iter()
            .filter(|a| a.chat_id == chat)
            .collect();
        rows.sort_by_key(|a| (a.created_at, a.id));
        rows
    }

    /// Rows of `chat` that are neither failed nor soft-deleted.
    pub async fn live_rows(&self, chat: Uuid) -> Vec<attachments::Model> {
        self.rows_of(chat)
            .await
            .into_iter()
            .filter(|a| a.status != "failed" && a.deleted_at.is_none())
            .collect()
    }

    pub async fn provider_file_of(&self, attachment: Uuid) -> String {
        self.row(attachment)
            .await
            .provider_file_id
            .expect("the attachment has a provider file")
    }

    pub async fn stores_of(&self, chat: Uuid) -> Vec<chat_vector_stores::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read vector stores")
    }

    pub async fn all_stores(&self) -> Vec<chat_vector_stores::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_vector_stores::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read vector stores")
    }

    pub async fn vector_store_of(&self, chat: Uuid) -> String {
        self.stores_of(chat)
            .await
            .into_iter()
            .next()
            .and_then(|s| s.vector_store_id)
            .expect("the chat has a vector store")
    }

    pub async fn links(&self) -> Vec<message_attachments::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read links")
    }

    pub async fn links_of_chat(&self, chat: Uuid) -> Vec<message_attachments::Model> {
        self.links()
            .await
            .into_iter()
            .filter(|l| l.chat_id == chat)
            .collect()
    }

    pub async fn turns(&self) -> Vec<chat_turns::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
    }

    pub async fn turn_of(&self, request_id: Uuid) -> Option<chat_turns::Model> {
        self.turns()
            .await
            .into_iter()
            .find(|t| t.request_id == request_id)
    }

    pub async fn messages_in(&self, chat: Uuid) -> Vec<messages::Model> {
        let mut rows: Vec<_> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .filter(|m| m.chat_id == chat)
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows
    }

    /// The user message of the turn `request_id` (`Sent::message_id` is the assistant message).
    pub async fn user_message_of(&self, chat: Uuid, request_id: Uuid) -> Uuid {
        self.messages_in(chat)
            .await
            .into_iter()
            .find(|m| m.request_id == Some(request_id) && m.role == "user")
            .map(|m| m.id)
            .expect("the turn has a user message")
    }

    /// `(turns, messages, links, quota rows)` of the whole database: a rejected request must not
    /// change them.
    pub async fn row_counts(&self) -> (usize, usize, usize, usize) {
        let conn = self.chat.db.conn().expect("db connection");
        let usage = quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage");
        (
            self.turns().await.len(),
            self.chat.all_messages().await.len(),
            self.links().await.len(),
            usage.len(),
        )
    }

    pub async fn usage_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage")
    }

    /// Insert one `quota_usage` row, or - when a turn already created the row of this unique key -
    /// add the seeded amounts on top of it (the S4 `QuotaHarness::seed_usage`).
    pub async fn seed_usage(&self, seed: UsageSeed) {
        use quota_usage::Column;
        let conn = self.chat.db.conn().expect("db connection");
        let model = quota_usage::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(seed.tenant),
            user_id: Set(seed.user),
            period_type: Set(seed.period.to_owned()),
            period_start: Set(if seed.stale {
                crate::quota_support::previous_period_start(seed.period)
            } else {
                period_start_today(seed.period)
            }),
            bucket: Set(seed.bucket.to_owned()),
            spent_credits_micro: Set(seed.spent),
            reserved_credits_micro: Set(seed.reserved),
            calls: Set(seed.calls),
            input_tokens: Set(0),
            output_tokens: Set(0),
            file_search_calls: Set(0),
            web_search_calls: Set(seed.web_search_calls),
            code_interpreter_calls: Set(seed.code_interpreter_calls),
            rag_retrieval_calls: Set(0),
            image_inputs: Set(0),
            image_upload_bytes: Set(0),
            updated_at: Set(None),
        };
        let table = quota_usage::Entity;
        let add = |column: Column, by: i64| {
            sea_orm::sea_query::ExprTrait::add(sea_orm::sea_query::Expr::col((table, column)), by)
        };
        let on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
            Column::TenantId,
            Column::UserId,
            Column::PeriodType,
            Column::PeriodStart,
            Column::Bucket,
        ])
        .value(
            Column::SpentCreditsMicro,
            add(Column::SpentCreditsMicro, seed.spent),
        )
        .and_then(|c| {
            c.value(
                Column::ReservedCreditsMicro,
                add(Column::ReservedCreditsMicro, seed.reserved),
            )
        })
        .and_then(|c| c.value(Column::Calls, add(Column::Calls, i64::from(seed.calls))))
        .and_then(|c| {
            c.value(
                Column::WebSearchCalls,
                add(Column::WebSearchCalls, i64::from(seed.web_search_calls)),
            )
        })
        .and_then(|c| {
            c.value(
                Column::CodeInterpreterCalls,
                add(
                    Column::CodeInterpreterCalls,
                    i64::from(seed.code_interpreter_calls),
                ),
            )
        })
        .expect("seed on-conflict clause");
        quota_usage::Entity::insert(model)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .expect("seed scope")
            .on_conflict(on_conflict)
            .exec(&conn)
            .await
            .expect("seed quota_usage");
    }

    /// Seed `calls` code interpreter calls on today's daily `total` bucket of `USER_A`.
    pub async fn seed_code_interpreter_calls(&self, calls: i32) {
        self.seed_usage(UsageSeed::total("daily").code_interpreter_calls(calls))
            .await;
    }

    /// `(spent, reserved, calls, code_interpreter_calls)` of a bucket of `USER_A` today.
    pub async fn bucket(&self, bucket: &str, period: &str) -> Option<quota_usage::Model> {
        self.usage_rows().await.into_iter().find(|r| {
            r.user_id == USER_A
                && r.bucket == bucket
                && r.period_type == period
                && r.period_start == period_start_today(period)
        })
    }

    /// Wait (10 s) until the row satisfies `check`.
    pub async fn wait_row<F: Fn(&attachments::Model) -> bool>(
        &self,
        attachment: Uuid,
        what: &str,
        check: F,
    ) -> attachments::Model {
        for _ in 0..1000 {
            let row = self.row(attachment).await;
            if check(&row) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let row = self.row(attachment).await;
        panic!(
            "timed out waiting for {what}: status={} error={:?} cleanup={:?} attempts={}",
            row.status, row.error_code, row.cleanup_status, row.cleanup_attempts
        );
    }

    pub async fn wait_status(&self, attachment: Uuid, status: &str) -> attachments::Model {
        self.wait_row(attachment, &format!("status {status}"), |r| {
            r.status == status
        })
        .await
    }

    pub async fn wait_cleanup(&self, attachment: Uuid, status: &str) -> attachments::Model {
        self.wait_row(attachment, &format!("cleanup_status {status}"), |r| {
            r.cleanup_status.as_deref() == Some(status)
        })
        .await
    }

    /// Stop the outbox workers (the queued messages stay queued).
    pub async fn stop_outbox(self) {
        self.outbox.stop().await;
    }
}

/// The payload the chat-delete transaction writes (what the real chat-cleanup queue carries).
pub fn chat_cleanup_payload(chat: Uuid, tenant: Uuid) -> Value {
    json!({
        "tenant_id": tenant,
        "chat_id": chat,
        "system_request_id": Uuid::new_v4(),
        "reason": "chat_soft_delete",
        "chat_deleted_at": "2025-01-01T00:00:01Z",
    })
}

/// `id` of an attachment body.
pub fn id_of(body: &Value) -> Uuid {
    Uuid::parse_str(body["id"].as_str().expect("an id")).expect("a UUID")
}

/// An outbox message as the shared outbox hands it to a leased handler.
pub fn outbox_message(payload_type: &str, payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: payload_type.to_owned(),
        created_at: DateTimeUtc::default(),
        attempts,
    }
}

// ── provider request views ───────────────────────────────────────────────────

/// `input` items of a recorded Responses request whose role is `user`, last one first.
pub fn last_user_content(request: &crate::stream_support::RecordedProviderRequest) -> Vec<Value> {
    request.body["input"]
        .as_array()
        .and_then(|items| items.iter().rev().find(|i| i["role"] == json!("user")))
        .and_then(|item| item["content"].as_array().cloned())
        .unwrap_or_default()
}

/// The `input_image` file ids of the last user item, in order.
pub fn image_file_ids(request: &crate::stream_support::RecordedProviderRequest) -> Vec<String> {
    last_user_content(request)
        .iter()
        .filter(|part| part["type"] == json!("input_image"))
        .filter_map(|part| part["file_id"].as_str().map(str::to_owned))
        .collect()
}

/// Every `input_image` part of the whole request (any item).
pub fn all_image_parts(request: &crate::stream_support::RecordedProviderRequest) -> Vec<Value> {
    request.body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == json!("input_image"))
        .cloned()
        .collect()
}

/// Assert a 400 `out_of_range` / `invalid_argument` problem whose field violation names `field`
/// and `reason` and whose `context.resource_type` is the attachment type.
pub fn assert_attachment_violation(
    resp: &Resp,
    category: &str,
    field: &str,
    reason: &str,
) -> Value {
    let problem = assert_problem(resp, 400, category);
    assert_field_violation(&problem, field, reason);
    assert_eq!(
        problem["context"]["resource_type"],
        json!("gts.cf.core.mini_chat.attachment.v1~"),
        "the problem names the attachment type: {}",
        resp.raw
    );
    problem
}

/// `Retry-After` header as text.
pub fn retry_after(resp: &Resp) -> Option<String> {
    resp.headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// A 503 with the generic body and `Retry-After`.
pub fn assert_unavailable(resp: &Resp, retry_after_secs: &str) -> Value {
    let problem = assert_problem(resp, 503, "service_unavailable");
    assert_eq!(
        retry_after(resp).as_deref(),
        Some(retry_after_secs),
        "{}",
        resp.raw
    );
    assert_eq!(
        problem["context"]["retry_after_seconds"],
        json!(retry_after_secs.parse::<u64>().unwrap()),
        "{}",
        resp.raw
    );
    problem
}
