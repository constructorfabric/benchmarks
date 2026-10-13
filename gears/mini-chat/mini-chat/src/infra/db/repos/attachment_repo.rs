//! `attachments` / `message_attachments` queries (children of an authorized
//! chat).

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use super::tenant_scope;
use crate::domain::enums::{AttachmentKind, AttachmentStatus, CleanupStatus, SecondaryStatus};
use crate::domain::error::DomainError;
use crate::infra::db::entities::{attachment, chat_vector_store, message_attachment};

/// Marks the chat's live attachments that have no cleanup state yet as
/// `cleanup_status = 'pending'` (chat delete). Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn mark_chat_cleanup_pending(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(attachment::Entity::update_many()
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::DeletedAt.is_null())
        .filter(attachment::Column::CleanupStatus.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Live attachments linked to each of `message_ids` (via `message_attachments`;
/// soft-deleted attachments are dropped, as an inner join would), in link
/// order. Messages without attachments are absent.
///
/// # Errors
/// Database failure.
pub async fn live_for_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<attachment::Model>>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = tenant_scope(tenant_id);
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::ChatId.eq(chat_id))
        .filter(message_attachment::Column::MessageId.is_in(message_ids.iter().copied()))
        .order_by_asc(message_attachment::Column::CreatedAt)
        .order_by_asc(message_attachment::Column::AttachmentId)
        .secure()
        .scope_with(&scope)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(HashMap::new());
    }
    let attachments: HashMap<Uuid, attachment::Model> = attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Id.is_in(links.iter().map(|l| l.attachment_id)))
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&scope)
        .all(runner)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let mut out: HashMap<Uuid, Vec<attachment::Model>> = HashMap::new();
    for link in links {
        if let Some(a) = attachments.get(&link.attachment_id) {
            out.entry(link.message_id).or_default().push(a.clone());
        }
    }
    Ok(out)
}

/// Live (non-deleted) attachments of the chat with `status = 'ready'`, oldest
/// first (served by the `(tenant_id, chat_id)` index).
///
/// # Errors
/// Database failure.
pub async fn ready_in_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(attachment::Column::TenantId.eq(tenant_id))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Status.eq(AttachmentStatus::Ready.as_str()))
        .filter(attachment::Column::DeletedAt.is_null())
        .order_by_asc(attachment::Column::CreatedAt)
        .order_by_asc(attachment::Column::Id)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(runner)
        .await?)
}

/// Live attachments of the chat whose id is in `ids` (any status, any
/// uploader); unknown, foreign-chat and deleted ids are absent.
///
/// # Errors
/// Database failure.
pub async fn live_in_chat_by_ids(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Id.is_in(ids.iter().copied()))
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(runner)
        .await?)
}

/// Inserts one `message_attachments` row per id (the message's attachment
/// list, DESIGN section 3.7).
///
/// # Errors
/// Database failure.
pub async fn link_to_message(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = tenant_scope(tenant_id);
    for id in ids {
        secure_insert::<message_attachment::Entity>(
            message_attachment::ActiveModel {
                tenant_id: Set(tenant_id),
                chat_id: Set(chat_id),
                message_id: Set(message_id),
                attachment_id: Set(*id),
                created_at: Set(now),
            },
            &scope,
            runner,
        )
        .await?;
    }
    Ok(())
}

/// The chat's provider vector store id, once created.
///
/// # Errors
/// Database failure.
pub async fn chat_vector_store_id(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<String>, DomainError> {
    let row = chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::TenantId.eq(tenant_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?;
    Ok(row.and_then(|r| r.vector_store_id))
}

// ── Upload / get / delete (attachment service) ──────────────────────────────

/// A new `pending` attachment row.
#[derive(Clone, Debug)]
pub struct NewAttachment {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

/// Inserts `a` with `status = 'pending'`.
///
/// # Errors
/// Database failure.
pub async fn insert_pending(
    runner: &impl DBRunner,
    a: &NewAttachment,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    secure_insert::<attachment::Entity>(
        attachment::ActiveModel {
            id: Set(a.id),
            tenant_id: Set(a.tenant_id),
            chat_id: Set(a.chat_id),
            uploaded_by_user_id: Set(a.uploaded_by_user_id),
            filename: Set(a.filename.clone()),
            content_type: Set(a.content_type.clone()),
            size_bytes: Set(a.size_bytes),
            storage_backend: Set(a.storage_backend.clone()),
            provider_file_id: Set(None),
            status: Set(AttachmentStatus::Pending.as_str().to_owned()),
            error_code: Set(None),
            attachment_kind: Set(a.kind.as_str().to_owned()),
            for_file_search: Set(a.for_file_search),
            for_code_interpreter: Set(a.for_code_interpreter),
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
            secondary_status: Set(SecondaryStatus::NotAttempted.as_str().to_owned()),
            secondary_provider_kind: Set(None),
        },
        &tenant_scope(a.tenant_id),
        runner,
    )
    .await?;
    Ok(())
}

#[derive(Debug, FromQueryResult)]
struct UsageRow {
    attachment_kind: String,
    size_bytes: i64,
}

/// What counts against the per-chat limits: live, non-failed attachments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatUsage {
    pub documents: u64,
    pub total_bytes: i64,
}

/// Document count and total size of the chat's live, non-failed attachments
/// (DESIGN section 4, RAG controls).
///
/// # Errors
/// Database failure.
pub async fn chat_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<ChatUsage, DomainError> {
    let rows = attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::DeletedAt.is_null())
        .filter(attachment::Column::Status.ne(AttachmentStatus::Failed.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .project_all(runner, |q| {
            q.select_only()
                .column(attachment::Column::AttachmentKind)
                .column(attachment::Column::SizeBytes)
                .into_model::<UsageRow>()
        })
        .await?;
    let mut usage = ChatUsage::default();
    for r in rows {
        if r.attachment_kind == AttachmentKind::Document.as_str() {
            usage.documents += 1;
        }
        usage.total_bytes = usage.total_bytes.saturating_add(r.size_bytes);
    }
    Ok(usage)
}

/// The attachment `id` of the chat, deleted or not, any uploader.
///
/// # Errors
/// Database failure.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?)
}

/// `pending` -> `uploaded` with the provider file id, only while the row is
/// live and not claimed by cleanup (a chat or attachment delete during the
/// provider upload leaves it untouched; the caller then owns the new file).
///
/// # Errors
/// Database failure.
pub async fn set_uploaded(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    provider_file_id: &str,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(attachment::Entity::update_many()
        .col_expr(
            attachment::Column::Status,
            Expr::value(AttachmentStatus::Uploaded.as_str()),
        )
        .col_expr(
            attachment::Column::ProviderFileId,
            Expr::value(Some(provider_file_id.to_owned())),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::Status.eq(AttachmentStatus::Pending.as_str()))
        .filter(attachment::Column::CleanupStatus.is_null())
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Thumbnail columns of an image (`webp`, width, height).
pub type ThumbnailColumns = (Vec<u8>, i32, i32);

/// `uploaded` -> `ready` (with the image thumbnail, if any), only while the
/// row is live and not claimed by cleanup.
///
/// # Errors
/// Database failure.
pub async fn set_ready(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    thumbnail: Option<ThumbnailColumns>,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let mut update = attachment::Entity::update_many()
        .col_expr(
            attachment::Column::Status,
            Expr::value(AttachmentStatus::Ready.as_str()),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
    if let Some((data, w, h)) = thumbnail {
        update = update
            .col_expr(attachment::Column::ImgThumbnail, Expr::value(Some(data)))
            .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(Some(w)))
            .col_expr(attachment::Column::ImgThumbnailHeight, Expr::value(Some(h)));
    }
    Ok(
        uploaded_and_unclaimed(update.filter(attachment::Column::Id.eq(id)))
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .exec(runner)
            .await?
            .rows_affected,
    )
}

/// `status = 'uploaded' AND cleanup_status IS NULL AND deleted_at IS NULL`.
fn uploaded_and_unclaimed(
    q: sea_orm::UpdateMany<attachment::Entity>,
) -> sea_orm::UpdateMany<attachment::Entity> {
    q.filter(attachment::Column::Status.eq(AttachmentStatus::Uploaded.as_str()))
        .filter(attachment::Column::CleanupStatus.is_null())
        .filter(attachment::Column::DeletedAt.is_null())
}

/// Marks an in-flight (`pending` / `uploaded`) row `failed` with `error_code`.
///
/// # Errors
/// Database failure.
pub async fn set_failed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    error_code: &str,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(attachment::Entity::update_many()
        .col_expr(
            attachment::Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(
            attachment::Column::ErrorCode,
            Expr::value(Some(error_code.to_owned())),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::Status.is_in([
            AttachmentStatus::Pending.as_str(),
            AttachmentStatus::Uploaded.as_str(),
        ]))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Background indexing heartbeat: refreshes `updated_at` of a live,
/// unclaimed `uploaded` row (B.9.5).
///
/// # Errors
/// Database failure.
pub async fn heartbeat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let update = attachment::Entity::update_many()
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::Id.eq(id));
    Ok(uploaded_and_unclaimed(update)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Background indexing failure: `failed` / `indexing_failed` with
/// `cleanup_status = 'pending'`, only while the row is live, unclaimed and
/// `uploaded`.
///
/// # Errors
/// Database failure.
pub async fn fail_indexing_for_cleanup(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    error_code: &str,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let update = attachment::Entity::update_many()
        .col_expr(
            attachment::Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(
            attachment::Column::ErrorCode,
            Expr::value(Some(error_code.to_owned())),
        )
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::Id.eq(id));
    Ok(uploaded_and_unclaimed(update)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Soft-deletes the caller's live attachment unless a message references it:
/// `deleted_at`, `cleanup_status = 'pending'`. Returns the affected count.
///
/// # Errors
/// Database failure.
pub async fn soft_delete_unreferenced(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    uploader: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let referenced = Query::select()
        .column(message_attachment::Column::AttachmentId)
        .from(message_attachment::Entity)
        .and_where(message_attachment::Column::ChatId.eq(chat_id))
        .and_where(message_attachment::Column::AttachmentId.eq(id))
        .to_owned();
    Ok(attachment::Entity::update_many()
        .col_expr(attachment::Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::UploadedByUserId.eq(uploader))
        .filter(attachment::Column::DeletedAt.is_null())
        .filter(attachment::Column::Id.not_in_subquery(referenced))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Whether any message of the chat references the attachment.
///
/// # Errors
/// Database failure.
pub async fn is_referenced(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> Result<bool, DomainError> {
    Ok(message_attachment::Entity::find()
        .filter(message_attachment::Column::ChatId.eq(chat_id))
        .filter(message_attachment::Column::AttachmentId.eq(id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?
        .is_some())
}

// ── Provider cleanup (outbox handlers) ──────────────────────────────────────

/// Longest `last_cleanup_error` stored (characters).
const MAX_CLEANUP_ERROR_CHARS: usize = 1000;

/// Attachments of the chat with `cleanup_status = 'pending'`, deleted or not
/// (chat cleanup owns every one of them once the chat is soft-deleted).
///
/// # Errors
/// Database failure.
pub async fn pending_cleanup_in_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .order_by_asc(attachment::Column::CreatedAt)
        .order_by_asc(attachment::Column::Id)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(runner)
        .await?)
}

/// Whether any attachment of the chat ended in `cleanup_status = 'failed'`.
///
/// # Errors
/// Database failure.
pub async fn has_failed_cleanup(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    Ok(attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::CleanupStatus.eq(CleanupStatus::Failed.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(runner)
        .await?
        .is_some())
}

/// Cleanup `pending` -> `done`. Returns the affected count (0 when the row
/// is no longer `pending`).
///
/// # Errors
/// Database failure.
pub async fn mark_cleanup_done(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    Ok(attachment::Entity::update_many()
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Done.as_str().to_owned())),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// A failed provider delete, recorded by [`record_cleanup_failure`].
#[derive(Clone, Copy, Debug)]
pub struct CleanupFailure<'a> {
    /// `cleanup_attempts` as read before the attempt (compare-and-set).
    pub seen_attempts: i32,
    pub error: &'a str,
    /// The attempt exhausts the budget: the row becomes `failed`.
    pub terminal: bool,
}

/// Records a failed provider delete on a `pending` row whose
/// `cleanup_attempts` is still `f.seen_attempts` (compare-and-set):
/// `cleanup_attempts = seen_attempts + 1`, `last_cleanup_error`,
/// `cleanup_updated_at`, and `cleanup_status = 'failed'` when `f.terminal`.
/// Returns the affected count (0 when another worker changed the row).
///
/// # Errors
/// Database failure.
pub async fn record_cleanup_failure(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    f: CleanupFailure<'_>,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let CleanupFailure {
        seen_attempts,
        error,
        terminal,
    } = f;
    let error: String = error.chars().take(MAX_CLEANUP_ERROR_CHARS).collect();
    let mut update = attachment::Entity::update_many()
        .col_expr(
            attachment::Column::CleanupAttempts,
            Expr::value(seen_attempts.saturating_add(1)),
        )
        .col_expr(
            attachment::Column::LastCleanupError,
            Expr::value(Some(error)),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
    if terminal {
        update = update.col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Failed.as_str().to_owned())),
        );
    }
    Ok(update
        .filter(attachment::Column::Id.eq(id))
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .filter(attachment::Column::CleanupAttempts.eq(seen_attempts))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

// ── Upload reaper (system job) ──────────────────────────────────────────────

/// `error_code` of an upload the reaper failed (DESIGN B.9.5).
pub const UPLOAD_ABANDONED: &str = "upload_abandoned";

/// Abandoned upload candidates across all tenants (system job, unscoped):
/// `status IN ('pending', 'uploaded') AND deleted_at IS NULL AND
/// cleanup_status IS NULL AND updated_at < cutoff`, oldest `updated_at`
/// first, at most `limit`.
///
/// # Errors
/// Database failure.
pub async fn stale_uploads(
    runner: &impl DBRunner,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(attachment::Column::Status.is_in([
            AttachmentStatus::Pending.as_str(),
            AttachmentStatus::Uploaded.as_str(),
        ]))
        .filter(attachment::Column::DeletedAt.is_null())
        .filter(attachment::Column::CleanupStatus.is_null())
        .filter(attachment::Column::UpdatedAt.lt(cutoff))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(attachment::Column::UpdatedAt, sea_orm::Order::Asc)
        .order_by(attachment::Column::Id, sea_orm::Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// One upload the reaper fails.
#[derive(Clone, Debug)]
pub struct AbandonUpload {
    pub tenant_id: Uuid,
    pub id: Uuid,
    /// The status the scan read (`pending` or `uploaded`).
    pub from_status: String,
    pub cutoff: OffsetDateTime,
    /// The row has a provider file: also set `cleanup_status = 'pending'`.
    pub with_cleanup: bool,
}

/// `status = 'failed'`, `error_code = 'upload_abandoned'`, `updated_at = now`
/// (and `cleanup_status = 'pending'` when `a.with_cleanup`), guarded by the
/// status the scan read, `deleted_at IS NULL`, `cleanup_status IS NULL` and
/// `updated_at < cutoff`. Returns the affected count (0 = the upload
/// finished, or the row was deleted or claimed by cleanup meanwhile).
///
/// # Errors
/// Database failure.
pub async fn abandon_upload(
    runner: &impl DBRunner,
    a: &AbandonUpload,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let mut update = attachment::Entity::update_many()
        .col_expr(
            attachment::Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(
            attachment::Column::ErrorCode,
            Expr::value(Some(UPLOAD_ABANDONED.to_owned())),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
    if a.with_cleanup {
        update = update
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
    }
    Ok(update
        .filter(attachment::Column::Id.eq(a.id))
        .filter(attachment::Column::Status.eq(a.from_status.as_str()))
        .filter(attachment::Column::DeletedAt.is_null())
        .filter(attachment::Column::CleanupStatus.is_null())
        .filter(attachment::Column::UpdatedAt.lt(a.cutoff))
        .secure()
        .scope_with(&tenant_scope(a.tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
