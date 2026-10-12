//! Plugin client traits and error types (DESIGN 3.2, 5.2.2, 5.6).
//!
//! Plugins register a scoped client in the `ClientHub` under the GTS instance
//! id of their types-registry registration; the gear resolves them by vendor.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::policy::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Failure of a model-policy plugin read call.
#[derive(Debug, thiserror::Error)]
pub enum ModelPolicyPluginError {
    /// The requested policy version is unknown to the plugin.
    #[error("policy version {0} not found")]
    PolicyVersionNotFound(u64),
    /// The plugin backend is temporarily unavailable.
    #[error("model policy backend unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("model policy plugin error: {0}")]
    Internal(String),
}

/// Failure of `publish_usage`.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// Retry later (the outbox handler returns `Retry`).
    #[error("transient publish failure: {0}")]
    Transient(String),
    /// Never retry (the outbox handler returns `Reject`).
    #[error("permanent publish failure: {0}")]
    Permanent(String),
}

impl PublishError {
    /// Whether the delivery should be retried.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Failure of an audit plugin `emit` call.
#[derive(Debug, thiserror::Error)]
pub enum AuditPluginError {
    /// Retry later.
    #[error("transient audit failure: {0}")]
    Transient(String),
    /// The plugin did not answer in time (transient).
    #[error("audit plugin timed out")]
    PluginTimeout,
    /// Never retry (the audit handler returns `Reject`).
    #[error("permanent audit failure: {0}")]
    Permanent(String),
}

impl AuditPluginError {
    /// Whether the delivery should be retried.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::PluginTimeout)
    }
}

/// Client of the model-policy plugin (`MiniChatModelPolicyPluginSpecV1`).
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
    ///
    /// # Errors
    /// Plugin failure.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError>;

    /// Immutable snapshot (catalog, kill switches) of a policy version.
    ///
    /// # Errors
    /// Plugin failure or unknown version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError>;

    /// Per-user credit limits under a policy version.
    ///
    /// # Errors
    /// Plugin failure or unknown version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError>;

    /// Publishes one usage event (at-least-once, consumer deduplicates).
    ///
    /// # Errors
    /// [`PublishError::Transient`] to retry, [`PublishError::Permanent`] to reject.
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check. The gear never calls it (the license gate is the
    /// platform base feature); the default answers "inactive".
    ///
    /// # Errors
    /// Plugin failure.
    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        let _ = user_id;
        Ok(UserLicenseStatus { active: false })
    }
}

/// Client of the audit plugin (`MiniChatAuditPluginSpecV1`).
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Receives one audit event.
    ///
    /// # Errors
    /// [`AuditPluginError::Transient`] / [`AuditPluginError::PluginTimeout`]
    /// to retry, [`AuditPluginError::Permanent`] to reject.
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
