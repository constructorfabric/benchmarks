//! Tests of the observability boundary the proxy handler wraps the pipeline in
//! (`cpt-cf-oagw-flow-observability-and-state-request-metrics`,
//! `cpt-cf-oagw-flow-observability-and-state-audit-record`,
//! `cpt-cf-oagw-flow-observability-and-state-trace-identifiers`).

use std::sync::Arc;

use std::time::Instant;

use super::super::error::{complete_problem_context, TraceIdentifier};
use super::{observe_outcome, Dispatched};
use crate::domain::proxy::{ProxyObservation, RateLimitObservation};
use crate::domain::services::management::Actor;
use crate::infra::audit::AuditSink;
use crate::infra::metrics::{MetricsRegistry, REQUESTS_IN_FLIGHT, REQUESTS_TOTAL};
use crate::infra::observability::Observability;
use crate::test_support::observation;

/// The seam over a captured audit sink, so the emitted lines are assertable.
fn wired() -> (Arc<MetricsRegistry>, Arc<AuditSink>, Observability) {
    let metrics = Arc::new(MetricsRegistry::new());
    let audit = AuditSink::captured();
    let seam = Observability::new(Arc::clone(&metrics), Arc::clone(&audit));
    (metrics, audit, seam)
}

/// One dispatched exchange, as the pipeline boundary hands it over.
fn outcome(actor: Option<Actor>, observation: Option<ProxyObservation>) -> Dispatched {
    Dispatched {
        trace_id: Some("req_1".to_owned()),
        method: "GET".to_owned(),
        path: "/oagw/v1/proxy/api.vendor.com/v1/orders".to_owned(),
        alias: Some("api.vendor.com".to_owned()),
        actor,
        request_size: observation.as_ref().map_or(0, |o| o.request_size),
        observation,
        error_type: None,
        refused_by_rate_limit: false,
    }
}

/// An exchange closes the in-flight gauge it opened, records the request family
/// once the status is known and emits one audit record under the correlation
/// identifier (`inst-os-req-1`, `inst-os-req-3`, `inst-os-trace-3`).
#[test]
fn an_exchange_closes_the_gauge_and_audits_under_its_identifier() {
    let (metrics, audit, seam) = wired();
    metrics.enter_in_flight("api.vendor.com");
    assert_eq!(metrics.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]), Some(1.0));

    let actor = Actor { tenant_id: uuid::Uuid::new_v4(), principal_id: uuid::Uuid::new_v4() };
    observe_outcome(&seam, &outcome(Some(actor), Some(observation(200, "/v1/orders"))), 200, &Instant::now());
    metrics.leave_in_flight("api.vendor.com");
    assert_eq!(metrics.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]), Some(0.0));
    assert_eq!(
        metrics.value(REQUESTS_TOTAL, &[("host", "api.vendor.com"), ("http.response.status_code", "200")]),
        Some(1.0)
    );
    assert_eq!(audit.lines().len(), 1);
    assert!(audit.lines()[0].contains("\"request_id\":\"req_1\""), "{}", audit.lines()[0]);
    assert!(audit.lines()[0].contains("\"event\":\"proxy_request\""), "{}", audit.lines()[0]);
}

/// A request the authentication surface rejected emits the `auth_failure`
/// class under the identifier the problem body carries
/// (`inst-os-audit-2c`, `inst-os-trace-3`).
#[test]
fn a_rejected_request_is_an_auth_failure_record() {
    let (metrics, audit, seam) = wired();
    observe_outcome(&seam, &outcome(None, None), 401, &Instant::now());
    let lines = audit.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"event\":\"auth_failure\""), "{}", lines[0]);
    assert!(lines[0].contains("\"request_id\":\"req_1\""), "{}", lines[0]);
    assert_eq!(metrics.value(REQUESTS_TOTAL, &[("host", "api.vendor.com")]), Some(1.0));
}

/// A rate-limit refusal is a `WARN`-level `proxy_request` record, emitted
/// however the sampling policy is set (`inst-os-audit-2`, `inst-os-audit-5`).
#[test]
fn a_refused_request_is_audited_as_a_refusal() {
    let (_metrics, audit, seam) = wired();
    audit.set_sample_rate(1_000);
    let mut observation = observation(429, "/v1");
    observation.rate_limit = Some(RateLimitObservation {
        host: "api.vendor.com".to_owned(),
        path: "/v1".to_owned(),
        refused: true,
        usage_ratio_parts_per_million: 900_000,
        retry_after_seconds: Some(3),
    });
    let actor = Actor { tenant_id: uuid::Uuid::new_v4(), principal_id: uuid::Uuid::new_v4() };
    let mut refused = outcome(Some(actor), Some(observation));
    refused.refused_by_rate_limit = true;
    observe_outcome(&seam, &refused, 429, &Instant::now());
    let document: serde_json::Value = serde_json::from_str(&audit.lines()[0]).expect("one JSON object");
    // The refusal rides on the record through its `WARN` level only: the field
    // set stays the fourteen of `inst-os-algo-audit-1`, with no extra key.
    assert_eq!(document["level"], "WARN", "a refusal is a `warn` outcome: {document}");
    assert_eq!(document["event"], "proxy_request", "{document}");
    assert_eq!(document["status"], 429, "{document}");
    assert_eq!(document.as_object().expect("an object").len(), 14, "the field set stays fourteen: {document}");
}

/// The completing layer publishes a correlation identifier for every request:
/// the carried one when the header named it, a minted one otherwise
/// (`inst-os-trace-1`, `inst-os-trace-2`).
#[tokio::test]
async fn the_completing_layer_publishes_the_correlation_identifier() {
    use tower::ServiceExt;

    async fn handler(extension: Option<axum::Extension<TraceIdentifier>>) -> String {
        extension.map(|axum::Extension(identifier)| identifier.as_str().to_owned()).unwrap_or_default()
    }

    let router = axum::Router::new()
        .route("/oagw/v1/proxy/{alias}", axum::routing::get(handler))
        .layer(axum::middleware::from_fn(complete_problem_context));

    let carried = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/oagw/v1/proxy/api.vendor.com")
                .header("x-request-id", "trace-7")
                .body(axum::body::Body::empty())
                .expect("well formed"),
        )
        .await
        .expect("the router answers");
    let carried = axum::body::to_bytes(carried.into_body(), 1024).await.expect("readable");
    assert_eq!(carried, "trace-7", "an inbound identifier is carried, never rewritten");

    let minted = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/oagw/v1/proxy/api.vendor.com")
                .body(axum::body::Body::empty())
                .expect("well formed"),
        )
        .await
        .expect("the router answers");
    let minted = axum::body::to_bytes(minted.into_body(), 1024).await.expect("readable");
    let minted = String::from_utf8_lossy(&minted).to_string();
    let parsed = uuid::Uuid::parse_str(&minted).expect("a minted identifier is a UUID");
    assert_eq!(minted, parsed.to_string(), "the minted identifier is lowercase hyphenated: {minted}");
    assert_ne!(minted, "trace-7", "the minted identifier carries no request-derived content");
}
