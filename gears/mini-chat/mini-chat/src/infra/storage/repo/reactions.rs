//! Message reaction repository.

use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::message_reactions;
use crate::error::DomainError;

/// Reaction repository.
#[derive(Clone)]
pub struct ReactionRepo {
    #[allow(dead_code)]
    db: Db,
}

impl ReactionRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Upsert a reaction; returns the stored row.
    pub async fn set(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: &str,
    ) -> Result<message_reactions::Model, DomainError> {
        let now = now();
        // Upsert via delete+insert is not atomic; use update-then-insert.
        let existing = message_reactions::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(message_reactions::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_reactions::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(message_reactions::Column::MessageId.eq(message_id)))
            .one(runner)
            .await?;
        if let Some(row) = existing {
            if row.reaction != reaction {
                message_reactions::Entity::update_many()
                    .col_expr(
                        message_reactions::Column::Reaction,
                        sea_orm::sea_query::Expr::value(reaction.to_owned()),
                    )
                    .filter(Condition::all().add(message_reactions::Column::Id.eq(row.id)))
                    .secure()
                    .scope_with(scope)
                    .exec(runner)
                    .await?;
            }
            return Ok(message_reactions::Model {
                reaction: reaction.to_owned(),
                ..row
            });
        }
        let am = message_reactions::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            message_id: ActiveValue::Set(message_id),
            user_id: ActiveValue::Set(user_id),
            tenant_id: ActiveValue::Set(tenant_id),
            reaction: ActiveValue::Set(reaction.to_owned()),
            created_at: ActiveValue::Set(now),
        };
        let model = toolkit_db::secure::secure_insert::<message_reactions::Entity>(am, scope, runner).await?;
        Ok(model)
    }

    /// Remove a reaction (idempotent).
    pub async fn remove(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        message_reactions::Entity::delete_many()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(message_reactions::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_reactions::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(message_reactions::Column::MessageId.eq(message_id)))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// Batch-read the caller's reactions for a set of messages.
    pub async fn for_messages(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, DomainError> {
        if message_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = message_reactions::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(message_reactions::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(message_reactions::Column::UserId.eq(user_id)))
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .all(runner)
            .await?;
        Ok(rows.into_iter().map(|r| (r.message_id, r.reaction)).collect())
    }
}
