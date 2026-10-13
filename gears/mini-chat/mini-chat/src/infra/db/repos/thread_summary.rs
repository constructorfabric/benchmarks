//! `thread_summaries` queries (one row per chat).

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::infra::db::entities::message;
use crate::infra::db::entities::thread_summary::{self, Column, Entity};
use crate::infra::db::repos::message::MessagePosition;

/// New summary state written by a thread-summary commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryUpdate {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub summary_text: String,
    /// New frontier (the task's frozen target).
    pub frontier: MessagePosition,
    pub token_estimate: i32,
    pub now: DateTime<Utc>,
}

/// Queries over `thread_summaries`.
pub struct ThreadSummaryRepo;

impl ThreadSummaryRepo {
    /// The thread summary of `chat_id`, if one exists.
    ///
    /// # Errors
    /// Database failures.
    pub async fn get_for_chat(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> DomainResult<Option<thread_summary::Model>> {
        Ok(Entity::find()
            .filter(Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(runner)
            .await?)
    }

    /// First summary of a chat: insert the row unless the chat already has one
    /// (`INSERT .. ON CONFLICT (chat_id) DO NOTHING`, so the transaction stays
    /// usable on `PostgreSQL`). Returns `false` when a row already existed.
    ///
    /// # Errors
    /// Scope violations and database failures.
    pub async fn insert_first(runner: &impl DBRunner, u: &SummaryUpdate) -> DomainResult<bool> {
        let scope = AccessScope::for_tenant(u.tenant_id);
        let am = thread_summary::Model {
            id: Uuid::new_v4(),
            tenant_id: u.tenant_id,
            chat_id: u.chat_id,
            summary_text: Some(u.summary_text.clone()),
            summarized_up_to_created_at: u.frontier.0,
            summarized_up_to_message_id: u.frontier.1,
            token_estimate: Some(u.token_estimate),
            created_at: u.now,
            updated_at: u.now,
        }
        .into_active_model()
        .reset_all();
        match Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict_raw(OnConflict::column(Column::ChatId).do_nothing().to_owned())
            .exec(runner)
            .await
        {
            Ok(_) => Ok(true),
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Compare-and-set of the chat's summary: replace text, frontier and token
    /// estimate only while the stored frontier still equals `base`. Returns
    /// `false` when no row matched (frontier moved or summary deleted).
    ///
    /// # Errors
    /// Database failures.
    pub async fn cas_update(
        runner: &impl DBRunner,
        base: MessagePosition,
        u: &SummaryUpdate,
    ) -> DomainResult<bool> {
        let res = Entity::update_many()
            .col_expr(
                Column::SummaryText,
                Expr::value(Some(u.summary_text.clone())),
            )
            .col_expr(Column::SummarizedUpToCreatedAt, Expr::value(u.frontier.0))
            .col_expr(Column::SummarizedUpToMessageId, Expr::value(u.frontier.1))
            .col_expr(Column::TokenEstimate, Expr::value(Some(u.token_estimate)))
            .col_expr(Column::UpdatedAt, Expr::value(u.now))
            .filter(Column::ChatId.eq(u.chat_id))
            .filter(Column::SummarizedUpToCreatedAt.eq(base.0))
            .filter(Column::SummarizedUpToMessageId.eq(base.1))
            .secure()
            .scope_with(&AccessScope::for_tenant(u.tenant_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected == 1)
    }

    /// Summary invalidation on a turn mutation (DESIGN §3.9 "Summary Interaction
    /// on Turn Mutation"): when the chat's summary frontier is at or after the
    /// mutated turn's user message `(user_msg_created_at, user_msg_id)`, delete the
    /// summary row and clear `is_compressed` on every message of the chat.
    /// Returns `true` when the summary was invalidated.
    ///
    /// # Errors
    /// Database failures.
    pub async fn invalidate_if_covers(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        user_msg_created_at: DateTime<Utc>,
        user_msg_id: Uuid,
    ) -> DomainResult<bool> {
        let Some(summary) = Self::get_for_chat(runner, tenant_id, chat_id).await? else {
            return Ok(false);
        };
        let frontier = (
            summary.summarized_up_to_created_at,
            summary.summarized_up_to_message_id,
        );
        if frontier < (user_msg_created_at, user_msg_id) {
            return Ok(false);
        }
        let scope = AccessScope::for_tenant(tenant_id);
        Entity::delete_many()
            .filter(Column::Id.eq(summary.id))
            .secure()
            .scope_with(&scope)
            .exec(runner)
            .await?;
        message::Entity::update_many()
            .col_expr(message::Column::IsCompressed, Expr::value(false))
            .filter(message::Column::ChatId.eq(chat_id))
            .filter(message::Column::IsCompressed.eq(true))
            .secure()
            .scope_with(&scope)
            .exec(runner)
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod thread_summary_tests;
