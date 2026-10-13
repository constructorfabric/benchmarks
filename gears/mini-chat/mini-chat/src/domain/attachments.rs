//! Attachments: upload, status, delete, vector stores and background indexing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::app::{App, fire, now, tenant_scope};
use super::authz::{actions, chat_scope};
use super::chats::load_chat;
use super::error::DomainError;
use super::thumbnail;
use crate::infra::db::entity::{attachments, chat_vector_stores, chats, message_attachments, messages};
use crate::infra::llm::resolver::RagTarget;
use crate::infra::llm::storage::{IndexStatus, StorageError};
use crate::infra::outbox::AttachmentCleanupPayload;

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
const STALE_PLACEHOLDER_SECS: i64 = 120;

/// Supported MIME types.
pub const SUPPORTED_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    XLSX,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];

/// Infers a MIME type from a filename extension.
#[must_use]
pub fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Truncates a filename to 255 characters keeping the extension; defaults to `upload`.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    if name.chars().count() <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{}", stem.chars().take(keep).collect::<String>(), ext)
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Resolved file classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileClass {
    pub content_type: String,
    pub kind: &'static str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

/// Validates the MIME type and derives kind and purposes.
///
/// # Errors
/// `UnsupportedContentType`.
pub fn classify(content_type: &str, filename: &str, allow_csv: bool) -> Result<FileClass, DomainError> {
    let mut ct = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if ct == "application/octet-stream"
        && let Some(m) = mime_from_extension(filename)
    {
        ct = m.to_owned();
    }
    if ct == "image/jpg" {
        ct = "image/jpeg".into();
    }
    if ct == "text/csv" {
        if !allow_csv {
            return Err(DomainError::UnsupportedContentType(ct));
        }
        ct = "text/plain".into();
    }
    if !SUPPORTED_TYPES.contains(&ct.as_str()) {
        return Err(DomainError::UnsupportedContentType(format!("unsupported content type '{ct}'")));
    }
    let kind = if ct.starts_with("image/") { "image" } else { "document" };
    let (fs, ci) = match (kind, ct.as_str()) {
        ("image", _) => (false, false),
        (_, XLSX) => (false, true),
        _ => (true, false),
    };
    Ok(FileClass { content_type: ct, kind, for_file_search: fs, for_code_interpreter: ci })
}

/// Upload limits and capabilities resolved before the body is read.
#[derive(Debug, Clone)]
pub struct UploadContext {
    pub chat: chats::Model,
    pub model: ModelCatalogEntry,
    pub snapshot: PolicySnapshot,
    pub rag: RagTarget,
    pub document_limit_bytes: u64,
    pub image_limit_bytes: u64,
    pub code_interpreter_available: bool,
}

/// A multipart file part as seen by the service.
pub struct UploadFile {
    pub filename: String,
    pub content_type: String,
}

fn provider_filename(chat_id: Uuid, attachment_id: Uuid, filename: &str) -> String {
    match filename.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 16 => format!("{chat_id}_{attachment_id}.{ext}"),
        _ => format!("{chat_id}_{attachment_id}"),
    }
}

impl App {
    /// Resolves chat, model and limits before the body is read.
    ///
    /// # Errors
    /// 404 chat, 400 `INVALID_MODEL`, 500 on resolver failure.
    pub async fn upload_context(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<UploadContext, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::UPLOAD_ATTACHMENT, Some(chat_id)).await?;
        let chat = {
            let conn = self.db.conn()?;
            load_chat(&conn, &scope, chat_id).await?
        };
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = snapshot.find(&chat.model).cloned().ok_or(DomainError::InvalidModel)?;
        let provider = self
            .resolver
            .resolve(&model.provider_id, chat.tenant_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{}' is not configured", model.provider_id)))?;
        let rag = provider
            .rag
            .ok_or_else(|| DomainError::internal(format!("provider '{}' has no storage", model.provider_id)))?;
        let model_cap = u64::from(model.general_config.max_file_size_mb).saturating_mul(1024 * 1024);
        let cap = |kb: u32| {
            let cfg = u64::from(kb) * 1024;
            if model_cap == 0 { cfg } else { cfg.min(model_cap) }
        };
        Ok(UploadContext {
            document_limit_bytes: cap(self.cfg.rag.uploaded_file_max_size_kb),
            image_limit_bytes: cap(self.cfg.rag.uploaded_image_max_size_kb),
            code_interpreter_available: model.general_config.tool_support.code_interpreter
                && !snapshot.kill_switches.disable_code_interpreter,
            chat,
            model,
            snapshot,
            rag,
        })
    }

    /// Validates the file part before its bytes are read.
    ///
    /// # Errors
    /// Unsupported type, disabled images, unavailable code interpreter.
    pub fn validate_upload(&self, uc: &UploadContext, file: &UploadFile) -> Result<FileClass, DomainError> {
        let mut class = classify(&file.content_type, &file.filename, self.cfg.rag.allow_csv_upload)?;
        if class.kind == "image" && uc.snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        if class.for_code_interpreter && !uc.code_interpreter_available {
            class.for_code_interpreter = false;
            if !class.for_file_search {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
        }
        Ok(class)
    }

    /// Stores an uploaded file: row, provider upload, indexing or thumbnail.
    ///
    /// # Errors
    /// Limits (429), storage failures (503), database errors.
    #[allow(clippy::too_many_lines)]
    pub async fn store_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        uc: &UploadContext,
        filename: String,
        class: FileClass,
        data: Bytes,
        started: Instant,
    ) -> Result<attachments::Model, DomainError> {
        let chat = &uc.chat;
        let scope = tenant_scope(chat.tenant_id);
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        let id = Uuid::new_v4();
        let at = now();
        {
            let conn = self.db.conn()?;
            let live = attachments::Entity::find()
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
            if class.kind == "document"
                && live.iter().filter(|a| a.attachment_kind == "document").count()
                    >= usize::try_from(self.cfg.rag.max_documents_per_chat).unwrap_or(usize::MAX)
            {
                return Err(DomainError::DocumentLimit);
            }
            let total: i64 = live.iter().map(|a| a.size_bytes).sum::<i64>() + size;
            if total > i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024 {
                return Err(DomainError::StorageLimit);
            }
            let am = attachments::ActiveModel {
                id: ActiveValue::Set(id),
                tenant_id: ActiveValue::Set(chat.tenant_id),
                chat_id: ActiveValue::Set(chat.id),
                uploaded_by_user_id: ActiveValue::Set(ctx.subject_id()),
                filename: ActiveValue::Set(filename.clone()),
                content_type: ActiveValue::Set(class.content_type.clone()),
                size_bytes: ActiveValue::Set(size),
                storage_backend: ActiveValue::Set(uc.rag.storage_backend.clone()),
                provider_file_id: ActiveValue::Set(None),
                status: ActiveValue::Set("pending".into()),
                error_code: ActiveValue::Set(None),
                attachment_kind: ActiveValue::Set(class.kind.into()),
                for_file_search: ActiveValue::Set(class.for_file_search),
                for_code_interpreter: ActiveValue::Set(class.for_code_interpreter),
                doc_summary: ActiveValue::Set(None),
                img_thumbnail: ActiveValue::Set(None),
                img_thumbnail_width: ActiveValue::Set(None),
                img_thumbnail_height: ActiveValue::Set(None),
                summary_model: ActiveValue::Set(None),
                summary_updated_at: ActiveValue::Set(None),
                cleanup_status: ActiveValue::Set(None),
                cleanup_attempts: ActiveValue::Set(0),
                last_cleanup_error: ActiveValue::Set(None),
                cleanup_updated_at: ActiveValue::Set(None),
                created_at: ActiveValue::Set(at),
                updated_at: ActiveValue::Set(at),
                deleted_at: ActiveValue::Set(None),
                secondary_file_id: ActiveValue::Set(None),
                secondary_status: ActiveValue::Set("not_attempted".into()),
                secondary_provider_kind: ActiveValue::Set(None),
            };
            attachments::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(&conn).await?;
        }

        let pfname = provider_filename(chat.id, id, &filename);
        let file_id = match self.storage.upload_file(&uc.rag, &pfname, &class.content_type, data.clone()).await {
            Ok(f) => f,
            Err(e) => {
                self.mark_failed(chat.tenant_id, id, "upload_failed").await;
                return Err(DomainError::StorageUnavailable(e.to_string()));
            }
        };
        self.update_attachment(chat.tenant_id, id, |u| {
            u.col_expr(attachments::Column::Status, Expr::value("uploaded"))
                .col_expr(attachments::Column::ProviderFileId, Expr::value(Some(file_id.clone())))
        })
        .await?;

        if class.kind == "image" {
            let thumb = thumbnail::generate(&data, &self.cfg.thumbnail);
            self.update_attachment(chat.tenant_id, id, |u| {
                let u = u.col_expr(attachments::Column::Status, Expr::value("ready"));
                match &thumb {
                    Some(t) => u
                        .col_expr(attachments::Column::ImgThumbnail, Expr::value(Some(t.data.clone())))
                        .col_expr(
                            attachments::Column::ImgThumbnailWidth,
                            Expr::value(Some(i32::try_from(t.width).unwrap_or(i32::MAX))),
                        )
                        .col_expr(
                            attachments::Column::ImgThumbnailHeight,
                            Expr::value(Some(i32::try_from(t.height).unwrap_or(i32::MAX))),
                        ),
                    None => u,
                }
            })
            .await?;
        } else if class.for_file_search {
            let vs = match self.vector_store_for(chat, &uc.rag).await {
                Ok(v) => v,
                Err(e) => {
                    self.mark_failed(chat.tenant_id, id, "vector_store_failed").await;
                    let _ = self.storage.delete_file(&uc.rag, &file_id).await;
                    return Err(e);
                }
            };
            let status = match self.storage.add_file_to_vector_store(&uc.rag, &vs, &file_id, id).await {
                Ok(s) => s,
                Err(e) => {
                    self.mark_failed(chat.tenant_id, id, "indexing_failed").await;
                    let _ = self.storage.delete_file(&uc.rag, &file_id).await;
                    return Err(DomainError::StorageUnavailable(e.to_string()));
                }
            };
            match self.wait_indexed(&uc.rag, &vs, &file_id, status, started).await {
                Ok(true) => {
                    self.update_attachment(chat.tenant_id, id, |u| u.col_expr(attachments::Column::Status, Expr::value("ready")))
                        .await?;
                }
                Ok(false) => {
                    let app = Arc::clone(self);
                    let rag = uc.rag.clone();
                    let (tenant, chat_id) = (chat.tenant_id, chat.id);
                    tokio::spawn(async move {
                        app.background_indexing(tenant, chat_id, id, rag, vs, file_id).await;
                    });
                }
                Err(e) => {
                    self.mark_failed(chat.tenant_id, id, "indexing_failed").await;
                    let app = Arc::clone(self);
                    let rag = uc.rag.clone();
                    tokio::spawn(async move {
                        let _ = app.storage.delete_file(&rag, &file_id).await;
                    });
                    return Err(DomainError::StorageUnavailable(e.to_string()));
                }
            }
        } else {
            self.update_attachment(chat.tenant_id, id, |u| u.col_expr(attachments::Column::Status, Expr::value("ready")))
                .await?;
        }
        let conn = self.db.conn()?;
        attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound)
    }

    /// Polls indexing until completed (true), still in progress at the
    /// deadline (false) or failed (error).
    async fn wait_indexed(
        &self,
        rag: &RagTarget,
        vs: &str,
        file_id: &str,
        mut status: IndexStatus,
        started: Instant,
    ) -> Result<bool, StorageError> {
        let deadline = self.indexing_deadline;
        let mut wait = Duration::from_millis(250);
        loop {
            match status {
                IndexStatus::Completed => return Ok(true),
                IndexStatus::Failed(s) => return Err(StorageError::Permanent(format!("indexing status {s}"))),
                IndexStatus::InProgress => {}
            }
            let elapsed = started.elapsed();
            if elapsed >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(wait.min(deadline - elapsed)).await;
            wait = (wait * 2).min(Duration::from_secs(2));
            if started.elapsed() >= deadline {
                return Ok(false);
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            status = match tokio::time::timeout(remaining, self.storage.vector_store_file_status(rag, vs, file_id)).await {
                Err(_) => return Ok(false),
                Ok(Ok(s)) => s,
                Ok(Err(e)) if e.is_transient() => IndexStatus::InProgress,
                Ok(Err(e)) => return Err(e),
            };
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn background_indexing(self: Arc<Self>, tenant_id: Uuid, chat_id: Uuid, id: Uuid, rag: RagTarget, vs: String, file_id: String) {
        let begin = Instant::now();
        let scope = tenant_scope(tenant_id);
        let still_owned = Condition::all()
            .add(attachments::Column::Id.eq(id))
            .add(attachments::Column::Status.eq("uploaded"))
            .add(attachments::Column::CleanupStatus.is_null())
            .add(attachments::Column::DeletedAt.is_null());
        let outcome: Result<bool, String> = 'outer: loop {
            // Heartbeat so the upload reaper leaves the row alone.
            let Ok(conn) = self.db.conn() else { break 'outer Err("db".into()) };
            let rows = attachments::Entity::update_many()
                .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                .filter(still_owned.clone())
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await
                .map(|r| r.rows_affected)
                .unwrap_or(0);
            drop(conn);
            if rows == 0 {
                return;
            }
            let round_start = Instant::now();
            let mut wait = Duration::from_millis(250).min(self.background_indexing_limit / 4);
            while round_start.elapsed() < BACKGROUND_ROUND {
                if begin.elapsed() >= self.background_indexing_limit {
                    break 'outer Err("timeout".into());
                }
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(Duration::from_secs(5));
                match self.storage.vector_store_file_status(&rag, &vs, &file_id).await {
                    Ok(IndexStatus::Completed) => break 'outer Ok(true),
                    Ok(IndexStatus::Failed(s)) => break 'outer Err(s),
                    Ok(IndexStatus::InProgress) => {}
                    Err(e) if e.is_transient() => {}
                    Err(e) => break 'outer Err(e.to_string()),
                }
            }
        };
        match outcome {
            Ok(_) => {
                for delay in [0_u64, 1, 2, 4] {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                    }
                    let Ok(conn) = self.db.conn() else { continue };
                    let res = attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("ready"))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                        .filter(still_owned.clone())
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await;
                    if res.is_ok() {
                        return;
                    }
                }
            }
            Err(reason) => {
                tracing::warn!(attachment_id = %id, reason = %reason, "background indexing failed");
                let app = Arc::clone(&self);
                let storage_backend = rag.storage_backend.clone();
                let res = self
                    .db
                    .transaction(move |tx| {
                        Box::pin(async move {
                            let at = now();
                            let rows = attachments::Entity::update_many()
                                .col_expr(attachments::Column::Status, Expr::value("failed"))
                                .col_expr(attachments::Column::ErrorCode, Expr::value(Some("indexing_failed")))
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(at)))
                                .col_expr(attachments::Column::UpdatedAt, Expr::value(at))
                                .filter(still_owned)
                                .secure()
                                .scope_with(&tenant_scope(tenant_id))
                                .exec(tx)
                                .await?
                                .rows_affected;
                            if rows == 0 {
                                return Ok(Vec::new());
                            }
                            let p = AttachmentCleanupPayload {
                                event_type: "attachment_indexing_failed".into(),
                                tenant_id,
                                chat_id,
                                attachment_id: id,
                                provider_file_id: Some(file_id),
                                vector_store_id: None,
                                storage_backend,
                                attachment_kind: "document".into(),
                                deleted_at: at,
                                secondary_ref: None,
                            };
                            Ok(vec![app.outbox.attachment_cleanup(tx, &p).await?])
                        })
                    })
                    .await;
                match res {
                    Ok(w) => fire(w),
                    Err(e) => tracing::warn!(error = %e, "failed to record indexing failure"),
                }
            }
        }
    }

    async fn update_attachment<F>(&self, tenant_id: Uuid, id: Uuid, f: F) -> Result<(), DomainError>
    where
        F: FnOnce(sea_orm::UpdateMany<attachments::Entity>) -> sea_orm::UpdateMany<attachments::Entity>,
    {
        let conn = self.db.conn()?;
        f(attachments::Entity::update_many())
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn mark_failed(&self, tenant_id: Uuid, id: Uuid, code: &'static str) {
        let res = self
            .update_attachment(tenant_id, id, |u| {
                u.col_expr(attachments::Column::Status, Expr::value("failed"))
                    .col_expr(attachments::Column::ErrorCode, Expr::value(Some(code)))
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to mark attachment failed");
        }
    }

    /// Get-or-create protocol of the chat's vector store.
    async fn vector_store_for(&self, chat: &chats::Model, rag: &RagTarget) -> Result<String, DomainError> {
        let scope = tenant_scope(chat.tenant_id);
        for _round in 0..3 {
            let row = {
                let conn = self.db.conn()?;
                find_vector_store(&conn, chat).await?
            };
            if let Some(r) = row {
                if r.provider != rag.storage_backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(id) = r.vector_store_id {
                    return Ok(id);
                }
                if (now() - r.created_at).whole_seconds() > STALE_PLACEHOLDER_SECS {
                    let conn = self.db.conn()?;
                    chat_vector_stores::Entity::delete_many()
                        .filter(
                            Condition::all()
                                .add(chat_vector_stores::Column::Id.eq(r.id))
                                .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await?;
                    continue;
                }
                return self.poll_vector_store(chat).await;
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_stores::ActiveModel {
                id: ActiveValue::Set(row_id),
                tenant_id: ActiveValue::Set(chat.tenant_id),
                chat_id: ActiveValue::Set(chat.id),
                vector_store_id: ActiveValue::Set(None),
                provider: ActiveValue::Set(rag.storage_backend.clone()),
                file_count: ActiveValue::Set(0),
                created_at: ActiveValue::Set(now()),
            };
            let inserted = {
                let conn = self.db.conn()?;
                chat_vector_stores::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(&conn).await
            };
            match inserted {
                Ok(_) => {}
                Err(e) if e.is_unique_violation() => return self.poll_vector_store(chat).await,
                Err(e) => return Err(e.into()),
            }
            let created = match self.storage.create_vector_store(rag, &format!("chat-{}", chat.id)).await {
                Ok(v) => v,
                Err(e) => {
                    let conn = self.db.conn()?;
                    let _ = chat_vector_stores::Entity::delete_many()
                        .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)))
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await;
                    return Err(DomainError::StorageUnavailable(e.to_string()));
                }
            };
            let rows = {
                let conn = self.db.conn()?;
                chat_vector_stores::Entity::update_many()
                    .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(Some(created.clone())))
                    .filter(
                        Condition::all()
                            .add(chat_vector_stores::Column::Id.eq(row_id))
                            .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                    )
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await?
                    .rows_affected
            };
            if rows == 1 {
                return Ok(created);
            }
            let _ = self.storage.delete_vector_store(rag, &created).await;
            return self.poll_vector_store(chat).await;
        }
        Err(DomainError::StorageUnavailable("vector store creation did not converge".into()))
    }

    async fn poll_vector_store(&self, chat: &chats::Model) -> Result<String, DomainError> {
        let mut wait = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(id) = find_vector_store(&conn, chat).await?.and_then(|r| r.vector_store_id) {
                return Ok(id);
            }
        }
        Err(DomainError::StorageUnavailable("vector store is being created".into()))
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// Authorization, not found or database errors.
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, attachment_id: Uuid) -> Result<attachments::Model, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::READ_ATTACHMENT, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let a = find_attachment(&conn, &chat, attachment_id).await?.ok_or(DomainError::AttachmentNotFound)?;
        if a.deleted_at.is_some() || a.uploaded_by_user_id != ctx.subject_id() {
            return Err(DomainError::AttachmentNotFound);
        }
        Ok(a)
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// Authorization, not found, locked or database errors.
    pub async fn delete_attachment(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid, attachment_id: Uuid) -> Result<(), DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, actions::DELETE_ATTACHMENT, Some(chat_id)).await?;
        let (chat, a) = {
            let conn = self.db.conn()?;
            let chat = load_chat(&conn, &scope, chat_id).await?;
            let a = find_attachment(&conn, &chat, attachment_id).await?.ok_or(DomainError::AttachmentNotFound)?;
            if a.uploaded_by_user_id != ctx.subject_id() {
                return Err(DomainError::AttachmentNotFound);
            }
            if a.deleted_at.is_some() {
                return Ok(());
            }
            if is_referenced(&conn, &chat, a.id).await? {
                return Err(DomainError::AttachmentLocked);
            }
            (chat, a)
        };
        let app = Arc::clone(self);
        let wakes = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = now();
                    let rows = attachments::Entity::update_many()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(Some(at)))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(at)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(at))
                        .filter(Condition::all().add(attachments::Column::Id.eq(a.id)).add(attachments::Column::DeletedAt.is_null()))
                        .secure()
                        .scope_with(&tenant_scope(chat.tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Ok(Vec::new());
                    }
                    let p = AttachmentCleanupPayload {
                        event_type: "attachment_deleted".into(),
                        tenant_id: chat.tenant_id,
                        chat_id: chat.id,
                        attachment_id: a.id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: at,
                        secondary_ref: None,
                    };
                    Ok(vec![app.outbox.attachment_cleanup(tx, &p).await?])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }
}

async fn find_vector_store(runner: &impl DBRunner, chat: &chats::Model) -> Result<Option<chat_vector_stores::Model>, DomainError> {
    Ok(chat_vector_stores::Entity::find()
        .secure()
        .scope_with(&tenant_scope(chat.tenant_id))
        .filter(
            Condition::all()
                .add(chat_vector_stores::Column::TenantId.eq(chat.tenant_id))
                .add(chat_vector_stores::Column::ChatId.eq(chat.id)),
        )
        .one(runner)
        .await?)
}

async fn find_attachment(runner: &impl DBRunner, chat: &chats::Model, id: Uuid) -> Result<Option<attachments::Model>, DomainError> {
    Ok(attachments::Entity::find()
        .secure()
        .scope_with(&tenant_scope(chat.tenant_id))
        .filter(Condition::all().add(attachments::Column::Id.eq(id)).add(attachments::Column::ChatId.eq(chat.id)))
        .one(runner)
        .await?)
}

async fn is_referenced(runner: &impl DBRunner, chat: &chats::Model, attachment_id: Uuid) -> Result<bool, DomainError> {
    let scope = tenant_scope(chat.tenant_id);
    let links = message_attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat.id))
                .add(message_attachments::Column::AttachmentId.eq(attachment_id)),
        )
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(false);
    }
    let ids: Vec<Uuid> = links.iter().map(|l| l.message_id).collect();
    let n = messages::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(messages::Column::Id.is_in(ids)).add(messages::Column::DeletedAt.is_null()))
        .count(runner)
        .await?;
    Ok(n > 0)
}

/// Reads a multipart field with a byte limit.
///
/// # Errors
/// `FileTooLarge` or a multipart error.
pub async fn read_limited(field: &mut multer::Field<'static>, limit: u64) -> Result<Bytes, DomainError> {
    let mut buf = BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| DomainError::Multipart {
        field: "multipart",
        reason: "MULTIPART_ERROR",
        detail: e.to_string(),
    })? {
        if u64::try_from(buf.len() + chunk.len()).unwrap_or(u64::MAX) > limit {
            return Err(DomainError::FileTooLarge(format!("file exceeds the limit of {limit} bytes")));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let c = classify("application/pdf", "a.pdf", true).unwrap();
        assert_eq!((c.kind, c.for_file_search, c.for_code_interpreter), ("document", true, false));
        let c = classify(XLSX, "a.xlsx", true).unwrap();
        assert_eq!((c.kind, c.for_file_search, c.for_code_interpreter), ("document", false, true));
        let c = classify("image/png", "a.png", true).unwrap();
        assert_eq!((c.kind, c.for_file_search, c.for_code_interpreter), ("image", false, false));
        let c = classify("application/octet-stream", "photo.JPG", true).unwrap();
        assert_eq!(c.content_type, "image/jpeg");
        assert_eq!(classify("text/csv", "a.csv", true).unwrap().content_type, "text/plain");
        assert!(classify("text/csv", "a.csv", false).is_err());
        assert!(classify("application/octet-stream", "a.bin", true).is_err());
        assert!(classify("application/x-msdownload", "a.exe", true).is_err());
    }

    #[test]
    fn filenames() {
        assert_eq!(normalize_filename(None), "upload");
        let long = format!("{}.pdf", "a".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert!(n.ends_with(".pdf"));
        let id = Uuid::nil();
        assert_eq!(provider_filename(id, id, "x.pdf"), format!("{id}_{id}.pdf"));
    }
}
