//! `messages` repository (tenant-scoped; chat ownership is checked by the
//! caller through an owner-scoped chat lookup).

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::message;

/// Message order key `(created_at, id)`.
pub type OrderKey = (OffsetDateTime, Uuid);

#[allow(clippy::too_many_arguments)]
pub fn new_model(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    role: &str,
    content: String,
    created_at: OffsetDateTime,
) -> message::Model {
    message::Model {
        id,
        tenant_id,
        chat_id,
        request_id: Some(request_id),
        role: role.to_owned(),
        content,
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::Value::Array(vec![]),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: None,
        is_compressed: false,
        created_at,
        deleted_at: None,
    }
}

pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    m: &message::Model,
) -> Result<(), DomainError> {
    let am = message::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(m.request_id),
        role: Set(m.role.clone()),
        content: Set(m.content.clone()),
        content_type: Set(m.content_type.clone()),
        token_estimate: Set(m.token_estimate),
        provider_response_id: Set(m.provider_response_id.clone()),
        request_kind: Set(m.request_kind.clone()),
        features_used: Set(m.features_used.clone()),
        input_tokens: Set(m.input_tokens),
        output_tokens: Set(m.output_tokens),
        cache_read_input_tokens: Set(m.cache_read_input_tokens),
        cache_write_input_tokens: Set(m.cache_write_input_tokens),
        reasoning_tokens: Set(m.reasoning_tokens),
        model: Set(m.model.clone()),
        is_compressed: Set(m.is_compressed),
        created_at: Set(m.created_at),
        deleted_at: Set(m.deleted_at),
    };
    message::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

fn live_in_chat(chat_id: Uuid) -> Condition {
    Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
}

/// Non-deleted message of a chat.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(live_in_chat(chat_id).add(message::Column::Id.eq(id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Message by id regardless of deletion (replay of the stored answer).
pub async fn find_by_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::Id.eq(id)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// Non-deleted message of a turn by role.
pub async fn find_by_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    role: &str,
) -> Result<Option<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(
            live_in_chat(chat_id)
                .add(message::Column::RequestId.eq(request_id))
                .add(message::Column::Role.eq(role)),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

/// `input_tokens + output_tokens` of the most recent non-deleted assistant
/// message with non-zero usage (`prior_context_tokens`).
pub async fn prior_context_tokens(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let row = message::Entity::find()
        .filter(
            live_in_chat(chat_id)
                .add(message::Column::Role.eq("assistant"))
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.gt(0))
                        .add(message::Column::OutputTokens.gt(0)),
                ),
        )
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

/// Latest non-deleted message `(created_at, id)` (snapshot boundary).
pub async fn latest_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<OrderKey>, DomainError> {
    let row = message::Entity::find()
        .filter(live_in_chat(chat_id))
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map(|m| (m.created_at, m.id)))
}

fn key_le(key: OrderKey) -> Expr {
    Expr::tuple([
        Expr::col(message::Column::CreatedAt),
        Expr::col(message::Column::Id),
    ])
    .lte(Expr::tuple([Expr::value(key.0), Expr::value(key.1)]))
}

fn key_gt(key: OrderKey) -> Expr {
    Expr::tuple([
        Expr::col(message::Column::CreatedAt),
        Expr::col(message::Column::Id),
    ])
    .gt(Expr::tuple([Expr::value(key.0), Expr::value(key.1)]))
}

/// Recent non-compressed messages for context assembly, chronological.
pub async fn recent_for_context(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    boundary: OrderKey,
    frontier: Option<OrderKey>,
    limit: u64,
) -> Result<Vec<message::Model>, DomainError> {
    if limit == 0 {
        return Ok(vec![]);
    }
    let mut cond = live_in_chat(chat_id)
        .add(message::Column::RequestId.is_not_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(key_le(boundary));
    if let Some(f) = frontier {
        cond = cond.add(key_gt(f));
    }
    let mut rows = message::Entity::find()
        .filter(cond)
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(limit)
        .all(runner)
        .await?;
    rows.reverse();
    Ok(rows)
}

/// Non-deleted, non-compressed messages in `(base, target]` (thread summary
/// range), chronological.
pub async fn summary_range(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: Option<OrderKey>,
    target: OrderKey,
) -> Result<Vec<message::Model>, DomainError> {
    let mut cond = live_in_chat(chat_id)
        .add(message::Column::IsCompressed.eq(false))
        .add(key_le(target));
    if let Some(b) = base {
        cond = cond.add(key_gt(b));
    }
    Ok(message::Entity::find()
        .filter(cond)
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

/// Mark `(base, target]` as compressed.
pub async fn mark_compressed(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: Option<OrderKey>,
    target: OrderKey,
) -> Result<u64, DomainError> {
    let mut cond = live_in_chat(chat_id)
        .add(message::Column::IsCompressed.eq(false))
        .add(key_le(target));
    if let Some(b) = base {
        cond = cond.add(key_gt(b));
    }
    let res = message::Entity::update_many()
        .secure()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(cond)
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected)
}

/// Clear `is_compressed` on all messages of a chat.
pub async fn clear_compressed(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<(), DomainError> {
    message::Entity::update_many()
        .secure()
        .col_expr(message::Column::IsCompressed, Expr::value(false))
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::IsCompressed.eq(true)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Soft-delete the messages of a turn.
pub async fn soft_delete_by_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, DomainError> {
    let res = message::Entity::update_many()
        .secure()
        .col_expr(message::Column::DeletedAt, Expr::value(now))
        .filter(live_in_chat(chat_id).add(message::Column::RequestId.eq(request_id)))
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected)
}

/// Messages by ids (any state), used by the thread-summary frontier check.
pub async fn find_live_by_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    id: Uuid,
) -> Result<Option<message::Model>, DomainError> {
    find_in_chat(runner, scope, chat_id, id).await
}

/// Latest non-deleted message of a chat that does not belong to the turn
/// `request_id` (frozen target frontier of a thread summary).
pub async fn latest_key_excluding_request(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<OrderKey>, DomainError> {
    let row = message::Entity::find()
        .filter(
            live_in_chat(chat_id).add(
                Condition::any()
                    .add(message::Column::RequestId.ne(request_id))
                    .add(message::Column::RequestId.is_null()),
            ),
        )
        .secure()
        .scope_with(scope)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    Ok(row.map(|m| (m.created_at, m.id)))
}
