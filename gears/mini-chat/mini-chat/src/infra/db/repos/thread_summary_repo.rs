//! `thread_summary` repository (chat child: tenant-only scope).

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureUpdateExt,
};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::thread_summary;

/// Repository for `thread_summary` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSummaryRepo;

impl ThreadSummaryRepo {
    /// Insert a complete row.
    ///
    /// # Errors
    ///
    /// `ScopeError` on scope denial or a database error (unique/CHECK
    /// violations included).
    pub async fn insert(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: thread_summary::Model,
    ) -> Result<thread_summary::Model, ScopeError> {
        insert_model::<thread_summary::Entity>(runner, &scope.tenant_only(), row).await
    }

    /// Load a row by id within the scope.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_id(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<thread_summary::Model>, ScopeError> {
        thread_summary::Entity::find_by_id(id)
            .secure()
            .scope_with(&scope.tenant_only())
            .one(runner)
            .await
    }

    /// The summary of the chat, if any (one row per chat).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn find_by_chat(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<thread_summary::Model>, ScopeError> {
        thread_summary::Entity::find()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(sea_orm::Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await
    }

    /// Compare-and-set of the chat's summary: replace text, frontier and
    /// token estimate only while the stored frontier is still the message
    /// `base_message_id` (a message id identifies its order key). Returns
    /// the rows updated (0 = the frontier moved or the row is gone).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    #[allow(clippy::too_many_arguments)]
    pub async fn advance(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        base_message_id: Uuid,
        summary_text: &str,
        frontier: (OffsetDateTime, Uuid),
        token_estimate: i32,
        now: OffsetDateTime,
    ) -> Result<u64, ScopeError> {
        let res = thread_summary::Entity::update_many()
            .filter(
                sea_orm::Condition::all()
                    .add(thread_summary::Column::ChatId.eq(chat_id))
                    .add(thread_summary::Column::SummarizedUpToMessageId.eq(base_message_id)),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .col_expr(
                thread_summary::Column::SummaryText,
                Expr::value(summary_text),
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
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Delete the summary row `id` of the chat. Returns the rows deleted.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn delete(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = thread_summary::Entity::delete_many()
            .secure()
            .scope_with(&scope.tenant_only())
            .filter(
                sea_orm::Condition::all()
                    .add(thread_summary::Column::ChatId.eq(chat_id))
                    .add(thread_summary::Column::Id.eq(id)),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }
}
