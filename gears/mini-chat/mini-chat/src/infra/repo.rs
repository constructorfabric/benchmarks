//! Small query helpers over the Secure ORM (every call is scoped).

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::infra::db::entity::{attachment, chat, chat_turn, message, thread_summary};

/// Current UTC time truncated to microseconds (the precision every backend
/// keeps).
#[must_use]
pub fn now_utc() -> OffsetDateTime {
    let n = OffsetDateTime::now_utc();
    let micros = n.microsecond();
    n.replace_microsecond(micros).unwrap_or(n)
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn find_chat(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat::Model>> {
    Ok(chat::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .one(r)
        .await?)
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn count_messages(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<u64> {
    Ok(message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .count(r)
        .await?)
}

/// Turn by `(chat_id, request_id)`, soft-deleted turns included.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn find_turn_by_request(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::RequestId.eq(request_id)),
        )
        .one(r)
        .await?)
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn find_turn_by_id(
    r: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
        .one(r)
        .await?)
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn find_running_turn(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::State.eq("running"))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .one(r)
        .await?)
}

/// Latest non-deleted turn by `(started_at, id)`.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn latest_turn(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<chat_turn::Model>> {
    Ok(chat_turn::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat_turn::Column::ChatId.eq(chat_id))
                .add(chat_turn::Column::DeletedAt.is_null()),
        )
        .order_by(chat_turn::Column::StartedAt, Order::Desc)
        .order_by(chat_turn::Column::Id, Order::Desc)
        .limit(1)
        .one(r)
        .await?)
}

/// Latest non-deleted message of a chat by `(created_at, id)`.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn latest_message(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<message::Model>> {
    Ok(message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null()),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(r)
        .await?)
}

/// `created_at` for a new message strictly after the chat's latest message
/// (keeps `(created_at, id)` order equal to insertion order).
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn next_message_time(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<OffsetDateTime> {
    let now = now_utc();
    let latest = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)))
        .order_by(message::Column::CreatedAt, Order::Desc)
        .limit(1)
        .one(r)
        .await?;
    Ok(match latest {
        Some(m) if m.created_at >= now => m.created_at + time::Duration::microseconds(1),
        _ => now,
    })
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn find_summary(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<thread_summary::Model>> {
    Ok(thread_summary::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .one(r)
        .await?)
}

/// Ready, non-deleted attachments of a chat.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn ready_attachments(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Vec<attachment::Model>> {
    Ok(attachment::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Status.eq("ready"))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .all(r)
        .await?)
}

/// Bump `chats.updated_at`.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn touch_chat(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    at: OffsetDateTime,
) -> DomainResult<()> {
    use toolkit_db::secure::SecureUpdateExt;
    chat::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(chat::Column::UpdatedAt, Expr::value(at))
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .exec(r)
        .await?;
    Ok(())
}
