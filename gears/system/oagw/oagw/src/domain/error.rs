//! OAGW problem errors.
//!
//! The gear emits RFC 9457 `application/problem+json` bodies whose `type` is a
//! GTS identifier (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`) and whose OAGW
//! specific extension fields sit flat on the object — not nested under a
//! `context` key, which is how the platform's canonical errors render extras.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, json};

use crate::gts;

/// A gateway-generated problem.
#[derive(Debug, Clone)]
pub struct OagwError {
    status: u16,
    type_id: &'static str,
    title: String,
    detail: String,
    instance: Option<String>,
    extensions: BTreeMap<String, serde_json::Value>,
}

impl OagwError {
    fn new(status: u16, type_id: &'static str, title: &str, detail: impl Into<String>) -> Self {
        Self {
            status,
            type_id,
            title: title.to_owned(),
            detail: detail.into(),
            instance: None,
            extensions: BTreeMap::new(),
        }
    }

    /// Attaches an OAGW extension field.
    #[must_use]
    pub fn with_extension(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// Attaches the `instance` member.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// 400 — request validation failed.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(
            400,
            gts::errors::VALIDATION_ERROR,
            "Validation failed",
            detail,
        )
    }

    /// 400 — route-level validation failure.
    #[must_use]
    pub fn route_error(detail: impl Into<String>) -> Self {
        Self::validation(detail)
    }

    /// 404 — a management resource does not exist.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(404, gts::errors::ROUTE_NOT_FOUND, "Not found", detail)
            .with_extension("resource", "route")
    }

    /// 404 — a management resource does not exist (upstream flavor).
    #[must_use]
    pub fn upstream_not_found(detail: impl Into<String>) -> Self {
        Self::new(404, gts::errors::ROUTE_NOT_FOUND, "Not found", detail)
            .with_extension("resource", "upstream")
    }

    /// 404 — a management resource does not exist (plugin flavor).
    #[must_use]
    pub fn plugin_not_found_api(detail: impl Into<String>) -> Self {
        Self::new(404, gts::errors::ROUTE_NOT_FOUND, "Not found", detail)
            .with_extension("resource", "plugin")
    }

    /// 405 — method not supported on a management resource.
    #[must_use]
    pub fn method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            405,
            gts::errors::VALIDATION_ERROR,
            "Method not allowed",
            detail,
        )
    }

    /// 409 — uniqueness conflict.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(409, gts::errors::VALIDATION_ERROR, "Conflict", detail)
    }

    /// 409 — plugin still bound to an upstream or route.
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(409, gts::errors::PLUGIN_IN_USE, "Plugin in use", detail)
    }

    /// 400 — `X-OAGW-Target-Host` required but absent.
    #[must_use]
    pub fn missing_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            400,
            gts::errors::ROUTING_MISSING_TARGET_HOST,
            "Missing target host",
            detail,
        )
    }

    /// 400 — `X-OAGW-Target-Host` malformed.
    #[must_use]
    pub fn invalid_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            400,
            gts::errors::ROUTING_INVALID_TARGET_HOST,
            "Invalid target host",
            detail,
        )
    }

    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    #[must_use]
    pub fn unknown_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            400,
            gts::errors::ROUTING_UNKNOWN_TARGET_HOST,
            "Unknown target host",
            detail,
        )
    }

    /// 400 — a route does not match the request.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(404, gts::errors::ROUTE_NOT_FOUND, "Route not found", detail)
    }

    /// 401 — authentication to the upstream failed.
    #[must_use]
    pub fn auth_failed(detail: impl Into<String>) -> Self {
        Self::new(
            401,
            gts::errors::AUTH_FAILED,
            "Authentication failed",
            detail,
        )
    }

    /// 413 — request payload exceeds the limit.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(
            413,
            gts::errors::PAYLOAD_TOO_LARGE,
            "Payload too large",
            detail,
        )
    }

    /// 429 — rate limit exhausted.
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(
            429,
            gts::errors::RATE_LIMIT_EXCEEDED,
            "Rate limit exceeded",
            detail,
        )
    }

    /// 500 — referenced secret missing.
    #[must_use]
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            500,
            gts::errors::SECRET_NOT_FOUND,
            "Secret not found",
            detail,
        )
    }

    /// 502 — protocol-level failure.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(502, gts::errors::PROTOCOL_ERROR, "Protocol error", detail)
    }

    /// 502 — the upstream service failed.
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        Self::new(
            502,
            gts::errors::DOWNSTREAM_ERROR,
            "Upstream service error",
            detail,
        )
    }

    /// 502 — a streamed connection aborted mid-flight.
    #[must_use]
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        Self::new(502, gts::errors::STREAM_ABORTED, "Stream aborted", detail)
    }

    /// 503 — the upstream link is unavailable.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            503,
            gts::errors::LINK_UNAVAILABLE,
            "Link unavailable",
            detail,
        )
    }

    /// 503 — the circuit breaker is open.
    #[must_use]
    pub fn circuit_breaker_open(detail: impl Into<String>) -> Self {
        Self::new(
            503,
            gts::errors::CIRCUIT_BREAKER_OPEN,
            "Circuit breaker open",
            detail,
        )
    }

    /// 503 — a bound plugin could not be resolved.
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(
            503,
            gts::errors::PLUGIN_NOT_FOUND,
            "Plugin not found",
            detail,
        )
    }

    /// 504 — connection could not be established in time.
    #[must_use]
    pub fn connection_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            504,
            gts::errors::TIMEOUT_CONNECTION,
            "Connection timeout",
            detail,
        )
    }

    /// 504 — the upstream did not produce a response in time.
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(504, gts::errors::TIMEOUT_REQUEST, "Request timeout", detail)
    }

    /// 504 — the upstream stopped sending data mid-response.
    #[must_use]
    pub fn idle_timeout(detail: impl Into<String>) -> Self {
        Self::new(504, gts::errors::TIMEOUT_IDLE, "Idle timeout", detail)
    }

    /// 403 — CORS origin rejected.
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            403,
            gts::errors::CORS_ORIGIN_NOT_ALLOWED,
            "Origin not allowed",
            detail,
        )
    }

    /// 403 — CORS method rejected.
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            403,
            gts::errors::CORS_METHOD_NOT_ALLOWED,
            "Method not allowed",
            detail,
        )
    }

    /// 400 — the request body is not valid JSON.
    #[must_use]
    pub fn malformed_body(detail: impl Into<String>) -> Self {
        Self::validation(detail)
    }

    /// Builds a problem from an explicit status and GTS type identifier.
    ///
    /// Used where a plugin names the problem it wants surfaced.
    #[must_use]
    pub fn custom(status: u16, type_id: &'static str, detail: impl Into<String>) -> Self {
        let title = crate::proxy::status_title(status);
        Self::new(status, type_id, title, detail)
    }

    /// HTTP status code.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// GTS type identifier.
    #[must_use]
    pub fn type_id(&self) -> &'static str {
        self.type_id
    }

    /// The `Retry-After`/`retry_after_seconds` hint, when the error carries one.
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        self.extensions
            .get("retry_after_seconds")
            .and_then(serde_json::Value::as_u64)
    }

    /// Renders the problem body as JSON.
    #[must_use]
    pub fn to_body(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut body = Map::new();
        body.insert("type".into(), json!(self.type_id));
        body.insert("title".into(), json!(self.title));
        body.insert("status".into(), json!(self.status));
        body.insert("detail".into(), json!(self.detail));
        if let Some(instance) = &self.instance {
            body.insert("instance".into(), json!(instance));
        }
        for (k, v) in &self.extensions {
            body.insert(k.clone(), v.clone());
        }
        body
    }

    /// Builds the axum response for this problem.
    #[must_use]
    pub fn to_response(&self) -> Response {
        let body = self.to_body();
        let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned());

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        headers.insert("x-oagw-error-source", HeaderValue::from_static("gateway"));
        if let Some(retry_after) = self.retry_after()
            && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
        {
            headers.insert(header::RETRY_AFTER, value);
        }

        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, headers, payload).into_response()
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.title, self.status, self.detail)
    }
}

impl std::error::Error for OagwError {}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        self.to_response()
    }
}

impl From<OagwError> for Response {
    fn from(err: OagwError) -> Self {
        err.to_response()
    }
}

/// Extracts a flat OAGW extension field from a serde_json object.
#[must_use]
pub fn extension(value: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    value.get(key).cloned()
}

/// Collects a JSON object into a flat extension map.
#[must_use]
pub fn extensions_from(value: Option<&serde_json::Value>) -> BTreeMap<String, serde_json::Value> {
    match value {
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "type" | "title" | "status" | "detail" | "instance"
                )
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => BTreeMap::new(),
    }
}
