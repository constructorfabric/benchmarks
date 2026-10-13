//! Plugin client contracts resolved by the gear through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::models::{PolicySnapshot, UsageEvent, UserLicenseStatus, UserLimits};

/// Errors of the model policy plugin.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    #[error("policy version {0} not found")]
    VersionNotFound(u64),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    #[error("transient publish error: {0}")]
    Transient(String),
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("permanent audit error: {0}")]
    Permanent(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
}

/// Model policy plugin (`mini-chat-model-policy-plugin`): policy snapshots,
/// per-user limits and usage publication.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version applicable to the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError>;

    /// Immutable snapshot of a policy version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user credit limits under a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish a usage event (at-least-once; consumers deduplicate on `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check; not called by the gear in P1.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin (`MiniChatAuditPluginClientV1`).
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
