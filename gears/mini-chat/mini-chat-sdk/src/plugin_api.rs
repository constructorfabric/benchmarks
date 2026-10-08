//! Plugin traits implemented by mini-chat plugins and resolved by the gear
//! through types-registry + scoped `ClientHub` registration.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{
    MiniChatAuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus,
    UserLimits,
};

/// Model policy plugin: delivers versioned policy snapshots (model catalog,
/// kill switches), per-user credit limits, and receives usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable policy snapshot of the given version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user credit allocation for the given policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish a settled usage event (at-least-once; consumers dedupe by
    /// `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License status of a user. Not called by the gear in P1.
    async fn check_user_license(
        &self,
        _tenant_id: Uuid,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events delivered by the audit outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
