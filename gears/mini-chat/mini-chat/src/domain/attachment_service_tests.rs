#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Attachment service tests: MIME / purpose / filename / size rules (pure),
//! and the upload, read and delete flows on in-memory `SQLite` with a
//! scripted fake of the provider storage.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit_db::outbox::MessageResult;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::*;
use crate::domain::chat_service::tests::{OwnerPdp, ctx_of, owner_authz};
use crate::domain::error::{ExhaustedLimit, conflict};
use crate::domain::models_service::tests::catalog_entry;
use crate::domain::ports::{
    AttachmentRepository, MessageAttachmentLinks, MessageRole, NewChat, NewVectorStorePlaceholder,
    StorageError, StoredThumbnail,
};
use crate::infra::chat_cleanup_handler::ChatCleanupHandler;
use crate::infra::db::entities::{attachments, chat_vector_stores};
use crate::infra::db::repo::tests::{
    MessageSeed, TENANT_A, TENANT_B, TestStore, USER_A1, USER_A2, USER_B1, at, attachment_row,
    chat_row, link_attachment, seed_message, subject_scope,
};
use crate::infra::db::repo::{attachments as attachment_repo, chats as chat_repo};
use crate::infra::outbox::{ChatCleanupPayload, ChatCleanupReason, OutboxQueues};

// ── Pure rules ───────────────────────────────────────────────────────────

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

fn resolve(declared: &str, filename: &str) -> Result<ResolvedType, DomainError> {
    resolve_content_type(declared, filename, true)
}

fn is_unsupported(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::InvalidArgument {
            reason: InvalidArgumentReason::UnsupportedContentType,
            ..
        }
    )
}

#[test]
fn document_types_are_documents_for_file_search() {
    for (mime, extension) in [
        ("application/pdf", "pdf"),
        ("text/plain", "txt"),
        ("text/markdown", "md"),
        ("text/html", "html"),
        ("application/json", "json"),
        (DOCX, "docx"),
        (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
    ] {
        let resolved = resolve(mime, "file").unwrap();
        assert_eq!(resolved.content_type, mime);
        assert_eq!(resolved.extension, extension);
        assert_eq!(resolved.kind, AttachmentKind::Document);
        assert!(!resolved.code_interpreter_only, "{mime}");
    }
}

#[test]
fn image_types_are_images() {
    for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        let resolved = resolve(mime, "picture").unwrap();
        assert_eq!(resolved.kind, AttachmentKind::Image, "{mime}");
        assert_eq!(resolved.content_type, mime);
    }
    assert_eq!(
        resolve("image/jpg", "a").unwrap().content_type,
        "image/jpeg"
    );
}

#[test]
fn xlsx_is_a_code_interpreter_only_document() {
    let resolved = resolve(XLSX_MIME, "sheet.xlsx").unwrap();
    assert_eq!(resolved.kind, AttachmentKind::Document);
    assert!(resolved.code_interpreter_only);
    assert_eq!(resolved.extension, "xlsx");
}

#[test]
fn content_type_parameters_and_case_are_ignored() {
    assert_eq!(
        resolve("Text/Plain; charset=utf-8", "a.txt")
            .unwrap()
            .content_type,
        "text/plain"
    );
    assert_eq!(
        resolve(" APPLICATION/PDF ", "a").unwrap().content_type,
        "application/pdf"
    );
}

#[test]
fn octet_stream_is_inferred_from_the_extension() {
    assert_eq!(
        resolve(OCTET_STREAM, "scan.pdf").unwrap().content_type,
        "application/pdf"
    );
    assert_eq!(
        resolve(OCTET_STREAM, "sheet.XLSX").unwrap().content_type,
        XLSX_MIME
    );
    let png = resolve(OCTET_STREAM, "photo.png").unwrap();
    assert_eq!(
        (png.content_type, png.kind),
        ("image/png", AttachmentKind::Image)
    );
    for filename in ["blob.qqq", "noext", ".pdf", "trailing."] {
        assert!(
            is_unsupported(&resolve(OCTET_STREAM, filename).unwrap_err()),
            "{filename}"
        );
    }
}

#[test]
fn csv_is_plain_text_when_allowed() {
    let csv = resolve_content_type("text/csv", "revenue.csv", true).unwrap();
    assert_eq!(csv.content_type, TEXT_PLAIN);
    assert_eq!(csv.extension, "txt", "the provider file is a .txt");
    assert_eq!(
        resolve_content_type(OCTET_STREAM, "revenue.csv", true)
            .unwrap()
            .content_type,
        TEXT_PLAIN
    );
    assert!(is_unsupported(
        &resolve_content_type("text/csv", "revenue.csv", false).unwrap_err()
    ));
}

#[test]
fn other_types_are_unsupported() {
    for mime in [
        "application/x-msdownload",
        "video/mp4",
        "image/tiff",
        "",
        "application/zip",
    ] {
        assert!(
            is_unsupported(&resolve(mime, "file.bin").unwrap_err()),
            "{mime}"
        );
    }
}

fn model(tool_ci: bool, max_file_size_mb: u32) -> ModelCatalogEntry {
    catalog_entry(
        "m",
        json!({ "general_config": {
            "max_file_size_mb": max_file_size_mb,
            "tool_support": { "code_interpreter": tool_ci },
        }}),
    )
}

fn switches(disable_images: bool, disable_code_interpreter: bool) -> KillSwitches {
    KillSwitches {
        disable_images,
        disable_code_interpreter,
        ..KillSwitches::default()
    }
}

fn is_ci_unavailable(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::InvalidArgument {
            field: "file",
            reason: InvalidArgumentReason::CodeInterpreterUnavailable,
        }
    )
}

#[test]
fn purpose_matrix() {
    let pdf = resolve("application/pdf", "a.pdf").unwrap();
    let png = resolve("image/png", "a.png").unwrap();
    let xlsx = resolve(XLSX_MIME, "a.xlsx").unwrap();
    let ci = model(true, 25);
    let no_ci = model(false, 25);
    let off = switches(false, false);

    assert_eq!(purposes(&pdf, off, &ci).unwrap(), (true, false));
    assert_eq!(purposes(&png, off, &ci).unwrap(), (false, false));
    assert_eq!(purposes(&xlsx, off, &ci).unwrap(), (false, true));

    // Kill switches and model capability.
    assert!(matches!(
        purposes(&png, switches(true, false), &ci).unwrap_err(),
        DomainError::FailedPrecondition {
            subject: PreconditionSubject::Images
        }
    ));
    assert!(is_ci_unavailable(
        &purposes(&xlsx, switches(false, true), &ci).unwrap_err()
    ));
    assert!(is_ci_unavailable(
        &purposes(&xlsx, off, &no_ci).unwrap_err()
    ));

    // Documents are unaffected by both switches.
    assert_eq!(
        purposes(&pdf, switches(true, true), &no_ci).unwrap(),
        (true, false)
    );
}

#[test]
fn filename_defaults_and_truncation() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("   ")), "upload");
    assert_eq!(normalize_filename(Some(" report.pdf ")), "report.pdf");

    let long = format!("{}.pdf", "q".repeat(296));
    let truncated = normalize_filename(Some(&long));
    assert_eq!(truncated, format!("{}.pdf", "q".repeat(251)));
    assert_eq!(truncated.chars().count(), 255);

    let no_extension = "z".repeat(300);
    assert_eq!(normalize_filename(Some(&no_extension)), "z".repeat(255));

    let unicode = format!("{}.txt", "\u{e9}".repeat(300));
    let truncated = normalize_filename(Some(&unicode));
    assert_eq!(truncated.chars().count(), 255);
    assert_eq!(truncated.rsplit_once('.').map(|(_, ext)| ext), Some("txt"));

    let exact = "a".repeat(255);
    assert_eq!(normalize_filename(Some(&exact)), exact);
}

#[test]
fn effective_size_limit_is_the_smaller_of_gear_and_model() {
    let settings = AttachmentSettings {
        document_max_bytes: 25_600 * 1024,
        image_max_bytes: 5_120 * 1024,
        ..test_settings()
    };
    assert_eq!(
        effective_max_bytes(AttachmentKind::Document, &settings, &model(true, 25)),
        25 * MIB
    );
    assert_eq!(
        effective_max_bytes(AttachmentKind::Image, &settings, &model(true, 25)),
        5 * MIB
    );
    assert_eq!(
        effective_max_bytes(AttachmentKind::Document, &settings, &model(true, 2)),
        2 * MIB
    );
}

#[test]
fn size_check_rejects_only_above_the_limit() {
    let spec = FileSpec {
        filename: "a.txt".to_owned(),
        content_type: TEXT_PLAIN,
        kind: AttachmentKind::Document,
        for_file_search: true,
        for_code_interpreter: false,
        max_bytes: 10,
        provider_extension: "txt",
    };
    spec.check_size(10).unwrap();
    assert!(matches!(
        spec.check_size(11).unwrap_err(),
        DomainError::OutOfRange {
            field: "content_length",
            reason: OutOfRangeReason::FileTooLarge,
        }
    ));
}

#[test]
fn settings_follow_the_configuration() {
    let mut config = MiniChatConfig::default();
    config.rag.max_total_upload_mb_per_chat = 3;
    config.rag.uploaded_file_max_size_kb = 2;
    config.rag.uploaded_image_max_size_kb = 1;
    let settings = AttachmentSettings::from_config(&config);
    assert_eq!(settings.max_total_upload_bytes_per_chat, 3 * MIB);
    assert_eq!(settings.document_max_bytes, 2048);
    assert_eq!(settings.image_max_bytes, 1024);
    assert_eq!(settings.timing, UploadTiming::default());
    assert_eq!(settings.timing.indexing_deadline, Duration::from_secs(25));
}

// ── Fakes ────────────────────────────────────────────────────────────────

/// One recorded storage call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Upload {
        backend: String,
        filename: String,
        content_type: String,
        size: usize,
    },
    DeleteFile(String),
    CreateStore(Uuid),
    DeleteStore(String),
    AddFile {
        vector_store_id: String,
        file_id: String,
        attachment_id: Uuid,
    },
    Status(String),
}

type Scripted<T> = Mutex<VecDeque<Result<T, StorageError>>>;

/// Scripted provider storage: each operation pops its next scripted result,
/// or succeeds (status reads return `default_status`).
pub struct FakeStorage {
    pub uploads: Scripted<String>,
    pub creates: Scripted<String>,
    pub adds: Scripted<VectorStoreFileStatus>,
    pub statuses: Scripted<VectorStoreFileStatus>,
    pub deletes: Scripted<()>,
    pub default_status: Mutex<VectorStoreFileStatus>,
    pub upload_delay: Mutex<Duration>,
    pub create_delay: Mutex<Duration>,
    pub calls: Mutex<Vec<Call>>,
    seq: AtomicUsize,
}

impl FakeStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            uploads: Mutex::new(VecDeque::new()),
            creates: Mutex::new(VecDeque::new()),
            adds: Mutex::new(VecDeque::new()),
            statuses: Mutex::new(VecDeque::new()),
            deletes: Mutex::new(VecDeque::new()),
            default_status: Mutex::new(VectorStoreFileStatus::Completed),
            upload_delay: Mutex::new(Duration::ZERO),
            create_delay: Mutex::new(Duration::ZERO),
            calls: Mutex::new(Vec::new()),
            seq: AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, matches: impl Fn(&Call) -> bool) -> usize {
        self.calls().iter().filter(|call| matches(call)).count()
    }

    pub fn set_default_status(&self, status: VectorStoreFileStatus) {
        *self.default_status.lock().unwrap() = status;
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}{:012}", self.seq.fetch_add(1, Ordering::SeqCst))
    }

    fn pop<T>(queue: &Scripted<T>) -> Option<Result<T, StorageError>> {
        queue.lock().unwrap().pop_front()
    }
}

#[async_trait]
impl FileStorage for FakeStorage {
    fn backend_for_provider(&self, provider_id: &str) -> Result<String, DomainError> {
        // The Anthropic entries of the secondary-copy tests keep their files
        // with the `openai` RAG provider.
        if matches!(provider_id, "openai" | "claude-first" | "claude-second") {
            Ok("openai".to_owned())
        } else {
            Err(DomainError::internal("unknown provider"))
        }
    }

    async fn upload_file(
        &self,
        target: &StorageTarget,
        file: FileUpload,
    ) -> Result<String, StorageError> {
        self.record(Call::Upload {
            backend: target.backend.clone(),
            filename: file.filename,
            content_type: file.content_type,
            size: file.data.len(),
        });
        let delay = *self.upload_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        Self::pop(&self.uploads).unwrap_or_else(|| Ok(self.next_id("file-")))
    }

    async fn delete_file(
        &self,
        _target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), StorageError> {
        self.record(Call::DeleteFile(file_id.to_owned()));
        Self::pop(&self.deletes).unwrap_or(Ok(()))
    }
}

#[async_trait]
impl VectorStores for FakeStorage {
    async fn create_vector_store(
        &self,
        _target: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError> {
        self.record(Call::CreateStore(chat_id));
        let delay = *self.create_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        Self::pop(&self.creates).unwrap_or_else(|| Ok(self.next_id("vs_")))
    }

    async fn delete_vector_store(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError> {
        self.record(Call::DeleteStore(vector_store_id.to_owned()));
        Ok(())
    }

    async fn add_file(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        self.record(Call::AddFile {
            vector_store_id: vector_store_id.to_owned(),
            file_id: file_id.to_owned(),
            attachment_id,
        });
        Self::pop(&self.adds).unwrap_or(Ok(VectorStoreFileStatus::Completed))
    }

    async fn file_status(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
        file_id: &str,
    ) -> Result<VectorStoreFileStatus, StorageError> {
        self.record(Call::Status(file_id.to_owned()));
        Self::pop(&self.statuses).unwrap_or_else(|| Ok(self.default_status.lock().unwrap().clone()))
    }
}

/// Thumbnailer returning a fixed 4x2 thumbnail.
struct FixedThumbnail;

impl ThumbnailRenderer for FixedThumbnail {
    fn render(&self, _data: &[u8]) -> Option<StoredThumbnail> {
        Some(StoredThumbnail {
            data: b"RIFF-thumb".to_vec(),
            width: 4,
            height: 2,
        })
    }
}

/// Policy plugin serving one catalog with configurable kill switches.
struct TestPolicy {
    catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for TestPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        Ok(1)
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Ok(PolicySnapshot {
            policy_version,
            model_catalog: self.catalog.clone(),
            kill_switches: self.kill_switches,
        })
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

fn test_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry(
            "standard",
            json!({ "general_config": { "tool_support": { "code_interpreter": true } } }),
        ),
        catalog_entry("text-only", json!({})),
        catalog_entry("foreign", json!({ "provider_id": "elsewhere" })),
        catalog_entry("claude", json!({ "provider_id": "claude-second" })),
    ]
}

/// Short waits so the deadline, background and loser paths run in tests.
pub fn fast_timing() -> UploadTiming {
    UploadTiming {
        indexing_deadline: Duration::from_millis(800),
        poll_first_wait: Duration::from_millis(20),
        poll_max_wait: Duration::from_millis(80),
        background_round: Duration::from_millis(150),
        background_max_wait: Duration::from_millis(40),
        background_limit: Duration::from_millis(1500),
        set_ready_backoff: [Duration::from_millis(10); 3],
        store_poll_first_wait: Duration::from_millis(30),
        store_polls: 5,
        stale_placeholder_after: Duration::from_secs(120),
    }
}

fn test_settings() -> AttachmentSettings {
    AttachmentSettings {
        max_documents_per_chat: 50,
        max_total_upload_bytes_per_chat: 100 * MIB,
        allow_csv_upload: true,
        max_concurrent_uploads: 10,
        document_max_bytes: 25 * MIB,
        image_max_bytes: 5 * MIB,
        timing: fast_timing(),
    }
}

fn a1() -> SecurityContext {
    ctx_of(TENANT_A, USER_A1)
}

fn a1_scope() -> AccessScope {
    subject_scope(TENANT_A, USER_A1)
}

struct Fixture {
    store: TestStore,
    storage: Arc<FakeStorage>,
    service: AttachmentService,
    shutdown: CancellationToken,
}

impl Fixture {
    async fn new() -> Self {
        Self::with(test_settings(), KillSwitches::default()).await
    }

    async fn with(settings: AttachmentSettings, kill_switches: KillSwitches) -> Self {
        let store = TestStore::new().await;
        let storage = FakeStorage::new();
        let pdp = OwnerPdp::new();
        let authz = owner_authz(&pdp);
        let policy = Arc::new(TestPolicy {
            catalog: test_catalog(),
            kill_switches,
        });
        let models = Arc::new(ModelsService::new(policy, Arc::clone(&authz)));
        let shutdown = CancellationToken::new();
        let ports = AttachmentPorts {
            chats: Arc::clone(&store.repo) as _,
            attachments: Arc::clone(&store.repo) as _,
            vector_stores: Arc::clone(&store.repo) as _,
            files: Arc::clone(&storage) as _,
            stores: Arc::clone(&storage) as _,
            thumbnails: Arc::new(FixedThumbnail),
        };
        let service = AttachmentService::new(ports, models, authz, settings, shutdown.clone());
        Self {
            store,
            storage,
            service,
            shutdown,
        }
    }

    async fn chat_with(&self, tenant: Uuid, user: Uuid, model: &str) -> Uuid {
        let id = Uuid::new_v4();
        chat_repo::insert(
            &self.store.conn(),
            self.store.ts(),
            &AccessScope::allow_all(),
            NewChat {
                id,
                tenant_id: tenant,
                user_id: user,
                model: model.to_owned(),
                title: None,
                now: at(0),
            },
        )
        .await
        .unwrap();
        id
    }

    async fn chat(&self) -> Uuid {
        self.chat_with(TENANT_A, USER_A1, "standard").await
    }

    async fn upload(
        &self,
        chat_id: Uuid,
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> Result<AttachmentDetail, DomainError> {
        let session = self.service.begin_upload(&a1(), chat_id).await?;
        let spec = self
            .service
            .classify_file(&session, Some(filename), content_type)?;
        self.service
            .upload(session, spec, Bytes::copy_from_slice(data))
            .await
    }

    async fn pdf(&self, chat_id: Uuid) -> Result<AttachmentDetail, DomainError> {
        self.upload(chat_id, "report.pdf", "application/pdf", b"%PDF-1.4 test")
            .await
    }

    async fn store_rows(&self, chat_id: Uuid) -> Vec<chat_vector_stores::Model> {
        chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    async fn rows(&self, chat_id: Uuid) -> Vec<attachments::Model> {
        attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }

    fn cleanup_queue() -> String {
        OutboxQueues::default().attachment_cleanup
    }
}

fn is_storage_unavailable(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::ServiceUnavailable {
            retry_after_secs: 10
        }
    )
}

fn is_provider_mismatch(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::AlreadyExists {
            resource_name: conflict::PROVIDER_MISMATCH,
            ..
        }
    )
}

fn is_not_found(err: &DomainError, resource: ResourceKind) -> bool {
    matches!(err, DomainError::NotFound { resource: r, .. } if *r == resource)
}

async fn eventually<F, Fut>(what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

// ── Upload: documents ────────────────────────────────────────────────────

#[tokio::test]
async fn document_upload_indexes_into_a_new_chat_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;

    let detail = fx.pdf(chat_id).await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(detail.kind, AttachmentKind::Document);
    assert_eq!(detail.filename, "report.pdf");
    assert_eq!(detail.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(detail.size_bytes, Some(13));
    assert_eq!(detail.error_code, None);
    assert_eq!(detail.img_thumbnail, None);

    let calls = fx.storage.calls();
    let Call::Upload {
        backend,
        filename,
        content_type,
        size,
    } = &calls[0]
    else {
        panic!("first call is the upload: {calls:?}");
    };
    assert_eq!(backend, "openai");
    assert_eq!(filename, &format!("{chat_id}_{}.pdf", detail.id));
    assert_eq!((content_type.as_str(), *size), ("application/pdf", 13));
    assert_eq!(calls[1], Call::CreateStore(chat_id));
    let row = attachment_row(&fx.store, detail.id).await;
    let stores = fx.store_rows(chat_id).await;
    assert_eq!(stores.len(), 1);
    let vector_store_id = stores[0].vector_store_id.clone().unwrap();
    assert_eq!(stores[0].provider, "openai");
    assert_eq!(stores[0].tenant_id, TENANT_A);
    assert_eq!(
        calls[2],
        Call::AddFile {
            vector_store_id,
            file_id: row.provider_file_id.clone().unwrap(),
            attachment_id: detail.id,
        }
    );

    assert_eq!(row.status, "ready");
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(row.uploaded_by_user_id, USER_A1);
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.secondary_status, "not_attempted");
}

#[tokio::test]
async fn second_document_reuses_the_chat_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;

    fx.pdf(chat_id).await.unwrap();
    fx.upload(chat_id, "notes.txt", "text/plain", b"hello")
        .await
        .unwrap();

    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 1);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::AddFile { .. })), 2);
    assert_eq!(fx.store_rows(chat_id).await.len(), 1);

    let other = fx.chat().await;
    fx.pdf(other).await.unwrap();
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 2);
}

#[tokio::test]
async fn csv_is_stored_as_plain_text_with_a_txt_provider_name() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;

    let detail = fx
        .upload(chat_id, "revenue.csv", "text/csv", b"a,b\n1,2\n")
        .await
        .unwrap();

    assert_eq!(detail.content_type.as_deref(), Some(TEXT_PLAIN));
    assert_eq!(detail.filename, "revenue.csv");
    assert!(fx.storage.calls().iter().any(|call| matches!(
        call,
        Call::Upload { filename, content_type, .. }
            if filename.rsplit_once('.').map(|(_, ext)| ext) == Some("txt") && content_type == TEXT_PLAIN
    )));
}

#[tokio::test]
async fn xlsx_is_ready_without_the_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;

    let detail = fx
        .upload(chat_id, "sales.xlsx", XLSX_MIME, b"PK\x03\x04")
        .await
        .unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    let row = attachment_row(&fx.store, detail.id).await;
    assert!(!row.for_file_search && row.for_code_interpreter);
    assert!(row.provider_file_id.is_some());
    assert_eq!(fx.storage.calls().len(), 1, "upload only");
    assert!(fx.store_rows(chat_id).await.is_empty());
}

#[tokio::test]
async fn image_is_ready_with_a_thumbnail_and_without_the_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;

    let detail = fx
        .upload(chat_id, "chart.png", "image/png", b"\x89PNG")
        .await
        .unwrap();

    assert_eq!(detail.kind, AttachmentKind::Image);
    assert_eq!(detail.status, AttachmentStatus::Ready);
    let thumbnail = detail.img_thumbnail.unwrap();
    assert_eq!(thumbnail.content_type, "image/webp");
    assert_eq!((thumbnail.width, thumbnail.height), (4, 2));
    let row = attachment_row(&fx.store, detail.id).await;
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.img_thumbnail.as_deref(), Some(&b"RIFF-thumb"[..]));
    assert_eq!(fx.storage.calls().len(), 1, "upload only");
    assert!(fx.store_rows(chat_id).await.is_empty());
}

// ── Upload: pre-body and header checks ───────────────────────────────────

#[tokio::test]
async fn begin_upload_hides_unreachable_chats() {
    let fx = Fixture::new().await;
    let foreign = fx.chat_with(TENANT_A, USER_A2, "standard").await;
    let other_tenant = fx.chat_with(TENANT_B, USER_B1, "standard").await;

    for chat_id in [Uuid::new_v4(), foreign, other_tenant] {
        let err = fx.service.begin_upload(&a1(), chat_id).await.unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Chat), "{err:?}");
    }
}

#[tokio::test]
async fn begin_upload_rejects_a_model_that_left_the_catalog() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat_with(TENANT_A, USER_A1, "retired").await;

    let err = fx.service.begin_upload(&a1(), chat_id).await.unwrap_err();

    assert!(matches!(
        err,
        DomainError::InvalidArgument {
            field: "model",
            reason: InvalidArgumentReason::InvalidModel
        }
    ));
}

#[tokio::test]
async fn begin_upload_fails_for_an_unconfigured_provider() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat_with(TENANT_A, USER_A1, "foreign").await;

    let err = fx.service.begin_upload(&a1(), chat_id).await.unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn upload_slots_are_limited_per_process() {
    let fx = Fixture::with(
        AttachmentSettings {
            max_concurrent_uploads: 1,
            ..test_settings()
        },
        KillSwitches::default(),
    )
    .await;
    let chat_id = fx.chat().await;

    let held = fx.service.begin_upload(&a1(), chat_id).await.unwrap();
    let err = fx.service.begin_upload(&a1(), chat_id).await.unwrap_err();
    assert!(matches!(
        err,
        DomainError::ServiceUnavailable {
            retry_after_secs: 5
        }
    ));

    drop(held);
    fx.service.begin_upload(&a1(), chat_id).await.unwrap();
}

#[tokio::test]
async fn kill_switches_and_model_capability_apply_to_the_file_part() {
    let fx = Fixture::with(test_settings(), switches(true, true)).await;
    let chat_id = fx.chat().await;
    let session = fx.service.begin_upload(&a1(), chat_id).await.unwrap();

    let image = fx
        .service
        .classify_file(&session, Some("a.png"), "image/png")
        .unwrap_err();
    assert!(matches!(
        image,
        DomainError::FailedPrecondition {
            subject: PreconditionSubject::Images
        }
    ));
    let xlsx = fx
        .service
        .classify_file(&session, Some("a.xlsx"), XLSX_MIME)
        .unwrap_err();
    assert!(is_ci_unavailable(&xlsx));
    let pdf = fx
        .service
        .classify_file(&session, None, "application/pdf")
        .unwrap();
    assert_eq!(pdf.filename, "upload");
    assert!(pdf.for_file_search);

    let fx = Fixture::new().await;
    let text_only = fx.chat_with(TENANT_A, USER_A1, "text-only").await;
    let session = fx.service.begin_upload(&a1(), text_only).await.unwrap();
    assert!(is_ci_unavailable(
        &fx.service
            .classify_file(&session, Some("a.xlsx"), XLSX_MIME)
            .unwrap_err()
    ));
}

#[tokio::test]
async fn oversized_files_are_rejected_before_any_write() {
    let fx = Fixture::with(
        AttachmentSettings {
            document_max_bytes: 8,
            image_max_bytes: 4,
            ..test_settings()
        },
        KillSwitches::default(),
    )
    .await;
    let chat_id = fx.chat().await;

    let session = fx.service.begin_upload(&a1(), chat_id).await.unwrap();
    let image = fx
        .service
        .classify_file(&session, Some("a.png"), "image/png")
        .unwrap();
    assert_eq!(image.max_bytes, 4);
    let err = fx
        .service
        .upload(session, image, Bytes::from_static(b"12345"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DomainError::OutOfRange {
            reason: OutOfRangeReason::FileTooLarge,
            ..
        }
    ));
    assert!(fx.rows(chat_id).await.is_empty());
    assert!(fx.storage.calls().is_empty());

    // Documents have their own limit.
    fx.upload(chat_id, "a.txt", "text/plain", b"12345")
        .await
        .unwrap();
}

#[tokio::test]
async fn upload_into_a_store_of_another_backend_is_provider_mismatch() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    crate::infra::db::repo::vector_stores::insert_placeholder(
        &fx.store.conn(),
        fx.store.ts(),
        &a1_scope(),
        NewVectorStorePlaceholder {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            chat_id,
            provider: "azure-legacy".to_owned(),
            now: at(0),
        },
    )
    .await
    .unwrap();

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(is_provider_mismatch(&err), "{err:?}");
    assert!(fx.rows(chat_id).await.is_empty());
    assert!(fx.storage.calls().is_empty());

    // Images do not use the vector store.
    fx.upload(chat_id, "a.png", "image/png", b"png")
        .await
        .unwrap();
}

// ── Upload: per-chat limits ──────────────────────────────────────────────

#[tokio::test]
async fn document_count_limit_counts_live_documents_only() {
    let fx = Fixture::with(
        AttachmentSettings {
            max_documents_per_chat: 2,
            ..test_settings()
        },
        KillSwitches::default(),
    )
    .await;
    let chat_id = fx.chat().await;
    let first = fx.pdf(chat_id).await.unwrap();
    fx.pdf(chat_id).await.unwrap();
    let uploads = fx.storage.count(|c| matches!(c, Call::Upload { .. }));

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(matches!(
        err,
        DomainError::ResourceExhausted {
            limit: ExhaustedLimit::DocumentLimit
        }
    ));
    assert_eq!(fx.rows(chat_id).await.len(), 2, "the insert is rolled back");
    assert_eq!(
        fx.storage.count(|c| matches!(c, Call::Upload { .. })),
        uploads
    );

    // Images are not documents; deleted documents and other chats do not count.
    fx.upload(chat_id, "a.png", "image/png", b"png")
        .await
        .unwrap();
    fx.service.delete(&a1(), chat_id, first.id).await.unwrap();
    fx.pdf(chat_id).await.unwrap();
    fx.pdf(fx.chat().await).await.unwrap();
}

#[tokio::test]
async fn storage_limit_counts_live_non_failed_bytes_including_images() {
    let fx = Fixture::with(
        AttachmentSettings {
            max_total_upload_bytes_per_chat: 1000,
            ..test_settings()
        },
        KillSwitches::default(),
    )
    .await;
    let chat_id = fx.chat().await;
    fx.upload(chat_id, "a.txt", "text/plain", &[b'a'; 600])
        .await
        .unwrap();

    for (filename, content_type) in [("b.txt", "text/plain"), ("b.png", "image/png")] {
        let err = fx
            .upload(chat_id, filename, content_type, &[b'b'; 401])
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::ResourceExhausted {
                    limit: ExhaustedLimit::StorageLimit
                }
            ),
            "{filename}: {err:?}"
        );
    }
    fx.upload(chat_id, "c.txt", "text/plain", &[b'c'; 400])
        .await
        .unwrap();

    // A failed upload does not count.
    let other = fx.chat().await;
    fx.storage
        .uploads
        .lock()
        .unwrap()
        .push_back(Err(StorageError::Transient("HTTP 500".to_owned())));
    fx.upload(other, "big.txt", "text/plain", &[b'x'; 900])
        .await
        .unwrap_err();
    fx.upload(other, "next.txt", "text/plain", &[b'y'; 900])
        .await
        .unwrap();
}

// ── Upload: provider failures ────────────────────────────────────────────

#[tokio::test]
async fn provider_upload_failure_fails_the_row_with_upload_failed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .uploads
        .lock()
        .unwrap()
        .push_back(Err(StorageError::Transient("HTTP 500".to_owned())));

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(is_storage_unavailable(&err));
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("upload_failed"));
    assert_eq!(rows[0].provider_file_id, None);
    let detail = fx.service.get(&a1(), chat_id, rows[0].id).await.unwrap();
    assert_eq!(detail.status, AttachmentStatus::Failed);
    assert_eq!(detail.error_code.as_deref(), Some("upload_failed"));
}

#[tokio::test]
async fn failed_vector_store_create_removes_the_placeholder() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .creates
        .lock()
        .unwrap()
        .push_back(Err(StorageError::Transient("HTTP 500".to_owned())));

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(is_storage_unavailable(&err));
    assert!(fx.store_rows(chat_id).await.is_empty());
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows[0].error_code.as_deref(), Some("vector_store_failed"));
    let file_id = rows[0].provider_file_id.clone().unwrap();
    eventually("provider file delete", || async {
        fx.storage
            .calls()
            .contains(&Call::DeleteFile(file_id.clone()))
    })
    .await;

    let detail = fx.pdf(chat_id).await.unwrap();
    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 2);
}

#[tokio::test]
async fn failed_add_to_the_vector_store_is_indexing_failed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .adds
        .lock()
        .unwrap()
        .push_back(Err(StorageError::Transient("HTTP 500".to_owned())));

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(is_storage_unavailable(&err));
    let rows = fx.rows(chat_id).await;
    assert_eq!(
        (rows[0].status.as_str(), rows[0].error_code.as_deref()),
        ("failed", Some("indexing_failed"))
    );
    let file_id = rows[0].provider_file_id.clone().unwrap();
    eventually("provider file delete", || async {
        fx.storage
            .calls()
            .contains(&Call::DeleteFile(file_id.clone()))
    })
    .await;
    assert_eq!(fx.storage.count(|c| matches!(c, Call::AddFile { .. })), 1);
}

// ── Upload: indexing wait ────────────────────────────────────────────────

#[tokio::test]
async fn in_progress_indexing_is_polled_until_completed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .adds
        .lock()
        .unwrap()
        .push_back(Ok(VectorStoreFileStatus::InProgress));
    {
        let mut statuses = fx.storage.statuses.lock().unwrap();
        statuses.push_back(Ok(VectorStoreFileStatus::InProgress));
        statuses.push_back(Ok(VectorStoreFileStatus::InProgress));
        statuses.push_back(Ok(VectorStoreFileStatus::Completed));
    }

    let detail = fx.pdf(chat_id).await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::Status(_))), 3);
}

#[tokio::test]
async fn transient_status_read_errors_keep_polling() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .adds
        .lock()
        .unwrap()
        .push_back(Ok(VectorStoreFileStatus::InProgress));
    {
        let mut statuses = fx.storage.statuses.lock().unwrap();
        statuses.push_back(Err(StorageError::Transient("HTTP 500".to_owned())));
        statuses.push_back(Err(StorageError::Transient("gateway".to_owned())));
        statuses.push_back(Ok(VectorStoreFileStatus::Completed));
    }

    let detail = fx.pdf(chat_id).await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::Status(_))), 3);
}

#[tokio::test]
async fn failed_cancelled_unknown_or_rejected_indexing_fails_the_upload() {
    let outcomes = [
        (Ok(VectorStoreFileStatus::Failed), true),
        (Ok(VectorStoreFileStatus::InProgress), false),
        (Ok(VectorStoreFileStatus::Cancelled), true),
        (Ok(VectorStoreFileStatus::Other("paused".to_owned())), true),
    ];
    for (added, failed_on_add) in outcomes {
        let fx = Fixture::new().await;
        let chat_id = fx.chat().await;
        fx.storage.adds.lock().unwrap().push_back(added.clone());
        if !failed_on_add {
            fx.storage
                .statuses
                .lock()
                .unwrap()
                .push_back(Err(StorageError::Rejected("HTTP 400".to_owned())));
        }

        let err = fx.pdf(chat_id).await.unwrap_err();

        assert!(is_storage_unavailable(&err), "{added:?}");
        let rows = fx.rows(chat_id).await;
        assert_eq!(rows[0].status, "failed", "{added:?}");
        assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
        let file_id = rows[0].provider_file_id.clone().unwrap();
        eventually("provider file delete", || async {
            fx.storage
                .calls()
                .contains(&Call::DeleteFile(file_id.clone()))
        })
        .await;
    }
}

#[tokio::test]
async fn indexing_past_the_deadline_returns_uploaded_then_becomes_ready() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    fx.storage
        .set_default_status(VectorStoreFileStatus::InProgress);
    fx.storage
        .adds
        .lock()
        .unwrap()
        .push_back(Ok(VectorStoreFileStatus::InProgress));

    let started = std::time::Instant::now();
    let detail = fx.pdf(chat_id).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    assert!(
        elapsed >= Duration::from_millis(750) && elapsed < Duration::from_millis(1500),
        "{elapsed:?}"
    );
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.provider_file_id.is_some());

    fx.storage
        .set_default_status(VectorStoreFileStatus::Completed);
    eventually("background indexing to finish", || async {
        attachment_row(&fx.store, detail.id).await.status == "ready"
    })
    .await;
    let row = attachment_row(&fx.store, detail.id).await;
    assert!(
        row.updated_at.at() > row.created_at.at(),
        "heartbeat or ready"
    );
    assert_eq!(row.cleanup_status, None);
}

/// Uploads a document whose indexing outlasts the request deadline.
async fn upload_still_indexing(fx: &Fixture) -> (Uuid, AttachmentDetail) {
    let chat_id = fx.chat().await;
    fx.storage
        .set_default_status(VectorStoreFileStatus::InProgress);
    fx.storage
        .adds
        .lock()
        .unwrap()
        .push_back(Ok(VectorStoreFileStatus::InProgress));
    let detail = fx.pdf(chat_id).await.unwrap();
    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    (chat_id, detail)
}

#[tokio::test]
async fn background_indexing_failure_hands_the_file_to_the_cleanup_outbox() {
    let fx = Fixture::new().await;
    let (chat_id, detail) = upload_still_indexing(&fx).await;

    fx.storage.set_default_status(VectorStoreFileStatus::Failed);

    eventually("background failure", || async {
        attachment_row(&fx.store, detail.id).await.status == "failed"
    })
    .await;
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.deleted_at, None);
    let delivered = fx.store.wait_delivered(&Fixture::cleanup_queue(), 1).await;
    assert_eq!(delivered.len(), 1);
    let payload = &delivered[0].payload;
    assert_eq!(payload["event_type"], "attachment_indexing_failed");
    assert_eq!(payload["attachment_id"], detail.id.to_string());
    assert_eq!(payload["chat_id"], chat_id.to_string());
    assert_eq!(payload["provider_file_id"], json!(row.provider_file_id));
    assert_eq!(payload["storage_backend"], "openai");
    assert!(
        !fx.storage
            .calls()
            .iter()
            .any(|call| matches!(call, Call::DeleteFile(_))),
        "no inline delete in the background path"
    );
}

#[tokio::test]
async fn background_indexing_times_out_into_indexing_failed() {
    let fx = Fixture::new().await;
    let (_, detail) = upload_still_indexing(&fx).await;

    eventually("background timeout", || async {
        attachment_row(&fx.store, detail.id).await.status == "failed"
    })
    .await;
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn background_indexing_stops_when_the_attachment_is_deleted() {
    let fx = Fixture::new().await;
    let (chat_id, detail) = upload_still_indexing(&fx).await;

    fx.service.delete(&a1(), chat_id, detail.id).await.unwrap();
    fx.storage
        .set_default_status(VectorStoreFileStatus::Completed);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.status, "uploaded", "never ready once deleted");
    let delivered = fx.store.wait_delivered(&Fixture::cleanup_queue(), 1).await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload["event_type"], "attachment_deleted");
}

#[tokio::test]
async fn background_indexing_stops_on_gear_shutdown() {
    let fx = Fixture::new().await;
    let (_, detail) = upload_still_indexing(&fx).await;

    fx.shutdown.cancel();
    fx.storage.set_default_status(VectorStoreFileStatus::Failed);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.cleanup_status, None);
}

// ── Vector store creation protocol ───────────────────────────────────────

#[tokio::test]
async fn concurrent_first_uploads_create_one_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    *fx.storage.create_delay.lock().unwrap() = Duration::from_millis(150);

    let (first, second) = tokio::join!(fx.pdf(chat_id), fx.pdf(chat_id));

    assert_eq!(first.unwrap().status, AttachmentStatus::Ready);
    assert_eq!(second.unwrap().status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 1);
    let stores = fx.store_rows(chat_id).await;
    assert_eq!(stores.len(), 1);
    let vector_store_id = stores[0].vector_store_id.clone().unwrap();
    assert_eq!(
        fx.storage.count(|c| matches!(
            c,
            Call::AddFile { vector_store_id: id, .. } if *id == vector_store_id
        )),
        2
    );
}

// ── Chat deleted during an upload ────────────────────────────────────────

/// Soft-deletes the chat and claims its live attachments for the chat
/// cleanup (the rows `soft_delete_in_tx` writes).
async fn delete_chat(fx: &Fixture, chat_id: Uuid) {
    let conn = fx.store.conn();
    assert!(
        chat_repo::mark_deleted(
            &conn,
            fx.store.ts(),
            &AccessScope::allow_all(),
            chat_id,
            at(0)
        )
        .await
        .unwrap()
    );
    attachment_repo::mark_chat_for_cleanup(
        &conn,
        fx.store.ts(),
        &AccessScope::allow_all(),
        chat_id,
        at(0),
    )
    .await
    .unwrap();
}

/// One delivery of the chat cleanup of `chat_id` over the fake storage.
async fn run_chat_cleanup(fx: &Fixture, chat_id: Uuid) -> MessageResult {
    let handler = ChatCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        fx.storage.clone(),
        fx.storage.clone(),
        3,
        "mini_chat_test",
    );
    let payload = ChatCleanupPayload {
        tenant_id: TENANT_A,
        chat_id,
        system_request_id: Uuid::new_v4(),
        reason: ChatCleanupReason::ChatSoftDelete,
        chat_deleted_at: at(0),
    };
    handler.process(&payload, 1).await
}

#[tokio::test]
async fn chat_deleted_during_the_provider_upload_keeps_no_provider_file() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    *fx.storage.upload_delay.lock().unwrap() = Duration::from_millis(300);

    let (uploaded, cleanup) = tokio::join!(fx.pdf(chat_id), async {
        eventually("pending row", || async {
            !fx.rows(chat_id).await.is_empty()
        })
        .await;
        delete_chat(&fx, chat_id).await;
        run_chat_cleanup(&fx, chat_id).await
    });

    assert!(matches!(cleanup, MessageResult::Ok), "{cleanup:?}");
    assert!(uploaded.is_err(), "{uploaded:?}");
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "pending");
    assert_eq!(
        rows[0].provider_file_id, None,
        "the claimed row keeps no file"
    );
    assert_eq!(rows[0].cleanup_status.as_deref(), Some("done"));
    eventually("provider file delete", || async {
        fx.storage.count(|c| matches!(c, Call::DeleteFile(_))) == 1
    })
    .await;
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 0);
    assert!(fx.store_rows(chat_id).await.is_empty());
}

#[tokio::test]
async fn chat_deleted_between_provider_upload_and_mark_uploaded_is_chat_not_found() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    *fx.storage.upload_delay.lock().unwrap() = Duration::from_millis(300);

    let (uploaded, ()) = tokio::join!(fx.pdf(chat_id), async {
        eventually("provider upload started", || async {
            fx.storage.count(|c| matches!(c, Call::Upload { .. })) == 1
        })
        .await;
        delete_chat(&fx, chat_id).await;
    });

    // The same race the vector store guard reports as 404 (chat).
    let err = uploaded.unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat), "{err:?}");
    eventually("best-effort provider file delete", || async {
        fx.storage.count(|c| matches!(c, Call::DeleteFile(_))) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fx.storage.count(|c| matches!(c, Call::DeleteFile(_))),
        1,
        "exactly one best-effort file delete"
    );
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 0);
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider_file_id, None);
}

#[tokio::test]
async fn attachment_leaving_pending_on_a_live_chat_stays_internal() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    *fx.storage.upload_delay.lock().unwrap() = Duration::from_millis(300);

    let (uploaded, ()) = tokio::join!(fx.pdf(chat_id), async {
        eventually("provider upload started", || async {
            fx.storage.count(|c| matches!(c, Call::Upload { .. })) == 1
        })
        .await;
        let id = fx.rows(chat_id).await[0].id;
        mark_deleted(&fx, id).await;
    });

    let err = uploaded.unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    eventually("best-effort provider file delete", || async {
        fx.storage.count(|c| matches!(c, Call::DeleteFile(_))) == 1
    })
    .await;
}

#[tokio::test]
async fn deleted_chat_gets_no_new_vector_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    delete_chat(&fx, chat_id).await;
    let upload = Upload {
        scope: a1_scope(),
        chat_id,
        id: Uuid::new_v4(),
        target: StorageTarget {
            backend: "openai".to_owned(),
            tenant_id: TENANT_A,
        },
        started: Instant::now(),
    };

    let err = fx.service.chat_vector_store(&upload).await.unwrap_err();

    assert!(is_not_found(&err, ResourceKind::Chat), "{err:?}");
    assert!(fx.storage.calls().is_empty());
    assert!(fx.store_rows(chat_id).await.is_empty());
}

#[tokio::test]
async fn chat_deleted_during_vector_store_creation_deletes_the_new_store() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    *fx.storage.create_delay.lock().unwrap() = Duration::from_millis(300);

    let (uploaded, ()) = tokio::join!(fx.pdf(chat_id), async {
        eventually("provider store create", || async {
            fx.storage.count(|c| matches!(c, Call::CreateStore(_))) == 1
        })
        .await;
        delete_chat(&fx, chat_id).await;
    });

    let err = uploaded.unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat), "{err:?}");
    assert!(
        fx.store_rows(chat_id).await.is_empty(),
        "placeholder removed"
    );
    eventually("provider store delete", || async {
        fx.storage.count(|c| matches!(c, Call::DeleteStore(_))) == 1
    })
    .await;
    assert_eq!(fx.storage.count(|c| matches!(c, Call::AddFile { .. })), 0);

    // The chat cleanup finds no store left and deletes the uploaded file.
    let cleanup = run_chat_cleanup(&fx, chat_id).await;
    assert!(matches!(cleanup, MessageResult::Ok), "{cleanup:?}");
    assert_eq!(fx.storage.count(|c| matches!(c, Call::DeleteStore(_))), 1);
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows[0].cleanup_status.as_deref(), Some("done"));
}

async fn seed_placeholder(fx: &Fixture, chat_id: Uuid, created_ms_ago: i64) {
    crate::infra::db::repo::vector_stores::insert_placeholder(
        &fx.store.conn(),
        fx.store.ts(),
        &a1_scope(),
        NewVectorStorePlaceholder {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            chat_id,
            provider: "openai".to_owned(),
            now: at(-created_ms_ago),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn stale_placeholder_is_reclaimed() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    seed_placeholder(&fx, chat_id, 200_000).await;

    let detail = fx.pdf(chat_id).await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 1);
    let stores = fx.store_rows(chat_id).await;
    assert_eq!(stores.len(), 1);
    assert!(stores[0].vector_store_id.is_some());
}

#[tokio::test]
async fn loser_gives_up_when_the_store_never_appears() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    seed_placeholder(&fx, chat_id, 1_000).await;

    let err = fx.pdf(chat_id).await.unwrap_err();

    assert!(is_storage_unavailable(&err));
    assert_eq!(fx.storage.count(|c| matches!(c, Call::CreateStore(_))), 0);
    let rows = fx.rows(chat_id).await;
    assert_eq!(rows[0].error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(
        fx.store_rows(chat_id).await.len(),
        1,
        "the live placeholder stays"
    );
}

// ── Get ──────────────────────────────────────────────────────────────────

/// Inserts a pending attachment of `chat_id` uploaded by `uploader`.
async fn seed_upload(fx: &Fixture, chat_id: Uuid, uploader: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    attachment_repo::insert_pending(
        &fx.store.conn(),
        fx.store.ts(),
        &a1_scope(),
        NewAttachment {
            id,
            tenant_id: TENANT_A,
            chat_id,
            uploaded_by_user_id: uploader,
            filename: "seed.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            size_bytes: 10,
            storage_backend: "openai".to_owned(),
            kind: AttachmentKind::Document,
            for_file_search: true,
            for_code_interpreter: false,
            now: at(0),
        },
        ChatUploadLimits {
            max_documents: 100,
            max_total_bytes: u64::MAX,
        },
    )
    .await
    .unwrap();
    id
}

async fn mark_deleted(fx: &Fixture, id: Uuid) {
    attachments::Entity::update_many()
        .col_expr(
            attachments::Column::DeletedAt,
            Expr::value(Some(fx.store.stamp(at(0)))),
        )
        .filter(attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&fx.store.conn())
        .await
        .unwrap();
}

#[tokio::test]
async fn get_returns_only_the_callers_live_attachments_of_the_chat() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let other_chat = fx.chat().await;
    let own = fx.pdf(chat_id).await.unwrap();
    let foreign = seed_upload(&fx, chat_id, USER_A2).await;
    let deleted = seed_upload(&fx, chat_id, USER_A1).await;
    mark_deleted(&fx, deleted).await;

    assert_eq!(fx.service.get(&a1(), chat_id, own.id).await.unwrap(), own);
    for id in [foreign, deleted, Uuid::new_v4()] {
        let err = fx.service.get(&a1(), chat_id, id).await.unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Attachment), "{err:?}");
    }
    let err = fx.service.get(&a1(), other_chat, own.id).await.unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Attachment));
    let err = fx
        .service
        .get(&ctx_of(TENANT_A, USER_A2), chat_id, own.id)
        .await
        .unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat));
}

#[tokio::test]
async fn get_reports_pending_rows_without_error_code() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let id = seed_upload(&fx, chat_id, USER_A1).await;

    let detail = fx.service.get(&a1(), chat_id, id).await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Pending);
    assert_eq!(detail.error_code, None);
    assert_eq!(detail.size_bytes, Some(10));
}

// ── Delete ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_soft_deletes_and_enqueues_one_cleanup_message() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let detail = fx
        .upload(chat_id, "chart.png", "image/png", b"png")
        .await
        .unwrap();
    let provider_file_id = attachment_row(&fx.store, detail.id)
        .await
        .provider_file_id
        .unwrap();

    fx.service.delete(&a1(), chat_id, detail.id).await.unwrap();

    let row = attachment_row(&fx.store, detail.id).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.cleanup_updated_at.is_some());
    let delivered = fx.store.wait_delivered(&Fixture::cleanup_queue(), 1).await;
    assert_eq!(delivered.len(), 1);
    let payload = delivered[0].payload.as_object().unwrap();
    let mut keys: Vec<&str> = payload.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "attachment_id",
            "attachment_kind",
            "chat_id",
            "deleted_at",
            "event_type",
            "provider_file_id",
            "secondary_ref",
            "storage_backend",
            "tenant_id",
            "vector_store_id",
        ]
    );
    assert_eq!(payload["event_type"], "attachment_deleted");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["provider_file_id"], provider_file_id);
    assert_eq!(payload["attachment_kind"], "image");
    assert_eq!(payload["storage_backend"], "openai");
    assert!(payload["vector_store_id"].is_null() && payload["secondary_ref"].is_null());
    let err = fx.service.get(&a1(), chat_id, detail.id).await.unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Attachment));

    // Idempotent: no second message.
    fx.service.delete(&a1(), chat_id, detail.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fx.store.delivered(&Fixture::cleanup_queue()).len(), 1);
}

#[tokio::test]
async fn delete_of_a_referenced_attachment_is_locked() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let detail = fx.pdf(chat_id).await.unwrap();
    let message_id = seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed::new(MessageRole::User, 0),
    )
    .await;
    link_attachment(&fx.store, chat_id, TENANT_A, message_id, detail.id, 0).await;

    let err = fx
        .service
        .delete(&a1(), chat_id, detail.id)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        DomainError::AlreadyExists {
            resource_name: conflict::ATTACHMENT_LOCKED,
            ..
        }
    ));
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!((row.deleted_at, row.cleanup_status), (None, None));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.store.delivered(&Fixture::cleanup_queue()).is_empty());
}

#[tokio::test]
async fn delete_checks_the_uploader_before_idempotency() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let foreign = seed_upload(&fx, chat_id, USER_A2).await;
    let foreign_deleted = seed_upload(&fx, chat_id, USER_A2).await;
    mark_deleted(&fx, foreign_deleted).await;

    for id in [foreign, foreign_deleted, Uuid::new_v4()] {
        let err = fx.service.delete(&a1(), chat_id, id).await.unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Attachment), "{err:?}");
    }
    assert_eq!(attachment_row(&fx.store, foreign).await.deleted_at, None);
    let err = fx
        .service
        .delete(&ctx_of(TENANT_B, USER_B1), chat_id, foreign)
        .await
        .unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.store.delivered(&Fixture::cleanup_queue()).is_empty());
}

#[tokio::test]
async fn delete_of_a_pending_upload_without_provider_file() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let id = seed_upload(&fx, chat_id, USER_A1).await;

    fx.service.delete(&a1(), chat_id, id).await.unwrap();

    let delivered = fx.store.wait_delivered(&Fixture::cleanup_queue(), 1).await;
    assert!(delivered[0].payload["provider_file_id"].is_null());
    assert_eq!(delivered[0].payload["attachment_kind"], "document");
}

#[tokio::test]
async fn delete_locks_the_chat_row_before_it_touches_the_attachment() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let detail = fx
        .upload(chat_id, "chart.png", "image/png", b"png")
        .await
        .unwrap();
    let message_id = seed_message(
        &fx.store,
        chat_id,
        TENANT_A,
        MessageSeed::new(MessageRole::User, 0),
    )
    .await;
    link_attachment(&fx.store, chat_id, TENANT_A, message_id, detail.id, 0).await;
    // The chat is soft-deleted after the service's chat check.
    assert!(
        chat_repo::mark_deleted(&fx.store.conn(), fx.store.ts(), &a1_scope(), chat_id, at(1))
            .await
            .unwrap()
    );

    // Had the attachment update and the reference check run first, the
    // referenced attachment would be `attachment_locked`: the chat row lock
    // is the first statement and finds no live chat.
    let err = AttachmentStore::soft_delete(
        fx.store.repo.as_ref(),
        &a1_scope(),
        chat_id,
        detail.id,
        at(2),
        None,
    )
    .await
    .unwrap_err();

    assert!(is_not_found(&err, ResourceKind::Chat), "{err:?}");
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!((row.deleted_at, row.cleanup_status), (None, None));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.store.delivered(&Fixture::cleanup_queue()).is_empty());
}

#[tokio::test]
async fn delete_chat_row_lock_keeps_the_chat_unchanged() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let detail = fx.pdf(chat_id).await.unwrap();
    let before = chat_row(&fx.store, chat_id).await;

    fx.service.delete(&a1(), chat_id, detail.id).await.unwrap();

    let after = chat_row(&fx.store, chat_id).await;
    assert_eq!(after.updated_at.at(), before.updated_at.at());
    assert!(after.deleted_at.is_none());
    assert!(
        attachment_row(&fx.store, detail.id)
            .await
            .deleted_at
            .is_some()
    );
}

/// The attachment part of the `messages:stream` reserve transaction: the
/// chat row lock (`touch`), the attachment check, then the link. `false`
/// when the attachment is no longer usable (`400 invalid_attachment`).
async fn reserve_link(
    fx: &Fixture,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
) -> Result<bool, DomainError> {
    let repo = Arc::clone(&fx.store.repo);
    let scope = a1_scope();
    fx.store
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move {
                assert!(repo.touch_in_tx(tx, &scope, chat_id, at(1)).await?);
                let rows = repo
                    .find_in_chat_in_tx(tx, &scope, chat_id, &[attachment_id])
                    .await?;
                if !rows
                    .iter()
                    .any(|row| row.id == attachment_id && !row.deleted)
                {
                    return Ok(false);
                }
                repo.link_in_tx(
                    tx,
                    &scope,
                    MessageAttachmentLinks {
                        tenant_id: TENANT_A,
                        chat_id,
                        message_id,
                        attachment_ids: vec![attachment_id],
                        at: at(1),
                    },
                )
                .await?;
                Ok::<_, DomainError>(true)
            })
        })
        .await
}

async fn link_count(fx: &Fixture, attachment_id: Uuid) -> usize {
    crate::infra::db::entities::message_attachments::Entity::find()
        .filter(
            crate::infra::db::entities::message_attachments::Column::AttachmentId.eq(attachment_id),
        )
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&fx.store.conn())
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn concurrent_delete_and_send_never_link_a_deleted_attachment() {
    let fx = Fixture::new().await;
    let chat_id = fx.chat().await;
    let ctx = a1();

    for _ in 0..10 {
        let detail = fx.pdf(chat_id).await.unwrap();
        let message_id = seed_message(
            &fx.store,
            chat_id,
            TENANT_A,
            MessageSeed::new(MessageRole::User, 0),
        )
        .await;

        let (deleted, linked) = tokio::join!(
            fx.service.delete(&ctx, chat_id, detail.id),
            reserve_link(&fx, chat_id, message_id, detail.id),
        );

        let linked = linked.expect("reserve");
        let row = attachment_row(&fx.store, detail.id).await;
        let links = link_count(&fx, detail.id).await;
        if linked {
            // The send committed first: the delete saw the link.
            assert!(
                matches!(
                    deleted,
                    Err(DomainError::AlreadyExists {
                        resource_name: conflict::ATTACHMENT_LOCKED,
                        ..
                    })
                ),
                "{deleted:?}"
            );
            assert_eq!((row.deleted_at, links), (None, 1));
        } else {
            // The delete committed first: the send saw `deleted_at`.
            deleted.expect("delete");
            assert!(row.deleted_at.is_some());
            assert_eq!(links, 0);
        }
    }
}

// ── Secondary copies with several Anthropic entries ──────────────────────

/// Two `anthropic_messages` entries on different upstream aliases; the
/// `claude` catalog model is served by the second one.
fn two_anthropic_entries() -> std::collections::BTreeMap<String, crate::config::ProviderEntry> {
    let entry = |alias: &str| {
        serde_json::from_value(json!({
            "kind": "anthropic_messages",
            "host": "api.anthropic.com",
            "upstream_alias": alias,
            "storage_kind": "openai",
            "rag_provider": "openai",
        }))
        .unwrap()
    };
    std::collections::BTreeMap::from([
        ("claude-first".to_owned(), entry("first-anthropic")),
        ("claude-second".to_owned(), entry("second-anthropic")),
    ])
}

/// A fixture whose service copies images to the Anthropic Files API of
/// [`two_anthropic_entries`] through `gateway`.
async fn secondary_fixture(
    gateway: &Arc<crate::infra::llm::fake_gateway::FakeGateway>,
) -> (
    Fixture,
    Arc<crate::infra::llm::anthropic_files::AnthropicFiles>,
) {
    let files = Arc::new(
        crate::infra::llm::anthropic_files::AnthropicFiles::from_providers(
            Arc::clone(gateway) as _,
            Arc::new(crate::infra::llm::fake_gateway::StaticSystemContext::default()),
            &two_anthropic_entries(),
        )
        .unwrap(),
    );
    let Fixture {
        store,
        storage,
        service,
        shutdown,
    } = Fixture::new().await;
    let service = service.with_secondary(Arc::clone(&files) as _, usize::try_from(MIB).unwrap());
    let fx = Fixture {
        store,
        storage,
        service,
        shutdown,
    };
    (fx, files)
}

/// Uploads an image into `chat_id` whose secondary copy gets `copy_id`.
async fn upload_with_copy(
    fx: &Fixture,
    gateway: &crate::infra::llm::fake_gateway::FakeGateway,
    chat_id: Uuid,
    copy_id: &str,
) -> Uuid {
    gateway.push_proxy(Ok(crate::infra::llm::fake_gateway::json_response(
        200,
        &json!({ "id": copy_id, "type": "file" }),
        &[],
        None,
    )));
    let detail = fx
        .upload(chat_id, "chart.png", "image/png", b"png")
        .await
        .unwrap();
    let row = attachment_row(&fx.store, detail.id).await;
    assert_eq!(row.secondary_file_id.as_deref(), Some(copy_id));
    assert_eq!(row.secondary_status, "uploaded");
    assert_eq!(row.secondary_provider_id.as_deref(), Some("claude-second"));
    detail.id
}

/// The secondary Files API calls the gateway saw, as `METHOD uri`.
fn files_calls(gateway: &crate::infra::llm::fake_gateway::FakeGateway) -> Vec<String> {
    crate::infra::llm::fake_gateway::lock(&gateway.proxied)
        .iter()
        .map(|sent| format!("{} {}", sent.method, sent.uri))
        .collect()
}

fn deleted_response() -> http::Response<oagw_sdk::body::Body> {
    crate::infra::llm::fake_gateway::json_response(
        200,
        &json!({ "id": "deleted", "type": "file_deleted" }),
        &[],
        None,
    )
}

#[tokio::test]
async fn attachment_delete_removes_the_copy_from_the_upload_account() {
    let gateway = Arc::new(crate::infra::llm::fake_gateway::FakeGateway::default());
    let (fx, files) = secondary_fixture(&gateway).await;
    let chat_id = fx.chat_with(TENANT_A, USER_A1, "claude").await;
    let id = upload_with_copy(&fx, &gateway, chat_id, "file_copy_1").await;

    fx.service.delete(&a1(), chat_id, id).await.unwrap();

    let delivered = fx.store.wait_delivered(&Fixture::cleanup_queue(), 1).await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].payload["secondary_ref"],
        json!({
            "file_id": "file_copy_1",
            "provider_kind": "anthropic",
            "upstream_alias": "second-anthropic",
        })
    );
    let payload: crate::infra::outbox::AttachmentCleanupPayload =
        serde_json::from_value(delivered[0].payload.clone()).unwrap();
    gateway.push_proxy(Ok(deleted_response()));
    let handler = crate::infra::outbox_handlers::AttachmentCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        fx.storage.clone(),
        3,
    )
    .with_secondary(files);
    assert!(matches!(handler.process(&payload).await, MessageResult::Ok));

    assert_eq!(
        files_calls(&gateway),
        [
            "POST /second-anthropic/v1/files",
            "DELETE /second-anthropic/v1/files/file_copy_1",
        ]
    );
}

#[tokio::test]
async fn chat_cleanup_removes_the_copy_from_the_upload_account() {
    let gateway = Arc::new(crate::infra::llm::fake_gateway::FakeGateway::default());
    let (fx, files) = secondary_fixture(&gateway).await;
    let chat_id = fx.chat_with(TENANT_A, USER_A1, "claude").await;
    upload_with_copy(&fx, &gateway, chat_id, "file_copy_2").await;
    delete_chat(&fx, chat_id).await;

    gateway.push_proxy(Ok(deleted_response()));
    let handler = ChatCleanupHandler::new(
        Arc::clone(&fx.store.repo),
        fx.storage.clone(),
        fx.storage.clone(),
        3,
        "mini_chat_test",
    )
    .with_secondary(files.clone(), files);
    let payload = ChatCleanupPayload {
        tenant_id: TENANT_A,
        chat_id,
        system_request_id: Uuid::new_v4(),
        reason: ChatCleanupReason::ChatSoftDelete,
        chat_deleted_at: at(0),
    };
    assert!(matches!(
        handler.process(&payload, 1).await,
        MessageResult::Ok
    ));

    assert_eq!(
        files_calls(&gateway),
        [
            "POST /second-anthropic/v1/files",
            "DELETE /second-anthropic/v1/files/file_copy_2",
        ]
    );
}
