//! The OAGW RFC 9457 gateway error envelope (`GatewayError`) (feature
//! `cpt-cf-oagw-feature-error-semantics`, flow
//! `cpt-cf-oagw-flow-error-semantics-gateway-error`).
//!
//! The single `application/problem+json` error type used by all management and
//! proxy handlers (DoD `cpt-cf-oagw-dod-error-semantics-envelope`):
//! standard RFC 9457 fields (`type`, `title`, `status`, `detail`,
//! `instance`) plus the OAGW extension fields (`upstream_id`, `host`, `path`,
//! `retry_after_seconds`, `trace_id`) and `X-OAGW-Error-Source: gateway`
//! (DoD `cpt-cf-oagw-dod-error-semantics-source`, algorithm
//! `cpt-cf-oagw-algo-error-semantics-attach-source`).
//!
//! `type` carries the OAGW GTS *instance* identifier from
//! [`DomainError::instance`] (not a canonical category URI) so callers can
//! classify retriability from the type alone (algorithm
//! `cpt-cf-oagw-algo-error-semantics-build-instance`; classification is
//! algorithm `cpt-cf-oagw-algo-error-semantics-classify-retry`).  The
//! request-id/trace correlation fields honor DoD
//! `cpt-cf-oagw-dod-error-semantics-request-id` (algorithm
//! `cpt-cf-oagw-algo-error-semantics-propagate-request-id`).

use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::entity::ReferencedBy;
use crate::domain::error::{DomainError, ErrorSource};

/// Response header tagging the origin of a response
/// (`X-OAGW-Error-Source: gateway|upstream`).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Request correlation/user context attached to a gateway error envelope.
#[derive(Debug, Clone, Default)]
pub struct ErrorRequestContext {
    /// The request path (RFC 9457 `instance` occurrence reference).
    pub request_path: String,
    /// Ingress correlation id (`X-Request-ID`).
    pub request_id: Option<String>,
    /// Distributed-tracing correlation id.
    pub trace_id: Option<String>,
}

/// One upstream/route binding that references a deleted plugin — the
/// serialized `referenced_by` item of the 409 `plugin.in_use` envelope (DoD
/// `cpt-cf-oagw-dod-control-plane-api-plugin-crud`, ADR
/// `cpt-cf-oagw-adr-request-routing`).
#[derive(Debug, Clone, Serialize)]
pub struct ReferencedByEnvelope {
    /// `upstream` or `route` (the referencing resource kind).
    pub resource: String,
    /// The referencing resource's UUID.
    pub id: Uuid,
}

/// Serialized RFC 9457 problem+json gateway error.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayError {
    /// The OAGW GTS instance identifier (e.g.
    /// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`).
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// URI reference identifying the specific occurrence (request path).
    pub instance: String,
    // --- OAGW extension fields ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Ingress correlation id (`X-Request-ID`) — serialized so the error
    /// envelope shares the audit/metrics request id (DoD
    /// `cpt-cf-oagw-dod-observability-audit-correlation`, `inst-ob-cor-record`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Upstream/route bindings referencing a plugin on 409 `plugin.in_use`
    /// (present only for that envelope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<Vec<ReferencedByEnvelope>>,
}

impl GatewayError {
    /// Builds an envelope from a domain error and request context
    /// (algorithm `cpt-cf-oagw-algo-error-semantics-build-instance`).
    #[must_use]
    pub fn from_domain(err: &DomainError, ctx: &ErrorRequestContext) -> Self {
        Self {
            problem_type: err.instance().to_owned(),
            title: err.title().to_owned(),
            status: err.status(),
            detail: err.to_string(),
            instance: if ctx.request_path.is_empty() {
                "/".to_owned()
            } else {
                ctx.request_path.clone()
            },
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: err.retry_after().map(|d| d.as_secs()),
            request_id: ctx.request_id.clone(),
            trace_id: ctx.trace_id.clone(),
            referenced_by: Self::referenced_by_of(err),
        }
    }

    /// Renders the `referenced_by` extension from a domain error carrying
    /// [`ReferencedBy`] items (the 409 `plugin.in_use` envelope).
    fn referenced_by_of(err: &DomainError) -> Option<Vec<ReferencedByEnvelope>> {
        let refs = err.referenced_by();
        if refs.is_empty() {
            return None;
        }
        Some(
            refs.iter()
                .map(|r: &ReferencedBy| ReferencedByEnvelope {
                    resource: r.resource.as_str().to_owned(),
                    id: r.id,
                })
                .collect(),
        )
    }

    /// Attaches the data-plane target context extensions (`upstream_id`,
    /// `host`, `path`).
    #[must_use]
    pub fn with_target(
        mut self,
        upstream_id: Option<String>,
        host: Option<String>,
        path: Option<String>,
    ) -> Self {
        self.upstream_id = upstream_id;
        self.host = host;
        self.path = path;
        self
    }

    /// The effective status code.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        self.status
    }

    /// Renders this envelope as an HTTP response carrying
    /// `X-OAGW-Error-Source: gateway`.
    #[must_use]
    pub fn into_http_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| b"{}".to_vec());

        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        response.headers_mut().insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(ErrorSource::Gateway.as_str()),
        );
        response
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        self.into_http_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::DomainError;
    use serde_json::Value;

    #[test]
    fn envelope_carries_instance_and_extensions() {
        let err = DomainError::RouteNotFound {
            detail: "no route".to_owned(),
        };
        let ctx = ErrorRequestContext {
            request_path: "/api/oagw/v1/proxy/unknown".to_owned(),
            request_id: Some("req-1".to_owned()),
            trace_id: Some("trace-9".to_owned()),
        };
        let env = GatewayError::from_domain(&err, &ctx).with_target(
            Some("up-1".to_owned()),
            Some("api.vendor.com".to_owned()),
            Some("/v1/x".to_owned()),
        );

        assert_eq!(
            env.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(env.status, 404);
        assert_eq!(env.instance, "/api/oagw/v1/proxy/unknown");
        assert_eq!(env.request_id.as_deref(), Some("req-1"));
        assert_eq!(env.trace_id.as_deref(), Some("trace-9"));
        assert_eq!(env.upstream_id.as_deref(), Some("up-1"));
        assert_eq!(env.retry_after_seconds, None);
    }

    #[test]
    fn rate_limit_envelope_has_retry_after_seconds() {
        let err = DomainError::rate_limit_exceeded(
            "too fast".to_owned(),
            Some(std::time::Duration::from_secs(60)),
        );
        let env = GatewayError::from_domain(&err, &ErrorRequestContext::default());
        assert_eq!(env.status, 429);
        assert_eq!(env.retry_after_seconds, Some(60));
    }

    #[test]
    fn response_has_problem_json_and_error_source_header() {
        let err = DomainError::RouteNotFound {
            detail: "no route".to_owned(),
        };
        let resp = GatewayError::from_domain(
            &err,
            &ErrorRequestContext {
                request_path: "/p".to_owned(),
                ..Default::default()
            },
        )
        .into_http_response();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        assert_eq!(resp.headers().get(ERROR_SOURCE_HEADER).unwrap(), "gateway");
    }

    #[test]
    fn body_serializes_standard_and_extension_fields() {
        let err = DomainError::rate_limit_exceeded(
            "too fast".to_owned(),
            Some(std::time::Duration::from_secs(1)),
        );
        let env = GatewayError::from_domain(
            &err,
            &ErrorRequestContext {
                request_path: "/lol".to_owned(),
                request_id: Some("req-1".to_owned()),
                trace_id: Some("t1".to_owned()),
            },
        );
        let v: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(v["status"], 429);
        assert_eq!(v["retry_after_seconds"], 1);
        assert_eq!(v["request_id"], "req-1");
        assert_eq!(v["trace_id"], "t1");
        assert_eq!(v["instance"], "/lol");
        // Absent extensions are not serialized.
        assert!(v.get("upstream_id").is_none());
        assert!(v.get("host").is_none());
    }

    #[test]
    fn plugin_in_use_envelope_carries_referenced_by() {
        use crate::domain::entity::{ReferencedBy, ReferencedByResource};
        let err = DomainError::PluginInUse {
            name: "apikey".to_owned(),
            referenced_by: vec![
                ReferencedBy {
                    resource: ReferencedByResource::Upstream,
                    id: Uuid::from_u128(1),
                },
                ReferencedBy {
                    resource: ReferencedByResource::Route,
                    id: Uuid::from_u128(2),
                },
            ],
        };
        let env = GatewayError::from_domain(&err, &ErrorRequestContext::default());
        assert_eq!(env.status, 409);
        let v: Value = serde_json::to_value(&env).unwrap();
        let rbs = v["referenced_by"]
            .as_array()
            .expect("referenced_by present");
        assert_eq!(rbs.len(), 2);
        assert_eq!(rbs[0]["resource"], "upstream");
        assert_eq!(rbs[0]["id"], Uuid::from_u128(1).to_string());
        assert_eq!(rbs[1]["resource"], "route");
        // Non-in-use errors omit the extension entirely.
        let other = DomainError::RouteNotFound {
            detail: "x".to_owned(),
        };
        let v2: Value = serde_json::to_value(GatewayError::from_domain(
            &other,
            &ErrorRequestContext::default(),
        ))
        .unwrap();
        assert!(v2.get("referenced_by").is_none());
    }
}
