//! The SQL implementation of the attachment persistence ports
//! ([`AttachmentStore`], [`ChatVectorStoreRepo`]) over the Secure ORM.
//!
//! Every statement is scoped to the tenant (`AccessScope::for_tenant`) and filters on the chat or
//! the row id as the contract requires; every status transition is a guarded conditional update,
//! so a concurrent transition (the reaper, a delete) is never overwritten. The outbox enqueue of
//! the background failure runs in the transaction of the status update and the wake fires only
//! after the commit. No ordering by a timestamp column is done here (implementation note 1).

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Set};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::attachments::cleanup::{AttemptRecord, CleanupFrom, CleanupRow, CleanupStore};
use crate::domain::attachments::facts::{AttachmentFactsStore, ReadyFile};
use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::model::{
    AttachmentKind, AttachmentRecord, AttachmentStatus, SecondaryStatus, StoredThumbnail, Thumbnail,
};
use crate::domain::attachments::reaper::{StaleUpload, UploadReaperStore};
use crate::domain::attachments::store::SecondaryRef;
use crate::domain::attachments::store::{
    AttachmentCleanupMessage, AttachmentCleanupQueue, AttachmentStore, CleanupEvent, DeleteOutcome,
    DeleteRequest, NewAttachment, SecondaryUpdate,
};
use crate::domain::attachments::vector_store::{
    ChatVectorStoreRepo, PlaceholderInsert, VectorStoreRow,
};
use crate::domain::error::{AttachmentError, DomainError, ResourceKind};
use crate::infra::storage::entity::{attachment, chat, chat_vector_store, message_attachment};
use crate::infra::storage::keyset_page::{TimeStorage, time_bound, time_key};
use crate::infra::storage::stream_store::time_storage;

fn scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

fn status_of(text: &str) -> AttachmentStatus {
    match text {
        "pending" => AttachmentStatus::Pending,
        "uploaded" => AttachmentStatus::Uploaded,
        "ready" => AttachmentStatus::Ready,
        _ => AttachmentStatus::Failed,
    }
}

fn secondary_of(text: &str) -> SecondaryStatus {
    match text {
        "pending" => SecondaryStatus::Pending,
        "uploaded" => SecondaryStatus::Uploaded,
        "failed" => SecondaryStatus::Failed,
        _ => SecondaryStatus::NotAttempted,
    }
}

fn record_of(row: attachment::Model) -> AttachmentRecord {
    AttachmentRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        uploaded_by_user_id: row.uploaded_by_user_id,
        filename: row.filename,
        content_type: row.content_type.unwrap_or_default(),
        size_bytes: row.size_bytes.unwrap_or_default(),
        storage_backend: row.storage_backend,
        provider_file_id: row.provider_file_id,
        status: status_of(&row.status),
        error_code: row.error_code,
        kind: if row.attachment_kind == "image" {
            AttachmentKind::Image
        } else {
            AttachmentKind::Document
        },
        for_file_search: row.for_file_search,
        for_code_interpreter: row.for_code_interpreter,
        thumbnail: match (
            row.img_thumbnail,
            row.img_thumbnail_width,
            row.img_thumbnail_height,
        ) {
            (Some(bytes), Some(width), Some(height)) => Some(StoredThumbnail {
                bytes,
                width,
                height,
            }),
            _ => None,
        },
        secondary_file_id: row.secondary_file_id,
        secondary_status: secondary_of(&row.secondary_status),
        secondary_provider_kind: row.secondary_provider_kind,
        cleanup_status: row.cleanup_status,
        cleanup_attempts: row.cleanup_attempts,
        created_at: row.created_at,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
    }
}

fn live(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::ChatId.eq(chat_id))
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::Status.ne("failed"))
}

async fn live_rows(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Vec<attachment::Model>, DomainError> {
    Ok(attachment::Entity::find()
        .filter(live(chat_id))
        .secure()
        .scope_with(&scope(tenant_id))
        .all(runner)
        .await?)
}

fn sum(rows: &[attachment::Model]) -> u64 {
    rows.iter()
        .map(|row| u64::try_from(row.size_bytes.unwrap_or_default()).unwrap_or_default())
        .sum()
}

fn guard_live_uploaded(id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::Id.eq(id))
        .add(attachment::Column::Status.eq("uploaded"))
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::CleanupStatus.is_null())
}

#[async_trait]
impl AttachmentStore for DBProvider<DomainError> {
    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<AttachmentRecord, DomainError> {
        let new = new.clone();
        let limits = *limits;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-begin
        // one transaction: a refused limit rolls back and nothing is inserted
        self.transaction(move |tx| {
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-begin
            Box::pin(async move {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-sum-size
                // the rows `pending` and `uploaded` count; a failed or deleted row counts toward
                // neither limit
                let rows = live_rows(tx, new.tenant_id, new.chat_id).await?;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-sum-size
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-is-document
                if new.kind == AttachmentKind::Document {
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-count-docs
                    let documents = u64::try_from(
                        rows.iter()
                            .filter(|row| row.attachment_kind == "document")
                            .count(),
                    )
                    .unwrap_or(u64::MAX);
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-count-docs
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-doc-limit
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return-doc-limit
                    if limits.document_limit_reached(documents) {
                        return Err(DomainError::Attachment(AttachmentError::DocumentLimit));
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return-doc-limit
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-doc-limit
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-is-document
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-storage-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return-storage-limit
                if limits.storage_limit_reached(sum(&rows)) {
                    return Err(DomainError::Attachment(AttachmentError::StorageLimit));
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return-storage-limit
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-storage-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-insert
                let row = attachment::ActiveModel {
                    id: Set(new.id),
                    tenant_id: Set(new.tenant_id),
                    chat_id: Set(new.chat_id),
                    uploaded_by_user_id: Set(new.uploaded_by_user_id),
                    filename: Set(new.filename.clone()),
                    content_type: Set(Some(new.content_type.clone())),
                    size_bytes: Set(Some(0)),
                    storage_backend: Set(new.storage_backend.clone()),
                    provider_file_id: Set(None),
                    status: Set("pending".to_owned()),
                    error_code: Set(None),
                    attachment_kind: Set(new.kind.as_str().to_owned()),
                    for_file_search: Set(new.for_file_search),
                    for_code_interpreter: Set(new.for_code_interpreter),
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
                    created_at: Set(new.now),
                    updated_at: Set(new.now),
                    deleted_at: Set(None),
                    secondary_file_id: Set(None),
                    secondary_status: Set("not_attempted".to_owned()),
                    secondary_provider_kind: Set(None),
                };
                let model =
                    secure_insert::<attachment::Entity>(row, &scope(new.tenant_id), tx).await?;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-insert
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return
                Ok(record_of(model))
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-return
            })
        })
        .await
    }

    async fn mark_uploaded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        provider_file_id: &str,
        size_bytes: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::ProviderFileId,
                Expr::value(provider_file_id.to_owned()),
            )
            .col_expr(attachment::Column::SizeBytes, Expr::value(size_bytes))
            .col_expr(attachment::Column::Status, Expr::value("uploaded"))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::Status.eq("pending")),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        if affected == 0 {
            return Err(DomainError::internal("the attachment is no longer pending"));
        }
        Ok(())
    }

    async fn storage_recheck(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        final_bytes: i64,
        limits: &ChatLimits,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-recheck-sum
        let rows = live_rows(&conn, tenant_id, chat_id).await?;
        let others: Vec<attachment::Model> = rows
            .into_iter()
            .filter(|row| row.id != attachment_id)
            .collect();
        let final_bytes = u64::try_from(final_bytes).unwrap_or_default();
        Ok(!limits.recheck_exceeded(sum(&others), final_bytes))
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits:p1:inst-lim-recheck-sum
    }

    async fn mark_failed(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AttachmentStatus>, DomainError> {
        let conn = self.conn()?;
        let row = attachment::Entity::find()
            .filter(attachment::Column::Id.eq(attachment_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let prior = status_of(&row.status);
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-terminal
        // `ready` and `failed` are terminal: only a `pending` or `uploaded` row can fail
        if !matches!(
            prior,
            AttachmentStatus::Pending | AttachmentStatus::Uploaded
        ) {
            return Ok(None);
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status:p1:inst-as-terminal
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("failed"))
            .col_expr(
                attachment::Column::ErrorCode,
                Expr::value(error_code.to_owned()),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::Status.eq(prior.as_str())),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok((affected > 0).then_some(prior))
    }

    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let mut statement = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
        statement = match update {
            SecondaryUpdate::Pending => {
                statement.col_expr(attachment::Column::SecondaryStatus, Expr::value("pending"))
            }
            SecondaryUpdate::Failed => {
                statement.col_expr(attachment::Column::SecondaryStatus, Expr::value("failed"))
            }
            SecondaryUpdate::Uploaded { file_id } => statement
                .col_expr(attachment::Column::SecondaryStatus, Expr::value("uploaded"))
                .col_expr(
                    attachment::Column::SecondaryFileId,
                    Expr::value(file_id.clone()),
                )
                .col_expr(
                    attachment::Column::SecondaryProviderKind,
                    Expr::value("anthropic"),
                ),
        };
        statement
            .filter(attachment::Column::Id.eq(attachment_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn mark_ready(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.conn()?;
        let (bytes, width, height) = match thumbnail {
            Some(t) => (
                Some(t.bytes.clone()),
                i32::try_from(t.width).ok(),
                i32::try_from(t.height).ok(),
            ),
            None => (None, None, None),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-storage
        // the thumbnail lives only in the `attachments` row; it is never uploaded anywhere
        // guarded by `status = uploaded`: a row the reaper or another transition already moved is
        // left alone
        attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("ready"))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .col_expr(attachment::Column::ImgThumbnail, Expr::value(bytes))
            .col_expr(attachment::Column::ImgThumbnailWidth, Expr::value(width))
            .col_expr(attachment::Column::ImgThumbnailHeight, Expr::value(height))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::Status.eq("uploaded")),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-storage
        Ok(())
    }

    async fn get(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        uploader_id: Uuid,
    ) -> Result<AttachmentRecord, DomainError> {
        let conn = self.conn()?;
        let row = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        match row {
            Some(row) if row.uploaded_by_user_id == uploader_id => Ok(record_of(row)),
            _ => Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
                id: Some(attachment_id.to_string()),
            }),
        }
    }

    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(guard_live_uploaded(attachment_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    async fn set_ready_guarded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("ready"))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(guard_live_uploaded(attachment_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    async fn fail_indexing(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        let wake = self
            .transaction(move |tx| {
                Box::pin(async move {
                    let affected = attachment::Entity::update_many()
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value("indexing_failed"),
                        )
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(guard_live_uploaded(attachment_id))
                        .secure()
                        .scope_with(&scope(tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-guard-lost
                    // the guard matched no row: roll back and enqueue nothing
                    if affected == 0 {
                        return Ok(None);
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing:p1:inst-bg-guard-lost
                    let row = attachment::Entity::find()
                        .filter(attachment::Column::Id.eq(attachment_id))
                        .secure()
                        .scope_with(&scope(tenant_id))
                        .one(tx)
                        .await?
                        .ok_or_else(|| DomainError::internal("the failed attachment vanished"))?;
                    let message = AttachmentCleanupMessage {
                        event_type: CleanupEvent::AttachmentIndexingFailed,
                        tenant_id,
                        chat_id,
                        attachment_id,
                        provider_file_id: row.provider_file_id.clone(),
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: if row.attachment_kind == "image" {
                            AttachmentKind::Image
                        } else {
                            AttachmentKind::Document
                        },
                        enqueued_at: now,
                        secondary_ref: None,
                    };
                    Ok(Some(
                        cleanup.enqueue_attachment_cleanup(tx, &message).await?,
                    ))
                })
            })
            .await?;
        match wake {
            Some(wake) => {
                wake.fire();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn delete_attachment(
        &self,
        request: &DeleteRequest,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<DeleteOutcome, DomainError> {
        let request = request.clone();
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-try
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-catch
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-db-failure
        let outcome = self
            .transaction(move |tx| {
                Box::pin(async move {
                    let scope = scope(request.tenant_id);
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-select
                    let row = attachment::Entity::find()
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(request.attachment_id))
                                .add(attachment::Column::ChatId.eq(request.chat_id)),
                        )
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-select
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-not-found
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-not-found
                    // the uploader check runs before the idempotency check
                    let Some(row) = row else {
                        return Ok((DeleteOutcome::NotFound, None));
                    };
                    if row.uploaded_by_user_id != request.uploader_id {
                        return Ok((DeleteOutcome::NotFound, None));
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-not-found
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-not-found
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-already
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-already
                    if row.deleted_at.is_some() {
                        return Ok((DeleteOutcome::AlreadyDeleted, None));
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-already
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-already
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-lock-check
                    let linked = message_attachment::Entity::find()
                        .filter(
                            Condition::all()
                                .add(
                                    message_attachment::Column::AttachmentId
                                        .eq(request.attachment_id),
                                )
                                .add(message_attachment::Column::ChatId.eq(request.chat_id)),
                        )
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-lock-check
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-locked
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-locked
                    // the message's `attachments` array is never modified; returning rolls back
                    if linked.is_some() {
                        return Ok((DeleteOutcome::Locked, None));
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-locked
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-locked
                    // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-pending
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-soft-delete
                    let now = request.now;
                    let affected = attachment::Entity::update_many()
                        .col_expr(attachment::Column::DeletedAt, Expr::value(now))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .col_expr(
                            attachment::Column::CleanupStatus,
                            Expr::expr(
                                Expr::case(
                                    Expr::col(attachment::Column::CleanupStatus).is_null(),
                                    Expr::value("pending"),
                                )
                                .finally(Expr::col(attachment::Column::CleanupStatus)),
                            ),
                        )
                        .col_expr(
                            attachment::Column::CleanupUpdatedAt,
                            Expr::expr(
                                Expr::case(
                                    Expr::col(attachment::Column::CleanupStatus).is_null(),
                                    Expr::value(now),
                                )
                                .finally(Expr::col(attachment::Column::CleanupUpdatedAt)),
                            ),
                        )
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(request.attachment_id))
                                .add(attachment::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-soft-delete
                    // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-pending
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-race
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-race
                    if affected == 0 {
                        return Ok((DeleteOutcome::AlreadyDeleted, None));
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-race
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-race
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-secondary
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-secondary-ref
                    let secondary_ref = match (
                        row.secondary_status.as_str(),
                        &row.secondary_file_id,
                        &request.secondary_upstream_alias,
                    ) {
                        ("uploaded", Some(file_id), Some(alias)) => Some(SecondaryRef {
                            file_id: file_id.clone(),
                            provider_kind: "anthropic".to_owned(),
                            upstream_alias: alias.clone(),
                        }),
                        _ => None,
                    };
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-secondary-ref
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-secondary
                    let message = AttachmentCleanupMessage {
                        event_type: CleanupEvent::AttachmentDeleted,
                        tenant_id: request.tenant_id,
                        chat_id: request.chat_id,
                        attachment_id: request.attachment_id,
                        provider_file_id: row.provider_file_id.clone(),
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: kind_of(&row.attachment_kind),
                        enqueued_at: now,
                        secondary_ref,
                    };
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-enqueue
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-too-large
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-too-large
                    // an oversized payload fails the enqueue: the rollback keeps the attachment active
                    let wake = cleanup.enqueue_attachment_cleanup(tx, &message).await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-too-large
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-too-large
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-enqueue
                    Ok((DeleteOutcome::Deleted, Some(wake)))
                })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-db-failure
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-catch
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-try
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-begin
        // the closure above runs in one database transaction, opened by `self.transaction`
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-begin
        let (outcome, wake) = outcome;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-exclusion
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-deleted
        // committed: the attachment is excluded from chat metadata, the `file_search` inclusion
        // check and the citations; no provider or vector store call is made in the request
        if let Some(wake) = wake {
            wake.fire();
        }
        Ok(outcome)
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-return-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-exclusion
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction:p1:inst-dtx-commit
    }
}

fn kind_of(text: &str) -> AttachmentKind {
    if text == "image" {
        AttachmentKind::Image
    } else {
        AttachmentKind::Document
    }
}

fn ready_file_of(row: attachment::Model) -> Option<ReadyFile> {
    Some(ReadyFile {
        attachment_id: row.id,
        provider_file_id: row.provider_file_id?,
        filename: row.filename,
    })
}

#[async_trait]
impl CleanupStore for DBProvider<DomainError> {
    async fn chat_is_live(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let row = chat::Entity::find()
            .filter(chat::Column::Id.eq(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        Ok(row.is_some_and(|row| row.deleted_at.is_none()))
    }

    async fn load_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        let conn = self.conn()?;
        let row = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::ChatId.eq(chat_id)),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?;
        Ok(row.map(|row| CleanupRow {
            cleanup_status: row.cleanup_status,
            cleanup_attempts: row.cleanup_attempts,
        }))
    }

    async fn record_failed_attempt(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError> {
        let cause = cause.to_owned();
        self.transaction(move |tx| {
            let cause = cause.clone();
            Box::pin(async move {
                let guard = || {
                    Condition::all()
                        .add(attachment::Column::Id.eq(attachment_id))
                        .add(attachment::Column::CleanupStatus.eq("pending"))
                };
                let affected = attachment::Entity::update_many()
                    .col_expr(
                        attachment::Column::CleanupAttempts,
                        Expr::col(attachment::Column::CleanupAttempts).add(1),
                    )
                    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-inspect
                    // `last_cleanup_error` holds the cause text only, never file content
                    .col_expr(attachment::Column::LastCleanupError, Expr::value(cause))
                    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-inspect
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                    .filter(guard())
                    .secure()
                    .scope_with(&scope(tenant_id))
                    .exec(tx)
                    .await?
                    .rows_affected;
                if affected == 0 {
                    return Ok(None);
                }
                let row = attachment::Entity::find()
                    .filter(attachment::Column::Id.eq(attachment_id))
                    .secure()
                    .scope_with(&scope(tenant_id))
                    .one(tx)
                    .await?
                    .ok_or_else(|| DomainError::internal("the cleanup row vanished"))?;
                let exhausted = u32::try_from(row.cleanup_attempts).unwrap_or(0) >= max_attempts;
                if exhausted {
                    attachment::Entity::update_many()
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("failed"))
                        .filter(guard())
                        .secure()
                        .scope_with(&scope(tenant_id))
                        .exec(tx)
                        .await?;
                }
                Ok(Some(AttemptRecord {
                    attempts: row.cleanup_attempts,
                    exhausted,
                }))
            })
        })
        .await
    }

    async fn mark_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        from: CleanupFrom,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let expected = match from {
            CleanupFrom::Pending => "pending",
            CleanupFrom::Failed => "failed",
        };
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(attachment::Column::CleanupStatus, Expr::value("done"))
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::CleanupStatus.eq(expected)),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    async fn record_replay_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let affected = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::LastCleanupError,
                Expr::value(cause.to_owned()),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(attachment_id))
                    .add(attachment::Column::CleanupStatus.eq("failed")),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }
}

fn stale_condition(storage: TimeStorage, cutoff: OffsetDateTime) -> Result<Condition, DomainError> {
    let bound = time_bound(storage, cutoff)
        .map_err(|e| DomainError::internal(format!("cannot bind the reaper cutoff: {e}")))?;
    Ok(Condition::all()
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::CleanupStatus.is_null())
        .add(
            time_key(
                storage,
                Expr::col((attachment::Entity, attachment::Column::UpdatedAt)),
            )
            .lt(bound),
        ))
}

#[async_trait]
impl UploadReaperStore for DBProvider<DomainError> {
    async fn select_stale(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        let storage = time_storage(self);
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-select
        // the reaper is a system worker without a user scope: it scans every tenant, and each
        // per-row write below is scoped to the row's own tenant
        let rows = attachment::Entity::find()
            .filter(stale_condition(storage, cutoff)?.add(
                attachment::Column::Status.is_in(["pending".to_owned(), "uploaded".to_owned()]),
            ))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .order_by(
                time_key(
                    storage,
                    Expr::col((attachment::Entity, attachment::Column::UpdatedAt)),
                ),
                Order::Asc,
            )
            .order_by(attachment::Column::Id, Order::Asc)
            .limit(u64::from(limit))
            .all(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-select
        Ok(rows
            .into_iter()
            .map(|row| StaleUpload {
                id: row.id,
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                status: status_of(&row.status),
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                kind: kind_of(&row.attachment_kind),
                secondary_file_id: row.secondary_file_id,
                updated_at: row.updated_at,
            })
            .collect())
    }

    async fn reap(
        &self,
        row: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        let storage = time_storage(self);
        let row = row.clone();
        let outcome = self
            .transaction(move |tx| {
                let cleanup = Arc::clone(&cleanup);
                let row = row.clone();
                Box::pin(async move {
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-cas
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-limits
                    // `deleted_at` stays NULL: the reaped row remains visible through the get flow
                    let affected = attachment::Entity::update_many()
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value("upload_abandoned"),
                        )
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            stale_condition(storage, cutoff)?
                                .add(attachment::Column::Id.eq(row.id))
                                .add(attachment::Column::Status.eq(row.status.as_str())),
                        )
                        .secure()
                        .scope_with(&scope(row.tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-limits
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-cas
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-skip
                    if affected == 0 {
                        return Ok(None);
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-skip
                    if row.provider_file_id.is_none() {
                        return Ok(Some(None));
                    }
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-enqueue
                    // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-pending
                    attachment::Entity::update_many()
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
                        .filter(attachment::Column::Id.eq(row.id))
                        .secure()
                        .scope_with(&scope(row.tenant_id))
                        .exec(tx)
                        .await?;
                    // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-pending
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-reaper-secondary
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-reaper-warn
                    if let Some(secondary) = &row.secondary_file_id {
                        tracing::warn!(
                            secondary_file_id = %secondary,
                            "an abandoned upload has a secondary copy that is not deleted"
                        );
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-reaper-warn
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-reaper-secondary
                    let message = AttachmentCleanupMessage {
                        event_type: CleanupEvent::AttachmentUploadAbandoned,
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: row.provider_file_id.clone(),
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.kind,
                        enqueued_at: now,
                        secondary_ref: None,
                    };
                    let wake = cleanup.enqueue_attachment_cleanup(tx, &message).await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-enqueue
                    Ok(Some(Some(wake)))
                })
            })
            .await?;
        match outcome {
            None => Ok(false),
            Some(wake) => {
                if let Some(wake) = wake {
                    wake.fire();
                }
                Ok(true)
            }
        }
    }
}

#[async_trait]
impl AttachmentFactsStore for DBProvider<DomainError> {
    async fn ready_document_count(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<u32, DomainError> {
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-count
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-not-kind
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-no-store
        // the predicate is `for_file_search`, never `attachment_kind`; `chat_vector_stores` is not
        // read here (the file search contribution checks the bound store)
        let count = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::ForFileSearch.eq(true)),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .count(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-no-store
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-not-kind
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-count
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-return
        u32::try_from(count).map_err(|_| DomainError::internal("the document count overflows"))
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-return
    }

    async fn ready_code_interpreter_files(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError> {
        let storage = time_storage(self);
        let conn = self.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-select
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-not-kind
        // the predicate is `for_code_interpreter` with a provider file id, never
        // `attachment_kind`; the order is chronological on `SQLite` too
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::ForCodeInterpreter.eq(true))
                    .add(attachment::Column::ProviderFileId.is_not_null()),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .order_by(
                time_key(
                    storage,
                    Expr::col((attachment::Entity, attachment::Column::CreatedAt)),
                ),
                Order::Asc,
            )
            .order_by(attachment::Column::Id, Order::Asc)
            .all(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-not-kind
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-select
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-return
        Ok(rows.into_iter().filter_map(ready_file_of).collect())
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-return
    }

    async fn bound_vector_store_id(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.conn()?;
        Ok(chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?
            .and_then(|row| row.vector_store_id))
    }

    async fn citation_files(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<ReadyFile>, DomainError> {
        let conn = self.conn()?;
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::ProviderFileId.is_not_null()),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .all(&conn)
            .await?;
        Ok(rows.into_iter().filter_map(ready_file_of).collect())
    }
}

fn row_of(model: chat_vector_store::Model) -> VectorStoreRow {
    VectorStoreRow {
        id: model.id,
        tenant_id: model.tenant_id,
        chat_id: model.chat_id,
        vector_store_id: model.vector_store_id,
        provider: model.provider,
        file_count: model.file_count,
        created_at: model.created_at,
    }
}

#[async_trait]
impl ChatVectorStoreRepo for DBProvider<DomainError> {
    async fn find(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        let conn = self.conn()?;
        Ok(chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scope(tenant_id))
            .one(&conn)
            .await?
            .map(row_of))
    }

    async fn insert_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: OffsetDateTime,
    ) -> Result<PlaceholderInsert, DomainError> {
        let conn = self.conn()?;
        let id = Uuid::new_v4();
        let row = chat_vector_store::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            vector_store_id: Set(None),
            provider: Set(provider.to_owned()),
            file_count: Set(0),
            created_at: Set(now),
        };
        match secure_insert::<chat_vector_store::Entity>(row, &scope(tenant_id), &conn).await {
            Ok(_) => Ok(PlaceholderInsert::Inserted(id)),
            Err(error) => match DomainError::from(error) {
                DomainError::UniqueViolation { .. } => Ok(PlaceholderInsert::AlreadyExists),
                other => Err(other),
            },
        }
    }

    async fn delete_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError> {
        use toolkit_db::secure::SecureDeleteExt;
        let conn = self.conn()?;
        let affected = chat_vector_store::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::ChatId.eq(chat_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    async fn set_vector_store_id(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let conn = self.conn()?;
        let affected = chat_vector_store::Entity::update_many()
            .col_expr(
                chat_vector_store::Column::VectorStoreId,
                Expr::value(vector_store_id.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::ChatId.eq(chat_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&scope(tenant_id))
            .exec(&conn)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }
}

#[cfg(test)]
#[path = "attachment_repo_tests.rs"]
mod attachment_repo_tests;

#[cfg(test)]
#[path = "vector_store_repo_tests.rs"]
mod vector_store_repo_tests;

#[cfg(test)]
#[path = "attachment_delete_repo_tests.rs"]
mod attachment_delete_repo_tests;

#[cfg(test)]
#[path = "attachment_cleanup_repo_tests.rs"]
mod attachment_cleanup_repo_tests;

#[cfg(test)]
#[path = "attachment_facts_repo_tests.rs"]
mod attachment_facts_repo_tests;
