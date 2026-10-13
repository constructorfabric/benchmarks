//! `message_reaction` repository (owner-scoped).

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureUpdateExt,
};
use uuid::Uuid;

use super::insert_model;
use crate::infra::db::entity::message_reaction;

/// Repository for `message_reaction` rows.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReactionRepo;

impl ReactionRepo {
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
        row: message_reaction::Model,
    ) -> Result<message_reaction::Model, ScopeError> {
        insert_model::<message_reaction::Entity>(runner, scope, row).await
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
    ) -> Result<Option<message_reaction::Model>, ScopeError> {
        message_reaction::Entity::find_by_id(id)
            .secure()
            .scope_with(scope)
            .one(runner)
            .await
    }

    /// Reactions of `user_id` on the given messages (owner scope).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn list_for_messages(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<Vec<message_reaction::Model>, ScopeError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        message_reaction::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .all(runner)
            .await
    }

    /// Replace the reaction of `user_id` on `message_id` (value and
    /// `created_at`). Returns the updated row, `None` when the user has no
    /// reaction on the message.
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn replace(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
        reaction: &str,
        now: OffsetDateTime,
    ) -> Result<Option<message_reaction::Model>, ScopeError> {
        let res = message_reaction::Entity::update_many()
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.eq(message_id)),
            )
            .secure()
            .scope_with(scope)
            .col_expr(message_reaction::Column::Reaction, Expr::value(reaction))
            .col_expr(message_reaction::Column::CreatedAt, Expr::value(now))
            .exec(runner)
            .await?;
        if res.rows_affected == 0 {
            return Ok(None);
        }
        Ok(self
            .list_for_messages(runner, scope, user_id, &[message_id])
            .await?
            .into_iter()
            .next())
    }

    /// Delete the reaction of `user_id` on `message_id`. Returns the rows
    /// deleted (0 when there was none).
    ///
    /// # Errors
    ///
    /// `ScopeError` on a database error.
    pub async fn delete_for_message(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<u64, ScopeError> {
        let res = message_reaction::Entity::delete_many()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(message_reaction::Column::UserId.eq(user_id))
                    .add(message_reaction::Column::MessageId.eq(message_id)),
            )
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }
}
