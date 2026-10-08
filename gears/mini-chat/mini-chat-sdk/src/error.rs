//! Plugin error types.

use thiserror::Error;

/// Error returned by the model-policy plugin read operations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("policy not found: {0}")]
    NotFound(String),
    #[error("policy source unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Error returned by `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure (the outbox handler returns `Retry`).
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure (the outbox handler returns `Reject`).
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Error returned by the audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AuditPluginError {
    /// Retryable failure.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// Permanent failure (event is dead-lettered).
    #[error("permanent audit error: {0}")]
    Permanent(String),
    /// The plugin did not answer in time (transient).
    #[error("audit plugin timeout")]
    PluginTimeout,
}
