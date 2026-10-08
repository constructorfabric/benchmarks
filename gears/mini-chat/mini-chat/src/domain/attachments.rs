//! Attachments: upload (multipart, provider Files API, vector store,
//! thumbnails), get, delete, background indexing (DESIGN "File Upload").

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authz::actions;
use super::clock::{self, Timestamp};
use super::error::DomainError;
use super::service::{ChatAccess, MiniChat};
use crate::config::ProviderKind;
use crate::infra::llm::{StorageError, StorageTarget};
use crate::infra::outbox::{OutboxKind, enqueue_json};
use crate::infra::storage::entity::{attachment, chat_vector_store, message, message_attachment};
use crate::infra::thumbnail;

/// Indexing deadline of the upload request (from its start).
pub const UPLOAD_INDEXING_DEADLINE: Duration = Duration::from_secs(25);
/// Background indexing limit.
pub const BACKGROUND_INDEXING_LIMIT: Duration = Duration::from_secs(600);
/// Background indexing heartbeat round.
pub const BACKGROUND_ROUND: Duration = Duration::from_secs(20);
/// Stale vector-store placeholder age.
const PLACEHOLDER_STALE: Duration = Duration::from_secs(120);
const _: () = assert!(BACKGROUND_ROUND.as_secs() * 2 <= 60, "heartbeat must be at most half the minimum stale_after_secs");

pub mod event_types {
    pub const DELETED: &str = "attachment_deleted";
    pub const UPLOAD_ABANDONED: &str = "attachment_upload_abandoned";
    pub const INDEXING_FAILED: &str = "attachment_indexing_failed";
}

/// Secondary (Anthropic) copy reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment-cleanup outbox payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupPayload {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    pub deleted_at: Timestamp,
    pub secondary_ref: Option<SecondaryRef>,
}

const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    XLSX,
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-script.python",
    "application/x-python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "application/x-javascript",
    "text/x-typescript",
    "application/typescript",
    "application/x-typescript",
    "text/x-rust",
    "text/rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-c#",
    "text/x-ruby",
    "application/x-ruby",
    "application/sql",
    "text/x-sql",
];

fn ext_of(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() && !filename.starts_with('.') {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn mime_from_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" | "text" | "log" => "text/plain",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/x-typescript",
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

/// Resolve the effective MIME type of an upload part.
///
/// # Errors
/// `UnsupportedContentType` when the type is not on the allowlist.
pub fn resolve_mime(part_content_type: &str, filename: &str, allow_csv: bool) -> Result<String, DomainError> {
    let base = part_content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let mut mime = base.clone();
    if mime == "application/octet-stream"
        && let Some(m) = ext_of(filename).as_deref().and_then(mime_from_ext)
    {
        m.clone_into(&mut mime);
    }
    if mime == "image/jpg" {
        "image/jpeg".clone_into(&mut mime);
    }
    if mime == "text/csv" || mime == "application/csv" {
        if allow_csv {
            return Ok("text/plain".to_owned());
        }
        return Err(DomainError::UnsupportedContentType { field: "file", content_type: mime });
    }
    if IMAGE_TYPES.contains(&mime.as_str()) || DOCUMENT_TYPES.contains(&mime.as_str()) {
        return Ok(mime);
    }
    Err(DomainError::UnsupportedContentType { field: "file", content_type: base })
}

/// `image` or `document`.
#[must_use]
pub fn kind_of(mime: &str) -> &'static str {
    if IMAGE_TYPES.contains(&mime) { "image" } else { "document" }
}

/// Default and truncate a filename (255 chars, keeping the extension).
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    // Strip any path components a client may send.
    let name = name.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or("upload");
    let count = name.chars().count();
    if count <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 250 && !stem.is_empty() => {
            let keep = 255 - ext.chars().count() - 1;
            format!("{}.{}", stem.chars().take(keep).collect::<String>(), ext)
        }
        _ => name.chars().take(255).collect(),
    }
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct SizeSum {
    total: Option<i64>,
}

/// Indexing status classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexState {
    Completed,
    InProgress,
    Failed,
}

fn classify_status(s: Option<&str>) -> IndexState {
    match s {
        None | Some("in_progress") => IndexState::InProgress,
        Some("completed") => IndexState::Completed,
        Some(_) => IndexState::Failed,
    }
}

impl MiniChat {
    /// `POST /v1/chats/{id}/attachments` (multipart field `file`).
    ///
    /// # Errors
    /// See the upload error table (DESIGN "Upload Attachment").
    pub async fn upload_attachment<S>(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        content_type: Option<String>,
        body: S,
    ) -> Result<attachment::Model, DomainError>
    where
        S: Stream<Item = Result<Bytes, axum::Error>> + Send + 'static,
    {
        let started = Instant::now();
        let access = self.load_chat(ctx, actions::UPLOAD_ATTACHMENT, chat_id).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        let model = policy
            .find(&access.chat.model)
            .cloned()
            .ok_or_else(|| DomainError::InvalidModel(access.chat.model.clone()))?;
        let _permit = self
            .upload_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| DomainError::UploadConcurrencyLimit)?;

        let ct = content_type.unwrap_or_default();
        let boundary = multer::parse_boundary(&ct).map_err(|e| DomainError::Multipart {
            field: "content_type",
            reason: "BOUNDARY_REQUIRED",
            detail: e.to_string(),
        })?;
        let mut multipart = multer::Multipart::new(body, boundary);
        let mut field = loop {
            match multipart.next_field().await {
                Ok(Some(f)) if f.name() == Some("file") => break f,
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(DomainError::Multipart {
                        field: "file",
                        reason: "MISSING_FILE",
                        detail: "multipart field `file` is required".to_owned(),
                    });
                }
                Err(e) => {
                    return Err(DomainError::Multipart { field: "multipart", reason: "MULTIPART_ERROR", detail: e.to_string() });
                }
            }
        };
        let filename = normalize_filename(field.file_name());
        let part_ct = field.content_type().map(ToString::to_string).ok_or_else(|| DomainError::Multipart {
            field: "content_type",
            reason: "MISSING_CONTENT_TYPE",
            detail: "the `file` part has no content type".to_owned(),
        })?;
        let mime = resolve_mime(&part_ct, &filename, self.cfg.rag.allow_csv_upload)?;
        let kind = kind_of(&mime);
        let ks = policy.snapshot.kill_switches;
        if kind == "image" && ks.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        let (for_file_search, for_code_interpreter) = if kind == "image" {
            (false, false)
        } else if mime == XLSX {
            if ks.disable_code_interpreter || !model.general_config.tool_support.code_interpreter {
                return Err(DomainError::CodeInterpreterUnavailable);
            }
            (false, true)
        } else {
            (true, false)
        };
        let gear_limit = if kind == "image" {
            u64::from(self.cfg.rag.uploaded_image_max_size_kb) * 1024
        } else {
            u64::from(self.cfg.rag.uploaded_file_max_size_kb) * 1024
        };
        let model_limit = u64::from(model.general_config.max_file_size_mb) * 1024 * 1024;
        let limit = if model_limit > 0 { std::cmp::min(gear_limit, model_limit) } else { gear_limit };
        let mut buf = BytesMut::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if (buf.len() + chunk.len()) as u64 > limit {
                        return Err(DomainError::FileTooLarge { limit_bytes: limit });
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(DomainError::Multipart { field: "multipart", reason: "MULTIPART_ERROR", detail: e.to_string() });
                }
            }
        }
        let data = buf.freeze();
        let storage = self.llm.storage_for(&model.provider_id, access.chat.tenant_id)?;
        let row = self
            .insert_pending(&access, ctx.subject_id(), &filename, &mime, kind, data.len(), for_file_search, for_code_interpreter, &storage)
            .await?;
        self.metrics.attachments_pending(1);
        let res = self
            .process_upload(ctx, &access, &model.provider_id, row, data.clone(), &storage, started)
            .await;
        self.metrics.attachments_pending(-1);
        self.metrics.attachment_upload(kind, if res.is_ok() { "ok" } else { "error" }, data.len() as u64);
        res
    }

    #[allow(clippy::too_many_arguments, reason = "row fields")]
    async fn insert_pending(
        &self,
        access: &ChatAccess,
        user_id: Uuid,
        filename: &str,
        mime: &str,
        kind: &'static str,
        size: usize,
        for_file_search: bool,
        for_code_interpreter: bool,
        storage: &StorageTarget,
    ) -> Result<attachment::Model, DomainError> {
        let child = access.child_scope.clone();
        let chat_id = access.chat.id;
        let tenant_id = access.chat.tenant_id;
        let max_docs = u64::from(self.cfg.rag.max_documents_per_chat);
        let max_bytes = i64::from(self.cfg.rag.max_total_upload_mb_per_chat) * 1024 * 1024;
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        let now = clock::now();
        let am = attachment::ActiveModel {
            id: Set(Uuid::now_v7()),
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            uploaded_by_user_id: Set(user_id),
            filename: Set(filename.to_owned()),
            content_type: Set(mime.to_owned()),
            size_bytes: Set(size),
            storage_backend: Set(storage.backend.clone()),
            provider_file_id: Set(None),
            status: Set("pending".to_owned()),
            error_code: Set(None),
            attachment_kind: Set(kind.to_owned()),
            for_file_search: Set(for_file_search),
            for_code_interpreter: Set(for_code_interpreter),
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
        self.tx(move |tx| {
                Box::pin(async move {
                    let live = || {
                        attachment::Entity::find()
                            .filter(attachment::Column::ChatId.eq(chat_id))
                            .filter(attachment::Column::DeletedAt.is_null())
                            .filter(attachment::Column::Status.ne("failed"))
                    };
                    if kind == "document" {
                        let docs = live()
                            .filter(attachment::Column::AttachmentKind.eq("document"))
                            .secure()
                            .scope_with(&child)
                            .count(tx)
                            .await?;
                        if docs + 1 > max_docs {
                            return Err(DomainError::DocumentLimit);
                        }
                    }
                    let sum = live()
                        .secure()
                        .scope_with(&child)
                        .project_all(tx, |q| {
                            use sea_orm::QuerySelect;
                            q.select_only()
                                .column_as(attachment::Column::SizeBytes.sum(), "total")
                                .into_model::<SizeSum>()
                        })
                        .await?;
                    let used = sum.first().and_then(|s| s.total).unwrap_or(0);
                    if used.saturating_add(size) > max_bytes {
                        return Err(DomainError::StorageLimit);
                    }
                    Ok(secure_insert::<attachment::Entity>(am, &child, tx).await?)
                })
            })
            .await
    }

    async fn update_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
        guard: Condition,
        f: impl FnOnce(toolkit_db::secure::SecureUpdateMany<attachment::Entity, toolkit_db::secure::Unscoped>) -> toolkit_db::secure::SecureUpdateMany<attachment::Entity, toolkit_db::secure::Unscoped>,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        let upd = attachment::Entity::update_many()
            .secure()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()));
        let res = f(upd)
            .filter(Condition::all().add(attachment::Column::Id.eq(id)).add(guard))
            .scope_with(scope)
            .exec(&conn)
            .await?;
        Ok(res.rows_affected)
    }

    async fn mark_failed(&self, scope: &AccessScope, id: Uuid, code: &str) {
        let code = code.to_owned();
        let res = self
            .update_attachment(scope, id, Condition::all().add(attachment::Column::Status.ne("failed")), |u| {
                u.col_expr(attachment::Column::Status, Expr::value("failed"))
                    .col_expr(attachment::Column::ErrorCode, Expr::value(Some(code)))
            })
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, attachment_id = %id, "failed to mark attachment failed");
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "upload state machine"
    )]
    async fn process_upload(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        access: &ChatAccess,
        provider_id: &str,
        row: attachment::Model,
        data: Bytes,
        storage: &StorageTarget,
        started: Instant,
    ) -> Result<attachment::Model, DomainError> {
        let scope = access.child_scope.clone();
        let id = row.id;
        let ext = ext_of(&row.filename).unwrap_or_else(|| "bin".to_owned());
        let provider_name = format!("{}_{}.{}", access.chat.id, id, ext);
        let file_id = match self.llm.upload_file(ctx, storage, &provider_name, &row.content_type, data.clone()).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %id, "provider file upload failed");
                self.mark_failed(&scope, id, "upload_failed").await;
                return Err(DomainError::StorageUnavailable("provider upload failed".to_owned()));
            }
        };
        let fid = file_id.clone();
        self.update_attachment(&scope, id, Condition::all().add(attachment::Column::Status.eq("pending")), |u| {
            u.col_expr(attachment::Column::Status, Expr::value("uploaded"))
                .col_expr(attachment::Column::ProviderFileId, Expr::value(Some(fid)))
        })
        .await?;

        if row.attachment_kind == "image" {
            let mut secondary: Option<(String, &'static str)> = None;
            if let Ok(p) = self.llm.resolve(provider_id, access.chat.tenant_id)
                && p.kind == ProviderKind::AnthropicMessages
            {
                match self.llm.upload_anthropic_file(ctx, &p.alias, &row.filename, &row.content_type, data.clone()).await {
                    Ok(sid) => secondary = Some((sid, "uploaded")),
                    Err(e) => {
                        tracing::warn!(error = %e, attachment_id = %id, "secondary (anthropic) upload failed");
                        secondary = Some((String::new(), "failed"));
                    }
                }
            }
            let thumb = if data.len() <= self.cfg.thumbnail.max_decode_bytes {
                let cfg = self.cfg.thumbnail.clone();
                let d = data.clone();
                tokio::task::spawn_blocking(move || thumbnail::generate(&d, &cfg)).await.ok().flatten()
            } else {
                None
            };
            self.update_attachment(&scope, id, Condition::all().add(attachment::Column::Status.eq("uploaded")), |mut u| {
                u = u.col_expr(attachment::Column::Status, Expr::value("ready"));
                if let Some(t) = thumb {
                    u = u
                        .col_expr(attachment::Column::ImgThumbnail, Expr::value(Some(t.bytes)))
                        .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(Some(i32::try_from(t.width).unwrap_or(0))))
                        .col_expr(attachment::Column::ImgThumbnailHeight, Expr::value(Some(i32::try_from(t.height).unwrap_or(0))));
                }
                if let Some((sid, status)) = secondary {
                    u = u
                        .col_expr(attachment::Column::SecondaryStatus, Expr::value(status))
                        .col_expr(attachment::Column::SecondaryProviderKind, Expr::value(Some("anthropic".to_owned())));
                    if !sid.is_empty() {
                        u = u.col_expr(attachment::Column::SecondaryFileId, Expr::value(Some(sid)));
                    }
                }
                u
            })
            .await?;
            return self.reload_attachment(&scope, id).await;
        }
        if !row.for_file_search {
            self.update_attachment(&scope, id, Condition::all().add(attachment::Column::Status.eq("uploaded")), |u| {
                u.col_expr(attachment::Column::Status, Expr::value("ready"))
            })
            .await?;
            return self.reload_attachment(&scope, id).await;
        }

        // Document for file_search: vector store + indexing.
        let vs = match self.get_or_create_vector_store(ctx, access, storage).await {
            Ok(vs) => vs,
            Err(e) => {
                let code = if matches!(e, DomainError::ProviderMismatch) { None } else { Some("vector_store_failed") };
                if let Some(code) = code {
                    self.mark_failed(&scope, id, code).await;
                    self.spawn_file_delete(ctx, storage, &file_id);
                    return Err(DomainError::StorageUnavailable("vector store unavailable".to_owned()));
                }
                self.mark_failed(&scope, id, "vector_store_failed").await;
                self.spawn_file_delete(ctx, storage, &file_id);
                return Err(e);
            }
        };
        let deadline = started + UPLOAD_INDEXING_DEADLINE;
        let add = tokio::time::timeout_at(deadline, self.llm.add_vector_store_file(ctx, storage, &vs, &file_id, id)).await;
        let mut state = match add {
            Ok(Ok(s)) => classify_status(s.as_deref()),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, attachment_id = %id, "add to vector store failed");
                IndexState::Failed
            }
            Err(_) => IndexState::InProgress,
        };
        let mut wait = Duration::from_millis(250);
        while state == IndexState::InProgress && Instant::now() < deadline {
            let now = Instant::now();
            tokio::time::sleep(std::cmp::min(wait, deadline - now)).await;
            wait = std::cmp::min(wait * 2, Duration::from_secs(2));
            if Instant::now() >= deadline {
                break;
            }
            match tokio::time::timeout_at(deadline, self.llm.vector_store_file_status(ctx, storage, &vs, &file_id)).await {
                Ok(Ok(s)) => state = classify_status(s.as_deref()),
                Ok(Err(e)) if e.transient => {
                    tracing::debug!(error = %e, "transient indexing status read error");
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, attachment_id = %id, "indexing status read failed");
                    state = IndexState::Failed;
                }
                Err(_) => break,
            }
        }
        match state {
            IndexState::Completed => {
                self.update_attachment(&scope, id, Condition::all().add(attachment::Column::Status.eq("uploaded")), |u| {
                    u.col_expr(attachment::Column::Status, Expr::value("ready"))
                })
                .await?;
            }
            IndexState::Failed => {
                self.mark_failed(&scope, id, "indexing_failed").await;
                self.spawn_file_delete(ctx, storage, &file_id);
                return Err(DomainError::StorageUnavailable("indexing failed".to_owned()));
            }
            IndexState::InProgress => {
                self.spawn_background_indexing(ctx.clone(), access.chat.tenant_id, access.chat.id, scope.clone(), storage.clone(), vs, file_id, id);
            }
        }
        self.reload_attachment(&scope, id).await
    }

    fn spawn_file_delete(&self, ctx: &SecurityContext, storage: &StorageTarget, file_id: &str) {
        let llm = self.llm.clone();
        let ctx = ctx.clone();
        let storage = storage.clone();
        let file_id = file_id.to_owned();
        tokio::spawn(async move {
            if let Err(e) = llm.delete_file(&ctx, &storage, &file_id).await {
                tracing::warn!(error = %e, "best-effort provider file delete failed");
            }
        });
    }

    async fn reload_attachment(&self, scope: &AccessScope, id: Uuid) -> Result<attachment::Model, DomainError> {
        let conn = self.db.conn()?;
        attachment::Entity::find()
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::AttachmentNotFound(id))
    }

    /// Get-or-create the chat vector store (creation protocol, DESIGN 3.7).
    #[allow(clippy::cognitive_complexity, reason = "multi-round creation protocol state machine")]
    async fn get_or_create_vector_store(
        &self,
        ctx: &SecurityContext,
        access: &ChatAccess,
        storage: &StorageTarget,
    ) -> Result<String, DomainError> {
        let tenant_id = access.chat.tenant_id;
        let chat_id = access.chat.id;
        let scope = &access.child_scope;
        for _round in 0..3 {
            let conn = self.db.conn()?;
            let existing = chat_vector_store::Entity::find()
                .filter(chat_vector_store::Column::TenantId.eq(tenant_id))
                .filter(chat_vector_store::Column::ChatId.eq(chat_id))
                .secure()
                .scope_with(scope)
                .one(&conn)
                .await?;
            #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
            drop(conn);
            if let Some(row) = existing {
                if row.provider != storage.backend {
                    return Err(DomainError::ProviderMismatch);
                }
                if let Some(vs) = row.vector_store_id {
                    return Ok(vs);
                }
                let age = clock::now() - row.created_at;
                if age > chrono::Duration::from_std(PLACEHOLDER_STALE).unwrap_or(chrono::Duration::MAX) {
                    let conn = self.db.conn()?;
                    chat_vector_store::Entity::delete_many()
                        .secure()
                        .scope_with(scope)
                        .filter(
                            Condition::all()
                                .add(chat_vector_store::Column::Id.eq(row.id))
                                .add(chat_vector_store::Column::VectorStoreId.is_null()),
                        )
                        .exec(&conn)
                        .await?;
                    continue;
                }
                return self.poll_vector_store(scope, tenant_id, chat_id).await;
            }
            // Winner path: insert the placeholder (auto-committed).
            let row_id = Uuid::now_v7();
            let conn = self.db.conn()?;
            let inserted = secure_insert::<chat_vector_store::Entity>(
                chat_vector_store::ActiveModel {
                    id: Set(row_id),
                    tenant_id: Set(tenant_id),
                    chat_id: Set(chat_id),
                    vector_store_id: Set(None),
                    provider: Set(storage.backend.clone()),
                    file_count: Set(0),
                    created_at: Set(clock::now()),
                },
                scope,
                &conn,
            )
            .await;
            #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
            drop(conn);
            match inserted.map_err(DomainError::from) {
                Ok(_) => {}
                Err(DomainError::UniqueViolation(_)) => {
                    return self.poll_vector_store(scope, tenant_id, chat_id).await;
                }
                Err(e) => return Err(e),
            }
            let created = self.llm.create_vector_store(ctx, storage, &format!("chat_{chat_id}")).await;
            let vs = match created {
                Ok(vs) => vs,
                Err(e) => {
                    tracing::warn!(error = %e, %chat_id, "vector store creation failed");
                    if let Ok(conn) = self.db.conn()
                        && let Err(e) = chat_vector_store::Entity::delete_many()
                            .secure()
                            .scope_with(scope)
                            .filter(
                                Condition::all()
                                    .add(chat_vector_store::Column::Id.eq(row_id))
                                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
                            )
                            .exec(&conn)
                            .await
                    {
                        tracing::debug!(error = %e, %chat_id, "failed to delete vector store placeholder");
                    }
                    return Err(DomainError::StorageUnavailable("vector store creation failed".to_owned()));
                }
            };
            let conn = self.db.conn()?;
            let rows = chat_vector_store::Entity::update_many()
                .secure()
                .col_expr(chat_vector_store::Column::VectorStoreId, Expr::value(Some(vs.clone())))
                .filter(
                    Condition::all()
                        .add(chat_vector_store::Column::Id.eq(row_id))
                        .add(chat_vector_store::Column::VectorStoreId.is_null()),
                )
                .scope_with(scope)
                .exec(&conn)
                .await?
                .rows_affected;
            #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
            drop(conn);
            if rows == 1 {
                return Ok(vs);
            }
            if let Err(e) = self.llm.delete_vector_store(ctx, storage, &vs).await {
                tracing::warn!(error = %e, "failed to delete superseded vector store");
            }
            return self.poll_vector_store(scope, tenant_id, chat_id).await;
        }
        Err(DomainError::StorageUnavailable("vector store unavailable".to_owned()))
    }

    async fn poll_vector_store(&self, scope: &AccessScope, tenant_id: Uuid, chat_id: Uuid) -> Result<String, DomainError> {
        let mut wait = Duration::from_millis(200);
        for _ in 0..5 {
            tokio::time::sleep(wait).await;
            wait *= 2;
            let conn = self.db.conn()?;
            if let Some(vs) = chat_vector_store::Entity::find()
                .filter(chat_vector_store::Column::TenantId.eq(tenant_id))
                .filter(chat_vector_store::Column::ChatId.eq(chat_id))
                .secure()
                .scope_with(scope)
                .one(&conn)
                .await?
                .and_then(|r| r.vector_store_id)
            {
                return Ok(vs);
            }
        }
        Err(DomainError::StorageUnavailable("vector store creation in progress".to_owned()))
    }

    #[allow(clippy::too_many_arguments, reason = "task inputs")]
    fn spawn_background_indexing(
        self: &Arc<Self>,
        ctx: SecurityContext,
        tenant_id: Uuid,
        chat_id: Uuid,
        scope: AccessScope,
        storage: StorageTarget,
        vs: String,
        file_id: String,
        id: Uuid,
    ) {
        let svc = Arc::clone(self);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = svc.background_indexing(ctx, tenant_id, chat_id, scope, storage, vs, file_id, id) => {}
            }
        });
    }

    #[allow(clippy::too_many_arguments, clippy::cognitive_complexity, reason = "task inputs; indexing state machine")]
    async fn background_indexing(
        &self,
        ctx: SecurityContext,
        tenant_id: Uuid,
        chat_id: Uuid,
        scope: AccessScope,
        storage: StorageTarget,
        vs: String,
        file_id: String,
        id: Uuid,
    ) {
        let end = Instant::now() + BACKGROUND_INDEXING_LIMIT;
        let live_guard = || {
            Condition::all()
                .add(attachment::Column::Status.eq("uploaded"))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null())
        };
        let mut last_err: Option<String> = None;
        let outcome = 'outer: loop {
            if Instant::now() >= end {
                break 'outer IndexState::InProgress;
            }
            match self.update_attachment(&scope, id, live_guard(), |u| u).await {
                Ok(0) => {
                    self.metrics.background_indexing("stopped");
                    return;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "indexing heartbeat failed"),
            }
            let round_end = std::cmp::min(Instant::now() + BACKGROUND_ROUND, end);
            let mut wait = Duration::from_millis(250);
            while Instant::now() < round_end {
                tokio::time::sleep(std::cmp::min(wait, round_end.saturating_duration_since(Instant::now()))).await;
                wait = std::cmp::min(wait * 2, Duration::from_secs(5));
                match self.llm.vector_store_file_status(&ctx, &storage, &vs, &file_id).await {
                    Ok(s) => match classify_status(s.as_deref()) {
                        IndexState::InProgress => {}
                        other => break 'outer other,
                    },
                    Err(StorageError { transient: true, message, .. }) => {
                        if last_err.is_none() {
                            tracing::warn!(error = %message, attachment_id = %id, "transient indexing status error");
                        }
                        last_err = Some(message);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, attachment_id = %id, "indexing status read failed");
                        break 'outer IndexState::Failed;
                    }
                }
            }
        };
        if outcome == IndexState::Completed {
            let mut delay = Duration::from_secs(1);
            for attempt in 0..4 {
                match self
                    .update_attachment(&scope, id, live_guard(), |u| u.col_expr(attachment::Column::Status, Expr::value("ready")))
                    .await
                {
                    Ok(1) => {
                        self.metrics.background_indexing("ready");
                        return;
                    }
                    Ok(_) => {
                        self.metrics.background_indexing("stopped");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, attempt, "set ready failed");
                        if attempt < 3 {
                            tokio::time::sleep(delay).await;
                            delay *= 2;
                        }
                    }
                }
            }
            self.metrics.background_indexing("set_ready_failed");
            return;
        }
        if outcome == IndexState::InProgress {
            tracing::warn!(attachment_id = %id, last_error = ?last_err, "background indexing timed out");
        }
        // Failed or timed out: one transaction (failed + cleanup pending + outbox).
        let slot = self.outbox.clone();
        let res = self
            .tx(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let rows = attachment::Entity::update_many()
                        .secure()
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(attachment::Column::ErrorCode, Expr::value(Some("indexing_failed".to_owned())))
                        .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(Condition::all().add(attachment::Column::Id.eq(id)).add(live_guard()))
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Ok(None);
                    }
                    let payload = AttachmentCleanupPayload {
                        event_type: event_types::INDEXING_FAILED.to_owned(),
                        tenant_id,
                        chat_id,
                        attachment_id: id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: storage.backend.clone(),
                        attachment_kind: "document".to_owned(),
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Ok(Some(enqueue_json(&slot, tx, OutboxKind::AttachmentCleanup, tenant_id, &payload).await?))
                })
            })
            .await;
        match res {
            Ok(Some(w)) => {
                w.fire();
                self.metrics.background_indexing(if outcome == IndexState::InProgress { "timeout" } else { "failed" });
            }
            Ok(None) => self.metrics.background_indexing("stopped"),
            Err(e) => tracing::error!(error = %e, attachment_id = %id, "failed to record indexing failure"),
        }
    }

    async fn find_own_attachment(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        access: &ChatAccess,
        id: Uuid,
    ) -> Result<attachment::Model, DomainError> {
        attachment::Entity::find()
            .filter(attachment::Column::Id.eq(id))
            .filter(attachment::Column::ChatId.eq(access.chat.id))
            .filter(attachment::Column::UploadedByUserId.eq(ctx.subject_id()))
            .secure()
            .scope_with(&access.child_scope)
            .one(runner)
            .await?
            .ok_or(DomainError::AttachmentNotFound(id))
    }

    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound`.
    pub async fn get_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<attachment::Model, DomainError> {
        let access = self.load_chat(ctx, actions::READ_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let a = self.find_own_attachment(&conn, ctx, &access, id).await?;
        if a.deleted_at.is_some() {
            return Err(DomainError::AttachmentNotFound(id));
        }
        Ok(a)
    }

    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// `ChatNotFound`, `AttachmentNotFound`, `AttachmentLocked`.
    pub async fn delete_attachment(&self, ctx: &SecurityContext, chat_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let access = self.load_chat(ctx, actions::DELETE_ATTACHMENT, chat_id).await?;
        let conn = self.db.conn()?;
        let a = self.find_own_attachment(&conn, ctx, &access, id).await?;
        if a.deleted_at.is_some() {
            return Ok(());
        }
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let child = access.child_scope.clone();
        let slot = self.outbox.clone();
        let tenant_id = access.chat.tenant_id;
        let secondary_alias = self
            .llm
            .resolve(
                &self
                    .policy
                    .current(ctx.subject_id())
                    .await
                    .ok()
                    .and_then(|p| p.find(&access.chat.model).map(|m| m.provider_id.clone()))
                    .unwrap_or_default(),
                tenant_id,
            )
            .ok()
            .filter(|p| p.kind == ProviderKind::AnthropicMessages)
            .map(|p| p.alias);
        let wake = self
            .tx(move |tx| {
                Box::pin(async move {
                    let links = message_attachment::Entity::find()
                        .filter(message_attachment::Column::ChatId.eq(chat_id))
                        .filter(message_attachment::Column::AttachmentId.eq(id))
                        .secure()
                        .scope_with(&child)
                        .all(tx)
                        .await?;
                    if !links.is_empty() {
                        let msg_ids: Vec<Uuid> = links.iter().map(|l| l.message_id).collect();
                        let live = message::Entity::find()
                            .filter(message::Column::ChatId.eq(chat_id))
                            .filter(message::Column::Id.is_in(msg_ids))
                            .filter(message::Column::DeletedAt.is_null())
                            .secure()
                            .scope_with(&child)
                            .count(tx)
                            .await?;
                        if live > 0 {
                            return Err(DomainError::AttachmentLocked);
                        }
                    }
                    let now = clock::now();
                    let rows = attachment::Entity::update_many()
                        .secure()
                        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(Condition::all().add(attachment::Column::Id.eq(id)).add(attachment::Column::DeletedAt.is_null()))
                        .scope_with(&child)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Ok(None);
                    }
                    let secondary_ref = match (&a.secondary_file_id, secondary_alias) {
                        (Some(fid), Some(alias)) if a.secondary_status == "uploaded" => Some(SecondaryRef {
                            file_id: fid.clone(),
                            provider_kind: a.secondary_provider_kind.clone().unwrap_or_else(|| "anthropic".to_owned()),
                            upstream_alias: alias,
                        }),
                        _ => None,
                    };
                    let payload = AttachmentCleanupPayload {
                        event_type: event_types::DELETED.to_owned(),
                        tenant_id,
                        chat_id,
                        attachment_id: id,
                        provider_file_id: a.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: a.storage_backend.clone(),
                        attachment_kind: a.attachment_kind.clone(),
                        deleted_at: now,
                        secondary_ref,
                    };
                    Ok(Some(enqueue_json(&slot, tx, OutboxKind::AttachmentCleanup, tenant_id, &payload).await?))
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
mod tests {
    use super::*;

    #[test]
    fn mime_resolution() {
        assert_eq!(resolve_mime("application/pdf", "a.pdf", true).unwrap(), "application/pdf");
        assert_eq!(resolve_mime("application/octet-stream", "Report.PDF", true).unwrap(), "application/pdf");
        assert_eq!(resolve_mime("application/octet-stream", "x.xlsx", true).unwrap(), XLSX);
        assert_eq!(resolve_mime("text/csv", "x.csv", true).unwrap(), "text/plain");
        assert!(resolve_mime("text/csv", "x.csv", false).is_err());
        assert!(resolve_mime("application/octet-stream", "x.exe", true).is_err());
        assert!(resolve_mime("application/zip", "x.zip", true).is_err());
        assert_eq!(resolve_mime("image/png; charset=binary", "a.png", true).unwrap(), "image/png");
        assert_eq!(resolve_mime("text/plain; charset=utf-8", "a.txt", true).unwrap(), "text/plain");
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("image/gif"), "image");
        assert_eq!(kind_of("application/pdf"), "document");
    }

    #[test]
    fn filenames() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("  ")), "upload");
        assert_eq!(normalize_filename(Some("dir/a.txt")), "a.txt");
        let long = format!("{}.pdf", "a".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert!(std::path::Path::new(&n).extension().is_some_and(|e| e == "pdf"));
    }

    #[test]
    fn index_status_classification() {
        assert_eq!(classify_status(None), IndexState::InProgress);
        assert_eq!(classify_status(Some("in_progress")), IndexState::InProgress);
        assert_eq!(classify_status(Some("completed")), IndexState::Completed);
        assert_eq!(classify_status(Some("cancelled")), IndexState::Failed);
        assert_eq!(classify_status(Some("weird")), IndexState::Failed);
    }
}
