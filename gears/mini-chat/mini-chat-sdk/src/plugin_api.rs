//! Plugin client traits of the mini-chat gear.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{AuditPluginError, PolicyPluginError, PublishError};
use crate::models::{
    AuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits,
};

/// Model policy plugin: policy distribution, user limits and usage ingestion.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError>;

    /// Immutable policy snapshot for `(user_id, policy_version)`.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, PolicyPluginError>;

    /// Per-user credit limits for `(user_id, policy_version)`.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, PolicyPluginError>;

    /// Deliver a settled usage event (at-least-once).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// Whether the user holds an active license. Defaults to inactive.
    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, PolicyPluginError> {
        let _ = user_id;
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Emit one audit event.
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
