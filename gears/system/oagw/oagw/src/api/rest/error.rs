//! RFC 9457 problem-detail responses for OAGW gateway errors (DESIGN §"Error
//! Response Format", ADR-0007).
//!
//! OAGW deliberately does **not** reuse the toolkit's canonical problem type:
//! this gear must own its GTS error instance identifiers and the
//! `X-OAGW-Error-Source: gateway` header, so handlers return
//! [`OagwProblem`] directly instead of `toolkit`'s `CanonicalError`.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::error::{DomainError, ProxyContext, ProxyError};
use crate::gts;

/// A gateway error in RFC 9457 `application/problem+json` form.
///
/// `type` is always one of the `gts::ERR_*` instance identifiers; `instance`
/// is an opaque `urn:uuid:` for this occurrence. Extension fields are omitted
/// when unset so the wire payload stays compact.
#[derive(Debug, Clone, Serialize)]
pub struct OagwProblem {
    /// GTS error instance identifier.
    pub r#type: &'static str,
    /// Human-readable summary.
    pub title: &'static str,
    /// HTTP status code (RFC 9457 `status` member).
    #[serde(serialize_with = "serialize_status")]
    pub status: StatusCode,
    /// Human-readable explanation for this occurrence.
    pub detail: String,
    /// URI reference identifying this specific occurrence.
    pub instance: String,
    /// GTS instance id of the resolved upstream, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Target host, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The proxy path suffix, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Retry guidance in seconds (rate limit, upstream unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Effective rate-limit bucket capacity (429 only, `X-RateLimit-Limit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_limit: Option<u64>,
    /// Tokens remaining in the bucket (429 only, `X-RateLimit-Remaining`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_remaining: Option<u64>,
    /// Unix epoch seconds when the bucket resets (429 only,
    /// `X-RateLimit-Reset`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_reset: Option<u64>,
    /// Distributed-tracing correlation id (reserved; not emitted by this build).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl OagwProblem {
    /// Build a problem for an error with the given GTS id, status, title and
    /// detail, and no request-context extensions.
    #[must_use]
    pub fn new(
        r#type: &'static str,
        status: StatusCode,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            r#type,
            title,
            status,
            detail: detail.into(),
            instance: format!("urn:uuid:{}", Uuid::new_v4()),
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            rate_limit_limit: None,
            rate_limit_remaining: None,
            rate_limit_reset: None,
            trace_id: None,
        }
    }

    /// Attach the OAGW request-context extension fields.
    #[must_use]
    pub fn with_context(mut self, context: &ProxyContext) -> Self {
        self.upstream_id.clone_from(&context.upstream_id);
        self.host.clone_from(&context.host);
        self.path.clone_from(&context.path);
        self
    }
}

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        headers.insert("x-oagw-error-source", HeaderValue::from_static("gateway"));
        if let Some(retry_after) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
        {
            headers.insert(axum::http::header::RETRY_AFTER, value);
        }
        if let Some(limit) = self.rate_limit_limit
            && let Ok(value) = HeaderValue::from_str(&limit.to_string())
        {
            headers.insert("x-ratelimit-limit", value);
        }
        if let Some(remaining) = self.rate_limit_remaining
            && let Ok(value) = HeaderValue::from_str(&remaining.to_string())
        {
            headers.insert("x-ratelimit-remaining", value);
        }
        if let Some(reset) = self.rate_limit_reset
            && let Ok(value) = HeaderValue::from_str(&reset.to_string())
        {
            headers.insert("x-ratelimit-reset", value);
        }
        // Serialization of an owned problem never fails for this struct, but
        // keep a deterministic static fallback rather than silently emitting
        // an empty body.
        let body = if let Ok(bytes) = serde_json::to_vec(&self) {
            Body::from(bytes)
        } else {
            tracing::error!("failed to serialize OAGW problem envelope; emitting fallback");
            Body::from(format!(
                "{{\"type\":\"{}\",\"title\":\"Internal Server Error\",\"status\":500, \
                 \"detail\":\"error envelope serialization failed\"}}",
                gts::ERR_INTERNAL
            ))
        };
        let status = self.status;
        (status, headers, body).into_response()
    }
}

/// Serialize the RFC 9457 `status` member as its numeric HTTP code.
///
/// serde `serialize_with` functions receive `&T` regardless of `T: Copy`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_status<S: serde::Serializer>(
    status: &StatusCode,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u16(u16::from(*status))
}

/// Map a control-plane error onto a gateway problem.
#[must_use]
pub fn domain_error_to_problem(e: DomainError) -> OagwProblem {
    match e {
        DomainError::NotFound => OagwProblem::new(
            gts::ERR_ROUTE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "Not Found",
            "the resource does not exist in the calling tenant's scope",
        ),
        DomainError::Conflict { detail } => {
            OagwProblem::new(gts::ERR_CONFLICT, StatusCode::CONFLICT, "Conflict", detail)
        }
        DomainError::Validation { detail } => OagwProblem::new(
            gts::ERR_VALIDATION,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            detail,
        ),
        DomainError::AccessDenied { detail } => OagwProblem::new(
            gts::ERR_FORBIDDEN,
            StatusCode::FORBIDDEN,
            "Forbidden",
            detail,
        ),
        DomainError::PluginInUse => OagwProblem::new(
            gts::ERR_PLUGIN_IN_USE,
            StatusCode::CONFLICT,
            "Conflict",
            "the plugin is still referenced by an upstream or route",
        ),
        DomainError::Internal { .. } => OagwProblem::new(
            gts::ERR_INTERNAL,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "internal gateway error",
        ),
    }
}

/// Map a data-plane error onto a gateway problem (DESIGN error table).
///
/// The diagnostic payload of an internal error is never leaked: only a static
/// detail string reaches the wire.
#[must_use]
pub fn proxy_error_to_problem(e: ProxyError) -> OagwProblem {
    let context = proxy_context(&e);
    let problem = match e {
        ProxyError::RouteNotFound { .. } => OagwProblem::new(
            gts::ERR_ROUTE_NOT_FOUND,
            StatusCode::NOT_FOUND,
            "Not Found",
            "no matching route found for the requested alias and path",
        ),
        ProxyError::Validation { detail, .. } => OagwProblem::new(
            gts::ERR_VALIDATION,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            detail,
        ),
        ProxyError::MissingTargetHost { valid_hosts, .. } => OagwProblem::new(
            gts::ERR_MISSING_TARGET_HOST,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            format!(
                "X-OAGW-Target-Host is required for this multi-endpoint pool (valid: {valid_hosts:?})"
            ),
        ),
        ProxyError::InvalidTargetHost { detail, .. } => OagwProblem::new(
            gts::ERR_INVALID_TARGET_HOST,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            detail,
        ),
        ProxyError::UnknownTargetHost { valid_hosts, .. } => OagwProblem::new(
            gts::ERR_UNKNOWN_TARGET_HOST,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            format!("X-OAGW-Target-Host matches no configured endpoint (valid: {valid_hosts:?})"),
        ),
        ProxyError::AuthenticationFailed { detail, .. } => OagwProblem::new(
            gts::ERR_AUTH_FAILED,
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            detail,
        ),
        ProxyError::CorsOriginNotAllowed { origin, .. } => OagwProblem::new(
            gts::ERR_CORS_ORIGIN,
            StatusCode::FORBIDDEN,
            "Forbidden",
            format!("origin {origin:?} is not allowed by the CORS policy"),
        ),
        ProxyError::CorsMethodNotAllowed { method, .. } => OagwProblem::new(
            gts::ERR_CORS_METHOD,
            StatusCode::FORBIDDEN,
            "Forbidden",
            format!("method {method:?} is not allowed by the CORS policy"),
        ),
        ProxyError::PayloadTooLarge { .. } => OagwProblem::new(
            gts::ERR_PAYLOAD_TOO_LARGE,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Payload Too Large",
            "request payload exceeds the hard body limit",
        ),
        ProxyError::RateLimitExceeded {
            retry_after,
            limit,
            remaining,
            reset,
            ..
        } => OagwProblem {
            r#type: gts::ERR_RATE_LIMIT,
            title: "Too Many Requests",
            status: StatusCode::TOO_MANY_REQUESTS,
            detail: "rate limit exceeded".to_owned(),
            instance: format!("urn:uuid:{}", Uuid::new_v4()),
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: Some(retry_after.as_secs()),
            rate_limit_limit: Some(limit),
            rate_limit_remaining: Some(remaining),
            rate_limit_reset: Some(reset),
            trace_id: None,
        },
        ProxyError::SecretNotFound { reference, .. } => OagwProblem::new(
            gts::ERR_SECRET_NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            format!("referenced secret not found: {reference}"),
        ),
        ProxyError::ProtocolError { detail, .. } => OagwProblem::new(
            gts::ERR_PROTOCOL,
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            detail,
        ),
        ProxyError::DownstreamError { detail, .. } => OagwProblem::new(
            gts::ERR_DOWNSTREAM,
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            detail,
        ),
        ProxyError::StreamAborted { detail, .. } => OagwProblem::new(
            gts::ERR_STREAM_ABORTED,
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            detail,
        ),
        ProxyError::LinkUnavailable { .. } => OagwProblem::new(
            gts::ERR_LINK_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            "the upstream link is unavailable",
        ),
        ProxyError::PluginNotFound { plugin_ref, .. } => OagwProblem::new(
            gts::ERR_PLUGIN_NOT_FOUND,
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            format!("a bound plugin could not be resolved: {plugin_ref}"),
        ),
        ProxyError::ConnectionTimeout { .. } => OagwProblem::new(
            gts::ERR_CONNECTION_TIMEOUT,
            StatusCode::GATEWAY_TIMEOUT,
            "Gateway Timeout",
            "connection to the upstream timed out",
        ),
        ProxyError::RequestTimeout { timeout_secs, .. } => OagwProblem::new(
            gts::ERR_REQUEST_TIMEOUT,
            StatusCode::GATEWAY_TIMEOUT,
            "Gateway Timeout",
            format!("the proxied request exceeded the {timeout_secs}s budget"),
        ),
        ProxyError::IdleTimeout { .. } => OagwProblem::new(
            gts::ERR_IDLE_TIMEOUT,
            StatusCode::GATEWAY_TIMEOUT,
            "Gateway Timeout",
            "the upstream stream was idle for too long",
        ),
        ProxyError::GrpcNotImplemented { .. } => OagwProblem::new(
            gts::ERR_VALIDATION,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            "gRPC proxying is not implemented (Phase 3)",
        ),
        ProxyError::Internal { .. } => OagwProblem::new(
            gts::ERR_INTERNAL,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "internal data-plane error",
        ),
    };
    problem.with_context(&context)
}

/// Extract the request-context fields from an owned proxy error before it is
/// consumed (the mapping above owns `e`).
fn proxy_context(e: &ProxyError) -> ProxyContext {
    match e {
        ProxyError::RouteNotFound { context }
        | ProxyError::Validation { context, .. }
        | ProxyError::MissingTargetHost { context, .. }
        | ProxyError::InvalidTargetHost { context, .. }
        | ProxyError::UnknownTargetHost { context, .. }
        | ProxyError::AuthenticationFailed { context, .. }
        | ProxyError::CorsOriginNotAllowed { context, .. }
        | ProxyError::CorsMethodNotAllowed { context, .. }
        | ProxyError::PayloadTooLarge { context }
        | ProxyError::RateLimitExceeded { context, .. }
        | ProxyError::SecretNotFound { context, .. }
        | ProxyError::ProtocolError { context, .. }
        | ProxyError::DownstreamError { context, .. }
        | ProxyError::StreamAborted { context, .. }
        | ProxyError::LinkUnavailable { context }
        | ProxyError::PluginNotFound { context, .. }
        | ProxyError::ConnectionTimeout { context }
        | ProxyError::RequestTimeout { context, .. }
        | ProxyError::IdleTimeout { context }
        | ProxyError::GrpcNotImplemented { context }
        | ProxyError::Internal { context, .. } => context.clone(),
    }
}
