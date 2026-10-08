//! Error types of the plugin APIs.

use thiserror::Error;

/// Errors returned by the model policy plugin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyPluginError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("policy plugin unavailable: {0}")]
    Unavailable(String),
    #[error("internal policy plugin error: {0}")]
    Internal(String),
}

/// Errors returned when publishing a usage event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    /// The delivery may succeed if retried.
    #[error("transient publish failure: {0}")]
    Transient(String),
    /// The event can never be accepted; retrying is pointless.
    #[error("permanent publish failure: {0}")]
    Permanent(String),
}

/// Errors returned by the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditPluginError {
    #[error("transient audit failure: {0}")]
    Transient(String),
    #[error("permanent audit failure: {0}")]
    Permanent(String),
    #[error("audit plugin timed out")]
    PluginTimeout,
}
