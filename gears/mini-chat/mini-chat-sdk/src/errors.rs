//! Errors returned by mini-chat plugins.

use thiserror::Error;

/// Error of a model-policy plugin read operation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyPluginError {
    /// The requested policy version is not known to the plugin.
    #[error("policy version {policy_version} not found")]
    VersionNotFound { policy_version: u64 },
    /// The plugin backend is temporarily unavailable.
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Error of `publish_usage`. `Transient` is retried by the outbox, `Permanent`
/// dead-letters the event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    #[error("transient usage publish error: {0}")]
    Transient(String),
    #[error("permanent usage publish error: {0}")]
    Permanent(String),
}

/// Error of an audit plugin `emit`. `Transient` and `PluginTimeout` are
/// retried by the outbox, `Permanent` dead-letters the event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditPluginError {
    #[error("transient audit plugin error: {0}")]
    Transient(String),
    #[error("permanent audit plugin error: {0}")]
    Permanent(String),
    #[error("audit plugin call timed out")]
    PluginTimeout,
}
