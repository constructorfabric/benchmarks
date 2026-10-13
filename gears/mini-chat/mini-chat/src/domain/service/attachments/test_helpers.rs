//! Test helpers shared by the attachment, cleanup and reaper tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc, dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{AttachmentService, UploadTimings};
use crate::domain::error::DomainError;
use crate::domain::service::Deps;
use crate::domain::service::test_support::{FakeStorage, TestEnv};
use crate::infra::db::entity::{attachment, chat, chat_vector_store, message, message_attachment};
use crate::infra::llm::{FileStorage, StorageError, VectorFileStatus};

/// Shortened poll intervals / deadlines.
#[must_use]
pub fn fast_timings() -> UploadTimings {
    UploadTimings {
        request_deadline: Duration::from_millis(300),
        sync_poll_initial: Duration::from_millis(10),
        sync_poll_max: Duration::from_millis(20),
        background_round: Duration::from_millis(100),
        background_total: Duration::from_secs(3),
        background_poll_initial: Duration::from_millis(10),
        background_poll_max: Duration::from_millis(30),
        set_ready_retry: vec![Duration::from_millis(10); 3],
        vs_loser_polls: 3,
        vs_loser_initial: Duration::from_millis(10),
        vs_stale_placeholder: Duration::from_secs(120),
    }
}

/// Storage fake with scripted vector store status reads / add failures; everything else is
/// delegated to the environment's [`FakeStorage`] (which records the calls).
pub struct ScriptStorage {
    pub inner: Arc<FakeStorage>,
    pub add_error: Mutex<Option<StorageError>>,
    pub statuses: Mutex<VecDeque<Result<VectorFileStatus, StorageError>>>,
    pub status_reads: Mutex<u32>,
}

impl ScriptStorage {
    #[must_use]
    pub fn new(inner: Arc<FakeStorage>) -> Self {
        Self {
            inner,
            add_error: Mutex::new(None),
            statuses: Mutex::new(VecDeque::new()),
            status_reads: Mutex::new(0),
        }
    }
}

#[async_trait]
impl FileStorage for ScriptStorage {
    async fn upload_file(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
        filename: &str,
        content_type: &str,
        data: bytes::Bytes,
    ) -> Result<String, StorageError> {
        self.inner
            .upload_file(provider_id, tenant_id, filename, content_type, data)
            .await
    }

    async fn delete_file(&self, p: &str, t: Uuid, f: &str) -> Result<(), StorageError> {
        self.inner.delete_file(p, t, f).await
    }

    async fn create_vector_store(&self, p: &str, t: Uuid, n: &str) -> Result<String, StorageError> {
        self.inner.create_vector_store(p, t, n).await
    }

    async fn add_file_to_vector_store(
        &self,
        p: &str,
        t: Uuid,
        vs: &str,
        f: &str,
        attributes: BTreeMap<String, String>,
    ) -> Result<VectorFileStatus, StorageError> {
        if let Some(e) = self.add_error.lock().clone() {
            return Err(e);
        }
        self.inner.add_file_to_vector_store(p, t, vs, f, attributes).await
    }

    async fn get_vector_store_file_status(
        &self,
        p: &str,
        t: Uuid,
        vs: &str,
        f: &str,
    ) -> Result<VectorFileStatus, StorageError> {
        *self.status_reads.lock() += 1;
        let scripted = self.statuses.lock().pop_front();
        match scripted {
            Some(r) => r,
            None => self.inner.get_vector_store_file_status(p, t, vs, f).await,
        }
    }

    async fn delete_vector_store(&self, p: &str, t: Uuid, vs: &str) -> Result<(), StorageError> {
        self.inner.delete_vector_store(p, t, vs).await
    }
}

/// Copy of the environment's deps with another storage port.
#[must_use]
pub fn deps_with_storage(env: &TestEnv, storage: Arc<dyn FileStorage>) -> Arc<Deps> {
    let d = &env.deps;
    Arc::new(Deps {
        cfg: Arc::clone(&d.cfg),
        db: Arc::clone(&d.db),
        enforcer: d.enforcer.clone(),
        policy: Arc::clone(&d.policy),
        audit: Arc::clone(&d.audit),
        outbox: Arc::clone(&d.outbox),
        llm: Arc::clone(&d.llm),
        storage,
        knowledge: d.knowledge.clone(),
        providers: Arc::clone(&d.providers),
        upload_slots: Arc::clone(&d.upload_slots),
        shutdown: d.shutdown.clone(),
        tasks: d.tasks.clone(),
    })
}

/// Attachment service over the environment with fast timings.
#[must_use]
pub fn fast_service(env: &TestEnv) -> AttachmentService {
    AttachmentService::new(Arc::clone(&env.deps)).with_timings(fast_timings())
}

/// Inserts a chat row.
pub async fn insert_chat(env: &TestEnv, user: Uuid, tenant: Uuid, model: &str) -> Uuid {
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    let am = chat::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        user_id: Set(user),
        model: Set(model.to_owned()),
        title: Set(None),
        is_temporary: Set(false),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat::Entity>(am, &AccessScope::for_tenant(tenant), &conn)
        .await
        .unwrap();
    id
}

/// Soft-deletes a chat and marks its attachments cleanup-pending (what chat deletion does).
pub async fn soft_delete_chat(env: &TestEnv, tenant: Uuid, chat_id: Uuid) {
    let conn = env.deps.db.conn().unwrap();
    let scope = AccessScope::for_tenant(tenant);
    chat::Entity::update_many()
        .col_expr(chat::Column::DeletedAt, Expr::value(Some(OffsetDateTime::now_utc())))
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .exec(&conn)
        .await
        .unwrap();
    attachment::Entity::update_many()
        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .exec(&conn)
        .await
        .unwrap();
}

/// Inserts an attachment row; `f` customizes it.
pub async fn insert_attachment(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    user: Uuid,
    f: impl FnOnce(&mut attachment::ActiveModel),
) -> Uuid {
    let now = OffsetDateTime::now_utc();
    let id = Uuid::new_v4();
    let mut am = attachment::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(user),
        filename: Set("doc.pdf".to_owned()),
        content_type: Set("application/pdf".to_owned()),
        size_bytes: Set(10),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(Some(format!("file-{}", id.simple()))),
        status: Set("ready".to_owned()),
        error_code: Set(None),
        attachment_kind: Set("document".to_owned()),
        for_file_search: Set(true),
        for_code_interpreter: Set(false),
        doc_summary: Set(None),
        img_thumbnail: Set(None),
        img_thumbnail_width: Set(None),
        img_thumbnail_height: Set(None),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
    };
    f(&mut am);
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::for_tenant(tenant), &conn)
        .await
        .unwrap();
    id
}

/// Inserts a vector store row for a chat.
pub async fn insert_vector_store(
    env: &TestEnv,
    tenant: Uuid,
    chat_id: Uuid,
    vs: Option<&str>,
    provider: &str,
    created_at: OffsetDateTime,
) -> Uuid {
    let id = Uuid::new_v4();
    let am = chat_vector_store::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        vector_store_id: Set(vs.map(ToOwned::to_owned)),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(created_at),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(am, &AccessScope::for_tenant(tenant), &conn)
        .await
        .unwrap();
    id
}

/// Links an attachment to a new user message.
pub async fn link_to_message(env: &TestEnv, tenant: Uuid, chat_id: Uuid, attachment_id: Uuid) {
    let now = OffsetDateTime::now_utc();
    let msg_id = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let conn = env.deps.db.conn().unwrap();
    let m = message::ActiveModel {
        id: Set(msg_id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(Some(Uuid::new_v4())),
        role: Set("user".to_owned()),
        content: Set("hi".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(1),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(None),
    };
    secure_insert::<message::Entity>(m, &scope, &conn).await.unwrap();
    let link = message_attachment::ActiveModel {
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        message_id: Set(msg_id),
        attachment_id: Set(attachment_id),
        created_at: Set(now),
    };
    secure_insert::<message_attachment::Entity>(link, &scope, &conn)
        .await
        .unwrap();
}

/// Reads an attachment row (any state).
pub async fn row(env: &TestEnv, tenant: Uuid, id: Uuid) -> attachment::Model {
    let conn = env.deps.db.conn().unwrap();
    attachment::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .and_id(id)
        .unwrap()
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment row")
}

/// Vector store rows of a chat.
pub async fn vector_stores(env: &TestEnv, tenant: Uuid, chat_id: Uuid) -> Vec<chat_vector_store::Model> {
    let conn = env.deps.db.conn().unwrap();
    chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .all(&conn)
        .await
        .unwrap()
}

/// Waits (up to ~5 s) until the row satisfies `pred`.
pub async fn wait_row(
    env: &TestEnv,
    tenant: Uuid,
    id: Uuid,
    pred: impl Fn(&attachment::Model) -> bool,
) -> attachment::Model {
    for _ in 0..250 {
        let r = row(env, tenant, id).await;
        if pred(&r) {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    row(env, tenant, id).await
}

/// Full service upload path (prepare → validate → upload).
pub async fn upload(
    svc: &AttachmentService,
    ctx: &SecurityContext,
    chat_id: Uuid,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> Result<attachment::Model, DomainError> {
    let target = svc.prepare_upload(ctx, chat_id).await?;
    let spec = svc.validate_file(&target, content_type, Some(filename))?;
    svc.upload(ctx, &target, spec, bytes::Bytes::copy_from_slice(data), Instant::now())
        .await
}

/// Storage calls recorded so far with the given prefix.
#[must_use]
pub fn calls(storage: &FakeStorage, prefix: &str) -> Vec<String> {
    storage
        .calls
        .lock()
        .iter()
        .filter(|c| c.starts_with(prefix))
        .cloned()
        .collect()
}

/// Waits (up to ~5 s) until at least `n` storage calls with `prefix` were recorded.
pub async fn wait_calls(storage: &FakeStorage, prefix: &str, n: usize) -> Vec<String> {
    for _ in 0..250 {
        let c = calls(storage, prefix);
        if c.len() >= n {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    calls(storage, prefix)
}

/// Builds a multipart body: `(name, filename, content_type, data)` parts.
#[must_use]
pub fn multipart_body(
    boundary: &str,
    parts: &[(&str, Option<&str>, Option<&str>, &[u8])],
) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, filename, ct, data) in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let mut disp = format!("Content-Disposition: form-data; name=\"{name}\"");
        if let Some(f) = filename {
            disp.push_str(&format!("; filename=\"{f}\""));
        }
        out.extend_from_slice(disp.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = ct {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Small PNG image.
#[must_use]
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
