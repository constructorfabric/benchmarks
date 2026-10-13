//! Ports of the turn mutations: the mutation store and the metrics sink. The
//! infrastructure layer implements them; the domain never sees `SeaORM`.

use async_trait::async_trait;
use mini_chat_sdk::TurnMutationAuditEvent;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::types::{LatestTurn, MutationOp, MutationResult, TargetMessage, TargetTurn};
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::MessageBoundary;

/// Everything the replacement transaction writes
/// (`cpt-cf-mini-chat-algo-turn-mutations-replace-turn`). Every value is
/// computed before the transaction and captured by value, so a conflict
/// re-runs the same closure.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ReplacementPlan {
    /// `Retry` or `Edit`.
    pub op: MutationOp,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The caller (`requester_user_id` of the new turn, owner scope of the
    /// chat bump).
    pub user_id: Uuid,
    /// Id of the replaced turn.
    pub target_turn_id: Uuid,
    /// `request_id` of the replaced turn.
    pub target_request_id: Uuid,
    /// `started_at` of the replaced turn (the latest-turn re-check bound).
    pub target_started_at: OffsetDateTime,
    /// `(created_at, id)` of the replaced user message (summary coverage).
    pub target_user_message: MessageBoundary,
    /// Id of the new turn.
    pub new_turn_id: Uuid,
    /// Server-generated `request_id` of the new turn.
    pub new_request_id: Uuid,
    /// Id of the new user message.
    pub new_user_message_id: Uuid,
    /// Pre-allocated assistant message id of the new turn.
    pub assistant_message_id: Uuid,
    /// Content of the new user message (original on retry, as sent on edit).
    pub content: String,
    /// `web_search_enabled` of the replaced turn, carried forward.
    pub web_search_enabled: bool,
    /// The mutation time.
    pub now: OffsetDateTime,
    /// The `turn_retry` / `turn_edit` event enqueued in the transaction.
    pub audit: TurnMutationAuditEvent,
}

/// Everything the delete transaction writes
/// (`cpt-cf-mini-chat-algo-turn-mutations-remove-turn`).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct DeletePlan {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// Id of the deleted turn.
    pub target_turn_id: Uuid,
    /// `request_id` of the deleted turn.
    pub target_request_id: Uuid,
    /// `started_at` of the deleted turn (the latest-turn re-check bound).
    pub target_started_at: OffsetDateTime,
    /// `(created_at, id)` of the deleted user message (summary coverage).
    pub target_user_message: MessageBoundary,
    /// The mutation time.
    pub now: OffsetDateTime,
    /// The `turn_delete` event enqueued in the transaction.
    pub audit: TurnMutationAuditEvent,
}

/// Storage of the turn mutations. Reads take the compiled `AccessScope` of
/// the PEP decision and are bound to the chat; they run outside any
/// transaction. The two transactions re-check their outcome with guarded
/// writes.
#[async_trait]
pub trait MutationRepository: Send + Sync {
    /// The turn with `(chat_id, request_id)`, soft-deleted rows included, with
    /// its `web_search_enabled`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn find_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetTurn>, DomainError>;

    /// The latest live turn of the chat: `deleted_at IS NULL`, greatest
    /// `(started_at, id)`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<LatestTurn>, DomainError>;

    /// The live user message of the turn with `request_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetMessage>, DomainError>;

    /// The attachment ids linked to `user_message_id` whose attachment row has
    /// `deleted_at IS NULL`, in link order.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn carried_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError>;

    /// The snapshot boundary of a replacement: the latest live message with a
    /// request id other than `new_request_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn history_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        new_request_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError>;

    /// The replacement transaction: the new `running` turn first, then the
    /// guarded soft-delete of the old turn, the latest-turn re-check, the
    /// message soft-delete, the new user message, the carried links, the
    /// summary invalidation, the chat bump and the audit enqueue.
    ///
    /// # Errors
    ///
    /// `UniqueViolation` when the INSERT of the new turn meets a unique
    /// constraint (the caller decides whether it is the one-running-turn
    /// index), `NotLatestTurn` when the guarded UPDATE updates no row or a
    /// newer live turn exists, `Database` / `Internal` otherwise. Nothing is
    /// left behind on an error.
    async fn replace_turn(&self, plan: &ReplacementPlan) -> Result<(), DomainError>;

    /// The delete transaction: the guarded soft-delete of the turn first, then
    /// the latest-turn re-check, the message soft-delete, the summary
    /// invalidation and the audit enqueue.
    ///
    /// # Errors
    ///
    /// `NotLatestTurn`, `Database` / `Internal`. Nothing is left behind on an
    /// error.
    async fn delete_turn(&self, plan: &DeletePlan) -> Result<(), DomainError>;

    /// The setup-failure mark: `state` `failed`, `error_code`, `completed_at`
    /// and `updated_at` `at`, WHERE `id` is the turn, `tenant_id` is the
    /// tenant, `state` is `running` and `reserve_tokens IS NULL`, as a single
    /// autocommit statement. Returns the updated rows.
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    async fn mark_setup_failed(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError>;
}

/// The mutation instruments
/// (`cpt-cf-mini-chat-algo-turn-mutations-record-metrics`); the only labels
/// are `op` and `result`.
pub trait MutationMetrics: Send + Sync {
    /// `mini_chat_turn_mutation{op,result}`, once per request.
    fn turn_mutation(&self, op: MutationOp, result: MutationResult);
    /// `mini_chat_turn_mutation_latency_ms{op}`, for committed mutations only.
    fn turn_mutation_latency_ms(&self, op: MutationOp, millis: f64);
}
