//! Plugin contracts resolved by mini-chat through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{
    MiniChatAuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus,
    UserLimits,
};

/// Model policy plugin: serves the versioned policy snapshot (model catalog,
/// kill switches), per-user credit limits, and receives usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable snapshot for a policy version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Credit limits of the user under a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publishes a usage settlement event (at-least-once; consumers deduplicate by `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check. Not called by mini-chat in P1 (ADR-0008).
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives turn and turn-mutation audit events.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Delivers one audit event.
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
