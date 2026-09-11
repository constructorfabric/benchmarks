//! The unit tests of the metric registry
//! (`cpt-cf-oagw-dod-observability-and-state-metric-surface`,
//! `-metric-cardinality`, `-rate-limit-metrics`, `-breaker-metrics`).
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-breaker-metrics:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-metric-cardinality:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-metric-surface:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-unit-tests:p1

use super::*;

/// Every family the registry declares carries its declared label keys.
#[test]
fn the_twelve_families_carry_their_declared_label_keys() {
    assert_eq!(FAMILIES.len(), 12);
    assert_eq!(label_keys(REQUESTS_TOTAL).len(), 4);
    assert_eq!(label_keys(REQUEST_DURATION_SECONDS), &["host", "http.route", "phase"]);
    assert_eq!(label_keys(REQUESTS_IN_FLIGHT), &["host"]);
    assert_eq!(label_keys(ERRORS_TOTAL), &["host", "http.route", "error_type"]);
    assert_eq!(label_keys(CIRCUIT_BREAKER_STATE), &["host"]);
    assert_eq!(
        label_keys(CIRCUIT_BREAKER_TRANSITIONS_TOTAL),
        &["host", "from_state", "to_state"]
    );
    assert_eq!(label_keys(RATE_LIMIT_EXCEEDED_TOTAL), &["host", "path"]);
    assert_eq!(label_keys(RATE_LIMIT_USAGE_RATIO), &["host", "path"]);
    assert_eq!(label_keys(ROUTING_TARGET_HOST_USED), &["upstream_id", "endpoint_host"]);
    assert_eq!(
        label_keys(ROUTING_ENDPOINT_SELECTED),
        &["upstream_id", "endpoint_host", "selection_method"]
    );
    assert_eq!(label_keys(UPSTREAM_AVAILABLE), &["host", "endpoint"]);
    assert_eq!(label_keys(UPSTREAM_CONNECTIONS), &["host", "state"]);
}

/// The declared bucket set of the request-duration histogram.
#[test]
fn the_duration_histogram_declares_the_bucket_set() {
    assert_eq!(
        DURATION_BUCKETS,
        [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    );
}

/// The five declared `phase` values, and no other value enters the vocabulary.
#[test]
fn the_phase_label_is_bounded_to_the_declared_set() {
    assert_eq!(
        PHASES,
        ["route_match", "plugin_chain_request", "upstream_call", "plugin_chain_response", "response"]
    );
    let registry = MetricsRegistry::new();
    registry.observe_phase("api.vendor.com", Some("/v1"), "route_match", 0.001);
    registry.observe_phase("api.vendor.com", Some("/v1"), "not_a_phase", 0.001);
    let phases: Vec<String> = registry
        .series(REQUEST_DURATION_SECONDS)
        .into_iter()
        .filter_map(|labels| {
            labels.into_iter().find(|(name, _)| name == "phase").map(|(_, value)| value)
        })
        .collect();
    assert_eq!(phases, vec!["route_match".to_owned()]);
}

/// `http.request.method` is normalized to a standard verb or `_OTHER`.
#[test]
fn a_non_standard_method_is_normalized_to_other() {
    assert_eq!(normalize_method("GET"), "GET");
    assert_eq!(normalize_method("POST"), "POST");
    assert_eq!(normalize_method("PUT"), "PUT");
    assert_eq!(normalize_method("DELETE"), "DELETE");
    assert_eq!(normalize_method("PATCH"), "PATCH");
    assert_eq!(normalize_method("TRACE"), METHOD_OTHER);
    assert_eq!(normalize_method("propfind"), METHOD_OTHER);
}

/// The status code is recorded in its numeric form.
#[test]
fn the_status_label_is_numeric() {
    let registry = MetricsRegistry::new();
    registry.record_request("api.vendor.com", "GET", Some("/v1"), 503);
    let labels = &registry.series(REQUESTS_TOTAL)[0];
    let status = labels.iter().find(|(name, _)| name == "http.response.status_code");
    assert_eq!(status.map(|(_, value)| value.as_str()), Some("503"));
}

/// No series ever carries a tenant label.
#[test]
fn no_metric_series_carries_a_tenant_label() {
    let registry = MetricsRegistry::new();
    registry.record_request("api.vendor.com", "GET", Some("/v1"), 200);
    registry.record_error("api.vendor.com", Some("/v1"), "gts.cf.core.errors.err.v1~x");
    registry.enter_in_flight("api.vendor.com");
    registry.record_rate_limit("api.vendor.com", "/v1", true, 500_000);
    registry.set_upstream_available("api.vendor.com", "10.0.0.1:443", true);
    registry.set_upstream_connections("api.vendor.com", 1, 2, 100);
    for family in FAMILIES {
        for series in registry.series(family) {
            for (name, _) in series {
                assert!(
                    !name.contains("tenant"),
                    "{family} carries the tenant label {name}"
                );
            }
        }
    }
}

/// A raw request path never appears as a `path` label value: the recorder is
/// handed the route match pattern and nothing else.
#[test]
fn the_rate_limit_path_label_is_the_route_pattern() {
    let registry = MetricsRegistry::new();
    registry.record_rate_limit("api.vendor.com", "/v1/orders", true, 1_000_000);
    let labels = &registry.series(RATE_LIMIT_EXCEEDED_TOTAL)[0];
    assert_eq!(
        labels.iter().find(|(name, _)| *name == "path").map(|(_, value)| value.as_str()),
        Some("/v1/orders")
    );
}

/// A request rejected before route matching omits the `http.route` label
/// instead of carrying the raw request path (`inst-os-algo-label-9`).
#[test]
fn a_pre_route_rejection_omits_the_route_label() {
    let registry = MetricsRegistry::new();
    registry.record_request("api.vendor.com", "GET", None, 404);
    registry.record_error("api.vendor.com", None, "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    for family in [REQUESTS_TOTAL, ERRORS_TOTAL] {
        for series in registry.series(family) {
            assert!(!series.iter().any(|(name, _)| name == "http.route"));
        }
    }
}

/// A gateway error increments both the request counter and the error counter.
#[test]
fn a_gateway_error_is_counted_in_both_families() {
    let registry = MetricsRegistry::new();
    registry.record_request("api.vendor.com", "GET", Some("/v1"), 502);
    registry.record_error("api.vendor.com", Some("/v1"), "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
    assert_eq!(registry.value(REQUESTS_TOTAL, &[("host", "api.vendor.com"), ("http.response.status_code", "502")]), Some(1.0));
    assert_eq!(
        registry.value(ERRORS_TOTAL, &[("host", "api.vendor.com")]),
        Some(1.0)
    );
}

/// The in-flight gauge returns to its prior value.
#[test]
fn the_in_flight_gauge_returns_to_its_prior_value() {
    let registry = MetricsRegistry::new();
    registry.enter_in_flight("api.vendor.com");
    registry.enter_in_flight("api.vendor.com");
    assert_eq!(registry.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]), Some(2.0));
    registry.leave_in_flight("api.vendor.com");
    assert_eq!(registry.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]), Some(1.0));
    registry.leave_in_flight("api.vendor.com");
    assert_eq!(registry.value(REQUESTS_IN_FLIGHT, &[("host", "api.vendor.com")]), Some(0.0));
}

/// The usage-ratio gauge stays within 0.0 to 1.0.
#[test]
fn the_usage_ratio_gauge_is_bounded() {
    let registry = MetricsRegistry::new();
    registry.record_rate_limit("api.vendor.com", "/v1", false, 0);
    assert_eq!(registry.value(RATE_LIMIT_USAGE_RATIO, &[("host", "api.vendor.com")]), Some(0.0));
    registry.record_rate_limit("api.vendor.com", "/v1", false, 500_000);
    assert_eq!(registry.value(RATE_LIMIT_USAGE_RATIO, &[("host", "api.vendor.com")]), Some(0.5));
    registry.record_rate_limit("api.vendor.com", "/v1", true, 2_000_000);
    assert_eq!(registry.value(RATE_LIMIT_USAGE_RATIO, &[("host", "api.vendor.com")]), Some(1.0));
}

/// The two breaker families are registered and emit no series in the graded
/// configuration (`inst-os-cb-2`).
#[test]
fn the_breaker_families_are_registered_and_emit_no_series() {
    let registry = MetricsRegistry::new();
    assert_eq!(registry.series(CIRCUIT_BREAKER_STATE).len(), 0);
    assert_eq!(registry.series(CIRCUIT_BREAKER_TRANSITIONS_TOTAL).len(), 0);
    let exposition = registry.exposition();
    assert!(exposition.contains("# TYPE oagw_circuit_breaker_state gauge"));
    assert!(exposition.contains("# TYPE oagw_circuit_breaker_transitions_total counter"));
}

/// A future breaker implementation records a transition without a registry
/// change (`inst-os-cb-5`).
#[test]
fn a_breaker_transition_is_recorded_through_the_registered_vocabulary() {
    let registry = MetricsRegistry::new();
    registry.record_circuit_breaker_transition("api.vendor.com", "closed", "open");
    registry.set_circuit_breaker_state("api.vendor.com", "open");
    assert_eq!(registry.value(CIRCUIT_BREAKER_STATE, &[("host", "api.vendor.com")]), Some(1.0));
}

/// The routing pair carries the selection method of the decision.
#[test]
fn the_routing_families_carry_the_selection_method() {
    let registry = MetricsRegistry::new();
    registry.record_routing("u-1", "10.0.0.1:443", true, SelectionMethod::ExplicitHeader);
    registry.record_routing("u-1", "10.0.0.2:443", false, SelectionMethod::RoundRobin);
    registry.record_routing("u-1", "10.0.0.3:443", false, SelectionMethod::Default);
    let methods: Vec<String> = registry
        .series(ROUTING_ENDPOINT_SELECTED)
        .into_iter()
        .filter_map(|labels| {
            labels.into_iter().find(|(name, _)| name == "selection_method").map(|(_, value)| value)
        })
        .collect();
    assert_eq!(methods, vec!["explicit_header", "round_robin", "default"]);
}

/// `oagw_upstream_available` carries both transitions.
#[test]
fn the_upstream_availability_gauge_carries_the_recovery_transition() {
    let registry = MetricsRegistry::new();
    registry.set_upstream_available("api.vendor.com", "10.0.0.1:443", false);
    assert_eq!(
        registry.value(UPSTREAM_AVAILABLE, &[("host", "api.vendor.com"), ("endpoint", "10.0.0.1:443")]),
        Some(0.0)
    );
    registry.set_upstream_available("api.vendor.com", "10.0.0.1:443", true);
    assert_eq!(
        registry.value(UPSTREAM_AVAILABLE, &[("host", "api.vendor.com"), ("endpoint", "10.0.0.1:443")]),
        Some(1.0)
    );
}

/// The exposition names every registered family with its declared type.
#[test]
fn the_exposition_declares_every_family() {
    let registry = MetricsRegistry::new();
    let exposition = registry.exposition();
    for family in FAMILIES {
        assert!(exposition.contains(&format!("# TYPE {family} ")), "{family} missing");
    }
    assert_eq!(exposition.matches("# TYPE").count(), FAMILIES.len());
}

/// The exposition is parseable Prometheus text: `# HELP`, `# TYPE`, and a
/// bucket/sum/count triple for a histogram series.
#[test]
fn the_exposition_renders_a_histogram_series() {
    let registry = MetricsRegistry::new();
    registry.observe_phase("api.vendor.com", Some("/v1"), "upstream_call", 0.012);
    let exposition = registry.exposition();
    assert!(exposition.contains("# TYPE oagw_request_duration_seconds histogram"));
    assert!(exposition.contains("oagw_request_duration_seconds_bucket{host=\"api.vendor.com\",http.route=\"/v1\",phase=\"upstream_call\",le=\"0.025\"} 1"));
    assert!(exposition.contains("oagw_request_duration_seconds_sum{host=\"api.vendor.com\""));
    assert!(exposition.contains("oagw_request_duration_seconds_count{host=\"api.vendor.com\""));
}

/// A label value is escaped, and a series with no label is rendered bare.
#[test]
fn the_exposition_escapes_label_values() {
    let registry = MetricsRegistry::new();
    registry.record_error("a\"b\\c", Some("/v1"), "e");
    let exposition = registry.exposition();
    assert!(exposition.contains("host=\"a\\\"b\\\\c\""));
}

/// Consecutive scrapes observe the current values and no snapshot
/// (`inst-os-scrape-6`).
#[test]
fn consecutive_scrapes_observe_current_values() {
    let registry = MetricsRegistry::new();
    assert!(!registry.exposition().contains("oagw_requests_total{"));
    registry.record_request("api.vendor.com", "GET", Some("/v1"), 200);
    assert!(registry.exposition().contains("oagw_requests_total{"));
    registry.record_request("api.vendor.com", "GET", Some("/v1"), 200);
    assert_eq!(
        registry.value(REQUESTS_TOTAL, &[("host", "api.vendor.com"), ("http.response.status_code", "200")]),
        Some(2.0)
    );
}

/// A family that carries no series still opens its HELP/TYPE block.
#[test]
fn an_unpopulated_family_is_declared_without_series() {
    let registry = MetricsRegistry::new();
    let exposition = registry.exposition();
    let block = exposition
        .split("# HELP ")
        .find(|block| block.starts_with(UPSTREAM_AVAILABLE))
        .expect("the family block");
    assert_eq!(block.matches('\n').count(), 2);
}
