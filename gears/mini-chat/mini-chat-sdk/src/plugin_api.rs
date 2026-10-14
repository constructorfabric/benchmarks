//! Plugin client traits.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{AuditPluginError, ModelPolicyPluginError, PublishError};
use crate::models::{AuditEvent, PolicySnapshot, UsageEvent, UserLicenseStatus, UserLimits};

/// Model-policy plugin: versioned policy snapshots, per-user limits and usage
/// publication. The plugin is "dumb": it returns configuration and receives
/// usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user's tenant.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError>;

    /// The immutable snapshot of `policy_version`.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError>;

    /// Per-user limits under `policy_version`. Kept apart from the shared snapshot.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError>;

    /// Publish one usage settlement event (at-least-once, idempotent downstream).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// User license check. The gear never calls it (the license gate is on the routes).
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events delivered by the `mini-chat.audit` outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Deliver one audit event.
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}

#[cfg(test)]
#[path = "plugin_api_tests.rs"]
mod plugin_api_tests;
