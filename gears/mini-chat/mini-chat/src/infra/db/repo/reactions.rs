//! Statements on `message_reactions` (tenant + owner scoped: pass the PDP scope of the chat
//! action, already narrowed to the calling user).

use std::collections::HashMap;

use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt as _, SecureEntityExt, SecureInsertExt as _,
};
use uuid::Uuid;

use crate::domain::error::{DomainError, map_scope_err};
use crate::infra::db::entity::message_reactions::{self, Column};

/// Inserts the reaction, or replaces the `reaction` value of the existing row of
/// `(message_id, user_id)` (its `id` and `created_at`, the reaction's creation time, stay).
///
/// # Errors
/// `AccessDenied` when the row is outside `scope`, `Internal` on a database error.
pub async fn upsert(
    conn: &impl DBRunner,
    scope: &AccessScope,
    row: message_reactions::ActiveModel,
) -> Result<(), DomainError> {
    let on_conflict = OnConflict::columns([Column::MessageId, Column::UserId])
        .update_column(Column::Reaction)
        .to_owned();
    message_reactions::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(map_scope_err)?
        .on_conflict_raw(on_conflict)
        .exec(conn)
        .await
        .map(drop)
        .map_err(map_scope_err)
}

/// The caller's reaction on `message_id`.
///
/// # Errors
/// `Internal` on a database error.
pub async fn find(
    conn: &impl DBRunner,
    scope: &AccessScope,
    message_id: Uuid,
) -> Result<Option<message_reactions::Model>, DomainError> {
    message_reactions::Entity::find()
        .filter(Column::MessageId.eq(message_id))
        .secure()
        .scope_with(scope)
        .one(conn)
        .await
        .map_err(map_scope_err)
}

/// Removes the caller's reaction on `message_id`; `false` when there was none.
///
/// # Errors
/// `Internal` on a database error.
pub async fn delete(
    conn: &impl DBRunner,
    scope: &AccessScope,
    message_id: Uuid,
) -> Result<bool, DomainError> {
    let res = message_reactions::Entity::delete_many()
        .filter(Column::MessageId.eq(message_id))
        .secure()
        .scope_with(scope)
        .exec(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(res.rows_affected > 0)
}

/// The caller's reaction (`like` / `dislike`) per message, for the messages that have one.
///
/// # Errors
/// `Internal` on a database error.
pub async fn for_messages(
    conn: &impl DBRunner,
    scope: &AccessScope,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, DomainError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = message_reactions::Entity::find()
        .filter(Column::MessageId.is_in(message_ids.iter().copied()))
        .secure()
        .scope_with(scope)
        .all(conn)
        .await
        .map_err(map_scope_err)?;
    Ok(rows
        .into_iter()
        .map(|r| (r.message_id, r.reaction))
        .collect())
}
