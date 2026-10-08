//! Plugin error types.

use thiserror::Error;

/// Errors of the model-policy plugin's read operations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    #[error("policy version {0} not found")]
    PolicyVersionNotFound(u64),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retry later (outbox `Retry`).
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Never succeeds (outbox `Reject`).
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
    #[error("permanent audit error: {0}")]
    Permanent(String),
}

impl MiniChatAuditPluginError {
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::PluginTimeout)
    }
}
