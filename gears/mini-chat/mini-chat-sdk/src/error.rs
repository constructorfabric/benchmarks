//! Error types of the mini-chat plugin contracts.

/// Errors returned by a model policy plugin.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MiniChatModelPolicyPluginError {
    /// The requested policy version or user allocation does not exist.
    #[error("policy data not found: {0}")]
    NotFound(String),
    /// The policy backend is temporarily unavailable.
    #[error("policy backend unavailable: {0}")]
    Unavailable(String),
    /// Any other plugin failure.
    #[error("policy plugin error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure: the outbox delivers the event again later.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Non-retryable failure: the event is dead-lettered.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors returned by an audit plugin.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MiniChatAuditPluginError {
    /// Retryable failure.
    #[error("transient audit error: {0}")]
    Transient(String),
    /// Non-retryable failure: the event is dead-lettered.
    #[error("permanent audit error: {0}")]
    Permanent(String),
    /// The plugin call timed out (treated as transient).
    #[error("audit plugin timed out")]
    PluginTimeout,
}
