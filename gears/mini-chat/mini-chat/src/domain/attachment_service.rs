//! Attachments: upload pipeline, vector-store creation protocol, indexing,
//! get and delete (DESIGN §3.3, §3.6 "File Upload", §4 "Attachment Deletion").

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::domain::thumbnail;
use crate::domain::views::AttachmentView;
use crate::infra::db::entity::{attachments, chat_vector_stores, chats, message_attachments, messages};
use crate::infra::db::now;
use crate::infra::llm::resolver::StorageTarget;
use crate::infra::llm::storage::StorageError;
use crate::infra::outbox::{AttachmentCleanupEvent, Queue, SecondaryRef};

/// Upload deadline for synchronous indexing (fixed).
pub const INDEXING_DEADLINE: Duration = Duration::from_secs(25);
const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
const BACKGROUND_LIMIT: Duration = Duration::from_secs(600);
const STALE_PLACEHOLDER: Duration = Duration::from_secs(120);

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "text/x-c",
    "text/x-c++",
    "text/x-csharp",
    "text/x-java",
    "text/x-python",
    "text/x-script.python",
    "text/x-ruby",
    "text/x-php",
    "text/x-tex",
    "text/css",
    "text/javascript",
    "application/typescript",
    "application/x-sh",
    XLSX,
];

/// MIME type from a file extension (used for `application/octet-stream`).
#[must_use]
pub fn mime_from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "c" => "text/x-c",
        "cpp" => "text/x-c++",
        "js" => "text/javascript",
        "ts" => "application/typescript",
        "sh" => "application/x-sh",
        "tex" => "text/x-tex",
        "css" => "text/css",
        _ => return None,
    })
}

/// Normalize and validate a MIME type.
///
/// # Errors
/// `UNSUPPORTED_CONTENT_TYPE`.
pub fn resolve_mime(raw: &str, filename: &str, allow_csv: bool) -> Result<String, DomainError> {
    let base = raw.split(';').next().unwrap_or(raw).trim().to_ascii_lowercase();
    let mut mime = if base == "application/octet-stream" {
        mime_from_extension(filename).map_or(base, ToOwned::to_owned)
    } else {
        base
    };
    if mime == "image/jpg" {
        mime = "image/jpeg".into();
    }
    if mime == "text/csv" {
        if allow_csv {
            mime = "text/plain".into();
        } else {
            return Err(DomainError::UnsupportedContentType(mime));
        }
    }
    if IMAGE_TYPES.contains(&mime.as_str()) || DOCUMENT_TYPES.contains(&mime.as_str()) {
        Ok(mime)
    } else {
        Err(DomainError::UnsupportedContentType(mime))
    }
}

/// Whether the MIME type is an image.
#[must_use]
pub fn is_image(mime: &str) -> bool {
    IMAGE_TYPES.contains(&mime)
}

/// Truncate a filename to 255 characters keeping the extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    if name.chars().count() <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{ext}", stem.chars().take(keep).collect::<String>())
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Upload limits resolved from config and the chat's model.
#[derive(Debug, Clone, Copy)]
pub struct UploadLimits {
    pub document_max_bytes: u64,
    pub image_max_bytes: u64,
    pub code_interpreter_available: bool,
    pub images_disabled: bool,
}

/// Context resolved before the body is read.
#[derive(Debug, Clone)]
pub struct UploadContext {
    pub chat: chats::Model,
    pub limits: UploadLimits,
    pub storage: StorageTarget,
    pub provider_id: String,
    pub anthropic: bool,
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 16 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into())
}

enum IndexOutcome {
    Ready,
    Failed(String),
    StillIndexing,
}

impl MiniChat {
    /// Authorize, load the chat and resolve upload limits (before the body is read).
    ///
    /// # Errors
    /// 404, `INVALID_MODEL`, 500 (plugin), 503 (no storage provider).
    pub async fn prepare_upload(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<UploadContext, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id).await?;
        let snap = self.snapshot(ctx).await?;
        let model = Self::chat_model(&snap, &chat.model)?;
        let mb = u64::from(model.general_config.max_file_size_mb) * 1024 * 1024;
        let cap = |kb: u32| {
            let cfg = u64::from(kb) * 1024;
            if mb > 0 { cfg.min(mb) } else { cfg }
        };
        let limits = UploadLimits {
            document_max_bytes: cap(self.cfg.rag.uploaded_file_max_size_kb),
            image_max_bytes: cap(self.cfg.rag.uploaded_image_max_size_kb),
            code_interpreter_available: !snap.kill_switches.disable_code_interpreter
                && model.tool_support().code_interpreter,
            images_disabled: snap.kill_switches.disable_images,
        };
        let storage = self.resolver.storage(&model.provider_id, &chat.tenant_id.to_string())?;
        Ok(UploadContext {
            chat,
            limits,
            storage,
            provider_id: model.provider_id.clone(),
            anthropic: self.resolver.is_anthropic(&model.provider_id),
        })
    }

    /// Purposes and kind checks for a resolved MIME type.
    ///
    /// # Errors
    /// `FEATURE_DISABLED` (images), `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn check_kind(up: &UploadContext, mime: &str) -> Result<(bool, bool, bool), DomainError> {
        let image = is_image(mime);
        if image {
            if up.limits.images_disabled {
                return Err(DomainError::FeatureDisabled("images"));
            }
            return Ok((true, false, false));
        }
        if mime == XLSX {
            if !up.limits.code_interpreter_available {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
            return Ok((false, false, true));
        }
        Ok((false, true, false))
    }

    async fn mark_failed(&self, tenant: Uuid, id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let res = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value("failed"))
            .col_expr(attachments::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to mark attachment failed");
        }
    }

    /// Upload processing after the body was read and validated.
    ///
    /// # Errors
    /// 429 limits, 409 provider mismatch, 503 storage failures.
    // reason: linear upload pipeline with compensating cleanup at each step; splitting risks reordering side effects
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub async fn complete_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        up: UploadContext,
        filename: String,
        mime: String,
        data: Bytes,
        started: Instant,
    ) -> Result<AttachmentView, DomainError> {
        let (image, for_fs, for_ci) = Self::check_kind(&up, &mime)?;
        let chat = up.chat.clone();
        let tenant = chat.tenant_id;
        let scope = AccessScope::for_tenant(tenant);
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        if for_fs {
            let conn = self.db.conn()?;
            if let Some(row) = chat_vector_stores::Entity::find()
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::TenantId.eq(tenant))
                        .add(chat_vector_stores::Column::ChatId.eq(chat.id)),
                )
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?
                && row.provider != up.storage.backend_label
            {
                return Err(DomainError::ProviderMismatch);
            }
        }
        let id = Uuid::new_v4();
        let max_docs = i64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let user = ctx.subject_id();
        let chat2 = chat.clone();
        let fname = filename.clone();
        let mime2 = mime.clone();
        let label = up.storage.backend_label.clone();
        let row = crate::infra::db::tx_retry(&self.db, move |tx| {
                let fname = fname.clone();
                let label = label.clone();
                let mime2 = mime2.clone();
                Box::pin(async move {
                    let live = attachments::Entity::find()
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat2.id))
                                .add(attachments::Column::DeletedAt.is_null())
                                .add(attachments::Column::Status.ne("failed")),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(chat2.tenant_id))
                        .all(tx)
                        .await?;
                    if !image {
                        let docs = live.iter().filter(|a| a.attachment_kind == "document").count();
                        if i64::try_from(docs).unwrap_or(i64::MAX) >= max_docs {
                            return Err(DomainError::DocumentLimit);
                        }
                    }
                    let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                    if total.saturating_add(size) > max_total {
                        return Err(DomainError::StorageLimit);
                    }
                    let ts = now();
                    let am = attachments::ActiveModel {
                        id: Set(id),
                        tenant_id: Set(chat2.tenant_id),
                        chat_id: Set(chat2.id),
                        uploaded_by_user_id: Set(user),
                        filename: Set(fname),
                        content_type: Set(mime2),
                        size_bytes: Set(size),
                        storage_backend: Set(label),
                        provider_file_id: Set(None),
                        status: Set("pending".into()),
                        error_code: Set(None),
                        attachment_kind: Set(if image { "image" } else { "document" }.into()),
                        for_file_search: Set(for_fs),
                        for_code_interpreter: Set(for_ci),
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
                    Ok(attachments::Entity::insert(am)
                        .secure()
                        .scope_unchecked(&AccessScope::for_tenant(chat2.tenant_id))?
                        .exec_with_returning(tx)
                        .await?)
                })
            })
            .await?;

        // Provider upload.
        let provider_name = format!("{}_{}.{}", chat.id, id, ext_of(&filename));
        let file_id = match self
            .storage
            .upload_file(&up.storage, &provider_name, &mime, data.clone())
            .await
        {
            Ok(f) => f,
            Err(e) => {
                self.mark_failed(tenant, id, "upload_failed").await;
                return Err(DomainError::StorageUnavailable(format!("file upload: {e}")));
            }
        };
        let conn = self.db.conn()?;
        attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value("uploaded"))
            .col_expr(attachments::Column::ProviderFileId, Expr::value(Some(file_id.clone())))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
            .filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;

        if image && up.anthropic && data.len() <= self.cfg.thumbnail.max_decode_bytes {
            self.secondary_upload(&up, id, &provider_name, &mime, data.clone()).await;
        }

        if for_fs {
            let vs = match self.ensure_vector_store(&chat, &up.storage).await {
                Ok(v) => v,
                Err(e) => {
                    self.mark_failed(tenant, id, "vector_store_failed").await;
                    self.best_effort_delete(&up.storage, &file_id);
                    return Err(e);
                }
            };
            match self.index_document(&up.storage, &vs, &file_id, id, started).await {
                IndexOutcome::Ready => self.set_ready(tenant, id, None).await?,
                IndexOutcome::Failed(why) => {
                    self.mark_failed(tenant, id, "indexing_failed").await;
                    self.best_effort_delete(&up.storage, &file_id);
                    return Err(DomainError::StorageUnavailable(format!("indexing: {why}")));
                }
                IndexOutcome::StillIndexing => {
                    let svc = Arc::clone(self);
                    let storage = up.storage.clone();
                    let chat_id = chat.id;
                    tokio::spawn(async move {
                        svc.background_indexing(tenant, chat_id, id, storage, vs, file_id).await;
                    });
                }
            }
        } else if image {
            let thumb = thumbnail::generate(&data, &self.cfg.thumbnail);
            self.set_ready(tenant, id, thumb).await?;
        } else {
            self.set_ready(tenant, id, None).await?;
        }
        let conn = self.db.conn()?;
        let row = attachments::Entity::find()
            .filter(attachments::Column::Id.eq(row.id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::AttachmentNotFound(id.to_string()))?;
        Ok(AttachmentView::from_model(&row))
    }

    async fn secondary_upload(&self, up: &UploadContext, id: Uuid, name: &str, mime: &str, data: Bytes) {
        let Ok(provider) = self.resolver.resolve(&up.provider_id, &up.chat.tenant_id.to_string()) else {
            return;
        };
        let (status, file) = match self.storage.upload_anthropic_file(&provider.alias, name, mime, data).await {
            Ok(f) => ("uploaded", Some(f)),
            Err(e) => {
                tracing::warn!(error = %e, "secondary (Anthropic) upload failed");
                ("failed", None)
            }
        };
        if let Ok(conn) = self.db.conn() {
            attachments::Entity::update_many()
                .col_expr(attachments::Column::SecondaryStatus, Expr::value(status))
                .col_expr(attachments::Column::SecondaryFileId, Expr::value(file))
                .col_expr(attachments::Column::SecondaryProviderKind, Expr::value(Some("anthropic".to_owned())))
                .filter(attachments::Column::Id.eq(id))
                .secure()
                .scope_with(&AccessScope::for_tenant(up.chat.tenant_id))
                .exec(&conn)
                .await.ok();
        }
    }

    fn best_effort_delete(&self, storage: &StorageTarget, file_id: &str) {
        let s = self.storage.clone();
        let t = storage.clone();
        let f = file_id.to_owned();
        tokio::spawn(async move {
            if let Err(e) = s.delete_file(&t, &f).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    async fn set_ready(&self, tenant: Uuid, id: Uuid, thumb: Option<thumbnail::Thumbnail>) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let mut upd = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value("ready"))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now()));
        if let Some(t) = thumb {
            upd = upd
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
        upd.filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// Get-or-create the chat vector store (placeholder + CAS protocol).
    ///
    /// # Errors
    /// 503 when the provider fails or the store does not appear; 409 mismatch.
    pub async fn ensure_vector_store(&self, chat: &chats::Model, storage: &StorageTarget) -> Result<String, DomainError> {
        let scope = AccessScope::for_tenant(chat.tenant_id);
        let chat_filter = || {
            Condition::all()
                .add(chat_vector_stores::Column::TenantId.eq(chat.tenant_id))
                .add(chat_vector_stores::Column::ChatId.eq(chat.id))
        };
        for _restart in 0..3 {
            let conn = self.db.conn()?;
            if let Some(row) = chat_vector_stores::Entity::find()
                .filter(chat_filter())
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?
            {
                if row.provider != storage.backend_label {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
                let age = time::OffsetDateTime::now_utc() - row.created_at;
                if age.whole_seconds() > i64::try_from(STALE_PLACEHOLDER.as_secs()).unwrap_or(120) {
                    chat_vector_stores::Entity::delete_many()
                        .filter(chat_filter().add(chat_vector_stores::Column::VectorStoreId.is_null()))
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
                id: Set(row_id),
                tenant_id: Set(chat.tenant_id),
                chat_id: Set(chat.id),
                vector_store_id: Set(None),
                provider: Set(storage.backend_label.clone()),
                file_count: Set(0),
                created_at: Set(now()),
            };
            let ins = chat_vector_stores::Entity::insert(am)
                .secure()
                .scope_unchecked(&scope)?
                .exec(&conn)
                .await;
            if let Err(e) = ins {
                let e: DomainError = e.into();
                if e.is_unique_violation() {
                    return self.poll_vector_store(chat).await;
                }
                return Err(e);
            }
            let created = self
                .storage
                .create_vector_store(storage, &format!("chat_{}", chat.id))
                .await;
            let vs = match created {
                Ok(v) => v,
                Err(e) => {
                    chat_vector_stores::Entity::delete_many()
                        .filter(chat_vector_stores::Column::Id.eq(row_id))
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await.ok();
                    return Err(DomainError::StorageUnavailable(format!("vector store create: {e}")));
                }
            };
            let res = chat_vector_stores::Entity::update_many()
                .col_expr(chat_vector_stores::Column::VectorStoreId, Expr::value(Some(vs.clone())))
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::Id.eq(row_id))
                        .add(chat_vector_stores::Column::VectorStoreId.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await?;
            if res.rows_affected == 1 {
                return Ok(vs);
            }
            self.storage.delete_vector_store(storage, &vs).await.ok();
            return self.poll_vector_store(chat).await;
        }
        Err(DomainError::StorageUnavailable("vector store unavailable".into()))
    }

    async fn poll_vector_store(&self, chat: &chats::Model) -> Result<String, DomainError> {
        let scope = AccessScope::for_tenant(chat.tenant_id);
        let mut delay = Duration::from_millis(100);
        for _ in 0..5 {
            tokio::time::sleep(delay).await;
            delay *= 2;
            let conn = self.db.conn()?;
            if let Some(vs) = chat_vector_stores::Entity::find()
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::TenantId.eq(chat.tenant_id))
                        .add(chat_vector_stores::Column::ChatId.eq(chat.id)),
                )
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?
                .and_then(|r| r.vector_store_id)
            {
                return Ok(vs);
            }
        }
        Err(DomainError::StorageUnavailable("vector store creation in progress".into()))
    }

    async fn index_document(
        &self,
        storage: &StorageTarget,
        vs: &str,
        file_id: &str,
        attachment_id: Uuid,
        started: Instant,
    ) -> IndexOutcome {
        let deadline = started + INDEXING_DEADLINE;
        let initial = match tokio::time::timeout_at(
            deadline.into(),
            self.storage
                .add_file_to_vector_store(storage, vs, file_id, &attachment_id.to_string()),
        )
        .await
        {
            Err(_) => return IndexOutcome::StillIndexing,
            Ok(Err(e)) => return IndexOutcome::Failed(e.to_string()),
            Ok(Ok(s)) => s,
        };
        match initial.as_deref() {
            Some("completed") => return IndexOutcome::Ready,
            Some("in_progress") | None => {}
            Some(other) => return IndexOutcome::Failed(format!("indexing status {other}")),
        }
        let mut delay = Duration::from_millis(250);
        loop {
            let now_i = Instant::now();
            if now_i >= deadline {
                return IndexOutcome::StillIndexing;
            }
            tokio::time::sleep(delay.min(deadline - now_i)).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            if Instant::now() >= deadline {
                return IndexOutcome::StillIndexing;
            }
            let r = tokio::time::timeout_at(
                deadline.into(),
                self.storage.get_vector_store_file_status(storage, vs, file_id),
            )
            .await;
            match r {
                Err(_) => return IndexOutcome::StillIndexing,
                Ok(Err(e)) if e.is_transient() => {}
                Ok(Err(e)) => return IndexOutcome::Failed(e.to_string()),
                Ok(Ok(Some(s))) if s == "completed" => return IndexOutcome::Ready,
                Ok(Ok(Some(s))) if s == "in_progress" => {}
                Ok(Ok(None)) => {}
                Ok(Ok(Some(s))) => return IndexOutcome::Failed(format!("indexing status {s}")),
            }
        }
    }

    /// Background indexing after the request deadline (rounds of 20 s, max 10 min).
    // reason: polling state machine with retry/cleanup branches; splitting risks behaviour drift
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn background_indexing(
        self: Arc<Self>,
        tenant: Uuid,
        chat_id: Uuid,
        id: Uuid,
        storage: StorageTarget,
        vs: String,
        file_id: String,
    ) {
        let scope = AccessScope::for_tenant(tenant);
        let start = Instant::now();
        let mut last_err: Option<StorageError> = None;
        let live_cond = || {
            Condition::all()
                .add(attachments::Column::Id.eq(id))
                .add(attachments::Column::Status.eq("uploaded"))
                .add(attachments::Column::CleanupStatus.is_null())
                .add(attachments::Column::DeletedAt.is_null())
        };
        let outcome: Result<bool, String> = 'outer: loop {
            if start.elapsed() >= BACKGROUND_LIMIT {
                break Err(format!("indexing timed out (last error: {last_err:?})"));
            }
            // heartbeat
            let Ok(conn) = self.db.conn() else { return };
            match attachments::Entity::update_many()
                .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                .filter(live_cond())
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await
            {
                Ok(r) if r.rows_affected == 0 => return,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "indexing heartbeat failed"),
            }
            let round_end = Instant::now() + BACKGROUND_ROUND;
            let mut delay = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                match self.storage.get_vector_store_file_status(&storage, &vs, &file_id).await {
                    Ok(Some(s)) if s == "completed" => break 'outer Ok(true),
                    Ok(Some(s)) if s == "in_progress" => {}
                    Ok(None) => {}
                    Ok(Some(s)) => break 'outer Err(format!("indexing status {s}")),
                    Err(e) if e.is_transient() => {
                        if last_err.is_none() {
                            tracing::warn!(error = %e, "transient indexing status error");
                        }
                        last_err = Some(e);
                    }
                    Err(e) => break 'outer Err(e.to_string()),
                }
            }
        };
        match outcome {
            Ok(_) => {
                for attempt in 0..4u32 {
                    if attempt > 0 {
                        tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
                    }
                    let Ok(conn) = self.db.conn() else { continue };
                    match attachments::Entity::update_many()
                        .col_expr(attachments::Column::Status, Expr::value("ready"))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(now()))
                        .filter(live_cond())
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await
                    {
                        Ok(_) => return,
                        Err(e) => tracing::warn!(error = %e, "set ready failed"),
                    }
                }
            }
            Err(why) => {
                tracing::warn!(attachment_id = %id, reason = %why, "background indexing failed");
                let outbox = self.outbox.clone();
                let label = storage.backend_label.clone();
                let fid = file_id.clone();
                let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                        let fid = fid.clone();
                        let label = label.clone();
                        let outbox = outbox.clone();
                        Box::pin(async move {
                            let ts = now();
                            let r = attachments::Entity::update_many()
                                .col_expr(attachments::Column::Status, Expr::value("failed"))
                                .col_expr(attachments::Column::ErrorCode, Expr::value(Some("indexing_failed".to_owned())))
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                                .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                                .filter(
                                    Condition::all()
                                        .add(attachments::Column::Id.eq(id))
                                        .add(attachments::Column::Status.eq("uploaded"))
                                        .add(attachments::Column::CleanupStatus.is_null())
                                        .add(attachments::Column::DeletedAt.is_null()),
                                )
                                .secure()
                                .scope_with(&AccessScope::for_tenant(tenant))
                                .exec(tx)
                                .await?;
                            if r.rows_affected == 0 {
                                return Ok(None);
                            }
                            let ev = AttachmentCleanupEvent {
                                event_type: "attachment_indexing_failed".into(),
                                tenant_id: tenant,
                                chat_id,
                                attachment_id: id,
                                provider_file_id: Some(fid),
                                vector_store_id: None,
                                storage_backend: label,
                                attachment_kind: "document".into(),
                                deleted_at: rfc3339(ts),
                                secondary_ref: None,
                            };
                            Ok(Some(outbox.enqueue(tx, Queue::AttachmentCleanup, tenant, &ev).await?))
                        })
                    })
                    .await;
                match res {
                    Ok(Some(w)) => w.fire(),
                    Ok(None) => {}
                    Err(e) => tracing::error!(error = %e, "failed to record indexing failure"),
                }
            }
        }
    }

    async fn load_attachment(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        chat: &chats::Model,
        id: Uuid,
    ) -> Result<attachments::Model, DomainError> {
        attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.eq(id))
                    .add(attachments::Column::ChatId.eq(chat.id))
                    .add(attachments::Column::UploadedByUserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .one(runner)
            .await?
            .ok_or_else(|| DomainError::AttachmentNotFound(id.to_string()))
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404 (chat or attachment).
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<AttachmentView, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::READ_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let a = self.load_attachment(&conn, ctx, &chat, id).await?;
        if a.deleted_at.is_some() {
            return Err(DomainError::AttachmentNotFound(id.to_string()));
        }
        Ok(AttachmentView::from_model(&a))
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404, 409 `attachment_locked`, 500 (outbox).
    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::DELETE_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let a = self.load_attachment(&conn, ctx, &chat, id).await?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let scope = AccessScope::for_tenant(chat.tenant_id);
        let links = message_attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat.id))
                    .add(message_attachments::Column::AttachmentId.eq(id)),
            )
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        if !links.is_empty() {
            let msg_ids: Vec<Uuid> = links.iter().map(|l| l.message_id).collect();
            let referenced = messages::Entity::find()
                .filter(
                    Condition::all()
                        .add(messages::Column::Id.is_in(msg_ids))
                        .add(messages::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .count(&conn)
                .await?;
            if referenced > 0 {
                return Err(DomainError::AttachmentLocked);
            }
        }
        let secondary_ref = match (&a.secondary_file_id, a.secondary_status.as_str()) {
            (Some(f), "uploaded") => self
                .resolver
                .providers()
                .iter()
                .find(|(_, e)| e.kind == crate::config::ProviderKind::AnthropicMessages)
                .map(|(pid, _)| SecondaryRef {
                    file_id: f.clone(),
                    provider_kind: "anthropic".into(),
                    upstream_alias: self
                        .resolver
                        .resolve(pid, &chat.tenant_id.to_string())
                        .map(|p| p.alias)
                        .unwrap_or_default(),
                }),
            _ => None,
        };
        let outbox = self.outbox.clone();
        let a2 = a.clone();
        let wake = crate::infra::db::tx_retry(&self.db, move |tx| {
                let a2 = a2.clone();
                let outbox = outbox.clone();
                let secondary_ref = secondary_ref.clone();
                Box::pin(async move {
                    let ts = now();
                    let r = attachments::Entity::update_many()
                        .col_expr(attachments::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts))
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::Id.eq(a2.id))
                                .add(attachments::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(a2.tenant_id))
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(None);
                    }
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_deleted".into(),
                        tenant_id: a2.tenant_id,
                        chat_id: a2.chat_id,
                        attachment_id: a2.id,
                        provider_file_id: a2.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a2.storage_backend.clone(),
                        attachment_kind: a2.attachment_kind.clone(),
                        deleted_at: rfc3339(ts),
                        secondary_ref,
                    };
                    Ok(Some(outbox.enqueue(tx, Queue::AttachmentCleanup, a2.tenant_id, &ev).await?))
                })
            })
            .await?;
        if let Some(w) = wake {
            w.fire();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "attachment_service_tests.rs"]
mod tests;
