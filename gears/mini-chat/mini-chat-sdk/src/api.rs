//! Plugin API traits for the mini-chat gear extension points.
//!
//! The gear discovers plugin instances through the types-registry (GTS) and
//! obtains their clients from the `ClientHub`:
//!
//! - [`MiniChatModelPolicyPluginClientV1`] — policy snapshots (model catalog,
//!   kill switches), per-user limits and usage publication.
//! - [`MiniChatAuditPluginClientV1`] — audit event ingestion.

use async_trait::async_trait;
use uuid::Uuid;

use crate::errors::{AuditError, ModelPolicyError, PublishError};
use crate::models::{PolicySnapshot, TurnAuditEvent, UsageEvent, UserLimits};

/// Plugin API of `mini-chat-model-policy-plugin` implementations.
///
/// The plugin is expected to be "dumb": it returns configuration and receives
/// usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Return the current (latest) policy version for the given user.
    ///
    /// # Errors
    ///
    /// - [`ModelPolicyError::Transient`] when the backend is unreachable
    /// - [`ModelPolicyError::Permanent`] for unrecoverable failures
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<i64, ModelPolicyError>;

    /// Return the immutable policy snapshot for the given version.
    ///
    /// For a fixed `(user_id, policy_version)` the plugin MUST return exactly
    /// the same snapshot forever.
    ///
    /// # Errors
    ///
    /// - [`ModelPolicyError::NotFound`] when the version is unknown
    /// - [`ModelPolicyError::Transient`] when the backend is unreachable
    /// - [`ModelPolicyError::Permanent`] for unrecoverable failures
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, ModelPolicyError>;

    /// Return the per-user credit allocation derived under `policy_version`.
    ///
    /// # Errors
    ///
    /// - [`ModelPolicyError::NotFound`] when the version is unknown
    /// - [`ModelPolicyError::Transient`] when the backend is unreachable
    /// - [`ModelPolicyError::Permanent`] for unrecoverable failures
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, ModelPolicyError>;

    /// Receive a settled usage event.
    ///
    /// Delivery is at-least-once; implementations MUST be idempotent on the
    /// event's `dedupe_key`.
    ///
    /// # Errors
    ///
    /// - [`PublishError::Transient`] / [`PublishError::PluginTimeout`] are
    ///   retried by the outbox
    /// - [`PublishError::Permanent`] dead-letters the event
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// Check whether the user holds an active mini-chat license.
    ///
    /// The gear never calls this method (the license gate is enforced on the
    /// routes with the platform base license feature); the default body
    /// returns `active: false`.
    async fn check_user_license(&self, _user_id: Uuid) -> bool {
        false
    }
}

/// Plugin API of the mini-chat audit plugin (`MiniChatAuditPluginClientV1`).
///
/// The gear enqueues audit events to the `mini-chat.audit` outbox queue; the
/// audit outbox handler delivers them to the selected plugin.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Record one structured audit event.
    ///
    /// # Errors
    ///
    /// - [`AuditError::Transient`] / [`AuditError::PluginTimeout`] are retried
    ///   by the outbox
    /// - [`AuditError::Permanent`] dead-letters the event
    async fn record_audit_event(&self, event: TurnAuditEvent) -> Result<(), AuditError>;
}
