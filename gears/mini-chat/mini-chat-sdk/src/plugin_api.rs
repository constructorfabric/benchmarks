//! Plugin client traits. Plugin gears register implementations as scoped
//! clients in the `ClientHub` under their GTS instance id.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{AuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{
    AuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits,
};

/// Model policy plugin: serves the policy snapshot (model catalog, kill
/// switches), per-user limits, and receives usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable snapshot of a policy version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user credit limits for a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publishes a usage event (at-least-once; consumers deduplicate).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check; not called by the gear in P1.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives structured audit events.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Delivers one audit event.
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
