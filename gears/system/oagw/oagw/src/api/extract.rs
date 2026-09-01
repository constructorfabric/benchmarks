// Created: 2026-08-31 by Constructor Tech
//! Request extraction (DESIGN §3.6 "Resource Identification", §3.3 errors).
//!
//! `{id}` path parameters accept the bare UUID **and** the GTS form
//! `gts.cf.core.oagw.<type>.v1~<uuid>`; response bodies always carry the bare
//! UUID. JSON bodies go through [`JsonBody`], which reports malformed payloads
//! as RFC 9457 problems instead of axum's plain-text rejections.

use std::error::Error as _;

use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::error::OagwError;

/// Parse a bare UUID or a GTS identifier whose instance part is a UUID.
///
/// # Errors
/// 400 validation when neither spelling parses.
pub fn parse_resource_id(raw: &str) -> Result<Uuid, OagwError> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    let Some((_type_path, instance)) = raw.split_once('~') else {
        return Err(OagwError::validation(format!(
            "'{raw}' is not a resource id: expected a UUID or a GTS identifier"
        )));
    };
    Uuid::parse_str(instance).map_err(|_| {
        OagwError::validation(format!(
            "'{raw}' is not a resource id: the GTS instance part is not a UUID"
        ))
    })
}

/// Configured request-body ceiling of the management API, injected as a
/// request extension by [`crate::api::routes::register_routes`].
///
/// The value is `oagw.config.max_body_bytes` (DESIGN §2.2
/// `constraint-body-limit`); the hard limit in [`crate::config`] is only a
/// last-resort backstop for code paths that run without the extension.
#[derive(Debug, Clone, Copy)]
pub struct BodyLimit(pub u64);

/// JSON payload extractor that maps decode failures onto OAGW problems.
///
/// `axum::Json` answers malformed or wrongly-typed payloads with plain-text
/// 4xx/5xx rejections; the management API contract (DESIGN §3.3) requires
/// `application/problem+json` with the GTS error id instead. Bodies above the
/// configured limit become 413 `payload.too_large.v1`.
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = OagwError;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let content_type = request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("application/json") {
            return Err(OagwError::validation(
                "request body must be application/json",
            ));
        }
        let limit = request
            .extensions()
            .get::<BodyLimit>()
            .map_or(crate::config::MAX_BODY_BYTES_HARD_LIMIT, |limit| limit.0);
        let bytes = read_body(request.into_body(), limit).await?;
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(|error| OagwError::validation(format!("invalid request body: {error}")))
    }
}

/// Read the request body, keeping the failure inside the OAGW error surface.
///
/// `enforce_body_limit` (see [`crate::api::error`]) already rejects declared
/// `Content-Length` values above `limit`; this read catches streaming bodies
/// that never declared one.
async fn read_body(body: axum::body::Body, limit: u64) -> Result<Vec<u8>, OagwError> {
    use axum::body::to_bytes;
    let cap = usize::try_from(limit).unwrap_or(usize::MAX);
    to_bytes(body, cap)
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| {
            if is_length_limit(&error) {
                OagwError::payload_too_large(limit, u64::try_from(cap).unwrap_or(u64::MAX))
            } else {
                tracing::debug!(error = %error, "oagw request body read failed");
                OagwError::validation("request body could not be read")
            }
        })
}

/// Whether the body read failed because the size cap was hit.
fn is_length_limit(error: &axum::Error) -> bool {
    error
        .source()
        .is_some_and(<dyn std::error::Error + 'static>::is::<http_body_util::LengthLimitError>)
}

#[cfg(test)]
mod tests {
    use super::parse_resource_id;
    use crate::error::OagwErrorKind;

    #[test]
    fn accepts_a_bare_uuid() -> Result<(), crate::error::OagwError> {
        let id = uuid::Uuid::new_v4();
        assert_eq!(parse_resource_id(&id.to_string())?, id);
        Ok(())
    }

    #[test]
    fn accepts_a_gts_identifier() -> Result<(), crate::error::OagwError> {
        let id = uuid::Uuid::new_v4();
        let raw = format!("gts.cf.core.oagw.upstream.v1~{id}");
        assert_eq!(parse_resource_id(&raw)?, id);
        Ok(())
    }

    #[test]
    fn rejects_non_uuid_payloads() {
        assert_eq!(
            parse_resource_id("api.openai.com")
                .err()
                .map(|error| *error.kind()),
            Some(OagwErrorKind::Validation)
        );
        assert!(parse_resource_id("gts.cf.core.oagw.upstream.v1~not-a-uuid").is_err());
    }
}
