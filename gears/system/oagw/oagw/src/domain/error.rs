//! OAGW's error catalogue and its RFC 9457 rendering.
//!
//! Two invariants come straight out of the accepted ADRs and are enforced
//! here, once, for every error the gear can produce:
//!
//! * `cpt-cf-oagw-principle-rfc9457` — the body is
//!   `application/problem+json` with a GTS `type` identifier.
//! * `cpt-cf-oagw-principle-error-source` — every response carries
//!   `X-OAGW-Error-Source: gateway|upstream`
//!   (`cpt-cf-oagw-adr-error-source-distinction`).
//!
//! Extension members (`upstream_id`, `host`, `valid_hosts`,
//! `retry_after_seconds`, …) are serialised as **top-level** members of the
//! problem object, which is where RFC 9457 puts extensions and where the ADR
//! examples show them.
//!
//! One consequence is deliberate: the platform's canonical error middleware
//! round-trips a problem body through `toolkit_canonical_errors::Problem`,
//! whose typed envelope has no catch-all for extension members and requires a
//! `context` member OAGW does not emit. Deserialisation therefore fails and
//! the middleware passes the body through unchanged — which is exactly what
//! is wanted, because a successful round-trip would silently drop every
//! extension member the ADRs specify. The middleware logs that failure, so
//! `instance` and `trace_id` are filled in here rather than left to it.

use std::collections::BTreeMap;

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

/// Response header naming the origin of an error
/// (`cpt-cf-oagw-adr-error-source-distinction`).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// `X-OAGW-Error-Source` value for OAGW-generated responses.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// `X-OAGW-Error-Source` value for responses relayed from an upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Routing header naming a specific endpoint inside a multi-endpoint pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

const PROBLEM_JSON: &str = "application/problem+json";
const ERR_PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// A single OAGW error, complete with the HTTP status, GTS type identifier,
/// title, detail and RFC 9457 extension members it renders with.
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: ErrorKind,
    detail: String,
    extensions: BTreeMap<String, Value>,
    /// `Retry-After` seconds, emitted both as a header and as the
    /// `retry_after_seconds` extension.
    retry_after: Option<u64>,
}

/// The catalogue from `DESIGN.md` § *Error Response Format*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Generic request validation failure.
    Validation,
    /// `X-OAGW-Target-Host` is required but absent.
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is syntactically invalid.
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` names no configured endpoint.
    UnknownTargetHost,
    /// Authentication towards the upstream failed.
    AuthenticationFailed,
    /// CORS origin rejected on an actual (non-preflight) request.
    CorsOriginNotAllowed,
    /// CORS method rejected on an actual (non-preflight) request.
    CorsMethodNotAllowed,
    /// No matching route.
    RouteNotFound,
    /// Management resource not found (or invisible to the calling tenant).
    NotFound,
    /// `(tenant_id, alias)` already taken.
    AliasConflict,
    /// Route match rule already taken within the upstream.
    RouteConflict,
    /// Plugin still referenced by an upstream or route.
    PluginInUse,
    /// Request body exceeds the hard limit.
    PayloadTooLarge,
    /// Rate limit exceeded.
    RateLimitExceeded,
    /// A referenced `cred://` secret could not be resolved.
    SecretNotFound,
    /// Unexpected gateway-side failure.
    Internal,
    /// Protocol-level failure talking to the upstream.
    ProtocolError,
    /// Upstream returned or behaved as an error.
    DownstreamError,
    /// A stream (SSE / WebSocket) was aborted.
    StreamAborted,
    /// The upstream link is unavailable (connect failure, disabled upstream).
    LinkUnavailable,
    /// Circuit breaker is open.
    CircuitBreakerOpen,
    /// A referenced plugin could not be resolved.
    PluginNotFound,
    /// Connection to the upstream timed out.
    ConnectionTimeout,
    /// The upstream exchange exceeded its deadline.
    RequestTimeout,
    /// A stream went idle past its deadline.
    IdleTimeout,
}

impl ErrorKind {
    /// HTTP status for this kind.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
            Self::RouteNotFound | Self::NotFound => StatusCode::NOT_FOUND,
            Self::AliasConflict | Self::RouteConflict | Self::PluginInUse => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound | Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => {
                StatusCode::BAD_GATEWAY
            }
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// GTS type identifier for this kind.
    #[must_use]
    pub fn gts_type(self) -> String {
        let suffix = match self {
            Self::Validation => "cf.oagw.validation.error.v1",
            Self::MissingTargetHost => "cf.oagw.routing.missing_target_host.v1",
            Self::InvalidTargetHost => "cf.oagw.routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "cf.oagw.routing.unknown_target_host.v1",
            Self::AuthenticationFailed => "cf.oagw.auth.failed.v1",
            Self::CorsOriginNotAllowed => "cf.oagw.cors.origin_not_allowed.v1",
            Self::CorsMethodNotAllowed => "cf.oagw.cors.method_not_allowed.v1",
            Self::RouteNotFound => "cf.oagw.route.not_found.v1",
            Self::NotFound => "cf.oagw.not_found.v1",
            Self::AliasConflict => "cf.oagw.alias.conflict.v1",
            Self::RouteConflict => "cf.oagw.route.conflict.v1",
            Self::PluginInUse => "cf.oagw.plugin.in_use.v1",
            Self::PayloadTooLarge => "cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "cf.oagw.secret.not_found.v1",
            Self::Internal => "cf.oagw.internal.error.v1",
            Self::ProtocolError => "cf.oagw.protocol.error.v1",
            Self::DownstreamError => "cf.oagw.downstream.error.v1",
            Self::StreamAborted => "cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "cf.oagw.timeout.idle.v1",
        };
        format!("{ERR_PREFIX}{suffix}")
    }

    /// Human-readable summary.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::RouteNotFound => "Route Not Found",
            Self::NotFound => "Not Found",
            Self::AliasConflict => "Alias Conflict",
            Self::RouteConflict => "Route Conflict",
            Self::PluginInUse => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::Internal => "Internal Error",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }

    /// Whether a client may retry, per `cpt-cf-oagw-fr-error-codes`.
    #[must_use]
    pub fn retriable(self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded
                | Self::LinkUnavailable
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

impl OagwError {
    /// Build an error of `kind` with a human-readable `detail`.
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: BTreeMap::new(),
            retry_after: None,
        }
    }

    /// Attach an RFC 9457 extension member.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// Attach the `trace_id` extension, read from the inbound correlation
    /// headers, so a client can join a failure to the gateway's own logs.
    #[must_use]
    pub fn with_trace_from(self, headers: &axum::http::HeaderMap) -> Self {
        let trace_id = headers
            .get("traceparent")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let parts: Vec<&str> = value.split('-').collect();
                (parts.len() >= 4 && parts[0] == "00").then(|| parts[1].to_owned())
            })
            .or_else(|| {
                ["x-trace-id", "x-request-id"].iter().find_map(|name| {
                    headers
                        .get(*name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                })
            });
        match trace_id {
            Some(trace_id) => self.with("trace_id", trace_id),
            None => self,
        }
    }

    /// Attach retry guidance (`Retry-After` header + `retry_after_seconds`).
    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }

    /// The error kind.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// HTTP status this error renders with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.kind.status()
    }

    /// The `detail` member.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Render the RFC 9457 problem object, including extension members.
    ///
    /// `instance` is the request URI when known; the platform's canonical
    /// error middleware fills it in for canonical problems, but OAGW carries
    /// extension members that the middleware's typed envelope cannot round
    /// trip, so the value is set here.
    #[must_use]
    pub fn to_problem(&self, instance: Option<&str>) -> Value {
        let mut map = Map::new();
        map.insert("type".to_owned(), Value::String(self.kind.gts_type()));
        map.insert(
            "title".to_owned(),
            Value::String(self.kind.title().to_owned()),
        );
        map.insert(
            "status".to_owned(),
            Value::Number(self.kind.status().as_u16().into()),
        );
        map.insert("detail".to_owned(), Value::String(self.detail.clone()));
        if let Some(instance) = instance {
            map.insert("instance".to_owned(), Value::String(instance.to_owned()));
        }
        map.insert("retriable".to_owned(), Value::Bool(self.kind.retriable()));
        if let Some(secs) = self.retry_after {
            map.insert("retry_after_seconds".to_owned(), Value::Number(secs.into()));
        }
        for (key, value) in &self.extensions {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }

    /// Render the error as a gateway-sourced HTTP response.
    #[must_use]
    pub fn into_response_with_instance(self, instance: Option<&str>) -> Response {
        let body = serde_json::to_vec(&self.to_problem(instance))
            .unwrap_or_else(|_| br#"{"title":"Internal Error","status":500}"#.to_vec());
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = self.kind.status();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        headers.insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if let Some(secs) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&secs.to_string())
        {
            headers.insert(header::RETRY_AFTER, value);
        }
        response
    }

    // -- Constructor shorthands used across the gear ------------------------

    /// `400` validation failure.
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, detail)
    }

    /// `400` validation failure keyed to a request field.
    pub fn field(field: &str, detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, detail).with("field", field.to_owned())
    }

    /// `404` management-resource miss.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, detail)
    }

    /// `404` proxy route miss.
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RouteNotFound, detail)
    }

    /// `500` unexpected gateway failure.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, detail)
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.title(), self.detail)
    }
}

impl std::error::Error for OagwError {}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        self.into_response_with_instance(None)
    }
}

/// Result alias for domain operations.
pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_match_the_prd_table() {
        assert_eq!(ErrorKind::Validation.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            ErrorKind::AuthenticationFailed.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(ErrorKind::RouteNotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            ErrorKind::PayloadTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ErrorKind::RateLimitExceeded.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorKind::SecretNotFound.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ErrorKind::DownstreamError.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            ErrorKind::CircuitBreakerOpen.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ErrorKind::RequestTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(ErrorKind::PluginInUse.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn gts_types_match_the_design_table() {
        assert_eq!(
            ErrorKind::RateLimitExceeded.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(
            ErrorKind::MissingTargetHost.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
        assert_eq!(
            ErrorKind::CorsOriginNotAllowed.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
    }

    #[test]
    fn problem_carries_top_level_extensions() {
        let err = OagwError::new(ErrorKind::MissingTargetHost, "need a host")
            .with("alias", "vendor.com".to_owned())
            .with(
                "valid_hosts",
                Value::Array(vec![Value::String("us.vendor.com".to_owned())]),
            );
        let problem = err.to_problem(Some("/oagw/v1/proxy/vendor.com/x"));
        assert_eq!(problem["status"], 400);
        assert_eq!(problem["alias"], "vendor.com");
        assert_eq!(problem["valid_hosts"][0], "us.vendor.com");
        assert_eq!(problem["instance"], "/oagw/v1/proxy/vendor.com/x");
    }

    #[test]
    fn responses_are_problem_json_tagged_as_gateway() {
        let response = OagwError::new(ErrorKind::RateLimitExceeded, "slow down")
            .with_retry_after(15)
            .into_response_with_instance(Some("/x"));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("15")
        );
    }

    #[test]
    fn trace_id_is_read_from_the_correlation_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req_abc"));
        let problem = OagwError::validation("x")
            .with_trace_from(&headers)
            .to_problem(None);
        assert_eq!(problem["trace_id"], "req_abc");

        headers.insert(
            "traceparent",
            HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        let problem = OagwError::validation("x")
            .with_trace_from(&headers)
            .to_problem(None);
        assert_eq!(problem["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");

        let problem = OagwError::validation("x")
            .with_trace_from(&axum::http::HeaderMap::new())
            .to_problem(None);
        assert!(problem.get("trace_id").is_none());
    }

    #[test]
    fn retriability_follows_the_prd() {
        assert!(ErrorKind::RateLimitExceeded.retriable());
        assert!(ErrorKind::RequestTimeout.retriable());
        assert!(!ErrorKind::Validation.retriable());
        assert!(!ErrorKind::RouteNotFound.retriable());
    }
}
