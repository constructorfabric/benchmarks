//! Chat repository port.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::model::{ChatDetail, NewChat};
use crate::domain::error::DomainError;

/// Persistence of chats. Every method applies the caller's [`AccessScope`] to its queries;
/// soft-deleted chats are never returned.
#[async_trait]
pub trait ChatRepository: Send + Sync {
    /// Inserts a chat; the insert is validated against the scope.
    async fn insert(&self, scope: &AccessScope, chat: NewChat) -> Result<ChatDetail, DomainError>;

    /// Loads a chat, or `None` when it is missing, soft-deleted or outside the scope.
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ChatDetail>, DomainError>;

    /// Lists the chats in scope: `OData` filter and order over `updated_at`, `id` and `title`,
    /// cursor pagination, newest activity first by default.
    async fn list(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError>;

    /// Renames a chat and bumps `updated_at`; `None` when the chat is not in scope.
    async fn rename(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        now: OffsetDateTime,
    ) -> Result<Option<ChatDetail>, DomainError>;

    /// Soft-deletes a chat, marks its attachments for provider cleanup and enqueues the
    /// chat-cleanup outbox message, all in one transaction. `false` when the chat is not in
    /// scope. `system_request_id` is the stable identity of the cleanup task.
    async fn soft_delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        system_request_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}
