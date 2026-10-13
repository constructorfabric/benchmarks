//! Plugin contracts of the mini-chat gear.
//!
//! The model-policy plugin serves policy snapshots and user limits and receives
//! usage events; the audit plugin receives audit events.

use async_trait::async_trait;
use uuid::Uuid;

use crate::models::{
    MiniChatAuditEvent, PolicySnapshot, PolicyVersionInfo, UsageEvent, UserLicenseStatus,
    UserLimits,
};

/// Error of the read operations of the model-policy plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyPluginError {
    /// The requested policy version or user is unknown.
    #[error("policy not found: {0}")]
    NotFound(String),
    /// The plugin failed internally.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Error of `publish_usage`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Retry later (the outbox handler returns `Retry`).
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Never retry (the outbox handler returns `Reject`).
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Error of the audit plugin `emit`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditPluginError {
    /// Retry later.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// The plugin did not answer in time (transient).
    #[error("audit plugin timeout")]
    PluginTimeout,
    /// Never retry.
    #[error("permanent audit error: {0}")]
    Permanent(String),
}

/// Model policy plugin.
#[async_trait]
pub trait MiniChatModelPolicyPluginClientV1: Send + Sync {
    /// Returns the current policy version of the user.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyPluginError`] when the plugin cannot answer.
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError>;

    /// Returns the immutable snapshot of `(user_id, policy_version)`.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyPluginError`] when the plugin cannot answer.
    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError>;

    /// Returns the user limits of `(user_id, policy_version)` (no tenant).
    ///
    /// # Errors
    ///
    /// Returns [`PolicyPluginError`] when the plugin cannot answer.
    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError>;

    /// Publishes a usage event.
    ///
    /// # Errors
    ///
    /// Returns [`PublishError::Transient`] to retry and
    /// [`PublishError::Permanent`] to dead-letter the message.
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError>;

    /// Returns the license status of the user. The default body reports an
    /// inactive license.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyPluginError`] when the plugin cannot answer.
    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, PolicyPluginError> {
        let _ = user_id;
        Ok(UserLicenseStatus { active: false })
    }
}

/// Audit plugin.
#[async_trait]
pub trait MiniChatAuditPluginClientV1: Send + Sync {
    /// Emits one audit event.
    ///
    /// # Errors
    ///
    /// Returns [`AuditPluginError`] (`Transient`, `PluginTimeout`, `Permanent`).
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError>;
}

#[cfg(test)]
#[path = "plugin_api_tests.rs"]
mod tests;
