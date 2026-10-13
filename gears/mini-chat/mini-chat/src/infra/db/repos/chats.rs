//! Chat persistence of the chat CRUD feature: the Secure ORM implementation of
//! the [`ChatRepository`] port, including the transactional soft delete with
//! the chat-cleanup enqueue.
// @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-temporary-deferred:p1

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::QueryFilter;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, FromQueryResult, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Record;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::clock::now_micros;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::ChatRepository;
use crate::infra::db::entities::{attachment, chat, message};
use crate::infra::db::mapping::scope_error;
use crate::infra::db::odata::{ChatFilterField, ChatMapper, paginate_scoped};
use crate::infra::db::repos::chat_access;
use crate::infra::outbox::chat_cleanup::{ChatCleanupPayload, map_enqueue_error};
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};

#[derive(FromQueryResult)]
struct CountRow {
    chat_id: Uuid,
    cnt: i64,
}

/// Secure ORM chat store.
pub struct SeaChatRepository {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaChatRepository {
    /// Creates the store over the database provider and the shared outbox
    /// enqueuer.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, outbox: Arc<OutboxEnqueuer>) -> Self {
        Self { db, outbox }
    }
}

/// Sets `deleted_at` and `updated_at` of a live chat; returns the affected rows.
async fn mark_deleted(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> Result<u64, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-mark-chat-deleted
    let result = chat::Entity::update_many()
        .col_expr(chat::Column::DeletedAt, Expr::value(at))
        .col_expr(chat::Column::UpdatedAt, Expr::value(at))
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    Ok(result.rows_affected)
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-mark-chat-deleted
}

/// Reads the chat row this transaction has just soft-deleted (the scoped
/// access still applies, the `deleted_at` filter does not).
async fn load_deleted_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Chat, DomainError> {
    chat::Entity::find()
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await
        .map_err(|error| scope_error(&error))?
        .map(Chat::from)
        .ok_or(DomainError::NotFound {
            resource: ResourceKind::Chat,
        })
}

/// Marks the chat's attachments without a cleanup status as `pending`.
async fn mark_attachments_pending(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> Result<u64, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-mark-attachments-pending
    let result = attachment::Entity::update_many()
        .col_expr(attachment::Column::CleanupStatus, Expr::value("pending"))
        .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(at))
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::CleanupStatus.is_null()),
        )
        .secure()
        .scope_with(&scope.tenant_only())
        .exec(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    Ok(result.rows_affected)
    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-mark-attachments-pending
}

#[async_trait]
impl ChatRepository for SeaChatRepository {
    async fn insert(&self, scope: &AccessScope, row: &Chat) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-insert-chat
        // `chats.model` and `chats.is_temporary` are written by this insert
        // only (`cpt-cf-mini-chat-constraint-model-locked-per-chat`); every
        // chat starts non-temporary and not deleted.
        let active = chat::ActiveModel {
            id: Set(row.id),
            tenant_id: Set(row.tenant_id),
            user_id: Set(row.user_id),
            model: Set(row.model.clone()),
            title: Set(row.title.clone()),
            is_temporary: Set(false),
            created_at: Set(row.created_at),
            updated_at: Set(row.updated_at),
            deleted_at: Set(None),
        };
        let inserted = secure_insert::<chat::Entity>(active, scope, &conn).await;
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-insert-chat
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-insert-out-of-scope
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-insert-denied
        inserted.map(|_| ()).map_err(|error| match error {
            ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => DomainError::AuthzDenied,
            other => scope_error(&other),
        })
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-insert-denied
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-if-insert-out-of-scope
    }

    async fn load(&self, scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-deleted-terminal
        // A soft-deleted chat is invisible to the scoped access (`deleted_at
        // IS NULL`), so every operation on it is 404 and nothing in this
        // gear restores or purges it.
        chat_access::load_chat(&conn, scope, chat_id).await
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-deleted-terminal
    }

    async fn message_counts(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError> {
        if chat_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-count-messages
        let rows: Vec<CountRow> = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.is_in(chat_ids.iter().copied()))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .project_all(&conn, |select| {
                select
                    .select_only()
                    .column(message::Column::ChatId)
                    .column_as(Expr::col(message::Column::Id).count(), "cnt")
                    .group_by(message::Column::ChatId)
                    .into_model::<CountRow>()
            })
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-project-chat-detail:p1:inst-count-messages
        Ok(rows
            .into_iter()
            .map(|row| (row.chat_id, u64::try_from(row.cnt).unwrap_or(0)))
            .collect())
    }

    async fn rename(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-update-title
        // Only `title` and `updated_at` are written.
        let result = chat::Entity::update_many()
            .col_expr(chat::Column::Title, Expr::value(title.to_owned()))
            .col_expr(chat::Column::UpdatedAt, Expr::value(at))
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(chat_id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(scope)
            .exec(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(result.rows_affected)
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-update-title
    }

    async fn page(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<Chat>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-page-chats
        let select = chat::Entity::find()
            .filter(Condition::all().add(chat::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(scope);
        // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-activity-active
        // The activity bump itself is written by the sending, retry and edit
        // features of the gear; this feature only consumes it: the default
        // order is the most recent `updated_at` first.
        let order = ("updated_at", SortDir::Desc);
        // @cpt-end:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-activity-active
        paginate_scoped::<ChatFilterField, ChatMapper, _, _>(
            select,
            &conn,
            query,
            order,
            Chat::from,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-page-chats
    }

    async fn soft_delete_with_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        let outbox = Arc::clone(&self.outbox);
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-begin-delete-tx
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-commit-delete
        // The transaction commits when the closure returns `Ok` and rolls back
        // on every error, so a 204 always means the cleanup message is stored.
        //
        // Write-first rule for every transaction of this gear: the first
        // statement is the guarded write, never a read. On `SQLite` (WAL) a
        // transaction that reads and then writes fails at once with
        // "database is locked" (code 5) when another writer committed in
        // between, and `busy_timeout` does not apply to that lock upgrade; a
        // transaction that starts with its write waits for the write lock
        // instead. Reads the transaction needs follow the first write.
        let wake = self
            .db
            .transaction(|tx| {
                Box::pin(async move {
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-take-delete-time
                    let deleted_at = now_micros();
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-take-delete-time

                    // The guarded update is the visibility check as well: a
                    // chat that is missing, soft-deleted, of another tenant or
                    // of another user matches no row, which is 404 in every
                    // case, and exactly one of several concurrent deletes of
                    // the same chat matches.
                    // @cpt-begin:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-to-soft-deleted
                    let affected = mark_deleted(tx, &scope, chat_id, deleted_at).await?;
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-if-delete-lost
                    if affected == 0 {
                        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-delete-lost
                        return Err(DomainError::NotFound {
                            resource: ResourceKind::Chat,
                        });
                        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-delete-lost
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-if-delete-lost

                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-load-chat-delete
                    // The row is ours now (the write lock is held): read it
                    // back for the cleanup payload.
                    let chat = load_deleted_chat(tx, &scope, chat_id).await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-load-chat-delete

                    mark_attachments_pending(tx, &scope, chat.id, deleted_at).await?;
                    // @cpt-end:cpt-cf-mini-chat-state-chat-crud-chat-lifecycle:p1:inst-to-soft-deleted

                    let payload = ChatCleanupPayload::new(chat.tenant_id, chat.id, deleted_at);
                    let bytes = payload
                        .encode()
                        .map_err(|error| DomainError::Internal(error.to_string()))?;

                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-try-enqueue
                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-enqueue-cleanup
                    let running = outbox.outbox().ok_or_else(|| {
                        DomainError::Internal("the outbox pipeline is not running".to_owned())
                    })?;
                    let queue = outbox.queue_name(QueueKind::ChatCleanup).to_owned();
                    let partition =
                        outbox.partition(QueueKind::ChatCleanup, chat.tenant_id, chat.id);
                    let record = Record::to(&queue, partition)
                        .payload(bytes, "application/json")
                        .build()
                        .map_err(|error| map_enqueue_error(&error))?;
                    let wake = running
                        .enqueue(tx, record)
                        .await
                        .map_err(|error| map_enqueue_error(&error))?;
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-enqueue-cleanup
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-try-enqueue

                    // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-delete-boundaries
                    // Nothing else is written: no `messages`, `chat_turns`,
                    // `message_attachments`, `message_reactions`,
                    // `thread_summaries`, `chat_vector_stores` or
                    // `attachments.deleted_at`; no running turn is cancelled,
                    // no provider is called and no audit event is enqueued.
                    Ok(wake)
                    // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-delete-boundaries
                })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-commit-delete
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-begin-delete-tx
        wake.fire();
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-deleted
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-deleted
    }
}
