//! Error types for the `AuthZ` resolver gear.

use thiserror::Error;

/// Errors that can occur when using the `AuthZ` resolver API.
///
/// These represent infrastructure/transport failures only.
/// Access denial is expressed via `EvaluationResponse.decision == false`,
/// not as an error variant.
#[derive(Debug, Error)]
pub enum AuthZResolverError {
    /// No `AuthZ` plugin is available to handle the request.
    #[error("no plugin available")]
    NoPluginAvailable,

    /// The plugin is not available yet.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}
