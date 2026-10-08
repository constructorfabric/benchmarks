//! Attachments: upload, indexing, get, delete (DESIGN §3.3, §3.6 "File Upload").

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::chats::load_chat;
use super::stream::chat_model;
use super::{AppServices, now, policy};
use crate::domain::error::{DomainError, NotFoundKind};
use crate::infra::db::entity::{attachments, chat_vector_stores, chats, message_attachments};
use crate::infra::llm::transport::{StorageError, StorageTarget, resolve_storage};
use crate::infra::outbox::Wakes;
use crate::infra::thumbnail::make_thumbnail;

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const DOC_TYPES: &[&str] = &[
    "application/pdf",
    DOCX,
    PPTX,
    XLSX,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-script.python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "application/typescript",
    "text/x-typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "text/x-sql",
];

const INDEX_DEADLINE: Duration = Duration::from_secs(25);
const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
const PLACEHOLDER_STALE_SECS: i64 = 120;

/// Infers a MIME type from a filename extension.
#[must_use]
pub fn mime_from_filename(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => DOCX,
        "pptx" => PPTX,
        "xlsx" => XLSX,
        "txt" | "text" | "log" => "text/plain",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" => "text/javascript",
        "ts" => "application/typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Normalizes and validates a content type; returns `(mime, kind)`.
///
/// # Errors
/// `UnsupportedContentType`.
pub fn classify_content_type(raw: &str, filename: &str, allow_csv: bool) -> Result<(String, &'static str), DomainError> {
    let mut mime = raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime == "application/octet-stream" || mime.is_empty() {
        mime = mime_from_filename(filename).unwrap_or("application/octet-stream").to_owned();
    }
    if mime == "image/jpg" {
        mime = "image/jpeg".to_owned();
    }
    if mime == "text/csv" {
        if allow_csv {
            mime = "text/plain".to_owned();
        } else {
            return Err(DomainError::UnsupportedContentType(mime));
        }
    }
    if IMAGE_TYPES.contains(&mime.as_str()) {
        return Ok((mime, "image"));
    }
    if DOC_TYPES.contains(&mime.as_str()) {
        return Ok((mime, "document"));
    }
    Err(DomainError::UnsupportedContentType(mime))
}

/// Sanitized filename: defaults to `upload`, max 255 chars keeping the extension.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    let name = raw
        .map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "upload".to_owned());
    if name.chars().count() <= 255 {
        return name;
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{ext}", stem.chars().take(keep).collect::<String>())
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Attachment detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentView {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    pub error_code: Option<String>,
    pub img_thumbnail: Option<super::messages::ThumbnailView>,
    pub created_at: DateTime<Utc>,
}

impl From<&attachments::Model> for AttachmentView {
    fn from(a: &attachments::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: a.status.clone(),
            kind: a.attachment_kind.clone(),
            error_code: if a.status == "failed" { a.error_code.clone() } else { None },
            img_thumbnail: super::messages::thumbnail_of(a),
            created_at: a.created_at,
        }
    }
}

/// Secondary (Anthropic) file reference in cleanup payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup outbox payload (DESIGN §4 "Attachment Deletion").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    pub deleted_at: DateTime<Utc>,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Facts resolved before the upload body is read.
pub struct UploadContext {
    pub chat: chats::Model,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
    pub storage: StorageTarget,
}

impl UploadContext {
    /// Byte limit for a kind: `min(rag limit, model max_file_size_mb)`.
    #[must_use]
    pub fn limit_bytes(&self, kind: &str, rag: &crate::config::RagConfig) -> u64 {
        let kb = if kind == "image" {
            rag.uploaded_image_max_size_kb
        } else {
            rag.uploaded_file_max_size_kb
        };
        let mut limit = u64::from(kb) * 1024;
        let model_mb = self.model.general_config.max_file_size_mb;
        if model_mb > 0 {
            limit = limit.min(u64::from(model_mb) * 1024 * 1024);
        }
        limit
    }

    /// Whether code interpreter is usable for uploads in this chat.
    #[must_use]
    pub fn code_interpreter_available(&self) -> bool {
        !self.kill_switches.disable_code_interpreter && self.model.general_config.tool_support.code_interpreter
    }
}

async fn load_attachment_any(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<attachments::Model>, DomainError> {
    Ok(attachments::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(attachments::Column::Id.eq(id))
                .add(attachments::Column::ChatId.eq(chat_id)),
        )
        .one(runner)
        .await?)
}

async fn set_status(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    status: &str,
    error_code: Option<&str>,
    provider_file_id: Option<&str>,
) -> Result<(), DomainError> {
    let mut q = attachments::Entity::update_many()
        .col_expr(attachments::Column::Status, Expr::value(status))
        .col_expr(attachments::Column::UpdatedAt, Expr::value(now()));
    if let Some(code) = error_code {
        q = q.col_expr(attachments::Column::ErrorCode, Expr::value(Some(code)));
    }
    if let Some(pid) = provider_file_id {
        q = q.col_expr(attachments::Column::ProviderFileId, Expr::value(Some(pid)));
    }
    q.filter(Condition::all().add(attachments::Column::Id.eq(id)))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

fn storage_unavailable(e: &StorageError) -> DomainError {
    DomainError::StorageUnavailable(e.message.clone())
}

impl AppServices {
    /// Resolves the chat and its model before the body is read.
    ///
    /// # Errors
    /// `NotFound(Chat)`, `InvalidModel`, policy failure.
    pub async fn prepare_upload(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<UploadContext, DomainError> {
        let scope = self.authz.chat_scope(ctx, "upload_attachment", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        drop(conn);
        let snapshot = policy::current_snapshot(self.policy.as_ref(), ctx.subject_id()).await?;
        let model = chat_model(&snapshot, &chat.model)?.clone();
        let storage = resolve_storage(&self.cfg, &model.provider_id, chat.tenant_id).ok_or_else(|| {
            DomainError::internal(format!("no storage provider for '{}'", model.provider_id))
        })?;
        Ok(UploadContext {
            chat,
            model,
            kill_switches: snapshot.kill_switches,
            storage,
        })
    }

    /// Validates an upload before its bytes are read; returns `(mime, kind, purposes)`.
    ///
    /// # Errors
    /// `UnsupportedContentType`, `FeatureDisabled("images")`, `CodeInterpreterUnavailable`.
    pub fn validate_upload_type(
        &self,
        up: &UploadContext,
        raw_content_type: &str,
        filename: &str,
    ) -> Result<(String, &'static str, bool, bool), DomainError> {
        let (mime, kind) = classify_content_type(raw_content_type, filename, self.cfg.rag.allow_csv_upload)?;
        if kind == "image" {
            if up.kill_switches.disable_images {
                return Err(DomainError::FeatureDisabled("images"));
            }
            return Ok((mime, kind, false, false));
        }
        let ci_only = mime == XLSX;
        if ci_only {
            if !up.code_interpreter_available() {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
            return Ok((mime, kind, false, true));
        }
        Ok((mime, kind, true, false))
    }

    /// Stores an uploaded file (`POST /v1/chats/{id}/attachments`).
    ///
    /// # Errors
    /// Limits, provider failures (503), database errors.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub async fn store_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        up: UploadContext,
        filename: String,
        mime: String,
        kind: &'static str,
        for_file_search: bool,
        for_code_interpreter: bool,
        data: Bytes,
        started: Instant,
    ) -> Result<AttachmentView, DomainError> {
        let _permit = Arc::clone(&self.upload_slots)
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrency)?;
        let chat = &up.chat;
        let tenant_id = chat.tenant_id;
        let scope = AccessScope::for_tenant(tenant_id);
        let conn = self.conn()?;
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        // Per-chat limits over non-deleted, non-failed attachments.
        let existing = attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat.id))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::Status.ne("failed")),
            )
            .all(&conn)
            .await?;
        if kind == "document"
            && existing.iter().filter(|a| a.attachment_kind == "document").count()
                >= self.cfg.rag.max_documents_per_chat as usize
        {
            return Err(DomainError::DocumentLimit);
        }
        let total: i64 = existing.iter().map(|a| a.size_bytes).sum();
        let cap = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        if total.saturating_add(size) > cap {
            return Err(DomainError::StorageLimit);
        }
        if for_file_search
            && let Some(vs) = chat_vector_stores::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat.id)))
                .one(&conn)
                .await?
            && vs.provider != up.storage.backend_label
        {
            return Err(DomainError::ProviderMismatch);
        }
        let id = Uuid::new_v4();
        let ts = now();
        let am = attachments::ActiveModel {
            id: sea_orm::Set(id),
            tenant_id: sea_orm::Set(tenant_id),
            chat_id: sea_orm::Set(chat.id),
            uploaded_by_user_id: sea_orm::Set(ctx.subject_id()),
            filename: sea_orm::Set(filename.clone()),
            content_type: sea_orm::Set(mime.clone()),
            size_bytes: sea_orm::Set(size),
            storage_backend: sea_orm::Set(up.storage.backend_label.clone()),
            provider_file_id: sea_orm::Set(None),
            status: sea_orm::Set("pending".to_owned()),
            error_code: sea_orm::Set(None),
            attachment_kind: sea_orm::Set(kind.to_owned()),
            for_file_search: sea_orm::Set(for_file_search),
            for_code_interpreter: sea_orm::Set(for_code_interpreter),
            doc_summary: sea_orm::Set(None),
            img_thumbnail: sea_orm::Set(None),
            img_thumbnail_width: sea_orm::Set(None),
            img_thumbnail_height: sea_orm::Set(None),
            summary_model: sea_orm::Set(None),
            summary_updated_at: sea_orm::Set(None),
            cleanup_status: sea_orm::Set(None),
            cleanup_attempts: sea_orm::Set(0),
            last_cleanup_error: sea_orm::Set(None),
            cleanup_updated_at: sea_orm::Set(None),
            created_at: sea_orm::Set(ts),
            updated_at: sea_orm::Set(ts),
            deleted_at: sea_orm::Set(None),
            secondary_file_id: sea_orm::Set(None),
            secondary_status: sea_orm::Set("not_attempted".to_owned()),
            secondary_provider_kind: sea_orm::Set(None),
        };
        attachments::Entity::insert(am)
            .secure()
            .scope_unchecked(&scope)?
            .exec(&conn)
            .await?;
        drop(conn);

        let ext = filename
            .rsplit_once('.')
            .map(|(_, e)| format!(".{e}"))
            .unwrap_or_default();
        let provider_name = format!("{}_{}{}", chat.id, id, ext);
        let file_id = match self
            .storage
            .upload_file(&up.storage, &provider_name, &mime, data.clone(), true)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %id, "provider file upload failed");
                set_status(&self.conn()?, tenant_id, id, "failed", Some("upload_failed"), None).await?;
                return Err(storage_unavailable(&e));
            }
        };
        set_status(&self.conn()?, tenant_id, id, "uploaded", None, Some(&file_id)).await?;

        if kind == "image" {
            let thumb = make_thumbnail(&data, &self.cfg.thumbnail);
            let mut q = attachments::Entity::update_many()
                .col_expr(attachments::Column::Status, Expr::value("ready"))
                .col_expr(attachments::Column::UpdatedAt, Expr::value(now()));
            if let Some(t) = thumb {
                q = q
                    .col_expr(attachments::Column::ImgThumbnail, Expr::value(Some(t.bytes)))
                    .col_expr(
                        attachments::Column::ImgThumbnailWidth,
                        Expr::value(Some(i32::try_from(t.width).unwrap_or(0))),
                    )
                    .col_expr(
                        attachments::Column::ImgThumbnailHeight,
                        Expr::value(Some(i32::try_from(t.height).unwrap_or(0))),
                    );
            }
            q.filter(Condition::all().add(attachments::Column::Id.eq(id)))
                .secure()
                .scope_with(&scope)
                .exec(&self.conn()?)
                .await?;
        } else if for_file_search {
            let vs = match self.get_or_create_vector_store(chat, &up.storage).await {
                Ok(v) => v,
                Err(e) => {
                    set_status(&self.conn()?, tenant_id, id, "failed", Some("vector_store_failed"), None).await?;
                    self.delete_file_best_effort(&up.storage, &file_id);
                    return Err(e);
                }
            };
            match self.index_document(&up.storage, &vs, &file_id, id, started).await {
                IndexResult::Ready => set_status(&self.conn()?, tenant_id, id, "ready", None, None).await?,
                IndexResult::Failed(msg) => {
                    tracing::warn!(attachment_id = %id, reason = %msg, "document indexing failed");
                    set_status(&self.conn()?, tenant_id, id, "failed", Some("indexing_failed"), None).await?;
                    self.delete_file_best_effort(&up.storage, &file_id);
                    return Err(DomainError::StorageUnavailable("indexing failed".into()));
                }
                IndexResult::Pending => {
                    let svc = Arc::clone(self);
                    let storage = up.storage.clone();
                    let chat_id = chat.id;
                    tokio::spawn(async move {
                        svc.background_indexing(storage, tenant_id, chat_id, id, vs, file_id).await;
                    });
                }
            }
        } else {
            set_status(&self.conn()?, tenant_id, id, "ready", None, None).await?;
        }
        let row = load_attachment_any(&self.conn()?, tenant_id, chat.id, id)
            .await?
            .ok_or(DomainError::NotFound(NotFoundKind::Attachment))?;
        Ok(AttachmentView::from(&row))
    }

    fn delete_file_best_effort(&self, storage: &StorageTarget, file_id: &str) {
        let client = self.storage.clone();
        let storage = storage.clone();
        let file_id = file_id.to_owned();
        tokio::spawn(async move {
            let _ = client.delete_file(&storage, &file_id).await;
        });
    }

    /// Per-chat vector store creation protocol (DESIGN §3.7 `chat_vector_stores`).
    async fn get_or_create_vector_store(&self, chat: &chats::Model, storage: &StorageTarget) -> Result<String, DomainError> {
        let scope = AccessScope::for_tenant(chat.tenant_id);
        for _ in 0..3 {
            let conn = self.conn()?;
            let row = chat_vector_stores::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat.id)))
                .one(&conn)
                .await?;
            if let Some(r) = row {
                if r.provider != storage.backend_label {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = r.vector_store_id {
                    return Ok(vs);
                }
                if (now() - r.created_at).num_seconds() > PLACEHOLDER_STALE_SECS {
                    chat_vector_stores::Entity::delete_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(chat_vector_stores::Column::Id.eq(r.id))
                                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                        )
                        .exec(&conn)
                        .await?;
                    continue;
                }
                drop(conn);
                return self.wait_for_vector_store(chat).await;
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_stores::ActiveModel {
                id: sea_orm::Set(row_id),
                tenant_id: sea_orm::Set(chat.tenant_id),
                chat_id: sea_orm::Set(chat.id),
                vector_store_id: sea_orm::Set(None),
                provider: sea_orm::Set(storage.backend_label.clone()),
                file_count: sea_orm::Set(0),
                created_at: sea_orm::Set(now()),
            };
            let inserted = chat_vector_stores::Entity::insert(am)
                .secure()
                .scope_unchecked(&scope)?
                .exec(&conn)
                .await;
            if let Err(e) = inserted {
                let e = DomainError::from(e);
                if matches!(e, DomainError::UniqueViolation) {
                    drop(conn);
                    return self.wait_for_vector_store(chat).await;
                }
                return Err(e);
            }
            drop(conn);
            let vs = match self
                .storage
                .create_vector_store(storage, &format!("chat_{}", chat.id))
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = chat_vector_stores::Entity::delete_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
                        .exec(&self.conn()?)
                        .await;
                    return Err(storage_unavailable(&e));
                }
            };
            let rows = chat_vector_stores::Entity::update_many()
                .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(Some(vs.clone())))
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::Id.eq(row_id))
                        .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .exec(&self.conn()?)
                .await?
                .rows_affected;
            if rows == 1 {
                return Ok(vs);
            }
            let client = self.storage.clone();
            let st = storage.clone();
            let stale = vs.clone();
            tokio::spawn(async move {
                let _ = client.delete_vector_store(&st, &stale).await;
            });
            return self.wait_for_vector_store(chat).await;
        }
        Err(DomainError::StorageUnavailable("vector store creation contention".into()))
    }

    async fn wait_for_vector_store(&self, chat: &chats::Model) -> Result<String, DomainError> {
        let mut delay = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(delay).await;
            delay *= 2;
            let row = chat_vector_stores::Entity::find()
                .secure()
                .scope_with(&AccessScope::for_tenant(chat.tenant_id))
                .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat.id)))
                .one(&self.conn()?)
                .await?;
            if let Some(vs) = row.and_then(|r| r.vector_store_id) {
                return Ok(vs);
            }
        }
        Err(DomainError::StorageUnavailable("vector store is being created".into()))
    }

    async fn index_document(
        &self,
        storage: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
        started: Instant,
    ) -> IndexResult {
        let first = match self.storage.add_vector_store_file(storage, vs, file_id, attachment_id).await {
            Ok(s) => s,
            Err(e) => return IndexResult::Failed(e.message),
        };
        match first.as_deref() {
            Some("completed") => return IndexResult::Ready,
            Some("in_progress") | None => {}
            Some(other) => return IndexResult::Failed(format!("status {other}")),
        }
        let mut delay = Duration::from_millis(250);
        loop {
            let remaining = INDEX_DEADLINE.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return IndexResult::Pending;
            }
            tokio::time::sleep(delay.min(remaining)).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            if started.elapsed() >= INDEX_DEADLINE {
                return IndexResult::Pending;
            }
            match tokio::time::timeout(
                INDEX_DEADLINE.saturating_sub(started.elapsed()),
                self.storage.vector_store_file_status(storage, vs, file_id),
            )
            .await
            {
                Err(_) => return IndexResult::Pending,
                Ok(Ok(Some(s))) if s == "completed" => return IndexResult::Ready,
                Ok(Ok(Some(s))) if s == "in_progress" => {}
                Ok(Ok(None)) => {}
                Ok(Ok(Some(other))) => return IndexResult::Failed(format!("status {other}")),
                Ok(Err(e)) if e.transient => {}
                Ok(Err(e)) => return IndexResult::Failed(e.message),
            }
        }
    }

    /// Continues indexing in the background for up to 10 minutes.
    async fn background_indexing(
        self: Arc<Self>,
        storage: StorageTarget,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
        vs: String,
        file_id: String,
    ) {
        let scope = AccessScope::for_tenant(tenant_id);
        let begin = Instant::now();
        let outcome: Option<bool> = 'outer: loop {
            if begin.elapsed() >= BACKGROUND_LIMIT {
                break Some(false);
            }
            // heartbeat; stop when the row is no longer ours to finish
            let Ok(conn) = self.conn() else { break None };
            let alive = attachments::Entity::update_many()
                .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                .filter(
                    Condition::all()
                        .add(attachments::Column::Id.eq(id))
                        .add(attachments::Column::Status.eq("uploaded"))
                        .add(attachments::Column::DeletedAt.is_null())
                        .add(attachments::Column::CleanupStatus.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await
                .map(|r| r.rows_affected > 0)
                .unwrap_or(false);
            drop(conn);
            if !alive {
                break None;
            }
            let round = Instant::now();
            let mut delay = Duration::from_millis(250);
            while round.elapsed() < BACKGROUND_ROUND {
                tokio::select! {
                    () = self.shutdown.cancelled() => break 'outer None,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                match self.storage.vector_store_file_status(&storage, &vs, &file_id).await {
                    Ok(Some(s)) if s == "completed" => break 'outer Some(true),
                    Ok(Some(s)) if s == "in_progress" => {}
                    Ok(None) => {}
                    Ok(Some(_)) => break 'outer Some(false),
                    Err(e) if e.transient => {}
                    Err(_) => break 'outer Some(false),
                }
            }
        };
        match outcome {
            Some(true) => {
                for (i, wait) in [0u64, 1, 2, 4].iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_secs(*wait)).await;
                    }
                    let Ok(conn) = self.conn() else { continue };
                    let res = attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("ready"))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(id))
                                .add(attachments::Column::Status.eq("uploaded"))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await;
                    if res.is_ok() {
                        return;
                    }
                }
            }
            Some(false) => {
                let outbox = Arc::clone(&self.outbox);
                let label = storage.backend_label.clone();
                let res = self
                    .db
                    .transaction_ref_mapped(move |tx| {
                        Box::pin(async move {
                            crate::domain::service::lock_for_write(tx).await?;
                            let ts = now();
                            let rows = attachments::Entity::update_many()
                                .col_expr(attachments::Column::Status, Expr::value("failed"))
                                .col_expr(attachments::Column::ErrorCode, Expr::value(Some("indexing_failed")))
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                                .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                                .filter(
                                    Condition::all()
                                        .add(attachments::Column::Id.eq(id))
                                        .add(attachments::Column::Status.eq("uploaded"))
                                        .add(attachments::Column::CleanupStatus.is_null()),
                                )
                                .secure()
                                .scope_with(&AccessScope::for_tenant(tenant_id))
                                .exec(tx)
                                .await?
                                .rows_affected;
                            let mut w = Wakes::default();
                            if rows > 0 {
                                let payload = AttachmentCleanupPayload {
                                    event_type: "attachment_indexing_failed".to_owned(),
                                    tenant_id,
                                    chat_id,
                                    attachment_id: id,
                                    provider_file_id: Some(file_id),
                                    vector_store_id: None,
                                    storage_backend: label,
                                    attachment_kind: "document".to_owned(),
                                    deleted_at: ts,
                                    secondary_ref: None,
                                };
                                w.push(outbox.attachment_cleanup(tx, tenant_id, &payload).await?);
                            }
                            Ok::<_, DomainError>(w)
                        })
                    })
                    .await;
                if let Ok(w) = res {
                    w.fire();
                }
            }
            None => {}
        }
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// `NotFound`, authorization and database errors.
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<AttachmentView, DomainError> {
        let scope = self.authz.chat_scope(ctx, "read_attachment", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let a = load_attachment_any(&conn, chat.tenant_id, chat_id, id)
            .await?
            .filter(|a| a.deleted_at.is_none() && a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::NotFound(NotFoundKind::Attachment))?;
        Ok(AttachmentView::from(&a))
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// `NotFound`, `AttachmentLocked`, authorization and database errors.
    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, "delete_attachment", Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let a = load_attachment_any(&conn, chat.tenant_id, chat_id, id)
            .await?
            .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::NotFound(NotFoundKind::Attachment))?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let referenced = message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat_id))
                    .add(message_attachments::Column::AttachmentId.eq(id)),
            )
            .count(&conn)
            .await?;
        if referenced > 0 {
            return Err(DomainError::AttachmentLocked);
        }
        drop(conn);
        let outbox = Arc::clone(&self.outbox);
        let tenant_id = chat.tenant_id;
        let wakes = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    let ts = now();
                    let rows = attachments::Entity::update_many()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(id))
                                .add(attachments::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    let mut w = Wakes::default();
                    if rows > 0 {
                        let payload = AttachmentCleanupPayload {
                            event_type: "attachment_deleted".to_owned(),
                            tenant_id,
                            chat_id,
                            attachment_id: id,
                            provider_file_id: a.provider_file_id.clone(),
                            vector_store_id: None,
                            storage_backend: a.storage_backend.clone(),
                            attachment_kind: a.attachment_kind.clone(),
                            deleted_at: ts,
                            secondary_ref: None,
                        };
                        w.push(outbox.attachment_cleanup(tx, tenant_id, &payload).await?);
                    }
                    Ok::<_, DomainError>(w)
                })
            })
            .await?;
        wakes.fire();
        Ok(())
    }
}

enum IndexResult {
    Ready,
    Pending,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_and_kinds() {
        assert_eq!(classify_content_type("image/PNG", "a.png", true).unwrap(), ("image/png".into(), "image"));
        assert_eq!(classify_content_type("application/octet-stream", "r.pdf", true).unwrap().0, "application/pdf");
        assert_eq!(classify_content_type("application/octet-stream", "x.xlsx", true).unwrap().0, XLSX);
        assert_eq!(classify_content_type("text/csv", "a.csv", true).unwrap().0, "text/plain");
        assert!(classify_content_type("text/csv", "a.csv", false).is_err());
        assert!(classify_content_type("application/octet-stream", "a.bin", true).is_err());
        assert!(classify_content_type("application/zip", "a.zip", true).is_err());
        assert_eq!(classify_content_type("text/plain; charset=utf-8", "a", true).unwrap(), ("text/plain".into(), "document"));
    }

    #[test]
    fn filenames_are_normalized() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("  ")), "upload");
        assert_eq!(normalize_filename(Some("dir/a.txt")), "a.txt");
        let long = format!("{}.pdf", "x".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert!(n.ends_with(".pdf"));
    }
}
