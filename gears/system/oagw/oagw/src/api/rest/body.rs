//! JSON request-body binding for the OAGW management surface.
//!
//! [`JsonBody`] replaces axum's built-in [`axum::Json`] extractor so that a
//! body which fails to deserialize is reported as an OAGW problem document
//! with `400 ValidationError` (DESIGN §3.3 error catalog) instead of axum's
//! plain-text `422`, and an oversized body as `413 PayloadTooLarge`.

use axum::extract::FromRequest;

use crate::api::error::OagwError;
use crate::config::OagwConfig;

/// Deserialized JSON request body.
#[derive(Debug, Clone)]
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = OagwError;

    async fn from_request(
        request: axum::extract::Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let limit = request
            .extensions()
            .get::<std::sync::Arc<OagwConfig>>()
            .map_or(crate::config::DEFAULT_BODY_LIMIT_BYTES, |config| {
                config.max_body_size_bytes
            });
        let content_length = request
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if content_length.is_some_and(|length| length > limit) {
            return Err(OagwError::from(
                crate::domain::error::DomainError::PayloadTooLarge { limit },
            ));
        }
        let body = request.into_body();
        let bytes = axum::body::to_bytes(body, limit as usize)
            .await
            .map_err(|_| {
                OagwError::from(crate::domain::error::DomainError::PayloadTooLarge { limit })
            })?;
        let value: T = serde_json::from_slice(&bytes).map_err(|error| {
            OagwError::from(crate::domain::error::DomainError::validation(format!(
                "request body is not a valid document: {error}"
            )))
        })?;
        Ok(Self(value))
    }
}
