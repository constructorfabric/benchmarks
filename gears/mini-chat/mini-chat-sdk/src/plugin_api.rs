//! Plugin API traits of the mini-chat gear.
//!
//! Plugins register an implementation as a scoped `ClientHub` client under
//! their GTS instance id; the mini-chat gear selects the instance by vendor
//! through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{
    AuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits,
};

/// Model policy plugin (`mini-chat-model-policy-plugin`): policy snapshots,
/// per-user limits and usage publication. Expected to be "dumb": it returns
/// configuration and receives usage events.
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

    /// Per-user credit limits bound to the given policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish one usage event (at-least-once; consumers deduplicate by
    /// `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License status of a user. Not called by the gear in P1 (the license
    /// gate is enforced on the routes).
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin (`mini-chat-audit-plugin`): receives turn and turn-mutation
/// audit events from the `mini-chat.audit` outbox queue.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Deliver one audit event.
    async fn emit(&self, event: AuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
