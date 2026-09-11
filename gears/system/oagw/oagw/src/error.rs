//! RFC 9457 gateway-error rendering and the oagw GTS error-type catalog.
//!
//! Every gateway-originated error is rendered as `application/problem+json`
//! carrying the documented `(HTTP status, GTS type identifier, title)`
//! triple from `DESIGN.md` §3.3's Error Response Format table, plus the
//! `X-OAGW-Error-Source: gateway` response header. Rendering the
//! complementary passthrough half (`X-OAGW-Error-Source: upstream` with the
//! upstream's response body forwarded unchanged) is
//! `cpt-cf-oagw-feature-proxy-core`'s (2.5) concern -- only it receives
//! upstream responses to pass through.
//!
//! See `docs/features/gear-foundation.md` §3 "RFC 9457 Gateway-Error
//! Rendering" (`cpt-cf-oagw-algo-error-render`) and §5 "RFC 9457 Error
//! Envelope and GTS Error-Type Catalog" (`cpt-cf-oagw-dod-error-envelope`,
//! `cpt-cf-oagw-dod-error-source-gateway-header`).

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;

/// Header set on every gateway-originated error response, distinguishing it
/// from an upstream-originated error passed through verbatim.
pub const ERROR_SOURCE_HEADER_NAME: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER_NAME`] for a gateway-originated error.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

const ERROR_SOURCE_HEADER: HeaderName = HeaderName::from_static(ERROR_SOURCE_HEADER_NAME);

/// The oagw gateway-originated error-type catalog, per `DESIGN.md` §3.3's
/// Error Response Format table, plus `CorsOriginNotAllowed`/
/// `CorsMethodNotAllowed` -- an oagw-level addition beyond DESIGN's table
/// (see `docs/features/gear-foundation.md`'s `cpt-cf-oagw-dod-error-envelope`
/// "Accepted residual risk" note: `DESIGN.md` is a frozen input whose §3.3
/// table has no `403` row at all, so these two rows exist only here and in
/// the oagw-level catalog doc, never in the frozen source). This rendering
/// engine can render every variant; which later feature actually raises a
/// given kind (e.g. `RateLimitExceeded` is raised by
/// `cpt-cf-oagw-feature-rate-limiting`, 2.8; the two CORS kinds are raised by
/// `cpt-cf-oagw-feature-cors-handling`, 2.7) is out of scope for this
/// feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OagwErrorKind {
    RouteError,
    ValidationError,
    MissingTargetHost,
    InvalidTargetHost,
    UnknownTargetHost,
    AuthenticationFailed,
    CorsOriginNotAllowed,
    CorsMethodNotAllowed,
    RouteNotFound,
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
    IdleTimeout,
}

struct CatalogEntry {
    status: u16,
    gts_type: &'static str,
    title: &'static str,
}

impl OagwErrorKind {
    // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-02
    const fn catalog_entry(self) -> CatalogEntry {
        match self {
            Self::RouteError => CatalogEntry {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                title: "Route Error",
            },
            Self::ValidationError => CatalogEntry {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                title: "Validation Error",
            },
            Self::MissingTargetHost => CatalogEntry {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
                title: "Missing Target Host",
            },
            Self::InvalidTargetHost => CatalogEntry {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
                title: "Invalid Target Host",
            },
            Self::UnknownTargetHost => CatalogEntry {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
                title: "Unknown Target Host",
            },
            Self::AuthenticationFailed => CatalogEntry {
                status: 401,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
                title: "Authentication Failed",
            },
            Self::CorsOriginNotAllowed => CatalogEntry {
                status: 403,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
                title: "CORS Origin Not Allowed",
            },
            Self::CorsMethodNotAllowed => CatalogEntry {
                status: 403,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
                title: "CORS Method Not Allowed",
            },
            Self::RouteNotFound => CatalogEntry {
                status: 404,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
                title: "Route Not Found",
            },
            Self::PluginInUse => CatalogEntry {
                status: 409,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
                title: "Plugin In Use",
            },
            Self::PayloadTooLarge => CatalogEntry {
                status: 413,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
                title: "Payload Too Large",
            },
            Self::RateLimitExceeded => CatalogEntry {
                status: 429,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
                title: "Rate Limit Exceeded",
            },
            Self::SecretNotFound => CatalogEntry {
                status: 500,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
                title: "Secret Not Found",
            },
            Self::ProtocolError => CatalogEntry {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
                title: "Protocol Error",
            },
            Self::DownstreamError => CatalogEntry {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
                title: "Downstream Error",
            },
            Self::StreamAborted => CatalogEntry {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
                title: "Stream Aborted",
            },
            Self::LinkUnavailable => CatalogEntry {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
                title: "Link Unavailable",
            },
            Self::CircuitBreakerOpen => CatalogEntry {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
                title: "Circuit Breaker Open",
            },
            Self::PluginNotFound => CatalogEntry {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
                title: "Plugin Not Found",
            },
            Self::ConnectionTimeout => CatalogEntry {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
                title: "Connection Timeout",
            },
            Self::RequestTimeout => CatalogEntry {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
                title: "Request Timeout",
            },
            Self::IdleTimeout => CatalogEntry {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
                title: "Idle Timeout",
            },
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-02
}

/// RFC 9457 Problem Details envelope rendered by [`OagwError::render`].
///
/// Standard fields (`type`/`title`/`status`/`detail`/`instance`) plus the
/// `upstream_id`/`host`/`path`/`retry_after_seconds`/`trace_id` extension
/// fields, each populated when available in context and omitted otherwise.
#[derive(Debug, Clone, Serialize)]
pub struct OagwProblem {
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

const FALLBACK_BODY: &[u8] =
    br#"{"title":"Internal","status":500,"detail":"failed to serialize oagw problem"}"#;

// @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-05
// @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-06
// @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-07
impl axum::response::IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry_after_seconds = self.retry_after_seconds;
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| FALLBACK_BODY.to_vec());

        // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-08
        let mut response = (
            status,
            [(header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
            body,
        )
            .into_response();
        // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-08

        // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-09
        response.headers_mut().insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-09

        // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-10
        // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-11
        if let Some(seconds) = retry_after_seconds {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-11
        // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-10

        // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-12
        response
        // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-12
    }
}
// @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-07
// @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-06
// @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-05

/// A gateway-originated error, carrying an [`OagwErrorKind`] plus whatever
/// request context is available at the point the error is raised.
// @cpt-algo:cpt-cf-oagw-algo-error-render:p1
// @cpt-dod:cpt-cf-oagw-dod-error-envelope:p1
// @cpt-dod:cpt-cf-oagw-dod-error-source-gateway-header:p1
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: OagwErrorKind,
    detail: String,
    instance: Option<String>,
    upstream_id: Option<String>,
    host: Option<String>,
    path: Option<String>,
    retry_after_seconds: Option<u64>,
    trace_id: Option<String>,
}

impl OagwError {
    /// Construct a new gateway error of the given `kind` with an
    /// occurrence-specific `detail` message.
    // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-01
    #[must_use]
    pub fn new(kind: OagwErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            trace_id: None,
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-01

    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_retry_after_seconds(mut self, retry_after_seconds: u64) -> Self {
        self.retry_after_seconds = Some(retry_after_seconds);
        self
    }

    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Render this error as the RFC 9457 [`OagwProblem`] envelope, resolving
    /// the documented `(status, GTS type, title)` triple from the catalog
    /// and including each extension field only when it was populated in
    /// context.
    // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-03
    // @cpt-begin:cpt-cf-oagw-algo-error-render:p1:inst-error-render-04
    #[must_use]
    pub fn render(&self) -> OagwProblem {
        let entry = self.kind.catalog_entry();
        OagwProblem {
            problem_type: entry.gts_type.to_owned(),
            title: entry.title.to_owned(),
            status: entry.status,
            detail: self.detail.clone(),
            instance: self.instance.clone(),
            upstream_id: self.upstream_id.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            retry_after_seconds: self.retry_after_seconds,
            trace_id: self.trace_id.clone(),
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-04
    // @cpt-end:cpt-cf-oagw-algo-error-render:p1:inst-error-render-03
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        self.render().into_response()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    const ALL_KINDS: [(OagwErrorKind, u16, &str); 22] = [
        (
            OagwErrorKind::RouteError,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        ),
        (
            OagwErrorKind::ValidationError,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        ),
        (
            OagwErrorKind::MissingTargetHost,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
        ),
        (
            OagwErrorKind::InvalidTargetHost,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
        ),
        (
            OagwErrorKind::UnknownTargetHost,
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
        ),
        (
            OagwErrorKind::AuthenticationFailed,
            401,
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        ),
        (
            OagwErrorKind::CorsOriginNotAllowed,
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
        ),
        (
            OagwErrorKind::CorsMethodNotAllowed,
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
        ),
        (
            OagwErrorKind::RouteNotFound,
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        ),
        (
            OagwErrorKind::PluginInUse,
            409,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
        ),
        (
            OagwErrorKind::PayloadTooLarge,
            413,
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        ),
        (
            OagwErrorKind::RateLimitExceeded,
            429,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
        ),
        (
            OagwErrorKind::SecretNotFound,
            500,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
        ),
        (
            OagwErrorKind::ProtocolError,
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
        ),
        (
            OagwErrorKind::DownstreamError,
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
        ),
        (
            OagwErrorKind::StreamAborted,
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
        ),
        (
            OagwErrorKind::LinkUnavailable,
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        ),
        (
            OagwErrorKind::CircuitBreakerOpen,
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
        ),
        (
            OagwErrorKind::PluginNotFound,
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
        ),
        (
            OagwErrorKind::ConnectionTimeout,
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
        ),
        (
            OagwErrorKind::RequestTimeout,
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
        ),
        (
            OagwErrorKind::IdleTimeout,
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
        ),
    ];

    #[tokio::test]
    async fn every_catalog_row_renders_documented_status_type_and_title() {
        for (kind, status, gts_type) in ALL_KINDS {
            let err =
                OagwError::new(kind, "occurrence-specific detail").with_instance("/oagw/v1/x");
            let response = err.into_response();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(APPLICATION_PROBLEM_JSON)
            );
            assert_eq!(
                response
                    .headers()
                    .get(ERROR_SOURCE_HEADER_NAME)
                    .and_then(|v| v.to_str().ok()),
                Some(ERROR_SOURCE_GATEWAY)
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["type"], gts_type);
            assert!(json["title"].is_string());
            assert_eq!(json["status"], status);
            assert!(json["detail"].is_string());
            assert_eq!(json["instance"], "/oagw/v1/x");
        }
    }

    #[tokio::test]
    async fn retry_after_seconds_present_in_body_and_header() {
        let err = OagwError::new(OagwErrorKind::RateLimitExceeded, "too many requests")
            .with_retry_after_seconds(7);
        let response = err.into_response();
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["retry_after_seconds"], 7);
    }

    #[tokio::test]
    async fn omitted_extension_fields_do_not_appear_and_response_stays_valid() {
        let err = OagwError::new(OagwErrorKind::ValidationError, "bad request");
        let response = err.into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("upstream_id").is_none());
        assert!(json.get("host").is_none());
        assert!(json.get("path").is_none());
        assert!(json.get("retry_after_seconds").is_none());
        assert!(json.get("trace_id").is_none());
    }
}
