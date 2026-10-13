//! Plugin API traits of the mini-chat gear.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::MiniChatAuditEvent;
use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Model policy plugin: model catalog, kill switches, user limits and usage
/// settlement. Registered in `ClientHub` under the plugin's GTS instance id.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version of the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable policy snapshot for `(user_id, policy_version)`.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Credit limits of the user for `policy_version`.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish a usage settlement event (at-least-once delivery).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// Whether the user holds an active license. Defaults to inactive.
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
    /// Deliver one audit event.
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
