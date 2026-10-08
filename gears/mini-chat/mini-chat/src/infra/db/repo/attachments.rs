//! `attachments` and `message_attachments` repository.

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::{attachment, message_attachment};

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";

pub const KIND_DOCUMENT: &str = "document";
pub const KIND_IMAGE: &str = "image";

pub const CLEANUP_PENDING: &str = "pending";
pub const CLEANUP_DONE: &str = "done";
pub const CLEANUP_FAILED: &str = "failed";

pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    m: &attachment::Model,
) -> Result<(), DomainError> {
    let am = attachment::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        uploaded_by_user_id: Set(m.uploaded_by_user_id),
        filename: Set(m.filename.clone()),
        content_type: Set(m.content_type.clone()),
        size_bytes: Set(m.size_bytes),
        storage_backend: Set(m.storage_backend.clone()),
        provider_file_id: Set(m.provider_file_id.clone()),
        status: Set(m.status.clone()),
        error_code: Set(m.error_code.clone()),
        attachment_kind: Set(m.attachment_kind.clone()),
        for_file_search: Set(m.for_file_search),
        for_code_interpreter: Set(m.for_code_interpreter),
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
        created_at: Set(m.created_at),
        updated_at: Set(m.updated_at),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set(m.secondary_status.clone()),
        secondary_provider_kind: Set(m.secondary_provider_kind.clone()),
    };
    attachment::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

/// Attachment of a chat by id, optionally including soft-deleted rows.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
    include_deleted: bool,
) -> Result<Option<attachment::Model>, DomainError> {
    let mut cond = Condition::all()
        .add(attachment::Column::ChatId.eq(chat_id))
        .add(attachment::Column::Id.eq(id));
    if !include_deleted {
        cond = cond.add(attachment::Column::DeletedAt.is_null());
    }
    Ok(attachment::Entity::find()
        .filter(cond)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Attachment by id (system workers).
pub async fn find_by_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
) -> Result<Option<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(Condition::all().add(attachment::Column::Id.eq(id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Non-deleted attachments of a chat among `ids`.
pub async fn find_many_in_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids.iter().copied()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

fn active_for_limits(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::ChatId.eq(chat_id))
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::Status.ne(STATUS_FAILED))
}

/// Non-deleted, non-failed attachments of a chat (per-chat limit checks).
pub async fn active_for_limits_list(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(active_for_limits(chat_id))
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Ready, non-deleted attachments of a chat.
pub async fn ready_in_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::Status.eq(STATUS_READY)),
        )
        .secure()
        .scope_with(scope)
        .order_by(attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// Record the provider upload (`pending` → `uploaded`).
pub async fn mark_uploaded(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    provider_file_id: &str,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_UPLOADED))
        .col_expr(
            attachment::Column::ProviderFileId,
            Expr::value(provider_file_id.to_owned()),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq(STATUS_PENDING))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Record the secondary (Anthropic) copy.
pub async fn set_secondary(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    file_id: Option<&str>,
    status: &str,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    attachment::Entity::update_many()
        .secure()
        .col_expr(
            attachment::Column::SecondaryFileId,
            Expr::value(file_id.map(str::to_owned)),
        )
        .col_expr(attachment::Column::SecondaryStatus, Expr::value(status))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all().add(attachment::Column::Id.eq(id)))
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Thumbnail data of an image attachment.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// `pending|uploaded` → `ready` (not for rows owned by chat cleanup).
pub async fn mark_ready(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    thumbnail: Option<Thumbnail>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let mut upd = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_READY))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
    if let Some(t) = thumbnail {
        upd = upd
            .col_expr(attachment::Column::ImgThumbnail, Expr::value(t.bytes))
            .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(t.width))
            .col_expr(
                attachment::Column::ImgThumbnailHeight,
                Expr::value(t.height),
            );
    }
    let res = upd
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED]))
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// `pending|uploaded` → `failed` with an error code. When `cleanup` is set the
/// row is handed to the outbox attachment cleanup (`cleanup_status = pending`).
pub async fn mark_failed(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    error_code: &str,
    cleanup: bool,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let mut upd = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_FAILED))
        .col_expr(
            attachment::Column::ErrorCode,
            Expr::value(error_code.to_owned()),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
    if cleanup {
        upd = upd
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(CLEANUP_PENDING),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now));
    }
    let res = upd
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED]))
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Heartbeat of the background indexing task (`uploaded` rows not owned by
/// chat cleanup). `false` stops the task.
pub async fn heartbeat_uploaded(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq(STATUS_UPLOADED))
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Soft-delete an attachment and hand it to provider cleanup.
pub async fn soft_delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::DeletedAt, Expr::value(now))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(CLEANUP_PENDING),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// `true` when any message references the attachment.
pub async fn is_referenced(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<bool, DomainError> {
    let n = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::AttachmentId.eq(id)),
        )
        .secure()
        .scope_with(scope)
        .count(runner)
        .await?;
    Ok(n > 0)
}

/// Link attachments to a message.
pub async fn link_to_message(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    for att in attachment_ids {
        let am = message_attachment::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*att),
            created_at: Set(now),
        };
        message_attachment::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)?
            .exec(runner)
            .await?;
    }
    Ok(())
}

/// Attachment ids linked to a message.
pub async fn linked_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<Uuid>, DomainError> {
    let rows = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.eq(message_id)),
        )
        .secure()
        .scope_with(scope)
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?;
    Ok(rows.into_iter().map(|r| r.attachment_id).collect())
}

/// `(message_id, attachment)` pairs for the given messages; soft-deleted
/// attachments are excluded.
pub async fn summaries_for_messages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<Vec<(Uuid, attachment::Model)>, DomainError> {
    if message_ids.is_empty() {
        return Ok(vec![]);
    }
    let links = message_attachment::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachment::Column::ChatId.eq(chat_id))
                .add(message_attachment::Column::MessageId.is_in(message_ids.iter().copied())),
        )
        .secure()
        .scope_with(scope)
        .order_by(message_attachment::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?;
    if links.is_empty() {
        return Ok(vec![]);
    }
    let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    let atts = find_many_in_chat(runner, scope, chat_id, &att_ids).await?;
    let by_id: std::collections::HashMap<Uuid, attachment::Model> =
        atts.into_iter().map(|a| (a.id, a)).collect();
    Ok(links
        .into_iter()
        .filter_map(|l| {
            by_id
                .get(&l.attachment_id)
                .map(|a| (l.message_id, a.clone()))
        })
        .collect())
}

/// Hand every attachment of a deleted chat to chat cleanup.
pub async fn mark_chat_cleanup_pending(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let res = attachment::Entity::update_many()
        .secure()
        .col_expr(
            attachment::Column::CleanupStatus,
            Expr::value(CLEANUP_PENDING),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::CleanupStatus.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected)
}

/// Attachments of a chat with `cleanup_status = pending`.
pub async fn cleanup_pending_for_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::CleanupStatus.eq(CLEANUP_PENDING)),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Count of attachments of a chat whose cleanup ended `failed`.
pub async fn cleanup_failed_count(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<u64, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::CleanupStatus.eq(CLEANUP_FAILED)),
        )
        .secure()
        .scope_with(scope)
        .count(runner)
        .await?)
}

/// Record a cleanup outcome (`done` / `failed`) of a pending row.
pub async fn set_cleanup_outcome(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    status: &str,
    last_error: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::CleanupStatus, Expr::value(status))
        .col_expr(
            attachment::Column::LastCleanupError,
            Expr::value(last_error.map(str::to_owned)),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::CleanupStatus.eq(CLEANUP_PENDING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

/// Record a failed cleanup attempt; returns the new attempt count.
pub async fn record_cleanup_attempt(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    error: &str,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    attachment::Entity::update_many()
        .secure()
        .col_expr(
            attachment::Column::CleanupAttempts,
            Expr::col(attachment::Column::CleanupAttempts).add(1),
        )
        .col_expr(
            attachment::Column::LastCleanupError,
            Expr::value(error.to_owned()),
        )
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::CleanupStatus.eq(CLEANUP_PENDING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Upload reaper scan: stale `pending` / `uploaded` rows not owned by cleanup.
pub async fn stale_uploads(
    runner: &impl DBRunner,
    scope: &AccessScope,
    cutoff: OffsetDateTime,
    limit: u64,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED]))
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::UpdatedAt.lt(cutoff)),
        )
        .secure()
        .scope_with(scope)
        .order_by(attachment::Column::UpdatedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// Upload reaper CAS: fail an abandoned row (still stale, same status).
pub async fn reap_cas(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    status: &str,
    with_cleanup: bool,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let mut upd = attachment::Entity::update_many()
        .secure()
        .col_expr(attachment::Column::Status, Expr::value(STATUS_FAILED))
        .col_expr(
            attachment::Column::ErrorCode,
            Expr::value("upload_abandoned"),
        )
        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
    if with_cleanup {
        upd = upd
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(CLEANUP_PENDING),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now));
    }
    let res = upd
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq(status.to_owned()))
                .add(attachment::Column::CleanupStatus.is_null())
                .add(attachment::Column::DeletedAt.is_null())
                .add(attachment::Column::UpdatedAt.lt(cutoff)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}
