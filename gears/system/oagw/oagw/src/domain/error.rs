//! Domain errors and their wire projection.
//!
//! Every gateway-originated failure becomes an RFC 9457 Problem Details
//! document carrying a GTS `type` identifier (`cpt-cf-oagw-principle-rfc9457`)
//! and an `X-OAGW-Error-Source` header (`cpt-cf-oagw-adr-error-source-distinction`).

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

use crate::domain::gts_helpers as gts;

/// Header distinguishing gateway-originated from upstream-originated
/// responses.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] for OAGW-generated responses.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] for passthrough upstream responses.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// A gateway error, ready to be rendered as Problem Details.
#[derive(Debug, Clone, Error)]
#[error("{title}: {detail}")]
pub struct OagwError {
    /// GTS error type identifier (the RFC 9457 `type` member).
    pub error_type: &'static str,
    /// Short, human-readable summary.
    pub title: &'static str,
    /// HTTP status code.
    pub status: u16,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// OAGW-specific extension members, rendered at the top level of the
    /// Problem document.
    pub extensions: BTreeMap<String, Value>,
    /// `Retry-After` guidance, in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Extra response headers to emit alongside the problem document, e.g.
    /// the `X-RateLimit-*` family on a `429`.
    pub headers: Vec<(String, String)>,
}

impl OagwError {
    /// Build an error from its type identifier, title, status and detail.
    #[must_use]
    pub fn new(
        error_type: &'static str,
        title: &'static str,
        status: u16,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            error_type,
            title,
            status,
            detail: detail.into(),
            extensions: BTreeMap::new(),
            retry_after_seconds: None,
            headers: Vec::new(),
        }
    }

    /// Attach an extension member.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// Attach retry guidance (also emitted as the `Retry-After` header).
    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self.extensions
            .insert("retry_after_seconds".to_owned(), Value::from(seconds));
        self
    }

    /// Attach a response header to emit with the problem document.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// Render the Problem Details document.
    ///
    /// Standard members come first, then the OAGW extensions. `context` is
    /// deliberately absent: this is a plain RFC 9457 document, not a canonical
    /// envelope, which is also what keeps the platform's canonical-error
    /// middleware from rewriting (and thereby dropping) the extensions.
    #[must_use]
    pub fn to_problem(&self, instance: Option<&str>) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("type".to_owned(), Value::from(self.error_type));
        map.insert("title".to_owned(), Value::from(self.title));
        map.insert("status".to_owned(), Value::from(self.status));
        map.insert("detail".to_owned(), Value::from(self.detail.clone()));
        if let Some(instance) = instance {
            map.insert("instance".to_owned(), Value::from(instance));
        }
        for (key, value) in &self.extensions {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }

    // -- constructors, one per documented error code -------------------------

    /// 400 — request validation failed.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_VALIDATION, "Validation Error", 400, detail)
    }

    /// 400 — `X-OAGW-Target-Host` is required for this upstream.
    #[must_use]
    pub fn missing_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_MISSING_TARGET_HOST,
            "Missing Target Host Header",
            400,
            detail,
        )
    }

    /// 400 — `X-OAGW-Target-Host` is malformed.
    #[must_use]
    pub fn invalid_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_INVALID_TARGET_HOST,
            "Invalid Target Host Format",
            400,
            detail,
        )
    }

    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    #[must_use]
    pub fn unknown_target_host(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_UNKNOWN_TARGET_HOST,
            "Unknown Target Host",
            400,
            detail,
        )
    }

    /// 401 — authenticating to the upstream failed.
    #[must_use]
    pub fn authentication_failed(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_AUTH_FAILED, "Authentication Failed", 401, detail)
    }

    /// 403 — the request origin is not allowed by the CORS policy.
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            "CORS Origin Not Allowed",
            403,
            detail,
        )
    }

    /// 403 — the request method is not allowed by the CORS policy.
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_CORS_METHOD_NOT_ALLOWED,
            "CORS Method Not Allowed",
            403,
            detail,
        )
    }

    /// 403 — the caller is not permitted to perform this operation.
    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_FORBIDDEN, "Forbidden", 403, detail)
    }

    /// 404 — no route matched the proxy request.
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_ROUTE_NOT_FOUND, "Route Not Found", 404, detail)
    }

    /// 404 — the addressed resource does not exist for this tenant.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_NOT_FOUND, "Not Found", 404, detail)
    }

    /// 409 — the write conflicts with existing state.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_CONFLICT, "Conflict", 409, detail)
    }

    /// 409 — the plugin is still referenced and cannot be deleted.
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_PLUGIN_IN_USE, "Plugin In Use", 409, detail)
    }

    /// 413 — the request payload exceeds the configured ceiling.
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_PAYLOAD_TOO_LARGE, "Payload Too Large", 413, detail)
    }

    /// 429 — a rate limit was exceeded.
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_RATE_LIMIT_EXCEEDED,
            "Rate Limit Exceeded",
            429,
            detail,
        )
    }

    /// 500 — a referenced secret could not be resolved.
    #[must_use]
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_SECRET_NOT_FOUND, "Secret Not Found", 500, detail)
    }

    /// 500 — unexpected gateway failure.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_INTERNAL, "Internal Error", 500, detail)
    }

    /// 502 — protocol-level failure talking to the upstream.
    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_PROTOCOL, "Protocol Error", 502, detail)
    }

    /// 502 — the upstream call failed.
    #[must_use]
    pub fn downstream(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_DOWNSTREAM, "Downstream Error", 502, detail)
    }

    /// 502 — an established stream was aborted.
    #[must_use]
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_STREAM_ABORTED, "Stream Aborted", 502, detail)
    }

    /// 503 — the upstream link is unavailable.
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_LINK_UNAVAILABLE, "Link Unavailable", 503, detail)
    }

    /// 503 — the upstream (or an ancestor of it) is disabled.
    #[must_use]
    pub fn upstream_disabled(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_LINK_UNAVAILABLE, "Upstream Disabled", 503, detail)
    }

    /// 503 — the circuit breaker for this upstream is open.
    #[must_use]
    pub fn circuit_breaker_open(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_CIRCUIT_BREAKER_OPEN,
            "Circuit Breaker Open",
            503,
            detail,
        )
    }

    /// 503 — a bound plugin could not be resolved.
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_PLUGIN_NOT_FOUND, "Plugin Not Found", 503, detail)
    }

    /// 504 — establishing the upstream connection timed out.
    #[must_use]
    pub fn connection_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            gts::ERR_CONNECTION_TIMEOUT,
            "Connection Timeout",
            504,
            detail,
        )
    }

    /// 504 — the upstream did not answer within the request budget.
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_REQUEST_TIMEOUT, "Request Timeout", 504, detail)
    }

    /// 504 — an established connection went idle past its budget.
    #[must_use]
    pub fn idle_timeout(detail: impl Into<String>) -> Self {
        Self::new(gts::ERR_IDLE_TIMEOUT, "Idle Timeout", 504, detail)
    }
}

/// Error raised by a plugin implementation.
#[derive(Debug, Clone, Error)]
pub enum PluginError {
    /// The plugin's configuration is unusable.
    #[error("plugin configuration invalid: {0}")]
    Config(String),
    /// A credential reference could not be resolved.
    #[error("secret not found: {0}")]
    SecretNotFound(String),
    /// The credential store refused access to the reference.
    #[error("credential access denied: {0}")]
    AccessDenied(String),
    /// Anything else, including transport failures to an IdP.
    #[error("plugin failure: {0}")]
    Internal(String),
}

impl From<PluginError> for OagwError {
    fn from(value: PluginError) -> Self {
        match value {
            PluginError::Config(detail) => OagwError::validation(detail),
            PluginError::SecretNotFound(detail) => OagwError::secret_not_found(detail),
            PluginError::AccessDenied(detail) => OagwError::authentication_failed(detail),
            PluginError::Internal(detail) => OagwError::authentication_failed(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OagwError;
    use crate::domain::gts_helpers as gts;

    #[test]
    fn problem_document_shape() {
        let err = OagwError::rate_limit_exceeded("too fast")
            .with("host", "api.openai.com")
            .with_retry_after(15);
        let problem = err.to_problem(Some("/oagw/v1/proxy/api.openai.com/v1/chat"));
        assert_eq!(problem["type"], gts::ERR_RATE_LIMIT_EXCEEDED);
        assert_eq!(problem["status"], 429);
        assert_eq!(problem["title"], "Rate Limit Exceeded");
        assert_eq!(problem["detail"], "too fast");
        assert_eq!(problem["host"], "api.openai.com");
        assert_eq!(problem["retry_after_seconds"], 15);
        assert_eq!(problem["instance"], "/oagw/v1/proxy/api.openai.com/v1/chat");
        assert!(
            problem.get("context").is_none(),
            "plain RFC 9457, not a canonical envelope"
        );
    }

    #[test]
    fn documented_status_codes() {
        assert_eq!(OagwError::validation("x").status, 400);
        assert_eq!(OagwError::authentication_failed("x").status, 401);
        assert_eq!(OagwError::route_not_found("x").status, 404);
        assert_eq!(OagwError::payload_too_large("x").status, 413);
        assert_eq!(OagwError::rate_limit_exceeded("x").status, 429);
        assert_eq!(OagwError::secret_not_found("x").status, 500);
        assert_eq!(OagwError::downstream("x").status, 502);
        assert_eq!(OagwError::circuit_breaker_open("x").status, 503);
        assert_eq!(OagwError::request_timeout("x").status, 504);
    }
}
