//! Transport-agnostic errors of the mini-chat plugin contracts.

use thiserror::Error;

/// Errors returned by the model policy plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version or user allocation does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The plugin backend is temporarily unavailable.
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    /// Unexpected plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure (the outbox redelivers the event).
    #[error("transient publish failure: {0}")]
    Transient(String),
    /// Permanent failure (the event is dead-lettered).
    #[error("permanent publish failure: {0}")]
    Permanent(String),
}

/// Errors returned by the audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    /// Retryable failure.
    #[error("transient audit failure: {0}")]
    Transient(String),
    /// The plugin call timed out (transient).
    #[error("audit plugin timeout")]
    PluginTimeout,
    /// Permanent failure (the event is dead-lettered).
    #[error("permanent audit failure: {0}")]
    Permanent(String),
}
