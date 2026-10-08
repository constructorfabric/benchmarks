//! Plugin client contracts resolved by the gear through the types-registry
//! and the scoped `ClientHub`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::{TurnAuditEvent, TurnMutationAuditEvent};
use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Model-policy plugin: serves versioned policy snapshots (model catalog,
/// kill switches), per-user credit limits and receives usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current (monotonic) policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    /// Immutable policy snapshot of a version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user credit limits under a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Publish one usage event (at-least-once; consumers deduplicate by
    /// `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// User license check. Not called by the gear in P1.
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
    /// Deliver a turn finalization audit event.
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError>;

    /// Deliver a turn mutation (retry / edit / delete) audit event.
    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError>;
}
