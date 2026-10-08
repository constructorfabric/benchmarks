//! Plugin error types.

/// Errors of the model policy plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MiniChatModelPolicyPluginError {
    #[error("policy version {0} not found")]
    VersionNotFound(u64),
    #[error("transient policy plugin error: {0}")]
    Transient(String),
    #[error("policy plugin error: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Retryable failure (the outbox handler returns `Retry`).
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure (the outbox handler returns `Reject`).
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
    #[error("permanent audit error: {0}")]
    Permanent(String),
}

impl AuditPluginError {
    /// Whether the outbox handler should retry the delivery.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::PluginTimeout)
    }
}
