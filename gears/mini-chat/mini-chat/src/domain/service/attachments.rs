//! Attachments: upload, status, deletion and background indexing
//! (DESIGN §3.3 Upload/Get Attachment, §3.6 File Upload, §4 Attachment Deletion).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use time::OffsetDateTime;
use tokio::sync::OwnedSemaphorePermit;
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{MiniChatService, now};
use crate::domain::authz::{ChatScopes, actions};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::infra::db::entities::{attachments, chat_vector_stores, chats, message_attachments, messages};
use crate::infra::llm::{RagError, RagTarget};
use crate::infra::outbox::{AttachmentCleanupEvent, fire};

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const INDEX_DEADLINE: Duration = Duration::from_secs(25);
const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);

const DOC_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
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
    "text/x-typescript",
    "application/typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "text/x-sql",
];
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Upload preparation result (authz + model + slot), obtained before the body is read.
pub struct UploadContext {
    pub scopes: ChatScopes,
    pub chat: chats::Model,
    pub model: mini_chat_sdk::ModelCatalogEntry,
    pub kill_switches: mini_chat_sdk::KillSwitches,
    pub doc_limit_bytes: u64,
    pub image_limit_bytes: u64,
    pub rag: RagTarget,
    _permit: OwnedSemaphorePermit,
}

/// MIME classification of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub mime: String,
    pub kind: &'static str,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

/// Infer a MIME type from a filename extension.
#[must_use]
pub fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" => "text/plain",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/x-typescript",
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

/// Normalize the uploaded filename: default `upload`, at most 255 characters keeping the extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
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

fn file_too_large() -> DomainError {
    DomainError::out_of_range(Res::Attachment, "content_length", "FILE_TOO_LARGE", "file exceeds the upload size limit")
}

fn storage_unavailable() -> DomainError {
    DomainError::ServiceUnavailable { retry_after: 10, detail: "Service temporarily unavailable".to_owned() }
}

/// Summary of a stored attachment row.
pub type AttachmentRow = attachments::Model;

impl MiniChatService {
    /// Authorize, resolve the chat model and take an upload slot (before the body is read).
    ///
    /// # Errors
    /// 404 chat, 400 `INVALID_MODEL`, 500 policy failure, 503 concurrency limit.
    pub async fn upload_prepare(&self, ctx: &SecurityContext, chat_id: Uuid) -> DomainResult<UploadContext> {
        let scopes = self.scopes(ctx, actions::UPLOAD_ATTACHMENT, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = self.load_chat(&conn, &scopes, chat_id).await?;
        let snap = self.snapshot(ctx).await?;
        let model = Self::chat_model(&snap, &chat)?;
        let rag = self
            .llm
            .resolver
            .rag_target(&model.provider_id, ctx.subject_tenant_id())
            .map_err(|e| DomainError::internal(format!("rag provider resolution: {e}")))?;
        let permit = Arc::clone(&self.upload_slots)
            .try_acquire_owned()
            .map_err(|_| DomainError::ServiceUnavailable { retry_after: 5, detail: "Too many concurrent uploads".to_owned() })?;
        let per_model = model
            .general_config
            .max_file_size_mb
            .filter(|m| *m > 0)
            .map_or(u64::MAX, |m| u64::from(m) * 1024 * 1024);
        Ok(UploadContext {
            scopes,
            chat,
            model,
            kill_switches: snap.kill_switches.clone(),
            doc_limit_bytes: (u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024).min(per_model),
            image_limit_bytes: (u64::from(self.cfg.rag.uploaded_image_max_size_kb) * 1024).min(per_model),
            rag,
            _permit: permit,
        })
    }

    /// Classify the declared MIME type and resolve purposes (before the body is read).
    ///
    /// # Errors
    /// 400 `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED` (images), `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn classify_upload(&self, up: &UploadContext, content_type: &str, filename: &str) -> DomainResult<Classified> {
        let mut mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        if (mime == "application/octet-stream" || mime.is_empty())
            && let Some(m) = mime_from_extension(filename)
        {
            m.clone_into(&mut mime);
        }
        if mime == "image/jpg" {
            "image/jpeg".clone_into(&mut mime);
        }
        if mime == "text/csv" || mime == "application/csv" {
            if self.cfg.rag.allow_csv_upload {
                "text/plain".clone_into(&mut mime);
            } else {
                return Err(DomainError::invalid(Res::Attachment, "file", "UNSUPPORTED_CONTENT_TYPE", "unsupported file type"));
            }
        }
        if IMAGE_TYPES.contains(&mime.as_str()) {
            if up.kill_switches.disable_images {
                return Err(DomainError::precondition(Res::Attachment, "images", "FEATURE_DISABLED", "image uploads are disabled"));
            }
            return Ok(Classified { mime, kind: "image", for_file_search: false, for_code_interpreter: false });
        }
        if !DOC_TYPES.contains(&mime.as_str()) {
            return Err(DomainError::invalid(Res::Attachment, "file", "UNSUPPORTED_CONTENT_TYPE", "unsupported file type"));
        }
        let for_fs = mime != XLSX;
        let mut for_ci = mime == XLSX;
        if for_ci && (up.kill_switches.disable_code_interpreter || !up.model.tool_support().code_interpreter) {
            for_ci = false;
        }
        if !for_fs && !for_ci {
            return Err(DomainError::invalid(
                Res::Attachment,
                "file",
                "CODE_INTERPRETER_UNAVAILABLE",
                "code interpreter is not available for this chat",
            ));
        }
        Ok(Classified { mime, kind: "document", for_file_search: for_fs, for_code_interpreter: for_ci })
    }

    #[must_use]
    pub fn size_limit(up: &UploadContext, c: &Classified) -> u64 {
        if c.kind == "image" { up.image_limit_bytes } else { up.doc_limit_bytes }
    }

    /// Insert the row and process the upload synchronously.
    ///
    /// # Errors
    /// 409 `provider_mismatch`, 429 `document_limit` / `storage_limit`, 503 storage failure.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // sequential upload pipeline with per-step compensation
    pub async fn upload_commit(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        up: UploadContext,
        c: Classified,
        filename: String,
        data: Bytes,
    ) -> DomainResult<AttachmentRow> {
        let started = Instant::now();
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if size > Self::size_limit(&up, &c) {
            return Err(file_too_large());
        }
        let chat_id = up.chat.id;
        let tenant_id = ctx.subject_tenant_id();
        let id = Uuid::new_v4();
        let backend = up.rag.storage_backend.clone();
        // Vector store backend mismatch.
        if c.for_file_search {
            let conn = self.db.conn()?;
            if let Some(vs) = chat_vector_stores::Entity::find()
                .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
                .secure()
                .scope_with(&up.scopes.tenant)
                .one(&conn)
                .await?
                && vs.provider != backend
            {
                return Err(DomainError::AlreadyExists {
                    res: Res::Attachment,
                    resource_name: "provider_mismatch".into(),
                    detail: "The chat's documents are stored with another provider".into(),
                });
            }
        }
        // Insert the pending row with the per-chat limit checks.
        let max_docs = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = u64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let scopes = up.scopes.clone();
        let row_c = c.clone();
        let row_filename = filename.clone();
        let row_backend = backend.clone();
        let user_id = ctx.subject_id();
        self.tx(move |tx| {
                let scopes = scopes.clone();
                let c = row_c.clone();
                let filename = row_filename.clone();
                let backend = row_backend.clone();
                Box::pin(async move {
                    let live = attachments::Entity::find()
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::Status.ne("failed")),
                        )
                        .secure()
                        .scope_with(&scopes.tenant)
                        .all(tx)
                        .await?;
                    if c.kind == "document" && live.iter().filter(|a| a.attachment_kind == "document").count() as u64 >= max_docs {
                        return Err(DomainError::LimitExceeded {
                            res: Res::Attachment,
                            subject: "document_limit".into(),
                            description: "per-chat document limit reached".into(),
                        });
                    }
                    let total: u64 = live.iter().map(|a| u64::try_from(a.size_bytes).unwrap_or(0)).sum();
                    if total + size > max_total {
                        return Err(DomainError::LimitExceeded {
                            res: Res::Attachment,
                            subject: "storage_limit".into(),
                            description: "per-chat storage limit reached".into(),
                        });
                    }
                    let ts = now();
                    let am = attachments::ActiveModel {
                        id: Set(id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        uploaded_by_user_id: Set(user_id),
                        filename: Set(filename),
                        content_type: Set(c.mime.clone()),
                        size_bytes: Set(i64::try_from(size).unwrap_or(i64::MAX)),
                        storage_backend: Set(backend),
                        provider_file_id: Set(None),
                        status: Set("pending".into()),
                        error_code: Set(None),
                        attachment_kind: Set(c.kind.into()),
                        for_file_search: Set(c.for_file_search),
                        for_code_interpreter: Set(c.for_code_interpreter),
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
                        created_at: Set(ts),
                        updated_at: Set(ts),
                        deleted_at: Set(None),
                        secondary_file_id: Set(None),
                        secondary_status: Set("not_attempted".into()),
                        secondary_provider_kind: Set(None),
                    };
                    secure_insert::<attachments::Entity>(am, &scopes.tenant, tx).await?;
                    Ok(())
                })
            })
            .await?;

        // Provider upload.
        let ext = filename.rsplit_once('.').map_or_else(|| "bin".to_owned(), |(_, e)| e.to_owned());
        let provider_name = format!("{chat_id}_{id}.{ext}");
        let thumb_source = (c.kind == "image").then(|| data.clone());
        let file_id = match self.llm.upload_file(&up.rag, &provider_name, &c.mime, data).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %id, "provider file upload failed");
                self.fail_attachment(tenant_id, id, "upload_failed").await;
                return Err(storage_unavailable());
            }
        };
        self.update_attachment(tenant_id, id, |q| {
            q.col_expr(attachments::Column::Status, Expr::value("uploaded"))
                .col_expr(attachments::Column::ProviderFileId, Expr::value(file_id.clone()))
        })
        .await?;

        if c.for_file_search {
            let vs = match self.ensure_vector_store(&up, tenant_id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, attachment_id = %id, "vector store unavailable");
                    self.fail_attachment(tenant_id, id, "vector_store_failed").await;
                    drop(self.llm.delete_file(&up.rag, &file_id).await);
                    return Err(match e {
                        DomainError::AlreadyExists { .. } => e,
                        _ => storage_unavailable(),
                    });
                }
            };
            let first = self.llm.add_vector_store_file(&up.rag, &vs, &file_id, id).await;
            let outcome = match first {
                Err(e) => {
                    tracing::warn!(error = %e, attachment_id = %id, "adding file to vector store failed");
                    IndexOutcome::Failed
                }
                Ok(status) => self.poll_indexing(&up.rag, &vs, &file_id, status, started + INDEX_DEADLINE).await,
            };
            match outcome {
                IndexOutcome::Completed => {}
                IndexOutcome::Failed => {
                    self.fail_attachment(tenant_id, id, "indexing_failed").await;
                    let llm = Arc::clone(&self.llm);
                    let rag = up.rag.clone();
                    let fid = file_id.clone();
                    tokio::spawn(async move {
                        drop(llm.delete_file(&rag, &fid).await);
                    });
                    return Err(storage_unavailable());
                }
                IndexOutcome::Pending => {
                    let svc = Arc::clone(self);
                    let rag = up.rag.clone();
                    tokio::spawn(async move { svc.background_indexing(tenant_id, chat_id, id, rag, vs, file_id).await });
                    return self.attachment_row(tenant_id, id).await;
                }
            }
        }
        let thumb = thumb_source.and_then(|d| crate::infra::thumbnail::generate(&d, &self.cfg.thumbnail));
        self.update_attachment(tenant_id, id, move |q| {
            let q = q.col_expr(attachments::Column::Status, Expr::value("ready"));
            match thumb {
                Some(t) => q
                    .col_expr(attachments::Column::ImgThumbnail, Expr::value(t.bytes))
                    .col_expr(attachments::Column::ImgThumbnailWidth, Expr::value(i32::try_from(t.width).unwrap_or(0)))
                    .col_expr(attachments::Column::ImgThumbnailHeight, Expr::value(i32::try_from(t.height).unwrap_or(0))),
                None => q,
            }
        })
        .await?;
        self.attachment_row(tenant_id, id).await
    }

    async fn attachment_row(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<AttachmentRow> {
        let conn = self.db.conn()?;
        attachments::Entity::find()
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await?
            .ok_or(DomainError::NotFound(Res::Attachment))
    }

    async fn update_attachment<F>(&self, tenant_id: Uuid, id: Uuid, f: F) -> DomainResult<u64>
    where
        F: FnOnce(sea_orm::UpdateMany<attachments::Entity>) -> sea_orm::UpdateMany<attachments::Entity>,
    {
        let conn = self.db.conn()?;
        let q = f(attachments::Entity::update_many().col_expr(attachments::Column::UpdatedAt, Expr::value(now())));
        let res = q
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        Ok(res.rows_affected)
    }

    async fn fail_attachment(&self, tenant_id: Uuid, id: Uuid, code: &str) {
        let code = code.to_owned();
        drop(
            self
                .update_attachment(tenant_id, id, move |q| {
                    q.col_expr(attachments::Column::Status, Expr::value("failed"))
                        .col_expr(attachments::Column::ErrorCode, Expr::value(code))
                })
                .await,
        );
    }

    /// Get or create the chat vector store (placeholder row + CAS protocol).
    async fn ensure_vector_store(&self, up: &UploadContext, tenant_id: Uuid) -> DomainResult<String> {
        let chat_id = up.chat.id;
        for _round in 0..3 {
            let conn = self.db.conn()?;
            let existing = chat_vector_stores::Entity::find()
                .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
                .secure()
                .scope_with(&up.scopes.tenant)
                .one(&conn)
                .await?;
            if let Some(row) = &existing {
                if row.provider != up.rag.storage_backend {
                    return Err(DomainError::AlreadyExists {
                        res: Res::Attachment,
                        resource_name: "provider_mismatch".into(),
                        detail: "The chat's documents are stored with another provider".into(),
                    });
                }
                if let Some(vs) = &row.vector_store_id {
                    return Ok(vs.clone());
                }
                let age = OffsetDateTime::now_utc() - row.created_at;
                if age > time::Duration::try_from(STALE_PLACEHOLDER).unwrap_or(time::Duration::MAX) {
                    chat_vector_stores::Entity::delete_many()
                        .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row.id)).add(chat_vector_stores::Column::VectorStoreId.is_null()))
                        .secure()
                        .scope_with(&up.scopes.tenant)
                        .exec(&conn)
                        .await?;
                    continue;
                }
                return self.wait_for_vector_store(up).await;
            }
            let row_id = Uuid::new_v4();
            let am = chat_vector_stores::ActiveModel {
                id: Set(row_id),
                tenant_id: Set(tenant_id),
                chat_id: Set(chat_id),
                vector_store_id: Set(None),
                provider: Set(up.rag.storage_backend.clone()),
                file_count: Set(0),
                created_at: Set(now()),
            };
            match secure_insert::<chat_vector_stores::Entity>(am, &up.scopes.tenant, &conn).await {
                Ok(_) => {}
                Err(e) if e.is_unique_violation() => return self.wait_for_vector_store(up).await,
                Err(e) => return Err(e.into()),
            }
            let created = self.llm.create_vector_store(&up.rag, &format!("chat-{chat_id}")).await;
            let vs = match created {
                Ok(v) => v,
                Err(e) => {
                    drop(
                        chat_vector_stores::Entity::delete_many()
                            .filter(chat_vector_stores::Column::Id.eq(row_id))
                            .secure()
                            .scope_with(&up.scopes.tenant)
                            .exec(&conn)
                            .await,
                    );
                    return Err(DomainError::internal(format!("create vector store: {e}")));
                }
            };
            let res = chat_vector_stores::Entity::update_many()
                .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(vs.clone()))
                .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(row_id)).add(chat_vector_stores::Column::VectorStoreId.is_null()))
                .secure()
                .scope_with(&up.scopes.tenant)
                .exec(&conn)
                .await?;
            if res.rows_affected == 1 {
                return Ok(vs);
            }
            drop(self.llm.delete_vector_store(&up.rag, &vs).await);
            return self.wait_for_vector_store(up).await;
        }
        Err(storage_unavailable())
    }

    async fn wait_for_vector_store(&self, up: &UploadContext) -> DomainResult<String> {
        let mut delay = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(delay).await;
            delay *= 2;
            let conn = self.db.conn()?;
            if let Some(vs) = chat_vector_stores::Entity::find()
                .filter(chat_vector_stores::Column::ChatId.eq(up.chat.id))
                .secure()
                .scope_with(&up.scopes.tenant)
                .one(&conn)
                .await?
                .and_then(|r| r.vector_store_id)
            {
                return Ok(vs);
            }
        }
        Err(storage_unavailable())
    }

    async fn poll_indexing(&self, rag: &RagTarget, vs: &str, file_id: &str, first: Option<String>, deadline: Instant) -> IndexOutcome {
        let mut status = first;
        let mut delay = Duration::from_millis(250);
        loop {
            match status.as_deref() {
                Some("completed") => return IndexOutcome::Completed,
                Some("in_progress") | None => {}
                Some(_) => return IndexOutcome::Failed,
            }
            let now = Instant::now();
            if now >= deadline {
                return IndexOutcome::Pending;
            }
            tokio::time::sleep(delay.min(deadline - now)).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            if Instant::now() >= deadline {
                return IndexOutcome::Pending;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.llm.vector_store_file_status(rag, vs, file_id)).await {
                Err(_) => return IndexOutcome::Pending,
                Ok(Ok(s)) => status = s,
                Ok(Err(RagError { transient: true, .. })) => status = Some("in_progress".to_owned()),
                Ok(Err(_)) => return IndexOutcome::Failed,
            }
        }
    }

    /// Background indexing continuation (up to 10 minutes).
    #[allow(clippy::cognitive_complexity)] // polling state machine with per-outcome branches
    async fn background_indexing(self: Arc<Self>, tenant_id: Uuid, chat_id: Uuid, id: Uuid, rag: RagTarget, vs: String, file_id: String) {
        let begun = Instant::now();
        let scope = AccessScope::for_tenant(tenant_id);
        let still_owned = Condition::all()
            .add(attachments::Column::Id.eq(id))
            .add(attachments::Column::Status.eq("uploaded"))
            .add(attachments::Column::CleanupStatus.is_null())
            .add(attachments::Column::DeletedAt.is_null());
        let mut status: Option<String> = Some("in_progress".to_owned());
        let mut last_error: Option<String> = None;
        let outcome = 'outer: loop {
            if begun.elapsed() >= BACKGROUND_LIMIT {
                break 'outer IndexOutcome::Pending;
            }
            // Heartbeat (keeps the upload reaper away); stop when the row is no longer ours.
            let Ok(conn) = self.db.conn() else { return };
            let touched = attachments::Entity::update_many()
                .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                .filter(still_owned.clone())
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await;
            match touched {
                Ok(r) if r.rows_affected == 0 => return,
                Err(_) => return,
                Ok(_) => {}
            }
            let round_end = Instant::now() + BACKGROUND_ROUND;
            let mut delay = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                match self.llm.vector_store_file_status(&rag, &vs, &file_id).await {
                    Ok(s) => status = s,
                    Err(e) if e.transient => {
                        last_error = Some(e.message.clone());
                    }
                    Err(_) => break 'outer IndexOutcome::Failed,
                }
                match status.as_deref() {
                    Some("completed") => break 'outer IndexOutcome::Completed,
                    Some("in_progress") | None => {}
                    Some(_) => break 'outer IndexOutcome::Failed,
                }
                if begun.elapsed() >= BACKGROUND_LIMIT {
                    break 'outer IndexOutcome::Pending;
                }
            }
        };
        if outcome == IndexOutcome::Completed {
            let mut wait = Duration::from_secs(1);
            for attempt in 0..4 {
                if let Ok(conn) = self.db.conn() {
                    let r = attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("ready"))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                        .filter(still_owned.clone())
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await;
                    if r.is_ok() {
                        return;
                    }
                }
                if attempt < 3 {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
            }
            return;
        }
        if outcome == IndexOutcome::Pending {
            tracing::warn!(attachment_id = %id, last_error = ?last_error, "background indexing timed out");
        }
        let outbox = self.outbox.clone();
        let res = self
            .tx(move |tx| {
                let outbox = outbox.clone();
                let still_owned = still_owned.clone();
                let scope = scope.clone();
                let rag_label = rag.storage_backend.clone();
                let file_id = file_id.clone();
                Box::pin(async move {
                    let ts = now();
                    let r = attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("failed"))
                        .col_expr(attachments::Column::ErrorCode, Expr::value("indexing_failed"))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(ts))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                        .filter(still_owned)
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(Vec::new());
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_indexing_failed".into(),
                        tenant_id,
                        chat_id,
                        attachment_id: id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: rag_label,
                        attachment_kind: "document".into(),
                        deleted_at: ts,
                        secondary_ref: None,
                    };
                    Ok(vec![outbox.attachment_cleanup(tx, &ev).await?])
                })
            })
            .await;
        match res {
            Ok(w) => fire(w),
            Err(e) => tracing::error!(error = %e, attachment_id = %id, "failing background indexing failed"),
        }
    }

    async fn find_attachment(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid, id: Uuid, user_id: Uuid) -> DomainResult<AttachmentRow> {
        attachments::Entity::find()
            .filter(Condition::all().add(attachments::Column::Id.eq(id)).add(attachments::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&scopes.tenant)
            .one(runner)
            .await?
            .filter(|a| a.uploaded_by_user_id == user_id)
            .ok_or(DomainError::NotFound(Res::Attachment))
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404 chat / attachment.
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> DomainResult<AttachmentRow> {
        let scopes = self.scopes(ctx, actions::READ_ATTACHMENT, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        let a = self.find_attachment(&conn, &scopes, chat_id, id, ctx.subject_id()).await?;
        if a.deleted_at.is_some() {
            return Err(DomainError::NotFound(Res::Attachment));
        }
        Ok(a)
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}` (idempotent).
    ///
    /// # Errors
    /// 404, 409 `attachment_locked`.
    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> DomainResult<()> {
        let scopes = self.scopes(ctx, actions::DELETE_ATTACHMENT, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        self.load_chat(&conn, &scopes, chat_id).await?;
        let a = self.find_attachment(&conn, &scopes, chat_id, id, ctx.subject_id()).await?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let links: Vec<Uuid> = message_attachments::Entity::find()
            .filter(Condition::all().add(message_attachments::Column::ChatId.eq(chat_id)).add(message_attachments::Column::AttachmentId.eq(id)))
            .secure()
            .scope_with(&scopes.tenant)
            .all(&conn)
            .await?
            .into_iter()
            .map(|l| l.message_id)
            .collect();
        if !links.is_empty() {
            let referenced = messages::Entity::find()
                .filter(Condition::all().add(messages::Column::Id.is_in(links)).add(messages::Column::DeletedAt.is_null()))
                .secure()
                .scope_with(&scopes.tenant)
                .count(&conn)
                .await?;
            if referenced > 0 {
                return Err(DomainError::AlreadyExists {
                    res: Res::Attachment,
                    resource_name: "attachment_locked".into(),
                    detail: "The attachment is referenced by a sent message".into(),
                });
            }
        }
        let outbox = self.outbox.clone();
        let wakes = self
            .tx(move |tx| {
                let scopes = scopes.clone();
                let outbox = outbox.clone();
                let a = a.clone();
                Box::pin(async move {
                    let ts = now();
                    let cleanup_owned = a.cleanup_status.is_none();
                    let mut q = attachments::Entity::update_many()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(ts))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts));
                    if cleanup_owned {
                        q = q
                            .col_expr(attachments::Column::CleanupStatus, Expr::value("pending"))
                            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(ts));
                    }
                    let r = q
                        .filter(Condition::all().add(attachments::Column::Id.eq(a.id)).add(attachments::Column::DeletedAt.is_null()))
                        .secure()
                        .scope_with(&scopes.tenant)
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 || !cleanup_owned {
                        return Ok(Vec::new());
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_deleted".into(),
                        tenant_id: a.tenant_id,
                        chat_id: a.chat_id,
                        attachment_id: a.id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: ts,
                        secondary_ref: None,
                    };
                    Ok(vec![outbox.attachment_cleanup(tx, &ev).await?])
                })
            })
            .await?;
        fire(wakes);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexOutcome {
    Completed,
    Failed,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_rules() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("  ")), "upload");
        assert_eq!(normalize_filename(Some("dir/a.pdf")), "a.pdf");
        let long = format!("{}.pdf", "x".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert!(std::path::Path::new(&n).extension().is_some_and(|e| e == "pdf"));
    }

    #[test]
    fn extension_inference() {
        assert_eq!(mime_from_extension("a.PDF"), Some("application/pdf"));
        assert_eq!(mime_from_extension("a.xlsx"), Some(XLSX));
        assert_eq!(mime_from_extension("a.png"), Some("image/png"));
        assert_eq!(mime_from_extension("a.exe"), None);
        assert_eq!(mime_from_extension("noext"), None);
    }
}
