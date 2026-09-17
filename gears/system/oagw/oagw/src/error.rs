//! Error-source distinction for the OAGW gear (ADR 0007).
//!
//! OAGW sits between internal clients and external upstream services, and
//! either side can fail. Clients therefore need to know who produced a given
//! failure: OAGW itself (rate limiting, auth failure, route not found, timeout,
//! circuit breaker) or the upstream service whose response is only being
//! forwarded.
//!
//! Per ADR 0007 the distinction is carried by a response header:
//!
//! - `X-OAGW-Error-Source: gateway` — OAGW generated the failure. The body is
//!   an RFC 9457 `application/problem+json` document whose `type` member is the
//!   GTS error type identifier for the failure.
//! - `X-OAGW-Error-Source: upstream` — the upstream returned the failure and
//!   OAGW passes the response through unchanged (no body manipulation).
//!
//! Every response this gear emits — errors and forwarded successes alike —
//! carries the header, so [`ErrorSource`] is the single place that knows how to
//! stamp it.

use std::fmt;
use std::sync::LazyLock;

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use serde_json::value::Map as JsonMap;
use toolkit_gts::gts_id;

/// Name of the header that tells clients who produced a response (ADR 0007).
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// Media type mandated for gateway-generated failures (RFC 9457 / ADR 0007).
pub const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";

/// Parsed form of [`ERROR_SOURCE_HEADER`], so the header is inserted with a
/// single canonical casing instead of a per-call parse.
pub static ERROR_SOURCE_HEADER_NAME: LazyLock<HeaderName> =
    LazyLock::new(|| HeaderName::from_static("x-oagw-error-source"));

/// Which side of the gateway produced a response (ADR 0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorSource {
    /// OAGW generated the failure itself.
    Gateway,
    /// The upstream service returned the failure; passed through unchanged.
    Upstream,
}

impl ErrorSource {
    /// The value carried by [`ERROR_SOURCE_HEADER`] for this source.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }

    /// Ready-made header value for [`ERROR_SOURCE_HEADER`].
    #[must_use]
    pub const fn header_value(self) -> HeaderValue {
        match self {
            Self::Gateway => HeaderValue::from_static("gateway"),
            Self::Upstream => HeaderValue::from_static("upstream"),
        }
    }

    /// Stamps [`ERROR_SOURCE_HEADER`] onto an existing response in place,
    /// leaving its status, headers and body untouched.
    pub fn set_on(self, response: &mut Response) {
        response
            .headers_mut()
            .insert(&*ERROR_SOURCE_HEADER_NAME, self.header_value());
    }

    /// Consumes `response` and returns it with [`ERROR_SOURCE_HEADER`] set.
    #[must_use]
    pub fn on(self, response: Response) -> Response {
        let (mut parts, body) = response.into_parts();
        parts
            .headers
            .insert(&*ERROR_SOURCE_HEADER_NAME, self.header_value());
        Response::from_parts(parts, body)
    }
}

impl fmt::Display for ErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The gateway failures OAGW can emit, each with the GTS error type
/// identifier, HTTP status and RFC 9457 title mandated by ADR 0007.
///
/// [`RateLimitExceeded`](Self::RateLimitExceeded),
/// [`MissingTargetHost`](Self::MissingTargetHost),
/// [`InvalidTargetHost`](Self::InvalidTargetHost) and
/// [`UnknownTargetHost`](Self::UnknownTargetHost) carry the identifiers spelled
/// out in ADR 0007, Appendix A. [`Validation`](Self::Validation) carries the
/// identifier DESIGN.md §3.3 assigns to `ValidationError`. The remaining
/// variants cover the gateway failure causes enumerated by ADR 0007 ("rate
/// limit, auth failure, route not found, timeout, circuit breaker") and follow
/// the same `cf.core.errors.err.v1~cf.oagw.<area>.<code>.v1` identifier scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GatewayErrorKind {
    /// Per-upstream or per-tenant rate limit exhausted (429).
    RateLimitExceeded,
    /// `X-OAGW-Target-Host` required but absent (400).
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is not a valid hostname or IP address (400).
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` matches no configured endpoint (400).
    UnknownTargetHost,
    /// A submitted configuration (upstream, route, alias, rate limit, CORS)
    /// failed validation (400). DESIGN.md §3.3 "Error Response Format".
    Validation,
    /// The request conflicts with existing state — a duplicate `(tenant,
    /// alias)` pair, or a route match rule that already exists (409).
    Conflict,
    /// A plugin that is still referenced by an upstream or a route was
    /// deleted (409). DESIGN.md §3.3 "Error Response Format" `PluginInUse`.
    PluginInUse,
    /// The requested configuration resource does not exist (404).
    NotFound,
    /// No upstream or route matched the request (404). DESIGN.md §3.3
    /// "Error Response Format" `RouteNotFound`.
    RouteNotFound,
    /// The caller failed gateway authentication (401).
    Unauthenticated,
    /// A cross-origin origin outside `allowed_origins` (403). ADR 0004
    /// "Error Responses".
    CorsOriginNotAllowed,
    /// A cross-origin method outside `allowed_methods` (403). ADR 0004
    /// "Error Responses".
    CorsMethodNotAllowed,
    /// The request payload exceeds the 100 MB hard limit (413). DESIGN.md §3.3
    /// "Error Response Format" `PayloadTooLarge`.
    PayloadTooLarge,
    /// The upstream could not be reached or refused the connection (502).
    /// DESIGN.md §3.3 "Error Response Format" `DownstreamError`.
    DownstreamError,
    /// The upstream did not answer within the configured timeout (504).
    /// DESIGN.md §3.3 "Error Response Format" `RequestTimeout`.
    UpstreamTimeout,
    /// The upstream exists but does not accept traffic — the resolved upstream
    /// is disabled, or its scheme is not diallable under the gear policy (503).
    /// DESIGN.md §3.3 "Error Response Format" `LinkUnavailable`.
    LinkUnavailable,
    /// The circuit breaker for the upstream is open (503).
    CircuitOpen,
    /// Unexpected gateway-side failure (500).
    Internal,
}

impl GatewayErrorKind {
    /// GTS error type identifier carried in the `type` member (ADR 0007).
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::RateLimitExceeded => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
            }
            Self::MissingTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1")
            }
            Self::InvalidTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1")
            }
            Self::UnknownTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1")
            }
            Self::Validation => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1")
            }
            Self::Conflict => gts_id!("cf.core.errors.err.v1~cf.oagw.config.conflict.v1"),
            Self::PluginInUse => gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"),
            Self::NotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.config.not_found.v1"),
            Self::RouteNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
            }
            Self::Unauthenticated => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.auth.unauthenticated.v1")
            }
            Self::CorsOriginNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
            }
            Self::CorsMethodNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1")
            }
            Self::PayloadTooLarge => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
            }
            Self::DownstreamError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
            }
            Self::UpstreamTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1")
            }
            Self::LinkUnavailable => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
            }
            Self::CircuitOpen => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.circuit_open.v1")
            }
            Self::Internal => gts_id!("cf.core.errors.err.v1~cf.oagw.internal.error.v1"),
        }
    }

    /// Human-readable summary carried in the `title` member.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::Validation => "Validation Error",
            Self::Conflict => "Conflict",
            Self::PluginInUse => "Plugin is referenced by existing resources",
            Self::NotFound => "Not Found",
            Self::RouteNotFound => "Route Not Found",
            Self::Unauthenticated => "Unauthenticated",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::DownstreamError => "Downstream Error",
            Self::UpstreamTimeout => "Upstream Timeout",
            Self::LinkUnavailable => "Upstream Link Unavailable",
            Self::CircuitOpen => "Circuit Breaker Open",
            Self::Internal => "Internal Gateway Error",
        }
    }

    /// HTTP status the gateway answers with for this failure class.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::RateLimitExceeded => 429,
            Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost
            | Self::Validation => 400,
            Self::NotFound | Self::RouteNotFound => 404,
            Self::Conflict | Self::PluginInUse => 409,
            Self::Unauthenticated => 401,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => 403,
            Self::PayloadTooLarge => 413,
            Self::DownstreamError => 502,
            Self::UpstreamTimeout => 504,
            Self::LinkUnavailable | Self::CircuitOpen => 503,
            Self::Internal => 500,
        }
    }
}

/// OAGW extension members of a problem document (ADR 0007, Appendix A).
///
/// Serialized flat, so they appear next to the RFC 9457 standard members
/// exactly as the ADR examples show them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProblemExtensions {
    /// Identifier of the upstream the request was routed to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,

    /// Host the request was (or would have been) forwarded to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,

    /// Path that was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Retry guidance, in seconds (`Retry-After` carries the same value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,

    /// Distributed tracing correlation identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,

    /// Failure-specific members (e.g. `alias`, `valid_hosts`,
    /// `invalid_value` for the routing failures of ADR 0007, Appendix A).
    #[serde(flatten)]
    pub extra: JsonMap<String, Value>,
}

/// RFC 9457 problem details document for a gateway-generated failure.
///
/// The five standard members are always present; the OAGW extension members
/// ([`ProblemExtensions`]) are omitted when unset.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProblemDetails {
    /// GTS error type identifier for the failure (RFC 9457 `type`).
    #[serde(rename = "type")]
    pub problem_type: String,

    /// Human-readable summary of the failure class.
    pub title: String,

    /// HTTP status code of the response.
    pub status: u16,

    /// Human-readable explanation of this occurrence.
    pub detail: String,

    /// URI reference identifying this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,

    /// OAGW extension members, serialized at the top level.
    #[serde(flatten)]
    pub extensions: ProblemExtensions,
}

/// A failure generated by OAGW itself (ADR 0007:
/// `X-OAGW-Error-Source: gateway`).
///
/// Renders as an `application/problem+json` response stamped with
/// [`ERROR_SOURCE_HEADER`].
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayError {
    kind: GatewayErrorKind,
    detail: String,
    instance: Option<String>,
    extensions: ProblemExtensions,
}

impl GatewayError {
    /// Builds a gateway failure of the given kind.
    #[must_use]
    pub fn new(kind: GatewayErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            extensions: ProblemExtensions::default(),
        }
    }

    /// Rate limit exhausted for an upstream; `retry_after_secs` feeds both the
    /// `retry_after_seconds` member and the `Retry-After` header.
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>, retry_after_secs: u64) -> Self {
        Self::new(GatewayErrorKind::RateLimitExceeded, detail)
            .with_retry_after_secs(retry_after_secs)
    }

    /// A submitted configuration failed validation (400,
    /// `cf.oagw.validation.error.v1`). `field` names the offending member and
    /// is emitted as the `field` extension member.
    #[must_use]
    pub fn validation(detail: impl Into<String>, field: impl Into<String>) -> Self {
        Self::new(GatewayErrorKind::Validation, detail).with_extension("field", field.into())
    }

    /// A conflict with existing configuration state (409,
    /// `cf.oagw.conflict.v1`) — a duplicate alias, or an already-registered
    /// route match rule.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(GatewayErrorKind::Conflict, detail)
    }

    /// A configuration resource that does not exist (404,
    /// `cf.oagw.not_found.v1`).
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(GatewayErrorKind::NotFound, detail)
    }

    /// A plugin that is still referenced by an upstream or a route was
    /// deleted (409, `cf.oagw.plugin.in_use.v1`). The caller adds the
    /// `plugin_id` and `referenced_by` extension members (ADR 0001,
    /// "Plugin Deletion Behavior").
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(GatewayErrorKind::PluginInUse, detail)
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> GatewayErrorKind {
        self.kind
    }

    /// GTS error type identifier rendered in the `type` member.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        self.kind.gts_type()
    }

    /// Human-readable summary rendered in the `title` member.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        self.kind.title()
    }

    /// HTTP status code of the response.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.kind.status()
    }

    /// Explanation of this occurrence rendered in the `detail` member.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The OAGW extension members this failure carries.
    #[must_use]
    pub const fn extensions(&self) -> &ProblemExtensions {
        &self.extensions
    }

    /// Renders the RFC 9457 problem details document for this failure.
    #[must_use]
    pub fn problem(&self) -> ProblemDetails {
        ProblemDetails {
            problem_type: self.kind.gts_type().to_owned(),
            title: self.kind.title().to_owned(),
            status: self.kind.status(),
            detail: self.detail.clone(),
            instance: self.instance.clone(),
            extensions: self.extensions.clone(),
        }
    }

    /// Sets the `instance` member (RFC 9457 occurrence URI reference).
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Sets the `upstream_id` extension member.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.extensions.upstream_id = Some(upstream_id.into());
        self
    }

    /// Sets the `host` extension member.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.extensions.host = Some(host.into());
        self
    }

    /// Sets the `path` extension member.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.extensions.path = Some(path.into());
        self
    }

    /// Sets the `retry_after_seconds` extension member (also emitted as the
    /// `Retry-After` header).
    #[must_use]
    pub fn with_retry_after_secs(mut self, seconds: u64) -> Self {
        self.extensions.retry_after_seconds = Some(seconds);
        self
    }

    /// Sets the `trace_id` extension member.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.extensions.trace_id = Some(trace_id.into());
        self
    }

    /// Inserts a failure-specific extension member (e.g. `alias`,
    /// `valid_hosts`, `invalid_value`).
    #[must_use]
    pub fn with_extension(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extensions.extra.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} gateway error {}: {}",
            self.kind.status(),
            self.kind.gts_type(),
            self.detail
        )
    }
}

impl std::error::Error for GatewayError {}

/// Marks a forwarded response as an upstream failure (ADR 0007): the upstream
/// status, headers and body are passed through untouched and only
/// [`ERROR_SOURCE_HEADER`] is added.
#[must_use]
pub fn mark_upstream_error(response: Response) -> Response {
    ErrorSource::Upstream.on(response)
}

/// Fallback body for the (unreachable in practice) case where the problem
/// document cannot be serialized.
fn serialization_fallback_body() -> String {
    format!(
        "{{\"type\":\"{}\",\"title\":\"{}\",\"status\":{},\"detail\":\"failed to serialize problem\"}}",
        GatewayErrorKind::Internal.gts_type(),
        GatewayErrorKind::Internal.title(),
        GatewayErrorKind::Internal.status(),
    )
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let problem = self.problem();

        let mut response = match serde_json::to_vec(&problem) {
            Ok(body) => (status, body).into_response(),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    problem_type = self.gts_type(),
                    "failed to serialize oagw problem details; emitting fallback body"
                );
                (status, serialization_fallback_body()).into_response()
            }
        };

        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(APPLICATION_PROBLEM_JSON),
        );
        ErrorSource::Gateway.set_on(&mut response);

        if let Some(seconds) = problem.extensions.retry_after_seconds {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }

        response
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// Instance URI used by the ADR 0007 examples.
    const EXAMPLE_INSTANCE: &str = "/api/oagw/v1/proxy/api.openai.com/v1/chat/completions";

    #[test]
    fn test_error_source_header_name() {
        assert_eq!(ERROR_SOURCE_HEADER, "X-OAGW-Error-Source");
        assert_eq!(ERROR_SOURCE_HEADER_NAME.as_str(), "x-oagw-error-source");
        assert_eq!(
            ERROR_SOURCE_HEADER.to_ascii_lowercase(),
            ERROR_SOURCE_HEADER_NAME.as_str(),
        );
    }

    #[test]
    fn test_error_source_values() {
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
        assert_eq!(ErrorSource::Gateway.header_value(), "gateway");
        assert_eq!(ErrorSource::Upstream.header_value(), "upstream");
        assert_eq!(ErrorSource::Gateway.to_string(), "gateway");
        assert_eq!(ErrorSource::Upstream.to_string(), "upstream");
    }

    #[test]
    fn test_error_source_set_on_leaves_response_untouched() {
        let mut response = Response::new("upstream body".into());
        response
            .headers_mut()
            .insert("x-upstream", "1".parse().unwrap());

        ErrorSource::Upstream.set_on(&mut response);

        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()[&*ERROR_SOURCE_HEADER_NAME], "upstream");
        assert_eq!(response.headers()["x-upstream"], "1");
    }

    #[test]
    fn test_error_source_on_replaces_previous_value() {
        let response = ErrorSource::Upstream.on(Response::default());

        let response = ErrorSource::Gateway.on(response);

        assert_eq!(response.headers()[&*ERROR_SOURCE_HEADER_NAME], "gateway");
    }

    /// ADR 0007, Appendix A "Gateway Error — RFC 9457 Problem Details".
    #[tokio::test]
    async fn test_gateway_error_response_carries_header_and_problem_body() {
        let error = GatewayError::rate_limit_exceeded(
            "Rate limit exceeded for upstream api.openai.com",
            15,
        )
        .with_instance(EXAMPLE_INSTANCE)
        .with_upstream_id("uuid-123")
        .with_host("api.openai.com")
        .with_trace_id("01J...");

        assert_eq!(error.status(), 429);
        assert_eq!(error.title(), "Rate Limit Exceeded");
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
        );

        let response = error.into_response();

        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()[&*ERROR_SOURCE_HEADER_NAME], "gateway");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            APPLICATION_PROBLEM_JSON
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "15");

        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
        );
        assert_eq!(json["title"], "Rate Limit Exceeded");
        assert_eq!(json["status"], 429);
        assert_eq!(
            json["detail"],
            "Rate limit exceeded for upstream api.openai.com"
        );
        assert_eq!(json["instance"], EXAMPLE_INSTANCE);
        assert_eq!(json["upstream_id"], "uuid-123");
        assert_eq!(json["host"], "api.openai.com");
        assert_eq!(json["retry_after_seconds"], 15);
        assert_eq!(json["trace_id"], "01J...");
        assert_eq!(parts.status, 429);
    }

    #[test]
    fn test_routing_error_types_match_adr_0007() {
        assert_eq!(
            GatewayErrorKind::MissingTargetHost.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
        );
        assert_eq!(
            GatewayErrorKind::InvalidTargetHost.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        );
        assert_eq!(
            GatewayErrorKind::UnknownTargetHost.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
        );
        assert_eq!(GatewayErrorKind::MissingTargetHost.status(), 400);
        assert_eq!(GatewayErrorKind::InvalidTargetHost.status(), 400);
        assert_eq!(GatewayErrorKind::UnknownTargetHost.status(), 400);
    }

    /// ADR 0007, Appendix A "Missing Target Host Header" — error-specific
    /// extension members travel at the top level of the problem document.
    #[test]
    fn test_error_specific_extensions_are_flattened() {
        let error = GatewayError::new(
            GatewayErrorKind::MissingTargetHost,
            "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: [us.vendor.com, eu.vendor.com]",
        )
        .with_instance("/api/oagw/v1/proxy/vendor.com/v1/api/resource")
        .with_upstream_id("gts.cf.core.oagw.upstream.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7")
        .with_extension("alias", "vendor.com")
        .with_extension("valid_hosts", vec!["us.vendor.com", "eu.vendor.com"]);

        let json = serde_json::to_value(error.problem()).unwrap();

        assert_eq!(json["alias"], "vendor.com");
        assert_eq!(json["valid_hosts"][0], "us.vendor.com");
        assert_eq!(json["valid_hosts"][1], "eu.vendor.com");
        assert_eq!(json["status"], 400);
        assert_eq!(json["title"], "Missing Target Host Header");
    }

    #[test]
    fn test_validation_conflict_and_not_found_constructors() {
        let validation = GatewayError::validation("port must be between 1 and 65535", "port");
        assert_eq!(validation.status(), 400);
        assert_eq!(
            validation.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        );
        assert_eq!(validation.extensions().extra["field"], "port");

        let conflict = GatewayError::conflict("alias `api.openai.com` already exists");
        assert_eq!(conflict.status(), 409);
        assert_eq!(conflict.title(), "Conflict");

        let missing = GatewayError::not_found("no upstream with id 1234");
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.title(), "Not Found");
    }

    #[test]
    fn test_unset_extension_members_are_omitted() {
        let error = GatewayError::new(GatewayErrorKind::RouteNotFound, "no upstream matched");

        let json = serde_json::to_value(error.problem()).unwrap();

        for key in [
            "instance",
            "upstream_id",
            "host",
            "path",
            "retry_after_seconds",
            "trace_id",
            "alias",
            "valid_hosts",
        ] {
            assert!(json.get(key).is_none(), "{key} must be omitted");
        }
        assert_eq!(json["status"], 404);
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        );
    }

    #[test]
    fn test_gateway_error_kinds_carry_adr_statuses_and_titles() {
        let expected = [
            (
                GatewayErrorKind::RateLimitExceeded,
                429,
                "Rate Limit Exceeded",
            ),
            (
                GatewayErrorKind::MissingTargetHost,
                400,
                "Missing Target Host Header",
            ),
            (
                GatewayErrorKind::InvalidTargetHost,
                400,
                "Invalid Target Host Format",
            ),
            (
                GatewayErrorKind::UnknownTargetHost,
                400,
                "Unknown Target Host",
            ),
            (GatewayErrorKind::Validation, 400, "Validation Error"),
            (GatewayErrorKind::Conflict, 409, "Conflict"),
            (
                GatewayErrorKind::PluginInUse,
                409,
                "Plugin is referenced by existing resources",
            ),
            (GatewayErrorKind::NotFound, 404, "Not Found"),
            (GatewayErrorKind::RouteNotFound, 404, "Route Not Found"),
            (GatewayErrorKind::Unauthenticated, 401, "Unauthenticated"),
            (
                GatewayErrorKind::CorsOriginNotAllowed,
                403,
                "CORS Origin Not Allowed",
            ),
            (
                GatewayErrorKind::CorsMethodNotAllowed,
                403,
                "CORS Method Not Allowed",
            ),
            (GatewayErrorKind::PayloadTooLarge, 413, "Payload Too Large"),
            (GatewayErrorKind::DownstreamError, 502, "Downstream Error"),
            (GatewayErrorKind::UpstreamTimeout, 504, "Upstream Timeout"),
            (GatewayErrorKind::CircuitOpen, 503, "Circuit Breaker Open"),
            (GatewayErrorKind::Internal, 500, "Internal Gateway Error"),
        ];

        for (kind, status, title) in expected {
            assert_eq!(kind.status(), status, "{title}");
            assert_eq!(kind.title(), title, "{kind:?}");
            assert!(
                kind.gts_type()
                    .starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
                "{kind:?} must use the ADR 0007 GTS error type namespace",
            );
        }
    }

    #[test]
    fn test_problem_details_render_all_rfc_9457_members() {
        let error = GatewayError::new(GatewayErrorKind::Unauthenticated, "missing bearer token")
            .with_instance("/api/oagw/v1/proxy/vendor.com/v1/api/resource");

        let problem = error.problem();

        assert_eq!(
            problem.problem_type,
            GatewayErrorKind::Unauthenticated.gts_type()
        );
        assert_eq!(problem.title, "Unauthenticated");
        assert_eq!(problem.status, 401);
        assert_eq!(problem.detail, "missing bearer token");
        assert_eq!(
            problem.instance.as_deref(),
            Some("/api/oagw/v1/proxy/vendor.com/v1/api/resource")
        );
    }

    #[test]
    fn test_mark_upstream_error_only_adds_the_header() {
        let response = (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"error\":\"upstream boom\"}",
        )
            .into_response();

        let response = mark_upstream_error(response);

        assert_eq!(response.status(), 500);
        assert_eq!(response.headers()[&*ERROR_SOURCE_HEADER_NAME], "upstream");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[test]
    fn test_gateway_error_display() {
        let error = GatewayError::new(GatewayErrorKind::UpstreamTimeout, "no response in 2s");

        let rendered = error.to_string();

        assert!(rendered.contains("504"), "got {rendered}");
        assert!(
            rendered.contains("cf.oagw.timeout.request.v1"),
            "got {rendered}"
        );
        assert!(rendered.contains("no response in 2s"), "got {rendered}");
    }

    /// The data-plane failure classes of DESIGN.md §3.3 "Error Response
    /// Format" that the proxy relies on (413 too large, 502 downstream, 504
    /// request timeout) and the 403 CORS rejection of ADR 0004.
    #[test]
    fn test_proxy_error_kinds_match_the_design_error_table() {
        assert_eq!(
            GatewayErrorKind::PayloadTooLarge.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        );
        assert_eq!(GatewayErrorKind::PayloadTooLarge.status(), 413);

        assert_eq!(
            GatewayErrorKind::DownstreamError.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
        );
        assert_eq!(GatewayErrorKind::DownstreamError.status(), 502);

        assert_eq!(
            GatewayErrorKind::UpstreamTimeout.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        );
        assert_eq!(GatewayErrorKind::UpstreamTimeout.status(), 504);

        assert_eq!(
            GatewayErrorKind::CorsOriginNotAllowed.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
        );
        assert_eq!(GatewayErrorKind::CorsOriginNotAllowed.status(), 403);

        assert_eq!(
            GatewayErrorKind::CorsMethodNotAllowed.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
        );
        assert_eq!(GatewayErrorKind::CorsMethodNotAllowed.status(), 403);

        assert_eq!(
            GatewayErrorKind::RouteNotFound.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        );

        assert_eq!(
            GatewayErrorKind::LinkUnavailable.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        );
        assert_eq!(GatewayErrorKind::LinkUnavailable.status(), 503);
        assert_eq!(
            GatewayErrorKind::LinkUnavailable.title(),
            "Upstream Link Unavailable"
        );
    }
}
