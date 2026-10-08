//! Error types of the mini-chat plugin contracts.

use thiserror::Error;

/// Errors returned by a model policy plugin.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version is unknown to the plugin.
    #[error("policy version {0} not found")]
    PolicyVersionNotFound(u64),
    /// The plugin is temporarily unavailable.
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin internal error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// Transient failure; the usage outbox handler retries the event.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure; the usage outbox handler dead-letters the event.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors returned by an audit plugin.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    /// Transient failure; the audit outbox handler retries the event.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// The plugin call timed out (transient).
    #[error("audit plugin timeout")]
    PluginTimeout,
    /// Permanent failure; the audit outbox handler dead-letters the event.
    #[error("permanent audit error: {0}")]
    Permanent(String),
}
