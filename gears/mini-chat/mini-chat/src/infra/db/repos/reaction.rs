//! `message_reactions` queries. A reaction belongs to `(message, user)`; every
//! method is scoped to the caller's tenant and owner.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::infra::db::entities::message_reaction::{ActiveModel, Column, Entity, Model};

/// Queries over `message_reactions`.
pub struct ReactionRepo;

fn owner_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

impl ReactionRepo {
    /// Set the reaction of `user_id` on `message_id`, replacing an existing one
    /// (`created_at` is the time of the latest value), in one `INSERT .. ON CONFLICT ..
    /// RETURNING` statement. Returns the stored row.
    ///
    /// # Errors
    /// Database failures.
    pub async fn upsert(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: &str,
        now: DateTime<Utc>,
    ) -> DomainResult<Model> {
        let scope = owner_scope(tenant_id, user_id);
        let am = ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            message_id: ActiveValue::Set(message_id),
            user_id: ActiveValue::Set(user_id),
            tenant_id: ActiveValue::Set(tenant_id),
            reaction: ActiveValue::Set(reaction.to_owned()),
            created_at: ActiveValue::Set(now),
        };
        let on_conflict = SecureOnConflict::<Entity>::columns([Column::MessageId, Column::UserId])
            .update_columns([Column::Reaction, Column::CreatedAt])?;
        Ok(Entity::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)?
            .on_conflict(on_conflict)
            .exec_with_returning(runner)
            .await?)
    }

    /// Remove the reaction of `user_id` on `message_id`; returns the removed row count.
    ///
    /// # Errors
    /// Database failures.
    pub async fn delete(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
    ) -> DomainResult<u64> {
        use toolkit_db::secure::SecureDeleteExt;
        let res = Entity::delete_many()
            .filter(
                Condition::all()
                    .add(Column::MessageId.eq(message_id))
                    .add(Column::UserId.eq(user_id)),
            )
            .secure()
            .scope_with(&owner_scope(tenant_id, user_id))
            .exec(runner)
            .await?;
        Ok(res.rows_affected)
    }

    /// Reaction values of `user_id` on `message_ids` (messages without one are absent).
    ///
    /// # Errors
    /// Database failures.
    pub async fn for_messages(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> DomainResult<HashMap<Uuid, String>> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = Entity::find()
            .filter(
                Condition::all()
                    .add(Column::UserId.eq(user_id))
                    .add(Column::MessageId.is_in(message_ids.iter().copied())),
            )
            .secure()
            .scope_with(&owner_scope(tenant_id, user_id))
            .all(runner)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.message_id, r.reaction))
            .collect())
    }
}
