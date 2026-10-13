//! `SeaORM` implementation of the attachment repositories over Secure ORM: the upload, get and
//! delete paths, the upload reaper and the cleanup handlers.
//!
//! Tenant isolation: attachment queries of a user request carry the tenant of the caller's scope
//! (the owner check happened on the parent chat); background work carries the tenant of its work
//! item. Only the reaper's candidate discovery is unscoped and advisory: every row it takes is
//! re-checked and written under the scope of its own tenant.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::entity::{attachments, chat_vector_stores, chats, message_attachments};
use super::map_scope_err;
use super::mutation_repo::lock_chat;
use crate::domain::attachment::{
    AttachmentKind, AttachmentRepository, CLEANUP_DONE, CLEANUP_FAILED, CLEANUP_PENDING,
    ChatLimits, CleanupFailure, CleanupRepository, DeleteOutcome, ERROR_INDEXING_FAILED,
    ERROR_UPLOAD_ABANDONED, NewAttachment, PROVIDER_KIND_ANTHROPIC, PendingCleanup, ReadyUpdate,
    SECONDARY_FAILED, SECONDARY_NOT_ATTEMPTED, SECONDARY_UPLOADED, STATUS_FAILED, STATUS_PENDING,
    STATUS_READY, STATUS_UPLOADED, SecondaryCopy, StaleUpload, StoredAttachment,
    UploadReaperRepository, VectorStoreRow,
};
use crate::domain::cleanup::AttachmentCleanupPayload;
use crate::domain::error::DomainError;
use crate::domain::message::Thumbnail;
use crate::infra::outbox::{EnqueuedMessage, OutboxEnqueuer};

/// Longest `last_cleanup_error` that is stored, in characters.
const MAX_CLEANUP_ERROR_CHARS: usize = 512;

/// The columns the per-chat limits are computed from.
#[derive(Debug, FromQueryResult)]
struct LimitRow {
    size_bytes: Option<i64>,
    attachment_kind: String,
}

/// Attachment repositories over the gear's database.
pub struct AttachmentRepositoryImpl {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl AttachmentRepositoryImpl {
    /// Creates the repository.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }
}

fn to_stored(row: attachments::Model) -> StoredAttachment {
    let thumbnail = match (
        row.img_thumbnail,
        row.img_thumbnail_width,
        row.img_thumbnail_height,
    ) {
        (Some(data), Some(width), Some(height)) => Some(Thumbnail {
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
            data,
        }),
        _ => None,
    };
    StoredAttachment {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        uploaded_by: row.uploaded_by_user_id,
        filename: row.filename,
        content_type: row.content_type,
        size_bytes: row.size_bytes,
        storage_backend: row.storage_backend,
        provider_file_id: row.provider_file_id,
        status: row.status,
        error_code: row.error_code,
        kind: AttachmentKind::parse(&row.attachment_kind).unwrap_or(AttachmentKind::Document),
        for_file_search: row.for_file_search,
        for_code_interpreter: row.for_code_interpreter,
        thumbnail,
        cleanup_status: row.cleanup_status,
        secondary_file_id: row.secondary_file_id,
        created_at: row.created_at,
        deleted_at: row.deleted_at,
    }
}

fn to_store_row(row: chat_vector_stores::Model) -> VectorStoreRow {
    VectorStoreRow {
        id: row.id,
        vector_store_id: row.vector_store_id,
        provider: row.provider,
        created_at: row.created_at,
    }
}

/// A row that has no cleanup owner other than a pending one: the cleanup outcomes apply to it.
fn cleanup_open() -> Condition {
    Condition::any()
        .add(attachments::Column::CleanupStatus.is_null())
        .add(attachments::Column::CleanupStatus.eq(CLEANUP_PENDING))
}

/// The per-chat limit check of the insert transaction.
fn check_limits(
    rows: &[LimitRow],
    new_size: i64,
    limits: ChatLimits,
    kind: AttachmentKind,
) -> Result<(), DomainError> {
    let documents = rows
        .iter()
        .filter(|row| row.attachment_kind == AttachmentKind::Document.as_str())
        .count();
    if kind == AttachmentKind::Document
        && u64::try_from(documents).unwrap_or(u64::MAX) >= limits.max_documents
    {
        return Err(DomainError::DocumentLimit);
    }
    let total: u64 = rows
        .iter()
        .map(|row| u64::try_from(row.size_bytes.unwrap_or(0)).unwrap_or(0))
        .sum();
    let incoming = u64::try_from(new_size).unwrap_or(0);
    if total.saturating_add(incoming) > limits.max_total_bytes {
        return Err(DomainError::StorageLimit);
    }
    Ok(())
}

fn insert_model(new: &NewAttachment) -> attachments::ActiveModel {
    attachments::ActiveModel {
        id: ActiveValue::Set(new.id),
        tenant_id: ActiveValue::Set(new.tenant_id),
        chat_id: ActiveValue::Set(new.chat_id),
        uploaded_by_user_id: ActiveValue::Set(new.uploaded_by),
        filename: ActiveValue::Set(new.filename.clone()),
        content_type: ActiveValue::Set(Some(new.content_type.clone())),
        size_bytes: ActiveValue::Set(Some(new.size_bytes)),
        storage_backend: ActiveValue::Set(new.storage_backend.clone()),
        provider_file_id: ActiveValue::Set(None),
        status: ActiveValue::Set(STATUS_PENDING.to_owned()),
        error_code: ActiveValue::Set(None),
        attachment_kind: ActiveValue::Set(new.kind.as_str().to_owned()),
        for_file_search: ActiveValue::Set(new.purposes.file_search),
        for_code_interpreter: ActiveValue::Set(new.purposes.code_interpreter),
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
        created_at: ActiveValue::Set(new.now),
        updated_at: ActiveValue::Set(new.now),
        deleted_at: ActiveValue::Set(None),
        secondary_file_id: ActiveValue::Set(None),
        secondary_status: ActiveValue::Set(SECONDARY_NOT_ATTEMPTED.to_owned()),
        secondary_provider_kind: ActiveValue::Set(None),
    }
}

#[async_trait]
impl AttachmentRepository for AttachmentRepositoryImpl {
    async fn insert_pending(
        &self,
        scope: &AccessScope,
        new: NewAttachment,
        limits: ChatLimits,
    ) -> Result<(), DomainError> {
        let chat_scope = scope.clone();
        let scope = scope.tenant_only();
        in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                // The chat row lock comes first: the chat is re-checked live after the body was
                // read (a soft-deleted chat is 404, so no row is created that chat cleanup would
                // never see), and uploads of one chat are serialized, so the count-then-insert of
                // the limits cannot be raced by a concurrent upload.
                lock_chat(tx, &chat_scope, new.chat_id, None).await?;
                let rows: Vec<LimitRow> = attachments::Entity::find()
                    .filter(attachments::Column::ChatId.eq(new.chat_id))
                    .filter(attachments::Column::DeletedAt.is_null())
                    .filter(attachments::Column::Status.ne(STATUS_FAILED))
                    .secure()
                    .scope_with(&scope)
                    .project_all(tx, |query| {
                        query
                            .select_only()
                            .column(attachments::Column::SizeBytes)
                            .column(attachments::Column::AttachmentKind)
                            .into_model::<LimitRow>()
                    })
                    .await
                    .map_err(map_scope_err)?;
                check_limits(&rows, new.size_bytes, limits, new.kind)?;
                secure_insert::<attachments::Entity>(insert_model(&new), &scope, tx)
                    .await
                    .map_err(map_scope_err)?;
                Ok::<_, DomainError>(((), Wake::empty()))
            })
        })
        .await
    }

    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError> {
        let conn = self.db.conn()?;
        Ok(attachments::Entity::find()
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(to_stored))
    }

    async fn mark_uploaded(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(STATUS_UPLOADED))
            .col_expr(
                attachments::Column::ProviderFileId,
                Expr::value(provider_file_id),
            )
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(attachments::Column::Status.eq(STATUS_PENDING))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(updated == 1)
    }

    async fn mark_ready(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        update: &ReadyUpdate,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let mut statement = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(STATUS_READY))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now));
        if let Some(thumbnail) = &update.thumbnail {
            statement = statement
                .col_expr(
                    attachments::Column::ImgThumbnail,
                    Expr::value(thumbnail.data.clone()),
                )
                .col_expr(
                    attachments::Column::ImgThumbnailWidth,
                    Expr::value(i32::try_from(thumbnail.width).unwrap_or(i32::MAX)),
                )
                .col_expr(
                    attachments::Column::ImgThumbnailHeight,
                    Expr::value(i32::try_from(thumbnail.height).unwrap_or(i32::MAX)),
                );
        }
        statement = match &update.secondary {
            SecondaryCopy::NotAttempted => statement,
            SecondaryCopy::Uploaded { file_id } => statement
                .col_expr(
                    attachments::Column::SecondaryFileId,
                    Expr::value(file_id.as_str()),
                )
                .col_expr(
                    attachments::Column::SecondaryStatus,
                    Expr::value(SECONDARY_UPLOADED),
                )
                .col_expr(
                    attachments::Column::SecondaryProviderKind,
                    Expr::value(PROVIDER_KIND_ANTHROPIC),
                ),
            SecondaryCopy::Failed => statement
                .col_expr(
                    attachments::Column::SecondaryStatus,
                    Expr::value(SECONDARY_FAILED),
                )
                .col_expr(
                    attachments::Column::SecondaryProviderKind,
                    Expr::value(PROVIDER_KIND_ANTHROPIC),
                ),
        };
        let updated = statement
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(attachments::Column::Status.eq(STATUS_UPLOADED))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(updated == 1)
    }

    async fn mark_failed(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, Expr::value(STATUS_FAILED))
            .col_expr(attachments::Column::ErrorCode, Expr::value(error_code))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(attachments::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED]))
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(updated == 1)
    }

    async fn touch_uploaded(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let updated = attachments::Entity::update_many()
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(attachments::Column::Status.eq(STATUS_UPLOADED))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(updated == 1)
    }

    async fn fail_indexing(
        &self,
        scope: &AccessScope,
        payload: &AttachmentCleanupPayload,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let scope = scope.tenant_only();
        let payload = payload.clone();
        let result = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let updated = attachments::Entity::update_many()
                    .col_expr(attachments::Column::Status, Expr::value(STATUS_FAILED))
                    .col_expr(
                        attachments::Column::ErrorCode,
                        Expr::value(ERROR_INDEXING_FAILED),
                    )
                    .col_expr(
                        attachments::Column::CleanupStatus,
                        Expr::value(CLEANUP_PENDING),
                    )
                    .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
                    .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
                    .filter(attachments::Column::Id.eq(payload.attachment_id))
                    .filter(attachments::Column::Status.eq(STATUS_UPLOADED))
                    .filter(attachments::Column::DeletedAt.is_null())
                    .filter(attachments::Column::CleanupStatus.is_null())
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if updated == 0 {
                    return Ok::<_, DomainError>(((false, None), Wake::empty()));
                }
                // Enqueue last: an error after the enqueue would drop the wake on rollback.
                let (wake, message) = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
                Ok(((true, Some(message)), wake))
            })
        })
        .await?;
        self.announce(result.1);
        Ok(result.0)
    }

    async fn delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        payload: &AttachmentCleanupPayload,
        now: OffsetDateTime,
    ) -> Result<DeleteOutcome, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let chat_scope = scope.clone();
        let scope = scope.tenant_only();
        let payload = payload.clone();
        let result = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let attachment_id = payload.attachment_id;
                // The chat row lock comes before the reference count: a send or a mutation links
                // attachments under the same lock, so a submitted message can never reference an
                // attachment that is soft-deleted here.
                lock_chat(tx, &chat_scope, chat_id, None).await?;
                let row = attachments::Entity::find()
                    .filter(attachments::Column::Id.eq(attachment_id))
                    .filter(attachments::Column::ChatId.eq(chat_id))
                    .secure()
                    .scope_with(&scope)
                    .one(tx)
                    .await
                    .map_err(map_scope_err)?;
                if row.is_none_or(|row| row.deleted_at.is_some()) {
                    return Ok::<_, DomainError>((
                        (DeleteOutcome::AlreadyDeleted, None),
                        Wake::empty(),
                    ));
                }
                let references = message_attachments::Entity::find()
                    .filter(message_attachments::Column::ChatId.eq(chat_id))
                    .filter(message_attachments::Column::AttachmentId.eq(attachment_id))
                    .secure()
                    .scope_with(&scope)
                    .count(tx)
                    .await
                    .map_err(map_scope_err)?;
                if references > 0 {
                    return Ok(((DeleteOutcome::Locked, None), Wake::empty()));
                }
                let deleted = attachments::Entity::update_many()
                    .col_expr(attachments::Column::DeletedAt, Expr::value(now))
                    .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
                    .filter(attachments::Column::Id.eq(attachment_id))
                    .filter(attachments::Column::DeletedAt.is_null())
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if deleted == 0 {
                    return Ok(((DeleteOutcome::AlreadyDeleted, None), Wake::empty()));
                }
                // Only a row without a cleanup owner gets the event: abandoned, failed-indexing and
                // chat-deleted rows already have theirs.
                let owned = attachments::Entity::update_many()
                    .col_expr(
                        attachments::Column::CleanupStatus,
                        Expr::value(CLEANUP_PENDING),
                    )
                    .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
                    .filter(attachments::Column::Id.eq(attachment_id))
                    .filter(attachments::Column::CleanupStatus.is_null())
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if owned == 0 {
                    return Ok(((DeleteOutcome::Deleted, None), Wake::empty()));
                }
                let (wake, message) = outbox.enqueue_attachment_cleanup(tx, &payload).await?;
                Ok(((DeleteOutcome::Deleted, Some(message)), wake))
            })
        })
        .await?;
        self.announce(result.1);
        Ok(result.0)
    }

    async fn find_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        let conn = self.db.conn()?;
        Ok(chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(to_store_row))
    }

    async fn insert_store_placeholder(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Uuid>, DomainError> {
        let chat_scope = scope.clone();
        let scope = scope.tenant_only();
        let id = Uuid::new_v4();
        let row = chat_vector_stores::ActiveModel {
            id: ActiveValue::Set(id),
            tenant_id: ActiveValue::Set(tenant_id),
            chat_id: ActiveValue::Set(chat_id),
            vector_store_id: ActiveValue::Set(None),
            provider: ActiveValue::Set(provider.to_owned()),
            file_count: ActiveValue::Set(0),
            created_at: ActiveValue::Set(now),
        };
        // The chat row lock makes the insert atomic with the liveness check: the placeholder (and
        // the provider store created after it) exists only for a live chat, so chat cleanup, which
        // runs after the soft delete, always sees it. A lost unique race rolls the transaction
        // back (it cannot be committed on PostgreSQL after the failed statement).
        let inserted = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                lock_chat(tx, &chat_scope, chat_id, None).await?;
                secure_insert::<chat_vector_stores::Entity>(row, &scope, tx)
                    .await
                    .map_err(map_scope_err)?;
                Ok::<_, DomainError>(((), Wake::empty()))
            })
        })
        .await;
        match inserted {
            Ok(()) => Ok(Some(id)),
            Err(DomainError::UniqueViolation) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn set_store_id(
        &self,
        scope: &AccessScope,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let updated = chat_vector_stores::Entity::update_many()
            .col_expr(
                chat_vector_stores::Column::VectorStoreId,
                Expr::value(vector_store_id),
            )
            .filter(chat_vector_stores::Column::Id.eq(row_id))
            .filter(chat_vector_stores::Column::VectorStoreId.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(updated == 1)
    }

    async fn delete_placeholder(
        &self,
        scope: &AccessScope,
        row_id: Uuid,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let deleted = chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::Id.eq(row_id))
            .filter(chat_vector_stores::Column::VectorStoreId.is_null())
            .secure()
            .scope_with(&scope.tenant_only())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected;
        Ok(deleted == 1)
    }
}

impl AttachmentRepositoryImpl {
    fn announce(&self, message: Option<EnqueuedMessage>) {
        if let Some(message) = message {
            self.outbox.announce(&[message]);
        }
    }
}

#[async_trait]
impl UploadReaperRepository for AttachmentRepositoryImpl {
    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        let conn = self.db.conn()?;
        // The scan is a system task without a tenant: candidate discovery is advisory only, every
        // row is re-checked by its own tenant's compare-and-set.
        let rows = attachments::Entity::find()
            .filter(attachments::Column::Status.is_in([STATUS_PENDING, STATUS_UPLOADED]))
            .filter(attachments::Column::DeletedAt.is_null())
            .filter(attachments::Column::CleanupStatus.is_null())
            .filter(attachments::Column::UpdatedAt.lt(cutoff))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .order_by(attachments::Column::UpdatedAt, sea_orm::Order::Asc)
            .order_by(attachments::Column::Id, sea_orm::Order::Asc)
            .limit(limit)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(rows
            .into_iter()
            .map(|row| StaleUpload {
                id: row.id,
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                status: row.status,
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                attachment_kind: row.attachment_kind,
                secondary_file_id: row.secondary_file_id,
            })
            .collect())
    }

    async fn abandon(
        &self,
        upload: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        cleanup: Option<&AttachmentCleanupPayload>,
    ) -> Result<bool, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let upload = upload.clone();
        let cleanup = cleanup.cloned();
        let result = in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let scope = AccessScope::for_tenant(upload.tenant_id);
                let mut statement = attachments::Entity::update_many()
                    .col_expr(attachments::Column::Status, Expr::value(STATUS_FAILED))
                    .col_expr(
                        attachments::Column::ErrorCode,
                        Expr::value(ERROR_UPLOAD_ABANDONED),
                    )
                    .col_expr(attachments::Column::UpdatedAt, Expr::value(now));
                if cleanup.is_some() {
                    statement = statement
                        .col_expr(
                            attachments::Column::CleanupStatus,
                            Expr::value(CLEANUP_PENDING),
                        )
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now));
                }
                let updated = statement
                    .filter(attachments::Column::Id.eq(upload.id))
                    .filter(attachments::Column::Status.eq(upload.status.as_str()))
                    .filter(attachments::Column::DeletedAt.is_null())
                    .filter(attachments::Column::CleanupStatus.is_null())
                    .filter(attachments::Column::UpdatedAt.lt(cutoff))
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?
                    .rows_affected;
                if updated == 0 {
                    return Ok::<_, DomainError>(((false, None), Wake::empty()));
                }
                match &cleanup {
                    Some(payload) => {
                        // Enqueue last: an error after the enqueue would drop the wake on rollback.
                        let (wake, message) =
                            outbox.enqueue_attachment_cleanup(tx, payload).await?;
                        Ok(((true, Some(message)), wake))
                    }
                    None => Ok(((true, None), Wake::empty())),
                }
            })
        })
        .await?;
        self.announce(result.1);
        Ok(result.0)
    }
}

#[async_trait]
impl CleanupRepository for AttachmentRepositoryImpl {
    async fn chat_is_deleted(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let deleted = chats::Entity::find()
            .filter(chats::Column::Id.eq(chat_id))
            .filter(chats::Column::DeletedAt.is_not_null())
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .count(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(deleted > 0)
    }

    async fn cleanup_status(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.db.conn()?;
        Ok(attachments::Entity::find()
            .filter(attachments::Column::Id.eq(attachment_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .and_then(|row| row.cleanup_status))
    }

    async fn claim_unowned_cleanups(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        Ok(attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(CLEANUP_PENDING),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
            .col_expr(attachments::Column::UpdatedAt, Expr::value(now))
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::CleanupStatus.is_null())
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .map_err(map_scope_err)?
            .rows_affected)
    }

    async fn chat_model(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.db.conn()?;
        Ok(chats::Entity::find()
            .filter(chats::Column::Id.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(|chat| chat.model))
    }

    async fn mark_cleanup_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        attachments::Entity::update_many()
            .col_expr(
                attachments::Column::CleanupStatus,
                Expr::value(CLEANUP_DONE),
            )
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(attachments::Column::Id.eq(attachment_id))
            .filter(cleanup_open())
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(())
    }

    async fn record_cleanup_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        error: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<CleanupFailure, DomainError> {
        let error: String = error.chars().take(MAX_CLEANUP_ERROR_CHARS).collect();
        in_transaction(&self.db.db(), move |tx| {
            Box::pin(async move {
                let scope = AccessScope::for_tenant(tenant_id);
                let row = attachments::Entity::find()
                    .filter(attachments::Column::Id.eq(attachment_id))
                    .secure()
                    .scope_with(&scope)
                    .one(tx)
                    .await
                    .map_err(map_scope_err)?;
                let Some(row) = row else {
                    // A message about an attachment that does not exist can never succeed.
                    return Ok::<_, DomainError>((
                        CleanupFailure {
                            attempts: max_attempts,
                            terminal: true,
                        },
                        Wake::empty(),
                    ));
                };
                let attempts = u32::try_from(row.cleanup_attempts)
                    .unwrap_or(0)
                    .saturating_add(1);
                let terminal = attempts >= max_attempts;
                let status = if terminal {
                    CLEANUP_FAILED
                } else {
                    CLEANUP_PENDING
                };
                attachments::Entity::update_many()
                    .col_expr(
                        attachments::Column::CleanupAttempts,
                        Expr::value(i32::try_from(attempts).unwrap_or(i32::MAX)),
                    )
                    .col_expr(attachments::Column::LastCleanupError, Expr::value(error))
                    .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now))
                    .col_expr(attachments::Column::CleanupStatus, Expr::value(status))
                    .filter(attachments::Column::Id.eq(attachment_id))
                    .filter(cleanup_open())
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await
                    .map_err(map_scope_err)?;
                Ok((CleanupFailure { attempts, terminal }, Wake::empty()))
            })
        })
        .await
    }

    async fn pending_cleanups(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingCleanup>, DomainError> {
        let conn = self.db.conn()?;
        Ok(attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::CleanupStatus.eq(CLEANUP_PENDING))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .order_by(attachments::Column::CreatedAt, sea_orm::Order::Asc)
            .order_by(attachments::Column::Id, sea_orm::Order::Asc)
            .all(&conn)
            .await
            .map_err(map_scope_err)?
            .into_iter()
            .map(|row| PendingCleanup {
                id: row.id,
                provider_file_id: row.provider_file_id,
                storage_backend: row.storage_backend,
                secondary_file_id: row.secondary_file_id,
            })
            .collect())
    }

    async fn failed_cleanups(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        attachments::Entity::find()
            .filter(attachments::Column::ChatId.eq(chat_id))
            .filter(attachments::Column::CleanupStatus.eq(CLEANUP_FAILED))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .count(&conn)
            .await
            .map_err(map_scope_err)
    }

    async fn chat_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        let conn = self.db.conn()?;
        Ok(chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(map_scope_err)?
            .map(to_store_row))
    }

    async fn delete_store_row(&self, tenant_id: Uuid, row_id: Uuid) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::Id.eq(row_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(())
    }
}
