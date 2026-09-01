//! Unit tests of the in-process metrics registry.

use super::*;

fn registry() -> MetricsRegistry {
    MetricsRegistry::new()
}

#[test]
fn counters_render_with_type_and_labels() {
    let metrics = registry();
    metrics.record_request("api.example.com", "get", "/api", 200);
    metrics.record_request("api.example.com", "get", "/api", 200);
    metrics.record_request("api.example.com", "POST", "/api", 502);
    let rendered = metrics.render();
    assert!(rendered.contains("# TYPE oagw_requests_total counter"));
    assert!(rendered.contains(
        "oagw_requests_total{host=\"api.example.com\",http.request.method=\"GET\",\
         http.route=\"/api\",http.response.status_code=\"2xx\"} 2"
    ));
    assert!(rendered.contains("http.response.status_code=\"5xx\"} 1"));
}

#[test]
fn error_counter_is_recorded() {
    let metrics = registry();
    metrics.record_error("api.example.com", "/api", "link.unavailable");
    let rendered = metrics.render();
    assert!(rendered.contains(
        "oagw_errors_total{host=\"api.example.com\",http.route=\"/api\",\
         error_type=\"link.unavailable\"} 1"
    ));
}

#[test]
fn unknown_verbs_collapse_into_other() {
    assert_eq!(normalize_method("get"), "GET");
    assert_eq!(normalize_method("PATCH"), "PATCH");
    assert_eq!(normalize_method("PROPFIND"), "_OTHER");
}

#[test]
fn status_classes_are_bounded() {
    assert_eq!(status_class(204), "2xx");
    assert_eq!(status_class(307), "3xx");
    assert_eq!(status_class(404), "4xx");
    assert_eq!(status_class(502), "5xx");
    assert_eq!(status_class(500), "5xx");
}

#[test]
fn gauges_are_overwritten_not_accumulated() {
    let metrics = registry();
    metrics.inc_in_flight("api.example.com");
    metrics.inc_in_flight("api.example.com");
    metrics.dec_in_flight("api.example.com");
    metrics.set_upstream_connections("api.example.com", "idle", 7);
    metrics.set_upstream_available("api.example.com", "https://api.example.com:8443", false);
    let rendered = metrics.render();
    assert!(rendered.contains("# TYPE oagw_requests_in_flight gauge"));
    assert!(rendered.contains("oagw_requests_in_flight{host=\"api.example.com\"} 1"));
    assert!(
        rendered.contains("oagw_upstream_connections{host=\"api.example.com\",state=\"idle\"} 7")
    );
    assert!(rendered.contains(
        "oagw_upstream_available{host=\"api.example.com\",\
         endpoint=\"https://api.example.com:8443\"} 0"
    ));
}

#[test]
fn circuit_breaker_metrics_carry_the_states() {
    let metrics = registry();
    metrics.record_circuit_breaker_transition("api.example.com", "closed", "open");
    metrics.set_circuit_breaker_state("api.example.com", 1);
    let rendered = metrics.render();
    assert!(rendered.contains(
        "oagw_circuit_breaker_transitions_total{host=\"api.example.com\",\
         from_state=\"closed\",to_state=\"open\"} 1"
    ));
    assert!(rendered.contains("oagw_circuit_breaker_state{host=\"api.example.com\"} 1"));
}

#[test]
fn routing_metrics_carry_the_selection() {
    let metrics = registry();
    metrics.record_target_host_used("ups-1", "api.example.com");
    metrics.record_endpoint_selected("ups-1", "api.example.com", "round_robin");
    let rendered = metrics.render();
    assert!(rendered.contains(
        "oagw_routing_target_host_used{upstream_id=\"ups-1\",\
         endpoint_host=\"api.example.com\"} 1"
    ));
    assert!(rendered.contains(
        "oagw_routing_endpoint_selected{upstream_id=\"ups-1\",\
         endpoint_host=\"api.example.com\",selection_method=\"round_robin\"} 1"
    ));
}

#[test]
fn rate_limit_metrics_are_recorded_and_ratio_clamped() {
    let metrics = registry();
    let label = route_label("GET", Some("/api/orders"));
    metrics.record_rate_limit_exceeded("api.example.com", &label);
    metrics.set_rate_limit_usage_ratio("api.example.com", &label, 4.0);
    metrics.set_rate_limit_usage_ratio("api.example.com", &label, -1.0);
    let rendered = metrics.render();
    assert!(rendered.contains(
        "oagw_rate_limit_exceeded_total{host=\"api.example.com\",path=\"GET /api/orders\"} 1"
    ));
    assert!(rendered.contains(
        "oagw_rate_limit_usage_ratio{host=\"api.example.com\",path=\"GET /api/orders\"} 0"
    ));
}

#[test]
fn route_labels_carry_the_configured_pattern() {
    assert_eq!(route_label("GET", Some("/v1/orders")), "GET /v1/orders");
    assert_eq!(route_label("post", Some("/v1/orders")), "POST /v1/orders");
    assert_eq!(route_label("GET", Some("/orders/{id}")), "GET /orders/{id}");
    assert_eq!(route_label("GET", None), UNMATCHED_ROUTE);
    assert_eq!(route_label("", Some("/x")), "_OTHER /x");
}

#[test]
fn twenty_paths_on_one_route_are_one_series() {
    // The caller labels every request of a route with the same [`route_label`],
    // so the number of request suffixes cannot change the series count.
    let metrics = registry();
    let label = route_label("GET", Some("/v1/orders"));
    for index in 0..20_u32 {
        let _ = index; // the request path (`/v1/orders/{index}`) is never recorded
        metrics.record_request("api.example.com", "GET", &label, 200);
    }
    let rendered = metrics.render();
    assert_eq!(
        rendered.matches("http.route=\"GET /v1/orders\"").count(),
        1,
        "one route pattern must stay one series"
    );
    assert!(!rendered.contains("http.route=\"GET /v1/orders/"));
}

#[test]
fn the_unmatched_route_is_a_fixed_literal() {
    let metrics = registry();
    for index in 0..20_u32 {
        metrics.record_error(
            "api.example.com",
            &route_label("GET", None),
            "route.not_matched",
        );
        let _ = index;
    }
    assert_eq!(
        metrics.render().matches("http.route=\"unmatched\"").count(),
        1
    );
}

#[test]
fn histogram_renders_buckets_sum_and_count() {
    let metrics = registry();
    metrics.record_duration("api.example.com", "/api", PHASE_TOTAL, 0.2);
    metrics.record_duration("api.example.com", "/api", PHASE_TOTAL, 0.004);
    let rendered = metrics.render();
    assert!(rendered.contains("# TYPE oagw_request_duration_seconds histogram"));
    assert!(rendered.contains(
        "oagw_request_duration_seconds_bucket{host=\"api.example.com\",http.route=\"/api\",\
         phase=\"total\",le=\"0.001\"} 0"
    ));
    assert!(rendered.contains(
        "oagw_request_duration_seconds_bucket{host=\"api.example.com\",http.route=\"/api\",\
         phase=\"total\",le=\"0.005\"} 1"
    ));
    assert!(rendered.contains(
        "oagw_request_duration_seconds_bucket{host=\"api.example.com\",http.route=\"/api\",\
         phase=\"total\",le=\"0.25\"} 2"
    ));
    assert!(rendered.contains(
        "oagw_request_duration_seconds_bucket{host=\"api.example.com\",http.route=\"/api\",\
         phase=\"total\",le=\"+Inf\"} 2"
    ));
    assert!(rendered.contains(
        "oagw_request_duration_seconds_count{host=\"api.example.com\",http.route=\"/api\",\
         phase=\"total\"} 2"
    ));
}

#[test]
fn buckets_match_the_design_table() {
    assert_eq!(
        DURATION_BUCKETS,
        [
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0
        ]
    );
}

#[test]
fn label_values_are_escaped() {
    let metrics = registry();
    metrics.record_error("host", "/a", "quoted \"value\"\nnext\\path");
    let rendered = metrics.render();
    assert!(rendered.contains("error_type=\"quoted \\\"value\\\"\\nnext\\\\path\""));
}

#[test]
fn empty_registry_renders_nothing() {
    assert_eq!(registry().render(), "");
}

#[test]
fn registry_is_shareable() {
    let metrics = shared();
    metrics.record_request("host", "GET", "/x", 200);
    assert!(metrics.render().contains("oagw_requests_total"));
}

#[test]
fn an_unmatched_request_collapses_onto_fixed_host_and_route_labels() {
    let metrics = registry();
    metrics.record_request(
        host_label(UNMATCHED_ROUTE, "invented.alias"),
        "GET",
        UNMATCHED_ROUTE,
        404,
    );
    metrics.record_error(
        host_label(UNMATCHED_ROUTE, "invented.alias"),
        UNMATCHED_ROUTE,
        "oagw.error.v1",
    );
    let rendered = metrics.render();
    assert!(rendered.contains("host=\"unmatched\""), "{rendered}");
    assert!(!rendered.contains("invented.alias"), "{rendered}");
}

#[test]
fn a_matched_request_keeps_the_configured_alias() {
    let metrics = registry();
    metrics.record_request(
        host_label("GET /v1/", "e2e.svc.internal"),
        "GET",
        "GET /v1/",
        200,
    );
    assert!(metrics.render().contains("host=\"e2e.svc.internal\""));
}

#[test]
fn the_in_flight_gauge_is_bounded_when_aliases_are_invented() {
    let metrics = registry();
    for index in 0..(MAX_IN_FLIGHT_HOSTS + 64) {
        let alias = format!("invented-{index}.alias");
        metrics.inc_in_flight(&alias);
        metrics.dec_in_flight(&alias);
    }
    let rendered = metrics.render();
    let series = rendered
        .lines()
        .filter(|line| line.starts_with(REQUESTS_IN_FLIGHT))
        .count();
    // Every invented name beyond the cap folds onto the one fixed series, so
    // the gauge never grows past `MAX_IN_FLIGHT_HOSTS` + the literal.
    assert!(
        series <= MAX_IN_FLIGHT_HOSTS + 1,
        "{series} in-flight series is over the bound"
    );
    assert!(
        series >= MAX_IN_FLIGHT_HOSTS,
        "{series} series is under the cap"
    );
    assert!(rendered.contains("host=\"unmatched\""), "{rendered}");
}

#[test]
fn the_in_flight_gauge_admits_a_normal_alias_under_its_own_name() {
    // A normal alias is admitted before the cap, on a series of its own: the
    // `unmatched` literal is the fold target, and the alias validator reserves
    // it, so no upstream can ever be confused with it.
    let metrics = registry();
    metrics.inc_in_flight("orders.internal");
    metrics.inc_in_flight("orders.internal");
    metrics.dec_in_flight("orders.internal");
    let rendered = metrics.render();
    let gauge = rendered
        .lines()
        .find(|line| line.starts_with(REQUESTS_IN_FLIGHT) && line.contains("orders.internal"))
        .map(|line| {
            line.rsplit(['}', ' '])
                .next()
                .unwrap_or("0")
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0)
        })
        .unwrap_or_default();
    assert_eq!(gauge, 1.0, "{rendered}");
    assert!(!rendered.contains("host=\"unmatched\""), "{rendered}");
}

#[test]
fn an_in_flight_decrement_lands_on_the_same_series_as_its_increment() {
    let metrics = registry();
    metrics.inc_in_flight("later.alias");
    for index in 0..(MAX_IN_FLIGHT_HOSTS + 16) {
        metrics.inc_in_flight(&format!("filler-{index}.alias"));
    }
    metrics.dec_in_flight("later.alias");
    let gauge = |line: &str| -> f64 {
        line.rsplit(['}', ' '])
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap_or(0.0)
    };
    let admitted = metrics
        .render()
        .lines()
        .find(|line| line.starts_with(REQUESTS_IN_FLIGHT) && line.contains("later.alias"))
        .map(gauge)
        .unwrap_or_default();
    assert_eq!(admitted, 0.0, "the admitted host must return to zero");
}
