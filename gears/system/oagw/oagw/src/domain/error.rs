//! Domain error model and its RFC 9457 `application/problem+json` rendering.
//!
//! The toolkit's canonical error type renders `type` as a `gts://…` URI, which
//! does not match the `gts.cf.core.errors.err.v1~cf.oagw.*.v1` identifiers the
//! OAGW contract tabulates. OAGW therefore owns its problem type and renders it
//! directly, always stamping `X-OAGW-Error-Source: gateway`.
//!
//! No credential material is ever copied into a [`DomainError`]: plugins hand
//! back `PluginFailure` values that carry only a static reason code, and the
//! extensions map is populated by this module from routing/audit metadata.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

use super::gts_helpers::{self, ERROR_SOURCE_HEADER};

/// RFC 9457 content type.
pub const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";

/// Closed set of gateway-originated failures (DESIGN §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Generic request validation failure (400).
    Validation,
    /// `X-OAGW-Target-Host` absent on a multi-endpoint common-suffix alias (400).
    MissingTargetHost,
    /// `X-OAGW-Target-Host` malformed (400).
    InvalidTargetHost,
    /// `X-OAGW-Target-Host` not in the endpoint pool (400).
    UnknownTargetHost,
    /// Outbound authentication failed (401).
    AuthenticationFailed,
    /// No upstream/route matched (404).
    RouteNotFound,
    /// Plugin still referenced (409).
    PluginInUse,
    /// Another 409-class conflict (alias collision, duplicate match rule).
    ///
    /// Extension beyond the DESIGN §3.3 table: the table enumerates only
    /// `PluginInUse` for the 409 class, but alias collisions and duplicate
    /// route match rules also surface as 409 and deserve their own `type`.
    Conflict,
    /// Cross-origin request from an origin outside `allowed_origins` (403).
    ///
    /// Tabulated by ADR 0004, which post-dates the DESIGN §3.3 table and
    /// specifies these two identifiers verbatim.
    CorsOriginNotAllowed,
    /// Cross-origin request using a method outside `allowed_methods` (403).
    ///
    /// Tabulated by ADR 0004, which post-dates the DESIGN §3.3 table and
    /// specifies these two identifiers verbatim.
    CorsMethodNotAllowed,
    /// Body over the configured ceiling (413).
    PayloadTooLarge,
    /// Token bucket exhausted (429).
    RateLimitExceeded,
    /// CredStore reference unresolvable (500).
    SecretNotFound,
    /// Protocol framing failure (502).
    ProtocolError,
    /// Upstream connection failed (502).
    DownstreamError,
    /// Streaming response aborted mid-flight (502).
    StreamAborted,
    /// Upstream disabled or unreachable (503).
    LinkUnavailable,
    /// Circuit breaker is open (503).
    CircuitBreakerOpen,
    /// Referenced plugin is not registered (503).
    PluginNotFound,
    /// TCP/TLS connect timed out (504).
    ConnectionTimeout,
    /// Upstream round trip exceeded the configured timeout (504).
    RequestTimeout,
    /// Idle timeout on an upgraded stream (504).
    IdleTimeout,
}

impl ErrorKind {
    /// HTTP status for this kind.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound => StatusCode::NOT_FOUND,
            Self::PluginInUse | Self::Conflict => StatusCode::CONFLICT,
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
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

    /// GTS `type` identifier, matching DESIGN §3.3 verbatim.
    #[must_use]
    pub fn gts_type(self) -> String {
        let suffix = match self {
            Self::Validation => "validation.error",
            Self::MissingTargetHost => "routing.missing_target_host",
            Self::InvalidTargetHost => "routing.invalid_target_host",
            Self::UnknownTargetHost => "routing.unknown_target_host",
            Self::AuthenticationFailed => "auth.failed",
            Self::RouteNotFound => "route.not_found",
            Self::PluginInUse => "plugin.in_use",
            Self::Conflict => "conflict",
            Self::CorsOriginNotAllowed => "cors.origin_not_allowed",
            Self::CorsMethodNotAllowed => "cors.method_not_allowed",
            Self::PayloadTooLarge => "payload.too_large",
            Self::RateLimitExceeded => "rate_limit.exceeded",
            Self::SecretNotFound => "secret.not_found",
            Self::ProtocolError => "protocol.error",
            Self::DownstreamError => "downstream.error",
            Self::StreamAborted => "stream.aborted",
            Self::LinkUnavailable => "link.unavailable",
            Self::CircuitBreakerOpen => "circuit_breaker.open",
            Self::PluginNotFound => "plugin.not_found",
            Self::ConnectionTimeout => "timeout.connection",
            Self::RequestTimeout => "timeout.request",
            Self::IdleTimeout => "timeout.idle",
        };
        gts_helpers::error_type(suffix)
    }

    /// Human readable `title`.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::PluginInUse => "Plugin In Use",
            Self::Conflict => "Conflict",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
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

    /// `true` when the design marks the kind retriable.
    #[must_use]
    pub const fn retriable(self) -> bool {
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

    /// Short machine-readable code (also used as a plugin failure reason).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Validation => "VALIDATION_ERROR",
            Self::MissingTargetHost => "MISSING_TARGET_HOST",
            Self::InvalidTargetHost => "INVALID_TARGET_HOST",
            Self::UnknownTargetHost => "UNKNOWN_TARGET_HOST",
            Self::AuthenticationFailed => "AUTHENTICATION_FAILED",
            Self::RouteNotFound => "ROUTE_NOT_FOUND",
            Self::PluginInUse => "PLUGIN_IN_USE",
            Self::Conflict => "CONFLICT",
            Self::CorsOriginNotAllowed => "CORS_ORIGIN_NOT_ALLOWED",
            Self::CorsMethodNotAllowed => "CORS_METHOD_NOT_ALLOWED",
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
}

/// Gateway-originated error, rendered as RFC 9457 problem+json.
#[derive(Debug, Clone)]
pub struct DomainError {
    /// Which failure occurred.
    pub kind: ErrorKind,
    /// Human readable explanation.
    pub detail: String,
    /// Request-scoped `instance` (the request path, when known).
    pub instance: Option<String>,
    /// `Retry-After` hint in seconds (429/503/504 only).
    pub retry_after_secs: Option<u64>,
    /// Additional problem members. Never populated with credentials.
    pub extensions: Vec<(String, Value)>,
}

impl DomainError {
    /// Builds an error with a detail message.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            retry_after_secs: None,
            extensions: Vec::new(),
        }
    }

    /// Attaches the request path as the problem `instance`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Sets the `Retry-After` hint.
    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// Appends an extension member, replacing a previous entry of the same name.
    #[must_use]
    pub fn with_extension(mut self, name: impl Into<String>, value: Value) -> Self {
        let name = name.into();
        if let Some(slot) = self.extensions.iter_mut().find(|(k, _)| *k == name) {
            slot.1 = value;
        } else {
            self.extensions.push((name, value));
        }
        self
    }

    /// Renders the problem body without HTTP framing (used by tests).
    #[must_use]
    pub fn to_problem_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            serde_json::json!({
                "type": self.kind.gts_type(),
                "title": self.kind.title(),
                "status": self.kind.status().as_u16(),
                "detail": self.detail,
            })
        })
    }
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.detail)
    }
}

impl std::error::Error for DomainError {}

impl From<ErrorKind> for DomainError {
    fn from(kind: ErrorKind) -> Self {
        Self::new(kind, kind.title())
    }
}

/// Serialized problem body. `type`/`title`/`status`/`detail` are always
/// present, `instance` and `retry_after_seconds` only when known.
impl Serialize for DomainError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), Value::String(self.kind.gts_type()));
        map.insert("title".into(), Value::String(self.kind.title().to_owned()));
        map.insert(
            "status".into(),
            Value::Number(serde_json::Number::from(self.kind.status().as_u16())),
        );
        map.insert("detail".into(), Value::String(self.detail.clone()));
        if let Some(instance) = &self.instance {
            map.insert("instance".into(), Value::String(instance.clone()));
        }
        if let Some(secs) = self.retry_after_secs {
            map.insert(
                "retry_after_seconds".into(),
                Value::Number(serde_json::Number::from(secs)),
            );
        }
        map.insert("retriable".into(), Value::Bool(self.kind.retriable()));
        map.insert(
            "error_code".into(),
            Value::String(self.kind.code().to_owned()),
        );
        for (key, value) in &self.extensions {
            map.insert(key.clone(), value.clone());
        }
        let mut ser = serializer.serialize_map(Some(map.len()))?;
        for (key, value) in &map {
            ser.serialize_entry(key, value)?;
        }
        ser.end()
    }
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        gateway_problem_response(&self)
    }
}

/// Builds a problem+json response carrying `X-OAGW-Error-Source: gateway`.
///
/// # Panics
///
/// Never: the static header value and the serialized body are both valid.
#[must_use]
pub fn gateway_problem_response(err: &DomainError) -> Response {
    let body = err.to_problem_json().to_string();
    let status = err.kind.status();
    let mut builder = Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)
        .header(ERROR_SOURCE_HEADER, "gateway");

    if let Some(secs) = err.retry_after_secs
        && let Ok(value) = http::HeaderValue::from_str(&secs.to_string())
    {
        builder = builder.header(http::header::RETRY_AFTER, value);
    }
    match builder.body(axum::body::Body::from(body)) {
        Ok(resp) => resp,
        Err(_) => axum::http::Response::builder()
            .status(status)
            .header(ERROR_SOURCE_HEADER, "gateway")
            .body(axum::body::Body::from("{\"title\":\"Internal Error\"}"))
            .unwrap_or_default(),
    }
}

/// Convenience alias used by handlers.
pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn problem_body_has_required_members() {
        let err = DomainError::new(ErrorKind::RouteNotFound, "no route")
            .with_instance("/oagw/v1/proxy/a.example/b");
        let body = err.to_problem_json();
        assert_eq!(body["type"], ErrorKind::RouteNotFound.gts_type());
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(body["title"], "Route Not Found");
        assert_eq!(body["status"], 404);
        assert_eq!(body["detail"], "no route");
        assert_eq!(body["instance"], "/oagw/v1/proxy/a.example/b");
        assert_eq!(body["error_code"], "ROUTE_NOT_FOUND");
    }

    #[test]
    fn rate_limit_error_carries_retry_hint() {
        let err = DomainError::new(ErrorKind::RateLimitExceeded, "slow down")
            .with_extension("host", serde_json::json!("api.example"))
            .with_extension("retry_after_seconds", serde_json::json!(15))
            .with_retry_after(15);
        assert_eq!(err.retry_after_secs, Some(15));
        assert_eq!(err.to_problem_json()["host"], "api.example");
        assert!(err.kind.retriable());
        assert_eq!(err.kind.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn status_map_is_complete() {
        let all = [
            ErrorKind::Validation,
            ErrorKind::MissingTargetHost,
            ErrorKind::InvalidTargetHost,
            ErrorKind::UnknownTargetHost,
            ErrorKind::AuthenticationFailed,
            ErrorKind::RouteNotFound,
            ErrorKind::PluginInUse,
            ErrorKind::Conflict,
            ErrorKind::CorsOriginNotAllowed,
            ErrorKind::CorsMethodNotAllowed,
            ErrorKind::PayloadTooLarge,
            ErrorKind::RateLimitExceeded,
            ErrorKind::SecretNotFound,
            ErrorKind::ProtocolError,
            ErrorKind::DownstreamError,
            ErrorKind::StreamAborted,
            ErrorKind::LinkUnavailable,
            ErrorKind::CircuitBreakerOpen,
            ErrorKind::PluginNotFound,
            ErrorKind::ConnectionTimeout,
            ErrorKind::RequestTimeout,
            ErrorKind::IdleTimeout,
        ];
        for kind in all {
            let rendered = kind.gts_type();
            assert!(rendered.starts_with("gts.cf.core.errors.err.v1~cf.oagw."));
            assert!(rendered.ends_with(".v1"));
        }
        assert_eq!(ErrorKind::PluginInUse.status(), StatusCode::CONFLICT);
        assert_eq!(
            ErrorKind::CorsOriginNotAllowed.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            ErrorKind::CorsMethodNotAllowed.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ErrorKind::SecretNotFound.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ErrorKind::StreamAborted.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(ErrorKind::IdleTimeout.status(), StatusCode::GATEWAY_TIMEOUT);
    }
}
