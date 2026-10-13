//! Plugin contracts resolved through types-registry and `ClientHub`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::events::{MiniChatAuditEvent, UsageEvent};
use crate::policy::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};

/// Errors of the model policy plugin read operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PolicyPluginError {
    #[error("policy version {0} not found")]
    VersionNotFound(i64),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PublishError {
    /// Retryable (the outbox handler returns `Retry`).
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Not retryable (the outbox handler returns `Reject`).
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("permanent audit error: {0}")]
    Permanent(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
}

/// `mini-chat-model-policy-plugin` contract (DESIGN §5.2.2).
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(&self, user_id: Uuid) -> Result<PolicyVersionInfo, PolicyPluginError>;

    /// Immutable snapshot of a policy version.
    async fn get_policy_snapshot(&self, user_id: Uuid, policy_version: i64)
    -> Result<PolicySnapshot, PolicyPluginError>;

    /// Per-user credit limits for a policy version.
    async fn get_user_limits(&self, user_id: Uuid, policy_version: i64) -> Result<UserLimits, PolicyPluginError>;

    /// Receives settled usage events (at-least-once; dedupe by `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check (never called by the gear in P1).
    async fn check_user_license(&self, _user_id: Uuid) -> Result<UserLicenseStatus, PolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin contract.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError>;
}
