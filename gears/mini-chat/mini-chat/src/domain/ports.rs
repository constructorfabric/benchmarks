//! Ports the domain services depend on; the infrastructure layer implements them.
// @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-quality-gates:p1

use std::collections::HashMap;

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, PolicyVersionInfo, UserLimits};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Chat, Message};

/// Read access to the model policy (current version and immutable snapshots),
/// resolved lazily from the model-policy plugin.
#[async_trait]
pub trait PolicyCatalog: Send + Sync {
    /// Current policy version of `user_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyResolution`] when the plugin is not registered,
    /// cannot be resolved, fails or times out.
    async fn current_policy_version(&self, user_id: Uuid)
    -> Result<PolicyVersionInfo, DomainError>;

    /// Immutable snapshot of `(user_id, policy_version)`.
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyResolution`] when the plugin is not registered,
    /// cannot be resolved, fails or times out.
    async fn policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError>;

    /// Credit limits of `(user_id, policy_version)` (no tenant id is passed:
    /// limits are keyed by user and version).
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyResolution`] when the plugin is not registered,
    /// cannot be resolved, fails or times out.
    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError>;
}

/// Storage of chats (the chat store component): every method is scoped by the
/// `AccessScope` of the PEP decision of the calling operation.
#[async_trait]
pub trait ChatRepository: Send + Sync {
    /// Inserts a chat inside `scope`.
    ///
    /// # Errors
    ///
    /// [`DomainError::AuthzDenied`] when the row is outside the scope,
    /// [`DomainError::Database`] otherwise.
    async fn insert(&self, scope: &AccessScope, chat: &Chat) -> Result<(), DomainError>;

    /// Loads a live chat visible to `scope`
    /// (`cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`).
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }` for a missing, soft-deleted or foreign chat.
    async fn load(&self, scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError>;

    /// Number of non-deleted messages (all roles) per chat, in one query; a
    /// chat without messages is absent from the map.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn message_counts(
        &self,
        scope: &AccessScope,
        chat_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError>;

    /// Sets `title` and `updated_at` of a live chat; returns the affected rows.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn rename(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError>;

    /// One page of the live chats visible to `scope`.
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidQuery`] for every `OData` failure.
    async fn page(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<Chat>, DomainError>;

    /// Soft-deletes the chat, marks its attachments for cleanup and enqueues
    /// the chat-cleanup message, all in one transaction
    /// (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`).
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }`, [`DomainError::CleanupPayloadTooLarge`],
    /// [`DomainError::Internal`] / [`DomainError::Database`]; nothing is
    /// written on error.
    async fn soft_delete_with_cleanup(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<(), DomainError>;
}

/// A stored message of the history, with the columns the response needs.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// Message id.
    pub id: Uuid,
    /// Request id; `None` only for a corrupt row.
    pub request_id: Option<Uuid>,
    /// `user`, `assistant` or `system`.
    pub role: String,
    /// Content.
    pub content: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Effective model, when stored.
    pub model: Option<String>,
    /// Input tokens (0 when unknown).
    pub input_tokens: i64,
    /// Output tokens (0 when unknown).
    pub output_tokens: i64,
}

/// A non-deleted attachment linked to a message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    /// Attachment id.
    pub id: Uuid,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// File name.
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// WebP thumbnail bytes, when stored.
    pub img_thumbnail: Option<Vec<u8>>,
    /// Thumbnail width.
    pub img_thumbnail_width: Option<i32>,
    /// Thumbnail height.
    pub img_thumbnail_height: Option<i32>,
}

/// A stored reaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReaction {
    /// Reacted message.
    pub message_id: Uuid,
    /// `like` or `dislike`.
    pub reaction: String,
    /// Time of the last `PUT`.
    pub created_at: OffsetDateTime,
}

/// Storage of messages and reactions.
#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Loads the live chat visible to `scope`.
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }`.
    async fn load_chat(&self, scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError>;

    /// Loads the chat and one of its live messages
    /// (`cpt-cf-mini-chat-algo-gear-foundation-scoped-chat-access`).
    ///
    /// # Errors
    ///
    /// `NotFound { Chat }` for the chat, `NotFound { Message }` for the child.
    async fn load_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<(Chat, Message), DomainError>;

    /// One page of the non-deleted messages of `chat_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidQuery`] for every `OData` failure.
    async fn page(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<MessageRow>, DomainError>;

    /// Non-deleted attachments of `message_ids` per message, in one batch,
    /// ordered by link time then attachment id.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn attachments_of(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentRow>>, DomainError>;

    /// The reactions of `user_id` on `message_ids`, in one batch.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn reactions_of(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, DomainError>;

    /// Upserts the reaction of `user_id` on `message_id` in one statement on
    /// UNIQUE `(message_id, user_id)` and returns the stored row.
    ///
    /// # Errors
    ///
    /// [`DomainError::AuthzDenied`] outside the scope,
    /// [`DomainError::Database`] otherwise.
    async fn upsert_reaction(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        message_id: Uuid,
        reaction: &str,
        at: OffsetDateTime,
    ) -> Result<StoredReaction, DomainError>;

    /// Deletes the reaction of `user_id` on `message_id`; 0 rows is success.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn delete_reaction(
        &self,
        scope: &AccessScope,
        user_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError>;
}
