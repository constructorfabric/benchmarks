//! Plugin contracts resolved by the gear through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::audit::{TurnAuditEvent, TurnMutationAuditEvent};
use crate::models::{PolicySnapshot, UserLicenseStatus, UserLimits};
use crate::usage::UsageEvent;

/// Errors of policy retrieval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MiniChatModelPolicyPluginError {
    #[error("policy version {0} not found")]
    VersionNotFound(u64),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors of usage publication.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Retried by the outbox.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Dead-lettered.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Model policy plugin (`mini-chat-model-policy-plugin`).
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Current policy version for the user.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError>;

    /// Policy snapshot of a given version.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    /// Per-user limits under a policy version.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    /// Receives one usage event (at-least-once).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// License check; never called by the gear in P1.
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Errors of audit delivery.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MiniChatAuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("permanent audit error: {0}")]
    Permanent(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
}

/// Audit plugin.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError>;

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), MiniChatAuditPluginError>;
}
