//! Transport-agnostic error types for the mini-chat SDK.

use thiserror::Error;

/// Errors of the model policy plugin client (`MiniChatModelPolicyPluginClientV1`).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ModelPolicyError {
    /// The requested policy version or model is unknown to the plugin.
    #[error("policy not found: {0}")]
    NotFound(String),
    /// The plugin failed transiently; the caller may retry.
    #[error("transient policy plugin error: {0}")]
    Transient(String),
    /// The plugin failed permanently; retrying is pointless.
    #[error("permanent policy plugin error: {0}")]
    Permanent(String),
}

/// Outcome classification of a `publish_usage` call.
///
/// The outbox usage handler maps these to outbox outcomes:
/// `Transient`/`PluginTimeout` -> retry, `Permanent` -> reject (dead letter).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// Transient failure; the outbox handler returns `Retry`.
    #[error("transient publish error: {0}")]
    Transient(String),
    /// Permanent failure; the outbox handler returns `Reject`.
    #[error("permanent publish error: {0}")]
    Permanent(String),
    /// The plugin call timed out; treated as transient.
    #[error("plugin call timed out: {0}")]
    PluginTimeout(String),
}

/// Errors of the audit plugin client (`MiniChatAuditPluginClientV1`).
///
/// The outbox audit handler maps these to outbox outcomes:
/// `Transient`/`PluginTimeout` -> `Retry`, `Permanent` -> `Reject`.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum AuditError {
    /// Transient failure; the outbox handler returns `Retry`.
    #[error("transient audit plugin error: {0}")]
    Transient(String),
    /// Permanent failure; the outbox handler returns `Reject`.
    #[error("permanent audit plugin error: {0}")]
    Permanent(String),
    /// The plugin call timed out (30 s); treated as transient.
    #[error("audit plugin call timed out: {0}")]
    PluginTimeout(String),
}
