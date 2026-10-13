//! Statements on `attachments` and `message_attachments` (send pipeline, attachment service).
//! Tenant scoped; callers pass a `chat_id` taken from an owner-scoped chat query.

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect, Set,
};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::attachments::{self, Column};
use crate::infra::db::entity::message_attachments;
use crate::infra::db::{AttachmentKind, AttachmentStatus, CleanupStatus, ts};

/// `secondary_provider_kind` of an Anthropic Files copy.
pub const SECONDARY_PROVIDER_ANTHROPIC: &str = "anthropic";

/// The chat's non-deleted attachments in status `ready`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn ready_in_chat(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachments::Model>, DomainError> {
    attachments::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::Status.eq(AttachmentStatus::Ready.as_str()))
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// The attachments with these ids inside `scope`, in any chat and state (deleted included).
///
/// # Errors
/// `Internal` on a database error.
pub async fn by_ids(
    conn: &impl DBRunner,
    scope: &AccessScope,
    ids: &[Uuid],
) -> Result<Vec<attachments::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    attachments::Entity::find()
        .filter(Column::Id.is_in(ids.iter().copied()))
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// Links `attachment_ids` to the message `message_id` of `chat_id`.
///
/// # Errors
/// `AccessDenied` outside `scope`, `Internal` on a database error.
pub async fn link_to_message(
    tx: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    for &attachment_id in attachment_ids {
        let link = message_attachments::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(attachment_id),
            created_at: Set(ts::normalize(now)),
        };
        secure_insert::<message_attachments::Entity>(link, scope, tx)
            .await
            .map_err(map_scope_err)?;
    }
    Ok(())
}

/// The columns of a non-deleted attachment that the message list shows.
#[derive(Debug, Clone, FromQueryResult)]
pub struct SummaryRow {
    pub id: Uuid,
    pub attachment_kind: String,
    pub filename: String,
    pub status: String,
    pub img_thumbnail: Option<Vec<u8>>,
    pub img_thumbnail_width: Option<i32>,
    pub img_thumbnail_height: Option<i32>,
}

/// The non-deleted attachments linked to each of `message_ids` (messages of `chat_id`): the
/// inner join of `message_attachments` and `attachments` with `attachments.deleted_at IS NULL`,
/// per message in link order (`message_attachments.created_at`, then attachment id). Messages
/// without a live attachment are absent from the map.
///
/// # Errors
/// `Internal` on a database error.
pub async fn summaries_for_messages(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<SummaryRow>>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut links = message_attachments::Entity::find()
        .filter(message_attachments::Column::ChatId.eq(chat_id))
        .filter(message_attachments::Column::MessageId.is_in(message_ids.iter().copied()))
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)?;
    if links.is_empty() {
        return Ok(HashMap::new());
    }
    links.sort_by_key(|l| (l.created_at, l.attachment_id));

    let attachment_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let rows = attachments::Entity::find()
        .filter(Column::Id.is_in(attachment_ids))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .project_all(conn, |select| {
            select
                .select_only()
                .column(Column::Id)
                .column(Column::AttachmentKind)
                .column(Column::Filename)
                .column(Column::Status)
                .column(Column::ImgThumbnail)
                .column(Column::ImgThumbnailWidth)
                .column(Column::ImgThumbnailHeight)
                .into_model::<SummaryRow>()
        })
        .await
        .map_err(map_scope_err)?;
    let by_id: HashMap<Uuid, SummaryRow> = rows.into_iter().map(|r| (r.id, r)).collect();

    let mut out: HashMap<Uuid, Vec<SummaryRow>> = HashMap::new();
    for link in links {
        if let Some(row) = by_id.get(&link.attachment_id) {
            out.entry(link.message_id).or_default().push(row.clone());
        }
    }
    Ok(out)
}

/// Inserts an attachment row.
///
/// # Errors
/// `AccessDenied` outside `scope`, `Internal` on a database error.
pub async fn insert(
    conn: &impl DBRunner,
    scope: &AccessScope,
    row: attachments::ActiveModel,
) -> Result<(), DomainError> {
    secure_insert::<attachments::Entity>(row, scope, conn)
        .await
        .map(drop)
        .map_err(map_scope_err)
}

/// The attachment `id` of `chat_id` (also when soft-deleted).
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_in_chat(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<attachments::Model>, DomainError> {
    attachments::Entity::find()
        .filter(Column::Id.eq(id))
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

#[derive(Debug, FromQueryResult)]
struct UsageRow {
    attachment_kind: String,
    size_bytes: i64,
}

/// What counts against the per-chat limits: the non-deleted, non-failed attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatUsage {
    pub documents: u64,
    pub total_bytes: i64,
}

/// Documents and total size of the chat's non-deleted, non-failed attachments.
///
/// # Errors
/// `Internal` on a database error.
pub async fn chat_usage(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<ChatUsage, DomainError> {
    let rows = attachments::Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::Status.ne(AttachmentStatus::Failed.as_str()))
        .secure()
        .scope_with(scope)
        .project_all(conn, |select| {
            select
                .select_only()
                .column(Column::AttachmentKind)
                .column(Column::SizeBytes)
                .into_model::<UsageRow>()
        })
        .await
        .map_err(map_scope_err)?;
    Ok(rows.iter().fold(
        ChatUsage {
            documents: 0,
            total_bytes: 0,
        },
        |acc, r| ChatUsage {
            documents: acc.documents
                + u64::from(r.attachment_kind == AttachmentKind::Document.as_str()),
            total_bytes: acc.total_bytes.saturating_add(r.size_bytes),
        },
    ))
}

/// `pending` -> `uploaded` with the provider file id; `false` when the row is no longer a live
/// `pending` row (deleted, or claimed by the cleanup of its deleted chat).
///
/// # Errors
/// `Internal` on a database error.
pub async fn set_uploaded(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    provider_file_id: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::value(AttachmentStatus::Uploaded.as_str()),
        )
        .col_expr(
            Column::ProviderFileId,
            Expr::value(Some(provider_file_id.to_owned())),
        )
        .col_expr(Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.eq(AttachmentStatus::Pending.as_str()))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::CleanupStatus.is_null())
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// `pending` / `uploaded` -> `failed` with `error_code`; `false` when the row is in another state.
///
/// # Errors
/// `Internal` on a database error.
pub async fn set_failed(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    error_code: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(Column::ErrorCode, Expr::value(Some(error_code.to_owned())))
        .col_expr(Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.is_in([
            AttachmentStatus::Pending.as_str(),
            AttachmentStatus::Uploaded.as_str(),
        ]))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// The guard of every transition of an `uploaded` row that indexing makes: still `uploaded`, not
/// deleted and not claimed by a cleanup (chat deletion).
fn live_uploaded(id: Uuid) -> Condition {
    Condition::all()
        .add(Column::Id.eq(id))
        .add(Column::Status.eq(AttachmentStatus::Uploaded.as_str()))
        .add(Column::DeletedAt.is_null())
        .add(Column::CleanupStatus.is_null())
}

/// A WebP preview with its width and height.
pub type ThumbnailColumns = (Vec<u8>, i32, i32);

/// A live `uploaded` row -> `ready` (with the image preview, if any); `false` when the row was
/// deleted, claimed by a cleanup or is in another state.
///
/// # Errors
/// `Internal` on a database error.
pub async fn set_ready(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    thumbnail: Option<ThumbnailColumns>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let (data, width, height) = match thumbnail {
        Some((data, w, h)) => (Some(data), Some(w), Some(h)),
        None => (None, None, None),
    };
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::value(AttachmentStatus::Ready.as_str()),
        )
        .col_expr(Column::ImgThumbnail, Expr::value(data))
        .col_expr(Column::ImgThumbnailWidth, Expr::value(width))
        .col_expr(Column::ImgThumbnailHeight, Expr::value(height))
        .col_expr(Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(live_uploaded(id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Records the outcome of the secondary (Anthropic) copy on a live `uploaded` row:
/// `secondary_status = uploaded` with the file id, or `failed` without one;
/// `secondary_provider_kind = anthropic`. `false` when the row was deleted or claimed meanwhile.
///
/// # Errors
/// `Internal` on a database error.
pub async fn set_secondary(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    file_id: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let status = if file_id.is_some() {
        "uploaded"
    } else {
        "failed"
    };
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::SecondaryFileId,
            Expr::value(file_id.map(str::to_owned)),
        )
        .col_expr(Column::SecondaryStatus, Expr::value(status))
        .col_expr(
            Column::SecondaryProviderKind,
            Expr::value(Some(SECONDARY_PROVIDER_ANTHROPIC)),
        )
        .col_expr(Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(live_uploaded(id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Heartbeat of the background indexing task: refreshes `updated_at` of a live `uploaded` row;
/// `false` when the task must stop.
///
/// # Errors
/// `Internal` on a database error.
pub async fn touch_uploaded(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachments::Entity::update_many()
        .col_expr(Column::UpdatedAt, Expr::value(ts::normalize(now)))
        .filter(live_uploaded(id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// A live `uploaded` row -> `failed` / `error_code` with `cleanup_status = pending` (the provider
/// file is handed to the attachment cleanup); `false` when the row is in another state.
///
/// # Errors
/// `Internal` on a database error.
pub async fn fail_for_cleanup(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    error_code: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = ts::normalize(now);
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(Column::ErrorCode, Expr::value(Some(error_code.to_owned())))
        .col_expr(
            Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str())),
        )
        .col_expr(Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(live_uploaded(id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// The stale-upload predicate (DESIGN B.9.5): a live `pending` / `uploaded` row without a
/// cleanup owner whose `updated_at` is before `cutoff`. `cutoff` must be [`ts::normalize`]d.
fn stale_upload(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(Column::Status.is_in([
            AttachmentStatus::Pending.as_str(),
            AttachmentStatus::Uploaded.as_str(),
        ]))
        .add(Column::DeletedAt.is_null())
        .add(Column::CleanupStatus.is_null())
        .add(Column::UpdatedAt.lt(ts::normalize(cutoff)))
}

/// At most `limit` stale uploads, oldest `updated_at` first (background scan, `scope` is
/// typically `allow_all`). Advisory: only [`abandon_stale`] decides.
///
/// # Errors
/// `Internal` on a database error.
pub async fn stale_uploads(
    conn: &impl DBRunner,
    scope: &AccessScope,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<attachments::Model>, DomainError> {
    attachments::Entity::find()
        .filter(stale_upload(cutoff))
        .order_by_asc(Column::UpdatedAt)
        .order_by_asc(Column::Id)
        .limit(limit)
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// The reaper CAS: the stale `status` row `id` -> `failed` / `upload_abandoned`
/// (`updated_at = now`); with `schedule_cleanup` also `cleanup_status = pending`. `false` when the
/// row changed since the scan (finished, deleted or claimed by a cleanup).
///
/// # Errors
/// `Internal` on a database error.
pub async fn abandon_stale(
    tx: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    status: AttachmentStatus,
    cutoff: OffsetDateTime,
    schedule_cleanup: bool,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = ts::normalize(now);
    let mut update = attachments::Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::value(AttachmentStatus::Failed.as_str()),
        )
        .col_expr(Column::ErrorCode, Expr::value(Some(UPLOAD_ABANDONED)))
        .col_expr(Column::UpdatedAt, Expr::value(now));
    if schedule_cleanup {
        update = update
            .col_expr(
                Column::CleanupStatus,
                Expr::value(Some(CleanupStatus::Pending.as_str())),
            )
            .col_expr(Column::CleanupUpdatedAt, Expr::value(Some(now)));
    }
    let res = update
        .filter(Column::Id.eq(id))
        .filter(stale_upload(cutoff))
        .filter(Column::Status.eq(status.as_str()))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// `attachments.error_code` of a row failed by the upload reaper.
pub const UPLOAD_ABANDONED: &str = "upload_abandoned";

/// Soft-deletes a non-deleted attachment and marks its provider cleanup `pending`; `false` when
/// it was already deleted.
///
/// # Errors
/// `Internal` on a database error.
pub async fn soft_delete(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = ts::normalize(now);
    let res = attachments::Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(Some(now)))
        .col_expr(
            Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str())),
        )
        .col_expr(Column::CleanupUpdatedAt, Expr::value(Some(now)))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::Id.eq(id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Whether any message references attachment `id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn is_referenced(
    conn: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<bool, DomainError> {
    let link = message_attachments::Entity::find()
        .filter(message_attachments::Column::AttachmentId.eq(id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(link.is_some())
}

/// The attachment `id` of `chat_id` in `tenant_id` (also when soft-deleted), for the cleanup
/// handlers (background work, `scope` is typically `allow_all`).
///
/// # Errors
/// `Internal` on a database error.
pub async fn find_for_cleanup(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<attachments::Model>, DomainError> {
    attachments::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// The chat's attachments whose provider cleanup is `pending`, oldest first.
///
/// # Errors
/// `Internal` on a database error.
pub async fn pending_cleanup_in_chat(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Vec<attachments::Model>, DomainError> {
    attachments::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .order_by_asc(Column::CreatedAt)
        .order_by_asc(Column::Id)
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)
}

/// Whether any attachment of the chat has a terminally `failed` provider cleanup.
///
/// # Errors
/// `Internal` on a database error.
pub async fn has_failed_cleanup_in_chat(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let row = attachments::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::CleanupStatus.eq(CleanupStatus::Failed.as_str()))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(row.is_some())
}

/// `pending` -> `done` (`cleanup_updated_at = now`); `false` when the row is not `pending`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn mark_cleanup_done(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachments::Entity::update_many()
        .col_expr(
            Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Done.as_str())),
        )
        .col_expr(
            Column::CleanupUpdatedAt,
            Expr::value(Some(ts::normalize(now))),
        )
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::Id.eq(id))
        .filter(Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// Outcome of [`record_cleanup_failure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupFailure {
    /// The attempt was recorded; the row is now in this status (`pending` or `failed`).
    Recorded(CleanupStatus),
    /// The row was no longer `pending` with the observed attempt count (another delivery moved
    /// it meanwhile); nothing was written. Carries the row's current status (`None`: no row or
    /// no cleanup status).
    Lost(Option<CleanupStatus>),
}

/// Records one failed provider delete of a `pending` row whose attempt count was
/// `observed_attempts` when the delete started: `cleanup_attempts += 1`,
/// `last_cleanup_error = error`, `cleanup_updated_at = now`; the row becomes `failed` when the
/// attempts reach `max_attempts`, otherwise it stays `pending`.
///
/// Compare-and-set on `(pending, observed_attempts)`: when another delivery changed the row
/// since it was read, nothing is written and the current status is re-read
/// ([`CleanupFailure::Lost`]).
///
/// # Errors
/// `Internal` on a database error.
#[allow(clippy::too_many_arguments)] // one CAS write: key, observed state and new values
pub async fn record_cleanup_failure(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    id: Uuid,
    error: &str,
    max_attempts: u32,
    observed_attempts: i32,
    now: OffsetDateTime,
) -> Result<CleanupFailure, DomainError> {
    let attempts = observed_attempts.saturating_add(1);
    let status = if u32::try_from(attempts).unwrap_or(u32::MAX) >= max_attempts {
        CleanupStatus::Failed
    } else {
        CleanupStatus::Pending
    };
    let res = attachments::Entity::update_many()
        .col_expr(Column::CleanupAttempts, Expr::value(attempts))
        .col_expr(
            Column::LastCleanupError,
            Expr::value(Some(error.to_owned())),
        )
        .col_expr(Column::CleanupStatus, Expr::value(Some(status.as_str())))
        .col_expr(
            Column::CleanupUpdatedAt,
            Expr::value(Some(ts::normalize(now))),
        )
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::Id.eq(id))
        .filter(Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
        .filter(Column::CleanupAttempts.eq(observed_attempts))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    if res.rows_affected > 0 {
        return Ok(CleanupFailure::Recorded(status));
    }
    let current = attachments::Entity::find()
        .filter(Column::TenantId.eq(tenant_id))
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(CleanupFailure::Lost(
        current
            .and_then(|row| row.cleanup_status)
            .as_deref()
            .and_then(CleanupStatus::parse),
    ))
}
