//! Plugin SPIs resolved by the mini-chat gear through types-registry.

use async_trait::async_trait;
use uuid::Uuid;

use crate::models::{
    PolicySnapshot, PolicyVersionInfo, TurnAuditEvent, TurnMutationAuditEvent, UsageEvent,
    UserLicenseStatus, UserLimits,
};

/// Errors returned by the model policy plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MiniChatModelPolicyPluginError {
    #[error("policy not found: {0}")]
    NotFound(String),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PublishError {
    #[error("transient publish error: {0}")]
    Transient(String),
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Model policy plugin (`mini-chat-model-policy-plugin`): policy snapshots, user limits, usage sink.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError>;

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError>;

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError>;

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError>;

    /// Never called by the gear in P1 (ADR-0008).
    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// Errors returned by the audit plugin.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
    #[error("permanent audit error: {0}")]
    Permanent(String),
}

/// Audit plugin (`MiniChatAuditPluginClientV1`).
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError>;

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError>;
}
