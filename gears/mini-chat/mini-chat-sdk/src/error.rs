//! Plugin contract errors.

use thiserror::Error;

/// Errors of the model-policy plugin read operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version or user allocation is unknown.
    #[error("not found: {0}")]
    NotFound(String),
    /// The plugin is temporarily unavailable.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure (the outbox redelivers the event).
    #[error("transient publish failure: {0}")]
    Transient(String),
    /// Permanent failure (the event is dead-lettered).
    #[error("permanent publish failure: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    /// Retryable failure.
    #[error("transient audit failure: {0}")]
    Transient(String),
    /// The plugin call timed out (treated as transient).
    #[error("audit plugin timed out")]
    PluginTimeout,
    /// Permanent failure (the event is dead-lettered).
    #[error("permanent audit failure: {0}")]
    Permanent(String),
}
