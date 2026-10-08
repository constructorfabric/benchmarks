//! Repository helpers for turns, messages, attachments and summaries.

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::{
    attachments, chat_turns, chat_vector_stores, message_attachments, messages, thread_summaries,
};

/// Turn states.
pub mod state {
    pub const RUNNING: &str = "running";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// Finds a turn by `(chat_id, request_id)` (deleted turns included).
///
/// # Errors
/// Database errors.
pub async fn turn_by_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// `true` when a non-deleted running turn exists.
///
/// # Errors
/// Database errors.
pub async fn has_running_turn(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid) -> Result<bool, DomainError> {
    let n = chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq(state::RUNNING))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .count(runner)
        .await?;
    Ok(n > 0)
}

/// Latest non-deleted turn by `(started_at, id)`.
///
/// # Errors
/// Database errors.
pub async fn latest_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .order_by(chat_turns::Column::StartedAt, Order::Desc)
        .order_by(chat_turns::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// `input + output` tokens of the latest assistant message with usage.
///
/// # Errors
/// Database errors.
pub async fn prior_context_tokens(runner: &impl DBRunner, scope: &AccessScope, chat_id: Uuid) -> Result<i64, DomainError> {
    let m = messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::Role.eq("assistant"))
                .add(messages::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(messages::Column::InputTokens.gt(0))
                        .add(messages::Column::OutputTokens.gt(0)),
                ),
        )
        .secure()
        .scope_with(scope)
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(m.map_or(0, |m| m.input_tokens + m.output_tokens))
}

/// Non-deleted attachments of a chat with the given ids.
///
/// # Errors
/// Database errors.
pub async fn attachments_by_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<attachments::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::Id.is_in(ids.to_vec()))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Ready, non-deleted attachments of a chat.
///
/// # Errors
/// Database errors.
pub async fn ready_attachments(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Vec<attachments::Model>, DomainError> {
    Ok(attachments::Entity::find()
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::Status.eq("ready"))
                .add(attachments::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Vector store id of a chat, when created.
///
/// # Errors
/// Database errors.
pub async fn chat_vector_store(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<chat_vector_stores::Model>, DomainError> {
    Ok(chat_vector_stores::Entity::find()
        .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Thread summary of a chat.
///
/// # Errors
/// Database errors.
pub async fn thread_summary(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<thread_summaries::Model>, DomainError> {
    Ok(thread_summaries::Entity::find()
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// `(created_at, id) > frontier` condition.
#[must_use]
pub fn after(created_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(messages::Column::CreatedAt.gt(created_at)).add(
        Condition::all()
            .add(messages::Column::CreatedAt.eq(created_at))
            .add(messages::Column::Id.gt(id)),
    )
}

/// `(created_at, id) <= bound` condition.
#[must_use]
pub fn at_or_before(created_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(messages::Column::CreatedAt.lt(created_at)).add(
        Condition::all()
            .add(messages::Column::CreatedAt.eq(created_at))
            .add(messages::Column::Id.lte(id)),
    )
}

/// Latest-N recent messages for context (chronological), excluding a request id.
///
/// # Errors
/// Database errors.
pub async fn recent_messages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    frontier: Option<(OffsetDateTime, Uuid)>,
    exclude_request: Option<Uuid>,
    limit: u64,
) -> Result<Vec<messages::Model>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut cond = Condition::all()
        .add(messages::Column::ChatId.eq(chat_id))
        .add(messages::Column::RequestId.is_not_null())
        .add(messages::Column::DeletedAt.is_null())
        .add(messages::Column::IsCompressed.eq(false))
        .add(messages::Column::Role.ne("system"));
    if let Some((ts, id)) = frontier {
        cond = cond.add(after(ts, id));
    }
    if let Some(r) = exclude_request {
        cond = cond.add(messages::Column::RequestId.ne(r));
    }
    let mut rows = messages::Entity::find()
        .filter(cond)
        .secure()
        .scope_with(scope)
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(limit)
        .all(runner)
        .await?;
    rows.reverse();
    Ok(rows)
}

/// Message rows of a request (user + assistant), non-deleted.
///
/// # Errors
/// Database errors.
pub async fn messages_of_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Vec<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::RequestId.eq(request_id))
                .add(messages::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Attachment ids linked to messages.
///
/// # Errors
/// Database errors.
pub async fn links_of_messages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    message_ids: &[Uuid],
) -> Result<Vec<message_attachments::Model>, DomainError> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(message_attachments::Entity::find()
        .filter(Condition::all().add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())))
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?)
}

/// Updates turn progress timestamp and tool counters.
///
/// # Errors
/// Database errors.
pub async fn update_progress(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    ts: OffsetDateTime,
    counts: (i32, i32, i32),
) -> Result<(), DomainError> {
    chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(ts)))
        .col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(counts.0))
        .col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(counts.1))
        .col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(counts.2))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(state::RUNNING)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Map provider file id → (attachment id, filename) for citation resolution.
#[must_use]
pub fn citation_map(ready: &[attachments::Model]) -> HashMap<String, (Uuid, String)> {
    ready
        .iter()
        .filter_map(|a| a.provider_file_id.clone().map(|f| (f, (a.id, a.filename.clone()))))
        .collect()
}
