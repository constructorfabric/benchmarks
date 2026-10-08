//! Authorized, owner-scoped chat lookup shared by every chat-scoped operation.

use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::domain::authz;
use crate::domain::error::DomainError;
use crate::domain::service::Deps;
use crate::infra::db::entity::chat;

/// A chat loaded under an authorized scope.
pub struct AuthorizedChat {
    pub chat: chat::Model,
    /// Full owner scope (for the chat row).
    pub scope: AccessScope,
    /// Tenant-only scope for child tables (messages, turns, attachments, ...),
    /// valid only together with a `chat_id` filter.
    pub child_scope: AccessScope,
}

/// Evaluates the PDP for `action` on `chat_id` and loads the non-deleted chat.
///
/// # Errors
/// 403/503 from the PEP, 404 when missing, soft-deleted or foreign.
pub async fn load_chat(
    deps: &Deps,
    ctx: &SecurityContext,
    chat_id: Uuid,
    action: &str,
) -> Result<AuthorizedChat, DomainError> {
    let scope = authz::chat_scope(&deps.enforcer, ctx, action, Some(chat_id)).await?;
    let conn = deps.db.conn()?;
    let chat = find_chat(&conn, &scope, chat_id).await?;
    let child_scope = scope.tenant_only();
    Ok(AuthorizedChat {
        chat,
        scope,
        child_scope,
    })
}

/// Loads a non-deleted chat under `scope` (owner + tenant predicates).
///
/// # Errors
/// 404 when missing / deleted / outside the scope.
pub async fn find_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<chat::Model, DomainError> {
    chat::Entity::find()
        .filter(chat::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .and_id(chat_id)?
        .one(runner)
        .await?
        .ok_or_else(DomainError::chat_not_found)
}
