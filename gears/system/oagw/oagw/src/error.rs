//! The OAGW error model.
//!
//! Every gateway-generated error is an RFC 9457 problem document served as
//! `application/problem+json`, carrying the GTS error identifier from the component's
//! status table in `type` and the OAGW extension fields (`upstream_id`, `host`, `path`,
//! `retry_after_seconds`, `trace_id`) inside the canonical `context` object. Errors are
//! also expressed in the platform's shared [`CanonicalError`] model so the management
//! surface stays consistent with the rest of the server.

use toolkit_canonical_errors::{CanonicalError, Problem, resource_error};
use toolkit_gts::gts_id;

/// Value of `X-OAGW-Error-Source` on every response the gateway generates itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of `X-OAGW-Error-Source` on every response relayed from an upstream,
/// including relayed upstream failures.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";
/// Header that distinguishes a gateway-generated response from a relayed one.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Header naming the endpoint a request was pinned to (never forwarded upstream).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// Header carrying the gateway's per-request correlation identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Header carrying the matched route pattern on a proxied response.
pub const ROUTE_HEADER: &str = "x-oagw-route";

/// Resource marker for the canonical error model projection.
#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub(crate) struct OagwResource;

/// GTS error type identifiers, one per entry in the gateway's status table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    ValidationError,
    MissingTargetHost,
    InvalidTargetHost,
    UnknownTargetHost,
    AuthenticationFailed,
    RouteNotFound,
    AlreadyExists,
    PluginInUse,
    PayloadTooLarge,
    RateLimitExceeded,
    CorsOriginNotAllowed,
    CorsMethodNotAllowed,
    SecretNotFound,
    DownstreamError,
    ProtocolError,
    StreamAborted,
    LinkUnavailable,
    CircuitBreakerOpen,
    PluginNotFound,
    ConnectionTimeout,
    RequestTimeout,
    IdleTimeout,
    Internal,
}

impl ErrorKind {
    /// Canonical GTS instance identifier for this error kind.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::ValidationError => gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1"),
            Self::MissingTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1")
            }
            Self::InvalidTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1")
            }
            Self::UnknownTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1")
            }
            Self::AuthenticationFailed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1")
            }
            Self::RouteNotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1"),
            Self::AlreadyExists => gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.exists.v1"),
            Self::PluginInUse => gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"),
            Self::PayloadTooLarge => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
            }
            Self::RateLimitExceeded => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
            }
            Self::CorsOriginNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
            }
            Self::CorsMethodNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1")
            }
            Self::SecretNotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"),
            Self::DownstreamError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
            }
            Self::ProtocolError => gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1"),
            Self::StreamAborted => gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"),
            Self::LinkUnavailable => gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"),
            Self::CircuitBreakerOpen => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1")
            }
            Self::PluginNotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"),
            Self::ConnectionTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1")
            }
            Self::RequestTimeout => gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1"),
            Self::IdleTimeout => gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"),
            Self::Internal => gts_id!("cf.core.errors.err.v1~cf.oagw.internal.error.v1"),
        }
    }

    /// `gts://` URI form of [`ErrorKind::gts_id`], as it appears in `Problem::type`.
    #[must_use]
    pub fn gts_uri(self) -> String {
        format!("gts://{}", self.gts_id())
    }

    /// Human-readable summary used as the problem document's `title`.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::ValidationError => "Validation failed",
            Self::MissingTargetHost => "Target host required",
            Self::InvalidTargetHost => "Invalid target host",
            Self::UnknownTargetHost => "Unknown target host",
            Self::AuthenticationFailed => "Authentication failed",
            Self::RouteNotFound => "Route not found",
            Self::AlreadyExists => "Resource already exists",
            Self::PluginInUse => "Plugin in use",
            Self::PayloadTooLarge => "Payload too large",
            Self::RateLimitExceeded => "Rate limit exceeded",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::SecretNotFound => "Secret not found",
            Self::DownstreamError => "Downstream error",
            Self::ProtocolError => "Protocol error",
            Self::StreamAborted => "Stream aborted",
            Self::LinkUnavailable => "Link unavailable",
            Self::CircuitBreakerOpen => "Circuit breaker open",
            Self::PluginNotFound => "Plugin not found",
            Self::ConnectionTimeout => "Connection timeout",
            Self::RequestTimeout => "Request timeout",
            Self::IdleTimeout => "Idle timeout",
            Self::Internal => "Internal error",
        }
    }

    /// HTTP status this error kind answers with.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::RouteNotFound => 404,
            Self::AlreadyExists | Self::PluginInUse => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => 403,
            Self::SecretNotFound | Self::Internal => 500,
            Self::DownstreamError | Self::ProtocolError | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }
}

fn number(value: u64) -> serde_json::Value {
    serde_json::Value::Number(serde_json::Number::from(value))
}

/// OAGW extension fields attached to a problem document.
#[derive(Debug, Clone, Default)]
pub struct Extensions {
    pub upstream_id: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub retry_after_seconds: Option<u64>,
    pub trace_id: Option<String>,
    /// The quota a throttled caller ran into, rendered as the `X-RateLimit-*` headers
    /// (ADR-0003): the bucket's capacity, what is left in it and when it refills.
    pub rate_limit: Option<RateLimitQuota>,
}

/// The quota figures a `429` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitQuota {
    /// The bucket's capacity: the burst the caller was entitled to.
    pub limit: u64,
    /// Tokens left after the rejected request.
    pub remaining: u64,
    /// Seconds until the bucket admits another request of this cost.
    pub reset_in_secs: u64,
}

impl Extensions {
    /// Serializes the extensions into a JSON object for the problem document's
    /// `context` field.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut ctx = serde_json::Map::new();
        if let Some(v) = &self.upstream_id {
            ctx.insert("upstream_id".to_owned(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = &self.host {
            ctx.insert("host".to_owned(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = &self.path {
            ctx.insert("path".to_owned(), serde_json::Value::String(v.clone()));
        }
        if let Some(v) = self.retry_after_seconds {
            ctx.insert(
                "retry_after_seconds".to_owned(),
                serde_json::Value::Number(serde_json::Number::from(v)),
            );
        }
        if let Some(v) = &self.trace_id {
            ctx.insert("trace_id".to_owned(), serde_json::Value::String(v.clone()));
        }
        if let Some(quota) = &self.rate_limit {
            ctx.insert("rate_limit_limit".to_owned(), number(quota.limit));
            ctx.insert("rate_limit_remaining".to_owned(), number(quota.remaining));
            ctx.insert("rate_limit_reset_in_secs".to_owned(), number(quota.reset_in_secs));
        }
        ctx
    }
}

/// A gateway error: an [`ErrorKind`] plus the human-readable detail and the request
/// context that the problem document's extension fields carry.
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: ErrorKind,
    detail: String,
    extensions: Extensions,
}

impl OagwError {
    /// Creates a new gateway error of `kind` with the supplied detail.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: Extensions::default(),
        }
    }

    /// Attaches the request context carried in the problem document's extensions.
    #[must_use]
    pub fn with_extensions(mut self, ext: Extensions) -> Self {
        self.extensions = ext;
        self
    }

    /// Names the upstream the failure is attached to.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: &str) -> Self {
        self.extensions.upstream_id = Some(upstream_id.to_owned());
        self
    }

    /// Names the host the failure is attached to.
    #[must_use]
    pub fn with_host(mut self, host: &str) -> Self {
        self.extensions.host = Some(host.to_owned());
        self
    }

    /// Names the path the failure is attached to.
    #[must_use]
    pub fn with_path(mut self, path: &str) -> Self {
        self.extensions.path = Some(path.to_owned());
        self
    }

    /// Attaches the quota a throttled caller ran into.
    #[must_use]
    pub fn with_rate_limit(mut self, quota: RateLimitQuota) -> Self {
        self.extensions.rate_limit = Some(quota);
        self
    }

    /// Adds retry guidance for a throttled or unavailable upstream.
    #[must_use]
    pub const fn with_retry_after(mut self, seconds: u64) -> Self {
        self.extensions.retry_after_seconds = Some(seconds);
        self
    }

    /// Names the correlation identifier the request was given, so the problem document's
    /// `trace_id` extension points an operator at the same trace the log record and the
    /// relayed response name (FR-029).
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: &str) -> Self {
        if !trace_id.is_empty() {
            self.extensions.trace_id = Some(trace_id.to_owned());
        }
        self
    }

    /// The error kind.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The extension fields carried on the wire.
    #[must_use]
    pub const fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Converts the error into the platform's shared canonical error model, preserving
    /// the documented HTTP status through a transport override where the canonical
    /// catalog has no native variant.
    #[must_use]
    pub fn to_canonical(&self) -> CanonicalError {
        match self.kind {
            // The canonical catalog has no 413 category; keep the documented status
            // through a transport override.
            ErrorKind::PayloadTooLarge => {
                return OagwResource::invalid_argument()
                    .with_field_violation("body", &self.detail, "PAYLOAD_TOO_LARGE")
                    .with_override(toolkit_canonical_errors::Http::status_code(413))
                    .create();
            }
            ErrorKind::ValidationError
            | ErrorKind::MissingTargetHost
            | ErrorKind::InvalidTargetHost
            | ErrorKind::UnknownTargetHost => OagwResource::invalid_argument()
                .with_field_violation("request", &self.detail, "VALIDATION_FAILED")
                .create(),
            // The gateway answers 401: the caller's credentials were rejected by the
            // upstream, which is an authentication failure, not an authorization one.
            ErrorKind::AuthenticationFailed => toolkit_canonical_errors::CanonicalError::unauthenticated()
                .with_reason("UPSTREAM_AUTHENTICATION_FAILED")
                .create(),
            ErrorKind::RouteNotFound => OagwResource::not_found(&self.detail)
                .with_resource("route")
                .create(),
            ErrorKind::AlreadyExists => OagwResource::already_exists(&self.detail)
                .with_resource("upstream")
                .create(),
            ErrorKind::PluginInUse => OagwResource::already_exists(&self.detail)
                .with_resource("plugin")
                .create(),
            ErrorKind::RateLimitExceeded => OagwResource::resource_exhausted(&self.detail)
                .with_quota_violation("requests", "RATE_LIMIT_EXCEEDED")
                .create(),
            // A CORS rejection is the caller's own policy violation, not an upstream
            // one; the canonical catalog has no 403 category, so the documented status
            // travels through a transport override.
            ErrorKind::CorsOriginNotAllowed | ErrorKind::CorsMethodNotAllowed => {
                OagwResource::unknown(&self.detail)
                    .with_override(toolkit_canonical_errors::Http::status_code(403))
                    .create()
            }
            ErrorKind::SecretNotFound | ErrorKind::Internal => {
                CanonicalError::internal(&self.detail).create()
            }
            // The canonical catalog has no 502 category; keep the documented status
            // through a transport override.
            ErrorKind::DownstreamError | ErrorKind::ProtocolError | ErrorKind::StreamAborted => {
                OagwResource::unknown(&self.detail)
                    .with_override(toolkit_canonical_errors::Http::status_code(502))
                    .create()
            }
            ErrorKind::LinkUnavailable
            | ErrorKind::CircuitBreakerOpen
            | ErrorKind::PluginNotFound => CanonicalError::service_unavailable()
                .with_detail(&self.detail)
                .create(),
            ErrorKind::ConnectionTimeout | ErrorKind::RequestTimeout | ErrorKind::IdleTimeout => {
                OagwResource::deadline_exceeded(&self.detail).create()
            }
        }
    }

    /// Renders the error as an RFC 9457 problem document, filling `instance` and the
    /// OAGW extension fields. `type` is the documented GTS error identifier, not the
    /// canonical category URI.
    #[must_use]
    pub fn to_problem(&self, instance: Option<String>) -> Problem {
        let mut ctx = self.extensions.to_json();
        if ctx.is_empty() {
            ctx.insert("data".to_owned(), serde_json::json!({}));
        }
        Problem {
            problem_type: self.kind.gts_uri(),
            title: self.kind.title().to_owned(),
            status: self.kind.status(),
            detail: self.detail.clone(),
            instance,
            trace_id: self.extensions.trace_id.clone(),
            context: serde_json::Value::Object(ctx),
            error_code: None,
            error_domain: None,
        }
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.detail, self.kind.gts_id())
    }
}

impl std::error::Error for OagwError {}

impl From<OagwError> for CanonicalError {
    fn from(err: OagwError) -> Self {
        err.to_canonical()
    }
}

impl axum::response::IntoResponse for OagwError {
    fn into_response(self) -> axum::response::Response {
        let status = axum::http::StatusCode::from_u16(self.kind.status())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = match serde_json::to_vec(&self.to_problem(None)) {
            Ok(bytes) => bytes,
            Err(_) => self.detail.clone().into_bytes(),
        };
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        if let Ok(name) = axum::http::HeaderName::from_bytes(ERROR_SOURCE_HEADER.as_bytes()) {
            headers.insert(
                name,
                axum::http::HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
            );
        }

        if let Some(seconds) = self.extensions.retry_after_seconds {
            if let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        // ADR-0003 defaults `response_headers` on: a throttled caller is told the quota
        // it ran into, in the headers a client can read without parsing the body.
        if let Some(quota) = &self.extensions.rate_limit {
            let headers = response.headers_mut();
            for (name, value) in [
                ("x-ratelimit-limit", quota.limit.to_string()),
                ("x-ratelimit-remaining", quota.remaining.to_string()),
                ("x-ratelimit-reset", quota.reset_in_secs.to_string()),
            ] {
                if let Ok(name) = axum::http::HeaderName::from_bytes(name.as_bytes()) {
                    if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
                        headers.insert(name, value);
                    }
                }
            }
        }
        response
    }
}
