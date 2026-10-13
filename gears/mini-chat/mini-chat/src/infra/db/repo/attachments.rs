//! `attachments` / `message_attachments` repository (DESIGN section 3.7
//! `attachments`, `message_attachments`; "Cleanup on Chat Deletion").
//!
//! - Attachment summaries of a message page: one query, `message_attachments`
//!   inner join `attachments` on `(attachment_id, chat_id)`, live attachments
//!   only, projected to the `AttachmentSummary` columns. The provider file
//!   ids are never selected.
//! - Cleanup marks of a soft-deleted chat: `cleanup_status = pending` on the
//!   chat's live attachments that have no cleanup outcome yet.
//!
//! - Stream reads: the chat's `ready`, non-deleted attachments (tool gates,
//!   code-interpreter file ids, the citation `provider_file_id` map), the
//!   rows of a request's `attachment_ids` (validated by the stream service,
//!   also inside the reserve transaction) and the chat's vector store id.
//! - Stream write: the `message_attachments` links of a new user message.
//! - Turn mutations (DESIGN section 3.9): the live attachments linked to a
//!   user message (image guards of a retry / edit) and the copy of those
//!   links to the replacement user message; links to deleted attachments are
//!   not copied and the old rows stay for audit.
//! - Attachment service (DESIGN sections 3.3, 3.6, "Attachment Deletion";
//!   [`crate::domain::ports::AttachmentStore`]): the `pending` insert with the
//!   per-chat document and size limits (one transaction, the insert first),
//!   the guarded status transitions of an upload, the background indexing
//!   heartbeat and failure transaction, and the soft delete with its
//!   attachment cleanup outbox message.
//! - Attachment cleanup handler ("Attachment cleanup state machine"): the
//!   parent chat check, `cleanup_status = done`, and the failed-attempt
//!   bookkeeping (`cleanup_attempts`, `last_cleanup_error`, terminal
//!   `failed` at the attempt limit).
//!
//! Both tables are tenant-scoped; queries use the tenant predicates of the
//! parent chat scope plus the parent `chat_id`. Every transaction opened here
//! issues a write as its first statement, so on `SQLite` it takes the write
//! lock before it reads. The deletion transaction's first write is the no-op
//! lock of the parent chat row, which serializes it with the turn
//! transactions that link attachments to messages.

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait, Func, SimpleExpr};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, FromQueryResult, JoinType, Order,
    QueryFilter, QueryOrder, QuerySelect, RelationTrait,
};
use time::OffsetDateTime;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{
    DBRunner, DbTx, SecureEntityExt, SecureUpdateExt, secure_insert, secure_insert_many,
};
use toolkit_security::AccessScope;
use tracing::warn;
use uuid::Uuid;

use super::{DbRepository, child_scope};
use crate::domain::error::{DomainError, ExhaustedLimit, ResourceKind};
use crate::domain::ports::{
    AttachmentDeletion, AttachmentKind, AttachmentLinkCopy, AttachmentRecord, AttachmentRepository,
    AttachmentStatus, AttachmentStore, AttachmentSummaryRecord, ChatAttachmentRecord,
    ChatUploadLimits, CleanupStatus, MessageAttachmentLinks, NewAttachment, SecondaryAliasResolver,
    SecondaryCopy, StoredThumbnail, attachment_error,
};
use crate::infra::db::entities::{
    DbTimestamp, TimestampForm, attachments, chat_vector_stores, chats, message_attachments,
};
use crate::infra::outbox::{
    AttachmentCleanupEvent, AttachmentCleanupPayload, OutboxEnqueuer, SecondaryFileRef,
};

#[derive(Debug, FromQueryResult)]
struct SummaryRow {
    message_id: Uuid,
    attachment_id: Uuid,
    attachment_kind: String,
    filename: String,
    status: String,
    img_thumbnail: Option<Vec<u8>>,
    img_thumbnail_width: Option<i32>,
    img_thumbnail_height: Option<i32>,
}

impl SummaryRow {
    fn into_record(self) -> Result<AttachmentSummaryRecord, DomainError> {
        let kind = AttachmentKind::parse(&self.attachment_kind).ok_or_else(|| {
            DomainError::internal(format!(
                "attachment {} has an unknown stored kind `{}`",
                self.attachment_id, self.attachment_kind
            ))
        })?;
        let status = AttachmentStatus::parse(&self.status).ok_or_else(|| {
            DomainError::internal(format!(
                "attachment {} has an unknown stored status `{}`",
                self.attachment_id, self.status
            ))
        })?;
        let thumbnail = match (
            self.img_thumbnail,
            self.img_thumbnail_width,
            self.img_thumbnail_height,
        ) {
            (Some(data), Some(width), Some(height)) => Some(StoredThumbnail {
                data,
                width,
                height,
            }),
            _ => None,
        };
        Ok(AttachmentSummaryRecord {
            message_id: self.message_id,
            attachment_id: self.attachment_id,
            kind,
            filename: self.filename,
            status,
            thumbnail,
        })
    }
}

/// The live attachments linked to `message_ids` of `chat_id`, ordered by
/// link time (then attachment id), in one query.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn summaries_for_messages(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<Vec<AttachmentSummaryRecord>, DomainError> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    let scope = child_scope(chat_scope);
    let rows = message_attachments::Entity::find()
        .join(
            JoinType::InnerJoin,
            message_attachments::Relation::Attachment.def(),
        )
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.is_in(message_ids.iter().copied()))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .and_scope_for::<attachments::Entity>(&scope)
        .project_all(runner, |select| {
            select
                .select_only()
                .column(message_attachments::Column::MessageId)
                .column_as(attachments::Column::Id, "attachment_id")
                .column(attachments::Column::AttachmentKind)
                .column(attachments::Column::Filename)
                .column(attachments::Column::Status)
                .column(attachments::Column::ImgThumbnail)
                .column(attachments::Column::ImgThumbnailWidth)
                .column(attachments::Column::ImgThumbnailHeight)
                .order_by_asc(message_attachments::Column::CreatedAt)
                .order_by_asc(attachments::Column::Id)
                .into_model::<SummaryRow>()
        })
        .await?;
    rows.into_iter().map(SummaryRow::into_record).collect()
}

/// Marks the live attachments of `chat_id` without a cleanup outcome
/// `cleanup_status = pending` (`cleanup_updated_at = at`, bound as `ts`);
/// returns the number of marked rows. `scope` is the child scope of the chat.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_chat_for_cleanup(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> Result<u64, DomainError> {
    let result = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
        )
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::CleanupStatus.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected)
}

// ── Streaming ────────────────────────────────────────────────────────────

fn to_chat_record(model: attachments::Model) -> Result<ChatAttachmentRecord, DomainError> {
    let kind = AttachmentKind::parse(&model.attachment_kind).ok_or_else(|| {
        DomainError::internal(format!(
            "attachment {} has an unknown stored kind `{}`",
            model.id, model.attachment_kind
        ))
    })?;
    let status = AttachmentStatus::parse(&model.status).ok_or_else(|| {
        DomainError::internal(format!(
            "attachment {} has an unknown stored status `{}`",
            model.id, model.status
        ))
    })?;
    Ok(ChatAttachmentRecord {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        uploaded_by_user_id: model.uploaded_by_user_id,
        kind,
        status,
        for_file_search: model.for_file_search,
        for_code_interpreter: model.for_code_interpreter,
        filename: model.filename,
        provider_file_id: model.provider_file_id,
        secondary_file_id: model
            .secondary_file_id
            .filter(|_| model.secondary_status == SECONDARY_UPLOADED),
        deleted: model.deleted_at.is_some(),
    })
}

/// The `ready`, non-deleted attachments of `chat_id`, oldest first.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn ready_for_chat(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
    attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::Status.eq(AttachmentStatus::Ready.as_str()))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(attachments::Column::CreatedAt, Order::Asc)
        .order_by(attachments::Column::Id, Order::Asc)
        .all(runner)
        .await?
        .into_iter()
        .map(to_chat_record)
        .collect()
}

/// The attachments `ids` of `chat_id` visible through the tenant scope, in
/// any status, deleted or not.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::Id.is_in(ids.iter().copied())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .all(runner)
        .await?
        .into_iter()
        .map(to_chat_record)
        .collect()
}

/// Inserts the `message_attachments` rows of `link` (timestamps bound as
/// `ts`).
///
/// # Errors
///
/// [`DomainError`] when a row is outside the scope or the insert fails.
pub async fn link_to_message(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    link: MessageAttachmentLinks,
) -> Result<(), DomainError> {
    let rows: Vec<message_attachments::ActiveModel> = link
        .attachment_ids
        .iter()
        .map(|attachment_id| message_attachments::ActiveModel {
            tenant_id: Set(link.tenant_id),
            chat_id: Set(link.chat_id),
            message_id: Set(link.message_id),
            attachment_id: Set(*attachment_id),
            created_at: Set(ts.stamp(link.at)),
        })
        .collect();
    secure_insert_many::<message_attachments::Entity>(rows, &child_scope(chat_scope), runner)
        .await?;
    Ok(())
}

/// The provider vector store id of `chat_id`, once its creation finished.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn vector_store_id(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<String>, DomainError> {
    let found = chat_vector_stores::Entity::find()
        .filter(
            Condition::all()
                .add(chat_vector_stores::Column::ChatId.eq(chat_id))
                .add(chat_vector_stores::Column::VectorStoreId.is_not_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?;
    Ok(found.and_then(|row| row.vector_store_id))
}

// ── Turn mutations ───────────────────────────────────────────────────────

/// The non-deleted attachments linked to the message `message_id` of
/// `chat_id`, ordered by link time (then attachment id).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn live_for_message(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
    let links = message_attachments::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::MessageId.eq(message_id)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(message_attachments::Column::CreatedAt, Order::Asc)
        .order_by(message_attachments::Column::AttachmentId, Order::Asc)
        .all(runner)
        .await?;
    let ids: Vec<Uuid> = links.iter().map(|link| link.attachment_id).collect();
    let rows = find_in_chat(runner, chat_scope, chat_id, &ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| rows.iter().find(|row| row.id == *id && !row.deleted))
        .cloned()
        .collect())
}

/// Copies the links of `copy.from_message_id` to non-deleted attachments onto
/// `copy.to_message_id` (timestamps bound as `ts`); returns the copied ids in
/// link order.
///
/// # Errors
///
/// [`DomainError`] when a query or the insert fails.
pub async fn copy_live_links(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    copy: AttachmentLinkCopy,
) -> Result<Vec<Uuid>, DomainError> {
    let attachment_ids: Vec<Uuid> =
        live_for_message(runner, chat_scope, copy.chat_id, copy.from_message_id)
            .await?
            .into_iter()
            .map(|row| row.id)
            .collect();
    if !attachment_ids.is_empty() {
        link_to_message(
            runner,
            ts,
            chat_scope,
            MessageAttachmentLinks {
                tenant_id: copy.tenant_id,
                chat_id: copy.chat_id,
                message_id: copy.to_message_id,
                attachment_ids: attachment_ids.clone(),
                at: copy.at,
            },
        )
        .await?;
    }
    Ok(attachment_ids)
}

// ── Attachment service ───────────────────────────────────────────────────

fn to_record(model: attachments::Model) -> Result<AttachmentRecord, DomainError> {
    let kind = AttachmentKind::parse(&model.attachment_kind).ok_or_else(|| {
        DomainError::internal(format!(
            "attachment {} has an unknown stored kind `{}`",
            model.id, model.attachment_kind
        ))
    })?;
    let status = AttachmentStatus::parse(&model.status).ok_or_else(|| {
        DomainError::internal(format!(
            "attachment {} has an unknown stored status `{}`",
            model.id, model.status
        ))
    })?;
    let thumbnail = match (
        model.img_thumbnail,
        model.img_thumbnail_width,
        model.img_thumbnail_height,
    ) {
        (Some(data), Some(width), Some(height)) => Some(StoredThumbnail {
            data,
            width,
            height,
        }),
        _ => None,
    };
    Ok(AttachmentRecord {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        uploaded_by_user_id: model.uploaded_by_user_id,
        filename: model.filename,
        content_type: model.content_type,
        size_bytes: model.size_bytes,
        storage_backend: model.storage_backend,
        provider_file_id: model.provider_file_id,
        status,
        error_code: model.error_code,
        kind,
        for_file_search: model.for_file_search,
        for_code_interpreter: model.for_code_interpreter,
        thumbnail,
        cleanup_status: model.cleanup_status,
        cleanup_attempts: model.cleanup_attempts,
        created_at: model.created_at.at(),
        deleted_at: model.deleted_at.map(DbTimestamp::at),
        secondary_file_id: model.secondary_file_id,
        secondary_provider_kind: model.secondary_provider_kind,
        secondary_provider_id: model.secondary_provider_id,
    })
}

fn one_attachment(chat_id: Uuid, id: Uuid) -> Condition {
    Condition::all()
        .add(attachments::Column::Id.eq(id))
        .add(attachments::Column::ChatId.eq(chat_id))
}

/// `cleanup_status IS NULL`, `deleted_at IS NULL`: a row no cleanup owns.
fn unclaimed() -> Condition {
    Condition::all()
        .add(attachments::Column::DeletedAt.is_null())
        .add(attachments::Column::CleanupStatus.is_null())
}

/// `cleanup_status` is not terminal (`NULL` or `pending`).
fn cleanup_outstanding() -> Condition {
    Condition::any()
        .add(attachments::Column::CleanupStatus.is_null())
        .add(attachments::Column::CleanupStatus.eq(CleanupStatus::Pending.as_str()))
}

#[derive(Debug, FromQueryResult)]
struct ChatUsageRow {
    documents: i64,
    total_bytes: i64,
}

/// Number of documents and total `size_bytes` of the non-deleted, non-failed
/// attachments of `chat_id`.
async fn chat_usage(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<(u64, u64), DomainError> {
    let rows = attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::Status.ne(AttachmentStatus::Failed.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .project_all(runner, |select| {
            select
                .select_only()
                .column_as(
                    Expr::cust("COUNT(CASE WHEN attachment_kind = 'document' THEN 1 END)"),
                    "documents",
                )
                .column_as(
                    Expr::cust("CAST(COALESCE(SUM(size_bytes), 0) AS BIGINT)"),
                    "total_bytes",
                )
                .into_model::<ChatUsageRow>()
        })
        .await?;
    let usage = rows.into_iter().next().map_or((0, 0), |row| {
        (
            u64::try_from(row.documents).unwrap_or(0),
            u64::try_from(row.total_bytes).unwrap_or(0),
        )
    });
    Ok(usage)
}

/// Inserts `attachment` as `pending` (timestamps bound as `ts`), then checks
/// `limits` against the chat's usage including the new row. Run inside a
/// transaction: a limit error rolls the insert back.
///
/// # Errors
///
/// `429 document_limit` / `storage_limit`; [`DomainError`] when the insert or
/// the usage query fails.
pub async fn insert_pending(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    attachment: NewAttachment,
    limits: ChatUploadLimits,
) -> Result<AttachmentRecord, DomainError> {
    let kind = attachment.kind;
    let chat_id = attachment.chat_id;
    let model = attachments::ActiveModel {
        id: Set(attachment.id),
        tenant_id: Set(attachment.tenant_id),
        chat_id: Set(attachment.chat_id),
        uploaded_by_user_id: Set(attachment.uploaded_by_user_id),
        filename: Set(attachment.filename),
        content_type: Set(Some(attachment.content_type)),
        size_bytes: Set(Some(attachment.size_bytes)),
        storage_backend: Set(attachment.storage_backend),
        provider_file_id: Set(None),
        status: Set(AttachmentStatus::Pending.as_str().to_owned()),
        error_code: Set(None),
        attachment_kind: Set(kind.as_str().to_owned()),
        for_file_search: Set(attachment.for_file_search),
        for_code_interpreter: Set(attachment.for_code_interpreter),
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
        created_at: Set(ts.stamp(attachment.now)),
        updated_at: Set(ts.stamp(attachment.now)),
        deleted_at: Set(None),
        secondary_file_id: Set(None),
        secondary_status: Set(SECONDARY_NOT_ATTEMPTED.to_owned()),
        secondary_provider_kind: Set(None),
        secondary_provider_id: Set(None),
    };
    let inserted =
        secure_insert::<attachments::Entity>(model, &child_scope(chat_scope), runner).await?;
    let (documents, total_bytes) = chat_usage(runner, chat_scope, chat_id).await?;
    if kind == AttachmentKind::Document && documents > u64::from(limits.max_documents) {
        return Err(DomainError::ResourceExhausted {
            limit: ExhaustedLimit::DocumentLimit,
        });
    }
    if total_bytes > limits.max_total_bytes {
        return Err(DomainError::ResourceExhausted {
            limit: ExhaustedLimit::StorageLimit,
        });
    }
    to_record(inserted)
}

/// The attachment `id` of `chat_id`, deleted or not.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn find(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<AttachmentRecord>, DomainError> {
    attachments::Entity::find()
        .filter(one_attachment(chat_id, id))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
        .map(to_record)
        .transpose()
}

/// Applies `columns` (plus `updated_at = at`) to the attachment `id` of
/// `chat_id` when `guard` holds; `true` when the row was updated.
async fn guarded_update(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    guard: Condition,
    columns: Vec<(attachments::Column, SimpleExpr)>,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let mut update = attachments::Entity::update_many()
        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts.stamp(at)));
    for (column, value) in columns {
        update = update.col_expr(column, value);
    }
    let result = update
        .filter(one_attachment(chat_id, id).add(guard))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

fn status_value(status: AttachmentStatus) -> SimpleExpr {
    Expr::value(status.as_str().to_owned())
}

/// `pending -> uploaded` with `provider_file_id`, while the row is not
/// deleted and not claimed by a cleanup ([`unclaimed`]). `false` when the
/// attachment or its chat was deleted during the provider upload: the
/// cleanup that claimed the row saw no provider file, so the caller owns
/// the delete of the file it just uploaded.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_uploaded(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    provider_file_id: &str,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    guarded_update(
        runner,
        ts,
        chat_scope,
        (chat_id, id),
        unclaimed().add(attachments::Column::Status.eq(AttachmentStatus::Pending.as_str())),
        vec![
            (
                attachments::Column::Status,
                status_value(AttachmentStatus::Uploaded),
            ),
            (
                attachments::Column::ProviderFileId,
                Expr::value(Some(provider_file_id.to_owned())),
            ),
        ],
        at,
    )
    .await
}

/// `uploaded -> ready` with the thumbnail, while the row is not deleted and
/// not claimed by a cleanup.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_ready(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    thumbnail: Option<StoredThumbnail>,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let (data, width, height) = match thumbnail {
        Some(thumbnail) => (
            Some(thumbnail.data),
            Some(thumbnail.width),
            Some(thumbnail.height),
        ),
        None => (None, None, None),
    };
    guarded_update(
        runner,
        ts,
        chat_scope,
        (chat_id, id),
        unclaimed().add(attachments::Column::Status.eq(AttachmentStatus::Uploaded.as_str())),
        vec![
            (
                attachments::Column::Status,
                status_value(AttachmentStatus::Ready),
            ),
            (attachments::Column::ImgThumbnail, Expr::value(data)),
            (attachments::Column::ImgThumbnailWidth, Expr::value(width)),
            (attachments::Column::ImgThumbnailHeight, Expr::value(height)),
        ],
        at,
    )
    .await
}

/// `pending | uploaded -> failed` with `error_code`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_failed(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    error_code: &str,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    guarded_update(
        runner,
        ts,
        chat_scope,
        (chat_id, id),
        Condition::all().add(attachments::Column::Status.is_in([
            AttachmentStatus::Pending.as_str(),
            AttachmentStatus::Uploaded.as_str(),
        ])),
        vec![
            (
                attachments::Column::Status,
                status_value(AttachmentStatus::Failed),
            ),
            (
                attachments::Column::ErrorCode,
                Expr::value(Some(error_code.to_owned())),
            ),
        ],
        at,
    )
    .await
}

/// Refreshes `updated_at` of an `uploaded`, unclaimed row (background
/// indexing heartbeat).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn touch_uploaded(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    guarded_update(
        runner,
        ts,
        chat_scope,
        (chat_id, id),
        unclaimed().add(attachments::Column::Status.eq(AttachmentStatus::Uploaded.as_str())),
        Vec::new(),
        at,
    )
    .await
}

/// `secondary_status` of a row without a secondary upload attempt.
const SECONDARY_NOT_ATTEMPTED: &str = "not_attempted";
/// `secondary_status` of a stored secondary copy.
const SECONDARY_UPLOADED: &str = "uploaded";
/// `secondary_status` of a failed secondary upload.
const SECONDARY_FAILED: &str = "failed";

/// Records the secondary copy outcome while the row is not deleted and not
/// claimed by a cleanup.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_secondary(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    copy: SecondaryCopy,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let (status, file_id, provider_kind, provider_id) = match copy {
        SecondaryCopy::Uploaded {
            file_id,
            provider_kind,
            provider_id,
        } => (
            SECONDARY_UPLOADED,
            Some(file_id),
            provider_kind,
            provider_id,
        ),
        SecondaryCopy::Failed {
            provider_kind,
            provider_id,
        } => (SECONDARY_FAILED, None, provider_kind, provider_id),
    };
    guarded_update(
        runner,
        ts,
        chat_scope,
        (chat_id, id),
        unclaimed(),
        vec![
            (
                attachments::Column::SecondaryStatus,
                Expr::value(status.to_owned()),
            ),
            (attachments::Column::SecondaryFileId, Expr::value(file_id)),
            (
                attachments::Column::SecondaryProviderKind,
                Expr::value(Some(provider_kind)),
            ),
            (
                attachments::Column::SecondaryProviderId,
                Expr::value(Some(provider_id)),
            ),
        ],
        at,
    )
    .await
}

/// The upstream alias of `record`'s secondary copy, resolved by `aliases`
/// from the stored provider entry (`secondary_provider_id`; by kind only on
/// a row without one); `None` without a copy. A copy whose alias cannot be
/// resolved is logged and left in place.
fn secondary_alias(
    record: &AttachmentRecord,
    aliases: Option<&dyn SecondaryAliasResolver>,
) -> Option<String> {
    record.secondary_file_id.as_ref()?;
    let kind = record.secondary_provider_kind.as_deref()?;
    let provider_id = record.secondary_provider_id.as_deref();
    let alias =
        aliases.and_then(|aliases| aliases.upstream_alias(record.tenant_id, kind, provider_id));
    if alias.is_none() {
        warn!(target: "mini_chat.attachments", attachment_id = %record.id, provider_kind = %kind, provider_id = provider_id.unwrap_or_default(), "secondary copy has no resolvable upstream alias; the cleanup does not delete it");
    }
    alias
}

/// The attachment cleanup payload of `record` (enqueue time `at`).
/// `secondary_ref` is set when the row has a secondary copy (file id and
/// provider kind) and its upstream alias is known.
fn cleanup_payload(
    record: &AttachmentRecord,
    event_type: AttachmentCleanupEvent,
    at: OffsetDateTime,
    secondary_alias: Option<String>,
) -> AttachmentCleanupPayload {
    let secondary_ref = match (
        &record.secondary_file_id,
        &record.secondary_provider_kind,
        secondary_alias,
    ) {
        (Some(file_id), Some(provider_kind), Some(upstream_alias)) => Some(SecondaryFileRef {
            file_id: file_id.clone(),
            provider_kind: provider_kind.clone(),
            upstream_alias,
        }),
        _ => None,
    };
    AttachmentCleanupPayload {
        event_type,
        tenant_id: record.tenant_id,
        chat_id: record.chat_id,
        attachment_id: record.id,
        provider_file_id: record.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: record.storage_backend.clone(),
        attachment_kind: record.kind.as_str().to_owned(),
        deleted_at: at,
        secondary_ref,
    }
}

/// The background indexing failure unit of work inside `tx`: the guarded
/// update (`uploaded`, unclaimed) to `failed` / `indexing_failed` /
/// `cleanup_status = pending`, issued first, then the
/// `attachment_indexing_failed` cleanup message. `false` and an empty wake
/// when the guard does not hold.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query or outbox failure.
pub async fn fail_indexing_in_tx(
    tx: &(impl DBRunner + Sync),
    ts: TimestampForm,
    outbox: &OutboxEnqueuer,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    at: OffsetDateTime,
) -> Result<(bool, Wake), DomainError> {
    let updated = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::Status,
            status_value(AttachmentStatus::Failed),
        )
        .col_expr(
            attachments::Column::ErrorCode,
            Expr::value(Some(attachment_error::INDEXING_FAILED.to_owned())),
        )
        .col_expr(
            attachments::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
        )
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts.stamp(at)))
        .filter(
            one_attachment(chat_id, id)
                .add(unclaimed())
                .add(attachments::Column::Status.eq(AttachmentStatus::Uploaded.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec_with_returning(tx)
        .await?;
    let Some(row) = updated.into_iter().next() else {
        return Ok((false, Wake::empty()));
    };
    let record = to_record(row)?;
    let payload = cleanup_payload(&record, AttachmentCleanupEvent::IndexingFailed, at, None);
    let wake = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
    Ok((true, wake))
}

/// Whether any `message_attachments` row of `chat_id` references `id`.
async fn is_referenced(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<bool, DomainError> {
    let link = message_attachments::Entity::find()
        .filter(
            Condition::all()
                .add(message_attachments::Column::ChatId.eq(chat_id))
                .add(message_attachments::Column::AttachmentId.eq(id)),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?;
    Ok(link.is_some())
}

/// The deletion unit of work inside `tx`: the parent chat row lock issued
/// first ([`super::chats::lock_active`], a no-op `UPDATE`), the guarded soft
/// delete (`deleted_at = updated_at = at`, `cleanup_status = pending` when
/// unset, `cleanup_updated_at = at`), the reference check, then the
/// `attachment_deleted` cleanup message. Its `secondary_ref` is built from
/// the row returned by the soft delete: the stored secondary copy and the
/// alias `secondary_aliases` resolves from its stored provider entry.
///
/// The reserve transaction of a new turn and the retry / edit mutation
/// commit lock the same chat row (`touch`) before they read the attachments
/// and write `message_attachments` links. Taking it first serializes the
/// deletion with them per chat: a deletion committed first makes their
/// attachment check see `deleted_at` (`400 invalid_attachment`, or the link
/// is not copied), a link committed first makes the reference check here
/// see it (`409 attachment_locked`). No message is linked to a soft-deleted
/// attachment whose provider file is being deleted.
///
/// # Errors
///
/// `404` (chat) when the parent chat is no longer visible or was
/// soft-deleted; `409 attachment_locked` when a message references the
/// attachment (the caller's transaction must roll back);
/// [`DomainError::Internal`] on a query or outbox failure.
pub async fn soft_delete_in_tx(
    tx: &(impl DBRunner + Sync),
    ts: TimestampForm,
    outbox: &OutboxEnqueuer,
    chat_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    at: OffsetDateTime,
    secondary_aliases: Option<&dyn SecondaryAliasResolver>,
) -> Result<(AttachmentDeletion, Wake), DomainError> {
    if !super::chats::lock_active(tx, chat_scope, chat_id).await? {
        return Err(DomainError::not_found(
            ResourceKind::Chat,
            chat_id.to_string(),
        ));
    }
    let pending: SimpleExpr = Func::coalesce([
        Expr::col(attachments::Column::CleanupStatus),
        Expr::value(CleanupStatus::Pending.as_str()),
    ])
    .into();
    let updated = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::DeletedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .col_expr(attachments::Column::UpdatedAt, Expr::value(ts.stamp(at)))
        .col_expr(attachments::Column::CleanupStatus, pending)
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .filter(one_attachment(chat_id, id).add(attachments::Column::DeletedAt.is_null()))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec_with_returning(tx)
        .await?;
    let Some(row) = updated.into_iter().next() else {
        return Ok((AttachmentDeletion::AlreadyDeleted, Wake::empty()));
    };
    if is_referenced(tx, chat_scope, chat_id, id).await? {
        return Err(DomainError::attachment_locked());
    }
    let record = to_record(row)?;
    let alias = secondary_alias(&record, secondary_aliases);
    let payload = cleanup_payload(&record, AttachmentCleanupEvent::Deleted, at, alias);
    let wake = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
    Ok((AttachmentDeletion::Deleted, wake))
}

// ── Attachment cleanup handler ───────────────────────────────────────────

/// Whether the chat `chat_id` visible through the tenant scope is
/// soft-deleted; `None` when no such chat exists.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn chat_deleted(
    runner: &impl DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<bool>, DomainError> {
    let chat = chats::Entity::find()
        .filter(chats::Column::Id.eq(chat_id))
        .secure()
        .scope_with(tenant_scope)
        .one(runner)
        .await?;
    Ok(chat.map(|chat| chat.deleted_at.is_some()))
}

/// `cleanup_status = done` (`cleanup_updated_at = at`) while the cleanup is
/// outstanding; `false` when it already has a terminal outcome.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_cleanup_done(
    runner: &impl DBRunner,
    ts: TimestampForm,
    tenant_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let result = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupStatus,
            Expr::value(Some(CleanupStatus::Done.as_str().to_owned())),
        )
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .filter(one_attachment(chat_id, id).add(cleanup_outstanding()))
        .secure()
        .scope_with(&child_scope(tenant_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// A failed provider delete recorded on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupFailure {
    /// `cleanup_attempts` after the increment.
    pub attempts: i32,
    /// The attempt limit was reached: `cleanup_status = failed`.
    pub terminal: bool,
}

/// Records a failed provider delete inside `tx`: `cleanup_attempts + 1`,
/// `last_cleanup_error`, `cleanup_updated_at = at` (the first statement),
/// then `cleanup_status = failed` when the attempts reach `max_attempts`.
/// `None` when the cleanup already has a terminal outcome.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn record_cleanup_failure_in_tx(
    tx: &impl DBRunner,
    ts: TimestampForm,
    tenant_scope: &AccessScope,
    (chat_id, id): (Uuid, Uuid),
    error: &str,
    max_attempts: u32,
    at: OffsetDateTime,
) -> Result<Option<CleanupFailure>, DomainError> {
    let scope = child_scope(tenant_scope);
    let updated = attachments::Entity::update_many()
        .col_expr(
            attachments::Column::CleanupAttempts,
            Expr::col(attachments::Column::CleanupAttempts).add(1),
        )
        .col_expr(
            attachments::Column::LastCleanupError,
            Expr::value(Some(error.to_owned())),
        )
        .col_expr(
            attachments::Column::CleanupUpdatedAt,
            Expr::value(Some(ts.stamp(at))),
        )
        .filter(one_attachment(chat_id, id).add(cleanup_outstanding()))
        .secure()
        .scope_with(&scope)
        .exec_with_returning(tx)
        .await?;
    let Some(row) = updated.into_iter().next() else {
        return Ok(None);
    };
    let attempts = row.cleanup_attempts;
    let terminal = u32::try_from(attempts).unwrap_or(0) >= max_attempts;
    if terminal {
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(CleanupStatus::Failed.as_str().to_owned())),
            )
            .filter(one_attachment(chat_id, id))
            .secure()
            .scope_with(&scope)
            .exec(tx)
            .await?;
    }
    Ok(Some(CleanupFailure { attempts, terminal }))
}

#[async_trait]
impl AttachmentStore for DbRepository {
    async fn insert_pending(
        &self,
        scope: &AccessScope,
        attachment: NewAttachment,
        limits: ChatUploadLimits,
    ) -> Result<AttachmentRecord, DomainError> {
        let ts = self.timestamps();
        let scope = scope.clone();
        self.db()
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move { insert_pending(tx, ts, &scope, attachment, limits).await })
            })
            .await
    }

    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<AttachmentRecord>, DomainError> {
        let conn = self.db().conn()?;
        find(&conn, scope, chat_id, id).await
    }

    async fn mark_uploaded(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        mark_uploaded(
            &conn,
            self.timestamps(),
            scope,
            (chat_id, id),
            provider_file_id,
            at,
        )
        .await
    }

    async fn mark_ready(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        thumbnail: Option<StoredThumbnail>,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        mark_ready(
            &conn,
            self.timestamps(),
            scope,
            (chat_id, id),
            thumbnail,
            at,
        )
        .await
    }

    async fn mark_failed(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        mark_failed(
            &conn,
            self.timestamps(),
            scope,
            (chat_id, id),
            error_code,
            at,
        )
        .await
    }

    async fn touch_uploaded(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        touch_uploaded(&conn, self.timestamps(), scope, (chat_id, id), at).await
    }

    async fn fail_indexing_with_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let db = self.db().db();
        let ts = self.timestamps();
        let outbox = std::sync::Arc::clone(self.outbox());
        let scope = scope.clone();
        in_transaction(&db, move |tx| {
            Box::pin(async move {
                fail_indexing_in_tx(tx, ts, &outbox, &scope, (chat_id, id), at).await
            })
        })
        .await
    }

    async fn mark_secondary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        copy: SecondaryCopy,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db().conn()?;
        mark_secondary(&conn, self.timestamps(), scope, (chat_id, id), copy, at).await
    }

    async fn soft_delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
        at: OffsetDateTime,
        secondary_aliases: Option<std::sync::Arc<dyn SecondaryAliasResolver>>,
    ) -> Result<AttachmentDeletion, DomainError> {
        let db = self.db().db();
        let ts = self.timestamps();
        let outbox = std::sync::Arc::clone(self.outbox());
        let scope = scope.clone();
        in_transaction(&db, move |tx| {
            Box::pin(async move {
                soft_delete_in_tx(
                    tx,
                    ts,
                    &outbox,
                    &scope,
                    (chat_id, id),
                    at,
                    secondary_aliases.as_deref(),
                )
                .await
            })
        })
        .await
    }
}

#[async_trait]
impl AttachmentRepository for DbRepository {
    async fn summaries_for_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<Vec<AttachmentSummaryRecord>, DomainError> {
        let conn = self.db().conn()?;
        summaries_for_messages(&conn, scope, chat_id, message_ids).await
    }

    async fn ready_for_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
        let conn = self.db().conn()?;
        ready_for_chat(&conn, scope, chat_id).await
    }

    async fn find_in_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_in_chat(&conn, scope, chat_id, ids).await
    }

    async fn find_in_chat_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
        find_in_chat(tx, scope, chat_id, ids).await
    }

    async fn link_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        link: MessageAttachmentLinks,
    ) -> Result<(), DomainError> {
        link_to_message(tx, self.timestamps(), scope, link).await
    }

    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.db().conn()?;
        vector_store_id(&conn, scope, chat_id).await
    }

    async fn live_for_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Vec<ChatAttachmentRecord>, DomainError> {
        let conn = self.db().conn()?;
        live_for_message(&conn, scope, chat_id, message_id).await
    }

    async fn copy_live_links_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        copy: AttachmentLinkCopy,
    ) -> Result<Vec<Uuid>, DomainError> {
        copy_live_links(tx, self.timestamps(), scope, copy).await
    }
}

// ── Upload reaper and chat cleanup handler ───────────────────────────────

pub use background::{
    AbandonedUpload, ChatCleanupState, PendingCleanup, StaleUpload, abandon_upload_in_tx,
    chat_cleanup_state, pending_cleanup_for_chat, stale_uploads,
};

/// The upload reaper's scan and abandon unit of work (DESIGN B.9.5) and the
/// chat cleanup handler's reads (section 3.6 "Cleanup on Chat Deletion").
///
/// The reaper scan is a system read across tenants (unconstrained scope);
/// every other query is confined to one tenant and chat.
mod background {
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
    use time::OffsetDateTime;
    use toolkit_db::outbox::Wake;
    use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt};
    use toolkit_security::AccessScope;
    use uuid::Uuid;

    use super::{child_scope, cleanup_payload, one_attachment, status_value, to_record};
    use crate::domain::error::DomainError;
    use crate::domain::ports::{AttachmentStatus, CleanupStatus, UPLOAD_ABANDONED};
    use crate::infra::db::entities::{TimestampForm, attachments};
    use crate::infra::outbox::{AttachmentCleanupEvent, OutboxEnqueuer};

    /// An abandoned-upload candidate of the reaper scan (advisory: the
    /// abandon CAS re-checks every predicate).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StaleUpload {
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        /// `pending` or `uploaded` at scan time; the CAS requires it unchanged.
        pub status: AttachmentStatus,
    }

    /// A row the reaper failed as `upload_abandoned`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AbandonedUpload {
        pub from_status: AttachmentStatus,
        /// An attachment cleanup message was enqueued (the row had a
        /// `provider_file_id`) and `cleanup_status = pending` was set.
        pub cleanup_enqueued: bool,
        /// Secondary copy the cleanup message does not reference.
        pub secondary_file_id: Option<String>,
    }

    /// `status IN (pending, uploaded) AND deleted_at IS NULL AND
    /// cleanup_status IS NULL AND updated_at < cutoff`.
    fn abandoned(ts: TimestampForm, cutoff: OffsetDateTime) -> Condition {
        Condition::all()
            .add(attachments::Column::DeletedAt.is_null())
            .add(attachments::Column::CleanupStatus.is_null())
            .add(attachments::Column::UpdatedAt.lt(ts.stamp(cutoff)))
    }

    /// At most `limit` abandoned uploads (all tenants), oldest `updated_at`
    /// first.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure or a stored status
    /// outside the value set.
    pub async fn stale_uploads(
        runner: &impl DBRunner,
        ts: TimestampForm,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        let rows = attachments::Entity::find()
            .filter(
                abandoned(ts, cutoff).add(attachments::Column::Status.is_in([
                    AttachmentStatus::Pending.as_str(),
                    AttachmentStatus::Uploaded.as_str(),
                ])),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .order_by(attachments::Column::UpdatedAt, Order::Asc)
            .order_by(attachments::Column::Id, Order::Asc)
            .limit(limit)
            .all(runner)
            .await?;
        rows.into_iter()
            .map(|row| {
                let status = AttachmentStatus::parse(&row.status).ok_or_else(|| {
                    DomainError::internal(format!(
                        "attachment {} has an unknown stored status `{}`",
                        row.id, row.status
                    ))
                })?;
                Ok(StaleUpload {
                    id: row.id,
                    tenant_id: row.tenant_id,
                    chat_id: row.chat_id,
                    status,
                })
            })
            .collect()
    }

    /// The abandon unit of work inside `tx` (timestamps bound as `ts`): the
    /// CAS `status = failed`, `error_code = upload_abandoned`, `updated_at =
    /// at`, guarded by the scanned status, `deleted_at IS NULL`,
    /// `cleanup_status IS NULL` and `updated_at < cutoff` (the first
    /// statement); when the row has a `provider_file_id`, `cleanup_status =
    /// pending` and the `attachment_upload_abandoned` cleanup message.
    /// `None` and an empty wake when the CAS matched nothing.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query or outbox failure.
    pub async fn abandon_upload_in_tx(
        tx: &(impl DBRunner + Sync),
        ts: TimestampForm,
        outbox: &OutboxEnqueuer,
        upload: StaleUpload,
        cutoff: OffsetDateTime,
        at: OffsetDateTime,
    ) -> Result<(Option<AbandonedUpload>, Wake), DomainError> {
        let scope = child_scope(&AccessScope::for_tenant(upload.tenant_id));
        let updated = attachments::Entity::update_many()
            .col_expr(
                attachments::Column::Status,
                status_value(AttachmentStatus::Failed),
            )
            .col_expr(
                attachments::Column::ErrorCode,
                Expr::value(Some(UPLOAD_ABANDONED.to_owned())),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(ts.stamp(at)))
            .filter(
                one_attachment(upload.chat_id, upload.id)
                    .add(abandoned(ts, cutoff))
                    .add(attachments::Column::Status.eq(upload.status.as_str())),
            )
            .secure()
            .scope_with(&scope)
            .exec_with_returning(tx)
            .await?;
        let Some(row) = updated.into_iter().next() else {
            return Ok((None, Wake::empty()));
        };
        let secondary_file_id = row.secondary_file_id.clone();
        let record = to_record(row)?;
        if record.provider_file_id.is_none() {
            let abandoned = AbandonedUpload {
                from_status: upload.status,
                cleanup_enqueued: false,
                secondary_file_id,
            };
            return Ok((Some(abandoned), Wake::empty()));
        }
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(Some(CleanupStatus::Pending.as_str().to_owned())),
            )
            .col_expr(
                attachments::Column::CleanupUpdatedAt,
                Expr::value(Some(ts.stamp(at))),
            )
            .filter(one_attachment(upload.chat_id, upload.id))
            .secure()
            .scope_with(&scope)
            .exec(tx)
            .await?;
        let payload = cleanup_payload(&record, AttachmentCleanupEvent::UploadAbandoned, at, None);
        let wake = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
        let abandoned = AbandonedUpload {
            from_status: upload.status,
            cleanup_enqueued: true,
            secondary_file_id,
        };
        Ok((Some(abandoned), wake))
    }

    /// An attachment of a soft-deleted chat whose provider cleanup is
    /// outstanding (`cleanup_status = pending`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PendingCleanup {
        pub id: Uuid,
        pub provider_file_id: Option<String>,
        pub storage_backend: String,
        pub secondary_file_id: Option<String>,
        pub secondary_provider_kind: Option<String>,
        /// Provider entry the secondary copy was uploaded with.
        pub secondary_provider_id: Option<String>,
    }

    /// The attachments of `chat_id` with `cleanup_status = pending` (deleted
    /// or not), oldest first.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn pending_cleanup_for_chat(
        runner: &impl DBRunner,
        tenant_scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<PendingCleanup>, DomainError> {
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::CleanupStatus.eq(CleanupStatus::Pending.as_str())),
            )
            .secure()
            .scope_with(&child_scope(tenant_scope))
            .order_by(attachments::Column::CreatedAt, Order::Asc)
            .order_by(attachments::Column::Id, Order::Asc)
            .all(runner)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| PendingCleanup {
                id: row.id,
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                secondary_file_id: row.secondary_file_id,
                secondary_provider_kind: row.secondary_provider_kind,
                secondary_provider_id: row.secondary_provider_id,
            })
            .collect())
    }

    /// Attachment cleanup outcomes of one chat.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ChatCleanupState {
        /// Rows still `pending`.
        pub pending: u64,
        /// Rows terminal `failed`.
        pub failed: u64,
    }

    /// Number of `pending` and `failed` cleanup rows of `chat_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a query failure.
    pub async fn chat_cleanup_state(
        runner: &impl DBRunner,
        tenant_scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatCleanupState, DomainError> {
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::CleanupStatus.is_in([
                        CleanupStatus::Pending.as_str(),
                        CleanupStatus::Failed.as_str(),
                    ])),
            )
            .secure()
            .scope_with(&child_scope(tenant_scope))
            .all(runner)
            .await?;
        Ok(rows
            .iter()
            .fold(ChatCleanupState::default(), |mut state, row| {
                if row.cleanup_status.as_deref() == Some(CleanupStatus::Pending.as_str()) {
                    state.pending += 1;
                } else {
                    state.failed += 1;
                }
                state
            }))
    }
}
