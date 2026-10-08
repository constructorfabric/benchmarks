//! Plugin client traits consumed by the mini-chat gear.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::{AuditPluginError, MiniChatAuditEvent};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Errors of policy queries.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyPluginError {
    #[error("policy version {0} not found")]
    VersionNotFound(u64),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin error: {0}")]
    Internal(String),
}

/// Errors of usage publication.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    #[error("transient publish error: {0}")]
    Transient(String),
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Model policy plugin (`mini-chat-model-policy-plugin`).
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError>;

    /// Immutable policy snapshot of a version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError>;

    /// Per-user credit limits for a version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError>;

    /// Publishes a usage event (at-least-once; consumers dedupe).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check; not called by the gear in P1.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, PolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Delivers one audit event.
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError>;
}
