//! Admin `/metrics` handler (feature `cpt-cf-oagw-feature-observability-audit`,
//! flow `cpt-cf-oagw-flow-observability-audit-scrape`; DoD
//! `cpt-cf-oagw-dod-observability-audit-metrics`).
//!
//! Serves the DESIGN §4.2 metric vocabulary in Prometheus text exposition
//! format over the **admin-only** surface (DESIGN §4.2 "Prometheus metrics at
//! `/metrics` (admin-only)"; PRD `cpt-cf-oagw-nfr-observability` — metrics
//! scraped at `/metrics`).
//!
//! The operation is registered `authenticated` (the platform api-gateway
//! requires authentication per the OpenAPI registry and injects the
//! [`SecurityContext`] extension).  The handler defensively re-checks the
//! extension: an absent context (a direct/unauthenticated call) is rejected
//! with the 401 `auth.failed` envelope **before any metric output is exposed**
//! (`inst-ob-scrape-authz`); an authenticated caller receives the exposition
//! (`inst-ob-scrape-cols`, `inst-ob-scrape-return`).  This mirrors the
//! proxy/control-plane surface contract — there is no role dimension in the
//! platform [`SecurityContext`], so "admin-only" is enforced as
//! "authenticated-only" at this boundary (documented decision).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::domain::GearState;
use crate::infra::error_envelope::{ErrorRequestContext, GatewayError};

/// `GET /metrics` — Prometheus text exposition of the DESIGN §4.2 registry.
///
/// Renders `state.metrics.render_prometheus()` as
/// `text/plain; version=0.0.4` when the caller is authenticated
/// (`inst-ob-scrape-cols`); serves the 401 `auth.failed` envelope without
/// metric output otherwise (`inst-ob-scrape-authz`).
pub async fn metrics(
    maybe_ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<GearState>>,
) -> Response {
    let Some(_ctx) = maybe_ctx else {
        return GatewayError::from_domain(
            &crate::domain::error::DomainError::AuthenticationFailed {
                detail: "metrics require an authenticated caller (admin surface)".to_owned(),
            },
            &ErrorRequestContext {
                request_path: "/metrics".to_owned(),
                ..Default::default()
            },
        )
        .into_http_response();
    };

    let body = state.metrics.render_prometheus();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}
