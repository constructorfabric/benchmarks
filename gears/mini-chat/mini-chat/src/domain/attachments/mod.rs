//! Attachment service: upload (provider Files API, chat vector store, indexing wait, image
//! thumbnails), get, delete and the cleanup payload helpers shared with the outbox handlers and
//! the upload reaper (DESIGN §3.3 "Get Attachment" / "Upload Attachment", §3.6 "File Upload",
//! §3.7 attachments / `chat_vector_stores`, §4 "Attachment Deletion").

pub mod indexing;
pub mod mime;
pub mod thumbnail;
pub mod vector_store;

#[cfg(test)]
pub mod test_support;
#[cfg(test)]
#[path = "unit_tests.rs"]
mod unit_tests;

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::chats;
use crate::domain::error::{DomainError, Resource};
use crate::domain::models;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{attachment, chat, message, message_attachment};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::storage;
use crate::infra::outbox::PAYLOAD_ATTACHMENT_CLEANUP;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

use self::mime::{AttachmentKind, ResolvedType};

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";

pub const ERR_UPLOAD_FAILED: &str = "upload_failed";
pub const ERR_VECTOR_STORE_FAILED: &str = "vector_store_failed";
pub const ERR_INDEXING_FAILED: &str = "indexing_failed";
pub const ERR_UPLOAD_ABANDONED: &str = "upload_abandoned";

pub const CLEANUP_PENDING: &str = "pending";
pub const CLEANUP_DONE: &str = "done";
pub const CLEANUP_FAILED: &str = "failed";

pub const EVENT_ATTACHMENT_DELETED: &str = "attachment_deleted";
pub const EVENT_UPLOAD_ABANDONED: &str = "attachment_upload_abandoned";
pub const EVENT_INDEXING_FAILED: &str = "attachment_indexing_failed";

/// `Retry-After` of provider / storage failures.
pub const PROVIDER_RETRY_AFTER_SECS: u64 = 10;
/// `Retry-After` of the upload concurrency limit.
pub const CONCURRENCY_RETRY_AFTER_SECS: u64 = 5;

const MIB: u64 = 1024 * 1024;

/// Security context used for provider calls of background work (no caller).
#[must_use]
pub fn system_ctx(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// Cleanup message of an attachment row (`vector_store_id` and `secondary_ref` are `null` in P1).
#[must_use]
pub fn cleanup_payload(event_type: &str, row: &attachment::Model, at: OffsetDateTime) -> AttachmentCleanupPayload {
    AttachmentCleanupPayload {
        event_type: event_type.to_owned(),
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        attachment_id: row.id,
        provider_file_id: row.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: row.storage_backend.clone(),
        attachment_kind: row.attachment_kind.clone(),
        deleted_at: at,
        secondary_ref: None,
    }
}

/// Enqueues an attachment cleanup message (partition key = tenant) inside `tx`.
///
/// # Errors
/// Outbox errors; an oversized payload is an internal error.
pub async fn enqueue_cleanup(
    app: &AppServices,
    tx: &DbTx<'_>,
    payload: &AttachmentCleanupPayload,
) -> Result<Wake, DomainError> {
    app.outbox
        .enqueue_json(tx, app.outbox.attachment_cleanup_queue(), payload.tenant_id, PAYLOAD_ATTACHMENT_CLEANUP, payload)
        .await
        .map_err(|e| match e {
            DomainError::InvalidFormat(m) => DomainError::internal(m),
            other => other,
        })
}

/// Loads an attachment of a chat (including soft-deleted rows).
///
/// # Errors
/// DB errors.
pub async fn find_attachment(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> Result<Option<attachment::Model>, DomainError> {
    let conn = app.db.conn()?;
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(attachment_id))
                .add(attachment::Column::ChatId.eq(chat_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(&conn)
        .await?)
}

/// Loads an attachment by id under a tenant (background work).
///
/// # Errors
/// DB errors.
pub async fn find_by_id(app: &AppServices, tenant_id: Uuid, attachment_id: Uuid) -> Result<Option<attachment::Model>, DomainError> {
    let conn = app.db.conn()?;
    Ok(attachment::Entity::find()
        .filter(attachment::Column::Id.eq(attachment_id))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(&conn)
        .await?)
}

fn attachment_not_found(id: Uuid) -> DomainError {
    DomainError::not_found(Resource::Attachment, id)
}

// ───────────────────────────── get / delete ─────────────────────────────

/// `GET /chats/{id}/attachments/{attachment_id}`: the caller's own, non-deleted attachment.
///
/// # Errors
/// 404 chat / attachment, authz errors, DB errors.
pub async fn get(
    app: &AppServices,
    ctx: &SecurityContext,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> Result<attachment::Model, DomainError> {
    let scope = app.authz.chat_scope(ctx, "read_attachment", Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    match find_attachment(app, chat.tenant_id, chat_id, attachment_id).await? {
        Some(a) if a.uploaded_by_user_id == ctx.subject_id() && a.deleted_at.is_none() => Ok(a),
        _ => Err(attachment_not_found(attachment_id)),
    }
}

/// `DELETE /chats/{id}/attachments/{attachment_id}`: soft-delete + `attachment_deleted` cleanup
/// message in one transaction. A repeated delete is a no-op; a referenced attachment is locked.
///
/// # Errors
/// 404 chat / attachment, 409 `attachment_locked`, authz / DB / outbox errors.
pub async fn delete(app: &Arc<AppServices>, ctx: &SecurityContext, chat_id: Uuid, attachment_id: Uuid) -> Result<(), DomainError> {
    let scope = app.authz.chat_scope(ctx, "delete_attachment", Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    let row = match find_attachment(app, chat.tenant_id, chat_id, attachment_id).await? {
        Some(a) if a.uploaded_by_user_id == ctx.subject_id() => a,
        _ => return Err(attachment_not_found(attachment_id)),
    };
    if row.deleted_at.is_some() {
        return Ok(());
    }
    let wake = crate::domain::tx::retry_contention(|| {
        let app2 = Arc::clone(app);
        let row = row.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let tenant_scope = AccessScope::for_tenant(row.tenant_id);
                if is_referenced(tx, &tenant_scope, row.chat_id, row.id).await? {
                    return Err(DomainError::AlreadyExists {
                        resource: Resource::Attachment,
                        name: "attachment_locked".to_owned(),
                        detail: "Attachment is referenced by a submitted message".to_owned(),
                    });
                }
                let now = clock::now();
                let res = attachment::Entity::update_many()
                    .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                    .col_expr(attachment::Column::CleanupStatus, Expr::value(Some(CLEANUP_PENDING.to_owned())))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                    .filter(
                        Condition::all()
                            .add(attachment::Column::Id.eq(row.id))
                            .add(attachment::Column::ChatId.eq(row.chat_id))
                            .add(attachment::Column::DeletedAt.is_null()),
                    )
                    .secure()
                    .scope_with(&tenant_scope)
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    return Ok(None);
                }
                let payload = cleanup_payload(EVENT_ATTACHMENT_DELETED, &row, now);
                Ok(Some(enqueue_cleanup(&app2, tx, &payload).await?))
            })
        })
    })
    .await?;
    if let Some(w) = wake {
        w.fire();
    }
    Ok(())
}

/// `true` when a non-deleted message references the attachment.
async fn is_referenced(tx: &DbTx<'_>, scope: &AccessScope, chat_id: Uuid, attachment_id: Uuid) -> Result<bool, DomainError> {
    let links = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::AttachmentId.eq(attachment_id)),
        )
        .secure()
        .scope_with(scope)
        .all(tx)
        .await?;
    if links.is_empty() {
        return Ok(false);
    }
    let ids: Vec<Uuid> = links.iter().map(|l| l.message_id).collect();
    let live = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::Id.is_in(ids))
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .count(tx)
        .await?;
    Ok(live > 0)
}

// ───────────────────────────── upload ─────────────────────────────

/// Everything resolved before the request body is read.
#[derive(Debug, Clone)]
pub struct UploadTarget {
    pub chat: chat::Model,
    pub model: ModelCatalogEntry,
    pub kill_switches: KillSwitches,
    /// Code interpreter usable: kill switch off and the chat model supports it.
    pub code_interpreter_available: bool,
    /// When the upload request started (25 s indexing deadline).
    pub started: Instant,
}

/// Validated `file` part metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub filename: String,
    pub resolved: ResolvedType,
    /// Effective per-file limit in bytes.
    pub max_bytes: u64,
}

/// Authorizes, loads the chat and resolves its model (before the body is read).
///
/// # Errors
/// authz errors, 404 chat, 400 `INVALID_MODEL`, policy plugin errors.
pub async fn prepare_upload(app: &AppServices, ctx: &SecurityContext, chat_id: Uuid) -> Result<UploadTarget, DomainError> {
    let started = Instant::now();
    let scope = app.authz.chat_scope(ctx, "upload_attachment", Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    let snapshot = app.policy.current_snapshot(ctx.subject_id()).await?;
    let model = models::chat_model(&snapshot, &chat.model)?.clone();
    let kill_switches = snapshot.kill_switches;
    let code_interpreter_available =
        !kill_switches.disable_code_interpreter && model.general_config.tool_support.code_interpreter;
    Ok(UploadTarget { chat, model, kill_switches, code_interpreter_available, started })
}

impl UploadTarget {
    /// Validates the `file` part headers: MIME type, kill switches, purposes and size limit.
    ///
    /// # Errors
    /// 400 `UNSUPPORTED_CONTENT_TYPE`, `FEATURE_DISABLED`, `CODE_INTERPRETER_UNAVAILABLE`.
    pub fn validate_part(&self, app: &AppServices, raw_filename: Option<&str>, part_content_type: &str) -> Result<FileMeta, DomainError> {
        let filename = mime::normalize_filename(raw_filename);
        let media = mime::effective_type(part_content_type, &filename);
        let mut resolved = mime::resolve(&media, app.cfg.rag.allow_csv_upload).ok_or_else(|| {
            DomainError::invalid(
                Resource::Attachment,
                "content_type",
                "UNSUPPORTED_CONTENT_TYPE",
                format!("content type '{media}' is not supported"),
            )
        })?;
        if resolved.kind == AttachmentKind::Image && self.kill_switches.disable_images {
            return Err(DomainError::feature_disabled("images"));
        }
        if !self.code_interpreter_available {
            resolved.for_code_interpreter = false;
            if resolved.kind == AttachmentKind::Document && !resolved.for_file_search {
                return Err(DomainError::invalid(
                    Resource::Attachment,
                    "file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                    "code interpreter is not available for this chat",
                ));
            }
        }
        let max_bytes = self.max_bytes(app, resolved.kind);
        Ok(FileMeta { filename, resolved, max_bytes })
    }

    /// `min(kind limit, model max_file_size_mb)` in bytes.
    #[must_use]
    pub fn max_bytes(&self, app: &AppServices, kind: AttachmentKind) -> u64 {
        let kb = match kind {
            AttachmentKind::Document => app.cfg.rag.uploaded_file_max_size_kb,
            AttachmentKind::Image => app.cfg.rag.uploaded_image_max_size_kb,
        };
        let cfg_limit = u64::from(kb) * 1024;
        let model_limit = u64::from(self.model.general_config.max_file_size_mb) * MIB;
        cfg_limit.min(model_limit)
    }
}

/// `FILE_TOO_LARGE` (400 `out_of_range`).
#[must_use]
pub fn file_too_large(limit: u64) -> DomainError {
    DomainError::out_of_range(
        Resource::Attachment,
        "content_length",
        "FILE_TOO_LARGE",
        format!("file exceeds the maximum size of {limit} bytes"),
    )
}

fn provider_unavailable(detail: impl Into<String>) -> DomainError {
    DomainError::unavailable(PROVIDER_RETRY_AFTER_SECS, detail)
}

#[derive(Debug, FromQueryResult)]
struct UsageRow {
    attachment_kind: String,
    size_bytes: i64,
}

/// Per-chat document count and total size limits (non-deleted, non-failed rows).
async fn check_chat_limits(app: &AppServices, chat: &chat::Model, kind: AttachmentKind, size: u64) -> Result<(), DomainError> {
    let conn = app.db.conn()?;
    let rows: Vec<UsageRow> = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat.id))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::Status.ne(STATUS_FAILED)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(chat.tenant_id))
        .project_all(&conn, |q| {
            q.select_only()
                .column(attachment::Column::AttachmentKind)
                .column(attachment::Column::SizeBytes)
                .into_model::<UsageRow>()
        })
        .await?;
    if kind == AttachmentKind::Document {
        let docs = rows.iter().filter(|r| r.attachment_kind == AttachmentKind::Document.as_str()).count();
        let max = app.cfg.rag.max_documents_per_chat as usize;
        if docs >= max {
            return Err(DomainError::ResourceExhausted {
                resource: Resource::Attachment,
                subject: "document_limit".to_owned(),
                description: format!("Maximum of {max} documents per chat reached"),
                detail: "Per-chat document limit reached".to_owned(),
            });
        }
    }
    let used: u64 = rows.iter().map(|r| u64::try_from(r.size_bytes).unwrap_or(0)).sum();
    let max_total = u64::from(app.cfg.rag.max_total_upload_mb_per_chat) * MIB;
    if used.saturating_add(size) > max_total {
        return Err(DomainError::ResourceExhausted {
            resource: Resource::Attachment,
            subject: "storage_limit".to_owned(),
            description: format!("Maximum total upload size of {} MB per chat reached", app.cfg.rag.max_total_upload_mb_per_chat),
            detail: "Per-chat storage limit reached".to_owned(),
        });
    }
    Ok(())
}

/// Stores a validated, fully read file: per-chat limits, row insert, provider upload, vector
/// store indexing (documents), thumbnail (images). Returns the row as it is after the request.
///
/// # Errors
/// 429 limits, 409 `provider_mismatch`, 503 provider failures, DB errors.
#[allow(clippy::cognitive_complexity, reason = "tracing macro expansion only")]
pub async fn store_upload(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    target: &UploadTarget,
    meta: FileMeta,
    data: Bytes,
) -> Result<attachment::Model, DomainError> {
    let chat = &target.chat;
    let size = data.len() as u64;
    let kind = meta.resolved.kind;
    check_chat_limits(app, chat, kind, size).await?;
    let provider = app.providers.resolve_rag(&target.model.provider_id, chat.tenant_id)?;
    if meta.resolved.for_file_search {
        vector_store::check_provider(app, chat.tenant_id, chat.id, &provider).await?;
    }

    let id = Uuid::new_v4();
    let now = clock::now();
    let tenant_scope = AccessScope::for_tenant(chat.tenant_id);
    let am = attachment::ActiveModel {
        id: Set(id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        uploaded_by_user_id: Set(ctx.subject_id()),
        filename: Set(meta.filename.clone()),
        content_type: Set(meta.resolved.content_type.to_owned()),
        size_bytes: Set(i64::try_from(size).unwrap_or(i64::MAX)),
        storage_backend: Set(provider.storage_backend.clone()),
        provider_file_id: Set(None),
        status: Set(STATUS_PENDING.to_owned()),
        error_code: Set(None),
        attachment_kind: Set(kind.as_str().to_owned()),
        for_file_search: Set(meta.resolved.for_file_search),
        for_code_interpreter: Set(meta.resolved.for_code_interpreter),
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
    {
        let conn = app.db.conn()?;
        secure_insert::<attachment::Entity>(am, &tenant_scope, &conn).await?;
    }

    // Provider upload (retry a still-pending OAGW upstream first).
    app.provisioning.wait_ready(&provider.alias, std::time::Duration::from_secs(5)).await;
    let provider_name = format!("{}_{}.{}", chat.id, id, mime::provider_extension(meta.resolved.content_type));
    let file_id = match storage::upload_file(
        app.transport.as_ref(),
        ctx,
        &provider,
        &provider_name,
        meta.resolved.content_type,
        data.clone(),
    )
    .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(attachment_id = %id, error = %e, "provider file upload failed");
            mark_failed(app, chat.tenant_id, id, ERR_UPLOAD_FAILED).await;
            return Err(provider_unavailable(format!("file upload failed: {e}")));
        }
    };
    if !set_uploaded(app, chat.tenant_id, id, &file_id).await? {
        // Deleted or reaped meanwhile: the provider file has no owner any more.
        spawn_delete_file(app, ctx.clone(), provider.clone(), file_id);
        return Err(attachment_not_found(id));
    }

    match kind {
        AttachmentKind::Image => {
            let cfg = app.cfg.thumbnail.clone();
            let thumb = tokio::task::spawn_blocking(move || thumbnail::generate(&data, &cfg)).await.ok().flatten();
            set_ready(app, chat.tenant_id, id, thumb).await?;
        }
        AttachmentKind::Document if meta.resolved.for_file_search => {
            index_document(app, ctx, target, &provider, id, &file_id).await?;
        }
        AttachmentKind::Document => {
            set_ready(app, chat.tenant_id, id, None).await?;
        }
    }
    find_by_id(app, chat.tenant_id, id).await?.ok_or_else(|| attachment_not_found(id))
}

/// Adds the file to the chat vector store and waits for indexing until the request deadline.
#[allow(clippy::cognitive_complexity, reason = "tracing macro expansion only")]
async fn index_document(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    target: &UploadTarget,
    provider: &ResolvedProvider,
    id: Uuid,
    file_id: &str,
) -> Result<(), DomainError> {
    let chat = &target.chat;
    let vs_id = match vector_store::ensure(app, ctx, chat.tenant_id, chat.id, provider).await {
        Ok(v) => v,
        Err(e) => {
            mark_failed(app, chat.tenant_id, id, ERR_VECTOR_STORE_FAILED).await;
            spawn_delete_file(app, ctx.clone(), provider.clone(), file_id.to_owned());
            return Err(e);
        }
    };
    let initial = match storage::add_file_to_vector_store(app.transport.as_ref(), ctx, provider, &vs_id, file_id, id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(attachment_id = %id, error = %e, "adding the file to the vector store failed");
            mark_failed(app, chat.tenant_id, id, ERR_INDEXING_FAILED).await;
            spawn_delete_file(app, ctx.clone(), provider.clone(), file_id.to_owned());
            return Err(provider_unavailable(format!("vector store add failed: {e}")));
        }
    };
    let timings = indexing::timings(app);
    let deadline = target.started + timings.request_deadline;
    match indexing::wait_in_request(app, ctx, provider, &vs_id, file_id, initial, deadline, &timings).await {
        indexing::WaitOutcome::Completed => {
            set_ready(app, chat.tenant_id, id, None).await?;
            Ok(())
        }
        indexing::WaitOutcome::Failed(reason) => {
            tracing::warn!(attachment_id = %id, %reason, "document indexing failed");
            mark_failed(app, chat.tenant_id, id, ERR_INDEXING_FAILED).await;
            spawn_delete_file(app, ctx.clone(), provider.clone(), file_id.to_owned());
            Err(provider_unavailable(format!("indexing failed: {reason}")))
        }
        indexing::WaitOutcome::Deadline => {
            indexing::spawn_background(
                Arc::clone(app),
                indexing::BackgroundJob {
                    tenant_id: chat.tenant_id,
                    attachment_id: id,
                    vector_store_id: vs_id,
                    provider_file_id: file_id.to_owned(),
                    provider: provider.clone(),
                },
            );
            Ok(())
        }
    }
}

/// `pending` → `uploaded` with the provider file id (only while not deleted / failed).
async fn set_uploaded(app: &AppServices, tenant_id: Uuid, id: Uuid, file_id: &str) -> Result<bool, DomainError> {
    let conn = app.db.conn()?;
    let res = attachment::Entity::update_many()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_UPLOADED))
        .col_expr(attachment::Column::ProviderFileId, Expr::value(Some(file_id.to_owned())))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq(STATUS_PENDING))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(res.rows_affected == 1)
}

/// `uploaded` → `ready` (with an optional thumbnail) while not deleted / claimed by cleanup.
///
/// # Errors
/// DB errors.
pub async fn set_ready(
    app: &AppServices,
    tenant_id: Uuid,
    id: Uuid,
    thumb: Option<thumbnail::Thumbnail>,
) -> Result<bool, DomainError> {
    let conn = app.db.conn()?;
    let mut q = attachment::Entity::update_many()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_READY))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()));
    if let Some(t) = thumb {
        q = q
            .col_expr(attachment::Column::ImgThumbnail, Expr::value(Some(t.bytes)))
            .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(Some(i32::try_from(t.width).unwrap_or(0))))
            .col_expr(attachment::Column::ImgThumbnailHeight, Expr::value(Some(i32::try_from(t.height).unwrap_or(0))));
    }
    let res = q
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq(STATUS_UPLOADED))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::CleanupStatus.is_null()),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Marks an in-flight row `failed` with `error_code` (best effort; errors are logged).
async fn mark_failed(app: &AppServices, tenant_id: Uuid, id: Uuid, code: &str) {
    let res = async {
        let conn = app.db.conn()?;
        attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value(STATUS_FAILED))
            .col_expr(attachment::Column::ErrorCode, Expr::value(Some(code.to_owned())))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED])),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        Ok::<_, DomainError>(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(attachment_id = %id, error = %e, "failed to mark the attachment failed");
    }
}

/// Best-effort, fire-and-forget provider file delete (not retried).
fn spawn_delete_file(app: &Arc<AppServices>, ctx: SecurityContext, provider: ResolvedProvider, file_id: String) {
    let app = Arc::clone(app);
    tokio::spawn(async move {
        if let Err(e) = storage::delete_file(app.transport.as_ref(), &ctx, &provider, &file_id).await {
            tracing::warn!(error = %e, "best-effort provider file delete failed");
        }
    });
}
