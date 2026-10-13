//! Plugin contracts resolved through the types registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::{TurnAuditEvent, TurnMutationAuditEvent};
use crate::error::{AuditPluginError, ModelPolicyPluginError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits};

/// Model-policy plugin: serves versioned policy snapshots and per-user limits and receives
/// usage events.
///
/// The plugin is expected to be "dumb": it returns configuration and receives usage. The gear
/// asks for the current version, the snapshot and the user limits on every preflight and for the
/// snapshot of the turn's applied version on every settlement.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// The current policy version for the user's tenant.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError>;

    /// The immutable snapshot of `policy_version`.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError>;

    /// The per-user limits derived under `policy_version`.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, ModelPolicyPluginError>;

    /// Publishes a settlement event (at-least-once; consumers deduplicate by `dedupe_key`).
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// Checks the user's license. The gear never calls this method: the license gate is
    /// enforced on the routes. The default body reports an inactive license.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives the audit events delivered by the audit outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Records a turn-finalization event.
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError>;

    /// Records a turn-mutation event (retry, edit or delete).
    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError>;
}
