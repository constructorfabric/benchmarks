#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tests for the hand-rendered Prometheus registry (`infra::metrics`).

use std::sync::Arc;

use super::{
    is_unit_ratio, DURATION_BUCKETS, ERRORS_TOTAL, RATE_LIMIT_EXCEEDED_TOTAL,
    RATE_LIMIT_USAGE_RATIO, REQUESTS_IN_FLIGHT, REQUESTS_TOTAL, REQUEST_DURATION,
    ROUTING_ENDPOINT_SELECTED,
};

/// Counts the occurrences of `needle` in `text`.
fn occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

#[test]
fn a_request_is_rendered_with_otel_labels() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_request("svc.local", "GET", "/v1/things", 200);

    let rendered = metrics.render();
    assert!(
        rendered.contains(
            &format!("{REQUESTS_TOTAL}{{host=\"svc.local\",http_request_method=\"GET\",http_route=\"/v1/things\",http_response_status_code=\"200\"}} 1")
        ),
        "unexpected exposition:\n{rendered}"
    );
    assert!(rendered.contains(&format!("# TYPE {REQUESTS_TOTAL} counter")));
}

#[test]
fn a_method_that_is_not_a_standard_verb_is_normalised() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_request("svc.local", "PROPFIND", "/v1/things", 207);

    let rendered = metrics.render();
    assert!(
        rendered.contains("http_request_method=\"PROPFIND\""),
        "unexpected exposition:\n{rendered}"
    );
}

#[test]
fn an_in_flight_gauge_is_always_present_and_updatable() {
    let metrics = super::MetricsRegistry::new();
    metrics.begin_request();
    metrics.begin_request();
    assert!(
        metrics.render().contains(&format!("{REQUESTS_IN_FLIGHT} 2")),
        "unexpected exposition:\n{}",
        metrics.render()
    );

    metrics.end_request();
    assert!(
        metrics.render().contains(&format!("{REQUESTS_IN_FLIGHT} 1")),
        "unexpected exposition:\n{}",
        metrics.render()
    );

    metrics.end_request();
    metrics.end_request();
    assert!(
        metrics.render().contains(&format!("{REQUESTS_IN_FLIGHT} 0")),
        "unexpected exposition:\n{}",
        metrics.render()
    );
}

#[test]
fn a_duration_is_bucketed_cumulatively_with_sum_and_count() {
    let metrics = super::MetricsRegistry::new();
    metrics.observe_duration("svc.local", "/v1/things", "upstream", 0.2);
    metrics.observe_duration("svc.local", "/v1/things", "upstream", 0.02);

    let rendered = metrics.render();
    assert!(
        rendered.contains(&format!("# TYPE {REQUEST_DURATION} histogram")),
        "unexpected exposition:\n{rendered}"
    );
    let bucket = format!(
        "{REQUEST_DURATION}_bucket{{host=\"svc.local\",http_route=\"/v1/things\",phase=\"upstream\",le=\"0.25\"}} 2"
    );
    assert!(
        rendered.contains(&bucket),
        "missing cumulative bucket {bucket}:\n{rendered}"
    );
    let slow_bucket = format!(
        "{REQUEST_DURATION}_bucket{{host=\"svc.local\",http_route=\"/v1/things\",phase=\"upstream\",le=\"0.1\"}} 1"
    );
    assert!(
        rendered.contains(&slow_bucket),
        "missing cumulative bucket {slow_bucket}:\n{rendered}"
    );
    assert!(
        rendered.contains(&format!("{REQUEST_DURATION}_count{{host=\"svc.local\",http_route=\"/v1/things\",phase=\"upstream\"}} 2")),
        "unexpected exposition:\n{rendered}"
    );
    assert!(
        rendered.contains(&format!("{REQUEST_DURATION}_sum{{host=\"svc.local\",http_route=\"/v1/things\",phase=\"upstream\"}}")),
        "unexpected exposition:\n{rendered}"
    );
}

#[test]
fn every_configured_bucket_is_rendered() {
    let metrics = super::MetricsRegistry::new();
    metrics.observe_duration("svc.local", "/v1/things", "total", 42.0);

    let rendered = metrics.render();
    for bound in DURATION_BUCKETS {
        let label = format!("le=\"{bound}\"");
        assert!(
            rendered.contains(&label),
            "missing bucket label {label}:\n{rendered}"
        );
    }
}

#[test]
fn an_error_is_tagged_by_type_without_a_tenant_label() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_error("svc.local", "/v1/things", "RouteNotFound");

    let rendered = metrics.render();
    assert!(
        rendered.contains(
            &format!("{ERRORS_TOTAL}{{host=\"svc.local\",http_route=\"/v1/things\",error_type=\"RouteNotFound\"}} 1")
        ),
        "unexpected exposition:\n{rendered}"
    );
    assert!(
        !rendered.contains("tenant"),
        "a tenant label leaked into the exposition:\n{rendered}"
    );
}

#[test]
fn endpoint_selections_are_recorded_per_method() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_endpoint_selection("up-1", "https://a.internal", "round_robin");

    let rendered = metrics.render();
    assert!(
        rendered.contains(
            &format!("{ROUTING_ENDPOINT_SELECTED}{{upstream_id=\"up-1\",endpoint_host=\"https://a.internal\",selection_method=\"round_robin\"}} 1")
        ),
        "unexpected exposition:\n{rendered}"
    );
}

#[test]
fn a_rate_limit_rejection_and_its_quota_are_rendered() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_rate_limit_exceeded("svc.local", "/v1/things");
    metrics.record_rate_limit_exceeded("svc.local", "/v1/things");
    metrics.record_rate_usage("svc.local", "/v1/things", 0.5);

    let rendered = metrics.render();
    assert!(
        rendered.contains(
            &format!("{RATE_LIMIT_EXCEEDED_TOTAL}{{host=\"svc.local\",path=\"/v1/things\"}} 2")
        ),
        "unexpected exposition:\n{rendered}"
    );
    assert!(
        rendered.contains(
            &format!("{RATE_LIMIT_USAGE_RATIO}{{host=\"svc.local\",path=\"/v1/things\"}} 0.5")
        ),
        "unexpected exposition:\n{rendered}"
    );
}

#[test]
fn a_quota_ratio_is_clamped_into_the_unit_interval() {
    let metrics = super::MetricsRegistry::new();
    metrics.record_rate_usage("svc.local", "/v1/things", 7.5);
    metrics.record_rate_usage("svc.local", "/v1/other", -3.0);

    let rendered = metrics.render();
    assert!(
        rendered.contains("path=\"/v1/things\"} 1.0"),
        "unexpected exposition:\n{rendered}"
    );
    assert!(
        rendered.contains("path=\"/v1/other\"} 0.0"),
        "unexpected exposition:\n{rendered}"
    );
    assert!(is_unit_ratio(1.0) && is_unit_ratio(0.0));
    assert!(!is_unit_ratio(1.5));
}

#[test]
fn an_empty_registry_omits_every_series_except_the_in_flight_gauge() {
    let metrics = super::MetricsRegistry::new();
    let rendered = metrics.render();
    assert_eq!(
        rendered,
        format!(
            "# HELP {REQUESTS_IN_FLIGHT} Requests currently being proxied\n# TYPE {REQUESTS_IN_FLIGHT} gauge\n{REQUESTS_IN_FLIGHT} 0\n"
        ),
        "unexpected exposition:\n{rendered}"
    );
}

#[test]
fn the_registry_is_sharable_across_threads() {
    let metrics = Arc::new(super::MetricsRegistry::new());
    let handles: Vec<_> = (0..8)
        .map(|index| {
            let metrics = Arc::clone(&metrics);
            std::thread::spawn(move || {
                metrics.begin_request();
                metrics.record_request("svc.local", "GET", "/v1/things", 200 + index);
                metrics.observe_duration("svc.local", "/v1/things", "total", 0.1);
                metrics.end_request();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(
        occurrences(&metrics.render(), &format!("{REQUESTS_TOTAL}{{host=\"svc.local\"")) == 8,
        "unexpected exposition:\n{}",
        metrics.render()
    );
}

#[test]
fn a_request_duration_below_the_finest_bucket_lands_in_the_first_bucket() {
    let metrics = super::MetricsRegistry::new();
    metrics.observe_duration("svc.local", "/v1/things", "upstream", 0.0001);

    let rendered = metrics.render();
    assert!(
        rendered.contains(&format!(
            "{REQUEST_DURATION}_bucket{{host=\"svc.local\",http_route=\"/v1/things\",phase=\"upstream\",le=\"0.001\"}} 1"
        )),
        "unexpected exposition:\n{rendered}"
    );
}
