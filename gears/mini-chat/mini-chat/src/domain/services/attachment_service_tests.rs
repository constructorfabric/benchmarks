#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{AttachmentDeps, AttachmentService, IndexingTiming, UploadedFile};
use crate::config::MiniChatConfig;
use crate::domain::background::Background;
use crate::domain::enums::{AttachmentKind, AttachmentStatus};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{ChatAction, IndexStatus, RagStorage, StorageError};
use crate::domain::services::model_service::ModelService;
use crate::domain::time::{db_now, db_ts};
use crate::infra::db::entities::{attachment, chat, chat_vector_store};
use crate::infra::llm::{ProviderResolver, ResolvedStorage};
use crate::infra::outbox::ATTACHMENT_CLEANUP_PAYLOAD_TYPE;
use crate::test_support::{
    FakeAuthz, FakePolicy, catalog_entry, ctx_for, link_attachment, seed_attachment, seed_message,
    snapshot, test_file_db, test_outbox,
};

const MODEL: &str = "b";
const BACKEND: &str = "secret-provider";
const PDF: &str = "application/pdf";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

// ── Fake storage ─────────────────────────────────────────────────────────────

type StatusResult = Result<IndexStatus, StorageError>;

/// [`RagStorage`] fake: records calls; `add` and every status read pop the
/// next scripted status, then return the sticky `default_status`.
struct FakeStorage {
    calls: Mutex<Vec<String>>,
    upload_error: Mutex<Option<StorageError>>,
    add_error: Mutex<Option<StorageError>>,
    script: Mutex<VecDeque<StatusResult>>,
    default_status: Mutex<StatusResult>,
    create_delay: Mutex<Duration>,
    seq: Mutex<u32>,
    /// When set, `upload_file` signals `upload_started` and waits for it.
    upload_gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    upload_started: tokio::sync::Notify,
}

impl Default for FakeStorage {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            upload_error: Mutex::new(None),
            add_error: Mutex::new(None),
            script: Mutex::new(VecDeque::new()),
            default_status: Mutex::new(Ok(IndexStatus::Completed)),
            create_delay: Mutex::new(Duration::ZERO),
            seq: Mutex::new(0),
            upload_gate: Mutex::new(None),
            upload_started: tokio::sync::Notify::new(),
        }
    }
}

impl FakeStorage {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn calls_starting(&self, prefix: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .collect()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        format!("{prefix}-{seq}")
    }

    fn set_default(&self, s: StatusResult) {
        *self.default_status.lock().unwrap() = s;
    }

    fn script(&self, items: Vec<StatusResult>) {
        self.script.lock().unwrap().extend(items);
    }

    fn next_status(&self) -> StatusResult {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.default_status.lock().unwrap().clone())
    }
}

#[async_trait]
impl RagStorage for FakeStorage {
    async fn upload_file(
        &self,
        st: &ResolvedStorage,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, StorageError> {
        assert_eq!(st.backend_label, BACKEND);
        self.record(format!("upload:{filename}:{content_type}:{}", bytes.len()));
        let gate = self.upload_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.upload_started.notify_one();
            gate.await.unwrap();
        }
        if let Some(e) = self.upload_error.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(self.next_id("file"))
    }

    async fn delete_file(&self, _st: &ResolvedStorage, file_id: &str) -> Result<(), StorageError> {
        self.record(format!("delete_file:{file_id}"));
        Ok(())
    }

    async fn create_vector_store(
        &self,
        _st: &ResolvedStorage,
        _name: &str,
    ) -> Result<String, StorageError> {
        self.record("create_vs".to_owned());
        let delay = *self.create_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(self.next_id("vs"))
    }

    async fn add_file_to_vector_store(
        &self,
        _st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        self.record(format!("add:{vs}:{file_id}:{attachment_id}"));
        if let Some(e) = self.add_error.lock().unwrap().clone() {
            return Err(e);
        }
        self.next_status()
    }

    async fn vector_store_file_status(
        &self,
        _st: &ResolvedStorage,
        vs: &str,
        file_id: &str,
    ) -> Result<IndexStatus, StorageError> {
        self.record(format!("status:{vs}:{file_id}"));
        self.next_status()
    }

    async fn delete_vector_store(
        &self,
        _st: &ResolvedStorage,
        vs: &str,
    ) -> Result<(), StorageError> {
        self.record(format!("delete_vs:{vs}"));
        Ok(())
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

struct Opts {
    cfg: MiniChatConfig,
    catalog: Vec<ModelCatalogEntry>,
    kill: KillSwitches,
    chat_model: String,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            cfg: MiniChatConfig::default(),
            catalog: vec![model_entry(true)],
            kill: snapshot(vec![]).kill_switches,
            chat_model: MODEL.to_owned(),
        }
    }
}

fn model_entry(code_interpreter: bool) -> ModelCatalogEntry {
    let mut m = catalog_entry(MODEL, true);
    m.general_config.tool_support.code_interpreter = code_interpreter;
    m
}

/// Scaled-down waits (paused tokio time does not mix with a real `SQLite`
/// pool: auto-advance fires the pool's acquire timeout). The in-request
/// deadline keeps room for the file database under parallel tests.
fn scaled_timing() -> IndexingTiming {
    let ms = Duration::from_millis;
    IndexingTiming {
        deadline: ms(2500),
        sync_first_wait: ms(25),
        sync_max_wait: ms(200),
        bg_round: ms(400),
        bg_first_wait: ms(5),
        bg_max_wait: ms(100),
        bg_total: ms(8000),
        set_ready_retry: [ms(10), ms(20), ms(40)],
        store_first_wait: ms(20),
    }
}

#[test]
fn default_timing_is_normative() {
    let t = IndexingTiming::default();
    let s = Duration::from_secs;
    assert_eq!(t.deadline, s(25));
    assert_eq!(t.sync_first_wait, Duration::from_millis(250));
    assert_eq!(t.sync_max_wait, s(2));
    assert_eq!(t.bg_round, s(20));
    assert_eq!(t.bg_first_wait, Duration::from_millis(250));
    assert_eq!(t.bg_max_wait, s(5));
    assert_eq!(t.bg_total, s(600));
    assert_eq!(t.set_ready_retry, [s(1), s(2), s(4)]);
}

struct Fx {
    _dir: tempfile::TempDir,
    db: Arc<DBProvider<DomainError>>,
    svc: AttachmentService,
    storage: Arc<FakeStorage>,
    authz: Arc<FakeAuthz>,
    outbox_rx: UnboundedReceiver<(String, Value)>,
    ctx: SecurityContext,
    chat: chat::Model,
    background: Background,
}

async fn fx() -> Fx {
    fx_with(Opts::default()).await
}

async fn fx_with(o: Opts) -> Fx {
    let (dir, raw) = test_file_db().await;
    let (outbox, outbox_rx) = test_outbox(raw.clone()).await;
    let db = Arc::new(DBProvider::new(raw));
    let mut cfg = o.cfg;
    cfg.providers = serde_json::from_value(json!({
        BACKEND: {"kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai"}
    }))
    .unwrap();
    cfg.fill_upstream_aliases();
    let cfg = Arc::new(cfg);
    let mut snap = snapshot(o.catalog);
    snap.kill_switches = o.kill;
    let policy = Arc::new(FakePolicy::new(snap));
    let authz = Arc::new(FakeAuthz::default());
    let storage = Arc::new(FakeStorage::default());
    let background = Background::default();
    let svc = AttachmentService::new(AttachmentDeps {
        db: Arc::clone(&db),
        cfg: Arc::clone(&cfg),
        authz: authz.clone(),
        models: Arc::new(ModelService::new(authz.clone(), policy)),
        resolver: Arc::new(ProviderResolver::new(&cfg)),
        storage: storage.clone(),
        outbox,
        background: background.clone(),
    })
    .with_timing(scaled_timing());
    let ctx = ctx_for(Uuid::new_v4(), Uuid::new_v4());
    let chat = insert_chat(&db, &ctx, &o.chat_model).await;
    Fx {
        _dir: dir,
        db,
        svc,
        storage,
        authz,
        outbox_rx,
        ctx,
        chat,
        background,
    }
}

async fn insert_chat(
    db: &DBProvider<DomainError>,
    ctx: &SecurityContext,
    model: &str,
) -> chat::Model {
    let conn = db.conn().unwrap();
    let now = db_now();
    secure_insert::<chat::Entity>(
        chat::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model.to_owned()),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap()
}

impl Fx {
    fn file(name: &str, content_type: &str, size: usize) -> UploadedFile {
        UploadedFile {
            filename: Some(name.to_owned()),
            content_type: content_type.to_owned(),
            bytes: Bytes::from(vec![b'x'; size]),
        }
    }

    async fn upload(&self, file: UploadedFile) -> Result<super::AttachmentView, DomainError> {
        let plan = self.svc.begin_upload(&self.ctx, self.chat.id).await?;
        self.svc.upload(&self.ctx, plan, file).await
    }

    async fn upload_pdf(&self) -> Result<super::AttachmentView, DomainError> {
        self.upload(Self::file("doc.pdf", PDF, 100)).await
    }

    async fn row(&self, id: Uuid) -> attachment::Model {
        let conn = self.db.conn().unwrap();
        attachment::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .expect("attachment row")
    }

    async fn rows(&self) -> Vec<attachment::Model> {
        let conn = self.db.conn().unwrap();
        attachment::Entity::find()
            .filter(attachment::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    async fn vector_stores(&self) -> Vec<chat_vector_store::Model> {
        let conn = self.db.conn().unwrap();
        chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::ChatId.eq(self.chat.id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    async fn insert_vector_store(
        &self,
        vs: Option<&str>,
        provider: &str,
        created_at: time::OffsetDateTime,
    ) {
        let conn = self.db.conn().unwrap();
        secure_insert::<chat_vector_store::Entity>(
            chat_vector_store::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.chat.tenant_id),
                chat_id: Set(self.chat.id),
                vector_store_id: Set(vs.map(str::to_owned)),
                provider: Set(provider.to_owned()),
                file_count: Set(0),
                created_at: Set(created_at),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    /// Seeds an attachment and overrides size / uploader / deletion.
    async fn seed(
        &self,
        kind: &str,
        status: &str,
        size: i64,
        uploader: Option<Uuid>,
        deleted: bool,
    ) -> attachment::Model {
        let row = seed_attachment(&self.db, &self.chat, kind, status, None).await;
        let id = row.id;
        let mut am = row.into_active_model();
        am.size_bytes = Set(size);
        if let Some(u) = uploader {
            am.uploaded_by_user_id = Set(u);
        }
        if deleted {
            am.deleted_at = Set(Some(db_now()));
        }
        let conn = self.db.conn().unwrap();
        toolkit_db::secure::secure_update_with_scope::<attachment::Entity>(
            am,
            &AccessScope::allow_all(),
            id,
            &conn,
        )
        .await
        .unwrap()
    }

    async fn wait_row(
        &self,
        id: Uuid,
        pred: impl Fn(&attachment::Model) -> bool,
    ) -> attachment::Model {
        for _ in 0..3000 {
            let row = self.row(id).await;
            if pred(&row) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("attachment {id} never reached the expected state");
    }

    /// Attachment cleanup payloads delivered within `window`.
    async fn cleanup_events(&mut self, window: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Some((ty, body))) =
            tokio::time::timeout_at(deadline, self.outbox_rx.recv()).await
        {
            if ty == ATTACHMENT_CLEANUP_PAYLOAD_TYPE {
                out.push(body);
            }
        }
        out
    }
}

fn png(w: u32, h: u32) -> Bytes {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        Rgba([
            u8::try_from(x % 256).unwrap(),
            u8::try_from(y % 256).unwrap(),
            7,
            255,
        ])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    Bytes::from(out.into_inner())
}

fn not_found_chat() -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Chat,
    }
}

fn not_found_attachment() -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Attachment,
    }
}

// ── Upload: documents and indexing ───────────────────────────────────────────

#[tokio::test]
async fn document_upload_indexes_and_is_ready() {
    let fx = fx().await;
    let view = fx.upload_pdf().await.unwrap();

    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(view.kind, AttachmentKind::Document);
    assert_eq!(view.filename, "doc.pdf");
    assert_eq!(view.content_type, PDF);
    assert_eq!(view.size_bytes, 100);
    assert_eq!(view.error_code, None);
    assert_eq!(view.img_thumbnail, None);

    let calls = fx.storage.calls();
    assert_eq!(
        calls,
        vec![
            format!("upload:{}_{}.pdf:{PDF}:100", fx.chat.id, view.id),
            "create_vs".to_owned(),
            format!("add:vs-2:file-1:{}", view.id),
        ]
    );
    let row = fx.row(view.id).await;
    assert_eq!(row.status, "ready");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(row.storage_backend, BACKEND);
    assert_eq!(row.uploaded_by_user_id, fx.ctx.subject_id());
    assert!(row.for_file_search);
    assert!(!row.for_code_interpreter);
    assert_eq!(row.secondary_status, "not_attempted");
    let stores = fx.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs-2"));
    assert_eq!(stores[0].provider, BACKEND);
    assert_eq!(fx.authz.chat_actions(), vec![ChatAction::UploadAttachment]);
}

#[tokio::test]
async fn document_polls_until_completed() {
    let fx = fx().await;
    fx.storage.script(vec![
        Ok(IndexStatus::InProgress),
        Ok(IndexStatus::InProgress),
        Err(StorageError::Transient("502".to_owned())),
    ]);
    let view = fx.upload_pdf().await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.calls_starting("status:").len(), 3);
}

#[tokio::test]
async fn vector_store_created_once_per_chat() {
    let fx = fx().await;
    *fx.storage.create_delay.lock().unwrap() = Duration::from_millis(100);
    let (a, b) = tokio::join!(fx.upload_pdf(), fx.upload_pdf());
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.status, AttachmentStatus::Ready);
    assert_eq!(b.status, AttachmentStatus::Ready);
    let c = fx.upload_pdf().await.unwrap();
    assert_eq!(c.status, AttachmentStatus::Ready);

    assert_eq!(fx.storage.calls_starting("create_vs").len(), 1);
    let adds = fx.storage.calls_starting("add:");
    assert_eq!(adds.len(), 3);
    let vs: Vec<&str> = adds.iter().map(|a| a.split(':').nth(1).unwrap()).collect();
    assert!(vs.iter().all(|v| *v == vs[0]), "{adds:?}");
    assert_eq!(fx.vector_stores().await.len(), 1);
}

#[tokio::test]
async fn stale_placeholder_is_reclaimed() {
    let fx = fx().await;
    let old = db_ts(db_now() - time::Duration::seconds(121));
    fx.insert_vector_store(None, BACKEND, old).await;
    let view = fx.upload_pdf().await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(fx.storage.calls_starting("create_vs").len(), 1);
    let stores = fx.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert!(stores[0].vector_store_id.is_some());
}

#[tokio::test]
async fn fresh_placeholder_without_store_is_503() {
    let fx = fx().await;
    fx.insert_vector_store(None, BACKEND, db_now()).await;
    let err = fx.upload_pdf().await.unwrap_err();
    assert_eq!(err, DomainError::StorageUnavailable);
    assert!(fx.storage.calls_starting("create_vs").is_empty());
    let row = &fx.rows().await[0];
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
}

#[tokio::test]
async fn indexing_failure_marks_failed_and_503() {
    let fx = fx().await;
    fx.storage
        .script(vec![Ok(IndexStatus::InProgress), Ok(IndexStatus::Failed)]);
    let err = fx.upload_pdf().await.unwrap_err();
    assert_eq!(err, DomainError::StorageUnavailable);
    let row = &fx.rows().await[0];
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status, None);
    // Best-effort provider delete (fire-and-forget).
    for _ in 0..100 {
        if !fx.storage.calls_starting("delete_file:").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fx.storage.calls_starting("delete_file:"),
        ["delete_file:file-1"]
    );

    // A non-transient status read error and a failed add do the same.
    fx.storage.script(vec![
        Ok(IndexStatus::InProgress),
        Err(StorageError::Failed("bad".to_owned())),
    ]);
    assert_eq!(
        fx.upload_pdf().await.unwrap_err(),
        DomainError::StorageUnavailable
    );
    *fx.storage.add_error.lock().unwrap() = Some(StorageError::Transient("503".to_owned()));
    assert_eq!(
        fx.upload_pdf().await.unwrap_err(),
        DomainError::StorageUnavailable
    );
    let rows = fx.rows().await;
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| r.status == "failed" && r.error_code.as_deref() == Some("indexing_failed"))
    );
}

#[tokio::test]
async fn provider_upload_failure_marks_failed_and_503() {
    let fx = fx().await;
    *fx.storage.upload_error.lock().unwrap() = Some(StorageError::Transient("500".to_owned()));
    let err = fx.upload_pdf().await.unwrap_err();
    assert_eq!(err, DomainError::StorageUnavailable);
    let row = &fx.rows().await[0];
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(row.provider_file_id, None);
    assert_eq!(fx.storage.calls().len(), 1);
}

#[tokio::test]
async fn indexing_timeout_returns_uploaded_then_background_ready() {
    let fx = fx().await;
    fx.storage.set_default(Ok(IndexStatus::InProgress));
    let start = tokio::time::Instant::now();
    let view = fx.upload_pdf().await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(view.status, AttachmentStatus::Uploaded);
    assert!(
        elapsed >= Duration::from_millis(2500) && elapsed <= Duration::from_millis(3500),
        "{elapsed:?}"
    );
    // 25 ms doubling to 200 ms within 2.5 s: ~14 reads after the add.
    let reads = fx.storage.calls_starting("status:").len();
    assert!((6..=20).contains(&reads), "{reads}");
    let row = fx.row(view.id).await;
    assert_eq!(row.status, "uploaded");
    let heartbeat_before = row.updated_at;

    // Background task: still in progress for a while (heartbeat refreshes
    // updated_at), then completed.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let row = fx.row(view.id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.updated_at > heartbeat_before);
    fx.storage.set_default(Ok(IndexStatus::Completed));
    let row = fx.wait_row(view.id, |r| r.status == "ready").await;
    assert_eq!(row.error_code, None);
    assert_eq!(row.cleanup_status, None);
    fx.background.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn background_failure_enqueues_cleanup() {
    let mut fx = fx().await;
    fx.storage.set_default(Ok(IndexStatus::InProgress));
    let view = fx.upload_pdf().await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Uploaded);
    fx.storage.set_default(Ok(IndexStatus::Failed));
    let row = fx.wait_row(view.id, |r| r.status == "failed").await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.deleted_at.is_none());
    // No inline delete: the outbox cleanup owns the provider file.
    assert!(fx.storage.calls_starting("delete_file:").is_empty());

    let events = fx.cleanup_events(Duration::from_secs(3)).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let ev = &events[0];
    assert_eq!(ev["event_type"], "attachment_indexing_failed");
    assert_eq!(ev["attachment_id"], view.id.to_string());
    assert_eq!(ev["chat_id"], fx.chat.id.to_string());
    assert_eq!(ev["tenant_id"], fx.chat.tenant_id.to_string());
    assert_eq!(ev["provider_file_id"], "file-1");
    assert_eq!(ev["storage_backend"], BACKEND);
    assert_eq!(ev["attachment_kind"], "document");
    assert_eq!(ev["vector_store_id"], Value::Null);
    assert_eq!(ev["secondary_ref"], Value::Null);
}

#[tokio::test]
async fn background_times_out_after_ten_minutes() {
    let mut fx = fx().await;
    fx.storage.set_default(Ok(IndexStatus::InProgress));
    let view = fx.upload_pdf().await.unwrap();
    let after_upload = tokio::time::Instant::now();
    let row = fx.wait_row(view.id, |r| r.status == "failed").await;
    let waited = after_upload.elapsed();
    assert!(
        waited >= Duration::from_millis(7900) && waited <= Duration::from_millis(10_000),
        "{waited:?}"
    );
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(fx.cleanup_events(Duration::from_secs(3)).await.len(), 1);
}

#[tokio::test]
async fn background_stops_without_changes_when_row_is_deleted() {
    let mut fx = fx().await;
    fx.storage.set_default(Ok(IndexStatus::InProgress));
    let view = fx.upload_pdf().await.unwrap();
    fx.svc.delete(&fx.ctx, fx.chat.id, view.id).await.unwrap();
    let deleted = fx.row(view.id).await;
    fx.storage.set_default(Ok(IndexStatus::Completed));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let row = fx.row(view.id).await;
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.updated_at, deleted.updated_at);
    let events = fx.cleanup_events(Duration::from_secs(3)).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "attachment_deleted");
}

#[tokio::test]
async fn background_task_observes_cancellation() {
    let fx = fx().await;
    fx.storage.set_default(Ok(IndexStatus::InProgress));
    let view = fx.upload_pdf().await.unwrap();
    fx.background.shutdown(Duration::from_secs(5)).await;
    assert!(fx.background.tracker.is_empty());
    fx.storage.set_default(Ok(IndexStatus::Completed));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(fx.row(view.id).await.status, "uploaded");
}

// ── Upload: purposes, kinds, kill switches ───────────────────────────────────

#[tokio::test]
async fn xlsx_rejected_when_code_interpreter_disabled() {
    let mut kill = snapshot(vec![]).kill_switches;
    kill.disable_code_interpreter = true;
    let fx = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let err = fx.upload(Fx::file("t.xlsx", XLSX, 10)).await.unwrap_err();
    assert_eq!(err, DomainError::CodeInterpreterUnavailable);
    assert!(fx.storage.calls().is_empty());
    assert!(fx.rows().await.is_empty());

    // Model without code interpreter support: same.
    let fx = fx_with(Opts {
        catalog: vec![model_entry(false)],
        ..Opts::default()
    })
    .await;
    let err = fx.upload(Fx::file("t.xlsx", XLSX, 10)).await.unwrap_err();
    assert_eq!(err, DomainError::CodeInterpreterUnavailable);
}

#[tokio::test]
async fn xlsx_is_ready_for_code_interpreter_without_vector_store() {
    let fx = fx().await;
    let view = fx.upload(Fx::file("t.xlsx", XLSX, 10)).await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(view.kind, AttachmentKind::Document);
    let row = fx.row(view.id).await;
    assert!(row.for_code_interpreter);
    assert!(!row.for_file_search);
    assert_eq!(
        fx.storage.calls(),
        [format!("upload:{}_{}.xlsx:{XLSX}:10", fx.chat.id, view.id)]
    );
    assert!(fx.vector_stores().await.is_empty());
}

#[tokio::test]
async fn image_rejected_when_images_disabled() {
    let mut kill = snapshot(vec![]).kill_switches;
    kill.disable_images = true;
    let fx = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let err = fx
        .upload(Fx::file("a.png", "image/png", 10))
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::FeatureDisabled { subject: "images" });
    assert!(fx.storage.calls().is_empty());
    assert!(fx.rows().await.is_empty());
}

#[tokio::test]
async fn image_upload_has_thumbnail_and_no_vector_store() {
    let fx = fx().await;
    let bytes = png(400, 200);
    let len = bytes.len();
    let view = fx
        .upload(UploadedFile {
            filename: Some("pic.png".to_owned()),
            content_type: "image/png".to_owned(),
            bytes,
        })
        .await
        .unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(view.kind, AttachmentKind::Image);
    let thumb = view.img_thumbnail.expect("thumbnail");
    assert_eq!(thumb.content_type, "image/webp");
    assert_eq!((thumb.width, thumb.height), (128, 64));
    assert_eq!(&thumb.data[8..12], b"WEBP");
    assert_eq!(
        fx.storage.calls(),
        [format!(
            "upload:{}_{}.png:image/png:{len}",
            fx.chat.id, view.id
        )]
    );
    assert!(fx.vector_stores().await.is_empty());
    let row = fx.row(view.id).await;
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.img_thumbnail_width, Some(128));

    // An undecodable image is still ready, without a thumbnail.
    let view = fx
        .upload(Fx::file("broken.jpg", "image/jpeg", 50))
        .await
        .unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
    assert_eq!(view.img_thumbnail, None);
    assert_eq!(fx.row(view.id).await.error_code, None);
}

// ── Upload: limits ───────────────────────────────────────────────────────────

#[tokio::test]
async fn document_limit_429() {
    let mut cfg = MiniChatConfig::default();
    cfg.rag.max_documents_per_chat = 2;
    let fx = fx_with(Opts {
        cfg,
        ..Opts::default()
    })
    .await;
    fx.seed("document", "ready", 10, None, false).await;
    fx.seed("document", "failed", 10, None, false).await;
    fx.seed("document", "ready", 10, None, true).await;
    fx.seed("image", "ready", 10, None, false).await;

    fx.upload_pdf().await.unwrap();
    let uploads_before = fx.storage.calls_starting("upload:").len();
    let err = fx.upload_pdf().await.unwrap_err();
    assert_eq!(err, DomainError::DocumentLimit);
    assert_eq!(fx.storage.calls_starting("upload:").len(), uploads_before);
    assert_eq!(fx.rows().await.len(), 5, "the rejected row is rolled back");

    // Images do not count against the document limit.
    let view = fx.upload(Fx::file("a.png", "image/png", 10)).await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
}

#[tokio::test]
async fn storage_limit_counts_images_and_excludes_failed() {
    let mut cfg = MiniChatConfig::default();
    cfg.rag.max_total_upload_mb_per_chat = 1;
    let fx = fx_with(Opts {
        cfg,
        ..Opts::default()
    })
    .await;
    fx.seed("image", "ready", 600_000, None, false).await;
    fx.seed("document", "failed", 900_000, None, false).await;
    fx.seed("document", "ready", 900_000, None, true).await;

    // 600_000 + 448_576 = 1 MiB exactly: allowed.
    fx.upload(Fx::file("a.txt", "text/plain", 448_576))
        .await
        .unwrap();
    let err = fx
        .upload(Fx::file("b.txt", "text/plain", 1))
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::StorageLimit);
    let err = fx
        .upload(Fx::file("c.png", "image/png", 1))
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::StorageLimit);
    assert_eq!(fx.rows().await.len(), 4);
}

#[tokio::test]
async fn oversize_file_is_too_large() {
    let fx = fx().await;
    let plan = fx.svc.begin_upload(&fx.ctx, fx.chat.id).await.unwrap();
    // min(rag 25 600 KiB, model 25 MiB) for documents; 5 120 KiB for images.
    assert_eq!(plan.doc_limit_bytes, 25 * 1024 * 1024);
    assert_eq!(plan.image_limit_bytes, 5120 * 1024);
    assert_eq!(plan.size_limit(Some("a.png"), "image/png"), 5120 * 1024);
    assert_eq!(
        plan.size_limit(Some("a.png"), "application/octet-stream"),
        5120 * 1024
    );
    assert_eq!(plan.size_limit(Some("a.pdf"), PDF), 25 * 1024 * 1024);
    let err = fx
        .svc
        .upload(
            &fx.ctx,
            plan,
            Fx::file("a.png", "image/png", 5120 * 1024 + 1),
        )
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::FileTooLarge);
    assert!(fx.rows().await.is_empty());

    let mut small = model_entry(true);
    small.general_config.max_file_size_mb = 1;
    let fx = fx_with(Opts {
        catalog: vec![small],
        ..Opts::default()
    })
    .await;
    let plan = fx.svc.begin_upload(&fx.ctx, fx.chat.id).await.unwrap();
    assert_eq!(plan.doc_limit_bytes, 1024 * 1024);
    assert_eq!(plan.image_limit_bytes, 1024 * 1024);
}

#[tokio::test]
async fn upload_concurrency_limit_is_503() {
    let mut cfg = MiniChatConfig::default();
    cfg.rag.max_concurrent_uploads = 1;
    let fx = fx_with(Opts {
        cfg,
        ..Opts::default()
    })
    .await;
    let plan = fx.svc.begin_upload(&fx.ctx, fx.chat.id).await.unwrap();
    let err = fx
        .svc
        .begin_upload(&fx.ctx, fx.chat.id)
        .await
        .err()
        .unwrap();
    assert_eq!(err, DomainError::UploadConcurrency);
    drop(plan);
    assert!(fx.svc.begin_upload(&fx.ctx, fx.chat.id).await.is_ok());
}

// ── Upload: content types and filenames ──────────────────────────────────────

#[tokio::test]
async fn unsupported_type_400() {
    let mut cfg = MiniChatConfig::default();
    cfg.rag.allow_csv_upload = false;
    let fx = fx_with(Opts {
        cfg,
        ..Opts::default()
    })
    .await;
    for (name, ct) in [
        ("setup.exe", "application/x-msdownload"),
        ("setup.exe", "application/octet-stream"),
        ("noext", "application/octet-stream"),
        ("data.csv", "text/csv"),
        ("data.csv", "application/octet-stream"),
        ("pic.bmp", "image/bmp"),
    ] {
        let err = fx.upload(Fx::file(name, ct, 10)).await.unwrap_err();
        assert_eq!(err, DomainError::UnsupportedContentType, "{name} {ct}");
    }
    assert!(fx.storage.calls().is_empty());
    assert!(fx.rows().await.is_empty());
}

#[tokio::test]
async fn octet_stream_inferred_from_extension() {
    let fx = fx().await;
    for (name, ct, want, ext) in [
        ("Report.PDF", "application/octet-stream", PDF, "pdf"),
        ("t.xlsx", "application/octet-stream", XLSX, "xlsx"),
        ("p.jpeg", "application/octet-stream", "image/jpeg", "jpg"),
        ("main.rs", "application/octet-stream", "text/x-rust", "rs"),
        ("data.csv", "text/csv", "text/plain", "txt"),
        ("data.csv", "application/octet-stream", "text/plain", "txt"),
        (
            "notes.md",
            "text/markdown; charset=utf-8",
            "text/markdown",
            "md",
        ),
        ("a.txt", "Text/Plain; charset=UTF-8", "text/plain", "txt"),
    ] {
        let view = fx.upload(Fx::file(name, ct, 10)).await.unwrap();
        assert_eq!(view.content_type, want, "{name} {ct}");
        assert_eq!(view.filename, name);
        let upload = fx.storage.calls_starting("upload:").pop().unwrap();
        assert_eq!(
            upload,
            format!("upload:{}_{}.{ext}:{want}:10", fx.chat.id, view.id),
            "{name}"
        );
    }
}

#[tokio::test]
async fn filename_defaults_and_is_truncated_keeping_extension() {
    let fx = fx().await;
    let view = fx
        .upload(UploadedFile {
            filename: None,
            content_type: PDF.to_owned(),
            bytes: Bytes::from_static(b"pdf"),
        })
        .await
        .unwrap();
    assert_eq!(view.filename, "upload");

    let long = format!("{}.pdf", "\u{e9}".repeat(300));
    let view = fx.upload(Fx::file(&long, PDF, 3)).await.unwrap();
    assert_eq!(view.filename.chars().count(), 255);
    assert!(view.filename.ends_with("\u{e9}.pdf"));
}

// ── Upload: provider mismatch, model, chat ───────────────────────────────────

#[tokio::test]
async fn provider_mismatch_409() {
    let fx = fx().await;
    fx.insert_vector_store(Some("vs-other"), "other-backend", db_now())
        .await;
    let err = fx.upload_pdf().await.unwrap_err();
    assert_eq!(err, DomainError::ProviderMismatch);
    assert!(fx.storage.calls_starting("add:").is_empty());
    assert!(fx.storage.calls_starting("create_vs").is_empty());

    // Images and code-interpreter files never use the vector store.
    let view = fx.upload(Fx::file("a.png", "image/png", 10)).await.unwrap();
    assert_eq!(view.status, AttachmentStatus::Ready);
}

#[tokio::test]
async fn chat_model_removed_is_invalid_model_before_body() {
    let fx = fx_with(Opts {
        chat_model: "gone".to_owned(),
        ..Opts::default()
    })
    .await;
    let err = fx
        .svc
        .begin_upload(&fx.ctx, fx.chat.id)
        .await
        .err()
        .unwrap();
    assert_eq!(err, DomainError::InvalidModel);
    assert!(fx.storage.calls().is_empty());

    // A disabled model still serves uploads (no enabled filter).
    let fx = fx_with(Opts {
        catalog: vec![catalog_entry(MODEL, false)],
        ..Opts::default()
    })
    .await;
    assert!(fx.svc.begin_upload(&fx.ctx, fx.chat.id).await.is_ok());
}

#[tokio::test]
async fn upload_to_unknown_or_foreign_chat_is_404() {
    let fx = fx().await;
    let chat_404 = DomainError::NotFound {
        resource: ResourceKind::Chat,
    };
    let err = fx
        .svc
        .begin_upload(&fx.ctx, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    assert_eq!(err, chat_404);
    let other = ctx_for(fx.chat.tenant_id, Uuid::new_v4());
    let err = fx.svc.begin_upload(&other, fx.chat.id).await.err().unwrap();
    assert_eq!(err, chat_404);
}

// ── Get ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_own_attachment() {
    let fx = fx().await;
    let view = fx.upload_pdf().await.unwrap();
    let got = fx.svc.get(&fx.ctx, fx.chat.id, view.id).await.unwrap();
    assert_eq!(got, view);

    let failed = fx.seed("document", "failed", 5, None, false).await;
    let failed_id = failed.id;
    let mut am = failed.into_active_model();
    am.error_code = Set(Some("upload_failed".to_owned()));
    let conn = fx.db.conn().unwrap();
    let failed = toolkit_db::secure::secure_update_with_scope::<attachment::Entity>(
        am,
        &AccessScope::allow_all(),
        failed_id,
        &conn,
    )
    .await
    .unwrap();
    let got = fx.svc.get(&fx.ctx, fx.chat.id, failed.id).await.unwrap();
    assert_eq!(got.status, AttachmentStatus::Failed);
    assert_eq!(got.error_code.as_deref(), Some("upload_failed"));
    assert!(
        fx.authz
            .chat_actions()
            .contains(&ChatAction::ReadAttachment)
    );
}

#[tokio::test]
async fn get_other_uploader_404() {
    let fx = fx().await;
    let foreign = fx
        .seed("document", "ready", 5, Some(Uuid::new_v4()), false)
        .await;
    let deleted = fx.seed("document", "ready", 5, None, true).await;
    for id in [foreign.id, deleted.id, Uuid::new_v4()] {
        let err = fx.svc.get(&fx.ctx, fx.chat.id, id).await.unwrap_err();
        assert_eq!(err, not_found_attachment());
    }
    // Attachment of another chat of the same user.
    let other_chat = insert_chat(&fx.db, &fx.ctx, MODEL).await;
    let own = fx.seed("document", "ready", 5, None, false).await;
    let err = fx
        .svc
        .get(&fx.ctx, other_chat.id, own.id)
        .await
        .unwrap_err();
    assert_eq!(err, not_found_attachment());
}

// ── Delete ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_referenced_is_locked() {
    let fx = fx().await;
    let att = fx.seed("document", "ready", 5, None, false).await;
    let msg = seed_message(&fx.db, &fx.chat, "user", db_now()).await;
    link_attachment(&fx.db, &msg, att.id).await;
    let err = fx
        .svc
        .delete(&fx.ctx, fx.chat.id, att.id)
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::AttachmentLocked);
    let row = fx.row(att.id).await;
    assert!(row.deleted_at.is_none());
    assert!(row.cleanup_status.is_none());
}

#[tokio::test]
async fn delete_twice_is_204_single_outbox_row() {
    let mut fx = fx().await;
    let att = fx.seed("image", "ready", 5, None, false).await;
    fx.svc.delete(&fx.ctx, fx.chat.id, att.id).await.unwrap();
    fx.svc.delete(&fx.ctx, fx.chat.id, att.id).await.unwrap();

    let row = fx.row(att.id).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let events = fx.cleanup_events(Duration::from_secs(3)).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let ev = &events[0];
    assert_eq!(ev["event_type"], "attachment_deleted");
    assert_eq!(ev["attachment_id"], att.id.to_string());
    assert_eq!(ev["provider_file_id"], "file-secret");
    assert_eq!(ev["storage_backend"], "openai");
    assert_eq!(ev["attachment_kind"], "image");
    assert_eq!(ev["secondary_ref"], Value::Null);
    assert!(
        fx.authz
            .chat_actions()
            .iter()
            .all(|a| *a == ChatAction::DeleteAttachment)
    );
}

#[tokio::test]
async fn delete_other_uploader_or_unknown_is_404() {
    let fx = fx().await;
    let foreign = fx
        .seed("document", "ready", 5, Some(Uuid::new_v4()), false)
        .await;
    // Another uploader's already-deleted attachment: 404, not 204.
    let foreign_deleted = fx
        .seed("document", "ready", 5, Some(Uuid::new_v4()), true)
        .await;
    for id in [foreign.id, foreign_deleted.id, Uuid::new_v4()] {
        let err = fx.svc.delete(&fx.ctx, fx.chat.id, id).await.unwrap_err();
        assert_eq!(err, not_found_attachment());
    }
    assert!(fx.row(foreign.id).await.deleted_at.is_none());
}

// ── Delete during the provider upload ────────────────────────────────────────

impl Fx {
    /// Runs an upload whose provider call blocks until `during` finished.
    async fn upload_racing<F: std::future::Future<Output = ()>>(
        &self,
        file: UploadedFile,
        during: F,
    ) -> Result<super::AttachmentView, DomainError> {
        let (release, gate) = tokio::sync::oneshot::channel();
        *self.storage.upload_gate.lock().unwrap() = Some(gate);
        let (res, ()) = tokio::join!(Box::pin(self.upload(file)), async {
            self.storage.upload_started.notified().await;
            during.await;
            release.send(()).unwrap();
        });
        res
    }
}

#[tokio::test]
async fn chat_deleted_during_upload_deletes_provider_file_and_fails() {
    let fx = fx().await;
    let err = fx
        .upload_racing(Fx::file("doc.pdf", PDF, 100), async {
            let conn = fx.db.conn().unwrap();
            let now = db_now();
            crate::infra::db::repos::chat_repo::soft_delete(
                &conn,
                &AccessScope::allow_all(),
                fx.chat.id,
                now,
            )
            .await
            .unwrap();
            crate::infra::db::repos::attachment_repo::mark_chat_cleanup_pending(
                &conn,
                fx.chat.tenant_id,
                fx.chat.id,
                now,
            )
            .await
            .unwrap();
        })
        .await
        .unwrap_err();
    assert_eq!(err, not_found_chat());
    let row = &fx.rows().await[0];
    assert_eq!(row.status, "pending", "never marked uploaded");
    assert_eq!(row.provider_file_id, None);
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        fx.storage.calls_starting("delete_file:"),
        vec!["delete_file:file-1"]
    );
    assert!(fx.storage.calls_starting("create_vs").is_empty());
}

#[tokio::test]
async fn attachment_deleted_during_upload_deletes_provider_file_and_fails() {
    let fx = fx().await;
    let err = fx
        .upload_racing(
            UploadedFile {
                filename: Some("pic.png".to_owned()),
                content_type: "image/png".to_owned(),
                bytes: png(8, 8),
            },
            async {
                let row = fx.rows().await.remove(0);
                fx.svc.delete(&fx.ctx, fx.chat.id, row.id).await.unwrap();
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err, not_found_attachment());
    let row = &fx.rows().await[0];
    assert!(row.deleted_at.is_some());
    assert_eq!(row.provider_file_id, None);
    assert_eq!(
        fx.storage.calls_starting("delete_file:"),
        vec!["delete_file:file-1"]
    );
}
