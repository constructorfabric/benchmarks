//! `message_reactions` repository (DESIGN section 3.7 `message_reactions`,
//! section 3.3 "Message Reaction API").
//!
//! Reactions are tenant- and owner-scoped: every query uses the tenant and
//! owner predicates of the parent chat scope plus an explicit `user_id`, and
//! runs after the message was checked to belong to the chat. One row per
//! `(message_id, user_id)` (`uq_message_reactions_message_user`).

use std::collections::HashMap;

use async_trait::async_trait;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{DbRepository, owner_scope};
use crate::domain::error::DomainError;
use crate::domain::ports::{NewReaction, Reaction, ReactionRecord, ReactionRepository};
use crate::infra::db::entities::{TimestampForm, message_reactions};

fn parse_reaction(model: &message_reactions::Model) -> Result<Reaction, DomainError> {
    Reaction::parse(&model.reaction).ok_or_else(|| {
        DomainError::internal(format!(
            "reaction {} has an unknown stored value `{}`",
            model.id, model.reaction
        ))
    })
}

fn of_user(user_id: Uuid) -> Condition {
    Condition::all().add(message_reactions::Column::UserId.eq(user_id))
}

/// The reactions of `user_id` on `message_ids`, in one query.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored value outside the
/// value set.
pub async fn reactions_of_user(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Reaction>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_reactions::Entity::find()
        .filter(
            of_user(user_id)
                .add(message_reactions::Column::MessageId.is_in(message_ids.iter().copied())),
        )
        .secure()
        .scope_with(&owner_scope(chat_scope))
        .all(runner)
        .await?;
    rows.iter()
        .map(|row| parse_reaction(row).map(|reaction| (row.message_id, reaction)))
        .collect()
}

/// Reads the reaction of `(message_id, user_id)`.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure or a stored value outside the
/// value set.
pub async fn find(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<Option<ReactionRecord>, DomainError> {
    let row = message_reactions::Entity::find()
        .filter(of_user(user_id).add(message_reactions::Column::MessageId.eq(message_id)))
        .secure()
        .scope_with(&owner_scope(chat_scope))
        .one(runner)
        .await?;
    row.map(|row| {
        Ok(ReactionRecord {
            message_id: row.message_id,
            user_id: row.user_id,
            reaction: parse_reaction(&row)?,
            created_at: row.created_at.at(),
        })
    })
    .transpose()
}

/// Inserts the reaction, or replaces the value and time of the existing
/// reaction of `(message_id, user_id)`, and returns the stored row; the time
/// is bound as `ts`.
///
/// The write comes first and the row is read back on the same runner: run
/// inside a transaction (as [`ReactionRepository::upsert`] does), the
/// transaction takes the write lock with its first statement and the
/// read-back returns the row this call wrote, whatever concurrent `PUT` or
/// `DELETE` of the same reaction is in flight.
///
/// # Errors
///
/// [`DomainError::Internal`] when the row is outside the scope or the write
/// fails.
pub async fn upsert(
    runner: &(impl DBRunner + Sync),
    ts: TimestampForm,
    chat_scope: &AccessScope,
    reaction: NewReaction,
) -> Result<ReactionRecord, DomainError> {
    let scope = owner_scope(chat_scope);
    let model = message_reactions::ActiveModel {
        id: Set(reaction.id),
        message_id: Set(reaction.message_id),
        user_id: Set(reaction.user_id),
        tenant_id: Set(reaction.tenant_id),
        reaction: Set(reaction.reaction.as_str().to_owned()),
        created_at: Set(ts.stamp(reaction.created_at)),
    };
    let on_conflict = SecureOnConflict::<message_reactions::Entity>::columns([
        message_reactions::Column::MessageId,
        message_reactions::Column::UserId,
    ])
    .update_columns([
        message_reactions::Column::Reaction,
        message_reactions::Column::CreatedAt,
    ])?;
    message_reactions::Entity::insert(model.clone())
        .secure()
        .scope_with_model(&scope, &model)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    find(runner, chat_scope, reaction.message_id, reaction.user_id)
        .await?
        .ok_or_else(|| DomainError::internal("upserted reaction is not readable"))
}

/// Removes the reaction of `(message_id, user_id)`; no-op when absent.
///
/// # Errors
///
/// [`DomainError::Internal`] on a query failure.
pub async fn delete(
    runner: &impl DBRunner,
    chat_scope: &AccessScope,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<(), DomainError> {
    message_reactions::Entity::delete_many()
        .filter(of_user(user_id).add(message_reactions::Column::MessageId.eq(message_id)))
        .secure()
        .scope_with(&owner_scope(chat_scope))
        .exec(runner)
        .await?;
    Ok(())
}

#[async_trait]
impl ReactionRepository for DbRepository {
    async fn reactions_of_user(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Reaction>, DomainError> {
        let conn = self.db().conn()?;
        reactions_of_user(&conn, scope, user_id, message_ids).await
    }

    /// The upsert and its read-back form one transaction, so the response
    /// is the row this request wrote.
    async fn upsert(
        &self,
        scope: &AccessScope,
        reaction: NewReaction,
    ) -> Result<ReactionRecord, DomainError> {
        let db = self.db().db();
        let ts = self.timestamps();
        let scope = scope.clone();
        db.transaction_ref_mapped(move |tx| {
            Box::pin(async move { upsert(tx, ts, &scope, reaction).await })
        })
        .await
    }

    async fn delete(
        &self,
        scope: &AccessScope,
        message_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), DomainError> {
        let conn = self.db().conn()?;
        delete(&conn, scope, message_id, user_id).await
    }
}
