//! `chats` repository (DESIGN section 3.7 `chats`, section 3.3 chat CRUD,
//! "Cleanup on Chat Deletion").
//!
//! Every chat query applies the PEP scope (tenant + owner, optionally the
//! chat id) and, except for the insert, `deleted_at IS NULL`.

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter,
    QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::outbox::{Wake, in_transaction};
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::odata::{
    CHAT_TIEBREAKER, ChatField, ChatODataMapping, LIMITS, NativeTimestamps, TextTimestamps,
    TimestampBinding, with_default_order,
};
use super::paging::paginate_keyset;
use super::{DbRepository, attachments, child_scope};
use crate::domain::error::DomainError;
use crate::domain::ports::{ChatDeletion, ChatRecord, ChatRepository, NewChat};
use crate::infra::db::entities::{TimestampForm, chats, messages};
use crate::infra::outbox::{ChatCleanupPayload, ChatCleanupReason, EnqueueError, OutboxEnqueuer};

/// Client-safe text of an oversized chat-cleanup payload (`detail` and
/// `context.format` of the 400 response).
pub const CLEANUP_PAYLOAD_TOO_LARGE: &str =
    "The chat cleanup request exceeds the outbox message size limit";

impl From<chats::Model> for ChatRecord {
    fn from(model: chats::Model) -> Self {
        Self {
            id: model.id,
            tenant_id: model.tenant_id,
            user_id: model.user_id,
            model: model.model,
            title: model.title,
            is_temporary: model.is_temporary,
            created_at: model.created_at.at(),
            updated_at: model.updated_at.at(),
        }
    }
}

fn active_chat(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(chats::Column::Id.eq(chat_id))
        .add(chats::Column::DeletedAt.is_null())
}

/// Inserts `chat` (validated against `scope`), timestamps bound as `ts`.
///
/// # Errors
///
/// [`DomainError::Internal`] when the row is outside `scope` or the insert
/// fails; `already_exists` on a primary-key collision.
pub async fn insert(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    chat: NewChat,
) -> Result<ChatRecord, DomainError> {
    let model = chats::ActiveModel {
        id: Set(chat.id),
        tenant_id: Set(chat.tenant_id),
        user_id: Set(chat.user_id),
        model: Set(chat.model),
        title: Set(chat.title),
        is_temporary: Set(false),
        created_at: Set(ts.stamp(chat.now)),
        updated_at: Set(ts.stamp(chat.now)),
        deleted_at: Set(None),
    };
    let stored = secure_insert::<chats::Entity>(model, scope, runner).await?;
    Ok(stored.into())
}

/// The non-deleted chat `chat_id` visible through `scope`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn find_active(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<ChatRecord>, DomainError> {
    let found = chats::Entity::find()
        .filter(active_chat(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?;
    Ok(found.map(ChatRecord::from))
}

/// One page of the non-deleted chats visible through `scope`; date-time
/// literals and cursor values are bound as `ts`.
///
/// # Errors
///
/// [`DomainError::InvalidQuery`] for an invalid query.
pub async fn list_active(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    query: &ODataQuery,
) -> Result<Page<ChatRecord>, DomainError> {
    let query = with_default_order(query, ("updated_at", toolkit_odata::SortDir::Desc));
    match ts {
        TimestampForm::Native => list_page::<NativeTimestamps>(runner, scope, &query).await,
        TimestampForm::FixedText => list_page::<TextTimestamps>(runner, scope, &query).await,
    }
}

async fn list_page<T: TimestampBinding>(
    runner: &impl DBRunner,
    scope: &AccessScope,
    query: &ODataQuery,
) -> Result<Page<ChatRecord>, DomainError> {
    let select = chats::Entity::find()
        .filter(chats::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope);
    paginate_keyset::<ChatField, ChatODataMapping<T>, ChatRecord>(
        select,
        runner,
        query,
        CHAT_TIEBREAKER,
        LIMITS,
        ChatRecord::from,
    )
    .await
    .map_err(DomainError::InvalidQuery)
}

#[derive(Debug, FromQueryResult)]
struct ChatMessageCount {
    chat_id: Uuid,
    message_count: i64,
}

/// Non-deleted messages per chat of `chat_ids` (chats loaded through
/// `chat_scope`), in one grouped query.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn count_active_messages(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_ids: &[Uuid],
) -> Result<HashMap<Uuid, u64>, DomainError> {
    if chat_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.is_in(chat_ids.iter().copied()))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .project_all(runner, |select| {
            select
                .select_only()
                .column(messages::Column::ChatId)
                .column_as(
                    Expr::expr(Func::count(Expr::col((
                        messages::Entity,
                        messages::Column::Id,
                    )))),
                    "message_count",
                )
                .group_by(messages::Column::ChatId)
                .into_model::<ChatMessageCount>()
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.chat_id, u64::try_from(row.message_count).unwrap_or(0)))
        .collect())
}

/// Sets the title of the non-deleted chat `chat_id` and bumps `updated_at`
/// (bound as `ts`).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn update_title(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    chat_id: Uuid,
    title: &str,
    now: OffsetDateTime,
) -> Result<Option<ChatRecord>, DomainError> {
    let updated = chats::Entity::update_many()
        .col_expr(chats::Column::Title, Expr::value(title.to_owned()))
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts.stamp(now)))
        .filter(active_chat(chat_id))
        .secure()
        .scope_with(scope)
        .exec_with_returning(runner)
        .await?;
    Ok(updated.into_iter().next().map(ChatRecord::from))
}

/// Marks the non-deleted chat `chat_id` deleted (`deleted_at` and
/// `updated_at` = `at`, bound as `ts`); `false` when no such chat is visible.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn mark_deleted(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let result = chats::Entity::update_many()
        .col_expr(chats::Column::DeletedAt, Expr::value(Some(ts.stamp(at))))
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts.stamp(at)))
        .filter(active_chat(chat_id))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Bumps `updated_at` of the non-deleted chat `chat_id` to `at` (bound as
/// `ts`); `false` when no such chat is visible. Used by the reserve
/// transaction of a new turn.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn touch(
    runner: &impl DBRunner,
    ts: TimestampForm,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> Result<bool, DomainError> {
    let result = chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts.stamp(at)))
        .filter(active_chat(chat_id))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Locks the row of the non-deleted chat `chat_id` without changing it: a
/// no-op `UPDATE` (`updated_at = updated_at`); `false` when no such chat is
/// visible.
///
/// The row lock is the one [`touch`] takes in the reserve transaction of a
/// new turn and in the retry / edit mutation commit before they write
/// `message_attachments` links, so an attachment deletion that issues it as
/// its first statement is serialized with them per chat (on `PostgreSQL`
/// the second transaction waits for the first to commit; on `SQLite` it is
/// the write lock of the whole database). `updated_at` keeps its value: an
/// attachment deletion is not chat activity.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn lock_active(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let result = chats::Entity::update_many()
        .col_expr(
            chats::Column::UpdatedAt,
            Expr::col(chats::Column::UpdatedAt),
        )
        .filter(active_chat(chat_id))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected > 0)
}

/// The chat `chat_id` visible through `scope`, deleted or not.
async fn find_scoped(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<ChatRecord>, DomainError> {
    let found = chats::Entity::find()
        .filter(chats::Column::Id.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?;
    Ok(found.map(ChatRecord::from))
}

/// The soft-delete unit of work inside `tx`: the chat row, the attachment
/// cleanup marks and the chat-cleanup outbox message (timestamps bound as
/// `ts`). Returns `false` and an empty wake when the chat is not visible or
/// already deleted.
///
/// The conditional `UPDATE` is the first statement of the unit of work, so
/// the transaction takes the write lock before it reads anything: on
/// `SQLite` a transaction that starts with a read cannot upgrade to a write
/// once another connection (the outbox workers) has committed, and fails
/// with `SQLITE_BUSY` instead of waiting.
///
/// # Errors
///
/// [`DomainError::InvalidFormat`] when the payload exceeds the outbox size
/// limit; [`DomainError::Internal`] on any other failure.
pub async fn soft_delete_in_tx(
    tx: &(impl DBRunner + Sync),
    ts: TimestampForm,
    outbox: &OutboxEnqueuer,
    scope: &AccessScope,
    deletion: ChatDeletion,
) -> Result<(bool, Wake), DomainError> {
    if !mark_deleted(tx, ts, scope, deletion.chat_id, deletion.deleted_at).await? {
        return Ok((false, Wake::empty()));
    }
    let chat = find_scoped(tx, scope, deletion.chat_id)
        .await?
        .ok_or_else(|| DomainError::internal("soft-deleted chat is not readable"))?;
    attachments::mark_chat_for_cleanup(tx, ts, &child_scope(scope), chat.id, deletion.deleted_at)
        .await?;
    let payload = ChatCleanupPayload {
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        system_request_id: deletion.system_request_id,
        reason: ChatCleanupReason::ChatSoftDelete,
        chat_deleted_at: deletion.deleted_at,
    };
    let wake = outbox
        .enqueue_chat_cleanup(tx, &payload)
        .await
        .map_err(|err| match err {
            EnqueueError::PayloadTooLarge { .. } => DomainError::InvalidFormat {
                message: CLEANUP_PAYLOAD_TOO_LARGE.to_owned(),
            },
            other => DomainError::from(other),
        })?;
    Ok((true, wake))
}

#[async_trait]
impl ChatRepository for DbRepository {
    async fn insert(&self, scope: &AccessScope, chat: NewChat) -> Result<ChatRecord, DomainError> {
        let conn = self.db().conn()?;
        insert(&conn, self.timestamps(), scope, chat).await
    }

    async fn find_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ChatRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_active(&conn, scope, chat_id).await
    }

    async fn list_active(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatRecord>, DomainError> {
        let conn = self.db().conn()?;
        list_active(&conn, self.timestamps(), scope, query).await
    }

    async fn count_active_messages(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError> {
        let conn = self.db().conn()?;
        count_active_messages(&conn, scope, chat_ids).await
    }

    async fn update_title(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatRecord>, DomainError> {
        let conn = self.db().conn()?;
        update_title(&conn, self.timestamps(), scope, chat_id, title, now).await
    }

    async fn soft_delete(
        &self,
        scope: &AccessScope,
        deletion: ChatDeletion,
    ) -> Result<bool, DomainError> {
        let db = self.db().db();
        let ts = self.timestamps();
        let outbox = std::sync::Arc::clone(self.outbox());
        let scope = scope.clone();
        in_transaction(&db, move |tx| {
            Box::pin(async move { soft_delete_in_tx(tx, ts, &outbox, &scope, deletion).await })
        })
        .await
    }

    async fn touch_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        touch(tx, self.timestamps(), scope, chat_id, at).await
    }
}
