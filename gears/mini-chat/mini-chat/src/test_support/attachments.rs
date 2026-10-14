//! The in-process harness of the attachments feature (slice S1-upload-lifecycle): the quota rig
//! (database, PEP, outbox pipeline), the real router, the mock OAGW of the storage APIs, the
//! virtual clock and timer, the recording metrics, plus multipart request builders, image
//! fixtures, row seeding and raw inspection helpers that do not go through the code under test.
//!
//! If the author stage changes the constructors of `AttachmentsService`, `AttachmentsDeps`,
//! `OagwStorageClient` or `Services`, [`rig_with`] is the only place to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "test support"
)]

use std::fmt::Write as _;
use std::io::Cursor;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::Request;
use axum::middleware::from_fn;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot};
use sea_orm::{EntityTrait, Set};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::{OpenApiRegistryImpl, canonical_error_middleware};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use super::attach_fakes::RecordingAttachmentMetrics;
use super::chats::{OutboxRow, RecordingOpenApi, Reply, a1, outbox_rows, request};
use super::manual_time::ManualTime;
use super::provider::Gate;
use super::quota::{
    QuotaGateway, QuotaRig, QuotaRigOptions, STANDARD, UsageQueueKind, default_now,
};
use super::s2::{MetricsHarness, metrics_harness};
use super::send::{catalog_of, prem_model, provider_s2s, std_model};
use super::storage_mock::MockStorage;
use super::{Pdp, TENANT_A, TestDb, USER_A1};
use crate::api::rest::register_routes;
use crate::config::{MiniChatConfig, OutboxConfig, RagConfig, ThumbnailConfig};
use crate::domain::attachments::AttachmentMetrics;
use crate::domain::attachments::completion::UploadPermits;
use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::model::{AttachmentRecord, AttachmentStatus, Thumbnail};
use crate::domain::attachments::service::{
    AttachmentSettings, AttachmentsDeps, AttachmentsService,
};
use crate::domain::attachments::store::{
    AttachmentCleanupQueue, AttachmentStore, DeleteOutcome, DeleteRequest, NewAttachment,
    SecondaryUpdate,
};
use crate::domain::chats::{ChatCleanupQueue, ChatStore, ChatsService};
use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;
use crate::domain::services::Services;
use crate::infra::oagw::storage_client::OagwStorageClient;
use crate::infra::outbox::{OutboxEnqueuer, queue_specs};
use crate::infra::provider_task::S2sContext;
use crate::infra::storage::entity::{attachment, chat_vector_store};
use crate::infra::storage::repo_impl::ChatRecord;

/// `context.resource_type` of an attachment not-found.
pub const ATTACHMENT_RESOURCE: &str = "gts.cf.core.mini_chat.attachment.v1~";

/// The models of the attachments catalog.
/// The default model: code interpreter on, `max_file_size_mb` 25.
pub const STD: &str = STANDARD;
/// `max_file_size_mb` 1.
pub const SMALL_MODEL: &str = "small-1";
/// `tool_support.code_interpreter` off.
pub const NO_CI_MODEL: &str = "noci-1";
/// Served by the `anthropic` provider (kind `anthropic_messages`, storage on `azure_openai`).
pub const ANTHROPIC_CHAT_MODEL: &str = "anthropic-1";
/// A model whose provider is not in the registry.
pub const ORPHAN_MODEL: &str = "orphan-1";
/// Storage on an Azure provider with its own backend label.
pub const AZURE_MODEL: &str = "azure-1";

/// The upstream aliases of the providers of [`providers`].
pub const OPENAI_ALIAS: &str = "azure.example.com";
pub const ANTHROPIC_ALIAS: &str = "anthropic.example.com";
pub const AZURE_RAG_ALIAS: &str = "rag.azure.example.com";
/// The `storage_backend` labels.
pub const OPENAI_BACKEND: &str = "azure_openai";
pub const AZURE_BACKEND: &str = "azure-rag-label";

/// The `api-version` of the Azure storage provider.
pub const AZURE_API_VERSION: &str = "2024-10-21";

/// The upload and get URIs.
pub fn attachments_uri(chat_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/attachments")
}

pub fn attachment_uri(chat_id: Uuid, attachment_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/attachments/{attachment_id}")
}

// ── catalog and providers ────────────────────────────────────────────────────

/// The provider registry of the attachments rig: `azure_openai` (Responses, OpenAI storage),
/// `anthropic` (Messages, storage on `azure_openai`) and `azure_rag` (Responses, Azure storage
/// with its own label and `api-version`).
pub fn providers() -> Arc<ProviderRegistry> {
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": { "kind": "openai_responses", "host": OPENAI_ALIAS,
                               "storage_kind": "openai" },
            "anthropic": { "kind": "anthropic_messages", "host": ANTHROPIC_ALIAS,
                            "storage_kind": "openai", "api_path": "/v1/messages",
                            "rag_provider": "azure_openai" },
            "azure_rag": { "kind": "openai_responses", "host": AZURE_RAG_ALIAS,
                            "storage_kind": "azure", "api_version": AZURE_API_VERSION,
                            "storage_backend": AZURE_BACKEND }
        }
    }))
    .expect("valid config");
    Arc::new(ProviderRegistry::build(&config.providers))
}

/// `tool_support` with file search on and the given code interpreter flag.
fn tools(code_interpreter: bool) -> Value {
    json!({
        "web_search": true, "file_search": true, "image_generation": false,
        "code_interpreter": code_interpreter, "mcp": false
    })
}

fn with_general(entry: &ModelCatalogEntry, f: impl Fn(&mut Value)) -> ModelCatalogEntry {
    let mut value = serde_json::to_value(entry).expect("entry serializes");
    f(&mut value["general_config"]);
    serde_json::from_value(value).expect("entry")
}

/// The catalog of the attachments rig (no kill switch): the standard model, `small-1`
/// (`max_file_size_mb` 1), `noci-1` (no code interpreter), `anthropic-1`, `orphan-1` and `azure-1`.
pub fn catalog() -> PolicySnapshot {
    let on = |id: &str, provider: &str| {
        std_model(&json!({
            "id": id, "provider_id": provider, "provider_model_id": format!("provider-{id}"),
        }))
    };
    let std = std_model(&json!({}));
    let small = with_general(&on(SMALL_MODEL, "azure_openai"), |g| {
        g["max_file_size_mb"] = json!(1);
    });
    let noci = with_general(&on(NO_CI_MODEL, "azure_openai"), |g| {
        g["tool_support"] = tools(false);
    });
    catalog_of(vec![
        prem_model(&json!({})),
        std,
        small,
        noci,
        on(ANTHROPIC_CHAT_MODEL, "anthropic"),
        on(ORPHAN_MODEL, "ghost"),
        on(AZURE_MODEL, "azure_rag"),
    ])
}

/// The catalog with the given kill switches.
pub fn catalog_with(switches: KillSwitches) -> PolicySnapshot {
    let mut snapshot = catalog();
    snapshot.kill_switches = switches;
    snapshot
}

// ── the rig ──────────────────────────────────────────────────────────────────

/// How to build an [`AttachRig`].
pub struct AttachRigOptions {
    pub pdp: Pdp,
    pub snapshot: PolicySnapshot,
    pub rag: RagConfig,
    pub thumbnail: ThumbnailConfig,
    /// `true`: sleeps advance the virtual time at once; `false`: sleepers park.
    pub auto_time: bool,
    /// Record on the real OTLP instruments instead of the recording double.
    pub otlp: bool,
    pub storage: Arc<MockStorage>,
}

impl AttachRigOptions {
    pub fn new() -> Self {
        Self {
            pdp: Pdp::TenantOnly,
            snapshot: catalog(),
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            auto_time: true,
            otlp: false,
            storage: MockStorage::new(),
        }
    }

    pub fn with_pdp(mut self, pdp: Pdp) -> Self {
        self.pdp = pdp;
        self
    }

    pub fn with_snapshot(mut self, snapshot: PolicySnapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    pub fn with_rag(mut self, rag: RagConfig) -> Self {
        self.rag = rag;
        self
    }

    pub fn with_thumbnail(mut self, thumbnail: ThumbnailConfig) -> Self {
        self.thumbnail = thumbnail;
        self
    }

    pub fn manual_time(mut self) -> Self {
        self.auto_time = false;
        self
    }

    pub fn with_otlp(mut self) -> Self {
        self.otlp = true;
        self
    }
}

impl Default for AttachRigOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The in-process harness of the upload and get routes.
pub struct AttachRig {
    pub quota: QuotaRig,
    pub router: Router,
    pub storage: Arc<MockStorage>,
    pub time: Arc<ManualTime>,
    pub metrics: Arc<RecordingAttachmentMetrics>,
    pub otlp: Option<MetricsHarness>,
    pub permits: Arc<UploadPermits>,
    pub shutdown: CancellationToken,
    pub rag: RagConfig,
    /// Fault injection in front of the attachment store (none armed by default).
    pub faults: Arc<StoreFaults>,
}

impl Deref for AttachRig {
    type Target = QuotaRig;

    fn deref(&self) -> &QuotaRig {
        &self.quota
    }
}

/// A rig with the default catalog, the tenant-only PDP and the automatic virtual timer.
pub async fn rig() -> AttachRig {
    rig_with(AttachRigOptions::new()).await
}

/// A rig built from `options`.
pub async fn rig_with(options: AttachRigOptions) -> AttachRig {
    let time = if options.auto_time {
        ManualTime::auto(default_now())
    } else {
        ManualTime::manual(default_now())
    };
    let gateway = QuotaGateway::serving(
        options.snapshot,
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    );
    let clock: Arc<dyn Clock> = time.clone();
    let quota = super::quota::quota_rig_with(QuotaRigOptions {
        pdp: options.pdp,
        gateway,
        queue: UsageQueueKind::Real,
        clock: Arc::clone(&clock),
        ..QuotaRigOptions::new()
    })
    .await;
    let policy: Arc<dyn ModelPolicyGateway> = quota.gateway.clone();
    let otlp = options.otlp.then(|| metrics_harness("mini_chat"));
    let recording = RecordingAttachmentMetrics::new();
    let metrics: Arc<dyn AttachmentMetrics> = otlp.as_ref().map_or_else(
        || recording.clone() as Arc<dyn AttachmentMetrics>,
        |harness| harness.metrics.clone() as Arc<dyn AttachmentMetrics>,
    );
    let permits = UploadPermits::new(options.rag.max_concurrent_uploads, Arc::clone(&metrics));
    let shutdown = CancellationToken::new();
    let storage_client =
        OagwStorageClient::new(options.storage.clone(), S2sContext::bound(provider_s2s()));
    let chat_store: Arc<dyn ChatStore> = quota.db.provider.clone();
    let faults = Arc::new(StoreFaults::default());
    let store: Arc<dyn AttachmentStore> = Arc::new(FaultyStore {
        inner: quota.db.provider.clone(),
        faults: Arc::clone(&faults),
    });
    let deps = AttachmentsDeps {
        pep: Arc::clone(&quota.pep),
        policy: Arc::clone(&policy),
        chats: chat_store,
        store,
        vector_stores: quota.db.provider.clone(),
        providers: providers(),
        storage: Arc::new(storage_client),
        cleanup: quota.enqueuer.clone(),
        permits: Arc::clone(&permits),
        clock,
        timer: time.clone(),
        metrics,
        settings: AttachmentSettings {
            rag: options.rag.clone(),
            thumbnail: options.thumbnail,
        },
        shutdown: shutdown.clone(),
    };
    let cleanup: Arc<dyn ChatCleanupQueue> = Arc::new(OutboxEnqueuer::new(queue_specs(
        &OutboxConfig::default(),
        300,
    )));
    let chats = ChatsService::new(
        Arc::clone(&quota.pep),
        Arc::clone(&policy),
        quota.db.provider.clone(),
        cleanup,
    );
    let services = Arc::new(
        Services::new(Arc::clone(&quota.pep), Arc::clone(&policy))
            .with_chats(chats)
            .with_attachments(AttachmentsService::new(deps)),
    );
    let openapi = Arc::new(RecordingOpenApi {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = register_routes(Router::new(), openapi.as_ref(), "/mini-chat", services)
        .layer(from_fn(canonical_error_middleware));
    AttachRig {
        quota,
        router,
        storage: options.storage,
        time,
        metrics: recording,
        otlp,
        permits,
        shutdown,
        rag: options.rag,
        faults,
    }
}

/// The armed faults of a [`FaultyStore`]: an armed step answers an internal error instead of
/// running (nothing is committed for it).
#[derive(Default)]
pub struct StoreFaults {
    pub mark_uploaded: AtomicBool,
    pub storage_recheck: AtomicBool,
    pub mark_ready: AtomicBool,
    /// The delete transaction answers an internal error (a database failure).
    pub delete: AtomicBool,
    /// `mark_uploaded` waits for this gate before it runs.
    mark_uploaded_gate: Mutex<Option<Arc<Gate>>>,
    mark_uploaded_calls: AtomicUsize,
    reached: Notify,
}

impl StoreFaults {
    /// Hold every `mark_uploaded` until the gate opens (the provider upload has completed by then).
    pub fn gate_mark_uploaded(&self, gate: &Arc<Gate>) {
        *self.mark_uploaded_gate.lock().unwrap() = Some(Arc::clone(gate));
    }

    /// Wait (5 s of real time at most) until `mark_uploaded` was called.
    pub async fn wait_for_mark_uploaded(&self) {
        let wait = async {
            loop {
                let notified = self.reached.notified();
                if self.mark_uploaded_calls.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("mark_uploaded was never reached");
    }

    fn injected(step: &str) -> DomainError {
        DomainError::internal(format!("injected {step} failure"))
    }
}

/// The attachment store with fault injection in front of the database one.
struct FaultyStore {
    inner: Arc<dyn AttachmentStore>,
    faults: Arc<StoreFaults>,
}

#[async_trait]
impl AttachmentStore for FaultyStore {
    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<AttachmentRecord, DomainError> {
        self.inner.insert_pending(new, limits).await
    }

    async fn mark_uploaded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        provider_file_id: &str,
        size_bytes: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        self.faults
            .mark_uploaded_calls
            .fetch_add(1, Ordering::SeqCst);
        self.faults.reached.notify_waiters();
        let gate = self.faults.mark_uploaded_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        if self.faults.mark_uploaded.load(Ordering::SeqCst) {
            return Err(StoreFaults::injected("mark_uploaded"));
        }
        self.inner
            .mark_uploaded(tenant_id, attachment_id, provider_file_id, size_bytes, now)
            .await
    }

    async fn storage_recheck(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        final_bytes: i64,
        limits: &ChatLimits,
    ) -> Result<bool, DomainError> {
        if self.faults.storage_recheck.load(Ordering::SeqCst) {
            return Err(StoreFaults::injected("storage_recheck"));
        }
        self.inner
            .storage_recheck(tenant_id, chat_id, attachment_id, final_bytes, limits)
            .await
    }

    async fn mark_failed(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AttachmentStatus>, DomainError> {
        self.inner
            .mark_failed(tenant_id, attachment_id, error_code, now)
            .await
    }

    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        self.inner
            .set_secondary(tenant_id, attachment_id, update, now)
            .await
    }

    async fn mark_ready(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        if self.faults.mark_ready.load(Ordering::SeqCst) {
            return Err(StoreFaults::injected("mark_ready"));
        }
        self.inner
            .mark_ready(tenant_id, attachment_id, thumbnail, now)
            .await
    }

    async fn get(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        uploader_id: Uuid,
    ) -> Result<AttachmentRecord, DomainError> {
        self.inner
            .get(tenant_id, chat_id, attachment_id, uploader_id)
            .await
    }

    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.inner.heartbeat(tenant_id, attachment_id, now).await
    }

    async fn set_ready_guarded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.inner
            .set_ready_guarded(tenant_id, attachment_id, now)
            .await
    }

    async fn fail_indexing(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        self.inner
            .fail_indexing(tenant_id, chat_id, attachment_id, now, cleanup)
            .await
    }

    async fn delete_attachment(
        &self,
        request: &DeleteRequest,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<DeleteOutcome, DomainError> {
        if self.faults.delete.load(Ordering::SeqCst) {
            return Err(StoreFaults::injected("delete"));
        }
        self.inner.delete_attachment(request, cleanup).await
    }
}

impl AttachRig {
    /// Send one request through the router.
    pub async fn send(&self, request: Request<Body>) -> Reply {
        send_through(self.router.clone(), request).await
    }

    /// Start `request` on its own task (for concurrent uploads).
    pub fn spawn(&self, request: Request<Body>) -> JoinHandle<Reply> {
        let router = self.router.clone();
        tokio::spawn(send_through(router, request))
    }

    /// A live chat of user 1 of tenant A on `model`.
    pub async fn chat(&self, model: &str) -> ChatRecord {
        self.quota.chat_a1(model).await
    }

    /// A live chat of user 1 of tenant A on the standard model.
    pub async fn std_chat(&self) -> ChatRecord {
        self.quota.chat_a1(STD).await
    }

    /// Upload one file part as user 1 of tenant A.
    pub async fn upload(&self, chat_id: Uuid, filename: &str, mime: &str, data: &[u8]) -> Reply {
        self.upload_as(&a1(), chat_id, filename, mime, data).await
    }

    pub async fn upload_as(
        &self,
        who: &SecurityContext,
        chat_id: Uuid,
        filename: &str,
        mime: &str,
        data: &[u8],
    ) -> Reply {
        self.send(upload_request(
            chat_id,
            who,
            &[FilePart::file(filename, mime, data)],
        ))
        .await
    }

    /// `GET` the attachment as user 1 of tenant A.
    pub async fn get_attachment(&self, chat_id: Uuid, attachment_id: Uuid) -> Reply {
        self.get_attachment_as(&a1(), chat_id, attachment_id).await
    }

    pub async fn get_attachment_as(
        &self,
        who: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Reply {
        self.send(request("GET", &attachment_uri(chat_id, attachment_id), who))
            .await
    }

    /// The `attachments` rows of the chat, ordered by id.
    pub async fn rows(&self, chat_id: Uuid) -> Vec<attachment::Model> {
        super::chats::attachment_rows(&self.db, chat_id).await
    }

    /// The only `attachments` row of the chat.
    pub async fn only_row(&self, chat_id: Uuid) -> attachment::Model {
        let rows = self.rows(chat_id).await;
        assert_eq!(rows.len(), 1, "expected exactly one attachment row");
        rows.into_iter().next().unwrap()
    }

    /// The row `attachment_id`.
    pub async fn row(&self, chat_id: Uuid, attachment_id: Uuid) -> attachment::Model {
        self.rows(chat_id)
            .await
            .into_iter()
            .find(|row| row.id == attachment_id)
            .expect("attachment row")
    }

    /// The `chat_vector_stores` rows.
    pub async fn vector_stores(&self) -> Vec<chat_vector_store::Model> {
        vector_store_rows(&self.db).await
    }

    /// The messages of the attachment cleanup queue.
    pub async fn attachment_cleanup_rows(&self) -> Vec<OutboxRow> {
        attachment_cleanup_rows(&self.db, &self.outbox).await
    }

    /// Wait (5 s of real time at most) until the row has `status`.
    pub async fn wait_for_status(&self, chat_id: Uuid, attachment_id: Uuid, status: &str) {
        let wait = async {
            loop {
                if self.row(chat_id, attachment_id).await.status == status {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("the attachment never reached `{status}`"));
    }
}

async fn send_through(router: Router, request: Request<Body>) -> Reply {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let (content_type, location, retry_after) = {
        let header = |name: &str| -> Option<String> {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        (
            header("content-type").unwrap_or_default(),
            header("location"),
            header("retry-after"),
        )
    };
    let body = response.into_body();
    let bytes = to_bytes(body, 1 << 24).await.unwrap();
    Reply {
        status,
        content_type,
        location,
        retry_after,
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// The rows of `chat_vector_stores`, ordered by id.
pub async fn vector_store_rows(db: &TestDb) -> Vec<chat_vector_store::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = chat_vector_store::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| row.id);
    rows
}

/// The messages of the attachment cleanup queue of the configured name.
pub async fn attachment_cleanup_rows(db: &TestDb, outbox: &OutboxConfig) -> Vec<OutboxRow> {
    outbox_rows(db)
        .await
        .into_iter()
        .filter(|row| row.queue == outbox.cleanup_queue_name)
        .collect()
}

// ── multipart requests ───────────────────────────────────────────────────────

/// The boundary of the requests built here.
pub const BOUNDARY: &str = "XBOUNDARYX";

/// One part of a multipart body.
#[derive(Debug, Clone)]
pub struct FilePart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl FilePart {
    /// A `file` part with a filename and a content type.
    pub fn file(filename: &str, content_type: &str, data: &[u8]) -> Self {
        Self {
            name: "file".to_owned(),
            filename: Some(filename.to_owned()),
            content_type: Some(content_type.to_owned()),
            data: data.to_vec(),
        }
    }

    /// A `file` part without a filename.
    pub fn nameless(content_type: &str, data: &[u8]) -> Self {
        Self {
            filename: None,
            ..Self::file("x", content_type, data)
        }
    }

    /// A `file` part without a content type.
    pub fn untyped(filename: &str, data: &[u8]) -> Self {
        Self {
            content_type: None,
            ..Self::file(filename, "x/y", data)
        }
    }

    /// A text field of another name.
    pub fn field(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            filename: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }
}

/// The `multipart/form-data` body of `parts`.
pub fn multipart_body(parts: &[FilePart]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = &part.filename {
            write!(disposition, "; filename=\"{filename}\"").unwrap();
        }
        body.extend_from_slice(disposition.as_bytes());
        body.extend_from_slice(b"\r\n");
        if let Some(content_type) = &part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// `POST /chats/{id}/attachments` with a multipart body of `parts`.
pub fn upload_request(chat_id: Uuid, who: &SecurityContext, parts: &[FilePart]) -> Request<Body> {
    upload_request_raw(
        chat_id,
        who,
        Some(&format!("multipart/form-data; boundary={BOUNDARY}")),
        Body::from(multipart_body(parts)),
    )
}

/// `POST /chats/{id}/attachments` with an arbitrary `Content-Type` and body.
pub fn upload_request_raw(
    chat_id: Uuid,
    who: &SecurityContext,
    content_type: Option<&str>,
    body: Body,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(attachments_uri(chat_id))
        .header("x-request-id", "req-42");
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let mut request = builder.body(body).unwrap();
    request.extensions_mut().insert(who.clone());
    request
}

/// A request body that records whether anything polled it.
pub fn tracked_body(parts: &[FilePart]) -> (Body, Arc<AtomicBool>) {
    let polled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&polled);
    let bytes = Bytes::from(multipart_body(parts));
    let stream = futures::stream::poll_fn({
        let mut sent = false;
        move |_cx| {
            flag.store(true, Ordering::SeqCst);
            if sent {
                std::task::Poll::Ready(None)
            } else {
                sent = true;
                std::task::Poll::Ready(Some(Ok::<Bytes, std::io::Error>(bytes.clone())))
            }
        }
    });
    (Body::from_stream(stream), polled)
}

// ── image fixtures ───────────────────────────────────────────────────────────

/// The value modulo 256 as a byte.
fn byte(value: u32) -> u8 {
    u8::try_from(value % 256).unwrap()
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    image.write_to(&mut buffer, format).expect("encode");
    buffer.into_inner()
}

/// A gradient PNG of `width` x `height`.
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| Rgb([byte(x), byte(y), byte(x + y)]));
    encode(&DynamicImage::ImageRgb8(image), ImageFormat::Png)
}

/// A gradient JPEG of `width` x `height`.
pub fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| Rgb([byte(x), byte(y), 128]));
    encode(&DynamicImage::ImageRgb8(image), ImageFormat::Jpeg)
}

/// A GIF of `width` x `height`.
pub fn gif(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_fn(width, height, |x, y| Rgba([byte(x), byte(y), 64, 255]));
    encode(&DynamicImage::ImageRgba8(image), ImageFormat::Gif)
}

/// A (lossless) WebP of `width` x `height`.
pub fn webp(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_fn(width, height, |x, y| Rgba([byte(x), byte(y), 200, 255]));
    encode(&DynamicImage::ImageRgba8(image), ImageFormat::WebP)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    chunk.extend_from_slice(&body);
    chunk.extend_from_slice(&crc32(&body).to_be_bytes());
    chunk
}

/// A PNG whose header advertises `width` x `height` and whose pixel data is a stub: a pixel bomb
/// that only the header pre-screening can recognize.
pub fn png_header_only(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    bytes.extend(png_chunk(*b"IHDR", &ihdr));
    bytes.extend(png_chunk(
        *b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    ));
    bytes.extend(png_chunk(*b"IEND", &[]));
    bytes
}

// ── row seeding ──────────────────────────────────────────────────────────────

/// An `attachments` row to insert; every column the S1 tests read is settable.
#[derive(Debug, Clone)]
pub struct RowSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub kind: &'static str,
    pub status: &'static str,
    pub provider_file_id: Option<String>,
    pub error_code: Option<String>,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub thumbnail: Option<(i32, i32, Vec<u8>)>,
    pub cleanup_status: Option<&'static str>,
    pub cleanup_attempts: i32,
    pub last_cleanup_error: Option<String>,
    pub storage_backend: String,
    pub secondary_file_id: Option<String>,
    pub secondary_status: &'static str,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

impl RowSeed {
    /// A live `ready` document of user 1 of tenant A.
    pub fn document() -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            tenant_id: TENANT_A,
            uploaded_by_user_id: USER_A1,
            filename: "seed.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            size_bytes: 1000,
            kind: "document",
            status: "ready",
            provider_file_id: Some(format!("file-{}", id.simple())),
            error_code: None,
            for_file_search: true,
            for_code_interpreter: false,
            thumbnail: None,
            cleanup_status: None,
            cleanup_attempts: 0,
            last_cleanup_error: None,
            storage_backend: OPENAI_BACKEND.to_owned(),
            secondary_file_id: None,
            secondary_status: "not_attempted",
            created_at: super::chats::at(0),
            updated_at: super::chats::at(0),
            deleted_at: None,
        }
    }

    /// A live `ready` image of user 1 of tenant A.
    pub fn image() -> Self {
        Self {
            filename: "seed.png".to_owned(),
            content_type: "image/png".to_owned(),
            kind: "image",
            for_file_search: false,
            ..Self::document()
        }
    }

    pub fn status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    pub fn size(mut self, bytes: i64) -> Self {
        self.size_bytes = bytes;
        self
    }

    pub fn uploader(mut self, user_id: Uuid) -> Self {
        self.uploaded_by_user_id = user_id;
        self
    }

    pub fn tenant(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = tenant_id;
        self
    }

    pub fn deleted(mut self, at: OffsetDateTime) -> Self {
        self.deleted_at = Some(at);
        self
    }

    pub fn error(mut self, code: &str) -> Self {
        self.error_code = Some(code.to_owned());
        self
    }

    pub fn cleanup(mut self, status: &'static str) -> Self {
        self.cleanup_status = Some(status);
        self
    }

    pub fn no_provider_file(mut self) -> Self {
        self.provider_file_id = None;
        self
    }

    /// The provider file id of the row.
    pub fn file_id(mut self, provider_file_id: &str) -> Self {
        self.provider_file_id = Some(provider_file_id.to_owned());
        self
    }

    /// `cleanup_attempts` of the row.
    pub fn attempts(mut self, attempts: i32) -> Self {
        self.cleanup_attempts = attempts;
        self
    }

    pub fn last_error(mut self, error: &str) -> Self {
        self.last_cleanup_error = Some(error.to_owned());
        self
    }

    pub fn backend(mut self, storage_backend: &str) -> Self {
        self.storage_backend = storage_backend.to_owned();
        self
    }

    /// An Anthropic secondary copy with the given file id and `secondary_status`.
    pub fn secondary(mut self, file_id: &str, status: &'static str) -> Self {
        self.secondary_file_id = Some(file_id.to_owned());
        self.secondary_status = status;
        self
    }

    pub fn created(mut self, at: OffsetDateTime) -> Self {
        self.created_at = at;
        self
    }

    pub fn updated(mut self, at: OffsetDateTime) -> Self {
        self.updated_at = at;
        self
    }

    /// `for_file_search` false and `for_code_interpreter` true: an XLSX-like document.
    pub fn code_interpreter_only(mut self) -> Self {
        self.for_file_search = false;
        self.for_code_interpreter = true;
        self
    }

    pub fn for_code_interpreter(mut self, flag: bool) -> Self {
        self.for_code_interpreter = flag;
        self
    }

    pub fn thumbnail(mut self, width: i32, height: i32, data: &[u8]) -> Self {
        self.thumbnail = Some((width, height, data.to_vec()));
        self
    }
}

/// Insert the seed into `chat`; returns its id.
pub async fn seed_row(db: &TestDb, chat: &ChatRecord, seed: RowSeed) -> Uuid {
    let conn = db.provider.conn().unwrap();
    let (width, height, data) = match seed.thumbnail {
        Some((w, h, d)) => (Some(w), Some(h), Some(d)),
        None => (None, None, None),
    };
    let secondary_kind = seed
        .secondary_file_id
        .as_ref()
        .map(|_| "anthropic".to_owned());
    let row = attachment::ActiveModel {
        id: Set(seed.id),
        tenant_id: Set(seed.tenant_id),
        chat_id: Set(chat.id),
        uploaded_by_user_id: Set(seed.uploaded_by_user_id),
        filename: Set(seed.filename),
        content_type: Set(Some(seed.content_type)),
        size_bytes: Set(Some(seed.size_bytes)),
        storage_backend: Set(seed.storage_backend),
        provider_file_id: Set(seed.provider_file_id),
        status: Set(seed.status.to_owned()),
        error_code: Set(seed.error_code),
        attachment_kind: Set(seed.kind.to_owned()),
        for_file_search: Set(seed.for_file_search),
        for_code_interpreter: Set(seed.for_code_interpreter),
        doc_summary: Set(None),
        img_thumbnail: Set(data),
        img_thumbnail_width: Set(width),
        img_thumbnail_height: Set(height),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(seed.cleanup_status.map(str::to_owned)),
        cleanup_attempts: Set(seed.cleanup_attempts),
        last_cleanup_error: Set(seed.last_cleanup_error),
        cleanup_updated_at: Set(None),
        created_at: Set(seed.created_at),
        updated_at: Set(seed.updated_at),
        deleted_at: Set(seed.deleted_at),
        secondary_file_id: Set(seed.secondary_file_id),
        secondary_status: Set(seed.secondary_status.to_owned()),
        secondary_provider_kind: Set(secondary_kind),
    };
    secure_insert::<attachment::Entity>(row, &AccessScope::for_tenant(seed.tenant_id), &conn)
        .await
        .expect("seed attachment");
    seed.id
}

/// Seed `count` live documents (any status other than `failed`) of `size` bytes each.
pub async fn seed_documents(db: &TestDb, chat: &ChatRecord, count: usize, size: i64) {
    for _ in 0..count {
        seed_row(db, chat, RowSeed::document().size(size)).await;
    }
}

/// Insert a `chat_vector_stores` row.
pub async fn seed_vector_store(
    db: &TestDb,
    chat: &ChatRecord,
    vector_store_id: Option<&str>,
    provider: &str,
    created_at: OffsetDateTime,
) -> Uuid {
    let conn = db.provider.conn().unwrap();
    let id = Uuid::new_v4();
    let row = chat_vector_store::ActiveModel {
        id: Set(id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        vector_store_id: Set(vector_store_id.map(str::to_owned)),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(created_at),
    };
    secure_insert::<chat_vector_store::Entity>(
        row,
        &AccessScope::for_tenant(chat.tenant_id),
        &conn,
    )
    .await
    .expect("seed vector store");
    id
}

/// A JSON object's key set, for projection checks.
pub fn key_set(value: &Value) -> std::collections::BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

/// Whether `text` contains none of the provider-side secrets the mock puts into its answers.
pub fn assert_no_provider_text(text: &str) {
    for needle in [
        "provider-secret-error-text",
        "file-",
        "vs_",
        "file_ant_",
        "azure.example.com",
        "indexing_failed",
        "upload_failed",
        "vector_store_failed",
    ] {
        assert!(
            !text.contains(needle),
            "the response leaks `{needle}`: {text}"
        );
    }
}

/// The text of a default-time kill-switch snapshot builder.
pub fn switches(images: bool, code_interpreter: bool, file_search: bool) -> KillSwitches {
    KillSwitches {
        disable_images: images,
        disable_code_interpreter: code_interpreter,
        disable_file_search: file_search,
        ..KillSwitches::default()
    }
}

// ── shared constants and assertions of the REST tests ────────────────────────

pub const PDF: &str = "application/pdf";
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

/// A small PDF-looking body.
pub fn pdf_bytes() -> Vec<u8> {
    b"%PDF-1.4 a small test document".to_vec()
}

/// The number of status reads of the in-request wait that ends at the 25 s deadline: reads at
/// 250, 750 and 1750 ms, then every 2 s from 3750 ms to 23750 ms.
pub const IN_REQUEST_READS: usize = 14;

/// The `id` of a 201 or 200 attachment body.
pub fn body_id(reply: &Reply) -> Uuid {
    Uuid::parse_str(reply.json()["id"].as_str().expect("id")).expect("uuid id")
}

/// Assert a 201 `AttachmentDetail` with `status`; returns the body.
pub fn assert_created(reply: &Reply, status: &str) -> Value {
    assert_eq!(reply.status.as_u16(), 201, "{}", reply.text);
    assert_eq!(reply.content_type, "application/json", "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["status"], status, "{body}");
    for key in [
        "id",
        "filename",
        "content_type",
        "size_bytes",
        "kind",
        "created_at",
    ] {
        assert!(body.get(key).is_some(), "missing `{key}` in {body}");
    }
    body
}

/// Assert the 503 of a provider storage failure: `Retry-After: 10`, the generic detail, and a
/// body without the row error code or provider text.
pub fn assert_storage_unavailable(reply: &Reply) -> Value {
    let body = super::chats::assert_problem(reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("10"), "{}", reply.text);
    assert_eq!(body["context"]["retry_after_seconds"], 10, "{body}");
    assert_eq!(body["detail"], "Service temporarily unavailable", "{body}");
    assert_no_provider_text(&reply.text);
    body
}

/// Make every INSERT into `attachments` fail (a database failure at the insert step).
pub async fn fail_attachment_inserts(db: &TestDb) {
    super::raw_exec(
        &db.raw,
        "CREATE TRIGGER fail_attachment_insert BEFORE INSERT ON attachments \
         BEGIN SELECT RAISE(ABORT, 'forced attachment insert failure'); END",
    )
    .await
    .expect("create trigger");
}

impl AttachRig {
    /// The state a refused request must leave: every permit free, no provider call, no row.
    pub async fn assert_untouched(&self, chat_id: Uuid) {
        assert_eq!(
            self.permits.available(),
            usize::from(self.rag.max_concurrent_uploads),
            "a permit leaked"
        );
        assert_eq!(self.storage.call_count(), 0, "a provider call was made");
        assert!(self.rows(chat_id).await.is_empty(), "a row was written");
    }

    /// Upload a PDF as user 1 of tenant A.
    pub async fn upload_pdf(&self, chat_id: Uuid) -> Reply {
        self.upload(chat_id, "report.pdf", PDF, &pdf_bytes()).await
    }

    /// The `(kind, result)` samples of `attachment_upload`.
    pub fn uploads(&self) -> Vec<(String, String)> {
        self.metrics.uploads()
    }
}

/// A live chat of `who` on the standard model.
pub async fn seed_chat_for(rig: &AttachRig, who: &SecurityContext) -> ChatRecord {
    rig.quota.chat(who, STD).await
}

/// Assert the 400 `out_of_range` of a file above the effective limit:
/// `field_violations[content_length].reason = FILE_TOO_LARGE`.
pub fn assert_file_too_large(reply: &Reply) -> Value {
    let body = super::chats::assert_problem(reply, 400, "out_of_range");
    assert_eq!(
        super::chats::violation(&body, "content_length")["reason"],
        "FILE_TOO_LARGE",
        "{body}"
    );
    body
}

/// Whether `name` ends with the extension `ext` (without the dot), ignoring case.
pub fn has_extension(name: &str, ext: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(ext))
}

// ── FEATURE 0005 slice S2: delete, cleanup handler and reaper helpers ───────────────────────

/// `DELETE` the attachment as `who`.
impl AttachRig {
    pub async fn delete_attachment_as(
        &self,
        who: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Reply {
        self.send(request(
            "DELETE",
            &attachment_uri(chat_id, attachment_id),
            who,
        ))
        .await
    }

    /// `DELETE` the attachment as user 1 of tenant A.
    pub async fn delete_attachment(&self, chat_id: Uuid, attachment_id: Uuid) -> Reply {
        self.delete_attachment_as(&a1(), chat_id, attachment_id)
            .await
    }
}

/// Soft-delete the chat row (`deleted_at`), as chat deletion does.
pub async fn soft_delete_chat(db: &TestDb, chat_id: Uuid, at: OffsetDateTime) {
    use sea_orm::ColumnTrait;
    use sea_orm::QueryFilter;
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;
    let conn = db.provider.conn().unwrap();
    crate::infra::storage::entity::chat::Entity::update_many()
        .col_expr(
            crate::infra::storage::entity::chat::Column::DeletedAt,
            Expr::value(at),
        )
        .filter(crate::infra::storage::entity::chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("soft-delete the chat");
}

/// The cleanup handler of the rig: the real database store (with fault injection), the real OAGW
/// storage client over the mock gateway, the virtual clock and the recording metrics.
pub struct CleanupRig {
    pub handler: crate::infra::attachment_cleanup::AttachmentCleanupHandler,
    pub faults: Arc<super::cleanup_fakes::CleanupFaults>,
}

impl AttachRig {
    /// The metrics sink of the rig: the OTLP instruments when the rig was built `with_otlp`,
    /// else the recording double.
    pub fn sink(&self) -> Arc<dyn AttachmentMetrics> {
        self.otlp.as_ref().map_or_else(
            || self.metrics.clone() as Arc<dyn AttachmentMetrics>,
            |harness| harness.metrics.clone() as Arc<dyn AttachmentMetrics>,
        )
    }

    /// A cleanup handler with the default budget (5) and an Anthropic Files client.
    pub fn cleanup(&self) -> CleanupRig {
        self.cleanup_with(5, true)
    }

    /// A cleanup handler with `max_attempts` and, when `secondary` is false, no Anthropic Files
    /// client.
    pub fn cleanup_with(&self, max_attempts: u32, secondary: bool) -> CleanupRig {
        use crate::domain::attachments::cleanup::{AttachmentCleanupWorker, CleanupDeps};
        use crate::domain::attachments::provider::StorageProvider;
        let faults = Arc::new(super::cleanup_fakes::CleanupFaults::default());
        let store = Arc::new(super::cleanup_fakes::FaultyCleanupStore {
            inner: self.quota.db.provider.clone(),
            faults: Arc::clone(&faults),
        });
        let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
            self.storage.clone(),
            S2sContext::bound(provider_s2s()),
        ));
        let clock: Arc<dyn Clock> = self.time.clone();
        let worker = AttachmentCleanupWorker::new(CleanupDeps {
            store,
            storage: Arc::clone(&client),
            secondary_storage: secondary.then(|| Arc::clone(&client)),
            providers: providers(),
            clock,
            metrics: self.sink(),
            max_attempts,
        });
        CleanupRig {
            handler: crate::infra::attachment_cleanup::AttachmentCleanupHandler::new(Arc::new(
                worker,
            )),
            faults,
        }
    }
}

/// The outbox message the handler is invoked with.
pub fn cleanup_message(payload: &Value, attempts: i16) -> toolkit_db::outbox::OutboxMessage {
    toolkit_db::outbox::OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: serde_json::to_vec(payload).unwrap(),
        payload_type: "mini_chat.attachment_cleanup".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

/// A raw outbox message (not necessarily JSON).
pub fn raw_cleanup_message(payload: &[u8]) -> toolkit_db::outbox::OutboxMessage {
    toolkit_db::outbox::OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: payload.to_vec(),
        payload_type: "mini_chat.attachment_cleanup".to_owned(),
        created_at: chrono::Utc::now(),
        attempts: 0,
    }
}

/// The payload of an `attachment_deleted` message for a seeded row.
pub fn deleted_payload(chat: &ChatRecord, attachment_id: Uuid, file: Option<&str>) -> Value {
    json!({
        "event_type": "attachment_deleted",
        "tenant_id": chat.tenant_id,
        "chat_id": chat.id,
        "attachment_id": attachment_id,
        "provider_file_id": file,
        "vector_store_id": Value::Null,
        "storage_backend": OPENAI_BACKEND,
        "attachment_kind": "document",
        "deleted_at": "2026-02-28T15:30:00Z",
        "secondary_ref": Value::Null,
    })
}

/// `ok`, `retry` or `reject` for a handler result.
pub fn outcome_of(result: &toolkit_db::outbox::MessageResult) -> &'static str {
    match result {
        toolkit_db::outbox::MessageResult::Ok => "ok",
        toolkit_db::outbox::MessageResult::Retry => "retry",
        toolkit_db::outbox::MessageResult::Reject(_) => "reject",
    }
}

// ── the upload reaper ────────────────────────────────────────────────────────────────────────

/// A leader elector the test can flip.
pub struct ToggleElector(pub AtomicBool);

impl ToggleElector {
    pub fn leader() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(true)))
    }

    pub fn follower() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
    }
}

#[async_trait]
impl crate::infra::leader::LeaderElector for ToggleElector {
    async fn is_leader(&self, role: crate::infra::leader::LeaderRole) -> bool {
        assert_eq!(
            role,
            crate::infra::leader::LeaderRole::UploadReaper,
            "the reaper asks for its own role"
        );
        self.0.load(Ordering::SeqCst)
    }
}

/// The settings of the defaults: enabled, a 60 s scan interval, a 300 s stale threshold.
pub fn reaper_defaults() -> crate::infra::upload_reaper::ReaperSettings {
    crate::infra::upload_reaper::ReaperSettings {
        enabled: true,
        scan_interval: Duration::from_secs(60),
        stale_after: Duration::from_secs(300),
    }
}

impl AttachRig {
    /// The reaper with the default settings, the real store and the leader role.
    pub fn reaper(&self) -> Arc<crate::infra::upload_reaper::UploadReaper> {
        self.reaper_with(reaper_defaults(), ToggleElector::leader(), None)
    }

    /// The reaper with the given settings and elector; `store` replaces the database store.
    pub fn reaper_with(
        &self,
        settings: crate::infra::upload_reaper::ReaperSettings,
        elector: Arc<dyn crate::infra::leader::LeaderElector>,
        store: Option<Arc<dyn crate::domain::attachments::reaper::UploadReaperStore>>,
    ) -> Arc<crate::infra::upload_reaper::UploadReaper> {
        let store = store.unwrap_or_else(|| self.quota.db.provider.clone());
        let clock: Arc<dyn Clock> = self.time.clone();
        Arc::new(crate::infra::upload_reaper::UploadReaper::new(
            crate::infra::upload_reaper::ReaperDeps {
                store,
                cleanup: self.quota.enqueuer.clone(),
                elector,
                clock,
                timer: self.time.clone(),
                metrics: self.sink(),
                settings,
            },
        ))
    }
}

/// An [`UploadReaperStore`](crate::domain::attachments::reaper::UploadReaperStore) that runs raw
/// SQL right after the selection, as a concurrent writer would between the selection and the
/// guarded update.
pub struct RacingReaperStore {
    pub inner: Arc<dyn crate::domain::attachments::reaper::UploadReaperStore>,
    pub raw: sea_orm::DatabaseConnection,
    pub after_select: Mutex<Vec<String>>,
}

#[async_trait]
impl crate::domain::attachments::reaper::UploadReaperStore for RacingReaperStore {
    async fn select_stale(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<crate::domain::attachments::reaper::StaleUpload>, DomainError> {
        let rows = self.inner.select_stale(cutoff, limit).await?;
        let statements: Vec<String> = std::mem::take(&mut *self.after_select.lock().unwrap());
        for statement in statements {
            super::raw_exec(&self.raw, &statement)
                .await
                .expect("racing write");
        }
        Ok(rows)
    }

    async fn reap(
        &self,
        row: &crate::domain::attachments::reaper::StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        self.inner.reap(row, cutoff, now, cleanup).await
    }
}

/// The raw SQL blob literal of a UUID.
pub fn blob(id: Uuid) -> String {
    super::chats::blob(id)
}

/// An [`UploadReaperStore`](crate::domain::attachments::reaper::UploadReaperStore) whose
/// selection always fails (a database failure).
pub struct FailingReaperStore;

#[async_trait]
impl crate::domain::attachments::reaper::UploadReaperStore for FailingReaperStore {
    async fn select_stale(
        &self,
        _cutoff: OffsetDateTime,
        _limit: u32,
    ) -> Result<Vec<crate::domain::attachments::reaper::StaleUpload>, DomainError> {
        Err(DomainError::internal("injected selection failure"))
    }

    async fn reap(
        &self,
        _row: &crate::domain::attachments::reaper::StaleUpload,
        _cutoff: OffsetDateTime,
        _now: OffsetDateTime,
        _cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        Err(DomainError::internal("never reached"))
    }
}
