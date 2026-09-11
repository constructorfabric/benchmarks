//! REST route registration of the metrics surface (FEATURE entry 2.9,
//! `cpt-cf-oagw-dod-observability-and-state-metric-surface`).
//!
//! The exposition lives on the **gear-relative** `/metrics` path, outside the
//! `/oagw/v1` prefix: it is an operations surface the platform operator
//! scrapes, not a configuration API a tenant calls
//! (`cpt-cf-oagw-actor-platform-operator`). It is registered
//! `authenticated()` — the caller identity the platform authentication
//! middleware resolved is what the admin gate evaluates — and the gate is the
//! `authz_resolver` decision for `gts.cf.core.oagw.proxy.v1~:metrics`, which no proxy-invoke
//! grant implies.
//!
//! The body is the Prometheus text exposition the registry renders, in the
//! family order DESIGN §4.2 lists and with a deterministic series order inside
//! each family (`inst-os-scrape-5`, `-6`).

use std::sync::Arc;

use axum::Router;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use toolkit::api::{OpenApiRegistry, ResponseSpec};
use toolkit_security::SecurityContext;

use super::error::ApiError;
use super::upstream_handlers::actor_of;
use crate::infra::authorization::MetricsGate;
use crate::infra::metrics::MetricsRegistry;

/// The OpenAPI tag of the metrics surface.
const TAG: &str = "OAGW Metrics";

/// The content type of a Prometheus text exposition.
pub const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Register `GET /metrics`.
///
/// The registry and the admin gate are handed over ready-built; the
/// registration adds the one operation and nothing else.
pub fn register_metrics_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    registry: Arc<MetricsRegistry>,
    gate: Arc<MetricsGate>,
) -> Router {
    let router = toolkit::api::OperationBuilder::get(super::METRICS_PATH)
        .operation_id("oagw.get_metrics")
        .summary("Scrape the metrics exposition")
        .description("Read the Prometheus text exposition of the twelve registered metric families. The path is gear-relative and outside the `/oagw/v1` prefix, and the operation is served only to a caller the admin authorization boundary grants `gts.cf.core.oagw.proxy.v1~:metrics`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(metrics)
        .response(ResponseSpec {
            status: 200,
            content_type: METRICS_CONTENT_TYPE,
            description: "The Prometheus text exposition".to_owned(),
            schema: None,
        })
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);
    router.layer(axum::Extension(registry)).layer(axum::Extension(gate))
}

/// `GET /metrics` — the Prometheus text exposition of the twelve families
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-1
// `inst-os-scrape-1` .. `-4`: the exposition is the registry's render, served
// only after the caller identity resolves and the admin gate grants it, with
// the Prometheus content type and no caching of any kind.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-7
pub async fn metrics(
    Extension(registry): Extension<Arc<MetricsRegistry>>,
    Extension(gate): Extension<Arc<MetricsGate>>,
    security: Option<Extension<SecurityContext>>,
) -> Response {
    let actor = match actor_of(security) {
        Ok(actor) => actor,
        Err(error) => return error.into_response(),
    };
    if let Err(denied) = gate.authorize(&actor).await {
        return ApiError::Authorization(denied).into_response();
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(METRICS_CONTENT_TYPE),
        )],
        registry.exposition(),
    )
        .into_response()
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-2
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-metrics-scrape:p1:inst-os-scrape-1
