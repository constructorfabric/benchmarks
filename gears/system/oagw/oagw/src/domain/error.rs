//! Gateway error catalog.
//!
//! Every error OAGW *originates* renders as RFC 9457 problem details with a GTS
//! `type` identifier and the `X-OAGW-Error-Source: gateway` header
//! (`cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`).
//! Errors *returned by an upstream* are never wrapped — they pass through
//! unchanged, marked `X-OAGW-Error-Source: upstream`.

use std::collections::BTreeMap;

use serde::Serialize;
use toolkit_gts::gts_id;

/// Response header naming who produced a response.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value for a response OAGW produced itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value for a response passed through from the upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// RFC 9457 media type.
pub const PROBLEM_JSON: &str = "application/problem+json";

macro_rules! error_kinds {
    ($( $variant:ident => ($status:expr, $gts:expr, $title:expr) ),* $(,)?) => {
        /// The catalogued gateway error kinds (`docs/DESIGN.md` §3.3).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ErrorKind {
            $( $variant, )*
        }

        impl ErrorKind {
            /// HTTP status this kind maps to.
            #[must_use]
            pub fn status(self) -> u16 {
                match self { $( Self::$variant => $status, )* }
            }

            /// GTS identifier carried in the problem `type` member.
            #[must_use]
            pub fn gts_type(self) -> &'static str {
                match self { $( Self::$variant => $gts, )* }
            }

            /// Human-readable summary carried in `title`.
            #[must_use]
            pub fn title(self) -> &'static str {
                match self { $( Self::$variant => $title, )* }
            }
        }
    };
}

error_kinds! {
    ValidationError => (400, gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1"), "Validation Error"),
    MissingTargetHost => (400, gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"), "Missing Target Host Header"),
    InvalidTargetHost => (400, gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"), "Invalid Target Host Format"),
    UnknownTargetHost => (400, gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"), "Unknown Target Host"),
    AuthenticationFailed => (401, gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1"), "Authentication Failed"),
    PermissionDenied => (403, gts_id!("cf.core.errors.err.v1~cf.core.err.permission_denied.v1"), "Permission Denied"),
    CorsOriginNotAllowed => (403, gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"), "CORS Origin Not Allowed"),
    CorsMethodNotAllowed => (403, gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"), "CORS Method Not Allowed"),
    RouteNotFound => (404, gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1"), "Route Not Found"),
    MethodNotAllowed => (405, gts_id!("cf.core.errors.err.v1~cf.oagw.method.not_allowed.v1"), "Method Not Allowed"),
    Conflict => (409, gts_id!("cf.core.errors.err.v1~cf.oagw.resource.conflict.v1"), "Conflict"),
    PluginInUse => (409, gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"), "Plugin In Use"),
    PayloadTooLarge => (413, gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"), "Payload Too Large"),
    RateLimitExceeded => (429, gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"), "Rate Limit Exceeded"),
    SecretNotFound => (500, gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"), "Secret Not Found"),
    Internal => (500, gts_id!("cf.core.errors.err.v1~cf.core.err.internal.v1"), "Internal Error"),
    ProtocolError => (502, gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1"), "Protocol Error"),
    DownstreamError => (502, gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1"), "Downstream Error"),
    StreamAborted => (502, gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"), "Stream Aborted"),
    LinkUnavailable => (503, gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"), "Link Unavailable"),
    CircuitBreakerOpen => (503, gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"), "Circuit Breaker Open"),
    PluginNotFound => (503, gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"), "Plugin Not Found"),
    ConnectionTimeout => (504, gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1"), "Connection Timeout"),
    RequestTimeout => (504, gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1"), "Request Timeout"),
    IdleTimeout => (504, gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"), "Idle Timeout"),
}

impl ErrorKind {
    /// Whether a client may usefully retry (`docs/PRD.md` §5.6).
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

/// A gateway error ready to be rendered as problem details.
#[derive(Debug, Clone)]
pub struct OagwError {
    pub kind: ErrorKind,
    pub detail: String,
    /// Extra members merged into the problem document (`upstream_id`, `host`,
    /// `valid_hosts`, `retry_after_seconds`, …).
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Value for a `Retry-After` response header, in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Extra response headers to emit alongside the problem document.
    pub headers: Vec<(String, String)>,
}

impl OagwError {
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            extensions: BTreeMap::new(),
            retry_after_seconds: None,
            headers: Vec::new(),
        }
    }

    /// Attach a response header to emit with the problem document.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self.extensions.insert(
            "retry_after_seconds".to_owned(),
            serde_json::Value::from(seconds),
        );
        self
    }

    #[must_use]
    pub fn status(&self) -> u16 {
        self.kind.status()
    }

    /// Build the wire document, anchoring `instance` at the request URI.
    #[must_use]
    pub fn to_problem(&self, instance: Option<&str>) -> ProblemJson {
        ProblemJson {
            problem_type: self.kind.gts_type().to_owned(),
            title: self.kind.title().to_owned(),
            status: self.kind.status(),
            detail: self.detail.clone(),
            instance: instance.map(ToOwned::to_owned),
            extensions: self.extensions.clone(),
        }
    }

    // --- Constructors for the shapes used across the gear ------------------

    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::ValidationError, detail)
    }

    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::RouteNotFound, detail)
    }

    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, detail)
    }

    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::PermissionDenied, detail)
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, detail)
    }

    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::LinkUnavailable, detail)
    }
}

impl std::fmt::Display for OagwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.kind.title(), self.status(), self.detail)
    }
}

impl std::error::Error for OagwError {}

/// RFC 9457 problem document with OAGW extension members.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemJson {
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Convenience alias for fallible domain operations.
pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
