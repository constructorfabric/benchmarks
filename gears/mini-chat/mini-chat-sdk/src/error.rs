//! Error types of the plugin contracts.

use thiserror::Error;

/// Errors returned by the model policy plugin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MiniChatModelPolicyPluginError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("transient error: {0}")]
    Transient(String),
    #[error("permanent error: {0}")]
    Permanent(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    #[error("transient publish error: {0}")]
    Transient(String),
    #[error("permanent publish error: {0}")]
    Permanent(String),
}

/// Errors returned by the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MiniChatAuditPluginError {
    #[error("transient audit error: {0}")]
    Transient(String),
    #[error("permanent audit error: {0}")]
    Permanent(String),
    #[error("audit plugin timeout")]
    PluginTimeout,
}
