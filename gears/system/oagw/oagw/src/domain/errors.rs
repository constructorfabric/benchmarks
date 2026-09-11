// Created: 2026-09-01 by Constructor Tech
//! Error catalog.
//!
//! `docs/DESIGN.md` §3.3 fixes both the HTTP status and the GTS `type`
//! identifier for every OAGW failure. The canonical error catalog cannot
//! express those identifiers: [`CanonicalError::gts_type`] returns the
//! category's own id (`cf.core.err.*`) and never consults the resource type
//! a builder was handed, so a gear-specific `type` cannot reach the wire
//! through it. OAGW therefore carries its own [`OagwError`], projected to an
//! RFC 9457 `application/problem+json` body by its `IntoResponse`
//! implementation — with `X-OAGW-Error-Source: gateway` set, and
//! `Retry-After` on the retriable kinds.
//!
//! The table is the contract:
//!
//! | Error | HTTP | GTS type |
//! |---|---|---|
//! | ValidationError | 400 | `…cf.oagw.validation.error.v1` |
//! | MissingTargetHost | 400 | `…cf.oagw.routing.missing_target_host.v1` |
//! | InvalidTargetHost | 400 | `…cf.oagw.routing.invalid_target_host.v1` |
//! | UnknownTargetHost | 400 | `…cf.oagw.routing.unknown_target_host.v1` |
//! | AuthenticationFailed | 401 | `…cf.oagw.auth.failed.v1` |
//! | RouteNotFound | 404 | `…cf.oagw.route.not_found.v1` |
//! | PluginInUse | 409 | `…cf.oagw.plugin.in_use.v1` |
//! | PayloadTooLarge | 413 | `…cf.oagw.payload.too_large.v1` |
//! | RateLimitExceeded | 429 | `…cf.oagw.rate_limit.exceeded.v1` |
//! | SecretNotFound | 500 | `…cf.oagw.secret.not_found.v1` |
//! | ProtocolError | 502 | `…cf.oagw.protocol.error.v1` |
//! | DownstreamError | 502 | `…cf.oagw.downstream.error.v1` |
//! | StreamAborted | 502 | `…cf.oagw.stream.aborted.v1` |
//! | LinkUnavailable | 503 | `…cf.oagw.link.unavailable.v1` |
//! | CircuitBreakerOpen | 503 | `…cf.oagw.circuit_breaker.open.v1` |
//! | PluginNotFound | 503 | `…cf.oagw.plugin.not_found.v1` |
//! | ConnectionTimeout | 504 | `…cf.oagw.timeout.connection.v1` |
//! | RequestTimeout | 504 | `…cf.oagw.timeout.request.v1` |
//!
//! (`…` abbreviates the shared `gts.cf.core.errors.err.v1~` prefix.)

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// Shared prefix of every OAGW error GTS type.
pub const ERR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// Wire header distinguishing gateway-produced errors from upstream
/// passthrough. See `docs/ADR/0007-error-source-distinction.md`.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// `docs/ADR/0003-rate-limiting.md`: a rate-limited response carries the
/// limiter's decision as `X-RateLimit-*` headers next to `Retry-After`. The
/// decision is recorded as extension members of the same names, so the two
/// never disagree.
const RATE_LIMIT_HEADERS: [&str; 3] = [
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
];

/// The error kinds in the OAGW catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names)] // names come from the documented table
pub enum ErrorKind {
    ValidationError,
    AliasConflict,
    MatchConflict,
    MissingTargetHost,
    InvalidTargetHost,
    UnknownTargetHost,
    AuthenticationFailed,
    RouteNotFound,
    CorsOriginNotAllowed,
    CorsMethodNotAllowed,
    PluginInUse,
    PayloadTooLarge,
    RateLimitExceeded,
    SecretNotFound,
    ProtocolError,
    DownstreamError,
    StreamAborted,
    LinkUnavailable,
    CircuitBreakerOpen,
    PluginNotFound,
    ConnectionTimeout,
    RequestTimeout,
}

impl ErrorKind {
    /// The documented HTTP status.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
            Self::AliasConflict | Self::MatchConflict | Self::PluginInUse => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => {
                StatusCode::BAD_GATEWAY
            }
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ConnectionTimeout | Self::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// The documented GTS type, in full-identifier form.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::ValidationError => "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Self::AliasConflict => "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1",
            Self::MatchConflict => "gts.cf.core.errors.err.v1~cf.oagw.route.match_conflict.v1",
            Self::MissingTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
            }
            Self::InvalidTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
            }
            Self::UnknownTargetHost => {
                "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
            }
            Self::AuthenticationFailed => "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            Self::RouteNotFound => "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            Self::CorsOriginNotAllowed => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
            }
            Self::CorsMethodNotAllowed => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
            }
            Self::PluginInUse => "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Self::PayloadTooLarge => "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Self::RateLimitExceeded => "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Self::SecretNotFound => "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Self::ProtocolError => "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Self::DownstreamError => "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Self::StreamAborted => "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Self::LinkUnavailable => "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            Self::CircuitBreakerOpen => "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            Self::PluginNotFound => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Self::ConnectionTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Self::RequestTimeout => "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        }
    }

    /// The RFC 9457 `title`.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ValidationError => "Validation Error",
            Self::AliasConflict => "Alias Conflict",
            Self::MatchConflict => "Match Conflict",
            Self::MissingTargetHost => "Missing Target Host",
            Self::InvalidTargetHost => "Invalid Target Host",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
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
        }
    }

    /// `true` for the kinds the PRD marks "Retriable".
    #[must_use]
    pub const fn retriable(self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded
                | Self::LinkUnavailable
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout
                | Self::RequestTimeout
        )
    }
}

/// An OAGW failure carrying its documented status, GTS type and the
/// OAGW-specific RFC 9457 extension fields.
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: ErrorKind,
    detail: String,
    instance: Option<String>,
    extensions: Vec<(String, Value)>,
    retry_after_secs: Option<u64>,
}

impl OagwError {
    /// Build an error of `kind` from a named constructor.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            extensions: Vec::new(),
            retry_after_secs: None,
        }
    }

    /// Set the `instance` URI reference identifying this occurrence.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attach a documented extension field (`upstream_id`, `host`, `path`,
    /// `trace_id`, …).
    #[must_use]
    pub fn with_extension(mut self, name: impl Into<String>, value: Value) -> Self {
        let name = name.into();
        self.extensions.retain(|(k, _)| *k != name);
        self.extensions.push((name, value));
        self
    }

    /// A documented extension field, when this error carries one.
    #[must_use]
    pub fn extension(&self, name: &str) -> Option<&Value> {
        self.extensions
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The `Retry-After` value, when the error carries one.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.retry_after_secs
    }

    /// Replace the `detail` text.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Set the `Retry-After` guidance (seconds).
    #[must_use]
    pub fn with_retry_after_secs(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// The error's kind.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The error's detail string.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The RFC 9457 `type` URI that reaches the wire.
    #[must_use]
    pub fn type_uri(&self) -> String {
        self.kind.gts_type().to_owned()
    }

    /// The HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.status_value()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The documented HTTP status as a bare number.
    #[must_use]
    pub const fn status_value(&self) -> u16 {
        match self.kind {
            ErrorKind::ValidationError
            | ErrorKind::MissingTargetHost
            | ErrorKind::InvalidTargetHost
            | ErrorKind::UnknownTargetHost => 400,
            ErrorKind::AuthenticationFailed => 401,
            ErrorKind::RouteNotFound => 404,
            ErrorKind::CorsOriginNotAllowed | ErrorKind::CorsMethodNotAllowed => 403,
            ErrorKind::AliasConflict | ErrorKind::MatchConflict | ErrorKind::PluginInUse => 409,
            ErrorKind::PayloadTooLarge => 413,
            ErrorKind::RateLimitExceeded => 429,
            ErrorKind::SecretNotFound => 500,
            ErrorKind::ProtocolError | ErrorKind::DownstreamError | ErrorKind::StreamAborted => 502,
            ErrorKind::LinkUnavailable
            | ErrorKind::CircuitBreakerOpen
            | ErrorKind::PluginNotFound => 503,
            ErrorKind::ConnectionTimeout | ErrorKind::RequestTimeout => 504,
        }
    }

    /// The wire headers an error carries: the problem content type and the
    /// `x-oagw-error-source` marker, plus `Retry-After` and the
    /// `X-RateLimit-*` decision when set.
    #[must_use]
    pub fn response_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "content-type".to_owned(),
                "application/problem+json".to_owned(),
            ),
            (
                ERROR_SOURCE_HEADER.to_owned(),
                ERROR_SOURCE_GATEWAY.to_owned(),
            ),
        ];
        if let Some(secs) = self.retry_after_secs {
            headers.push(("retry-after".to_owned(), secs.to_string()));
        }
        for name in RATE_LIMIT_HEADERS {
            if let Some(value) = self.extension(name).and_then(Value::as_u64) {
                headers.push((name.to_owned(), value.to_string()));
            }
        }
        headers
    }

    /// The RFC 9457 body as bytes, ready to be written to a socket.
    #[must_use]
    pub fn to_body_bytes(&self) -> Vec<u8> {
        self.to_body().to_string().into_bytes()
    }

    /// The RFC 9457 body, with the OAGW extension fields merged at the top
    /// level.
    #[must_use]
    pub fn to_body(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), Value::String(self.type_uri()));
        obj.insert("title".into(), Value::String(self.kind.title().into()));
        obj.insert("status".into(), Value::from(self.status_value()));
        obj.insert("detail".into(), Value::String(self.detail.clone()));
        if let Some(instance) = &self.instance {
            obj.insert("instance".into(), Value::String(instance.clone()));
        }
        for (k, v) in &self.extensions {
            obj.insert(k.clone(), v.clone());
        }
        Value::Object(obj)
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind.title(),
            self.status_value(),
            self.detail
        )
    }
}

impl std::error::Error for OagwError {}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = self.to_body();
        let mut builder = axum::response::Response::builder()
            .status(status)
            .header(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/problem+json"),
            )
            .header(
                HeaderName::from_static(ERROR_SOURCE_HEADER),
                HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
            );
        if let Some(secs) = self.retry_after_secs
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            builder = builder.header(axum::http::header::RETRY_AFTER, v);
        }
        for name in RATE_LIMIT_HEADERS {
            if let Some(value) = self.extension(name).and_then(Value::as_u64)
                && let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value.to_string()),
                )
            {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(axum::body::Body::from(body.to_string()))
            .unwrap_or_else(|_| {
                axum::response::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::from("error rendering problem body"))
                    .expect("static body is valid")
            })
    }
}

impl OagwError {
    /// `400 ValidationError`.
    #[must_use]
    pub fn validation_error(detail: impl Into<String>) -> Self {
        validation_error(detail)
    }

    /// `400 ValidationError` naming the offending field.
    #[must_use]
    pub fn validation_field_error(field: &str, detail: impl Into<String>) -> Self {
        validation_field_error(field, detail)
    }

    /// `400 MissingTargetHost`.
    #[must_use]
    pub fn missing_target_host(alias: &str) -> Self {
        missing_target_host(alias)
    }

    /// `400 InvalidTargetHost`.
    #[must_use]
    pub fn invalid_target_host(value: &str) -> Self {
        invalid_target_host(value)
    }

    /// `400 UnknownTargetHost`.
    #[must_use]
    pub fn unknown_target_host(value: &str) -> Self {
        unknown_target_host(value)
    }

    /// `401 AuthenticationFailed`.
    #[must_use]
    pub fn authentication_failed(detail: impl Into<String>) -> Self {
        authentication_failed(detail)
    }

    /// `404 RouteNotFound`.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        route_not_found(detail)
    }

    /// `403 CorsOriginNotAllowed`.
    #[must_use]
    pub fn cors_origin_not_allowed(origin: &str) -> Self {
        cors_origin_not_allowed(origin)
    }

    /// `403 CorsMethodNotAllowed`.
    #[must_use]
    pub fn cors_method_not_allowed(method: &str) -> Self {
        cors_method_not_allowed(method)
    }

    /// `409 PluginInUse`.
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        plugin_in_use(detail)
    }

    /// `409 AliasConflict`.
    #[must_use]
    pub fn alias_conflict(alias: &str) -> Self {
        alias_conflict(alias)
    }

    /// `409 MatchConflict`.
    #[must_use]
    pub fn match_conflict(detail: impl Into<String>) -> Self {
        match_conflict(detail)
    }

    /// `413 PayloadTooLarge`.
    #[must_use]
    pub fn payload_too_large(limit: usize) -> Self {
        payload_too_large(limit)
    }

    /// `429 RateLimitExceeded` with `Retry-After` guidance.
    #[must_use]
    pub fn rate_limit_exceeded(retry_after_secs: u64) -> Self {
        rate_limit_exceeded(retry_after_secs)
    }

    /// `500 SecretNotFound`.
    #[must_use]
    pub fn secret_not_found(reference: &str) -> Self {
        secret_not_found(reference)
    }

    /// `502 ProtocolError`.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        protocol_error(detail)
    }

    /// `502 DownstreamError`.
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        downstream_error(detail)
    }

    /// `502 StreamAborted`.
    #[must_use]
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        stream_aborted(detail)
    }

    /// `503 LinkUnavailable`.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        link_unavailable(detail)
    }

    /// `503 CircuitBreakerOpen` with `Retry-After` guidance.
    #[must_use]
    pub fn circuit_breaker_open(retry_after_secs: u64) -> Self {
        circuit_breaker_open(retry_after_secs)
    }

    /// `503 PluginNotFound`.
    #[must_use]
    pub fn plugin_not_found(reference: &str) -> Self {
        plugin_not_found(reference)
    }

    /// `504 ConnectionTimeout`.
    #[must_use]
    pub fn connection_timeout() -> Self {
        connection_timeout()
    }

    /// `504 RequestTimeout`.
    #[must_use]
    pub fn request_timeout() -> Self {
        request_timeout()
    }
}

// ---------------------------------------------------------------------------
// Constructors — one per documented error
// ---------------------------------------------------------------------------

/// `400 ValidationError`.
#[must_use]
pub fn validation_error(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::ValidationError, detail)
}

/// `400 ValidationError` naming the offending field.
#[must_use]
pub fn validation_field_error(field: &str, detail: impl Into<String>) -> OagwError {
    OagwError::new(
        ErrorKind::ValidationError,
        format!("{field}: {}", detail.into()),
    )
    .with_extension("field", Value::String(field.to_owned()))
}

/// `400 MissingTargetHost`.
#[must_use]
pub fn missing_target_host(alias: &str) -> OagwError {
    OagwError::new(
        ErrorKind::MissingTargetHost,
        format!(
            "alias '{alias}' is derived from a shared endpoint suffix; 'X-OAGW-Target-Host' must name one endpoint"
        ),
    )
}

/// `400 InvalidTargetHost`.
#[must_use]
pub fn invalid_target_host(value: &str) -> OagwError {
    OagwError::new(
        ErrorKind::InvalidTargetHost,
        format!("'{value}' is not a hostname or IP address (no port, path or special characters)"),
    )
}

/// `400 UnknownTargetHost`.
#[must_use]
pub fn unknown_target_host(value: &str) -> OagwError {
    OagwError::new(
        ErrorKind::UnknownTargetHost,
        format!("'{value}' does not match any endpoint configured for the resolved upstream"),
    )
}

/// `401 AuthenticationFailed`.
#[must_use]
pub fn authentication_failed(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::AuthenticationFailed, detail)
}

/// `404 RouteNotFound`.
#[must_use]
pub fn route_not_found(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::RouteNotFound, detail)
}

/// `403 CorsOriginNotAllowed`.
#[must_use]
pub fn cors_origin_not_allowed(origin: &str) -> OagwError {
    OagwError::new(
        ErrorKind::CorsOriginNotAllowed,
        format!("origin '{origin}' not in allowed origins list"),
    )
}

/// `403 CorsMethodNotAllowed`.
#[must_use]
pub fn cors_method_not_allowed(method: &str) -> OagwError {
    OagwError::new(
        ErrorKind::CorsMethodNotAllowed,
        format!("method '{method}' is not in the upstream's allowed methods"),
    )
}

/// `409 PluginInUse`.
#[must_use]
pub fn plugin_in_use(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::PluginInUse, detail)
}

/// `409 AliasConflict` — the tenant already owns an upstream with this alias.
#[must_use]
pub fn alias_conflict(alias: &str) -> OagwError {
    OagwError::new(
        ErrorKind::AliasConflict,
        format!("an upstream with alias '{alias}' already exists in this tenant"),
    )
}

/// `409 MatchConflict` — duplicate path, priority and method set.
#[must_use]
pub fn match_conflict(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::MatchConflict, detail)
}

/// `413 PayloadTooLarge`.
#[must_use]
pub fn payload_too_large(limit: usize) -> OagwError {
    OagwError::new(
        ErrorKind::PayloadTooLarge,
        format!("request body exceeds the {limit} byte limit"),
    )
}

/// `429 RateLimitExceeded` with `Retry-After` guidance.
#[must_use]
pub fn rate_limit_exceeded(retry_after_secs: u64) -> OagwError {
    OagwError::new(
        ErrorKind::RateLimitExceeded,
        format!("rate limit exhausted; retry after {retry_after_secs}s"),
    )
    .with_retry_after_secs(retry_after_secs)
}

/// `500 SecretNotFound`.
#[must_use]
pub fn secret_not_found(reference: &str) -> OagwError {
    OagwError::new(
        ErrorKind::SecretNotFound,
        format!("credential '{reference}' is not resolvable for this tenant"),
    )
}

/// `502 ProtocolError`.
#[must_use]
pub fn protocol_error(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::ProtocolError, detail)
}

/// `502 DownstreamError`.
#[must_use]
pub fn downstream_error(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::DownstreamError, detail)
}

/// `502 StreamAborted`.
#[must_use]
pub fn stream_aborted(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::StreamAborted, detail)
}

/// `503 LinkUnavailable`.
#[must_use]
pub fn link_unavailable(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::LinkUnavailable, detail)
}

/// `503 CircuitBreakerOpen`.
#[must_use]
pub fn circuit_breaker_open(retry_after_secs: u64) -> OagwError {
    OagwError::new(
        ErrorKind::CircuitBreakerOpen,
        "circuit breaker is open for this upstream; too many recent failures",
    )
    .with_retry_after_secs(retry_after_secs)
}

/// `503 PluginNotFound`.
#[must_use]
pub fn plugin_not_found(reference: &str) -> OagwError {
    OagwError::new(
        ErrorKind::PluginNotFound,
        format!("plugin '{reference}' is not registered in the gateway plugin registry"),
    )
}

/// `504 ConnectionTimeout`.
#[must_use]
pub fn connection_timeout() -> OagwError {
    OagwError::new(
        ErrorKind::ConnectionTimeout,
        "no response head was received from the upstream before the timeout elapsed",
    )
}

/// `504 RequestTimeout`.
#[must_use]
pub fn request_timeout() -> OagwError {
    OagwError::new(
        ErrorKind::RequestTimeout,
        "the upstream did not complete the request before the timeout elapsed",
    )
}

/// Shorthand result alias used across the gear.
pub type Result<T, E = OagwError> = std::result::Result<T, E>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn body(err: &OagwError) -> serde_json::Value {
        err.to_body()
    }

    #[test]
    fn each_error_carries_the_documented_status() {
        let cases: Vec<(OagwError, u16)> = vec![
            (validation_error("x"), 400),
            (validation_field_error("alias", "bad"), 400),
            (missing_target_host("a"), 400),
            (invalid_target_host("a"), 400),
            (unknown_target_host("a"), 400),
            (authentication_failed("x"), 401),
            (route_not_found("x"), 404),
            (plugin_in_use("x"), 409),
            (payload_too_large(1024), 413),
            (rate_limit_exceeded(1), 429),
            (secret_not_found("cred://k"), 500),
            (protocol_error("x"), 502),
            (downstream_error("x"), 502),
            (stream_aborted("x"), 502),
            (link_unavailable("x"), 503),
            (circuit_breaker_open(1), 503),
            (plugin_not_found("x"), 503),
            (connection_timeout(), 504),
            (request_timeout(), 504),
        ];
        for (err, want) in cases {
            assert_eq!(
                err.status().as_u16(),
                want,
                "wrong status for {}",
                err.type_uri()
            );
            assert_eq!(body(&err)["status"], want);
        }
    }

    #[test]
    fn gts_types_match_the_design_table() {
        assert_eq!(
            validation_error("x").type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(
            route_not_found("x").type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(
            circuit_breaker_open(1).type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"
        );
        assert_eq!(
            missing_target_host("a").type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[test]
    fn retriable_errors_expose_retry_after() {
        let err = rate_limit_exceeded(7);
        assert_eq!(err.retry_after_secs(), Some(7));
        assert!(err.kind().retriable());
        assert!(!route_not_found("x").kind().retriable());
    }

    #[test]
    fn extensions_reach_the_body() {
        let err = downstream_error("boom")
            .with_extension("upstream_id", Value::from("u1"))
            .with_extension("host", Value::from("api.example.com"))
            .with_extension("upstream_id", Value::from("u2"));
        let b = body(&err);
        assert_eq!(b["upstream_id"], "u2");
        assert_eq!(b["host"], "api.example.com");
        assert_eq!(b["title"], "Downstream Error");
    }

    #[test]
    fn content_type_is_problem_json() {
        let resp = OagwError::new(ErrorKind::RouteNotFound, "nope").into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(ct.starts_with("application/problem+json"), "ct={ct}");
        let source = resp
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(source, ERROR_SOURCE_GATEWAY);
    }

    #[test]
    fn retry_after_header_is_emitted() {
        let resp = rate_limit_exceeded(12).into_response();
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("12")
        );
    }
}
