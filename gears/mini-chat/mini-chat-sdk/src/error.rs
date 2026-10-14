//! Plugin error types.

use thiserror::Error;

/// Failure of a model-policy plugin call that returns data.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelPolicyPluginError {
    /// The requested policy version is unknown to the plugin.
    #[error("policy version {0} not found")]
    PolicyVersionNotFound(u64),
    /// The plugin backend cannot answer right now.
    #[error("policy backend unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Failure of `publish_usage`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    /// Retryable failure; the outbox handler returns `Retry`.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure; the outbox handler returns `Reject`.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Failure of an audit plugin delivery.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditPluginError {
    /// Retryable failure; the audit handler returns `Retry`.
    #[error("transient audit plugin error: {0}")]
    Transient(String),
    /// The plugin did not answer in time; transient.
    #[error("audit plugin timed out")]
    PluginTimeout,
    /// Permanent failure; the audit handler returns `Reject`.
    #[error("permanent audit plugin error: {0}")]
    Permanent(String),
}
