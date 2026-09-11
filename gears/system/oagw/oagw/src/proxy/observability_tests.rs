//! Tests for the per-request observability: the log record and the counters.

use std::time::Duration;

use crate::proxy::observability::{
    normalized_method, redact, Metrics, RequestRecord, DURATION_BUCKETS, ERRORS_TOTAL, REDACTED,
    REQUESTS_TOTAL, REQUEST_DURATION_SECONDS,
};

fn record() -> RequestRecord {
    RequestRecord {
        request_id: "req-1".to_owned(),
        tenant_id: "0192f0a0-0000-7000-8000-000000000000".to_owned(),
        host: "payments.example.com".to_owned(),
        route: "/v1/charge".to_owned(),
        method: "POST".to_owned(),
        status: 200,
        duration: Duration::from_millis(12),
        error_type: None,
    }
}

#[test]
fn a_record_names_the_request_and_not_the_body() {
    let line = record().log_line();
    for field in [
        "event=oagw.proxy.request",
        "request_id=req-1",
        "host=payments.example.com",
        "http.route=/v1/charge",
        "method=POST",
        "status=200",
        "duration_ms=12",
    ] {
        assert!(line.contains(field), "`{field}` missing from: {line}");
    }
    // The field set is closed: there is nowhere a body, a query string, a header value or
    // a credential to land.
    assert!(!line.contains("query"));
    assert!(!line.contains("authorization"));
    assert!(!line.contains("body"));
}

#[test]
fn a_rejected_request_names_its_error_type() {
    let mut record = record();
    record.status = 429;
    record.error_type = Some("cf.core.errors.err.v1~cf.oagw.rate_limit".to_owned());
    let line = record.log_line();
    assert!(
        line.contains("error_type=cf.core.errors.err.v1~cf.oagw.rate_limit"),
        "{line}"
    );
    assert!(record.is_error());
}

#[test]
fn a_server_status_is_treated_as_an_error_without_a_kind() {
    let mut record = record();
    record.status = 502;
    record.error_type = None;
    assert!(record.is_error());
    record.status = 200;
    assert!(!record.is_error());
}

#[test]
fn a_record_with_an_error_kind_is_an_error_at_any_status() {
    let mut record = record();
    record.error_type = Some("cf.core.errors.err.v1~cf.oagw.validation".to_owned());
    assert!(record.is_error());
}

#[test]
fn label_values_are_quoted_so_one_cannot_forge_another() {
    let metrics = Metrics::new();
    let mut record = record();
    // A caller-controlled alias cannot inject a second label into the exposition.
    record.host = "evil\" , oagw_injected=\"1".to_owned();
    metrics.record(&record);
    let rendered = metrics.render();
    assert!(
        rendered.contains("oagw_injected=\\\"1"),
        "the injected label was not escaped: {rendered}"
    );
    assert!(!rendered.contains("oagw_injected=\"1,"), "{rendered}");
}

#[test]
fn counters_increment_by_host_and_route_pattern() {
    let metrics = Metrics::new();
    metrics.record(&record());
    metrics.record(&record());
    let counters = metrics.counters();
    let hits = counters
        .iter()
        .filter(|(name, _)| name.starts_with(REQUESTS_TOTAL))
        .map(|(_, value)| *value)
        .sum::<u64>();
    assert_eq!(hits, 2, "{counters:?}");

    // The labels are the alias and the route pattern, never the raw request path.
    assert!(
        counters
            .iter()
            .all(|(name, _)| !name.contains("/v1/charge/x")),
        "{counters:?}"
    );
}

#[test]
fn the_method_is_normalized_for_the_label() {
    assert_eq!(normalized_method("get"), "GET");
    assert_eq!(normalized_method("POST"), "POST");
    assert_eq!(normalized_method("bogus"), "_OTHER");
}

#[test]
fn the_duration_histogram_uses_the_documented_buckets() {
    let documented = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
    for (actual, expected) in DURATION_BUCKETS.iter().zip(documented) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }
    let metrics = Metrics::new();
    metrics.record(&record());
    let histograms = metrics.histograms();
    assert_eq!(histograms.len(), 1, "{histograms:?}");
    let (name, buckets, _sum) = &histograms[0];
    assert!(name.starts_with(REQUEST_DURATION_SECONDS), "{name}");
    assert_eq!(buckets[DURATION_BUCKETS.len()], 1, "one observation total");
    // 12 ms falls into the 0.025 s bucket, so every cumulative bucket from there on is 1.
    let index = DURATION_BUCKETS
        .iter()
        .position(|bound| *bound >= 0.025)
        .expect("the bucket exists");
    assert_eq!(buckets[index], 1);
    assert_eq!(buckets[index - 1], 0, "the smaller buckets stay empty");
}

#[test]
fn an_error_is_counted_by_its_kind() {
    let metrics = Metrics::new();
    metrics.record_error("payments.example.com", "/v1/charge", "cf.oagw.route.not_found");
    let counters = metrics.counters();
    let hits = counters
        .iter()
        .filter(|(name, _)| name.starts_with(ERRORS_TOTAL))
        .count();
    assert_eq!(hits, 1, "{counters:?}");
}

#[test]
fn a_rate_limited_request_is_counted_separately() {
    let metrics = Metrics::new();
    metrics.record_rate_limited("payments.example.com", "/v1/charge");
    let rendered = metrics.render();
    assert!(
        rendered.contains("oagw_proxy_rate_limit_exceeded_total"),
        "{rendered}"
    );
}

#[test]
fn the_registry_renders_a_prometheus_document() {
    let metrics = Metrics::new();
    metrics.record(&record());
    let rendered = metrics.render();
    assert!(rendered.contains("# TYPE oagw_proxy_requests_total counter"), "{rendered}");
    assert!(
        rendered.contains("# TYPE oagw_proxy_request_duration_seconds histogram"),
        "{rendered}"
    );
    assert!(rendered.contains("le=\"0.001\""), "{rendered}");
    assert!(rendered.contains("le=\"+Inf\""), "{rendered}");
    assert!(rendered.contains("_sum"), "{rendered}");
    assert!(rendered.contains("_count"), "{rendered}");
}

#[test]
fn an_empty_registry_renders_nothing() {
    assert_eq!(Metrics::new().render(), "");
    assert!(Metrics::new().counters().is_empty());
    assert!(Metrics::new().histograms().is_empty());
}

/// TR-10: no secret material reaches a log line, an error detail or a response body.
#[test]
fn a_resolved_credential_is_scrubbed_from_text() {
    let secret = "sk_live_51H8xY2eZvKYlo2C9f";
    let text = format!("upstream returned 401 for token {secret}");
    assert_eq!(redact(&text, &[secret.to_owned()]), "upstream returned 401 for token [redacted]");
    assert!(!redact(&text, &[secret.to_owned()]).contains(secret));
}

#[test]
fn an_empty_secret_is_never_substituted() {
    // An empty pattern would replace every character boundary.
    assert_eq!(redact("unchanged", &[String::new()]), "unchanged");
    assert_eq!(redact("unchanged", &[]), "unchanged");
}

#[test]
fn the_redaction_placeholder_is_named() {
    assert_eq!(REDACTED, "[redacted]");
}

#[test]
fn a_record_built_from_a_request_cannot_carry_a_credential() {
    // The record has no field a credential could be assigned to; this is the
    // by-construction isolation the design requires.
    let record = record();
    let rendered = format!("{record:?}");
    assert!(!rendered.contains("credential"));
    assert!(!rendered.contains("authorization"));
}

/// US8/AC1: the record carries the correlation identifier the gateway settled on, which
/// the `request_id` transform generates when the caller sent none — so a request that
/// arrives without one is still logged with a traceable identifier.
#[test]
fn a_generated_request_id_is_recorded_not_dropped() {
    use http::HeaderMap;
    use crate::proxy::correlation_id;

    let inbound = HeaderMap::new();
    let mut outbound = HeaderMap::new();
    outbound.insert(
        crate::error::REQUEST_ID_HEADER,
        http::HeaderValue::from_static("req_6f9c0c0f2d0c4f4d9b0e"),
    );
    assert_eq!(correlation_id(&inbound, &outbound), "req_6f9c0c0f2d0c4f4d9b0e");
}

/// A caller who did supply one keeps it, and the gateway does not invent a second value.
#[test]
fn a_caller_supplied_request_id_is_kept() {
    use http::HeaderMap;
    use crate::proxy::correlation_id;

    let mut inbound = HeaderMap::new();
    inbound.insert(
        crate::error::REQUEST_ID_HEADER,
        http::HeaderValue::from_static("from-caller"),
    );
    let outbound = HeaderMap::new();
    assert_eq!(correlation_id(&inbound, &outbound), "from-caller");

    let mut outbound = inbound.clone();
    outbound.insert(
        crate::error::REQUEST_ID_HEADER,
        http::HeaderValue::from_static("propagated"),
    );
    assert_eq!(correlation_id(&inbound, &outbound), "propagated");
}

/// Nothing is invented when no plugin generated an identifier and the caller sent none.
#[test]
fn an_absent_request_id_is_recorded_as_absent() {
    use http::HeaderMap;
    use crate::proxy::correlation_id;

    assert_eq!(correlation_id(&HeaderMap::new(), &HeaderMap::new()), "");
}
