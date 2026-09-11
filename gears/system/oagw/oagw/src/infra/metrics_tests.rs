//! Metric label normalization (cardinality control).

use super::*;

#[test]
fn standard_verbs_pass_through_and_others_collapse() {
    assert_eq!(normalize_method("get"), "GET");
    assert_eq!(normalize_method("POST"), "POST");
    assert_eq!(normalize_method("PATCH"), "PATCH");
    assert_eq!(normalize_method("PROPFIND"), "_OTHER");
    assert_eq!(normalize_method(""), "_OTHER");
}

#[test]
fn selection_methods_render_as_the_documented_labels() {
    assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
    assert_eq!(SelectionMethod::RoundRobin.as_str(), "round_robin");
    assert_eq!(SelectionMethod::Default.as_str(), "default");
}

#[test]
fn the_instrument_set_builds_against_the_global_meter() {
    // A no-op meter provider is installed by default, so this exercises the
    // instrument construction without needing an exporter.
    let metrics = OagwMetrics::from_global();
    metrics.record_request("api.openai.com", "GET", "/v1/chat", 200, 0.01);
    metrics.record_phase("api.openai.com", "/v1/chat", "upstream", 0.005);
    metrics.set_in_flight("api.openai.com", 1);
    metrics.record_error("api.openai.com", "/v1/chat", "timeout");
    metrics.record_rate_limit("api.openai.com", "/v1/chat", true, 1.0);
    metrics.record_endpoint_selection("id", "us.vendor.com", SelectionMethod::RoundRobin);
}
