//! `attachments` repository.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::attachments::{ActiveModel, Column, Entity, Model};

fn tscope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Attachment statuses.
pub mod status {
    pub const PENDING: &str = "pending";
    pub const UPLOADED: &str = "uploaded";
    pub const READY: &str = "ready";
    pub const FAILED: &str = "failed";
}

/// Inserts an attachment row.
///
/// # Errors
/// Database errors.
pub async fn insert(runner: &impl DBRunner, tenant_id: Uuid, am: ActiveModel) -> Result<(), DomainError> {
    Entity::insert(am)
        .secure()
        .scope_unchecked(&tscope(tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}

/// Attachment of a chat (non-deleted).
///
/// # Errors
/// Database errors.
pub async fn find_in_chat(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
    include_deleted: bool,
) -> Result<Option<Model>, DomainError> {
    let mut q = Entity::find()
        .filter(Column::Id.eq(attachment_id))
        .filter(Column::ChatId.eq(chat_id));
    if !include_deleted {
        q = q.filter(Column::DeletedAt.is_null());
    }
    Ok(q.secure().scope_with(&tscope(tenant_id)).one(runner).await?)
}

/// Attachment by id (system access).
///
/// # Errors
/// Database errors.
pub async fn find_by_id(runner: &impl DBRunner, id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Id.eq(id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?)
}

/// Attachments with the given ids in a chat (including deleted).
///
/// # Errors
/// Database errors.
pub async fn find_many(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::Id.is_in(ids.to_vec()))
        .secure()
        .scope_with(&tscope(tenant_id))
        .all(runner)
        .await?)
}

/// Non-deleted attachments of a chat.
///
/// # Errors
/// Database errors.
pub async fn list_for_chat(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Vec<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::DeletedAt.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .order_by(Column::CreatedAt, Order::Asc)
        .all(runner)
        .await?)
}

/// All attachments of a chat including deleted (cleanup).
///
/// # Errors
/// Database errors.
pub async fn list_all_for_chat(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Vec<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .all(runner)
        .await?)
}

/// Generic column update of one attachment.
///
/// # Errors
/// Database errors.
pub async fn update(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    sets: Vec<(Column, sea_orm::sea_query::SimpleExpr)>,
    extra: Option<Condition>,
) -> Result<u64, DomainError> {
    let mut q = Entity::update_many();
    for (c, e) in sets {
        q = q.col_expr(c, e);
    }
    let mut cond = Condition::all().add(Column::Id.eq(id));
    if let Some(extra) = extra {
        cond = cond.add(extra);
    }
    Ok(q.filter(cond)
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Marks a row `failed` with an error code.
///
/// # Errors
/// Database errors.
pub async fn mark_failed(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    id: Uuid,
    error_code: &str,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    update(
        runner,
        tenant_id,
        id,
        vec![
            (Column::Status, Expr::value(status::FAILED)),
            (Column::ErrorCode, Expr::value(error_code.to_owned())),
            (Column::UpdatedAt, Expr::value(now)),
        ],
        None,
    )
    .await
}

/// Soft-deletes an attachment and marks cleanup pending.
///
/// # Errors
/// Database errors.
pub async fn soft_delete(runner: &impl DBRunner, tenant_id: Uuid, id: Uuid, now: DateTime<Utc>) -> Result<u64, DomainError> {
    update(
        runner,
        tenant_id,
        id,
        vec![
            (Column::DeletedAt, Expr::value(now)),
            (Column::UpdatedAt, Expr::value(now)),
            (Column::CleanupStatus, Expr::value("pending")),
            (Column::CleanupUpdatedAt, Expr::value(now)),
        ],
        Some(Condition::all().add(Column::DeletedAt.is_null())),
    )
    .await
}

/// Marks every attachment of a (deleted) chat `cleanup_status = pending`
/// where no cleanup state is set yet.
///
/// # Errors
/// Database errors.
pub async fn mark_chat_cleanup_pending(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), DomainError> {
    Entity::update_many()
        .col_expr(Column::CleanupStatus, Expr::value("pending"))
        .col_expr(Column::CleanupUpdatedAt, Expr::value(now))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::CleanupStatus.is_null())
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?;
    Ok(())
}

/// Stale `pending`/`uploaded` rows for the upload reaper.
///
/// # Errors
/// Database errors.
pub async fn stale_uploads(runner: &impl DBRunner, cutoff: DateTime<Utc>, limit: u64) -> Result<Vec<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::Status.is_in([status::PENDING, status::UPLOADED]))
        .filter(Column::DeletedAt.is_null())
        .filter(Column::CleanupStatus.is_null())
        .filter(Column::UpdatedAt.lt(cutoff))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .order_by(Column::UpdatedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await?)
}
