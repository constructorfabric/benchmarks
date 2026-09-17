//! A data-plane failure and its RFC 9457 rendering.
//!
//! Two failure families exist, and `docs/ADR/0007-error-source-distinction.md`
//! separates them on the wire:
//!
//! * **gateway** failures — the gateway decided something (no route, rate
//!   limit, disallowed origin, unusable configuration). They are rendered as
//!   `application/problem+json` with a catalogue GTS `type`.
//! * **upstream** failures — the upstream answered, badly. Its own response is
//!   passed through unchanged; only the error-source header is added.
//!
//! Failure *responses from a healthy upstream exchange* are not failures at
//! all in this model: a 404 from the upstream is forwarded as-is.
use http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::domain::plugin::{INTERNAL, VALIDATION, problem_type};

/// Which plane produced a failure (`docs/ADR/0007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway decided.
    Gateway,
    /// The upstream answered badly and its answer is being passed through.
    Upstream,
}

impl ErrorSource {
    /// Wire value of the `X-OAGW-Error-Source` header.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// A failure the gateway renders itself.
#[derive(Debug, Clone)]
pub struct ProxyFailure {
    /// HTTP status the gateway returns.
    pub status: u16,
    /// Full GTS `type` identifier of the problem document.
    pub type_uri: String,
    /// Short, human-readable `title`.
    pub title: String,
    /// Opaque `detail`.
    pub detail: String,
    /// Extension members travelling inside the platform's `context` object.
    pub context: Value,
    /// Extra response headers (`Retry-After`, `X-RateLimit-*`, `Vary`, CORS).
    pub headers: HeaderMap,
    /// Which plane produced the failure.
    pub source: ErrorSource,
}

impl ProxyFailure {
    /// A gateway failure from a bare catalogue id.
    #[must_use]
    pub fn new(
        status: u16,
        bare_type: &str,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            type_uri: problem_type(bare_type),
            title: title.into(),
            detail: detail.into(),
            context: Value::Object(Map::new()),
            headers: HeaderMap::new(),
            source: ErrorSource::Gateway,
        }
    }

    /// A 400 for a malformed proxy request.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(400, VALIDATION, "Validation Error", detail)
    }

    /// An opaque 500. The diagnostic stays server-side; the wire detail does
    /// not.
    #[must_use]
    pub fn internal(diagnostic: impl std::fmt::Display) -> Self {
        tracing::error!(diagnostic = %diagnostic, "oagw proxy internal error");
        Self::new(500, INTERNAL, "Internal Error", "internal error")
    }

    /// A 503 for an upstream the gateway could not reach.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            503,
            crate::domain::plugin::LINK_UNAVAILABLE,
            "Link Unavailable",
            detail,
        )
    }

    /// A 504 for a dial that did not complete.
    #[must_use]
    pub fn connection_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            504,
            crate::domain::plugin::CONNECTION_TIMEOUT,
            "Connection Timeout",
            detail,
        )
    }

    /// A 504 for an exchange that exceeded the configured timeout.
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            504,
            crate::domain::plugin::REQUEST_TIMEOUT,
            "Request Timeout",
            detail,
        )
    }

    /// A 502 for a malformed upstream exchange.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(
            502,
            crate::domain::plugin::PROTOCOL_ERROR,
            "Protocol Error",
            detail,
        )
    }

    /// Attach one extension member to the `context` object.
    #[must_use]
    pub fn with_context(mut self, key: &str, value: Value) -> Self {
        if let Value::Object(map) = &mut self.context {
            map.insert(key.to_owned(), value);
        }
        self
    }

    /// A gateway failure whose `type` is already the full GTS identifier.
    ///
    /// Plugin failures arrive carrying a complete `gts.cf.core.errors.err.v1~…`
    /// id, because they are raised away from the catalogue constants; re-running
    /// them through [`ProxyFailure::new`] would prefix the prefix.
    #[must_use]
    pub fn with_type_uri(
        status: u16,
        type_uri: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            type_uri: type_uri.into(),
            title: title.into(),
            detail: detail.into(),
            context: Value::Object(Map::new()),
            headers: HeaderMap::new(),
            source: ErrorSource::Gateway,
        }
    }

    /// Attach one response header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name),
            http::HeaderValue::try_from(value),
        ) {
            self.headers.insert(name, value);
        }
        self
    }

    /// Change which plane owns the failure.
    #[must_use]
    pub const fn with_source(mut self, source: ErrorSource) -> Self {
        self.source = source;
        self
    }
}

/// The platform-standard extension members every problem carries.
#[must_use]
pub fn resource_context(resource_type: &str, resource_name: &str) -> Value {
    json!({
        "resource_type": resource_type,
        "resource_name": resource_name,
    })
}
