//! OAGW error model.
//!
//! Every gateway-produced failure is rendered as an RFC 9457
//! `application/problem+json` response carrying the crate's normative GTS
//! `type` identifier (see `DESIGN.md` §3.3 "Error Response Format") and the
//! `X-OAGW-Error-Source: gateway` header (`ADR 0007`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Content type used for every gateway-generated error body.
pub const PROBLEM_JSON: &str = "application/problem+json";
/// Header distinguishing gateway-originated from relayed upstream errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] for errors produced by the gateway.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] for responses relayed from an upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";
/// Header prefix used for all OAGW-specific transport headers.
pub const OAGW_HEADER_PREFIX: &str = "x-oagw-";

/// Identifier of one gateway error class: its GTS `type`, HTTP status, and
/// RFC 9457 `title`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// General route / payload validation failure (400).
    Validation,
    /// A uniqueness invariant of the configuration model is already claimed
    /// (`(tenant, alias)` for an upstream, `(path, method)` for a route): the
    /// request is well formed but cannot be applied (409).
    Conflict,
    /// `X-OAGW-Target-Host` missing on a multi-endpoint common-suffix alias.
    MissingTargetHost,
    /// `X-OAGW-Target-Host` malformed.
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` not one of the configured endpoints.
    UnknownTargetHost,
    /// Inbound or upstream authentication failed.
    AuthenticationFailed,
    /// CORS origin rejected on an actual (non-preflight) request.
    CorsOriginNotAllowed,
    /// CORS method rejected on an actual (non-preflight) request.
    CorsMethodNotAllowed,
    /// No route matched the request.
    RouteNotFound,
    /// Referenced resource is in use and cannot be deleted.
    PluginInUse,
    /// Request body exceeded the configured limit.
    PayloadTooLarge,
    /// Rate limit exhausted.
    RateLimitExceeded,
    /// A referenced credential could not be resolved.
    SecretNotFound,
    /// Protocol-level failure while talking to the upstream.
    ProtocolError,
    /// Upstream returned an error (relayed status is preserved by the caller
    /// when it decides to pass through; this variant is the gateway-side
    /// classification).
    DownstreamError,
    /// Streaming response aborted mid-flight.
    StreamAborted,
    /// Upstream link could not be established.
    LinkUnavailable,
    /// Circuit breaker open.
    CircuitBreakerOpen,
    /// A configured plugin identifier has no backing implementation.
    PluginNotFound,
    /// Connection establishment timed out.
    ConnectionTimeout,
    /// Upstream did not complete within `proxy_timeout_secs`.
    RequestTimeout,
    /// Idle timeout while relaying a stream.
    IdleTimeout,
}

impl ErrorKind {
    /// Normative GTS `type` identifier (the instance part of
    /// `gts.cf.core.errors.err.v1~<fragment>`).
    #[must_use]
    pub fn gts_fragment(self) -> &'static str {
        match self {
            Self::Validation => "cf.oagw.validation.error.v1",
            Self::Conflict => "cf.oagw.conflict.v1",
            Self::MissingTargetHost => "cf.oagw.routing.missing_target_host.v1",
            Self::InvalidTargetHost => "cf.oagw.routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "cf.oagw.routing.unknown_target_host.v1",
            Self::AuthenticationFailed => "cf.oagw.auth.failed.v1",
            Self::CorsOriginNotAllowed => "cf.oagw.cors.origin_not_allowed.v1",
            Self::CorsMethodNotAllowed => "cf.oagw.cors.method_not_allowed.v1",
            Self::RouteNotFound => "cf.oagw.route.not_found.v1",
            Self::PluginInUse => "cf.oagw.plugin.in_use.v1",
            Self::PayloadTooLarge => "cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "cf.oagw.secret.not_found.v1",
            Self::ProtocolError => "cf.oagw.protocol.error.v1",
            Self::DownstreamError => "cf.oagw.downstream.error.v1",
            Self::StreamAborted => "cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "cf.oagw.timeout.request.v1",
            Self::IdleTimeout => "cf.oagw.timeout.idle.v1",
        }
    }

    /// Full GTS `type` URI emitted in the `type` member of the problem body.
    #[must_use]
    pub fn gts_type(self) -> String {
        format!("gts.cf.core.errors.err.v1~{}", self.gts_fragment())
    }

    /// HTTP status the error class maps to.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::Conflict | Self::PluginInUse => StatusCode::CONFLICT,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
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

    /// RFC 9457 `title`.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::Conflict => "Conflict",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::RouteNotFound => "Route Not Found",
            Self::PluginInUse => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
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

    /// Stable machine-readable code, also emitted as `context.error_code`.
    #[must_use]
    pub fn error_code(self) -> &'static str {
        match self {
            Self::Validation => "VALIDATION_ERROR",
            Self::Conflict => "CONFLICT",
            Self::MissingTargetHost => "MISSING_TARGET_HOST",
            Self::InvalidTargetHost => "INVALID_TARGET_HOST",
            Self::UnknownTargetHost => "UNKNOWN_TARGET_HOST",
            Self::AuthenticationFailed => "AUTHENTICATION_FAILED",
            Self::CorsOriginNotAllowed => "CORS_ORIGIN_NOT_ALLOWED",
            Self::CorsMethodNotAllowed => "CORS_METHOD_NOT_ALLOWED",
            Self::RouteNotFound => "ROUTE_NOT_FOUND",
            Self::PluginInUse => "PLUGIN_IN_USE",
            Self::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            Self::RateLimitExceeded => "RATE_LIMIT_EXCEEDED",
            Self::SecretNotFound => "SECRET_NOT_FOUND",
            Self::ProtocolError => "PROTOCOL_ERROR",
            Self::DownstreamError => "DOWNSTREAM_ERROR",
            Self::StreamAborted => "STREAM_ABORTED",
            Self::LinkUnavailable => "LINK_UNAVAILABLE",
            Self::CircuitBreakerOpen => "CIRCUIT_BREAKER_OPEN",
            Self::PluginNotFound => "PLUGIN_NOT_FOUND",
            Self::ConnectionTimeout => "CONNECTION_TIMEOUT",
            Self::RequestTimeout => "REQUEST_TIMEOUT",
            Self::IdleTimeout => "IDLE_TIMEOUT",
        }
    }

    /// Whether a client may retry the request.
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

/// A gateway error: one [`ErrorKind`] plus its occurrence-specific payload.
///
/// Extension members from the ADR 0007 examples (`upstream_id`, `host`,
/// `valid_hosts`, `invalid_value`, `retry_after_seconds`, `error_code`) are
/// carried in `context`, the RFC 9457 extension member used by the platform's
/// `Problem` envelope.
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: ErrorKind,
    detail: String,
    context: serde_json::Value,
    retry_after_secs: Option<u64>,
    headers: Vec<(String, String)>,
    status_override: Option<StatusCode>,
}

impl OagwError {
    /// Build an error of `kind` with `detail` and an optional extension map.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            context: serde_json::Value::Object(serde_json::Map::new()),
            retry_after_secs: None,
            headers: Vec::new(),
            status_override: None,
        }
    }

    /// Render this error with `status` instead of its class's own.
    ///
    /// A plugin reports the status it wants answered with (a guard rejection
    /// carries a phase-specific one, `ADR 0009`), and the class stays the same:
    /// the GTS `type` and the `title` keep describing the failure class, only
    /// the HTTP status the caller sees is overridden.
    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status_override = Some(status);
        self
    }

    /// HTTP status this error renders with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status_override.unwrap_or_else(|| self.kind.status())
    }

    /// Attach a response header to the rendered problem response.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Response headers attached to this error.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Attach an extension member under `context`.
    #[must_use]
    pub fn with_context(mut self, key: &str, value: serde_json::Value) -> Self {
        if let serde_json::Value::Object(map) = &mut self.context {
            map.insert(key.to_owned(), value);
        }
        self
    }

    /// Attach the `Retry-After` guidance (seconds) and the matching
    /// `context.retry_after_seconds` member.
    #[must_use]
    pub fn with_retry_after_secs(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self.with_context("retry_after_seconds", serde_json::json!(secs))
    }

    /// Attach a reference to the upstream the failure belongs to, rendered as
    /// the upstream's GTS identifier (`ADR 0007` examples).
    #[must_use]
    pub fn with_upstream(self, id: uuid::Uuid) -> Self {
        self.with_context(
            "upstream_id",
            serde_json::Value::String(crate::domain::model::gts_id(
                crate::domain::model::UPSTREAM_GTS_BASE,
                id,
            )),
        )
    }

    /// Attach the resolved upstream host.
    #[must_use]
    pub fn with_host(self, host: &str) -> Self {
        self.with_context("host", serde_json::Value::String(host.to_owned()))
    }

    /// Error class.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Human-readable occurrence detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Render into a serialized RFC 9457 body.
    ///
    /// # Errors
    ///
    /// Never fails in practice: the body is built from plain strings and
    /// numbers, and a serialization fault falls back to a minimal body.
    pub fn to_problem(&self, instance: Option<&str>, trace_id: Option<&str>) -> Response {
        let mut body = serde_json::Map::new();
        body.insert("type".to_owned(), serde_json::json!(self.kind.gts_type()));
        body.insert("title".to_owned(), serde_json::json!(self.kind.title()));
        body.insert(
            "status".to_owned(),
            serde_json::json!(self.status().as_u16()),
        );
        body.insert("detail".to_owned(), serde_json::json!(self.detail));
        if !matches!(&self.context, serde_json::Value::Object(map) if map.contains_key("error_code"))
        {
            body.insert(
                "error_code".to_owned(),
                serde_json::json!(self.kind.error_code()),
            );
        }
        if let Some(instance) = instance {
            body.insert("instance".to_owned(), serde_json::json!(instance));
        }
        if let Some(trace_id) = trace_id {
            body.insert("trace_id".to_owned(), serde_json::json!(trace_id));
        }
        if let serde_json::Value::Object(ext) = &self.context {
            // ADR 0007 renders OAGW extension members at the top level. They
            // are mirrored under `context` as well because the platform's
            // canonical-error middleware re-serializes `application/
            // problem+json` into the platform `Problem` envelope, whose known
            // members are `type`/`title`/`status`/`detail`/`instance`/
            // `trace_id`/`context`/`error_code` — so the mirrored copy is what
            // survives when that middleware is in the chain.
            for (key, value) in ext {
                body.insert(key.clone(), value.clone());
            }
            body.insert("context".to_owned(), serde_json::Value::Object(ext.clone()));
        }

        let payload = serde_json::Value::Object(body).to_string();
        let status = self.status();
        let mut response = Response::new(axum::body::Body::from(payload));
        *response.status_mut() = status;
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = HeaderValue::from_str(&secs.to_string())
        {
            headers.insert(header::RETRY_AFTER, value);
        }
        for (name, value) in &self.headers {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        response
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        self.to_problem(None, None)
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.error_code(), self.detail)
    }
}

impl std::error::Error for OagwError {}

impl From<toolkit_http::HttpError> for OagwError {
    fn from(err: toolkit_http::HttpError) -> Self {
        use toolkit_http::HttpError;
        match &err {
            HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => {
                OagwError::new(ErrorKind::RequestTimeout, err.to_string())
            }
            HttpError::InvalidUri { .. } => {
                OagwError::new(ErrorKind::ProtocolError, err.to_string())
            }
            HttpError::Overloaded | HttpError::ServiceClosed => {
                OagwError::new(ErrorKind::LinkUnavailable, err.to_string())
            }
            // A body ceiling can only be hit while relaying an upstream's
            // response: the caller's own request is bounded before the relay
            // (`api/handlers.rs`), so the breach is a bad gateway, not a 413.
            HttpError::BodyTooLarge { .. } => {
                OagwError::new(ErrorKind::DownstreamError, err.to_string())
            }
            // A connection that never came up (refused, unresolved host) is an
            // unavailable link, not a protocol fault (`ADR 0007`).
            HttpError::Transport(_) => OagwError::new(ErrorKind::LinkUnavailable, err.to_string()),
            _ => OagwError::new(ErrorKind::ProtocolError, err.to_string()),
        }
    }
}

impl From<anyhow::Error> for OagwError {
    fn from(err: anyhow::Error) -> Self {
        OagwError::new(ErrorKind::ProtocolError, format!("{err:#}"))
    }
}

impl From<serde_json::Error> for OagwError {
    fn from(err: serde_json::Error) -> Self {
        OagwError::new(
            ErrorKind::Validation,
            format!("malformed JSON payload: {err}"),
        )
    }
}

/// Shorthand constructors for the most common gateway failures.
pub mod prelude {
    use super::{ErrorKind, OagwError};

    /// 400 validation failure.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> OagwError {
        OagwError::new(ErrorKind::Validation, detail)
    }

    /// 401 inbound/upstream authentication failure.
    #[must_use]
    pub fn auth_failed(detail: impl Into<String>) -> OagwError {
        OagwError::new(ErrorKind::AuthenticationFailed, detail)
    }

    /// 404 no route matched.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> OagwError {
        OagwError::new(ErrorKind::RouteNotFound, detail)
    }

    /// 409 a uniqueness invariant of the configuration model is already
    /// claimed (an upstream alias, a route match rule).
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> OagwError {
        OagwError::new(ErrorKind::Conflict, detail)
    }

    /// 503 upstream link unavailable.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> OagwError {
        OagwError::new(ErrorKind::LinkUnavailable, detail)
    }
}
