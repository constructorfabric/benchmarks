//! Domain errors and their RFC 9457 wire rendering.
//!
//! Every gateway-originated failure is a [`DomainError`]. Its `IntoResponse`
//! produces an `application/problem+json` body whose `type` is one of the
//! OAGW GTS instance identifiers from `DESIGN.md §3.3`, and which always
//! carries `X-OAGW-Error-Source: gateway` (ADR 0007).
//!
//! Upstream failures are *not* `DomainError`s: the upstream response is passed
//! through byte-for-byte with `X-OAGW-Error-Source: upstream`.

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::ids::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_TYPE_PREFIX};

/// `Content-Type` of every gateway-originated error body.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// The set of failures the gateway can produce, with their HTTP status and
/// GTS problem `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Request body / configuration did not validate.
    Validation,
    /// `X-OAGW-Target-Host` is required but absent.
    MissingTargetHost,
    /// `X-OAGW-Target-Host` is not a bare hostname or IP.
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` names no configured endpoint.
    UnknownTargetHost,
    /// Upstream authentication could not be established.
    Authentication,
    /// No route matched the request.
    RouteNotFound,
    /// A uniqueness constraint was violated.
    Conflict,
    /// A plugin is still referenced by an upstream or a route.
    PluginInUse,
    /// Request body exceeds the configured maximum.
    PayloadTooLarge,
    /// A rate-limit bucket refused the request.
    RateLimit,
    /// A `secret_ref` could not be resolved from the credential store.
    SecretNotFound,
    /// Upstream spoke a protocol we cannot handle.
    Protocol,
    /// The upstream service failed.
    Downstream,
    /// An established stream was interrupted.
    StreamAborted,
    /// No healthy link to the upstream.
    LinkUnavailable,
    /// The circuit breaker for this upstream is open.
    CircuitBreakerOpen,
    /// A referenced plugin is not registered.
    PluginNotFound,
    /// Establishing the upstream connection timed out.
    ConnectionTimeout,
    /// The upstream did not produce a response in time.
    RequestTimeout,
    /// An idle streaming connection timed out.
    IdleTimeout,
}

impl ErrorKind {
    /// The GTS problem `type` identifier (the bare id form used on the wire).
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Validation => "validation.error.v1",
            Self::MissingTargetHost => "routing.missing_target_host.v1",
            Self::InvalidTargetHost => "routing.invalid_target_host.v1",
            Self::UnknownTargetHost => "routing.unknown_target_host.v1",
            Self::Authentication => "auth.failed.v1",
            Self::RouteNotFound => "route.not_found.v1",
            Self::Conflict => "conflict.v1",
            Self::PluginInUse => "plugin.in_use.v1",
            Self::PayloadTooLarge => "payload.too_large.v1",
            Self::RateLimit => "rate_limit.exceeded.v1",
            Self::SecretNotFound => "secret.not_found.v1",
            Self::Protocol => "protocol.error.v1",
            Self::Downstream => "downstream.error.v1",
            Self::StreamAborted => "stream.aborted.v1",
            Self::LinkUnavailable => "link.unavailable.v1",
            Self::CircuitBreakerOpen => "circuit_breaker.open.v1",
            Self::PluginNotFound => "plugin.not_found.v1",
            Self::ConnectionTimeout => "timeout.connection.v1",
            Self::RequestTimeout => "timeout.request.v1",
            Self::IdleTimeout => "timeout.idle.v1",
        }
    }

    /// The HTTP status this failure is reported with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::Authentication => 401,
            Self::RouteNotFound => 404,
            Self::Conflict => 409,
            Self::PluginInUse => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimit => 429,
            Self::SecretNotFound => 500,
            Self::Protocol | Self::Downstream | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen => 503,
            Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// Human-readable summary, matching the wording in the accepted ADRs.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::Authentication => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::Conflict => "Conflict",
            Self::PluginInUse => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimit => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::Protocol => "Protocol Error",
            Self::Downstream => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::LinkUnavailable => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }

    /// `true` for the failures the spec marks "Retriable".
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit
                | Self::LinkUnavailable
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

/// A gateway-originated failure, rendered as an RFC 9457 problem body.
///
/// Extension fields (`upstream_id`, `host`, `path`, `valid_hosts`, …) are kept
/// verbatim at the top level of the body — the specification shows them there
/// (ADR 0007 Appendix A), so they must not be folded into `context`.
#[derive(Debug, Clone)]
pub struct DomainError {
    kind: ErrorKind,
    detail: String,
    extensions: Vec<(String, Value)>,
    retry_after_seconds: Option<u64>,
    status_override: Option<u16>,
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.kind.gts_type(), self.detail)
    }
}

impl std::error::Error for DomainError {}

impl DomainError {
    /// Build a failure of `kind` with a human-readable explanation.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: Vec::new(),
            retry_after_seconds: None,
            status_override: None,
        }
    }

    /// Attach a top-level extension field to the problem body.
    #[must_use]
    pub fn with_extension(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.extensions.push((name.to_owned(), value.into()));
        self
    }

    /// Attach the `upstream_id` context field (full GTS instance id form).
    #[must_use]
    pub fn with_upstream_id(self, upstream_id: uuid::Uuid) -> Self {
        let instance = format!("{}{}", crate::ids::UPSTREAM_RESOURCE_TYPE, upstream_id);
        self.with_extension("upstream_id", instance)
    }

    /// Attach the `alias` context field.
    #[must_use]
    pub fn with_alias(self, alias: &str) -> Self {
        self.with_extension("alias", alias)
    }

    /// Attach the `host` context field.
    #[must_use]
    pub fn with_host(self, host: &str) -> Self {
        self.with_extension("host", host)
    }

    /// Emit `Retry-After` (header) and `retry_after_seconds` (body field).
    #[must_use]
    pub fn with_retry_after(self, seconds: u64) -> Self {
        Self {
            retry_after_seconds: Some(seconds),
            ..self
        }
    }

    /// The status this failure is reported with.
    ///
    /// # Errors
    ///
    /// ADR 0004 reports a disallowed CORS origin or method with `403`, which is
    /// below the taxonomy's granularity; `with_status` records it without
    /// inventing a problem `type` the specification does not list.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self.status_override {
            Some(status) => status,
            None => self.kind.status(),
        }
    }

    /// Report this failure with a different HTTP status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status_override = Some(status);
        self
    }

    /// The failure category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Render the problem body without the `instance` / `trace_id` fields,
    /// which the route layer fills from the inbound request.
    #[must_use]
    pub fn problem_body(&self) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert(
            "type".to_owned(),
            Value::String(format!("{}{}", ERROR_TYPE_PREFIX, self.kind.gts_type())),
        );
        body.insert(
            "title".to_owned(),
            Value::String(self.kind.title().to_owned()),
        );
        body.insert("status".to_owned(), Value::from(self.status()));
        body.insert("detail".to_owned(), Value::String(self.detail.clone()));
        for (name, value) in &self.extensions {
            body.insert(name.clone(), value.clone());
        }
        if let Some(seconds) = self.retry_after_seconds {
            body.insert("retry_after_seconds".to_owned(), Value::from(seconds));
        }
        body
    }

    /// Render this failure as a `serde_json::Value` with every field filled.
    #[must_use]
    pub fn to_problem(&self, instance: Option<&str>, trace_id: Option<&str>) -> Value {
        let mut body = self.problem_body();
        if let Some(instance) = instance {
            body.entry("instance".to_owned())
                .or_insert_with(|| Value::String(instance.to_owned()));
        }
        if let Some(trace_id) = trace_id {
            body.entry("trace_id".to_owned())
                .or_insert_with(|| Value::String(trace_id.to_owned()));
        }
        Value::Object(body)
    }

    /// `true` when this failure is one of the 4xx client errors.
    #[must_use]
    pub const fn is_client_error(&self) -> bool {
        let status = self.status();
        status >= 400 && status < 500
    }

    /// Log the failure at the level DESIGN §3.5 prescribes, without ever
    /// including credential material (`detail` is written by this crate and
    /// never carries secret values).
    pub fn log(&self) {
        let kind = self.kind;
        if self.is_client_error() {
            tracing::warn!(status = self.status(), problem_type = kind.gts_type(), detail = %self.detail, "oagw gateway error (client)");
        } else {
            tracing::error!(status = self.status(), problem_type = kind.gts_type(), detail = %self.detail, "oagw gateway error (server)");
        }
    }
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        self.log();
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = self.to_problem(None, None).to_string();
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON))
            .header(
                HeaderName::from_static(ERROR_SOURCE_HEADER),
                HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
            )
            .body(Body::from(body))
            .unwrap_or_else(|_| Response::new(Body::empty()));

        let retry_after = self
            .retry_after_seconds
            .or_else(|| default_retry_after(self.kind));
        if let Some(seconds) = retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        response
    }
}

/// Default `Retry-After` for the retriable failure kinds, in seconds.
const fn default_retry_after(kind: ErrorKind) -> Option<u64> {
    match kind {
        ErrorKind::CircuitBreakerOpen => Some(30),
        ErrorKind::LinkUnavailable => Some(5),
        ErrorKind::ConnectionTimeout | ErrorKind::RequestTimeout | ErrorKind::IdleTimeout => {
            Some(5)
        }
        _ => None,
    }
}

/// Convert an `anyhow::Error` into an internal [`DomainError`].
#[must_use]
pub fn from_anyhow(kind: ErrorKind, err: &anyhow::Error) -> DomainError {
    DomainError::new(kind, err.to_string())
}
