#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::format_push_string,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::struct_excessive_bools,
    clippy::field_reassign_with_default,
    clippy::let_underscore_must_use
)]
//! Fixture of the attachments tests (`cpt-cf-mini-chat-feature-attachments`):
//! the streaming fixture (migrated `SQLite`, running outbox, fake PDP, fake
//! policy, fake OAGW, the whole chat and stream router) extended with the
//! attachment store, the OAGW storage client, the attachments service and its
//! three routes, a hooked store wrapper, a multipart builder and parser, the
//! scripted OpenAI-compatible provider, failure injection through `SQLite`
//! triggers and raw-SQL readers.
//!
//! The services are built in [`AttEnv::with`] only; a change of a constructor
//! is made there.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures::future::BoxFuture;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_attachment_routes;
use crate::chat_test_env::{Reply, blob};
use crate::config::{MiniChatConfig, RagConfig, ThumbnailConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::ChatRepository;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::attachments::model::{
    AttachmentKind, ChatLimits, CleanupRow, CleanupState, DeleteOutcome, FailOutcome,
    FailWithCleanup, FailureCode, NewAttachment, ReadyOutcome, SecondaryUpdate, SoftDelete,
    StaleUpload, StoreRow, StoredAttachment, Thumbnail,
};
use crate::domain::service::attachments::payload::{CleanupEvent, CleanupMessage};
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::attachments::service::{
    AttachmentSettings, AttachmentsDeps, AttachmentsService,
};
use crate::domain::service::attachments::test_support::{RecordingAttachmentMetrics, TestRuntime};
use crate::domain::service::quota::service::Clock;
use crate::domain::service::quota::test_support::{NO_SWITCHES, model, with_switches};
use crate::infra::db::repos::attachments::SeaAttachmentRepository;
use crate::infra::db::repos::chats::SeaChatRepository;
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::outbox::attachment_cleanup::{AttachmentCleanupHandler, CleanupHandlerDeps};
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};
use crate::infra::storage::oagw_storage::OagwStorageClient;
use crate::infra::workers::leader::LeaderElector;
use crate::infra::workers::upload_reaper::{ReaperDeps, UploadReaper};
use crate::stream_test_env::{Options as StreamOptions, StreamEnv, base_time};
use crate::test_doubles::{
    FakeOagw, PdpReply, ProxyReply, RecordedProxy, USER, USER_TENANT, provider_json, registry,
    user_ctx,
};

/// Default `url_prefix`.
pub const PREFIX: &str = "/mini-chat";
/// Alias of the OpenAI-compatible upstream.
pub const ALIAS: &str = crate::stream_test_env::ALIAS;
/// Alias of the Azure upstream.
pub const AZURE_ALIAS: &str = "az.test";
/// `api-version` of the Azure entry.
pub const AZURE_API_VERSION: &str = "2025-01-01-preview";
/// Alias of the Anthropic upstream.
pub const ANTHROPIC_ALIAS: &str = "api.anthropic.test";
/// Queue of the attachment cleanup messages.
pub const CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";
/// `context.resource_type` of an attachment.
pub const ATTACHMENT_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
/// Boundary of the multipart bodies the fixture builds.
pub const BOUNDARY: &str = "XBOUNDARYX";

// ================================================================================================
// Multipart
// ================================================================================================

/// One part of a multipart body.
#[derive(Clone)]
pub struct PartSpec {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// A text field.
pub fn text_part(name: &str, value: &str) -> PartSpec {
    PartSpec {
        name: name.to_owned(),
        filename: None,
        content_type: None,
        body: value.as_bytes().to_vec(),
    }
}

/// The part named `file`.
pub fn file_part(filename: &str, content_type: &str, body: &[u8]) -> PartSpec {
    PartSpec {
        name: "file".to_owned(),
        filename: Some(filename.to_owned()),
        content_type: Some(content_type.to_owned()),
        body: body.to_vec(),
    }
}

/// `multipart/form-data; boundary=...`
pub fn multipart_content_type(boundary: &str) -> String {
    format!("multipart/form-data; boundary={boundary}")
}

/// Serializes `parts` with a closing boundary.
pub fn multipart_body(boundary: &str, parts: &[PartSpec]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = &part.filename {
            disposition.push_str(&format!("; filename=\"{filename}\""));
        }
        out.extend_from_slice(disposition.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(content_type) = &part.content_type {
            out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&part.body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// A part read back from a multipart body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn quoted_param(header: &str, key: &str) -> Option<String> {
    let marker = format!("{key}=\"");
    let start = header.find(&marker)? + marker.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

/// Parses a `multipart/form-data` body of a recorded request.
pub fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<ParsedPart> {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .unwrap_or_else(|| panic!("no boundary in `{content_type}`"))
        .trim_matches('"')
        .to_owned();
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut at = find(body, delimiter.as_bytes(), 0).expect("an opening boundary");
    loop {
        at += delimiter.len();
        if body[at..].starts_with(b"--") {
            break;
        }
        assert!(body[at..].starts_with(b"\r\n"), "a CRLF after the boundary");
        at += 2;
        let headers_end = find(body, b"\r\n\r\n", at).expect("the end of the part headers");
        let headers = String::from_utf8_lossy(&body[at..headers_end]).into_owned();
        let content_start = headers_end + 4;
        let next = find(body, format!("\r\n{delimiter}").as_bytes(), content_start)
            .expect("the next boundary");
        let mut part = ParsedPart {
            name: String::new(),
            filename: None,
            content_type: None,
            body: body[content_start..next].to_vec(),
        };
        for line in headers.split("\r\n") {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                part.name = quoted_param(line, "name").unwrap_or_default();
                part.filename = quoted_param(line, "filename");
            } else if lower.starts_with("content-type:") {
                part.content_type = Some(line["content-type:".len()..].trim().to_owned());
            }
        }
        parts.push(part);
        at = next + 2;
    }
    parts
}

/// The multipart parts of a recorded request.
pub fn parts_of(call: &RecordedProxy) -> Vec<ParsedPart> {
    parse_multipart(
        call.header("content-type").expect("a content type"),
        &call.body,
    )
}

// ================================================================================================
// Request bodies the tests can observe
// ================================================================================================

/// A body that counts how often it is polled; `chunks` are delivered, then EOF.
pub fn counting_body(chunks: Vec<Vec<u8>>) -> (Body, Arc<AtomicUsize>) {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polls);
    let mut chunks: std::collections::VecDeque<Vec<u8>> = chunks.into();
    let stream = futures::stream::poll_fn(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        std::task::Poll::Ready(
            chunks
                .pop_front()
                .map(|c| Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(c))),
        )
    });
    (Body::from_stream(stream), polls)
}

/// A request body the test feeds by hand; `idle` counts the times the reader
/// found it empty and had to wait.
pub struct HeldBody {
    pub body: Option<Body>,
    pub tx: tokio::sync::mpsc::UnboundedSender<Result<bytes::Bytes, std::io::Error>>,
    pub idle: Arc<AtomicUsize>,
}

struct HeldStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, std::io::Error>>,
    idle: Arc<AtomicUsize>,
}

impl futures::Stream for HeldStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        if polled.is_pending() {
            self.idle.fetch_add(1, Ordering::SeqCst);
        }
        polled
    }
}

/// A hand-fed body.
pub fn held_body() -> HeldBody {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let idle = Arc::new(AtomicUsize::new(0));
    HeldBody {
        body: Some(Body::from_stream(HeldStream {
            rx,
            idle: Arc::clone(&idle),
        })),
        tx,
        idle,
    }
}

impl HeldBody {
    /// Sends a chunk.
    pub fn send(&self, bytes: &[u8]) {
        let _ = self.tx.send(Ok(bytes::Bytes::copy_from_slice(bytes)));
    }

    /// The body to hand to the request (once).
    pub fn take(&mut self) -> Body {
        self.body.take().expect("the body is taken once")
    }
}

/// The opening of a multipart request up to (not including) the end of the file
/// content: the headers of the `file` part and `content`; no closing boundary.
pub fn multipart_head(filename: &str, content_type: &str, content: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    out.extend_from_slice(content);
    out
}

/// The closing of a multipart body that [`multipart_head`] opened.
pub fn multipart_tail() -> Vec<u8> {
    format!("\r\n--{BOUNDARY}--\r\n").into_bytes()
}

// ================================================================================================
// Images
// ================================================================================================

/// A PNG of `width` x `height` with a gradient.
pub fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

// ================================================================================================
// The hooked store
// ================================================================================================

type Hook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// An [`AttachmentRepository`] that delegates to another and runs a one-shot
/// hook after a named method returned (to interleave a concurrent writer).
pub struct HookedRepo {
    inner: Arc<dyn AttachmentRepository>,
    hooks: Mutex<HashMap<&'static str, Vec<Hook>>>,
    before_hooks: Mutex<HashMap<&'static str, Vec<Hook>>>,
}

impl HookedRepo {
    pub fn new(inner: Arc<dyn AttachmentRepository>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hooks: Mutex::new(HashMap::new()),
            before_hooks: Mutex::new(HashMap::new()),
        })
    }

    /// Runs `hook` once, right before the next call of `method` is delegated.
    pub fn before(
        &self,
        method: &'static str,
        hook: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) {
        self.before_hooks
            .lock()
            .unwrap()
            .entry(method)
            .or_default()
            .push(Arc::new(hook));
    }

    async fn run_before(&self, method: &'static str) {
        let hook = {
            let mut hooks = self.before_hooks.lock().unwrap();
            hooks
                .get_mut(method)
                .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        };
        if let Some(hook) = hook {
            hook().await;
        }
    }

    /// Runs `hook` once, right after the next call of `method` returned.
    pub fn after(
        &self,
        method: &'static str,
        hook: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) {
        self.hooks
            .lock()
            .unwrap()
            .entry(method)
            .or_default()
            .push(Arc::new(hook));
    }

    async fn run(&self, method: &'static str) {
        let hook = {
            let mut hooks = self.hooks.lock().unwrap();
            hooks
                .get_mut(method)
                .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        };
        if let Some(hook) = hook {
            hook().await;
        }
    }
}

macro_rules! hooked {
    ($self:ident, $name:literal, $call:expr) => {{
        $self.run_before($name).await;
        let out = $call.await;
        $self.run($name).await;
        out
    }};
}

#[async_trait]
impl AttachmentRepository for HookedRepo {
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError> {
        hooked!(self, "find", self.inner.find(scope, chat_id, attachment_id))
    }

    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError> {
        hooked!(
            self,
            "vector_store",
            self.inner.vector_store(tenant_id, chat_id)
        )
    }

    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<StoredAttachment, DomainError> {
        hooked!(
            self,
            "insert_pending",
            self.inner.insert_pending(new, limits)
        )
    }

    async fn set_uploaded(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        hooked!(
            self,
            "set_uploaded",
            self.inner
                .set_uploaded(tenant_id, id, provider_file_id, now)
        )
    }

    async fn set_ready(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<ReadyOutcome, DomainError> {
        hooked!(
            self,
            "set_ready",
            self.inner.set_ready(tenant_id, id, thumbnail, now)
        )
    }

    async fn set_failed(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        code: FailureCode,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        hooked!(
            self,
            "set_failed",
            self.inner.set_failed(tenant_id, id, code, now)
        )
    }

    async fn fail_with_cleanup(
        &self,
        request: &FailWithCleanup,
    ) -> Result<FailOutcome, DomainError> {
        hooked!(
            self,
            "fail_with_cleanup",
            self.inner.fail_with_cleanup(request)
        )
    }

    async fn enqueue_orphan_file(&self, message: &CleanupMessage) -> Result<(), DomainError> {
        hooked!(
            self,
            "enqueue_orphan_file",
            self.inner.enqueue_orphan_file(message)
        )
    }

    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        hooked!(
            self,
            "set_secondary",
            self.inner.set_secondary(tenant_id, id, update, now)
        )
    }

    async fn insert_store_placeholder(&self, row: &StoreRow) -> Result<(), DomainError> {
        hooked!(
            self,
            "insert_store_placeholder",
            self.inner.insert_store_placeholder(row)
        )
    }

    async fn bind_store(
        &self,
        tenant_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        hooked!(
            self,
            "bind_store",
            self.inner.bind_store(tenant_id, row_id, vector_store_id)
        )
    }

    async fn delete_placeholder(&self, tenant_id: Uuid, row_id: Uuid) -> Result<bool, DomainError> {
        hooked!(
            self,
            "delete_placeholder",
            self.inner.delete_placeholder(tenant_id, row_id)
        )
    }

    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        hooked!(self, "heartbeat", self.inner.heartbeat(tenant_id, id, now))
    }

    async fn soft_delete(
        &self,
        scope: &AccessScope,
        request: &SoftDelete,
    ) -> Result<DeleteOutcome, DomainError> {
        hooked!(self, "soft_delete", self.inner.soft_delete(scope, request))
    }

    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        hooked!(
            self,
            "stale_uploads",
            self.inner.stale_uploads(cutoff, limit)
        )
    }

    async fn abandon(
        &self,
        stale: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        hooked!(self, "abandon", self.inner.abandon(stale, cutoff, now))
    }

    async fn chat_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        hooked!(
            self,
            "chat_deleted",
            self.inner.chat_deleted(tenant_id, chat_id)
        )
    }

    async fn cleanup_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        hooked!(
            self,
            "cleanup_row",
            self.inner.cleanup_row(tenant_id, chat_id, id)
        )
    }

    async fn record_cleanup_attempt(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError> {
        hooked!(
            self,
            "record_cleanup_attempt",
            self.inner.record_cleanup_attempt(tenant_id, id, error, now)
        )
    }

    async fn finish_cleanup(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        state: CleanupState,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        hooked!(
            self,
            "finish_cleanup",
            self.inner.finish_cleanup(tenant_id, id, state, now)
        )
    }
}

// ================================================================================================
// The fixture
// ================================================================================================

/// A handler whose target is set after the pipeline started (the real handler
/// needs the enqueuer the pipeline binds).
#[derive(Default)]
pub struct LazyHandler(OnceLock<Arc<dyn LeasedMessageHandler>>);

#[async_trait]
impl LeasedMessageHandler for LazyHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        match self.0.get() {
            Some(inner) => inner.handle(msg).await,
            None => MessageResult::Retry,
        }
    }
}

/// Fixture options.
#[derive(Clone)]
pub struct AttOptions {
    /// Answer of the fake PDP.
    pub pdp: PdpReply,
    /// File-backed WAL database with a pool (real concurrency).
    pub file_db: bool,
    /// `rag.*`.
    pub rag: RagConfig,
    /// `thumbnail.*`.
    pub thumbnail: ThumbnailConfig,
    /// Kill switches of the snapshot.
    pub switches: KillSwitches,
    /// `cleanup_worker.max_attempts`.
    pub max_cleanup_attempts: u32,
    /// The real cleanup handler is registered on the queue (messages are
    /// consumed); otherwise they stay in the outbox tables.
    pub real_cleanup: bool,
    /// The repository enqueues to a queue that is not registered, so every
    /// enqueue fails after the database writes of its transaction.
    pub enqueue_fails: bool,
    /// The runtime is bound (the gear started: tasks can be spawned and the S2S
    /// context exists).
    pub runtime_bound: bool,
}

impl Default for AttOptions {
    fn default() -> Self {
        Self {
            pdp: PdpReply::AllowScoped,
            file_db: false,
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            switches: NO_SWITCHES,
            max_cleanup_attempts: 3,
            real_cleanup: false,
            enqueue_fails: false,
            runtime_bound: true,
        }
    }
}

/// The columns of an attachment row the tests read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttSnap {
    pub status: String,
    pub error_code: Option<String>,
    pub provider_file_id: Option<String>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i64,
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<String>,
    pub deleted_at: Option<String>,
    pub updated_at: String,
    pub created_at: String,
    pub content_type: String,
    pub filename: String,
    pub size_bytes: i64,
    pub kind: String,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub storage_backend: String,
    pub uploaded_by: Uuid,
    pub tenant: Uuid,
    pub chat: Uuid,
    pub secondary_status: String,
    pub secondary_file_id: Option<String>,
    pub secondary_provider_kind: Option<String>,
    pub thumbnail: Option<Vec<u8>>,
    pub thumbnail_width: Option<i64>,
    pub thumbnail_height: Option<i64>,
    pub doc_summary: Option<String>,
    pub summary_model: Option<String>,
}

/// The fixture.
pub struct AttEnv {
    pub stream: StreamEnv,
    pub oagw: Arc<FakeOagw>,
    pub sea: Arc<SeaAttachmentRepository>,
    pub repo: Arc<HookedRepo>,
    pub storage: Arc<OagwStorageClient>,
    pub directory: Arc<ProviderResolver>,
    pub metrics: Arc<RecordingAttachmentMetrics>,
    pub runtime: Arc<TestRuntime>,
    pub service: Arc<AttachmentsService>,
    pub router: Router,
    pub openapi: OpenApiRegistryImpl,
    pub config: MiniChatConfig,
    pub options: AttOptions,
    clock: Arc<Mutex<OffsetDateTime>>,
    next_chat: AtomicU32,
}

/// The catalog of the fixture next to the quota models `p` and `s`.
pub fn att_catalog() -> Vec<ModelCatalogEntry> {
    let mut catalog = crate::domain::service::quota::test_support::design_catalog();
    catalog.push(
        model("m-oa", ModelTier::Standard)
            .provider("openai")
            .build(),
    );
    let mut small = model("m-small", ModelTier::Standard)
        .provider("openai")
        .build();
    small.general_config.max_file_size_mb = 1;
    catalog.push(small);
    let mut uncapped = model("m-uncapped", ModelTier::Standard)
        .provider("openai")
        .build();
    uncapped.general_config.max_file_size_mb = 0;
    catalog.push(uncapped);
    catalog.push(
        model("m-noci", ModelTier::Standard)
            .provider("openai")
            .tools(true, true, false)
            .build(),
    );
    catalog.push(
        model("m-nov", ModelTier::Standard)
            .provider("openai")
            .vision(false)
            .build(),
    );
    catalog.push(model("m-az", ModelTier::Standard).provider("azure").build());
    catalog.push(
        model("m-an", ModelTier::Standard)
            .provider("anthropic")
            .build(),
    );
    catalog.push(
        model("m-off", ModelTier::Standard)
            .provider("openai")
            .enabled(false)
            .build(),
    );
    catalog
}

/// The provider entries of the fixture (`openai`, `azure`, `anthropic`).
pub fn att_directory() -> Arc<ProviderResolver> {
    let mut entries = serde_json::Map::new();
    entries.insert("openai".to_owned(), provider_json(ALIAS, "openai"));
    for (id, entry) in provider_entries() {
        entries.insert(id.to_owned(), entry);
    }
    Arc::new(ProviderResolver::new(&registry(&Value::Object(entries))))
}

fn provider_entries() -> Vec<(&'static str, Value)> {
    let mut anthropic = provider_json(ANTHROPIC_ALIAS, "openai");
    anthropic["kind"] = json!("anthropic_messages");
    anthropic["api_path"] = json!("/v1/messages");
    anthropic["rag_provider"] = json!("openai");
    vec![
        ("azure", provider_json(AZURE_ALIAS, "azure")),
        ("anthropic", anthropic),
    ]
}

impl AttEnv {
    /// Defaults.
    pub async fn new() -> Self {
        Self::with(AttOptions::default()).await
    }

    /// Builds the fixture.
    pub async fn with(options: AttOptions) -> Self {
        let lazy = Arc::new(LazyHandler::default());
        let stream = StreamEnv::with(StreamOptions {
            pdp: options.pdp,
            file_db: options.file_db,
            catalog: att_catalog(),
            switches: options.switches,
            providers: provider_entries(),
            attachment_cleanup: options
                .real_cleanup
                .then(|| Arc::clone(&lazy) as Arc<dyn LeasedMessageHandler>),
            ..StreamOptions::default()
        })
        .await;
        let oagw = Arc::clone(&stream.oagw);
        let mut config = MiniChatConfig::default();
        config.rag = options.rag.clone();
        config.thumbnail = options.thumbnail.clone();
        config.cleanup_worker.max_attempts = options.max_cleanup_attempts;

        let mut entries = serde_json::Map::new();
        entries.insert("openai".to_owned(), provider_json(ALIAS, "openai"));
        for (id, entry) in provider_entries() {
            entries.insert(id.to_owned(), entry);
        }
        let directory = Arc::new(ProviderResolver::new(&registry(&Value::Object(entries))));

        let quota = &stream.quota;
        let repo_enqueuer = if options.enqueue_fails {
            let outbox = crate::config::OutboxConfig {
                cleanup_queue_name: "mini-chat.not_registered".to_owned(),
                num_partitions: 4,
                ..crate::config::OutboxConfig::default()
            };
            let mismatched = Arc::new(OutboxEnqueuer::new(&outbox));
            assert!(mismatched.bind(&quota.enqueuer.outbox().expect("the outbox runs")));
            mismatched
        } else {
            Arc::clone(&quota.enqueuer)
        };
        let sea = Arc::new(SeaAttachmentRepository::new(
            Arc::clone(&quota.provider),
            repo_enqueuer,
        ));
        let repo = HookedRepo::new(Arc::clone(&sea) as Arc<dyn AttachmentRepository>);
        let storage = Arc::new(OagwStorageClient::new(
            Arc::clone(&oagw) as Arc<dyn oagw_sdk::ServiceGatewayClientV1>
        ));
        let metrics = RecordingAttachmentMetrics::new();
        let runtime = if options.runtime_bound {
            TestRuntime::bound()
        } else {
            TestRuntime::unbound()
        };
        let clock = Arc::new(Mutex::new(base_time()));
        let shared = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || *shared.lock().unwrap());

        let authz = Arc::new(AuthzService::new(
            Arc::clone(&quota.pdp) as Arc<dyn AuthZResolverApi>
        ));
        let chats: Arc<dyn ChatRepository> = Arc::new(SeaChatRepository::new(
            Arc::clone(&quota.provider),
            Arc::clone(&quota.enqueuer),
        ));
        let service = Arc::new(AttachmentsService::new(AttachmentsDeps {
            authz,
            chats,
            policy: Arc::clone(&quota.policy) as Arc<dyn PolicyCatalog>,
            directory: Arc::clone(&directory) as Arc<dyn StorageDirectory>,
            repo: Arc::clone(&repo) as Arc<dyn AttachmentRepository>,
            storage: Arc::clone(&storage) as Arc<dyn StorageClient>,
            metrics: Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>,
            runtime: Arc::clone(&runtime) as Arc<dyn BackgroundRuntime>,
            settings: AttachmentSettings::from_config(&config),
            clock: Arc::clone(&clock_fn),
        }));
        let openapi = OpenApiRegistryImpl::new();
        let router = register_attachment_routes(
            stream.router.clone(),
            &openapi,
            Arc::clone(&service),
            PREFIX,
        );
        let this = Self {
            stream,
            oagw,
            sea,
            repo,
            storage,
            directory,
            metrics,
            runtime,
            service,
            router,
            openapi,
            config,
            options,
            clock,
            next_chat: AtomicU32::new(0),
        };
        if this.options.real_cleanup {
            let handler: Arc<dyn LeasedMessageHandler> = Arc::new(this.cleanup_handler());
            assert!(lazy.0.set(handler).is_ok());
        }
        this
    }

    // ---- time -----------------------------------------------------------------------------

    /// The clock of the service.
    pub fn now(&self) -> OffsetDateTime {
        *self.clock.lock().unwrap()
    }

    pub fn advance(&self, secs: i64) {
        *self.clock.lock().unwrap() += time::Duration::seconds(secs);
    }

    /// The service clock as a closure.
    pub fn clock_fn(&self) -> Clock {
        let shared = Arc::clone(&self.clock);
        Arc::new(move || *shared.lock().unwrap())
    }

    /// The text `SQLite` stores for `at`.
    pub fn stamp(at: OffsetDateTime) -> String {
        at.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    // ---- handlers and workers ----------------------------------------------------------------

    /// A cleanup handler over the fixture's store, storage and runtime.
    pub fn cleanup_handler(&self) -> AttachmentCleanupHandler {
        self.cleanup_handler_timeout(Duration::from_secs(10))
    }

    /// A cleanup handler with its own `PROVIDER_DELETE_TIMEOUT`.
    pub fn cleanup_handler_timeout(&self, delete_timeout: Duration) -> AttachmentCleanupHandler {
        AttachmentCleanupHandler::new(CleanupHandlerDeps {
            repo: Arc::clone(&self.repo) as Arc<dyn AttachmentRepository>,
            storage: Arc::clone(&self.storage) as Arc<dyn StorageClient>,
            directory: Arc::clone(&self.directory) as Arc<dyn StorageDirectory>,
            metrics: Arc::clone(&self.metrics) as Arc<dyn AttachmentMetrics>,
            runtime: Arc::clone(&self.runtime) as Arc<dyn BackgroundRuntime>,
            max_attempts: self.options.max_cleanup_attempts,
            clock: self.clock_fn(),
            delete_timeout,
        })
    }

    /// A cleanup handler over an unbound runtime (the gear has not started).
    pub fn cleanup_handler_with_runtime(
        &self,
        runtime: Arc<dyn BackgroundRuntime>,
    ) -> AttachmentCleanupHandler {
        AttachmentCleanupHandler::new(CleanupHandlerDeps {
            repo: Arc::clone(&self.repo) as Arc<dyn AttachmentRepository>,
            storage: Arc::clone(&self.storage) as Arc<dyn StorageClient>,
            directory: Arc::clone(&self.directory) as Arc<dyn StorageDirectory>,
            metrics: Arc::clone(&self.metrics) as Arc<dyn AttachmentMetrics>,
            runtime,
            max_attempts: self.options.max_cleanup_attempts,
            clock: self.clock_fn(),
            delete_timeout: Duration::from_secs(10),
        })
    }

    /// A reaper over the fixture's store.
    pub fn reaper(&self, elector: Arc<dyn LeaderElector>) -> UploadReaper {
        UploadReaper::new(ReaperDeps {
            repo: Arc::clone(&self.repo) as Arc<dyn AttachmentRepository>,
            elector,
            metrics: Arc::clone(&self.metrics) as Arc<dyn AttachmentMetrics>,
            config: self.config.upload_reaper.clone(),
            clock: self.clock_fn(),
        })
    }

    // ---- requests -------------------------------------------------------------------------

    pub fn upload_uri(chat: Uuid) -> String {
        format!("{PREFIX}/v1/chats/{chat}/attachments")
    }

    pub fn attachment_uri(chat: Uuid, id: Uuid) -> String {
        format!("{PREFIX}/v1/chats/{chat}/attachments/{id}")
    }

    /// Sends a request through the router.
    pub async fn send(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        content_type: Option<&str>,
        body: Body,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 64 << 20).await.unwrap();
        Reply {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// Uploads one file as the default user.
    pub async fn upload(&self, chat: Uuid, name: &str, content_type: &str, bytes: &[u8]) -> Reply {
        self.upload_as(&user_ctx(), chat, name, content_type, bytes)
            .await
    }

    pub async fn upload_as(
        &self,
        ctx: &SecurityContext,
        chat: Uuid,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Reply {
        let body = multipart_body(BOUNDARY, &[file_part(name, content_type, bytes)]);
        self.upload_raw(
            ctx,
            chat,
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await
    }

    /// Uploads an arbitrary body.
    pub async fn upload_raw(
        &self,
        ctx: &SecurityContext,
        chat: Uuid,
        content_type: Option<&str>,
        body: Body,
    ) -> Reply {
        self.send("POST", &Self::upload_uri(chat), ctx, content_type, body)
            .await
    }

    pub async fn get_attachment(&self, chat: Uuid, id: Uuid) -> Reply {
        self.get_attachment_as(&user_ctx(), chat, id).await
    }

    pub async fn get_attachment_as(&self, ctx: &SecurityContext, chat: Uuid, id: Uuid) -> Reply {
        self.send(
            "GET",
            &Self::attachment_uri(chat, id),
            ctx,
            None,
            Body::empty(),
        )
        .await
    }

    pub async fn delete_attachment(&self, chat: Uuid, id: Uuid) -> Reply {
        self.delete_attachment_as(&user_ctx(), chat, id).await
    }

    pub async fn delete_attachment_as(&self, ctx: &SecurityContext, chat: Uuid, id: Uuid) -> Reply {
        self.send(
            "DELETE",
            &Self::attachment_uri(chat, id),
            ctx,
            None,
            Body::empty(),
        )
        .await
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// A new chat of the default user on `model`.
    pub async fn chat(&self, model: &str) -> Uuid {
        let n = u128::from(self.next_chat.fetch_add(1, Ordering::SeqCst)) + 100;
        self.stream.seed_chat(n, model).await
    }

    /// A new chat of `(tenant, user)` on `model`.
    pub async fn chat_of(&self, tenant: Uuid, user: Uuid, model: &str) -> Uuid {
        let n = u128::from(self.next_chat.fetch_add(1, Ordering::SeqCst)) + 100;
        self.stream.seed_chat_of(n, tenant, user, model).await
    }

    /// Soft-deletes the chat through the REST route (marks its attachments for
    /// cleanup like the product does).
    pub async fn delete_chat(&self, chat: Uuid) {
        let reply = self
            .send(
                "DELETE",
                &format!("{PREFIX}/v1/chats/{chat}"),
                &user_ctx(),
                None,
                Body::empty(),
            )
            .await;
        assert_eq!(reply.status.as_u16(), 204, "{}", reply.text);
    }

    /// A seeded attachment of the default user (`ready` PDF document with
    /// provider file `file-prov{n}`); `edit` adjusts the raw row.
    pub async fn seed(
        &self,
        chat: Uuid,
        n: u128,
        edit: impl FnOnce(crate::infra::db::test_support::Row) -> crate::infra::db::test_support::Row,
    ) -> Uuid {
        self.stream
            .seed_attachment(chat, n, |row| edit(row.set("storage_backend", "openai")))
            .await
    }

    /// Switches the kill switches of the policy snapshot.
    pub fn set_switches(&self, switches: KillSwitches) {
        let mut snapshots = self.stream.quota.policy.snapshots.lock().unwrap();
        let snapshot = snapshots.get(&7).cloned().unwrap();
        snapshots.insert(7, with_switches(snapshot, switches));
    }

    /// Links an attachment to a (new) message of the chat.
    pub async fn link(&self, chat: Uuid, attachment: Uuid, message_n: u128) {
        let message = self
            .stream
            .seed_message(chat, message_n, "user", "hello", 1)
            .await;
        self.stream
            .quota
            .db
            .insert(
                "message_attachments",
                crate::infra::db::test_support::Row::default()
                    .set("tenant_id", USER_TENANT)
                    .set("chat_id", chat)
                    .set("message_id", message)
                    .set("attachment_id", attachment)
                    .set("created_at", crate::chat_test_env::ts(1)),
            )
            .await
            .expect("link attachment");
    }

    /// The pending row of a new document or image of the default user.
    pub fn new_attachment(
        &self,
        chat: Uuid,
        kind: AttachmentKind,
        size_bytes: i64,
    ) -> NewAttachment {
        let image = kind == AttachmentKind::Image;
        NewAttachment {
            id: Uuid::new_v4(),
            tenant_id: USER_TENANT,
            chat_id: chat,
            user_id: USER,
            filename: if image { "a.png" } else { "a.pdf" }.to_owned(),
            content_type: if image {
                "image/png"
            } else {
                "application/pdf"
            }
            .to_owned(),
            size_bytes,
            storage_backend: "openai".to_owned(),
            kind,
            for_file_search: !image,
            for_code_interpreter: false,
            now: self.now(),
        }
    }

    /// A cleanup message of the row `id` of `chat` (default tenant, `openai`
    /// backend, document) at the fixture clock.
    pub fn cleanup_message(
        &self,
        event: CleanupEvent,
        chat: Uuid,
        id: Uuid,
        provider_file_id: Option<&str>,
    ) -> CleanupMessage {
        CleanupMessage {
            event,
            tenant_id: USER_TENANT,
            chat_id: chat,
            attachment_id: id,
            provider_file_id: provider_file_id.map(str::to_owned),
            storage_backend: "openai".to_owned(),
            kind: AttachmentKind::Document,
            deleted_at: self.now(),
            secondary_ref: None,
        }
    }

    /// The JSON the payload of `message` must have.
    pub fn payload_json(message: &CleanupMessage) -> Value {
        json!({
            "event_type": message.event.as_str(),
            "tenant_id": message.tenant_id.to_string(),
            "chat_id": message.chat_id.to_string(),
            "attachment_id": message.attachment_id.to_string(),
            "provider_file_id": message.provider_file_id,
            "vector_store_id": null,
            "storage_backend": message.storage_backend,
            "attachment_kind": message.kind.as_str(),
            "deleted_at": Self::stamp(message.deleted_at),
            "secondary_ref": message.secondary_ref.as_ref().map(|s| json!({
                "file_id": s.file_id,
                "provider_kind": "anthropic",
                "upstream_alias": s.upstream_alias,
            })),
        })
    }

    /// A store over the fixture's healthy enqueuer (the fixture's own store
    /// targets an unregistered queue under `enqueue_fails`).
    pub fn healthy_repo(&self) -> Arc<SeaAttachmentRepository> {
        Arc::new(SeaAttachmentRepository::new(
            Arc::clone(&self.stream.quota.provider),
            Arc::clone(&self.stream.quota.enqueuer),
        ))
    }

    /// The scope of the default tenant (what the PEP compiles to, without the
    /// owner predicate child rows do not carry).
    pub fn scope(&self) -> AccessScope {
        AccessScope::for_tenant(USER_TENANT)
    }

    /// Seeds a vector store row.
    pub async fn seed_store(
        &self,
        chat: Uuid,
        vector_store_id: Option<&str>,
        provider: &str,
        created_at: OffsetDateTime,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = crate::infra::db::test_support::Row::default()
            .set("id", id)
            .set("tenant_id", USER_TENANT)
            .set("chat_id", chat)
            .set("provider", provider)
            .set("file_count", 0_i32)
            .set("created_at", Self::stamp(created_at));
        row = match vector_store_id {
            Some(store) => row.set("vector_store_id", store),
            None => row.null("vector_store_id"),
        };
        self.stream
            .quota
            .db
            .insert("chat_vector_stores", row)
            .await
            .expect("seed store");
        id
    }

    // ---- readers ---------------------------------------------------------------------------

    async fn sql_rows(&self, sql: &str) -> Vec<sea_orm::QueryResult> {
        self.stream
            .quota
            .db
            .raw
            .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
    }

    /// Executes a statement on the raw connection.
    pub async fn exec(&self, sql: &str) {
        self.stream
            .quota
            .db
            .raw
            .execute_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }

    /// Ids of every attachment row, oldest first.
    pub async fn att_ids(&self) -> Vec<Uuid> {
        self.sql_rows("SELECT id FROM attachments ORDER BY created_at, id")
            .await
            .iter()
            .map(|r| Uuid::from_slice(&r.try_get::<Vec<u8>>("", "id").unwrap()).unwrap())
            .collect()
    }

    pub async fn att_count(&self) -> i64 {
        self.stream.count("attachments").await
    }

    /// The only attachment row.
    pub async fn only_att(&self) -> (Uuid, AttSnap) {
        let ids = self.att_ids().await;
        assert_eq!(ids.len(), 1, "exactly one attachment row: {ids:?}");
        (ids[0], self.att(ids[0]).await)
    }

    /// The columns of the row `id`.
    pub async fn att(&self, id: Uuid) -> AttSnap {
        let rows = self
            .sql_rows(&format!(
                "SELECT * FROM attachments WHERE id = {}",
                blob(id)
            ))
            .await;
        let row = rows
            .first()
            .unwrap_or_else(|| panic!("attachment {id} does not exist"));
        let text = |c: &str| row.try_get::<Option<String>>("", c).unwrap();
        let uuid = |c: &str| Uuid::from_slice(&row.try_get::<Vec<u8>>("", c).unwrap()).unwrap();
        AttSnap {
            status: text("status").unwrap(),
            error_code: text("error_code"),
            provider_file_id: text("provider_file_id"),
            cleanup_status: text("cleanup_status"),
            cleanup_attempts: row.try_get::<i64>("", "cleanup_attempts").unwrap(),
            last_cleanup_error: text("last_cleanup_error"),
            cleanup_updated_at: text("cleanup_updated_at"),
            deleted_at: text("deleted_at"),
            updated_at: text("updated_at").unwrap(),
            created_at: text("created_at").unwrap(),
            content_type: text("content_type").unwrap(),
            filename: text("filename").unwrap(),
            size_bytes: row.try_get::<i64>("", "size_bytes").unwrap(),
            kind: text("attachment_kind").unwrap(),
            for_file_search: row.try_get::<bool>("", "for_file_search").unwrap(),
            for_code_interpreter: row.try_get::<bool>("", "for_code_interpreter").unwrap(),
            storage_backend: text("storage_backend").unwrap(),
            uploaded_by: uuid("uploaded_by_user_id"),
            tenant: uuid("tenant_id"),
            chat: uuid("chat_id"),
            secondary_status: text("secondary_status").unwrap(),
            secondary_file_id: text("secondary_file_id"),
            secondary_provider_kind: text("secondary_provider_kind"),
            thumbnail: row.try_get::<Option<Vec<u8>>>("", "img_thumbnail").unwrap(),
            thumbnail_width: row
                .try_get::<Option<i64>>("", "img_thumbnail_width")
                .unwrap(),
            thumbnail_height: row
                .try_get::<Option<i64>>("", "img_thumbnail_height")
                .unwrap(),
            doc_summary: text("doc_summary"),
            summary_model: text("summary_model"),
        }
    }

    /// Every row and column of `attachments`, ordered, as one string.
    pub async fn attachments_dump(&self) -> String {
        self.stream.quota.dump("attachments").await
    }

    /// `chat_vector_stores` rows as `(vector_store_id, provider)`.
    pub async fn stores(&self) -> Vec<(Option<String>, String)> {
        self.sql_rows(
            "SELECT vector_store_id, provider FROM chat_vector_stores ORDER BY created_at, id",
        )
        .await
        .iter()
        .map(|r| {
            (
                r.try_get::<Option<String>>("", "vector_store_id").unwrap(),
                r.try_get::<String>("", "provider").unwrap(),
            )
        })
        .collect()
    }

    /// The cleanup messages of the outbox as JSON, oldest first.
    pub async fn cleanup_messages(&self) -> Vec<Value> {
        self.stream
            .quota
            .outbox_rows(CLEANUP_QUEUE)
            .await
            .iter()
            .map(super::chat_test_env::OutboxRow::json)
            .collect()
    }

    /// The partitions of the cleanup messages.
    pub async fn cleanup_partitions(&self) -> Vec<i64> {
        self.stream
            .quota
            .outbox_rows(CLEANUP_QUEUE)
            .await
            .iter()
            .map(|row| row.partition)
            .collect()
    }

    /// An outbox delivery of `payload`.
    pub fn message(payload: &Value, attempts: i16) -> OutboxMessage {
        crate::test_doubles::outbox_message(serde_json::to_vec(payload).unwrap(), attempts)
    }

    /// Partition of a cleanup message of `(tenant, chat)`.
    pub fn partition_of(&self, tenant: Uuid, chat: Uuid) -> u32 {
        self.stream
            .quota
            .enqueuer
            .partition(QueueKind::AttachmentCleanup, tenant, chat)
    }

    /// Polls `check` until it is true (5 s at most).
    pub async fn wait_until(&self, what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// Polls an async condition (5 s at most).
    pub async fn wait_for<F, Fut>(&self, what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..500 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    // ---- failure injection ------------------------------------------------------------------

    /// Makes every update of `attachments` rows matching `when` (a SQL
    /// expression over `NEW`) fail with a database error.
    pub async fn fail_updates(&self, name: &str, when: &str) {
        self.exec(&format!(
            "CREATE TRIGGER {name} BEFORE UPDATE ON attachments WHEN {when} \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
        ))
        .await;
    }

    /// Removes an injected failure.
    pub async fn heal(&self, name: &str) {
        self.exec(&format!("DROP TRIGGER IF EXISTS {name}")).await;
    }

    /// Makes every outbox enqueue fail inside its transaction.
    pub async fn fail_outbox(&self) {
        self.exec(
            "CREATE TRIGGER fail_outbox BEFORE INSERT ON mini_chat__outbox_body \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .await;
    }

    // ---- the scripted provider ---------------------------------------------------------------

    /// The OpenAI-compatible routes answer like a healthy provider: file upload
    /// `file-1`, `file-2`, ...; store create `vs_1`; add file and status
    /// `completed`; deletes 200. The Anthropic Files API answers `file_ant_N` and
    /// 200. Later `on_path` rules of a test override these.
    pub fn script_happy(&self) {
        script_happy_on(&self.oagw);
    }

    /// `METHOD path` of every proxied call, in order (the query is dropped).
    pub fn sequence(&self) -> Vec<String> {
        self.oagw
            .proxied()
            .iter()
            .map(|call| {
                format!(
                    "{} {}",
                    call.method,
                    call.uri.split('?').next().unwrap_or_default()
                )
            })
            .collect()
    }

    /// The recorded calls to the Files upload route of the `OpenAI` entry.
    pub fn file_uploads(&self) -> Vec<RecordedProxy> {
        self.oagw
            .proxied()
            .into_iter()
            .filter(|c| {
                c.method == "POST" && c.uri.split('?').next() == Some(&format!("/{ALIAS}/v1/files"))
            })
            .collect()
    }

    /// Calls of `method` whose path ends with `suffix`.
    pub fn calls_ending(&self, method: &str, suffix: &str) -> Vec<RecordedProxy> {
        self.oagw
            .proxied()
            .into_iter()
            .filter(|c| {
                c.method == method
                    && c.uri
                        .split('?')
                        .next()
                        .unwrap_or_default()
                        .ends_with(suffix)
            })
            .collect()
    }

    /// The DELETE calls to the provider (any route).
    pub fn deletes(&self) -> Vec<RecordedProxy> {
        self.oagw
            .proxied()
            .into_iter()
            .filter(|c| c.method == "DELETE")
            .collect()
    }
}

/// The OpenAI-compatible routes answer like a healthy provider: file upload
/// `file-1`, `file-2`, ...; store create `vs_1`; add file and status
/// `completed`; deletes 200. The Anthropic Files API answers `file_ant_N` and
/// 200. Later `on_path` rules of a test override these.
pub fn script_happy_on(oagw: &FakeOagw) {
    let files = Arc::new(AtomicUsize::new(0));
    let secondary = Arc::new(AtomicUsize::new(0));
    let stores = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&files);
    oagw.on_path("POST", &format!("/{ALIAS}/v1/files"), move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        ProxyReply::json(200, &json!({ "id": format!("file-{n}") }))
    });
    let counter = Arc::clone(&stores);
    oagw.on_path("POST", &format!("/{ALIAS}/v1/vector_stores"), move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        ProxyReply::json(200, &json!({ "id": format!("vs_{n}") }))
    });
    oagw.on_path_json(
        "POST",
        &format!("/{ALIAS}/v1/vector_stores/*/files"),
        200,
        &json!({ "status": "completed" }),
    );
    oagw.on_path_json(
        "GET",
        &format!("/{ALIAS}/v1/vector_stores/*/files/*"),
        200,
        &json!({ "status": "completed" }),
    );
    oagw.on_path_json(
        "DELETE",
        &format!("/{ALIAS}/v1/files/*"),
        200,
        &json!({ "deleted": true }),
    );
    oagw.on_path_json(
        "DELETE",
        &format!("/{ALIAS}/v1/vector_stores/*"),
        200,
        &json!({ "deleted": true }),
    );
    let counter = Arc::clone(&secondary);
    oagw.on_path("POST", &format!("/{ANTHROPIC_ALIAS}/v1/files"), move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        ProxyReply::json(200, &json!({ "id": format!("file_ant_{n}") }))
    });
    oagw.on_path_json(
        "DELETE",
        &format!("/{ANTHROPIC_ALIAS}/v1/files/*"),
        200,
        &json!({ "deleted": true }),
    );
    oagw.on_path("POST", &format!("/{AZURE_ALIAS}/openai/files"), move |_| {
        ProxyReply::json(200, &json!({ "id": "file-az-1" }))
    });
    oagw.on_path_json(
        "DELETE",
        &format!("/{AZURE_ALIAS}/openai/files/*"),
        200,
        &json!({ "deleted": true }),
    );
}

/// `User` and tenant of the default caller, re-exported for brevity.
pub fn default_ids() -> (Uuid, Uuid) {
    (USER_TENANT, USER)
}
