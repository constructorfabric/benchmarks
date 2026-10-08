//! Attachments: upload (synchronous with bounded indexing wait), status, delete (DESIGN §3.3, §3.6).

use crate::infra::db::WriteTransaction;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::time::Instant;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chats::tenant_scope;
use super::stream::is_image_mime;
use super::{Core, log_best_effort, now};
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, Resource};
use crate::infra::db::entities::{attachment, chat, message_attachment, vector_store};
use crate::infra::llm::provider::ResolvedProvider;
use crate::infra::llm::storage::StorageError;
use crate::infra::outbox::QueueKind;

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Tunable waits of the upload pipeline (DESIGN values by default; tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct UploadTimings {
    pub sync_deadline: Duration,
    pub sync_max_backoff: Duration,
    pub initial_backoff: Duration,
    pub background_total: Duration,
    pub background_round: Duration,
    pub background_max_backoff: Duration,
    pub loser_polls: u32,
    pub stale_placeholder: Duration,
}

impl Default for UploadTimings {
    fn default() -> Self {
        Self {
            sync_deadline: Duration::from_secs(25),
            sync_max_backoff: Duration::from_secs(2),
            initial_backoff: Duration::from_millis(250),
            background_total: Duration::from_secs(600),
            background_round: Duration::from_secs(20),
            background_max_backoff: Duration::from_secs(5),
            loser_polls: 5,
            stale_placeholder: Duration::from_secs(120),
        }
    }
}

/// Attachment-cleanup outbox payload (DESIGN §4 "Attachment Deletion").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<serde_json::Value>,
}

impl AttachmentCleanupPayload {
    #[must_use]
    pub fn from_row(a: &attachment::Model, event_type: &str, ts: OffsetDateTime) -> Self {
        Self {
            event_type: event_type.to_owned(),
            tenant_id: a.tenant_id,
            chat_id: a.chat_id,
            attachment_id: a.id,
            provider_file_id: a.provider_file_id.clone(),
            vector_store_id: None,
            storage_backend: a.storage_backend.clone(),
            attachment_kind: a.attachment_kind.clone(),
            deleted_at: ts,
            secondary_ref: None,
        }
    }
}

/// Upload request prepared by the handler (body already read with the size limit).
pub struct UploadInput {
    pub filename: String,
    pub content_type: String,
    pub data: Bytes,
}

/// Limits resolved before the body is read.
#[derive(Debug, Clone)]
pub struct UploadContext {
    pub chat: chat::Model,
    pub model: ModelCatalogEntry,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
    pub max_document_bytes: u64,
    pub max_image_bytes: u64,
}

impl UploadContext {
    #[must_use]
    pub fn limit_for(&self, mime: &str) -> u64 {
        if is_image_mime(mime) {
            self.max_image_bytes
        } else {
            self.max_document_bytes
        }
    }

    /// MIME / kill-switch / purpose validation (before the body is buffered).
    ///
    /// # Errors
    /// 400 `UNSUPPORTED_CONTENT_TYPE` / `FEATURE_DISABLED` / `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn validate_upload_type(&self, mime: &str) -> Result<(bool, bool), DomainError> {
        if is_image_mime(mime) {
            if self.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            return Ok((false, false));
        }
        if !is_supported_document(mime) {
            return Err(DomainError::invalid(
                Resource::Attachment,
                "content_type",
                "UNSUPPORTED_CONTENT_TYPE",
                format!("unsupported content type '{mime}'"),
            ));
        }
        if mime == XLSX {
            let ci_ok =
                !self.disable_code_interpreter && self.model.tool_support().code_interpreter;
            if !ci_ok {
                return Err(DomainError::invalid(
                    Resource::Attachment,
                    "file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                    "spreadsheets need the code interpreter, which is unavailable for this chat",
                ));
            }
            return Ok((false, true));
        }
        Ok((true, false))
    }
}

/// Normalizes a MIME type and infers it from the extension for `application/octet-stream`.
#[must_use]
pub fn resolve_mime(part_ct: &str, filename: &str, allow_csv: bool) -> String {
    let base = part_ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mime = if base == "application/octet-stream" || base.is_empty() {
        infer_from_extension(filename).unwrap_or(base)
    } else {
        base
    };
    if allow_csv && (mime == "text/csv" || mime == "application/csv") {
        "text/plain".to_owned()
    } else {
        mime
    }
}

fn infer_from_extension(filename: &str) -> Option<String> {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())?;
    let m = match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" | "log" => "text/plain",
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
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(m.to_owned())
}

/// Supported document MIME types (images handled separately).
#[must_use]
pub fn is_supported_document(mime: &str) -> bool {
    matches!(
        mime,
        "application/pdf"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | XLSX
            | "text/plain"
            | "text/markdown"
            | "text/x-markdown"
            | "text/html"
            | "application/json"
            | "text/x-python"
            | "text/x-script.python"
            | "application/x-python"
            | "text/x-java"
            | "text/x-java-source"
            | "text/javascript"
            | "application/javascript"
            | "text/typescript"
            | "application/typescript"
            | "text/x-typescript"
            | "text/x-rust"
            | "text/rust"
            | "text/x-go"
            | "text/x-golang"
            | "text/x-csharp"
            | "text/x-ruby"
            | "application/x-ruby"
            | "application/sql"
            | "text/x-sql"
    )
}

/// Truncates a filename to 255 characters keeping the extension; empty → `upload`.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let n = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("upload");
    if n.chars().count() <= 255 {
        return n.to_owned();
    }
    match n.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{}", stem.chars().take(keep).collect::<String>(), ext)
        }
        _ => n.chars().take(255).collect(),
    }
}

/// Generates a WebP thumbnail fitting `w x h` (best effort).
#[must_use]
pub fn make_thumbnail(
    data: &[u8],
    cfg: &crate::config::ThumbnailConfig,
) -> Option<(Vec<u8>, u32, u32)> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let (iw, ih) = reader.into_dimensions().ok()?;
    let pixels = u64::from(iw) * u64::from(ih);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height).to_rgba8();
    let (w, h) = (thumb.width(), thumb.height());
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(thumb.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .ok()?;
    if out.len() > cfg.max_bytes {
        return None;
    }
    Some((out, w, h))
}

fn storage_unavailable() -> DomainError {
    DomainError::ServiceUnavailable {
        retry_after: 10,
        detail: "Service temporarily unavailable".to_owned(),
    }
}

/// Indexing status classification.
enum Indexing {
    Completed,
    InProgress,
    Failed,
}

fn classify(status: Option<&str>) -> Indexing {
    match status {
        None | Some("in_progress") => Indexing::InProgress,
        Some("completed") => Indexing::Completed,
        Some(_) => Indexing::Failed,
    }
}

impl Core {
    /// Authorization, chat lookup and model resolution before the body is read.
    ///
    /// # Errors
    /// 404 chat, 400 `INVALID_MODEL`, 403/503 PEP, 500 policy.
    pub async fn prepare_upload(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<UploadContext, DomainError> {
        let chat = self
            .authorize_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id)
            .await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        let model = snapshot
            .find_model(&chat.model)
            .cloned()
            .ok_or_else(DomainError::invalid_model)?;
        let per_model = u64::from(model.general_config.max_file_size_mb) * 1024 * 1024;
        let cap = |kb: u32| {
            let c = u64::from(kb) * 1024;
            if per_model > 0 { c.min(per_model) } else { c }
        };
        Ok(UploadContext {
            chat,
            model,
            disable_images: snapshot.kill_switches.disable_images,
            disable_code_interpreter: snapshot.kill_switches.disable_code_interpreter,
            max_document_bytes: cap(self.cfg.rag.uploaded_file_max_size_kb),
            max_image_bytes: cap(self.cfg.rag.uploaded_image_max_size_kb),
        })
    }

    /// Uploads an attachment synchronously; returns the stored row.
    ///
    /// # Errors
    /// 429 limits, 409 provider mismatch, 503 storage, 500 internal.
    #[allow(
        clippy::cognitive_complexity,
        reason = "linear upload pipeline (validate, insert, upload, index); splitting obscures the sequence"
    )]
    pub async fn upload_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        uc: UploadContext,
        inp: UploadInput,
    ) -> Result<attachment::Model, DomainError> {
        let started = Instant::now();
        let mime = inp.content_type.clone();
        let (for_file_search, for_code_interpreter) = uc.validate_upload_type(&mime)?;
        let kind = if is_image_mime(&mime) {
            "image"
        } else {
            "document"
        };
        let chat = &uc.chat;
        let storage = self
            .providers
            .resolve_storage(&uc.model.provider_id, chat.tenant_id)?;
        let size = i64::try_from(inp.data.len()).unwrap_or(i64::MAX);
        let scope = tenant_scope(chat.tenant_id);

        // Provider mismatch of an existing chat vector store.
        if for_file_search {
            let conn = self.db.conn()?;
            if let Some(vs) = vector_store::Entity::find()
                .filter(vector_store::Column::ChatId.eq(chat.id))
                .filter(vector_store::Column::TenantId.eq(chat.tenant_id))
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?
                && vs.provider != storage.storage_backend
            {
                return Err(DomainError::AlreadyExists {
                    resource: Resource::Attachment,
                    name: "provider_mismatch",
                    detail: "The chat's documents are stored with another provider".to_owned(),
                });
            }
        }

        // Insert the pending row with the per-chat limits checked in the same transaction.
        let id = Uuid::new_v4();
        let ts = now();
        let row = attachment::Model {
            id,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            uploaded_by_user_id: ctx.subject_id(),
            filename: inp.filename.clone(),
            content_type: mime.clone(),
            size_bytes: size,
            storage_backend: storage.storage_backend.clone(),
            provider_file_id: None,
            status: "pending".to_owned(),
            error_code: None,
            attachment_kind: kind.to_owned(),
            for_file_search,
            for_code_interpreter,
            doc_summary: None,
            img_thumbnail: None,
            img_thumbnail_width: None,
            img_thumbnail_height: None,
            summary_model: None,
            summary_updated_at: None,
            cleanup_status: None,
            cleanup_attempts: 0,
            last_cleanup_error: None,
            cleanup_updated_at: None,
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
            secondary_provider_kind: None,
        };
        let max_docs = i64::from(self.cfg.rag.max_documents_per_chat);
        let max_total = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let row_c = row.clone();
        self.db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let scope = tenant_scope(row_c.tenant_id);
                    let live = attachment::Entity::find()
                        .filter(
                            Condition::all()
                                .add(attachment::Column::ChatId.eq(row_c.chat_id))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::Status.ne("failed")),
                        )
                        .secure()
                        .scope_with(&scope)
                        .all(tx)
                        .await?;
                    if row_c.attachment_kind == "document" {
                        let docs = live
                            .iter()
                            .filter(|a| a.attachment_kind == "document")
                            .count();
                        if i64::try_from(docs).unwrap_or(i64::MAX) >= max_docs {
                            return Err(DomainError::ResourceExhausted {
                                resource: Resource::Attachment,
                                subject: "document_limit".to_owned(),
                                description: "Per-chat document limit reached".to_owned(),
                            });
                        }
                    }
                    let total: i64 = live.iter().map(|a| a.size_bytes).sum();
                    if total.saturating_add(row_c.size_bytes) > max_total {
                        return Err(DomainError::ResourceExhausted {
                            resource: Resource::Attachment,
                            subject: "storage_limit".to_owned(),
                            description: "Per-chat storage limit reached".to_owned(),
                        });
                    }
                    insert_attachment(tx, &row_c).await
                })
            })
            .await?;

        // Provider upload.
        let sys = ctx.clone();
        let file_id = match self
            .storage
            .upload_file(
                &sys,
                &storage,
                &format!("{}_{}.{}", chat.id, id, extension_of(&inp.filename)),
                &mime,
                inp.data.clone(),
            )
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %id, "mini-chat: provider file upload failed");
                self.mark_attachment_failed(chat.tenant_id, id, "upload_failed")
                    .await;
                return Err(storage_unavailable());
            }
        };
        self.set_attachment_fields(chat.tenant_id, id, |u| {
            u.col_expr(attachment::Column::Status, Expr::value("uploaded"))
                .col_expr(
                    attachment::Column::ProviderFileId,
                    Expr::value(Some(file_id.clone())),
                )
        })
        .await?;

        if kind == "image" {
            let thumb = make_thumbnail(&inp.data, &self.cfg.thumbnail);
            self.set_attachment_fields(chat.tenant_id, id, |u| {
                let u = u.col_expr(attachment::Column::Status, Expr::value("ready"));
                match &thumb {
                    Some((b, w, h)) => u
                        .col_expr(
                            attachment::Column::ImgThumbnail,
                            Expr::value(Some(b.clone())),
                        )
                        .col_expr(
                            attachment::Column::ImgThumbnailWidth,
                            Expr::value(Some(i32::try_from(*w).unwrap_or(0))),
                        )
                        .col_expr(
                            attachment::Column::ImgThumbnailHeight,
                            Expr::value(Some(i32::try_from(*h).unwrap_or(0))),
                        ),
                    None => u,
                }
            })
            .await?;
        } else if for_file_search {
            match self
                .index_document(ctx, chat, &storage, id, &file_id, started)
                .await
            {
                Ok(true) => {
                    self.set_attachment_fields(chat.tenant_id, id, |u| {
                        u.col_expr(attachment::Column::Status, Expr::value("ready"))
                    })
                    .await?;
                }
                Ok(false) => {
                    // Still indexing at the request deadline: continue in the background.
                    let core = Arc::clone(self);
                    let tenant = chat.tenant_id;
                    let chat_id = chat.id;
                    let storage_c = storage.clone();
                    let file_c = file_id.clone();
                    let ctx_c = ctx.clone();
                    tokio::spawn(async move {
                        core.background_indexing(ctx_c, tenant, chat_id, storage_c, id, file_c)
                            .await;
                    });
                }
                Err(()) => {
                    self.mark_attachment_failed(chat.tenant_id, id, "indexing_failed")
                        .await;
                    if let Err(e) = self.storage.delete_file(ctx, &storage, &file_id).await {
                        tracing::debug!(error = %e, attachment_id = %id, "mini-chat: provider file delete after indexing failure failed");
                    }
                    return Err(storage_unavailable());
                }
            }
        } else {
            self.set_attachment_fields(chat.tenant_id, id, |u| {
                u.col_expr(attachment::Column::Status, Expr::value("ready"))
            })
            .await?;
        }
        self.load_attachment_row(chat.tenant_id, id).await
    }

    /// Gets or creates the chat vector store, adds the file and polls until the deadline.
    /// `Ok(true)` = indexed, `Ok(false)` = still in progress, `Err(())` = failed.
    async fn index_document(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        storage: &ResolvedProvider,
        attachment_id: Uuid,
        file_id: &str,
        started: Instant,
    ) -> Result<bool, ()> {
        let t = self.upload_timings;
        let deadline = started + t.sync_deadline;
        let vs = match self.ensure_vector_store(ctx, chat, storage).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "mini-chat: vector store unavailable");
                return Err(());
            }
        };
        let status = match self
            .storage
            .add_file_to_vector_store(ctx, storage, &vs, file_id, attachment_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "mini-chat: adding file to vector store failed");
                return Err(());
            }
        };
        match classify(status.as_deref()) {
            Indexing::Completed => return Ok(true),
            Indexing::Failed => return Err(()),
            Indexing::InProgress => {}
        }
        let mut wait = t.initial_backoff;
        loop {
            let now_i = Instant::now();
            if now_i >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(wait.min(deadline - now_i)).await;
            wait = (wait * 2).min(t.sync_max_backoff);
            if Instant::now() >= deadline {
                return Ok(false);
            }
            match self
                .storage
                .get_vector_store_file_status(ctx, storage, &vs, file_id)
                .await
            {
                Ok(s) => match classify(s.as_deref()) {
                    Indexing::Completed => return Ok(true),
                    Indexing::Failed => return Err(()),
                    Indexing::InProgress => {}
                },
                Err(StorageError::Transient(_)) => {}
                Err(StorageError::Failed { .. }) => return Err(()),
            }
        }
    }

    /// Vector store creation protocol (DESIGN §3.7 `chat_vector_stores`).
    async fn ensure_vector_store(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        storage: &ResolvedProvider,
    ) -> Result<String, DomainError> {
        let t = self.upload_timings;
        let scope = tenant_scope(chat.tenant_id);
        for _attempt in 0..2 {
            let conn = self.db.conn()?;
            let existing = vector_store::Entity::find()
                .filter(vector_store::Column::ChatId.eq(chat.id))
                .filter(vector_store::Column::TenantId.eq(chat.tenant_id))
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?;
            if let Some(row) = &existing {
                if let Some(id) = &row.vector_store_id {
                    return Ok(id.clone());
                }
                let age = now() - row.created_at;
                if age
                    > time::Duration::try_from(t.stale_placeholder)
                        .unwrap_or(time::Duration::MINUTE)
                {
                    vector_store::Entity::delete_many()
                        .filter(
                            Condition::all()
                                .add(vector_store::Column::Id.eq(row.id))
                                .add(vector_store::Column::VectorStoreId.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(&conn)
                        .await?;
                    continue;
                }
            }
            let placeholder_id = Uuid::new_v4();
            let am = vector_store::ActiveModel {
                id: ActiveValue::Set(placeholder_id),
                tenant_id: ActiveValue::Set(chat.tenant_id),
                chat_id: ActiveValue::Set(chat.id),
                vector_store_id: ActiveValue::Set(None),
                provider: ActiveValue::Set(storage.storage_backend.clone()),
                file_count: ActiveValue::Set(0),
                created_at: ActiveValue::Set(now()),
            };
            let inserted = if existing.is_some() {
                false
            } else {
                let conn = self.db.conn()?;
                vector_store::Entity::insert(am)
                    .secure()
                    .scope_unchecked(&scope)?
                    .exec(&conn)
                    .await
                    .is_ok()
            };
            if inserted {
                let created = self
                    .storage
                    .create_vector_store(ctx, storage, &format!("chat-{}", chat.id))
                    .await;
                let conn = self.db.conn()?;
                let vs_id = match created {
                    Ok(v) => v,
                    Err(e) => {
                        log_best_effort(
                            vector_store::Entity::delete_many()
                                .filter(vector_store::Column::Id.eq(placeholder_id))
                                .secure()
                                .scope_with(&scope)
                                .exec(&conn)
                                .await,
                            "vector store placeholder delete",
                        );
                        return Err(DomainError::ServiceUnavailable {
                            retry_after: 10,
                            detail: format!("vector store creation failed: {e}"),
                        });
                    }
                };
                let res = vector_store::Entity::update_many()
                    .col_expr(
                        vector_store::Column::VectorStoreId,
                        Expr::value(Some(vs_id.clone())),
                    )
                    .filter(
                        Condition::all()
                            .add(vector_store::Column::Id.eq(placeholder_id))
                            .add(vector_store::Column::VectorStoreId.is_null()),
                    )
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await?;
                if res.rows_affected == 1 {
                    return Ok(vs_id);
                }
                log_best_effort(
                    self.storage.delete_vector_store(ctx, storage, &vs_id).await,
                    "losing vector store delete",
                );
            }
            // Loser path: poll for the winner's id.
            let mut wait = t.initial_backoff;
            for _ in 0..t.loser_polls {
                tokio::time::sleep(wait).await;
                wait *= 2;
                let conn = self.db.conn()?;
                if let Some(id) = vector_store::Entity::find()
                    .filter(vector_store::Column::ChatId.eq(chat.id))
                    .filter(vector_store::Column::TenantId.eq(chat.tenant_id))
                    .secure()
                    .scope_with(&scope)
                    .one(&conn)
                    .await?
                    .and_then(|r| r.vector_store_id)
                {
                    return Ok(id);
                }
            }
            return Err(DomainError::ServiceUnavailable {
                retry_after: 10,
                detail: "vector store not available".to_owned(),
            });
        }
        Err(DomainError::ServiceUnavailable {
            retry_after: 10,
            detail: "vector store not available".to_owned(),
        })
    }

    /// Background indexing of an `uploaded` document (rounds with `updated_at` heartbeat).
    async fn background_indexing(
        self: Arc<Self>,
        ctx: SecurityContext,
        tenant: Uuid,
        chat_id: Uuid,
        storage: ResolvedProvider,
        attachment_id: Uuid,
        file_id: String,
    ) {
        let t = self.upload_timings;
        let end = Instant::now() + t.background_total;
        let scope = tenant_scope(tenant);
        let vs = {
            let Ok(conn) = self.db.conn() else { return };
            match vector_store::Entity::find()
                .filter(vector_store::Column::ChatId.eq(chat_id))
                .filter(vector_store::Column::TenantId.eq(tenant))
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await
            {
                Ok(Some(v)) => v.vector_store_id,
                _ => None,
            }
        };
        let Some(vs) = vs else { return };
        let mut outcome: Option<bool> = None;
        'outer: while Instant::now() < end {
            if self.shutdown.is_cancelled() {
                return;
            }
            // Heartbeat: refresh updated_at; stop when the row is no longer ours to finish.
            let Ok(conn) = self.db.conn() else { return };
            let alive = attachment::Entity::update_many()
                .col_expr(attachment::Column::UpdatedAt, Expr::value(now()))
                .filter(
                    Condition::all()
                        .add(attachment::Column::Id.eq(attachment_id))
                        .add(attachment::Column::Status.eq("uploaded"))
                        .add(attachment::Column::CleanupStatus.is_null())
                        .add(attachment::Column::DeletedAt.is_null()),
                )
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await
                .is_ok_and(|r| r.rows_affected == 1);
            if !alive {
                return;
            }
            let round_end = (Instant::now() + t.background_round).min(end);
            let mut wait = t.initial_backoff;
            while Instant::now() < round_end {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                wait = (wait * 2).min(t.background_max_backoff);
                match self
                    .storage
                    .get_vector_store_file_status(&ctx, &storage, &vs, &file_id)
                    .await
                {
                    Ok(s) => match classify(s.as_deref()) {
                        Indexing::Completed => {
                            outcome = Some(true);
                            break 'outer;
                        }
                        Indexing::Failed => {
                            outcome = Some(false);
                            break 'outer;
                        }
                        Indexing::InProgress => {}
                    },
                    Err(StorageError::Transient(_)) => {}
                    Err(StorageError::Failed { .. }) => {
                        outcome = Some(false);
                        break 'outer;
                    }
                }
            }
        }
        if outcome == Some(true) {
            for delay in [0u64, 1, 2, 4] {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
                let Ok(conn) = self.db.conn() else { continue };
                let res = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value("ready"))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now()))
                    .filter(
                        Condition::all()
                            .add(attachment::Column::Id.eq(attachment_id))
                            .add(attachment::Column::Status.eq("uploaded"))
                            .add(attachment::Column::CleanupStatus.is_null()),
                    )
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await;
                if res.is_ok() {
                    return;
                }
            }
            return;
        }
        // Failure or timeout: failed + cleanup_status pending + attachment cleanup event in one txn.
        let core = Arc::clone(&self);
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let scope = tenant_scope(tenant);
                    let n = attachment::Entity::update_many()
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value(Some("indexing_failed".to_owned())),
                        )
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending".to_owned())),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(attachment_id))
                                .add(attachment::Column::Status.eq("uploaded"))
                                .add(attachment::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if n == 0 {
                        return Ok(Wake::empty());
                    }
                    let row = attachment::Entity::find()
                        .filter(attachment::Column::Id.eq(attachment_id))
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?
                        .ok_or_else(|| DomainError::internal("attachment vanished"))?;
                    let payload =
                        AttachmentCleanupPayload::from_row(&row, "attachment_indexing_failed", ts);
                    core.outbox
                        .enqueue(tx, QueueKind::AttachmentCleanup, tenant, &payload)
                        .await
                })
            })
            .await;
        if let Ok(w) = res {
            w.fire();
        }
    }

    async fn set_attachment_fields<F>(
        &self,
        tenant: Uuid,
        id: Uuid,
        f: F,
    ) -> Result<(), DomainError>
    where
        F: FnOnce(
            toolkit_db::secure::SecureUpdateMany<attachment::Entity, toolkit_db::secure::Unscoped>,
        ) -> toolkit_db::secure::SecureUpdateMany<
            attachment::Entity,
            toolkit_db::secure::Unscoped,
        >,
    {
        let conn = self.db.conn()?;
        let base = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now()))
            .filter(attachment::Column::Id.eq(id))
            .secure();
        f(base)
            .scope_with(&tenant_scope(tenant))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// Marks an attachment failed with an error code.
    pub async fn mark_attachment_failed(&self, tenant: Uuid, id: Uuid, code: &str) {
        let code = code.to_owned();
        if let Err(e) = self
            .set_attachment_fields(tenant, id, |u| {
                u.col_expr(attachment::Column::Status, Expr::value("failed"))
                    .col_expr(attachment::Column::ErrorCode, Expr::value(Some(code)))
            })
            .await
        {
            tracing::debug!(error = %e, attachment_id = %id, "mini-chat: marking attachment failed did not persist");
        }
    }

    async fn load_attachment_row(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<attachment::Model, DomainError> {
        let conn = self.db.conn()?;
        attachment::Entity::find()
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&tenant_scope(tenant))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::not_found(Resource::Attachment, &id))
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404 / PEP errors.
    pub async fn get_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<attachment::Model, DomainError> {
        let chat = self
            .authorize_chat(ctx, actions::READ_ATTACHMENT, chat_id)
            .await?;
        let a = self
            .load_attachment_row(chat.tenant_id, attachment_id)
            .await?;
        if a.chat_id != chat_id
            || a.deleted_at.is_some()
            || a.uploaded_by_user_id != ctx.subject_id()
        {
            return Err(DomainError::not_found(Resource::Attachment, &attachment_id));
        }
        Ok(a)
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// 404, 409 `attachment_locked`, PEP errors.
    pub async fn delete_attachment(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let chat = self
            .authorize_chat(ctx, actions::DELETE_ATTACHMENT, chat_id)
            .await?;
        let a = self
            .load_attachment_row(chat.tenant_id, attachment_id)
            .await?;
        if a.chat_id != chat_id || a.uploaded_by_user_id != ctx.subject_id() {
            return Err(DomainError::not_found(Resource::Attachment, &attachment_id));
        }
        if a.deleted_at.is_some() {
            return Ok(());
        }
        let core = Arc::clone(self);
        let wake = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let scope = tenant_scope(a.tenant_id);
                    let refs = message_attachment::Entity::find()
                        .filter(
                            Condition::all()
                                .add(message_attachment::Column::ChatId.eq(a.chat_id))
                                .add(message_attachment::Column::AttachmentId.eq(a.id)),
                        )
                        .secure()
                        .scope_with(&scope)
                        .count(tx)
                        .await?;
                    if refs > 0 {
                        return Err(DomainError::AlreadyExists {
                            resource: Resource::Attachment,
                            name: "attachment_locked",
                            detail: "The attachment is referenced by a sent message".to_owned(),
                        });
                    }
                    let ts = now();
                    let n = attachment::Entity::update_many()
                        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::value(Some("pending".to_owned())),
                        )
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(a.id))
                                .add(attachment::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if n == 0 {
                        return Ok(Wake::empty());
                    }
                    let payload = AttachmentCleanupPayload::from_row(&a, "attachment_deleted", ts);
                    core.outbox
                        .enqueue(tx, QueueKind::AttachmentCleanup, a.tenant_id, &payload)
                        .await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

async fn insert_attachment(tx: &impl DBRunner, a: &attachment::Model) -> Result<(), DomainError> {
    let am = attachment::ActiveModel {
        id: ActiveValue::Set(a.id),
        tenant_id: ActiveValue::Set(a.tenant_id),
        chat_id: ActiveValue::Set(a.chat_id),
        uploaded_by_user_id: ActiveValue::Set(a.uploaded_by_user_id),
        filename: ActiveValue::Set(a.filename.clone()),
        content_type: ActiveValue::Set(a.content_type.clone()),
        size_bytes: ActiveValue::Set(a.size_bytes),
        storage_backend: ActiveValue::Set(a.storage_backend.clone()),
        provider_file_id: ActiveValue::Set(None),
        status: ActiveValue::Set(a.status.clone()),
        error_code: ActiveValue::Set(None),
        attachment_kind: ActiveValue::Set(a.attachment_kind.clone()),
        for_file_search: ActiveValue::Set(a.for_file_search),
        for_code_interpreter: ActiveValue::Set(a.for_code_interpreter),
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
        created_at: ActiveValue::Set(a.created_at),
        updated_at: ActiveValue::Set(a.updated_at),
        deleted_at: ActiveValue::Set(None),
        secondary_file_id: ActiveValue::Set(None),
        secondary_status: ActiveValue::Set("not_attempted".to_owned()),
        secondary_provider_kind: ActiveValue::Set(None),
    };
    attachment::Entity::insert(am)
        .secure()
        .scope_unchecked(&tenant_scope(a.tenant_id))?
        .exec(tx)
        .await?;
    Ok(())
}

fn extension_of(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map_or_else(|| "bin".to_owned(), |(_, e)| e.to_ascii_lowercase())
}
