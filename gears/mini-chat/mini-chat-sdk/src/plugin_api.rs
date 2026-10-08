//! Plugin client contracts resolved through the types registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::events::{MiniChatAuditEvent, UsageEvent};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};

/// Model-policy plugin: policy snapshots (catalog, kill switches), user limits
/// and usage publication (DESIGN §5.2).
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
    ///
    /// # Errors
    /// Plugin failures.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable snapshot of `policy_version`.
    ///
    /// # Errors
    /// Unknown version or plugin failure.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user limits under `policy_version`.
    ///
    /// # Errors
    /// Unknown version or plugin failure.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish one usage event (at-least-once; consumers deduplicate).
    ///
    /// # Errors
    /// Transient or permanent publish failure.
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check (not called by the gear in P1).
    ///
    /// # Errors
    /// Plugin failures.
    async fn check_user_license(
        &self,
        _tenant_id: Uuid,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events from the `mini-chat.audit` outbox queue.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Deliver one audit event.
    ///
    /// # Errors
    /// Transient / timeout / permanent failures.
    async fn emit_audit_event(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError>;
}
