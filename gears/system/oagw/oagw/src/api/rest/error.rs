//! RFC 9457 problem+json error rendering for the OAGW REST surfaces.
//!
//! Every OAGW response carries `X-OAGW-Error-Source: gateway|upstream`
//! (ADR 0007). Gateway-originated errors use `application/problem+json` with
//! GTS `type` identifiers and OAGW-specific extension fields.

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::header::HeaderValue;
use serde::Serialize;

/// Header present on every OAGW response (success and error).
pub const HEADER_ERROR_SOURCE: &str = "x-oagw-error-source";
/// Value indicating the gateway generated the response.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value indicating the response is an upstream passthrough.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// GTS error type identifiers (DESIGN.md error table).
pub mod type_ids {
    /// General route/request validation error (400).
    pub const VALIDATION_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// Missing `X-OAGW-Target-Host` for multi-endpoint alias (400).
    pub const MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// `X-OAGW-Target-Host` has an invalid format (400).
    pub const INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// `X-OAGW-Target-Host` matches no configured endpoint (400).
    pub const UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// Authentication to the upstream failed (401).
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// No matching route found (404).
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// Requested management resource not found (404).
    pub const NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.not_found.v1";
    /// Plugin in use by upstream(s)/route(s) (409).
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// Conflict (e.g. duplicate alias) (409).
    pub const CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1";
    /// Request payload too large (413).
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// Rate limit exceeded (429).
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// Referenced secret not found (500).
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// Protocol-level error (502).
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// Upstream service error (502).
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// Stream connection aborted (502).
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// Upstream link unavailable (503).
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// Circuit breaker open (503).
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// Plugin not found (503).
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// Connection timeout (504).
    pub const CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// Request timeout (504).
    pub const REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// Idle timeout (504).
    pub const IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
    /// CORS origin not allowed (403).
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// CORS method not allowed (403).
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
    /// Required header missing (400 request / 502 response, ADR 0009).
    pub const REQUIRED_HEADER_MISSING: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.required_header.missing.v1";
    /// Upstream TLS/unencrypted-scheme restrictions (gateway-side 502).
    pub const UPSTREAM_UNSUPPORTED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.unsupported.v1";
}

/// Referencing detail for the plugin-delete 409 body (ADR 0001).
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReferencedByDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
}

/// An RFC 9457 Problem Details response with OAGW extensions.
#[derive(Debug, Clone, Serialize)]
pub struct OagwProblem {
    #[serde(rename = "type")]
    pub type_: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    // --- OAGW extensions ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<ReferencedByDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_headers: Option<Vec<String>>,
}

impl OagwProblem {
    /// Start a problem with the GTS type id, title, and HTTP status.
    #[must_use]
    pub fn new(type_: impl Into<String>, title: impl Into<String>, status: StatusCode) -> Self {
        Self {
            type_: type_.into(),
            title: title.into(),
            status: status.as_u16(),
            detail: None,
            instance: None,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            trace_id: None,
            alias: None,
            valid_hosts: None,
            invalid_value: None,
            plugin_id: None,
            referenced_by: None,
            missing_headers: None,
        }
    }

    /// Convenience: 400 validation problem.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(
            type_ids::VALIDATION_ERROR,
            "Validation Error",
            StatusCode::BAD_REQUEST,
        )
        .detail(detail)
    }

    /// Convenience: 404 not-found problem.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(type_ids::NOT_FOUND, "Not Found", StatusCode::NOT_FOUND).detail(detail)
    }

    /// Convenience: 409 conflict problem.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(type_ids::CONFLICT, "Conflict", StatusCode::CONFLICT).detail(detail)
    }

    /// Convenience: gateway 502 downstream error.
    #[must_use]
    pub fn downstream(detail: impl Into<String>) -> Self {
        Self::new(
            type_ids::DOWNSTREAM_ERROR,
            "Downstream Error",
            StatusCode::BAD_GATEWAY,
        )
        .detail(detail)
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    #[must_use]
    pub fn upstream_id(mut self, id: impl Into<String>) -> Self {
        self.upstream_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn retry_after_seconds(mut self, secs: u64) -> Self {
        self.retry_after_seconds = Some(secs);
        self
    }

    #[must_use]
    pub fn trace_id(mut self, trace: impl Into<String>) -> Self {
        self.trace_id = Some(trace.into());
        self
    }

    #[must_use]
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    #[must_use]
    pub fn valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.valid_hosts = Some(hosts);
        self
    }

    #[must_use]
    pub fn invalid_value(mut self, value: impl Into<String>) -> Self {
        self.invalid_value = Some(value.into());
        self
    }

    #[must_use]
    pub fn plugin_id(mut self, id: impl Into<String>) -> Self {
        self.plugin_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn referenced_by(mut self, refs: ReferencedByDto) -> Self {
        self.referenced_by = Some(refs);
        self
    }

    #[must_use]
    pub fn missing_headers(mut self, headers: Vec<String>) -> Self {
        self.missing_headers = Some(headers);
        self
    }

    /// Render as a gateway error response: `application/problem+json` body +
    /// `X-OAGW-Error-Source: gateway` (ADR 0007, RFC 9457 `type` member).
    #[must_use]
    pub fn into_response(self) -> Response {
        let mut resp = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(self),
        )
            .into_response();
        // RFC 9457 media type: override axum's default `application/json`.
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        resp.headers_mut().insert(
            HEADER_ERROR_SOURCE,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        resp
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn problem_response_carries_header_and_json() {
        let resp = OagwProblem::validation("bad alias")
            .alias("api")
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(HEADER_ERROR_SOURCE).unwrap(),
            ERROR_SOURCE_GATEWAY
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(body["type"], type_ids::VALIDATION_ERROR);
        assert_eq!(body["status"], 400);
        assert_eq!(body["alias"], "api");
        assert!(body.get("context").is_none(), "no toolkit `context` field");
    }

    #[test]
    fn retry_after_extension_serializes_for_429() {
        let problem = OagwProblem::new(
            type_ids::RATE_LIMIT_EXCEEDED,
            "Rate Limit Exceeded",
            StatusCode::TOO_MANY_REQUESTS,
        )
        .retry_after_seconds(60);
        let json = serde_json::to_value(&problem).unwrap();
        // DESIGN extension field names are snake_case (e.g. `retry_after_seconds`).
        assert_eq!(json["retry_after_seconds"], 60);
    }
}
