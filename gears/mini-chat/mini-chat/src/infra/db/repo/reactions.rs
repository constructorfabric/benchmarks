//! `ReactionRepository` over the Secure ORM.

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::ReactionRepository;
use crate::domain::reactions::{NewReaction, ReactionRow};
use crate::infra::db::entity::message_reactions;
use crate::infra::db::timestamp::{from_db, to_db};

#[async_trait]
impl<R: DBRunner> ReactionRepository for R {
    async fn reactions_of_user(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, DomainError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = message_reactions::Entity::find()
            .filter(message_reactions::Column::UserId.eq(user_id))
            .filter(message_reactions::Column::MessageId.is_in(message_ids.iter().copied()))
            .secure()
            .scope_with(scope)
            .all(self)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.message_id, row.reaction))
            .collect())
    }

    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        reaction: NewReaction,
    ) -> Result<ReactionRow, DomainError> {
        let model = message_reactions::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(reaction.message_id),
            user_id: Set(reaction.user_id),
            tenant_id: Set(reaction.tenant_id),
            reaction: Set(reaction.reaction),
            created_at: Set(to_db(reaction.now)),
        };
        let on_conflict = SecureOnConflict::<message_reactions::Entity>::columns([
            message_reactions::Column::MessageId,
            message_reactions::Column::UserId,
        ])
        .update_columns([
            message_reactions::Column::Reaction,
            message_reactions::Column::CreatedAt,
        ])?;
        let stored = message_reactions::Entity::insert(model.clone())
            .secure()
            .scope_with_model(scope, &model)?
            .on_conflict(on_conflict)
            .exec_with_returning(self)
            .await?;
        Ok(ReactionRow {
            message_id: stored.message_id,
            reaction: stored.reaction,
            created_at: from_db(stored.created_at),
        })
    }

    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        message_reactions::Entity::delete_many()
            .filter(message_reactions::Column::UserId.eq(user_id))
            .filter(message_reactions::Column::MessageId.eq(message_id))
            .secure()
            .scope_with(scope)
            .exec(self)
            .await?;
        Ok(())
    }
}
