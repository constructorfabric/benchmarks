//! Error types of the plugin contracts.

use thiserror::Error;

/// Errors returned by the policy-reading methods of a model-policy plugin.
#[derive(Debug, Clone, Error)]
pub enum ModelPolicyPluginError {
    /// The requested policy version is not known to the plugin.
    #[error("policy version {policy_version} not found")]
    PolicyVersionNotFound {
        /// The version that was asked for.
        policy_version: i64,
    },
    /// The plugin could not serve the request right now; the call may be retried.
    #[error("model policy plugin unavailable: {0}")]
    Unavailable(String),
    /// The plugin failed in a way a retry will not fix.
    #[error("model policy plugin error: {0}")]
    Internal(String),
}

/// Errors returned by `publish_usage`.
///
/// The usage outbox handler retries a `Transient` failure and dead-letters a `Permanent` one.
#[derive(Debug, Clone, Error)]
pub enum PublishError {
    /// The event could not be delivered now; delivery may be retried.
    #[error("transient usage publication failure: {0}")]
    Transient(String),
    /// The event can never be delivered (for example it is rejected as malformed).
    #[error("permanent usage publication failure: {0}")]
    Permanent(String),
}

/// Errors returned by the audit plugin.
///
/// The audit outbox handler retries `Transient` and `PluginTimeout` failures and dead-letters
/// `Permanent` ones.
#[derive(Debug, Clone, Error)]
pub enum AuditPluginError {
    /// The event could not be recorded now; delivery may be retried.
    #[error("transient audit failure: {0}")]
    Transient(String),
    /// The plugin did not answer in time (raised by the gear, not by plugins).
    #[error("audit plugin timed out")]
    PluginTimeout,
    /// The event can never be recorded.
    #[error("permanent audit failure: {0}")]
    Permanent(String),
}
