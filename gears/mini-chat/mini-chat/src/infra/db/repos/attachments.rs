//! The Secure ORM implementation of the [`AttachmentRepository`] port: the
//! attachment rows, the chat vector store rows and the cleanup enqueues
//! (`cpt-cf-mini-chat-feature-attachments`).
//!
//! Every multi-statement transaction starts with a write; no provider call runs
//! inside one. Closures are deterministic (ids, times and payloads are computed
//! by the caller) because a lock or serialization conflict re-runs them, at most
//! `MAX_TX_ATTEMPTS` times.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Alias, Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::contention::is_retryable_contention;
use toolkit_db::outbox::{Record, Wake};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::service::attachments::model::{
    AttachmentKind, AttachmentStatus, ChatLimits, CleanupRow, CleanupState, DeleteOutcome,
    FailOutcome, FailWithCleanup, FailureCode, NewAttachment, ReadyOutcome, SecondaryUpdate,
    SoftDelete, StaleUpload, StoreRow, StoredAttachment, Thumbnail,
};
use crate::domain::service::attachments::payload::{CleanupEvent, CleanupMessage, SecondaryRef};
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::domain::service::quota::reserve::{CONFLICT_BACKOFF, MAX_TX_ATTEMPTS};
use crate::infra::db::entities::{attachment, chat, chat_vector_store, message_attachment};
use crate::infra::db::mapping::scope_error;
use crate::infra::outbox::attachment_cleanup::encode_cleanup;
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};

type TxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>;

/// Secure ORM attachment store.
pub struct SeaAttachmentRepository {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaAttachmentRepository {
    /// Creates the store over the database provider and the shared outbox
    /// enqueuer.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }

    /// Runs `attempt` in a transaction; a lock or serialization conflict re-runs
    /// it (at most `MAX_TX_ATTEMPTS` times in total). The wake of the outbox
    /// enqueue is fired after the commit.
    async fn run_tx<T, F>(&self, attempt: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> Fn(&'a DbTx<'a>) -> TxFuture<'a, (T, Option<Wake>)> + Send + Sync + 'static,
    {
        let backend = self.db.db().backend();
        let attempt = Arc::new(attempt);
        let mut number = 0_u32;
        loop {
            number += 1;
            let run = Arc::clone(&attempt);
            let result = self
                .db
                .transaction(move |tx| Box::pin(async move { run(tx).await }))
                .await;
            match result {
                Ok((value, wake)) => {
                    if let Some(wake) = wake {
                        wake.fire();
                    }
                    return Ok(value);
                }
                Err(DomainError::Database(message))
                    if number < MAX_TX_ATTEMPTS
                        && is_retryable_contention(
                            backend,
                            &sea_orm::DbErr::Custom(message.clone()),
                        ) =>
                {
                    let pause = CONFLICT_BACKOFF
                        .get(usize::try_from(number - 1).unwrap_or(0))
                        .copied()
                        .unwrap_or_default();
                    tokio::time::sleep(pause).await;
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-return-insert-error
                Err(error) => return Err(error),
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-return-insert-error
            }
        }
    }

    /// Enqueues `message` on the cleanup queue inside `tx`.
    async fn enqueue(
        outbox: &OutboxEnqueuer,
        tx: &DbTx<'_>,
        message: &CleanupMessage,
    ) -> Result<Wake, DomainError> {
        let bytes = encode_cleanup(message)
            .map_err(|error| DomainError::Internal(format!("cleanup payload: {error}")))?;
        let running = outbox.outbox().ok_or_else(|| {
            DomainError::Internal("the outbox pipeline is not running".to_owned())
        })?;
        let queue = outbox.queue_name(QueueKind::AttachmentCleanup).to_owned();
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-partition-by-tenant
        let partition = outbox.partition(
            QueueKind::AttachmentCleanup,
            message.tenant_id,
            message.chat_id,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-partition-by-tenant
        let record = Record::to(&queue, partition)
            .payload(bytes, "application/json")
            .build()
            .map_err(|error| DomainError::Internal(format!("cleanup enqueue: {error}")))?;
        running
            .enqueue(tx, record)
            .await
            .map_err(|error| DomainError::Internal(format!("cleanup enqueue: {error}")))
    }
}

/// Live rows and bytes of one attachment kind of a chat.
#[derive(sea_orm::FromQueryResult)]
struct KindUsage {
    attachment_kind: String,
    rows: i64,
    bytes: i64,
}

fn tenant(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

impl From<attachment::Model> for StoredAttachment {
    fn from(row: attachment::Model) -> Self {
        let thumbnail = match (
            row.img_thumbnail,
            row.img_thumbnail_width,
            row.img_thumbnail_height,
        ) {
            (Some(bytes), Some(width), Some(height)) => Some(Thumbnail {
                bytes,
                width: u32::try_from(width).unwrap_or(0),
                height: u32::try_from(height).unwrap_or(0),
            }),
            _ => None,
        };
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            uploaded_by_user_id: row.uploaded_by_user_id,
            filename: row.filename,
            content_type: row.content_type,
            size_bytes: row.size_bytes,
            storage_backend: row.storage_backend,
            provider_file_id: row.provider_file_id,
            status: AttachmentStatus::parse(&row.status).unwrap_or(AttachmentStatus::Failed),
            error_code: row.error_code,
            kind: AttachmentKind::parse(&row.attachment_kind).unwrap_or(AttachmentKind::Document),
            for_file_search: row.for_file_search,
            for_code_interpreter: row.for_code_interpreter,
            thumbnail,
            cleanup_status: row.cleanup_status,
            secondary_file_id: row.secondary_file_id,
            secondary_status: row.secondary_status,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted_at: row.deleted_at,
        }
    }
}

impl From<chat_vector_store::Model> for StoreRow {
    fn from(row: chat_vector_store::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            vector_store_id: row.vector_store_id,
            provider: row.provider,
            created_at: row.created_at,
        }
    }
}

/// The guard of every CAS update: live and not owned by a cleanup.
fn live() -> Condition {
    Condition::all()
        .add(attachment::Column::DeletedAt.is_null())
        .add(attachment::Column::CleanupStatus.is_null())
}

/// Fails with `NotFound(Chat)` unless the chat is still live (tenant-scoped).
///
/// Runs inside a write transaction after its first write: a chat soft-deleted
/// between the caller's chat load and that write marked only the attachments
/// that existed then, so a row or a store placeholder written now would never be
/// cleaned up. The error rolls the transaction back.
///
/// Takes a shared row lock (FOR SHARE on PostgreSQL) to prevent concurrent
/// soft-delete transactions from committing while this transaction holds the row.
async fn ensure_chat_live(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    lock_chat: bool,
) -> Result<(), DomainError> {
    let live = chat::Entity::find()
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .project_all(runner, |select| {
            let select = if lock_chat {
                select.lock_shared()
            } else {
                select
            };
            select.into_model::<chat::Model>()
        })
        .await
        .map_err(|error| scope_error(&error))?
        .into_iter()
        .next();
    if live.is_some() {
        Ok(())
    } else {
        Err(DomainError::NotFound {
            resource: ResourceKind::Chat,
        })
    }
}

/// Reads one attachment inside or outside a transaction.
async fn read_row(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    chat_id: Option<Uuid>,
    id: Uuid,
) -> Result<Option<StoredAttachment>, DomainError> {
    let mut filter = Condition::all().add(attachment::Column::Id.eq(id));
    if let Some(chat_id) = chat_id {
        filter = filter.add(attachment::Column::ChatId.eq(chat_id));
    }
    attachment::Entity::find()
        .filter(filter)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await
        .map(|row| row.map(StoredAttachment::from))
        .map_err(|error| scope_error(&error))
}

/// Number of rows an update touched.
async fn affected(
    update: sea_orm::UpdateMany<attachment::Entity>,
    scope: &AccessScope,
    runner: &impl toolkit_db::secure::DBRunner,
) -> Result<u64, DomainError> {
    update
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await
        .map(|result| result.rows_affected)
        .map_err(|error| scope_error(&error))
}

#[async_trait]
impl AttachmentRepository for SeaAttachmentRepository {
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError> {
        let conn = self.db.conn()?;
        read_row(&conn, &scope.tenant_only(), Some(chat_id), attachment_id).await
    }

    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError> {
        let conn = self.db.conn()?;
        chat_vector_store::Entity::find()
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&tenant(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(StoreRow::from))
            .map_err(|error| scope_error(&error))
    }

    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<StoredAttachment, DomainError> {
        let new = new.clone();
        let limits = *limits;
        let lock_chat = self.db.db().backend() == sea_orm::DbBackend::Postgres;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-commit-insert
        self.run_tx(move |tx| {
            let new = new.clone();
            Box::pin(async move {
                let scope = tenant(new.tenant_id);
                let active = attachment::ActiveModel {
                    id: Set(new.id),
                    tenant_id: Set(new.tenant_id),
                    chat_id: Set(new.chat_id),
                    uploaded_by_user_id: Set(new.user_id),
                    filename: Set(new.filename.clone()),
                    content_type: Set(new.content_type.clone()),
                    size_bytes: Set(new.size_bytes),
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
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-insert-row
                let inserted = secure_insert::<attachment::Entity>(active, &scope, tx)
                    .await
                    .map_err(|error| scope_error(&error))?;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-insert-row
                // The chat may have been soft-deleted since it was loaded: its cleanup
                // marked only the rows that existed then.
                ensure_chat_live(tx, &scope, new.chat_id, lock_chat).await?;
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-check-document-limit
                let usage: Vec<KindUsage> = attachment::Entity::find()
                    .filter(
                        Condition::all()
                            .add(attachment::Column::ChatId.eq(new.chat_id))
                            .add(attachment::Column::DeletedAt.is_null())
                            .add(attachment::Column::Status.ne("failed")),
                    )
                    .secure()
                    .scope_with(&scope)
                    .project_all(tx, |select| {
                        select
                            .select_only()
                            .column(attachment::Column::AttachmentKind)
                            .column_as(Expr::col(attachment::Column::Id).count(), "rows")
                            .column_as(
                                Expr::col(attachment::Column::SizeBytes)
                                    .sum()
                                    .cast_as(Alias::new("bigint")),
                                "bytes",
                            )
                            .group_by(attachment::Column::AttachmentKind)
                            .into_model::<KindUsage>()
                    })
                    .await
                    .map_err(|error| scope_error(&error))?;
                if new.kind == AttachmentKind::Document {
                    let documents = usage
                        .iter()
                        .filter(|kind| kind.attachment_kind == "document")
                        .map(|kind| kind.rows)
                        .sum::<i64>();
                    if u64::try_from(documents).unwrap_or(u64::MAX)
                        > u64::from(limits.max_documents)
                    {
                        return Err(DomainError::DocumentLimit);
                    }
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-check-document-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-check-storage-limit
                let total: i64 = usage.iter().map(|kind| kind.bytes).sum();
                if u64::try_from(total).unwrap_or(u64::MAX) > limits.max_total_bytes {
                    return Err(DomainError::StorageLimit);
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-check-storage-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-return-pending
                Ok((StoredAttachment::from(inserted), None))
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-return-pending
            })
        })
        .await
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-insert-pending:p1:inst-commit-insert
    }

    async fn set_uploaded(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("uploaded"))
            .col_expr(
                attachment::Column::ProviderFileId,
                Expr::value(provider_file_id.to_owned()),
            )
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                live()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.eq("pending")),
            );
        Ok(affected(update, &tenant(tenant_id), &conn).await? == 1)
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-attachments-send-pipeline-data:p1
    async fn set_ready(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<ReadyOutcome, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-set-ready
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-to-ready
        let mut update = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("ready"))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
        if let Some(thumbnail) = thumbnail {
            update = update
                .col_expr(
                    attachment::Column::ImgThumbnail,
                    Expr::value(thumbnail.bytes.clone()),
                )
                .col_expr(
                    attachment::Column::ImgThumbnailWidth,
                    Expr::value(i32::try_from(thumbnail.width).unwrap_or(i32::MAX)),
                )
                .col_expr(
                    attachment::Column::ImgThumbnailHeight,
                    Expr::value(i32::try_from(thumbnail.height).unwrap_or(i32::MAX)),
                );
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-to-ready
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-set-ready
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-ready-invariant
        let update = update.filter(
            live()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::Status.eq("uploaded"))
                .add(attachment::Column::ProviderFileId.is_not_null()),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-mark-ready:p1:inst-ready-invariant
        let scope = tenant(tenant_id);
        if affected(update, &scope, &conn).await? == 1 {
            return Ok(ReadyOutcome::Ready);
        }
        let chat_id = read_row(&conn, &scope, None, id)
            .await?
            .map(|row| row.chat_id);
        let chat_deleted = match chat_id {
            Some(chat_id) => self
                .chat_deleted(tenant_id, chat_id)
                .await?
                .unwrap_or(false),
            None => false,
        };
        Ok(ReadyOutcome::Taken { chat_deleted })
    }

    async fn set_failed(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        code: FailureCode,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-pending-to-failed
        let update = attachment::Entity::update_many()
            .col_expr(attachment::Column::Status, Expr::value("failed"))
            .col_expr(attachment::Column::ErrorCode, Expr::value(code.as_str()))
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachment::Column::DeletedAt.is_null()),
            );
        // @cpt-end:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-pending-to-failed
        Ok(affected(update, &tenant(tenant_id), &conn).await? == 1)
    }

    async fn fail_with_cleanup(
        &self,
        request: &FailWithCleanup,
    ) -> Result<FailOutcome, DomainError> {
        let request = request.clone();
        let outbox = Arc::clone(&self.outbox);
        self.run_tx(move |tx| {
            let request = request.clone();
            let outbox = Arc::clone(&outbox);
            Box::pin(async move {
                // @cpt-begin:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-uploaded-to-failed
                let update = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value("failed"))
                    .col_expr(
                        attachment::Column::ErrorCode,
                        Expr::value(request.code.as_str()),
                    )
                    .col_expr(
                        attachment::Column::ProviderFileId,
                        Expr::value(request.provider_file_id.clone()),
                    )
                    .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                    .col_expr(
                        attachment::Column::CleanupUpdatedAt,
                        Expr::value(request.now),
                    )
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(request.now))
                    .filter(
                        live()
                            .add(attachment::Column::Id.eq(request.attachment_id))
                            .add(attachment::Column::Status.is_in(["pending", "uploaded"])),
                    );
                // @cpt-end:cpt-cf-mini-chat-state-attachments-attachment-status:p1:inst-uploaded-to-failed
                let scope = tenant(request.tenant_id);
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-enqueue-failure-cleanup
                if affected(update, &scope, tx).await? == 1 {
                    let wake = Self::enqueue(&outbox, tx, &request.message).await?;
                    return Ok((FailOutcome::Failed, Some(wake)));
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-enqueue-failure-cleanup
                match &request.orphan {
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-enqueue-failure-orphan
                    Some(orphan) => {
                        let wake = Self::enqueue(&outbox, tx, orphan).await?;
                        Ok((FailOutcome::OrphanEnqueued, Some(wake)))
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-enqueue-failure-orphan
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-failure-row-taken
                    None => Ok((FailOutcome::RowTaken, None)),
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-fail-upload:p1:inst-failure-row-taken
                }
            })
        })
        .await
    }

    async fn enqueue_orphan_file(&self, message: &CleanupMessage) -> Result<(), DomainError> {
        let message = message.clone();
        let outbox = Arc::clone(&self.outbox);
        self.run_tx(move |tx| {
            let message = message.clone();
            let outbox = Arc::clone(&outbox);
            Box::pin(async move {
                let wake = Self::enqueue(&outbox, tx, &message).await?;
                Ok(((), Some(wake)))
            })
        })
        .await
    }

    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let mut statement = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
        statement = match update {
            SecondaryUpdate::Pending => statement
                .col_expr(attachment::Column::SecondaryStatus, Expr::value("pending"))
                .col_expr(
                    attachment::Column::SecondaryProviderKind,
                    Expr::value("anthropic"),
                ),
            SecondaryUpdate::Uploaded(file_id) => statement
                .col_expr(
                    attachment::Column::SecondaryFileId,
                    Expr::value(file_id.clone()),
                )
                .col_expr(attachment::Column::SecondaryStatus, Expr::value("uploaded")),
            SecondaryUpdate::Failed => {
                statement.col_expr(attachment::Column::SecondaryStatus, Expr::value("failed"))
            }
        };
        let statement = statement.filter(Condition::all().add(attachment::Column::Id.eq(id)));
        affected(statement, &tenant(tenant_id), &conn)
            .await
            .map(|_| ())
    }

    async fn insert_store_placeholder(&self, row: &StoreRow) -> Result<(), DomainError> {
        let row = row.clone();
        let lock_chat = self.db.db().backend() == sea_orm::DbBackend::Postgres;
        // Write first, then the chat check: a placeholder of a soft-deleted chat
        // is rolled back, so no vector store is created for it.
        self.run_tx(move |tx| {
            let row = row.clone();
            Box::pin(async move {
                let scope = tenant(row.tenant_id);
                let active = chat_vector_store::ActiveModel {
                    id: Set(row.id),
                    tenant_id: Set(row.tenant_id),
                    chat_id: Set(row.chat_id),
                    vector_store_id: Set(None),
                    provider: Set(row.provider.clone()),
                    file_count: Set(0),
                    created_at: Set(row.created_at),
                };
                secure_insert::<chat_vector_store::Entity>(active, &scope, tx)
                    .await
                    .map_err(|error| scope_error(&error))?;
                ensure_chat_live(tx, &scope, row.chat_id, lock_chat).await?;
                Ok(((), None))
            })
        })
        .await
    }

    async fn bind_store(
        &self,
        tenant_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let result = chat_vector_store::Entity::update_many()
            .col_expr(
                chat_vector_store::Column::VectorStoreId,
                Expr::value(vector_store_id.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&tenant(tenant_id))
            .exec(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(result.rows_affected == 1)
    }

    async fn delete_placeholder(&self, tenant_id: Uuid, row_id: Uuid) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let result = chat_vector_store::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(row_id))
                    .add(chat_vector_store::Column::VectorStoreId.is_null()),
            )
            .secure()
            .scope_with(&tenant(tenant_id))
            .exec(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(result.rows_affected == 1)
    }

    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
            .filter(
                live()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::Status.eq("uploaded")),
            );
        Ok(affected(update, &tenant(tenant_id), &conn).await? == 1)
    }

    async fn soft_delete(
        &self,
        scope: &AccessScope,
        request: &SoftDelete,
    ) -> Result<DeleteOutcome, DomainError> {
        let scope = scope.tenant_only();
        let request = request.clone();
        let outbox = Arc::clone(&self.outbox);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-commit-delete
        let result = self
            .run_tx(move |tx| {
                let scope = scope.clone();
                let request = request.clone();
                let outbox = Arc::clone(&outbox);
                Box::pin(async move {
                    let target = Condition::all()
                        .add(attachment::Column::Id.eq(request.attachment_id))
                        .add(attachment::Column::ChatId.eq(request.chat_id));
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-delete-with-cleanup
                    // @cpt-begin:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-pending
                    let with_cleanup = attachment::Entity::update_many()
                        .col_expr(attachment::Column::DeletedAt, Expr::value(request.now))
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(request.now))
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(
                            attachment::Column::CleanupUpdatedAt,
                            Expr::value(request.now),
                        )
                        .filter(target.clone().add(live()));
                    let enqueue = affected(with_cleanup, &scope, tx).await? == 1;
                    // @cpt-end:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-pending
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-delete-with-cleanup
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-delete-owned-cleanup
                    if !enqueue {
                        let owned = attachment::Entity::update_many()
                            .col_expr(attachment::Column::DeletedAt, Expr::value(request.now))
                            .col_expr(attachment::Column::UpdatedAt, Expr::value(request.now))
                            .filter(
                                target
                                    .add(attachment::Column::DeletedAt.is_null())
                                    .add(attachment::Column::CleanupStatus.is_not_null()),
                            );
                        if affected(owned, &scope, tx).await? == 0 {
                            return match read_row(
                                tx,
                                &scope,
                                Some(request.chat_id),
                                request.attachment_id,
                            )
                            .await?
                            {
                                Some(_) => Ok((DeleteOutcome::AlreadyDeleted, None)),
                                None => Err(DomainError::NotFound {
                                    resource: ResourceKind::Attachment,
                                }),
                            };
                        }
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-delete-owned-cleanup
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-check-lock
                    let linked = message_attachment::Entity::find()
                        .filter(
                            Condition::all()
                                .add(
                                    message_attachment::Column::AttachmentId
                                        .eq(request.attachment_id),
                                )
                                .add(message_attachment::Column::ChatId.eq(request.chat_id)),
                        )
                        .limit(1)
                        .secure()
                        .scope_with(&scope)
                        .all(tx)
                        .await
                        .map_err(|error| scope_error(&error))?;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-check-lock
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-locked
                    if !linked.is_empty() {
                        // Rolls the soft delete back; mapped to `Locked` below.
                        return Err(DomainError::AttachmentLocked);
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-locked
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-enqueue-delete
                    let wake = if enqueue {
                        let row =
                            read_row(tx, &scope, Some(request.chat_id), request.attachment_id)
                                .await?
                                .ok_or(DomainError::NotFound {
                                    resource: ResourceKind::Attachment,
                                })?;
                        let secondary_ref = request
                            .secondary_alias
                            .clone()
                            .zip(row.secondary_file_id.clone())
                            .filter(|_| row.secondary_status == "uploaded")
                            .map(|(upstream_alias, file_id)| SecondaryRef {
                                file_id,
                                upstream_alias,
                            });
                        let message = CleanupMessage::for_row(
                            CleanupEvent::AttachmentDeleted,
                            &row,
                            row.provider_file_id.clone(),
                            secondary_ref,
                            request.now,
                        );
                        Some(Self::enqueue(&outbox, tx, &message).await?)
                    } else {
                        None
                    };
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-enqueue-delete
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-deleted
                    Ok((DeleteOutcome::Deleted, wake))
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-deleted
                })
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-commit-delete
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-delete-error
        match result {
            Err(DomainError::AttachmentLocked) => Ok(DeleteOutcome::Locked),
            other => other,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-soft-delete:p1:inst-return-delete-error
    }

    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-select-stale
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .filter(
                live()
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by(attachment::Column::UpdatedAt, Order::Asc)
            .limit(u64::from(limit))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(rows
            .into_iter()
            .map(StoredAttachment::from)
            .map(|row| StaleUpload {
                id: row.id,
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                status: row.status,
                provider_file_id: row.provider_file_id,
                secondary_file_id: row.secondary_file_id,
                storage_backend: row.storage_backend,
                kind: row.kind,
                updated_at: row.updated_at,
            })
            .collect())
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-select-stale
    }

    async fn abandon(
        &self,
        stale: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let stale = stale.clone();
        let outbox = Arc::clone(&self.outbox);
        self.run_tx(move |tx| {
            let stale = stale.clone();
            let outbox = Arc::clone(&outbox);
            Box::pin(async move {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-cas-abandon
                let mut update = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value("failed"))
                    .col_expr(
                        attachment::Column::ErrorCode,
                        Expr::value(FailureCode::UploadAbandoned.as_str()),
                    )
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                if stale.provider_file_id.is_some() {
                    update = update
                        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
                        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now));
                }
                let update = update.filter(
                    live()
                        .add(attachment::Column::Id.eq(stale.id))
                        .add(attachment::Column::Status.eq(stale.status.as_str()))
                        .add(attachment::Column::UpdatedAt.lt(cutoff)),
                );
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-cas-abandon
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-if-abandon-lost
                if affected(update, &tenant(stale.tenant_id), tx).await? == 0 {
                    return Ok((false, None));
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-if-abandon-lost
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-enqueue-abandoned
                let wake = if let Some(file) = &stale.provider_file_id {
                    let message = CleanupMessage {
                        event: CleanupEvent::AttachmentUploadAbandoned,
                        tenant_id: stale.tenant_id,
                        chat_id: stale.chat_id,
                        attachment_id: stale.id,
                        provider_file_id: Some(file.clone()),
                        storage_backend: stale.storage_backend.clone(),
                        kind: stale.kind,
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Some(Self::enqueue(&outbox, tx, &message).await?)
                } else {
                    None
                };
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-enqueue-abandoned
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-commit-abandon
                Ok((true, wake))
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-commit-abandon
            })
        })
        .await
    }

    async fn chat_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        let conn = self.db.conn()?;
        chat::Entity::find()
            .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
            .secure()
            .scope_with(&tenant(tenant_id))
            .one(&conn)
            .await
            .map(|row| row.map(|chat| chat.deleted_at.is_some()))
            .map_err(|error| scope_error(&error))
    }

    async fn cleanup_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        let conn = self.db.conn()?;
        let row = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::ChatId.eq(chat_id)),
            )
            .secure()
            .scope_with(&tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(row.map(|row| CleanupRow {
            state: match row.cleanup_status.as_deref() {
                Some("pending") => Some(CleanupState::Pending),
                Some("done") => Some(CleanupState::Done),
                Some("failed") => Some(CleanupState::Failed),
                _ => None,
            },
            attempts: row.cleanup_attempts,
        }))
    }

    async fn record_cleanup_attempt(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError> {
        let conn = self.db.conn()?;
        let scope = tenant(tenant_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-record-attempt
        let update = attachment::Entity::update_many()
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
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            );
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-record-attempt
        if affected(update, &scope, &conn).await? == 0 {
            return Ok(None);
        }
        let attempts = attachment::Entity::find()
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?
            .map(|row| row.cleanup_attempts);
        Ok(attempts)
    }

    async fn finish_cleanup(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        state: CleanupState,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let update = attachment::Entity::update_many()
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(state.as_str()),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.eq(id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            );
        Ok(affected(update, &tenant(tenant_id), &conn).await? > 0)
    }
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "attachments_cas_tests.rs"]
mod cas_tests;

#[cfg(test)]
#[path = "attachments_store_tests.rs"]
mod store_tests;

#[cfg(test)]
#[path = "attachments_delete_tests.rs"]
mod delete_tests;

#[cfg(test)]
#[path = "attachments_cleanup_tests.rs"]
mod cleanup_tests;

#[cfg(test)]
#[path = "attachments_ensure_chat_live_tests.rs"]
mod ensure_chat_live_tests;
