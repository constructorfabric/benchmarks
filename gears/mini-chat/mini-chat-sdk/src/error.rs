//! Error types of the mini-chat plugin contracts.

use thiserror::Error;

/// Error returned by the model policy plugin read operations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version is not known to the plugin.
    #[error("policy version {0} not found")]
    PolicyVersionNotFound(u64),
    /// The plugin backend is temporarily unavailable.
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Error returned by `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Transient failure: the usage handler retries the delivery.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure: the event is dead-lettered.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Error returned by the audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AuditPluginError {
    /// Transient failure: the audit handler retries.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// The plugin call timed out (transient).
    #[error("audit plugin timed out")]
    PluginTimeout,
    /// Permanent failure: the event is dead-lettered.
    #[error("permanent audit error: {0}")]
    Permanent(String),
}
