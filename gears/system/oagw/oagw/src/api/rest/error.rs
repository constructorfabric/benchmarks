//! Management-API error mapping: `DomainError` → toolkit canonical errors.
//!
//! The proxy path (which carries its own RFC 9457 problem documents with
//! `X-OAGW-Error-Source`) is deliberately not routed through this mapping —
//! only the CRUD operations are.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, GTS_PLUGIN_IN_USE};

/// Resource error family for the OAGW control plane.
#[resource_error(gts_id!("cf.oagw.control_plane.resource.v1~"))]
pub struct OagwError;

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::Validation(msg) => OagwError::invalid_argument().with_format(msg).create(),
            DomainError::AliasConflict(alias) => {
                OagwError::already_exists(format!("upstream alias is already in use: {alias}"))
                    .with_resource(alias)
                    .create()
            }
            DomainError::RouteConflict(msg) => {
                OagwError::already_exists(format!("duplicate route match rule: {msg}"))
                    .with_resource(msg)
                    .create()
            }
            DomainError::NotFound(msg)
            | DomainError::SecretNotFound(msg)
            | DomainError::PluginNotFound(msg) => OagwError::not_found(msg.clone())
                .with_resource(msg)
                .create(),
            DomainError::PluginInUse {
                plugin_id,
                upstreams,
                routes,
            } => {
                // ADR-0001: plugin deletion while referenced → 409 Conflict.
                let detail = format!(
                    "plugin {plugin_id} is referenced by {} upstream(s) and {} route(s)",
                    upstreams.len(),
                    routes.len()
                );
                OagwError::already_exists(detail)
                    .with_resource(plugin_id)
                    .create()
            }
            // The proxy-path variants (pre-auth) use the canonical category
            // that best matches their façade; the management API surface is
            // only reachable after the authn/authz middleware, so most of
            // these remain defensive.
            DomainError::PermissionDenied(msg) => {
                OagwError::permission_denied().with_reason(msg).create()
            }
            err @ (DomainError::MissingTargetHost { .. }
            | DomainError::InvalidTargetHost(_)
            | DomainError::UnknownTargetHost { .. }) => {
                let detail = err.to_string();
                OagwError::invalid_argument().with_format(detail).create()
            }
            DomainError::AuthenticationFailed(msg) => {
                CanonicalError::unauthenticated().with_reason(msg).create()
            }
            DomainError::PayloadTooLarge(msg) => {
                OagwError::invalid_argument().with_format(msg).create()
            }
            DomainError::RateLimitExceeded { .. } => {
                OagwError::resource_exhausted("rate limit exceeded")
                    .with_quota_violation("rate-limit", "request rate limit exceeded")
                    .create()
            }
            DomainError::CorsOriginNotAllowed(_) | DomainError::CorsMethodNotAllowed(_) => {
                OagwError::invalid_argument()
                    .with_format("CORS policy rejected this request")
                    .create()
            }
            DomainError::RouteNotFound { host } => {
                OagwError::not_found(format!("no route matched for {host}"))
                    .with_resource(host)
                    .create()
            }
            DomainError::ProtocolError(msg)
            | DomainError::DownstreamError(msg)
            | DomainError::StreamAborted(msg) => OagwError::unknown(msg).create(),
            DomainError::LinkUnavailable(msg) | DomainError::UpstreamDisabled(msg) => {
                CanonicalError::service_unavailable()
                    .with_detail(msg)
                    .create()
            }
            DomainError::CircuitBreakerOpen => CanonicalError::service_unavailable().create(),
            DomainError::ConnectionTimeout
            | DomainError::RequestTimeout
            | DomainError::IdleTimeout => {
                OagwError::deadline_exceeded("request timed out").create()
            }
            DomainError::Internal(msg) => CanonicalError::internal(msg).create(),
        }
    }
}

/// Convenience accessor used by tests.
#[must_use]
pub fn canonical_from(e: DomainError) -> CanonicalError {
    e.into()
}

/// RFC 9457 problem response for a plugin-deletion conflict (ADR-0001).
///
/// The toolkit canonical `AlreadyExists` type cannot carry the documented
/// `cf.oagw.plugin.in_use.v1` instance id (`TransportOverride` only overrides the
/// HTTP status, not the GTS type), so the handler renders this problem
/// directly with the exact type + `referenced_by` extension field.
#[must_use]
pub fn plugin_in_use_problem(
    plugin_id: &str,
    upstreams: &[String],
    routes: &[String],
) -> axum::response::Response {
    let detail = format!(
        "plugin {plugin_id} is referenced by {} upstream(s) and {} route(s)",
        upstreams.len(),
        routes.len()
    );
    let body = serde_json::json!({
        "type": GTS_PLUGIN_IN_USE,
        "title": "Plugin In Use",
        "status": 409,
        "detail": detail,
        "plugin_id": plugin_id,
        "referenced_by": {
            "upstreams": upstreams,
            "routes": routes,
        },
    });
    let mut resp = axum::response::Response::new(axum::body::Body::from(
        serde_json::to_vec(&body).unwrap_or_default(),
    ));
    *resp.status_mut() = axum::http::StatusCode::CONFLICT;
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    resp
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::error::DomainError;

    #[test]
    fn validation_maps_to_bad_request() {
        let problem = toolkit_canonical_errors::Problem::from(canonical_from(
            DomainError::Validation("bad field".into()),
        ));
        assert_eq!(problem.status, 400);
    }

    #[test]
    fn alias_conflict_maps_to_conflict() {
        let problem = toolkit_canonical_errors::Problem::from(canonical_from(
            DomainError::AliasConflict("dup".into()),
        ));
        assert_eq!(problem.status, 409);
    }

    #[test]
    fn not_found_maps_to_404() {
        let problem = toolkit_canonical_errors::Problem::from(canonical_from(
            DomainError::NotFound("x".into()),
        ));
        assert_eq!(problem.status, 404);
    }

    #[test]
    fn rate_limit_maps_to_429() {
        let problem = toolkit_canonical_errors::Problem::from(canonical_from(
            DomainError::RateLimitExceeded {
                retry_after_secs: 1,
                limit: 10,
                remaining: 0,
                reset_at_unix: 1,
            },
        ));
        assert_eq!(problem.status, 429);
    }

    #[test]
    fn plugin_in_use_maps_to_conflict() {
        let problem =
            toolkit_canonical_errors::Problem::from(canonical_from(DomainError::PluginInUse {
                plugin_id: "p-1".into(),
                upstreams: vec!["gts.cf.core.oagw.upstream.v1~u-1".into()],
                routes: vec!["gts.cf.core.oagw.route.v1~r-1".into()],
            }));
        // ADR-0001: 409 Conflict with referenced_by detail.
        assert_eq!(problem.status, 409);
    }

    #[test]
    fn plugin_in_use_problem_has_exact_gts_type_and_referenced_by() {
        let resp = plugin_in_use_problem(
            "p-1",
            &["gts.cf.core.oagw.upstream.v1~u-1".to_owned()],
            &["gts.cf.core.oagw.route.v1~r-1".to_owned()],
        );
        assert_eq!(resp.status(), 409);
        // ADR-0001: the problem `type` is the exact OAGW plugin.in_use id.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bytes = rt
            .block_on(http_body_util::BodyExt::collect(resp.into_body()))
            .unwrap()
            .to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], GTS_PLUGIN_IN_USE);
        assert_eq!(
            body["referenced_by"]["upstreams"],
            serde_json::json!(["gts.cf.core.oagw.upstream.v1~u-1"])
        );
        assert_eq!(
            body["referenced_by"]["routes"],
            serde_json::json!(["gts.cf.core.oagw.route.v1~r-1"])
        );
        assert_eq!(body["status"], 409);
    }
}
