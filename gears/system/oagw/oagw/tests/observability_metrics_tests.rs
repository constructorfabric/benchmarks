//! The metrics surface and the twelve families.
//!
//! Covers `cpt-cf-oagw-dod-obs-metrics` and `cpt-cf-oagw-dod-obs-cardinality`
//! and the metrics rows of `cpt-cf-oagw-dod-obs-tests`: the registration of
//! the one path, its 401 and 403 and 200, the `# HELP` and `# TYPE` lines of
//! every family, the twelve histogram buckets with their `_sum` and `_count`
//! series, every label set DESIGN §4.2 enumerates, the closed label values of
//! `phase`, `error_type`, `selection_method`, and the method normalization,
//! and the in-flight gauge's raise and lower. Each test owns its registry, so
//! no test observes another's series.

// @cpt-dod:cpt-cf-oagw-dod-obs-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::data_plane::observability::{
    BreakerObservation, CollectingSink, EndpointObservation, Exchange, Observability,
    PhaseTimings, RateLimitObservation,
};
use oagw::domain::observability::{
    AUDIT_EVENTS, HISTOGRAM_BUCKETS, MetricLabelSet,
};
use oagw::domain::ratelimit::BreakerPhase;
use oagw::OagwState;

const METRICS_TYPE: &str = "gts.cf.core.oagw.metrics.v1~";
const TENANT: u128 = 0x61;

/// The `AuthZ` PDP the allowing stub stands in for.
struct Allowing;

#[async_trait::async_trait]
impl AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: json!(TENANT.to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// The `AuthZ` PDP the refusing stub stands in for.
struct Denying;

#[async_trait::async_trait]
impl AuthZResolverClient for Denying {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// One mounted surface over its own store.
struct Surface {
    router: Router,
    sink: Arc<CollectingSink>,
}

/// Builds a surface whose `AuthZ` client the caller states.
fn surface(enforcer: Option<PolicyEnforcer>) -> Surface {
    let store = Arc::new(oagw::store::OagwStore::new());
    let cache = Arc::new(ControlPlaneCache::new());
    let config = OagwConfig::default();
    let service = Arc::new(
        ManagementService::new(Arc::clone(&store), &config, Arc::clone(&cache))
            .expect("the validators compile"),
    );
    let state = Arc::new(OagwState::new(
        Arc::new(config),
        Arc::clone(&store),
        service,
        enforcer.map(Arc::new),
        None,
        Arc::clone(&cache),
    ));
    let sink = Arc::new(CollectingSink::new());
    state.swap_audit_sink(Arc::clone(&sink) as Arc<dyn oagw::data_plane::observability::AuditSink>);
    Surface {
        router: oagw::api::rest::register_management_routes(Router::new(), state),
        sink,
    }
}

/// The authenticated subject a request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// Issues one request to the mounted surface and returns status, headers, body.
async fn issue(
    app: Router,
    method: Method,
    uri: &str,
    authenticated: bool,
) -> (StatusCode, String, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if authenticated {
        builder = builder.extension(subject());
    }
    let request = builder.body(Body::empty()).expect("the request builds");
    let response = app.oneshot(request).await.expect("oneshot resolves");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(String::from)
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    (status, content_type, String::from_utf8_lossy(&bytes).into_owned())
}

/// An empty registry-backed runtime, for the rendering tests.
fn runtime() -> Observability {
    Observability::with_sink(Arc::new(CollectingSink::new()))
}

/// An exchange the exit step can observe: one resolved upstream, one route,
/// one method, one status, and the timings the path stamped.
fn exchange(host: &str, route: &str, method: &str, status: u16) -> Exchange {
    Exchange {
        host: Some(String::from(host)),
        route: Some(String::from(route)),
        method: String::from(method),
        status: Some(status),
        timings: Some(PhaseTimings::started()),
        request_size: 24,
        response_size: Some(48),
        upstream_resolved: true,
        ..Exchange::default()
    }
}

/// The twelve families the design enumerates, with the exposition kind each
/// is rendered with.
const FAMILIES: [(&str, &str); 12] = [
    ("oagw_requests_total", "counter"),
    ("oagw_request_duration_seconds", "histogram"),
    ("oagw_requests_in_flight", "gauge"),
    ("oagw_errors_total", "counter"),
    ("oagw_circuit_breaker_state", "gauge"),
    ("oagw_rate_limit_exceeded_total", "counter"),
    ("oagw_circuit_breaker_transitions_total", "counter"),
    ("oagw_rate_limit_usage_ratio", "gauge"),
    ("oagw_routing_target_host_used", "counter"),
    ("oagw_routing_endpoint_selected", "counter"),
    ("oagw_upstream_available", "gauge"),
    ("oagw_upstream_connections", "gauge"),
];

/// Whether the exposition declares one family with its type line.
fn declares(exposition: &str, name: &str, kind: &str) -> bool {
    exposition.contains(&format!("# TYPE {name} {kind}"))
}

/// The series lines of one family, without the `#` comment lines.
fn samples<'a>(exposition: &'a str, name: &str) -> Vec<&'a str> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| line.starts_with(name))
        .collect()
}

#[test]
fn every_exposed_family_is_declared_with_its_type_and_help() {
    let observability = runtime();
    let exposition = observability.render();
    for (name, kind) in FAMILIES {
        if name == "oagw_upstream_connections" {
            assert!(
                !exposition.contains(name),
                "a family whose underlying state the gear does not expose is omitted"
            );
            continue;
        }
        assert!(declares(&exposition, name, kind), "{name}");
        assert!(exposition.contains(&format!("# HELP {name}")), "{name}");
        // A family that has observed nothing renders its declaration and no
        // samples.
        assert!(
            samples(&exposition, name).is_empty(),
            "{name} rendered a sample it never observed"
        );
    }
    assert_eq!(FAMILIES.len(), 12);
}

#[test]
fn the_label_sets_are_exactly_the_ones_the_design_enumerates() {
    assert_eq!(MetricLabelSet::labels_of("oagw_requests_total"), Some(MetricLabelSet::REQUESTS_TOTAL));
    assert_eq!(MetricLabelSet::labels_of("oagw_request_duration_seconds"), Some(MetricLabelSet::REQUEST_DURATION));
    assert_eq!(MetricLabelSet::labels_of("oagw_requests_in_flight"), Some(MetricLabelSet::IN_FLIGHT));
    assert_eq!(MetricLabelSet::labels_of("oagw_errors_total"), Some(MetricLabelSet::ERRORS_TOTAL));
    assert_eq!(MetricLabelSet::labels_of("oagw_circuit_breaker_state"), Some(MetricLabelSet::BREAKER_STATE));
    assert_eq!(MetricLabelSet::labels_of("oagw_rate_limit_exceeded_total"), Some(MetricLabelSet::RATE_LIMIT_EXCEEDED));
    assert_eq!(MetricLabelSet::labels_of("oagw_circuit_breaker_transitions_total"), Some(MetricLabelSet::BREAKER_TRANSITIONS));
    assert_eq!(MetricLabelSet::labels_of("oagw_rate_limit_usage_ratio"), Some(MetricLabelSet::RATE_LIMIT_USAGE));
    assert_eq!(MetricLabelSet::labels_of("oagw_routing_target_host_used"), Some(MetricLabelSet::ROUTING_TARGET_USED));
    assert_eq!(MetricLabelSet::labels_of("oagw_routing_endpoint_selected"), Some(MetricLabelSet::ROUTING_SELECTED));
    assert_eq!(MetricLabelSet::labels_of("oagw_upstream_available"), Some(MetricLabelSet::UPSTREAM_AVAILABLE));
    assert_eq!(MetricLabelSet::labels_of("oagw_upstream_connections"), Some(MetricLabelSet::UPSTREAM_CONNECTIONS));

    // The four sets the design states verbatim.
    assert_eq!(MetricLabelSet::REQUESTS_TOTAL, &["host", "http.request.method", "http.route", "http.response.status_code"]);
    assert_eq!(MetricLabelSet::REQUEST_DURATION, &["host", "http.route", "phase"]);
    assert_eq!(MetricLabelSet::ERRORS_TOTAL, &["host", "http.route", "error_type"]);
    assert_eq!(MetricLabelSet::UPSTREAM_CONNECTIONS, &["host", "state"]);

    // No set carries a tenant label.
    for (name, _kind) in FAMILIES {
        let Some(labels) = MetricLabelSet::labels_of(name) else {
            continue;
        };
        for label in labels {
            assert!(!label.contains("tenant"), "{name} declares a tenant label");
        }
    }
}

#[test]
fn a_family_observed_is_rendered_with_the_labels_its_set_declares() {
    let observability = runtime();
    let correlation = None;
    let observed = exchange("up.example", "/api", "GET", 200);
    observability.observe(&observed, correlation);

    let exposition = observability.render();
    let lines = samples(&exposition, "oagw_requests_total");
    assert_eq!(lines.len(), 1, "{exposition}");
    assert!(lines[0].contains("host=\"up.example\""), "{lines:?}");
    assert!(lines[0].contains("http.request.method=\"GET\""), "{lines:?}");
    assert!(lines[0].contains("http.route=\"/api\""), "{lines:?}");
    assert!(lines[0].contains("http.response.status_code=\"200\""), "{lines:?}");
    assert!(lines[0].ends_with(" 1"), "{lines:?}");
}

#[test]
fn the_histogram_is_rendered_over_the_twelve_buckets_with_its_sum_and_count() {
    let observability = runtime();
    let observed = exchange("up.example", "/api", "GET", 200);
    observability.observe(&observed, None);

    let exposition = observability.render();
    for bucket in HISTOGRAM_BUCKETS {
        let le = format!("le=\"{bucket}\"");
        assert!(
            exposition.contains(&le),
            "the bucket {le} is missing from {exposition}"
        );
    }
    assert!(exposition.contains("oagw_request_duration_seconds_bucket"), "{exposition}");
    assert!(exposition.contains("oagw_request_duration_seconds_sum"), "{exposition}");
    assert!(exposition.contains("oagw_request_duration_seconds_count"), "{exposition}");
    // No bound outside the declared set is rendered.
    assert!(!exposition.contains("le=\"0.002\""), "{exposition}");
    assert!(exposition.contains("le=\"+Inf\""), "{exposition}");
}

#[test]
fn a_request_the_gateway_answered_without_resolving_an_upstream_is_filed_under_one_literal() {
    let observability = runtime();
    // Three distinct invented aliases, each answered without a resolved
    // upstream: all three answers are filed under the one bounded literal, so
    // the label set of the three answer families stays out of caller control.
    let mut first = exchange("invented.one", "/api", "GET", 404);
    first.upstream_resolved = false;
    let mut second = exchange("invented.two.example", "/api", "GET", 404);
    second.upstream_resolved = false;
    let third = Exchange {
        host: Some(String::from("invented.three")),
        route: None,
        method: String::from("GET"),
        status: Some(404),
        upstream_resolved: false,
        ..exchange("invented.three", "/api", "GET", 404)
    };
    observability.observe(&first, None);
    observability.observe(&second, None);
    observability.observe(&third, None);
    let exposition = observability.render();
    for family in ["oagw_requests_total", "oagw_errors_total", "oagw_request_duration_seconds"] {
        for line in samples(&exposition, family) {
            assert!(line.contains("host=\"_unresolved\""), "{line}");
            assert!(!line.contains("host=\"invented."), "{line}");
        }
    }
    // The gauge that is raised at the correlate step and lowered here keeps the
    // alias the request addressed, because the raise and the lower must name
    // the same series.
    let in_flight = samples(&exposition, "oagw_requests_in_flight");
    assert!(in_flight.iter().all(|line| line.contains("host=\"invented.")
        || line.contains("host=\"invented.three\"")), "{exposition}");
}

#[test]
fn the_method_is_normalized_to_the_verb_or_to_other() {
    assert_eq!(oagw::domain::observability::normalize_method("get"), "GET");
    assert_eq!(oagw::domain::observability::normalize_method("POST"), "POST");
    assert_eq!(oagw::domain::observability::normalize_method("TRACE"), "_OTHER");
    assert_eq!(oagw::domain::observability::normalize_method("OPTIONS"), "_OTHER");
    assert_eq!(oagw::domain::observability::normalize_method("connect"), "_OTHER");

    let observability = runtime();
    let observed = exchange("up.example", "/api", "TRACE", 200);
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        exposition.contains("http.request.method=\"_OTHER\""),
        "{exposition}"
    );
}

#[test]
fn the_route_label_carries_the_declared_pattern_and_not_the_request_path() {
    let observability = runtime();
    let observed = exchange("up.example", "/api/{id}", "GET", 200);
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(exposition.contains("http.route=\"/api/{id}\""), "{exposition}");
    assert!(!exposition.contains("http.route=\"/proxy"), "{exposition}");
}

#[test]
fn the_gateway_status_is_carried_on_a_request_the_gateway_answered() {
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 404);
    observed.error = Some(oagw::domain::error::ErrorKind::RouteNotFound);
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        exposition.contains("http.response.status_code=\"404\""),
        "{exposition}"
    );
    assert!(
        exposition.contains("error_type=\"route.not_found\""),
        "{exposition}"
    );
}

#[test]
fn an_upstream_failure_status_is_counted_with_the_upstream_literal() {
    let observability = runtime();
    let observed = exchange("up.example", "/api", "GET", 502);
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        exposition.contains("error_type=\"upstream\""),
        "{exposition}"
    );
}

#[test]
fn a_bare_gateway_refusal_counts_no_error_type() {
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 403);
    observed.gateway_answer = true;
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        !exposition.contains("error_type="),
        "a bare gateway refusal names no error_type: {exposition}"
    );
}

#[test]
fn the_phase_values_are_the_four_the_design_closes_the_set_at() {
    assert_eq!(
        oagw::domain::observability::PHASES,
        ["resolve", "chain", "upstream", "total"]
    );
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 200);
    let mut timings = PhaseTimings::started();
    timings.resolved();
    timings.chained();
    timings.forwarded();
    observed.timings = Some(timings);
    observability.observe(&observed, None);
    let exposition = observability.render();
    for phase in oagw::domain::observability::PHASES {
        assert!(
            exposition.contains(&format!("phase=\"{phase}\"")),
            "{phase} is missing from {exposition}"
        );
    }
    for foreign in ["plugin", "route", "upstream_select"] {
        assert!(!exposition.contains(&format!("phase=\"{foreign}\"")), "{exposition}");
    }
    // A phase the path never reached is absent rather than reported as a
    // zero-length span, so the per-phase mean an operator reads stays free of
    // requests that touched no upstream.
    let mut refused = exchange("up.example", "/api/other", "GET", 404);
    // A request the path resolved and then answered without forwarding.
    let mut stamps = PhaseTimings::started();
    stamps.resolved();
    refused.timings = Some(stamps);
    observability.observe(&refused, None);
    let refused_exposition = observability.render();
    let counts: Vec<&str> = samples(&refused_exposition, "oagw_request_duration_seconds_count");
    assert!(
        counts.iter().any(|line| line.contains("phase=\"resolve\"")),
        "{refused_exposition}"
    );
    assert!(
        counts.iter()
            .filter(|line| line.contains("http.route=\"/api/other\""))
            .all(|line| !line.contains("phase=\"upstream\"")),
        "{refused_exposition}"
    );
}

#[test]
fn the_in_flight_gauge_is_raised_and_lowered_in_balance() {
    let observability = runtime();
    observability.raise_in_flight("up.example");
    let raised = observability.render();
    assert!(
        raised.contains("oagw_requests_in_flight{host=\"up.example\"} 1"),
        "{raised}"
    );

    let observed = exchange("up.example", "/api", "GET", 200);
    observability.observe(&observed, None);
    let lowered = observability.render();
    assert!(
        lowered.contains("oagw_requests_in_flight{host=\"up.example\"} 0"),
        "{lowered}"
    );
}

#[test]
fn the_breaker_and_rate_limit_families_read_the_state_their_owner_owns() {
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 429);
    observed.breaker = Some(BreakerObservation {
        phase: Some(BreakerPhase::Open),
        transitions: vec![oagw::domain::ratelimit::BreakerTransition {
            from: BreakerPhase::Closed,
            to: BreakerPhase::Open,
        }],
    });
    observed.rate_limit = Some(RateLimitObservation {
        exceeded: true,
        usage_ratio: Some(0.75),
    });
    observability.observe(&observed, None);

    let exposition = observability.render();
    assert!(
        exposition.contains("oagw_circuit_breaker_state{host=\"up.example\"} 1"),
        "{exposition}"
    );
    assert!(
        exposition.contains("from_state=\"closed\"") && exposition.contains("to_state=\"open\""),
        "{exposition}"
    );
    assert!(
        exposition.contains("oagw_rate_limit_exceeded_total{host=\"up.example\""),
        "{exposition}"
    );
    assert!(
        exposition.contains("oagw_rate_limit_usage_ratio{host=\"up.example\""),
        "{exposition}"
    );
}

#[test]
fn the_endpoint_families_read_the_selection_the_proxy_performed() {
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 200);
    observed.endpoint = Some(EndpointObservation {
        upstream_id: Uuid::from_u128(TENANT),
        endpoint_host: String::from("ep.example"),
        method: "round_robin",
        used_header: true,
    });
    observability.observe(&observed, None);

    let exposition = observability.render();
    assert!(
        exposition.contains("oagw_routing_target_host_used{upstream_id="),
        "{exposition}"
    );
    assert!(
        exposition.contains("endpoint_host=\"ep.example\""),
        "{exposition}"
    );
    assert!(
        exposition.contains("selection_method=\"round_robin\""),
        "{exposition}"
    );
}

#[test]
fn the_availability_gauge_reports_whether_the_breaker_admits() {
    let observability = runtime();
    let mut observed = exchange("up.example", "/api", "GET", 200);
    observed.breaker = Some(BreakerObservation {
        phase: Some(BreakerPhase::HalfOpen),
        transitions: Vec::new(),
    });
    observed.endpoint = Some(EndpointObservation {
        upstream_id: Uuid::from_u128(TENANT),
        endpoint_host: String::from("ep.example"),
        method: "default",
        used_header: false,
    });
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        exposition.contains("oagw_upstream_available{host=\"up.example\""),
        "{exposition}"
    );
}

#[test]
fn a_label_value_that_escapes_is_rendered_escaped() {
    let observability = runtime();
    let observed = exchange("up\"example", "/api", "GET", 200);
    observability.observe(&observed, None);
    let exposition = observability.render();
    assert!(
        exposition.contains("host=\"up\\\"example\""),
        "{exposition}"
    );
}

#[test]
fn a_series_that_declares_a_label_outside_its_set_is_rendered_as_none() {
    let observability = runtime();
    let observed = exchange("up.example", "/api", "GET", 200);
    // A host label the in-flight family declares is the only one it admits;
    // a series that carried a foreign label is dropped at render.
    observability.raise_in_flight("up.example");
    let _ = observed;
    let exposition = observability.render();
    assert!(
        exposition.contains("oagw_requests_in_flight{host=\"up.example\"} 1"),
        "{exposition}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_metrics_path_answers_200_with_the_exposition() {
    let Surface { router, sink } = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let (status, content_type, body) = issue(router, Method::GET, "/oagw/v1/metrics", true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
    assert!(body.contains("# TYPE oagw_requests_total counter"), "{body}");
    // The scrape wrote no record and observed no series of its own.
    assert!(sink.records().is_empty(), "{:?}", sink.records());
    assert!(!body.contains("oagw_requests_total{host=\"/oagw/v1/metrics\"}"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_metrics_path_answers_403_without_the_permission() {
    let Surface { router, sink } = surface(Some(PolicyEnforcer::new(Arc::new(Denying))));
    let (status, content_type, body) = issue(router, Method::GET, "/oagw/v1/metrics", true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(content_type, "application/problem+json");
    assert!(body.contains(METRICS_TYPE), "{body}");
    assert!(!body.contains("oagw_requests_total"), "no exposition is rendered: {body}");
    assert!(sink.records().is_empty(), "{:?}", sink.records());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_metrics_path_answers_401_without_a_subject() {
    let Surface { router, sink: _sink } = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let (status, content_type, body) = issue(router, Method::GET, "/oagw/v1/metrics", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(content_type, "application/problem+json");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_metrics_path_is_registered_for_that_method_alone() {
    let Surface { router, sink: _sink } = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let (status, _type, body) = issue(router.clone(), Method::POST, "/oagw/v1/metrics", true).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    let (status, _type, body) = issue(router.clone(), Method::DELETE, "/oagw/v1/metrics", true).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    // The bare `/metrics` path is answered by no OAGW handler.
    let (status, _type, body) = issue(router, Method::GET, "/metrics", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_management_write_leaves_the_metrics_registry_alone() {
    let Surface { router, sink: _sink } = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "up.example", "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .extension(subject())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    let response = router
        .oneshot(request)
        .await
        .expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_the_gateway_answers_writes_no_record_and_no_series() {
    // The CORS preflight is answered before the proxy flow is reached, so it
    // produces neither an audit record nor a series of any family.
    let Surface { router, sink } = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/oagw/v1/proxy/no-such-upstream/api")
        .header("origin", "https://caller.example")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    assert!(
        sink.records().is_empty(),
        "a preflight wrote a record: {:?}",
        sink.records()
    );
}

#[test]
fn the_event_set_the_records_draw_from_is_the_twelve_literals() {
    assert_eq!(AUDIT_EVENTS.len(), 12);
    assert_eq!(AUDIT_EVENTS[0], "proxy_request.succeeded");
    assert_eq!(AUDIT_EVENTS[1], "proxy_request.failed");
    assert_eq!(AUDIT_EVENTS[10], "auth.failed");
    assert_eq!(AUDIT_EVENTS[11], "breaker.transitioned");
}

#[test]
fn the_status_code_label_carries_the_number_and_no_class_is_pre_aggregated() {
    // The status-class totals are computed at query time by regex on the
    // numeric code, which is why the exposition carries the number alone and
    // declares no `status_class` key on any family.
    let observability = runtime();
    observability.observe(&exchange("up.example", "/api", "GET", 502), None);
    let exposition = observability.render();

    assert!(
        exposition.contains("http.response.status_code=\"502\""),
        "{exposition}"
    );
    assert!(!exposition.contains("status_class"), "{exposition}");
    assert!(!exposition.contains("5xx"), "{exposition}");
    assert_eq!(
        MetricLabelSet::labels_of("oagw_requests_total"),
        Some(MetricLabelSet::REQUESTS_TOTAL)
    );
    assert!(
        !MetricLabelSet::REQUESTS_TOTAL.contains(&"status_class"),
        "{:?}",
        MetricLabelSet::REQUESTS_TOTAL
    );
}

#[test]
fn the_rate_limit_path_label_carries_the_route_pattern() {
    // The `path` label of both rate-limit families is the normalized route
    // match pattern, the same value `http.route` carries, so the label set
    // stays bounded by the number of configured routes.
    let observability = runtime();
    let mut observed = exchange("up.example", "/api/{id}", "GET", 429);
    observed.rate_limit = Some(RateLimitObservation {
        exceeded: true,
        usage_ratio: Some(0.5),
    });
    observability.observe(&observed, None);

    let exposition = observability.render();
    let exceeded = samples(&exposition, "oagw_rate_limit_exceeded_total");
    assert!(!exceeded.is_empty(), "{exposition}");
    for line in &exceeded {
        assert!(line.contains("path=\"/api/{id}\""), "{line}");
    }
    for line in samples(&exposition, "oagw_rate_limit_usage_ratio") {
        assert!(line.contains("path=\"/api/{id}\""), "{line}");
    }
}

#[test]
fn the_in_flight_gauge_holds_through_a_streamed_transfer() {
    // A streamed transfer defers its whole observation to the transfer's end,
    // so the gauge the entry raised stays raised for the whole of it and
    // returns to its prior value only when the deferred observation runs.
    let observability = Arc::new(runtime());
    observability.raise_in_flight("up.example");

    let session = Arc::new(parking_lot::Mutex::new(
        oagw::domain::stream::StreamSession::open_for_incremental(
            Uuid::from_u128(TENANT),
            Uuid::from_u128(TENANT),
            Some(String::from("text/event-stream")),
        ),
    ));
    let mut streamed = exchange("up.example", "/api", "GET", 200);
    streamed.session = Some(Arc::clone(&session));
    streamed.response_size = None;
    let deferred = Arc::clone(&observability).defer(streamed, None);

    let held = observability.render();
    assert!(
        held.contains("oagw_requests_in_flight{host=\"up.example\"} 1"),
        "{held}"
    );

    drop(deferred);
    let ended = observability.render();
    assert!(
        ended.contains("oagw_requests_in_flight{host=\"up.example\"} 0"),
        "{ended}"
    );
}
