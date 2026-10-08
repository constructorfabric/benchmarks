//! Plugin API traits implemented by model-policy and audit plugins.
//!
//! Plugins register an implementation as a scoped `ClientHub` client under
//! their GTS instance id; the mini-chat gear resolves the instance through
//! types-registry (`choose_plugin_instance` by `vendor`).

use async_trait::async_trait;
use uuid::Uuid;

use crate::errors::{AuditPluginError, PolicyPluginError, PublishError};
use crate::models::{
    MiniChatAuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus,
    UserLimits,
};

/// Model policy plugin (D§5.2, Appendix A): versioned policy snapshots,
/// per-user limits and usage ingestion.
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
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError>;

    /// Per-user credit limits for `(user_id, policy_version)`.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError>;

    /// Receive a settled usage event (at-least-once; must be idempotent on
    /// `dedupe_key`).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// License check. Exists for contract completeness; the gear never calls it.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, PolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin (ADR-0009): receives turn and turn-mutation audit events.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Deliver one audit event.
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError>;
}
