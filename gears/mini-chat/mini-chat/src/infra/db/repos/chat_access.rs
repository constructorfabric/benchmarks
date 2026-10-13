//! Chat-scoped data access (`cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`).
//!
//! The chat is loaded through Secure ORM with the scope from the PEP; child rows
//! are reached only through a chat that this scope can see. Child tables carry
//! no owner column, so their queries use the tenant part of the scope plus the
//! `chat_id` of the chat just loaded: owner isolation of child rows comes from
//! the owner-scoped chat query.
//!
//! Rows of `chat_vector_stores` are reachable only through
//! [`load_vector_store`]; no function of this module looks them up by `id` or
//! `vector_store_id` alone.

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{DBRunner, ScopableEntity, SecureEntityExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model::{Attachment, Chat, ChatTurn, ChatVectorStore, Message};
use crate::infra::db::entities::{attachment, chat, chat_turn, chat_vector_store, message};
use crate::infra::db::mapping::scope_error;

/// Columns of a child table the chat-scoped lookup filters on.
struct ChildColumns<C> {
    /// Primary key of the child.
    id: C,
    /// Parent chat column.
    chat_id: C,
    /// Soft-delete column, for the tables that have one.
    deleted_at: Option<C>,
}

/// Loads a live chat visible to `scope`.
///
/// # Errors
///
/// `NotFound { Chat }` when the chat is missing, soft-deleted, of another
/// tenant or of another user.
pub async fn load_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Chat, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-select-chat
    let row = chat::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-select-chat

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-chat-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-chat-404
    row.map(Chat::from).ok_or(DomainError::NotFound {
        resource: ResourceKind::Chat,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-chat-404
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-chat-missing
}

/// Loads a chat and one of its live messages.
///
/// # Errors
///
/// `NotFound { Chat }` for the chat, `NotFound { Message }` for the child.
pub async fn load_message(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    message_id: Uuid,
) -> Result<(Chat, Message), DomainError> {
    let columns = ChildColumns {
        id: message::Column::Id,
        chat_id: message::Column::ChatId,
        deleted_at: Some(message::Column::DeletedAt),
    };
    load_in_chat::<message::Entity, Message>(
        runner,
        scope,
        chat_id,
        message_id,
        ResourceKind::Message,
        &columns,
    )
    .await
}

/// Loads a chat and one of its live turns (by turn id).
///
/// # Errors
///
/// `NotFound { Chat }` for the chat, `NotFound { Turn }` for the child.
pub async fn load_turn(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    turn_id: Uuid,
) -> Result<(Chat, ChatTurn), DomainError> {
    let columns = ChildColumns {
        id: chat_turn::Column::Id,
        chat_id: chat_turn::Column::ChatId,
        deleted_at: Some(chat_turn::Column::DeletedAt),
    };
    load_in_chat::<chat_turn::Entity, ChatTurn>(
        runner,
        scope,
        chat_id,
        turn_id,
        ResourceKind::Turn,
        &columns,
    )
    .await
}

/// Loads a chat and one of its live attachments.
///
/// # Errors
///
/// `NotFound { Chat }` for the chat, `NotFound { Attachment }` for the child.
pub async fn load_attachment(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    attachment_id: Uuid,
) -> Result<(Chat, Attachment), DomainError> {
    let columns = ChildColumns {
        id: attachment::Column::Id,
        chat_id: attachment::Column::ChatId,
        deleted_at: Some(attachment::Column::DeletedAt),
    };
    load_in_chat::<attachment::Entity, Attachment>(
        runner,
        scope,
        chat_id,
        attachment_id,
        ResourceKind::Attachment,
        &columns,
    )
    .await
}

/// Loads a chat and the vector store bound to it, if any. This is the only
/// access path to `chat_vector_stores` for request code: the row is selected by
/// the `chat_id` of the visible chat and the tenant of the scope.
///
/// # Errors
///
/// `NotFound { Chat }` for the chat.
pub async fn load_vector_store(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<(Chat, Option<ChatVectorStore>), DomainError> {
    let chat = load_chat(runner, scope, chat_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-forbid-unscoped
    let row = chat_vector_store::Entity::find()
        .secure()
        .scope_with(&scope.tenant_only())
        .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat.id)))
        .one(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-forbid-unscoped
    Ok((chat, row.map(ChatVectorStore::from)))
}

/// Loads the chat, then one child row of it.
async fn load_in_chat<E, D>(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    child_id: Uuid,
    kind: ResourceKind,
    columns: &ChildColumns<E::Column>,
) -> Result<(Chat, D), DomainError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::Model: Into<D>,
{
    let chat = load_chat(runner, scope, chat_id).await?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-child
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-select-child
    let mut filter = Condition::all()
        .add(columns.id.eq(child_id))
        .add(columns.chat_id.eq(chat.id));
    if let Some(deleted_at) = columns.deleted_at {
        filter = filter.add(deleted_at.is_null());
    }
    let row = E::find()
        .secure()
        .scope_with(&scope.tenant_only())
        .filter(filter)
        .one(runner)
        .await
        .map_err(|error| scope_error(&error))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-select-child
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-child

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-child-missing
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-child-404
    let child = row.ok_or(DomainError::NotFound { resource: kind })?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-child-404
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-if-child-missing

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-rows
    Ok((chat, child.into()))
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access:p1:inst-return-rows
}

#[cfg(test)]
#[path = "chat_access_tests.rs"]
mod tests;
