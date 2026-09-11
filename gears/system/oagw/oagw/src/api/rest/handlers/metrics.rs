//! The metrics surface — the one path `cpt-cf-oagw-feature-observability`
//! registers.
//!
//! `GET /oagw/v1/metrics` is gear-relative on the mount point the foundation
//! created, is registered for that method alone, and is enforced with the
//! `gts.cf.core.oagw.metrics.v1~:read` permission before any collector is
//! read. The handler reads nothing but the seam's exposition and answers no
//! audit record of its own: the scrape observes nothing and is observed by
//! nothing (§1.5).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use toolkit_security::SecurityContext;

use super::{SharedState, READ, SUPPORTED_PROPERTIES};
use crate::api::rest::problem;

/// The content type the Prometheus text exposition format names.
const EXPOSITION_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The enforcer's descriptor of the metrics resource.
#[must_use]
fn metrics_resource_type() -> authz_resolver_sdk::pep::ResourceType {
    authz_resolver_sdk::pep::ResourceType::from_static(
        crate::gts::METRICS_TYPE,
        SUPPORTED_PROPERTIES,
    )
}

/// The 403 the metrics permission is answered with.
fn forbidden(instance: &str) -> Response {
    problem::forbidden_response(crate::gts::METRICS_TYPE, instance)
}

/// Answers `GET /oagw/v1/metrics` with the text exposition of the twelve
/// families DESIGN §4.2 declares.
pub async fn scrape(
    State(state): State<SharedState>,
    context: Option<Extension<SecurityContext>>,
) -> Response {
    let instance = String::from("/oagw/v1/metrics");

    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-issue
    // The actor issues the scrape with a bearer token; the handler answers it
    // with the exposition or with a refusal, and decides nothing else.
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-issue
    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-api
    // The API is the one path this feature registers, on the gear-relative
    // mount point the foundation created, and the platform middleware that
    // authenticates the bearer token has run before this handler did.
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-api

    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-authz
    // The permission is enforced before any collector is read: a token without
    // it is answered 403 and renders nothing, and a request with no
    // authenticated subject is answered 401, because the platform middleware
    // that would have resolved one did not run for it.
    let Some(context) = context.as_ref().map(|extension| &extension.0) else {
        return problem::problem_response(
            &crate::domain::error::DomainError::gateway(
                crate::domain::error::ErrorKind::AuthenticationFailed,
                "the request carries no authenticated subject",
            ),
            &instance,
        );
    };
    let Some(enforcer) = state.enforcer() else {
        tracing::warn!(instance, "no AuthZ client resolved; the metrics surface fails closed");
        return forbidden(&instance);
    };
    if let Err(error) = enforcer
        .access_scope(context, &metrics_resource_type(), READ, None)
        .await
    {
        tracing::warn!(instance, error = %error, "the metrics permission was refused");
        // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-permitted-else
        // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-forbidden
        // RETURN 403 with no exposition rendered, through the foundation's
        // error mapping: an `application/problem+json` body tagged
        // `X-OAGW-Error-Source: gateway` that carries the `trace_id` of the
        // correlation context this scrape request was assigned.
        // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-forbidden
        // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-permitted-else
        return forbidden(&instance);
    }
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-authz

    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-permitted-if
    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-render
    // `cpt-cf-oagw-algo-metrics-render` reads the twelve collectors at the
    // moment the scrape is served and renders the text exposition format, with
    // a `# HELP` and a `# TYPE` line per family and the histogram as its
    // `_bucket` series plus its `_sum` and `_count` series.
    let exposition = state.observability().render();
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-render
    // @cpt-begin:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-return
    // RETURN 200 with the exposition and the content type the format names;
    // no audit record is written and no series is observed for the scrape
    // itself.
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(EXPOSITION_TYPE),
        )
        .body(axum::body::Body::from(exposition))
        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()))
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-return
    // @cpt-end:cpt-cf-oagw-flow-metrics-scrape:p1:inst-ms-permitted-if
}
