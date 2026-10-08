//! Errors of the mini-chat plugin contracts.

use thiserror::Error;

/// Errors returned by a model-policy plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version or snapshot does not exist.
    #[error("policy not found: {0}")]
    NotFound(String),
    /// The plugin could not serve the request right now.
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure; the outbox redelivers the event.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure; the event is dead-lettered.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors returned by an audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    /// Retryable failure.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// The plugin call timed out (transient).
    #[error("audit plugin timeout")]
    PluginTimeout,
    /// Permanent failure; the event is dead-lettered.
    #[error("permanent audit error: {0}")]
    Permanent(String),
}
