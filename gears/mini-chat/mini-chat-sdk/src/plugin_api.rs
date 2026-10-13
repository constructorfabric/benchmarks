//! Plugin traits resolved by the mini-chat gear through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::{TurnAuditEvent, TurnDeleteAuditEvent, TurnEditAuditEvent, TurnRetryAuditEvent};
use crate::error::{MiniChatAuditPluginError, MiniChatModelPolicyPluginError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Model policy plugin: policy snapshots (model catalog, kill switches), per-user limits,
/// and the sink for usage events.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
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

    /// Publish one settled usage event (at-least-once; consumers deduplicate by `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check. Not called by the gear in P1 (the routes use the base license feature).
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
    /// A finalized turn (`turn_completed` / `turn_failed`).
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError>;
    /// A retry of the latest turn.
    async fn emit_turn_retry_audit(
        &self,
        event: TurnRetryAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError>;
    /// An edit of the latest turn.
    async fn emit_turn_edit_audit(
        &self,
        event: TurnEditAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError>;
    /// A delete of the latest turn.
    async fn emit_turn_delete_audit(
        &self,
        event: TurnDeleteAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError>;
}
