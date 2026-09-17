//! Gateway error responses (RFC 9457 problem details) for the data plane.
//!
//! Every gateway-generated error carries `X-OAGW-Error-Source: gateway` and
//! a GTS `type` from the DESIGN error table. Upstream responses are passed
//! through untouched (marked `X-OAGW-Error-Source: upstream`).

use std::collections::BTreeMap;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::domain::plugin::{AuthError, GuardError, TransformError};
use crate::gts;

/// OAGW problem-details body (RFC 9457 + OAGW extension fields).
#[derive(Debug, Serialize)]
pub struct Problem {
    /// GTS identifier for the error type.
    #[serde(rename = "type")]
    pub type_: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Occurrence-specific detail.
    pub detail: String,
    /// Structured error code (guard errors), optional.
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Retry guidance in seconds.
    #[serde(rename = "retryAfterSeconds", skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Request context (upstream/host/path), optional.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,
}

impl Problem {
    /// Build a gateway problem response with `X-OAGW-Error-Source: gateway`.
    #[must_use]
    pub fn response(
        status: StatusCode,
        type_: &str,
        title: &str,
        detail: impl Into<String>,
    ) -> Response {
        Self {
            type_: type_.to_owned(),
            title: title.to_owned(),
            status: status.as_u16(),
            detail: detail.into(),
            error_code: None,
            retry_after_seconds: None,
            context: BTreeMap::new(),
        }
        .into_gateway()
    }

    /// Render as a gateway error response.
    #[must_use]
    pub fn into_gateway(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, Json(self)).into_response();
        resp.headers_mut().insert(
            "x-oagw-error-source",
            http::HeaderValue::from_static("gateway"),
        );
        resp
    }
}

/// Attach `X-OAGW-Error-Source: upstream` to a proxied response.
#[must_use]
pub fn with_upstream_source(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        "x-oagw-error-source",
        http::HeaderValue::from_static("upstream"),
    );
    resp
}

/// Validation error (`400 validation.error.v1`).
#[must_use]
pub fn validation(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::BAD_REQUEST,
        gts::ERR_VALIDATION_ERROR,
        "Request Validation Failed",
        detail,
    )
}

/// Route/upstream not found (`404 route.not_found.v1`).
#[must_use]
pub fn route_not_found(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::NOT_FOUND,
        gts::ERR_ROUTE_NOT_FOUND,
        "Route Not Found",
        detail,
    )
}

/// Missing `X-OAGW-Target-Host` (`400 routing.missing_target_host.v1`).
#[must_use]
pub fn missing_target_host() -> Response {
    Problem::response(
        StatusCode::BAD_REQUEST,
        gts::ERR_MISSING_TARGET_HOST,
        "Missing Target Host",
        "X-OAGW-Target-Host is required for multi-endpoint upstreams with a \
         common suffix alias",
    )
}

/// Malformed `X-OAGW-Target-Host` (`400 routing.invalid_target_host.v1`).
#[must_use]
pub fn invalid_target_host() -> Response {
    Problem::response(
        StatusCode::BAD_REQUEST,
        gts::ERR_INVALID_TARGET_HOST,
        "Invalid Target Host",
        "X-OAGW-Target-Host must be a hostname or IP address without port, \
         path, or special characters",
    )
}

/// Known-but-not-configured `X-OAGW-Target-Host` (`400 routing.unknown_target_host.v1`).
#[must_use]
pub fn unknown_target_host(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::BAD_REQUEST,
        gts::ERR_UNKNOWN_TARGET_HOST,
        "Unknown Target Host",
        detail,
    )
}

/// Payload too large (`413 payload.too_large.v1`).
#[must_use]
pub fn payload_too_large(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::PAYLOAD_TOO_LARGE,
        gts::ERR_PAYLOAD_TOO_LARGE,
        "Payload Too Large",
        detail,
    )
}

/// Secret not found (`500 secret.not_found.v1`).
#[must_use]
pub fn secret_not_found(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::INTERNAL_SERVER_ERROR,
        gts::ERR_SECRET_NOT_FOUND,
        "Secret Not Found",
        detail,
    )
}

/// Link unavailable (`503 link.unavailable.v1`).
#[must_use]
pub fn link_unavailable(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::SERVICE_UNAVAILABLE,
        gts::ERR_LINK_UNAVAILABLE,
        "Link Unavailable",
        detail,
    )
}

/// Plugin not found / not executable (`503 plugin.not_found.v1`).
#[must_use]
pub fn plugin_not_found(detail: impl Into<String>) -> Response {
    Problem::response(
        StatusCode::SERVICE_UNAVAILABLE,
        gts::ERR_PLUGIN_NOT_FOUND,
        "Plugin Not Found",
        detail,
    )
}

/// Rate limit exceeded (`429 rate_limit.exceeded.v1`) with headers.
#[must_use]
pub fn rate_limited(detail: impl Into<String>, retry_after_secs: u64, limit: u64, reset: u64) -> Response {
    let mut problem = Problem {
        type_: gts::ERR_RATE_LIMIT_EXCEEDED.to_owned(),
        title: "Rate Limit Exceeded".to_owned(),
        status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
        detail: detail.into(),
        error_code: None,
        retry_after_seconds: Some(retry_after_secs),
        context: BTreeMap::new(),
    };
    problem.status = StatusCode::TOO_MANY_REQUESTS.as_u16();
    let status = StatusCode::TOO_MANY_REQUESTS;
    let mut resp = (status, Json(problem)).into_response();
    let headers = resp.headers_mut();
    headers.insert("x-oagw-error-source", http::HeaderValue::from_static("gateway"));
    headers.insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_str(&retry_after_secs.to_string())
            .unwrap_or_else(|_| http::HeaderValue::from_static("1")),
    );
    headers.insert(
        "x-ratelimit-limit",
        http::HeaderValue::from_str(&limit.to_string())
            .unwrap_or_else(|_| http::HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-ratelimit-remaining",
        http::HeaderValue::from_static("0"),
    );
    headers.insert(
        "x-ratelimit-reset",
        http::HeaderValue::from_str(&reset.to_string())
            .unwrap_or_else(|_| http::HeaderValue::from_static("0")),
    );
    resp
}

/// Map an auth-plugin failure to a gateway response.
#[must_use]
pub fn auth_error(err: &AuthError) -> Response {
    match err {
        AuthError::Rejected(detail) => Problem::response(
            StatusCode::UNAUTHORIZED,
            gts::ERR_AUTH_FAILED,
            "Authentication Failed",
            detail.clone(),
        ),
        AuthError::SecretNotFound(detail) => secret_not_found(detail.clone()),
        AuthError::Backend(detail) => Problem::response(
            StatusCode::BAD_GATEWAY,
            gts::ERR_DOWNSTREAM_ERROR,
            "Downstream Error",
            detail.clone(),
        ),
    }
}

/// Map a guard-plugin failure to a gateway response.
#[must_use]
pub fn guard_error(err: &GuardError) -> Response {
    match err {
        GuardError::Request { error_code, detail } => {
            // 400 validation.error.v1 with the plugin's error code surfaced
            // in `errorCode`.
            let body = Problem {
                type_: gts::ERR_VALIDATION_ERROR.to_owned(),
                title: "Request Validation Failed".to_owned(),
                status: StatusCode::BAD_REQUEST.as_u16(),
                detail: detail.clone(),
                error_code: Some(error_code.clone()),
                retry_after_seconds: None,
                context: BTreeMap::new(),
            };
            let mut resp = (StatusCode::BAD_REQUEST, Json(body)).into_response();
            resp.headers_mut().insert(
                "x-oagw-error-source",
                http::HeaderValue::from_static("gateway"),
            );
            resp
        }
        GuardError::Response { error_code, detail } => {
            // 502 with the plugin's error code surfaced in `errorCode`.
            let body = Problem {
                type_: gts::ERR_PROTOCOL_ERROR.to_owned(),
                title: "Protocol Error".to_owned(),
                status: StatusCode::BAD_GATEWAY.as_u16(),
                detail: detail.clone(),
                error_code: Some(error_code.clone()),
                retry_after_seconds: None,
                context: BTreeMap::new(),
            };
            let mut resp = (StatusCode::BAD_GATEWAY, Json(body)).into_response();
            resp.headers_mut().insert(
                "x-oagw-error-source",
                http::HeaderValue::from_static("gateway"),
            );
            resp
        }
    }
}

/// Map a transform-plugin failure to a gateway response.
#[must_use]
pub fn transform_error(err: &TransformError) -> Response {
    let detail = match err {
        TransformError::Failed(d) => d.clone(),
    };
    Problem::response(
        StatusCode::BAD_GATEWAY,
        gts::ERR_PROTOCOL_ERROR,
        "Protocol Error",
        detail,
    )
}

/// Map a transport error to a gateway response (502/503/504 per error table).
#[must_use]
pub fn transport_error(err: &toolkit_http::HttpError) -> Response {
    use toolkit_http::HttpError;
    match err {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => Problem::response(
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_TIMEOUT_REQUEST,
            "Request Timeout",
            format!("upstream request timed out: {err}"),
        ),
        HttpError::Overloaded | HttpError::ServiceClosed => Problem::response(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_LINK_UNAVAILABLE,
            "Link Unavailable",
            format!("upstream link unavailable: {err}"),
        ),
        HttpError::BodyTooLarge { limit, actual } => Problem::response(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            format!(
                "upstream response body exceeds limit: {actual} > {limit} bytes"
            ),
        ),
        HttpError::Transport(_) | HttpError::Tls(_) => Problem::response(
            StatusCode::BAD_GATEWAY,
            gts::ERR_DOWNSTREAM_ERROR,
            "Downstream Error",
            format!("upstream transport failure: {err}"),
        ),
        _ => Problem::response(
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL_ERROR,
            "Protocol Error",
            format!("upstream protocol error: {err}"),
        ),
    }
}

/// Mark a response's `Vary` header with `Origin` (CORS safety, ADR 0004).
pub fn append_vary_origin(headers: &mut HeaderMap) {
    let existing = headers
        .get(http::header::VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut parts: Vec<String> = existing
        .split(',')
        .map(str::trim)
        .map(str::to_owned)
        .filter(|p| !p.is_empty() && p != "Origin")
        .collect();
    parts.push("Origin".to_owned());
    if let Ok(v) = http::HeaderValue::from_str(&parts.join(", ")) {
        headers.insert(http::header::VARY, v);
    }
}
