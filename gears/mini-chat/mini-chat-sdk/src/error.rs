//! Plugin-SPI error enums (kept separate from the canonical error model).

use thiserror::Error;

/// Errors of the policy plugin's read methods.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("policy version {0} not found")]
    VersionNotFound(u64),
    #[error("user not found")]
    UserNotFound,
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("policy plugin error: {0}")]
    Internal(String),
}

/// Errors of `publish_usage`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// Retryable failure.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Non-retryable failure; the message is dead-lettered.
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors of the audit plugin.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("permanent audit error: {0}")]
    Permanent(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
}
