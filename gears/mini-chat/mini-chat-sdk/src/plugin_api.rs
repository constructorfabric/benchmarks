//! Plugin contracts resolved by the gear through types-registry + ClientHub.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::error::{AuditPluginError, PolicyError, PublishError};
use crate::models::{PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus, UserLimits};

/// Model policy plugin (`mini-chat-model-policy-plugin`): versioned policy
/// snapshots, per-user limits and usage publication.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
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

    /// Per-user credit allocation for a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyError>;

    /// Publish a settled usage event (at-least-once; consumers dedupe by
    /// `dedupe_key`).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check (never called by the gear, ADR-0008).
    async fn check_user_license(&self, _user_id: Uuid) -> Result<UserLicenseStatus, PolicyError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin: receives audit events from the audit outbox handler.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError>;
}
