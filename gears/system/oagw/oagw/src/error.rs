//! OAGW error model (ADR-0007, DESIGN §3.3 "Error Response Format").
//!
//! Gateway-generated errors are RFC 9457 `application/problem+json` documents
//! whose `type` is a GTS identifier under
//! `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1`, plus the OAGW extension
//! fields (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`).
//! Every response — success or error — carries `X-OAGW-Error-Source` so a
//! client can tell a gateway error from a passthrough upstream error.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Content type of a gateway error body (RFC 9457).
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Response header distinguishing gateway from upstream responses (ADR-0007),
/// in the ASCII-lowercase form `HeaderName::from_static` requires.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Value written when OAGW itself produced the response.
pub const GATEWAY_ERROR_SOURCE: &str = "gateway";

/// Value written by the data plane when the upstream produced the response.
pub const UPSTREAM_ERROR_SOURCE: &str = "upstream";

/// GTS `type` of a 400 request-validation failure (malformed payload, bad
/// alias, endpoint/alias rule violation).
pub const VALIDATION_ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// GTS `type` of a 400 `X-OAGW-Target-Host` format failure.
pub const INVALID_TARGET_HOST_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// GTS `type` of a 404 (unknown resource, unknown alias, no matching route).
pub const ROUTE_NOT_FOUND_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// GTS `type` of a 400 `X-OAGW-Target-Host` requirement failure: a
/// multi-endpoint upstream whose alias is a common domain suffix needs the
/// header to disambiguate the endpoint.
pub const MISSING_TARGET_HOST_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// GTS `type` of a 400 `X-OAGW-Target-Host` that matches no configured endpoint.
pub const UNKNOWN_TARGET_HOST_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// GTS `type` of a 413 request body above `body_limit_bytes`.
pub const PAYLOAD_TOO_LARGE_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// GTS `type` of a 401 inbound authentication failure.
pub const AUTHENTICATION_FAILED_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// GTS `type` of a 429 rate-limit rejection.
pub const RATE_LIMIT_EXCEEDED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// GTS `type` of a 502 response the upstream did not speak as HTTP.
pub const PROTOCOL_ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// GTS `type` of a 502 response interrupted before it was complete.
pub const DOWNSTREAM_ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// GTS `type` of a 503 unresolved plugin reference.
pub const PLUGIN_NOT_FOUND_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// GTS `type` of a 503 upstream link the data plane may not (or cannot) use.
pub const LINK_UNAVAILABLE_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// GTS `type` of a 504 dial/response-header deadline.
pub const CONNECTION_TIMEOUT_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
/// GTS `type` of a 504 request deadline.
pub const REQUEST_TIMEOUT_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// GTS `type` of a 409 conflict.
///
/// DESIGN "Error Response Format" catalogs exactly one 409 type
/// (`cf.oagw.plugin.in_use.v1`), so every conflict carries it and the `reason`
/// extension member tells the causes apart for a programmatic client:
/// `ALIAS_CONFLICT` (the alias is taken), `PLUGIN_NAME_CONFLICT` (a plugin with
/// that name exists), `PLUGIN_IN_USE` (the plugin is still bound) and
/// `ROUTE_MATCH_CONFLICT` (an existing route already matches the same path for
/// an overlapping method set). No other 409 `type` is minted, because the table
/// is the wire contract and inventing types would leave a client matching on
/// `type` in the dark.
pub const CONFLICT_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// GTS `type` of a 500 internal failure.
pub const INTERNAL_ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1";
/// GTS `type` of a 403 CORS rejection of the `Origin` of an actual request
/// (ADR-0004 "Error Responses").
pub const CORS_ORIGIN_NOT_ALLOWED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// GTS `type` of a 403 CORS rejection of the method of an actual request
/// (ADR-0004 "Error Responses").
pub const CORS_METHOD_NOT_ALLOWED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// Error code of a `required_headers.v1` guard rejection (ADR-0009). The guard
/// has no GTS `type` of its own: ADR-0009 names a phase-specific HTTP status and
/// this code, which travels as the `reason` extension member of the 400 (request
/// phase) or 502 (response phase) problem document the DESIGN error table
/// already defines for those phases.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// `Retry-After` (RFC 9457 §"Retry-After"): when the request may be repeated.
pub const RETRY_AFTER_HEADER: HeaderName = HeaderName::from_static("retry-after");
/// `X-RateLimit-Limit`: the limit the request was counted against (ADR-0003).
pub const X_RATELIMIT_LIMIT_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-limit");
/// `X-RateLimit-Remaining`: the budget left after the request (ADR-0003).
pub const X_RATELIMIT_REMAINING_HEADER: HeaderName =
    HeaderName::from_static("x-ratelimit-remaining");
/// `X-RateLimit-Reset`: when the budget is back (ADR-0003).
pub const X_RATELIMIT_RESET_HEADER: HeaderName = HeaderName::from_static("x-ratelimit-reset");

/// Wire representation of an OAGW gateway error (RFC 9457).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OagwProblem {
    /// GTS identifier of the error type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation of this occurrence.
    pub detail: String,
    /// Request path that produced the error (RFC 9457 `instance`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// OAGW extension fields (ADR-0007).
    #[serde(flatten)]
    pub extensions: Box<ErrorExtensions>,
}

impl OagwProblem {
    /// Build a problem document for an error.
    #[must_use]
    pub fn from_error(error: &OagwError) -> Self {
        Self {
            problem_type: error.gts_type().to_owned(),
            title: error.title().to_owned(),
            status: error.status_code(),
            detail: error.detail.clone(),
            instance: error.instance.clone(),
            extensions: error.extensions.clone(),
        }
    }
}

/// OAGW-specific RFC 9457 extension members.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorExtensions {
    /// Identifier of the upstream involved, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// GTS identifier of the plugin involved, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Identifiers of the resources that still reference a plugin, when a
    /// deletion was refused (ADR-0001 "Plugin Deletion Behavior").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<Vec<String>>,
    /// Upstream alias involved, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Upstream host involved, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path that produced the error, when it differs from `instance`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Hosts the request could legitimately have targeted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// Rejected value (header value, alias, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    /// Machine-readable discriminator inside a GTS `type` (e.g.
    /// `ALIAS_CONFLICT` for a 409 that is not a plugin-in-use conflict).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Retry guidance in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Distributed-tracing correlation identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Category of a gateway error: fixes the GTS `type`, the HTTP status and the
/// RFC 9457 `title`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400 — request validation failed (payload, alias, endpoint rules).
    Validation,
    /// 400 — `X-OAGW-Target-Host` is not a bare hostname or IP.
    InvalidTargetHost,
    /// 400 — a multi-endpoint upstream with a common-suffix alias requires
    /// `X-OAGW-Target-Host` to disambiguate the endpoint (ADR-0001).
    MissingTargetHost,
    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    UnknownTargetHost,
    /// 401 — inbound authentication failed.
    AuthenticationFailed,
    /// 404 — resource, alias or route not found.
    RouteNotFound,
    /// 409 — conflict (alias already taken, plugin still in use).
    Conflict,
    /// 413 — request body above `body_limit_bytes`.
    PayloadTooLarge,
    /// 429 — rate limit exceeded (S4).
    RateLimitExceeded,
    /// 403 — the `Origin` of an actual cross-origin request is not allowed
    /// (S4, ADR-0004).
    CorsOriginNotAllowed,
    /// 403 — the method of an actual cross-origin request is not allowed
    /// (S4, ADR-0004).
    CorsMethodNotAllowed,
    /// 502 — the upstream did not answer with a parseable HTTP response.
    ProtocolError,
    /// 502 — the upstream dropped the connection mid-response.
    DownstreamError,
    /// 503 — a plugin referenced by a binding does not resolve.
    PluginNotFound,
    /// 503 — the upstream link is unavailable (disabled, refused, unreachable).
    LinkUnavailable,
    /// 504 — dial or response-header deadline exceeded.
    ConnectionTimeout,
    /// 504 — request deadline exceeded.
    RequestTimeout,
    /// 500 — unexpected internal failure.
    Internal,
}

impl ErrorKind {
    /// GTS identifier carried in the problem `type` field.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Validation => VALIDATION_ERROR_TYPE,
            Self::InvalidTargetHost => INVALID_TARGET_HOST_TYPE,
            Self::MissingTargetHost => MISSING_TARGET_HOST_TYPE,
            Self::UnknownTargetHost => UNKNOWN_TARGET_HOST_TYPE,
            Self::AuthenticationFailed => AUTHENTICATION_FAILED_TYPE,
            Self::RouteNotFound => ROUTE_NOT_FOUND_TYPE,
            Self::Conflict => CONFLICT_TYPE,
            Self::PayloadTooLarge => PAYLOAD_TOO_LARGE_TYPE,
            Self::RateLimitExceeded => RATE_LIMIT_EXCEEDED_TYPE,
            Self::CorsOriginNotAllowed => CORS_ORIGIN_NOT_ALLOWED_TYPE,
            Self::CorsMethodNotAllowed => CORS_METHOD_NOT_ALLOWED_TYPE,
            Self::ProtocolError => PROTOCOL_ERROR_TYPE,
            Self::DownstreamError => DOWNSTREAM_ERROR_TYPE,
            Self::PluginNotFound => PLUGIN_NOT_FOUND_TYPE,
            Self::LinkUnavailable => LINK_UNAVAILABLE_TYPE,
            Self::ConnectionTimeout => CONNECTION_TIMEOUT_TYPE,
            Self::RequestTimeout => REQUEST_TIMEOUT_TYPE,
            Self::Internal => INTERNAL_ERROR_TYPE,
        }
    }

    /// RFC 9457 `title`.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::InvalidTargetHost => "Invalid Target Host",
            Self::MissingTargetHost => "Missing Target Host",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::RouteNotFound => "Not Found",
            Self::Conflict => "Conflict",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::PluginNotFound => "Plugin Not Found",
            Self::LinkUnavailable => "Link Unavailable",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::Internal => "Internal Error",
        }
    }

    /// HTTP status code.
    #[must_use]
    pub const fn status_code(self) -> u16 {
        match self {
            Self::Validation
            | Self::InvalidTargetHost
            | Self::MissingTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::RouteNotFound => 404,
            Self::Conflict => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => 403,
            Self::ProtocolError | Self::DownstreamError => 502,
            Self::PluginNotFound | Self::LinkUnavailable => 503,
            Self::ConnectionTimeout | Self::RequestTimeout => 504,
            Self::Internal => 500,
        }
    }

    /// HTTP status as an axum [`StatusCode`].
    const fn status(self) -> StatusCode {
        match self {
            Self::Validation
            | Self::InvalidTargetHost
            | Self::MissingTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
            Self::ProtocolError | Self::DownstreamError => StatusCode::BAD_GATEWAY,
            Self::PluginNotFound | Self::LinkUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::ConnectionTimeout | Self::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A gateway-generated error: a [`Problem`](OagwProblem) plus the
/// `X-OAGW-Error-Source: gateway` transport behaviour.
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: ErrorKind,
    detail: String,
    instance: Option<String>,
    extensions: Box<ErrorExtensions>,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
}

static ERROR_SOURCE_HEADER_NAME: HeaderName = HeaderName::from_static(ERROR_SOURCE_HEADER);

impl OagwError {
    /// 400 — the request payload violates the resource schema or one of the
    /// documented business rules (alias derivation, immutability, …).
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, detail)
    }

    /// 400 — `X-OAGW-Target-Host` is not a bare hostname or IP address.
    #[must_use]
    pub fn invalid_target_host(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidTargetHost, detail)
    }

    /// 400 — a multi-endpoint upstream whose alias is a common domain suffix
    /// needs `X-OAGW-Target-Host` to disambiguate the endpoint (ADR-0001).
    #[must_use]
    pub fn missing_target_host(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::MissingTargetHost, detail)
    }

    /// 400 — `X-OAGW-Target-Host` names no configured endpoint of the upstream.
    #[must_use]
    pub fn unknown_target_host(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownTargetHost, detail)
    }

    /// 401 — the caller is not authenticated to this surface.
    #[must_use]
    pub fn authentication_failed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::AuthenticationFailed, detail)
    }

    /// 413 — the request body is above the configured limit.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PayloadTooLarge, detail)
    }

    /// 429 — the rate limit for this surface is exhausted (S4).
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RateLimitExceeded, detail)
    }

    /// 403 — the `Origin` of an actual cross-origin request is not in the
    /// upstream's `allowed_origins` (S4, ADR-0004).
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorsOriginNotAllowed, detail)
    }

    /// 403 — the method of an actual cross-origin request is not in the
    /// upstream's `allowed_methods` (S4, ADR-0004).
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorsMethodNotAllowed, detail)
    }

    /// 502 — the upstream answered with something that is not a parseable HTTP
    /// response.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ProtocolError, detail)
    }

    /// 502 — the upstream dropped the connection before the response was
    /// complete.
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::DownstreamError, detail)
    }

    /// 503 — a plugin referenced by a binding does not resolve.
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PluginNotFound, detail)
    }

    /// 503 — the upstream link exists but may not (or cannot) be used: the
    /// upstream is disabled, or a plaintext connection is not allowed.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::LinkUnavailable, detail)
    }

    /// 504 — the dial or the response headers did not arrive within the
    /// configured deadline.
    #[must_use]
    pub fn connection_timeout(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ConnectionTimeout, detail)
    }

    /// 504 — the request did not complete within the configured deadline.
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RequestTimeout, detail)
    }

    /// 404 — the addressed resource (or the proxy alias / route) does not
    /// exist for the calling tenant.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RouteNotFound, detail)
    }

    /// 409 — the request conflicts with existing state. `reason` should
    /// discriminate the concrete case (`ALIAS_CONFLICT`, `PLUGIN_IN_USE`).
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, detail)
    }

    /// 500 — unexpected internal failure. `diagnostic` is logged server-side
    /// and must never carry secret material.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, diagnostic)
    }

    fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            extensions: Box::default(),
            extra_headers: Vec::new(),
        }
    }

    /// Attach a response header the error must travel with.
    ///
    /// A problem document cannot carry every header a rejection has to send —
    /// `Retry-After` and the `X-RateLimit-*` family of a 429 (ADR-0003) are
    /// response headers, not body members — so an error may carry them beside
    /// its problem document.
    #[must_use]
    pub fn with_response_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.extra_headers.push((name, value));
        self
    }

    /// The response headers this error travels with, beyond the content type
    /// and the error source every gateway error carries.
    #[must_use]
    pub fn extra_headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.extra_headers
    }

    /// Attach the RFC 9457 `instance` (the request path).
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attach the identifier of the upstream involved.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.extensions.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attach the upstream alias involved.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.extensions.alias = Some(alias.into());
        self
    }

    /// Attach the upstream host involved.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.extensions.host = Some(host.into());
        self
    }

    /// Attach the GTS identifier of the plugin involved.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.extensions.plugin_id = Some(plugin_id.into());
        self
    }

    /// Attach the identifiers of the resources that still reference a plugin.
    #[must_use]
    pub fn with_referenced_by(mut self, referenced_by: Vec<String>) -> Self {
        self.extensions.referenced_by = Some(referenced_by);
        self
    }

    /// Attach the request path that produced the error.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.extensions.path = Some(path.into());
        self
    }

    /// Attach the list of hosts the request could have targeted.
    #[must_use]
    pub fn with_valid_hosts(mut self, valid_hosts: Vec<String>) -> Self {
        self.extensions.valid_hosts = Some(valid_hosts);
        self
    }

    /// Attach the rejected value.
    #[must_use]
    pub fn with_invalid_value(mut self, invalid_value: impl Into<String>) -> Self {
        self.extensions.invalid_value = Some(invalid_value.into());
        self
    }

    /// Attach a machine-readable discriminator for the GTS `type`.
    ///
    /// Used where the design catalogs one `type` for several causes — the 409
    /// conflicts above, `ALIAS_CONFLICT` / `PLUGIN_IN_USE` / … — so a client
    /// matching on `type` still learns which conflict it hit.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.extensions.reason = Some(reason.into());
        self
    }

    /// Attach retry guidance in seconds.
    #[must_use]
    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.extensions.retry_after_seconds = Some(seconds);
        self
    }

    /// Attach a tracing correlation identifier.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.extensions.trace_id = Some(trace_id.into());
        self
    }

    /// Error category (GTS `type`, status and title selector).
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// GTS identifier carried in the problem `type` field.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        self.kind.gts_type()
    }

    /// RFC 9457 `title`.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        self.kind.title()
    }

    /// HTTP status code.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.kind.status_code()
    }

    /// Human-readable explanation of this occurrence.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The OAGW extension members carried by this error.
    #[must_use]
    pub fn extensions(&self) -> &ErrorExtensions {
        &self.extensions
    }

    /// Render the RFC 9457 problem document for this error.
    #[must_use]
    pub fn problem(&self) -> OagwProblem {
        OagwProblem::from_error(self)
    }

    /// `true` when this error is a client error (4xx).
    #[must_use]
    pub const fn is_client_error(&self) -> bool {
        let status = self.kind.status_code();
        status >= 400 && status < 500
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {} ({})",
            self.kind.status_code(),
            self.kind.gts_type(),
            self.detail
        )
    }
}

impl std::error::Error for OagwError {}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let mut response = (self.kind.status(), axum::Json(self.problem())).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        headers.insert(
            &ERROR_SOURCE_HEADER_NAME,
            HeaderValue::from_static(GATEWAY_ERROR_SOURCE),
        );
        for (name, value) in &self.extra_headers {
            headers.insert(name, value.clone());
        }
        response
    }
}

/// Handler result alias for the OAGW transport layer: the error is always a
/// gateway-generated [`OagwError`].
pub type ApiResult<T, E = OagwError> = Result<T, E>;

/// Attach `X-OAGW-Error-Source: gateway` to a response that does not carry the
/// header yet (ADR-0007 requires it on every OAGW response, successes included).
pub fn with_error_source(response: Response) -> Response {
    if response.headers().contains_key(&ERROR_SOURCE_HEADER_NAME) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        &ERROR_SOURCE_HEADER_NAME,
        HeaderValue::from_static(GATEWAY_ERROR_SOURCE),
    );
    Response::from_parts(parts, body)
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
