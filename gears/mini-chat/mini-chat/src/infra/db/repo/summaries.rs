//! `thread_summaries` repository.

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::thread_summaries::{ActiveModel, Column, Entity, Model};

fn tscope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Summary of a chat.
///
/// # Errors
/// Database errors.
pub async fn find(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<Model>, DomainError> {
    Ok(Entity::find()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .one(runner)
        .await?)
}

/// Deletes the summary of a chat.
///
/// # Errors
/// Database errors.
pub async fn delete(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<u64, DomainError> {
    Ok(Entity::delete_many()
        .filter(Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}

/// Inserts a new summary row.
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

/// CAS-update of a summary: only when the stored frontier equals `base`.
///
/// # Errors
/// Database errors.
#[allow(clippy::too_many_arguments)]
pub async fn cas_update(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: (DateTime<Utc>, Uuid),
    text: &str,
    target: (DateTime<Utc>, Uuid),
    token_estimate: i32,
    now: DateTime<Utc>,
) -> Result<u64, DomainError> {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;
    Ok(Entity::update_many()
        .col_expr(Column::SummaryText, Expr::value(text.to_owned()))
        .col_expr(Column::SummarizedUpToCreatedAt, Expr::value(target.0))
        .col_expr(Column::SummarizedUpToMessageId, Expr::value(target.1))
        .col_expr(Column::TokenEstimate, Expr::value(token_estimate))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(Column::ChatId.eq(chat_id))
        .filter(Column::SummarizedUpToCreatedAt.eq(base.0))
        .filter(Column::SummarizedUpToMessageId.eq(base.1))
        .secure()
        .scope_with(&tscope(tenant_id))
        .exec(runner)
        .await?
        .rows_affected)
}
