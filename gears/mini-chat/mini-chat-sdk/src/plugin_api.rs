//! Plugin client traits and their error types.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::MiniChatAuditEvent;
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

#[derive(Debug, thiserror::Error)]
pub enum MiniChatModelPolicyPluginError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Failure to publish a usage event. `Transient` is retried by the outbox,
/// `Permanent` is dead-lettered.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("transient publish failure: {0}")]
    Transient(String),
    #[error("permanent publish failure: {0}")]
    Permanent(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AuditPluginError {
    #[error("transient audit failure: {0}")]
    Transient(String),
    #[error("permanent audit failure: {0}")]
    Permanent(String),
    #[error("audit plugin timeout")]
    PluginTimeout,
}

/// Model policy plugin contract: policy versions, snapshots, limits, usage.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// Defaults to "inactive" for plugins that do not implement licensing.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin contract.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError>;
}
