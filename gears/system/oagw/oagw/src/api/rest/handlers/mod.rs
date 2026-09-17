//! REST handlers.
//!
//! Handlers are thin: they extract the tenant, delegate to a domain service or
//! the proxy engine, and shape the response. All error mapping happens in
//! [`crate::api::rest::error`].

use crate::api::rest::error::ApiError;
use crate::domain::error::DomainError;

/// Deserialize a JSON request body, reporting a 400 when it is absent or
/// malformed.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(DomainError::Validation(
            "a JSON request body is required".into(),
        )
        .into());
    }
    serde_json::from_slice(body)
        .map_err(|err| DomainError::Validation(format!("invalid JSON body: {err}")).into())
}

/// Upstream management.
pub mod upstreams;
/// Route management.
pub mod routes;
/// Tenant plugin management.
pub mod plugins;
/// Data-plane proxy endpoints.
pub mod proxy;
