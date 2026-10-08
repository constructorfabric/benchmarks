//! `thread_summaries` repository (committed summary state only).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::db::entities::thread_summary;
use crate::infra::db::repo::messages::OrderKey;

pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<thread_summary::Model>, DomainError> {
    Ok(thread_summary::Entity::find()
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

pub async fn delete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<u64, DomainError> {
    let res = thread_summary::Entity::delete_many()
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected)
}

/// Insert the first summary of a chat.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    text: &str,
    frontier: OrderKey,
    token_estimate: i32,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        summary_text: Set(text.to_owned()),
        summarized_up_to_created_at: Set(frontier.0),
        summarized_up_to_message_id: Set(frontier.1),
        token_estimate: Set(token_estimate),
        created_at: Set(now),
        updated_at: Set(now),
    };
    thread_summary::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)?
        .exec(runner)
        .await?;
    Ok(())
}

/// CAS update: advance the frontier only if it still equals `base`.
#[allow(clippy::too_many_arguments)]
pub async fn cas_update(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: OrderKey,
    text: &str,
    frontier: OrderKey,
    token_estimate: i32,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let res = thread_summary::Entity::update_many()
        .secure()
        .col_expr(
            thread_summary::Column::SummaryText,
            Expr::value(text.to_owned()),
        )
        .col_expr(
            thread_summary::Column::SummarizedUpToCreatedAt,
            Expr::value(frontier.0),
        )
        .col_expr(
            thread_summary::Column::SummarizedUpToMessageId,
            Expr::value(frontier.1),
        )
        .col_expr(
            thread_summary::Column::TokenEstimate,
            Expr::value(token_estimate),
        )
        .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(thread_summary::Column::ChatId.eq(chat_id))
                .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(base.0))
                .add(thread_summary::Column::SummarizedUpToMessageId.eq(base.1)),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}
