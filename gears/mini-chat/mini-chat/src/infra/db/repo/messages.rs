//! `messages` repository (DESIGN section 3.7 `messages`, section 3.3 "List
//! Messages").
//!
//! Message queries run after the parent chat was loaded through the chat
//! scope: they use the tenant predicates of that scope, the parent `chat_id`
//! and `deleted_at IS NULL`.
//!
//! Streaming reads and writes (DESIGN "`ContextPlan` Determinism and Snapshot
//! Boundary", "Recent messages query", section 3.6 "Thread Summary"):
//!
//! - the snapshot boundary (latest message key), the `prior_context_tokens`
//!   of the latest assistant message with usage (optionally outside one
//!   turn, for the retry / edit preflight), and the recent-messages
//!   query (`request_id IS NOT NULL`, live, `is_compressed = false`, `<=
//!   boundary`, `> frontier`, newest `K` by `(created_at, id)`, returned
//!   chronologically);
//! - the chat's thread summary (`thread_summaries`, read-only here) and the
//!   frozen summary target (latest live message outside the causing turn);
//! - the user message insert of the reserve transaction (a violation of
//!   `uq_messages_chat_request_role` is `request_id_conflict`) and the
//!   assistant message insert of the finalization transaction.
//!
//! Turn mutations (DESIGN section 3.9, "Summary Interaction on Turn
//! Mutation"): the live user message of a turn, the soft delete of a turn's
//! messages and the removal of a thread summary (the `thread_summaries` row
//! and every `is_compressed` flag of the chat), inside the mutation
//! transaction.
//!
//! Order keys compare `(created_at, id)` lexicographically; timestamps are
//! bound through the database's [`TimestampForm`].

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata_try;
use toolkit_db::secure::{
    DBRunner, DbTx, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::odata::{
    LIMITS, MESSAGE_TIEBREAKER, MessageField, MessageODataMapping, NativeTimestamps,
    TextTimestamps, TimestampBinding, check_timestamp_cursor, paginate_error, with_default_order,
    with_uuid_literals,
};
use super::{DbRepository, child_scope};
use crate::domain::error::{AbortReason, DomainError};
use crate::domain::ports::{
    ContextMessageRecord, MessageKey, MessageRecord, MessageRepository, MessageRole,
    NewAssistantMessage, NewUserMessage, ThreadSummaryRecord,
};
use crate::infra::db::entities::{TimestampForm, messages, thread_summaries};
use crate::infra::db::error::{UniqueTarget, unique_violation_target};

/// `messages.content_type` of every P1 message.
const CONTENT_TYPE_TEXT: &str = "text";
/// `messages.request_kind` of every P1 message.
const REQUEST_KIND_CHAT: &str = "chat";

/// The domain record of a stored message.
///
/// # Errors
///
/// [`DomainError::Internal`] when the stored role is outside the value set.
pub fn to_record(model: messages::Model) -> Result<MessageRecord, DomainError> {
    let role = MessageRole::parse(&model.role).ok_or_else(|| {
        DomainError::internal(format!(
            "message {} has an unknown stored role `{}`",
            model.id, model.role
        ))
    })?;
    Ok(MessageRecord {
        id: model.id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        role,
        content: model.content,
        model: model.model,
        input_tokens: model.input_tokens,
        output_tokens: model.output_tokens,
        created_at: model.created_at.at(),
    })
}

pub(super) fn active_in_chat(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(messages::Column::ChatId.eq(chat_id))
        .add(messages::Column::DeletedAt.is_null())
}

/// One page of the non-deleted messages of `chat_id`; date-time literals
/// and cursor values are bound as `ts` (a `created_at` cursor key is checked
/// to be a date-time in that form before the toolkit paginator decodes it).
///
/// # Errors
///
/// [`DomainError::InvalidQuery`] for an invalid query or cursor;
/// [`DomainError::Internal`] for a stored row outside the value sets.
pub async fn list_active(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<MessageRecord>, DomainError> {
    let query = with_uuid_literals(
        &with_default_order(query, ("created_at", toolkit_odata::SortDir::Asc)),
        &["id"],
    );
    check_timestamp_cursor(&query, ts, MessageField::CreatedAt)
        .map_err(DomainError::InvalidQuery)?;
    match ts {
        TimestampForm::Native => {
            list_page::<NativeTimestamps>(runner, chat_scope, chat_id, &query).await
        }
        TimestampForm::FixedText => {
            list_page::<TextTimestamps>(runner, chat_scope, chat_id, &query).await
        }
    }
}

async fn list_page<T: TimestampBinding>(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<MessageRecord>, DomainError> {
    let select = messages::Entity::find()
        .filter(active_in_chat(chat_id))
        .secure()
        .scope_with(&child_scope(chat_scope));
    paginate_odata_try::<
        MessageField,
        MessageODataMapping<T>,
        messages::Entity,
        _,
        _,
        DomainError,
        _,
    >(
        select,
        runner,
        query,
        MESSAGE_TIEBREAKER,
        LIMITS,
        to_record,
    )
    .await
    .map_err(paginate_error)
}

/// The non-deleted message `message_id` of `chat_id`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn find_active(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Option<MessageRecord>, DomainError> {
    messages::Entity::find()
        .filter(active_in_chat(chat_id).add(messages::Column::Id.eq(message_id)))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
        .map(to_record)
        .transpose()
}

// ── Streaming ────────────────────────────────────────────────────────────

fn key_of(model: &messages::Model) -> MessageKey {
    MessageKey {
        created_at: model.created_at.at(),
        id: model.id,
    }
}

/// `(created_at, id) <= key`.
pub(super) fn at_or_before(ts: TimestampForm, key: MessageKey) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.lt(ts.stamp(key.created_at)))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(ts.stamp(key.created_at)))
                .add(messages::Column::Id.lte(key.id)),
        )
}

/// `(created_at, id) > key`.
pub(super) fn after(ts: TimestampForm, key: MessageKey) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.gt(ts.stamp(key.created_at)))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(ts.stamp(key.created_at)))
                .add(messages::Column::Id.gt(key.id)),
        )
}

/// Order key of the latest non-deleted message of `chat_id` with a
/// `request_id`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn latest_key(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<MessageKey>, DomainError> {
    let found = messages::Entity::find()
        .filter(active_in_chat(chat_id).add(messages::Column::RequestId.is_not_null()))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(found.as_ref().map(key_of))
}

/// Order key of the latest non-deleted message of `chat_id` whose
/// `request_id` is not `request_id` (system messages without a request id
/// included).
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn latest_key_excluding_request(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<MessageKey>, DomainError> {
    let found = messages::Entity::find()
        .filter(
            active_in_chat(chat_id).add(
                Condition::any()
                    .add(messages::Column::RequestId.is_null())
                    .add(messages::Column::RequestId.ne(request_id)),
            ),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(found.as_ref().map(key_of))
}

/// `input_tokens + output_tokens` of the latest non-deleted assistant
/// message of `chat_id` with non-zero usage (0 when none); with
/// `exclude_request`, the messages of that turn are skipped.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn prior_context_tokens(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    exclude_request: Option<Uuid>,
) -> Result<u64, DomainError> {
    let mut condition = active_in_chat(chat_id)
        .add(messages::Column::Role.eq(MessageRole::Assistant.as_str()))
        .add(
            Condition::any()
                .add(messages::Column::InputTokens.gt(0_i64))
                .add(messages::Column::OutputTokens.gt(0_i64)),
        );
    if let Some(request_id) = exclude_request {
        condition = condition.add(
            Condition::any()
                .add(messages::Column::RequestId.is_null())
                .add(messages::Column::RequestId.ne(request_id)),
        );
    }
    let found = messages::Entity::find()
        .filter(condition)
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(found.map_or(0, |message| {
        let total = message
            .input_tokens
            .max(0)
            .saturating_add(message.output_tokens.max(0));
        u64::try_from(total).unwrap_or(0)
    }))
}

/// The recent-messages query (module docs), chronological.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored role outside the
/// value set.
pub async fn recent_for_context(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    boundary: MessageKey,
    frontier: Option<MessageKey>,
    limit: u32,
) -> Result<Vec<ContextMessageRecord>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut condition = active_in_chat(chat_id)
        .add(messages::Column::RequestId.is_not_null())
        .add(messages::Column::IsCompressed.eq(false))
        .add(at_or_before(ts, boundary));
    if let Some(frontier) = frontier {
        condition = condition.add(after(ts, frontier));
    }
    let mut rows = messages::Entity::find()
        .filter(condition)
        .secure()
        .scope_with(&child_scope(chat_scope))
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(u64::from(limit))
        .all(runner)
        .await?;
    rows.reverse();
    rows.into_iter()
        .map(|model| {
            let role = MessageRole::parse(&model.role).ok_or_else(|| {
                DomainError::internal(format!(
                    "message {} has an unknown stored role `{}`",
                    model.id, model.role
                ))
            })?;
            Ok(ContextMessageRecord {
                id: model.id,
                role,
                content: model.content,
                created_at: model.created_at.at(),
            })
        })
        .collect()
}

/// The committed thread summary of `chat_id`, if any.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn thread_summary(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<ThreadSummaryRecord>, DomainError> {
    let found = thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?;
    Ok(found.map(|model| ThreadSummaryRecord {
        text: model.summary_text,
        frontier: MessageKey {
            created_at: model.summarized_up_to_created_at.at(),
            id: model.summarized_up_to_message_id,
        },
        token_estimate: model.token_estimate,
    }))
}

/// Identity of a message row to insert.
#[derive(Clone, Copy)]
struct RowHead {
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    role: MessageRole,
    created_at: OffsetDateTime,
}

fn new_message_row(ts: TimestampForm, head: RowHead, content: String) -> messages::ActiveModel {
    messages::ActiveModel {
        id: Set(head.id),
        tenant_id: Set(head.tenant_id),
        chat_id: Set(head.chat_id),
        request_id: Set(Some(head.request_id)),
        role: Set(head.role.as_str().to_owned()),
        content: Set(content),
        content_type: Set(CONTENT_TYPE_TEXT.to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set(REQUEST_KIND_CHAT.to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(ts.stamp(head.created_at)),
        deleted_at: Set(None),
    }
}

/// Inserts the user message of a new turn (timestamps bound as `ts`).
///
/// # Errors
///
/// `409 aborted` `request_id_conflict` on `uq_messages_chat_request_role`;
/// [`DomainError::Internal`] on any other failure.
pub async fn insert_user(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    message: NewUserMessage,
) -> Result<(), DomainError> {
    let row = new_message_row(
        ts,
        RowHead {
            id: message.id,
            tenant_id: message.tenant_id,
            chat_id: message.chat_id,
            request_id: message.request_id,
            role: MessageRole::User,
            created_at: message.created_at,
        },
        message.content,
    );
    match secure_insert::<messages::Entity>(row, &child_scope(chat_scope), runner).await {
        Ok(_) => Ok(()),
        Err(err) => Err(match unique_violation_target(&err) {
            Some(UniqueTarget::MessageRequestRole) => {
                DomainError::aborted(AbortReason::RequestIdConflict)
            }
            _ => DomainError::from(err),
        }),
    }
}

fn non_negative(value: i64) -> i64 {
    value.max(0)
}

/// Inserts the assistant message of a finalized turn (timestamps bound as
/// `ts`).
///
/// # Errors
///
/// [`DomainError`] when the insert fails (any constraint or query failure).
pub async fn insert_assistant(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    message: NewAssistantMessage,
) -> Result<(), DomainError> {
    let mut row = new_message_row(
        ts,
        RowHead {
            id: message.id,
            tenant_id: message.tenant_id,
            chat_id: message.chat_id,
            request_id: message.request_id,
            role: MessageRole::Assistant,
            created_at: message.created_at,
        },
        message.content,
    );
    row.model = Set(Some(message.model));
    row.provider_response_id = Set(message.provider_response_id);
    row.input_tokens = Set(non_negative(message.usage.input_tokens));
    row.output_tokens = Set(non_negative(message.usage.output_tokens));
    row.cache_read_input_tokens = Set(non_negative(message.usage.cache_read_input_tokens));
    row.cache_write_input_tokens = Set(non_negative(message.usage.cache_write_input_tokens));
    row.reasoning_tokens = Set(non_negative(message.usage.reasoning_tokens));
    secure_insert::<messages::Entity>(row, &child_scope(chat_scope), runner).await?;
    Ok(())
}

// ── Turn mutations ───────────────────────────────────────────────────────

/// The non-deleted user message of the turn `request_id` of `chat_id`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored row outside the
/// value sets.
pub async fn find_user_of_request(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<MessageRecord>, DomainError> {
    messages::Entity::find()
        .filter(
            active_in_chat(chat_id)
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::Role.eq(MessageRole::User.as_str())),
        )
        .secure()
        .scope_with(&child_scope(chat_scope))
        .one(runner)
        .await?
        .map(to_record)
        .transpose()
}

/// Soft-deletes the live messages of the turn `request_id` of `chat_id`
/// (`deleted_at = at`, bound as `ts`); returns the number of rows.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn soft_delete_request(
    runner: &impl DBRunner,
    ts: TimestampForm,
    chat_scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    at: OffsetDateTime,
) -> Result<u64, DomainError> {
    let result = messages::Entity::update_many()
        .col_expr(messages::Column::DeletedAt, Expr::value(Some(ts.stamp(at))))
        .filter(active_in_chat(chat_id).add(messages::Column::RequestId.eq(request_id)))
        .secure()
        .scope_with(&child_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(result.rows_affected)
}

/// Deletes the thread summary of `chat_id` and clears `is_compressed` on
/// every message of the chat; `false` when no summary existed.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn drop_thread_summary(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let scope = child_scope(chat_scope);
    let deleted = thread_summaries::Entity::delete_many()
        .filter(thread_summaries::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .exec(runner)
        .await?;
    messages::Entity::update_many()
        .col_expr(messages::Column::IsCompressed, Expr::value(false))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::IsCompressed.eq(true)),
        )
        .secure()
        .scope_with(&scope)
        .exec(runner)
        .await?;
    Ok(deleted.rows_affected > 0)
}

#[async_trait]
impl MessageRepository for DbRepository {
    async fn list_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRecord>, DomainError> {
        let conn = self.db().conn()?;
        list_active(&conn, self.timestamps(), scope, chat_id, query).await
    }

    async fn find_active(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_active(&conn, scope, chat_id, message_id).await
    }

    async fn latest_key(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        let conn = self.db().conn()?;
        latest_key(&conn, scope, chat_id).await
    }

    async fn prior_context_tokens(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude_request: Option<Uuid>,
    ) -> Result<u64, DomainError> {
        let conn = self.db().conn()?;
        prior_context_tokens(&conn, scope, chat_id, exclude_request).await
    }

    async fn recent_for_context(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        boundary: MessageKey,
        frontier: Option<MessageKey>,
        limit: u32,
    ) -> Result<Vec<ContextMessageRecord>, DomainError> {
        let conn = self.db().conn()?;
        recent_for_context(
            &conn,
            self.timestamps(),
            scope,
            chat_id,
            boundary,
            frontier,
            limit,
        )
        .await
    }

    async fn thread_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError> {
        let conn = self.db().conn()?;
        thread_summary(&conn, scope, chat_id).await
    }

    async fn summary_frontier_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        Ok(thread_summary(tx, scope, chat_id)
            .await?
            .map(|summary| summary.frontier))
    }

    async fn latest_key_excluding_request_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        latest_key_excluding_request(tx, scope, chat_id, request_id).await
    }

    async fn insert_user_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        message: NewUserMessage,
    ) -> Result<(), DomainError> {
        insert_user(tx, self.timestamps(), scope, message).await
    }

    async fn insert_assistant_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        message: NewAssistantMessage,
    ) -> Result<(), DomainError> {
        insert_assistant(tx, self.timestamps(), scope, message).await
    }

    async fn find_user_of_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRecord>, DomainError> {
        let conn = self.db().conn()?;
        find_user_of_request(&conn, scope, chat_id, request_id).await
    }

    async fn latest_key_excluding_request(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageKey>, DomainError> {
        let conn = self.db().conn()?;
        latest_key_excluding_request(&conn, scope, chat_id, request_id).await
    }

    async fn soft_delete_request_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        soft_delete_request(tx, self.timestamps(), scope, chat_id, request_id, at).await
    }

    async fn drop_thread_summary_in_tx(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<bool, DomainError> {
        drop_thread_summary(tx, scope, chat_id).await
    }
}
