//! Plugin contracts consumed by the mini-chat gear.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{AuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{
    AuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits,
};

/// Model policy plugin: policy snapshots, user limits and usage publication.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable snapshot for `policy_version`.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user credit allocation for `policy_version`.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish one usage event (at-least-once; consumers dedupe by `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check. Not called by the gear in P1 (ADR-0008).
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events delivered by the audit outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
