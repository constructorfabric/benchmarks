//! `messages` and `message_attachments` repository.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, Scoped, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureSelect, SecureUpdateExt,
};
use uuid::Uuid;

use super::{tuple_gt, tuple_lte};
use crate::domain::error::DomainError;
use crate::infra::db::entities::message_attachments as ma;
use crate::infra::db::entities::messages::{ActiveModel, Column, Entity, Model};

fn tscope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Inserts a message.
///
/// # Errors
/// Database errors (unique violation on `(chat_id, request_id, role)`).
pub async fn insert(runner: &impl DBRunner, tenant_id: Uuid, am: ActiveModel) -> Result<(), DomainError> {
    Entity::insert(am)
        .secure()
        .scope_unchecked(&tscope(tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}

/// Number of non-deleted messages of a chat.
///
/// # Errors
/// Database errors.
pub async fn count_for_chat(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    let n = Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .count(runner)
        .await?;
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Base select of non-deleted messages of a chat (list endpoint).
pub fn list_select(tenant_id: Uuid, chat_id: Uuid) -> SecureSelect<Entity, Scoped> {
    Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::RequestId.is_not_null())
        .secure()
        .scope_with(&tscope(tenant_id))
}

/// Non-deleted message by id in a chat.
///
/// # Errors
/// Database errors.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(message_id))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .one(runner)
        .await?)
}

/// Message by id (system access, may be deleted).
///
/// # Errors
/// Database errors.
pub async fn find_by_id(runner: &impl DBRunner, message_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(message_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?)
}

/// Message of a turn by role (non-deleted unless `include_deleted`).
///
/// # Errors
/// Database errors.
pub async fn by_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    role: &str,
    include_deleted: bool,
) -> Result<Option<Model>, DomainError> {
    let mut q = Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .filter(Column::Role.eq(role));
    if !include_deleted {
        q = q.filter(Column::DeletedAt.is_null());
    }
    Ok(q.secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Desc)
        .one(runner)
        .await?)
}

/// Most recent non-deleted assistant message with non-zero usage.
///
/// # Errors
/// Database errors.
pub async fn latest_assistant_with_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::Role.eq("assistant"))
        .filter(Column::DeletedAt.is_null())
        .filter(
            Condition::any()
                .add(Column::InputTokens.gt(0))
                .add(Column::OutputTokens.gt(0)),
        )
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Desc)
        .order_by(Column::Id, Order::Desc)
        .one(runner)
        .await?)
}

/// Latest non-deleted message of the chat (snapshot boundary).
///
/// # Errors
/// Database errors.
pub async fn latest(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Desc)
        .order_by(Column::Id, Order::Desc)
        .one(runner)
        .await?)
}

/// Latest non-deleted message not belonging to `exclude_request_id`.
///
/// # Errors
/// Database errors.
pub async fn latest_excluding_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    exclude_request_id: Uuid,
) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::RequestId.ne(exclude_request_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Desc)
        .order_by(Column::Id, Order::Desc)
        .one(runner)
        .await?)
}

/// Recent context messages: newest first, at most `limit`, within the
/// snapshot boundary and after the summary frontier, not compressed.
///
/// # Errors
/// Database errors.
pub async fn recent_for_context(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    boundary: (DateTime<Utc>, Uuid),
    frontier: Option<(DateTime<Utc>, Uuid)>,
    limit: u64,
) -> Result<Vec<Model>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut q = Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::RequestId.is_not_null())
        .filter(Column::DeletedAt.is_null())
        .filter(Column::IsCompressed.eq(false))
        .filter(tuple_lte(Column::CreatedAt, Column::Id, boundary.0, boundary.1));
    if let Some((ts, id)) = frontier {
        q = q.filter(tuple_gt(Column::CreatedAt, Column::Id, ts, id));
    }
    Ok(q.secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Desc)
        .order_by(Column::Id, Order::Desc)
        .limit(limit)
        .all(runner)
        .await?)
}

/// Messages to summarize: non-deleted, not compressed, in `(base, target]`,
/// ordered `(created_at, id)` ascending.
///
/// # Errors
/// Database errors.
pub async fn summary_range(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(DateTime<Utc>, Uuid)>,
    target: (DateTime<Utc>, Uuid),
) -> Result<Vec<Model>, DomainError> {
    let mut q = Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::IsCompressed.eq(false))
        .filter(tuple_lte(Column::CreatedAt, Column::Id, target.0, target.1));
    if let Some((ts, id)) = base {
        q = q.filter(tuple_gt(Column::CreatedAt, Column::Id, ts, id));
    }
    Ok(q.secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Asc)
        .order_by(Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

/// Marks a message range compressed.
///
/// # Errors
/// Database errors.
pub async fn mark_compressed(runner: &impl DBRunner, tenant_id: Uuid, ids: &[Uuid]) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    Entity::update_many()
        .col_expr(Column::IsCompressed, Expr::value(true))
        .filter(Column::Id.is_in(ids.to_vec()))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Clears `is_compressed` on all messages of a chat.
///
/// # Errors
/// Database errors.
pub async fn clear_compressed(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<(), DomainError> {
    Entity::update_many()
        .col_expr(Column::IsCompressed, Expr::value(false))
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Soft-deletes the messages of a turn.
///
/// # Errors
/// Database errors.
pub async fn soft_delete_for_request(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    Entity::update_many()
        .col_expr(Column::DeletedAt, Expr::value(now))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::RequestId.eq(request_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Hard-deletes a message (rollback of a failed partial insert).
///
/// # Errors
/// Database errors.
pub async fn delete_by_id(runner: &impl DBRunner, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
    Entity::delete_many()
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

// ── message_attachments ─────────────────────────────────────────────────────

/// Links attachments to a message.
///
/// # Errors
/// Database errors.
pub async fn link_attachments(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    for att in attachment_ids {
        let am = ma::ActiveModel {
            tenant_id: sea_orm::ActiveValue::Set(tenant_id),
            chat_id: sea_orm::ActiveValue::Set(chat_id),
            message_id: sea_orm::ActiveValue::Set(message_id),
            attachment_id: sea_orm::ActiveValue::Set(*att),
            created_at: sea_orm::ActiveValue::Set(now),
        };
        ma::Entity::insert(am)
            .secure()
            .scope_unchecked(&tscope(tenant_id))?
            .exec(runner)
            .await?;
    }
    Ok(())
}

/// Attachment links of the given messages.
///
/// # Errors
/// Database errors.
pub async fn links_for_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_ids: &[Uuid],
) -> Result<Vec<ma::Model>, DomainError> {
    if message_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ma::Entity::find()
        .filter(ma::Column::ChatId.eq(chat_id))
        .filter(ma::Column::MessageId.is_in(message_ids.to_vec()))
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(ma::Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// Whether any message references the attachment.
///
/// # Errors
/// Database errors.
pub async fn attachment_is_referenced(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> Result<bool, DomainError> {
    let n = ma::Entity::find()
        .filter(ma::Column::ChatId.eq(chat_id))
        .filter(ma::Column::AttachmentId.eq(attachment_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .count(runner)
        .await?;
    Ok(n > 0)
}
