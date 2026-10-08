//! Plugin API traits implemented by mini-chat plugins and resolved by the
//! gear through types-registry + scoped `ClientHub`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::error::{AuditPluginError, PolicyError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Model policy plugin: policy snapshots (model catalog, kill switches),
/// per-user limits and usage ingestion.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyError>;

    /// Immutable snapshot for a policy version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyError>;

    /// Per-user limits bound to a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyError>;

    /// Publish a usage settlement event (at-least-once; consumers dedupe).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// User license check. Reserved: the gear never calls it.
    async fn check_user_license(&self, _user_id: Uuid) -> Result<UserLicenseStatus, PolicyError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events delivered by the audit outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
