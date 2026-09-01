//! Tests for [`crate::domain::audit`].

use std::time::{Duration, SystemTime};

use toolkit_security::SecurityContext;
use uuid::Uuid;

use serde_json::Value;

use super::{
    AUDIT_LEVEL, AUDIT_TARGET, PROXY_EVENT, ProxyExchange, ProxyOutcome, format_rfc3339,
    log_mutation, log_proxy, mutation_event, proxy_event,
};

fn ctx(tenant: Uuid, subject: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

#[test]
fn rfc3339_renders_known_instants() {
    let epoch = format_rfc3339(SystemTime::UNIX_EPOCH);
    assert_eq!(epoch, "1970-01-01T00:00:00.000Z");

    // 2026-02-03T11:09:37.431Z is the ADR-0001 example instant.
    let instant = SystemTime::UNIX_EPOCH + Duration::from_millis(1_770_116_977_431);
    assert_eq!(format_rfc3339(instant), "2026-02-03T11:09:37.431Z");

    // Leap-year day and end-of-year rollover both stay on the proleptic
    // Gregorian calendar.
    let leap = SystemTime::UNIX_EPOCH + Duration::from_millis(1_709_164_800_000);
    assert_eq!(format_rfc3339(leap), "2024-02-29T00:00:00.000Z");
    let nye = SystemTime::UNIX_EPOCH + Duration::from_millis(1_767_225_599_999);
    assert_eq!(format_rfc3339(nye), "2025-12-31T23:59:59.999Z");
}

#[test]
fn mutation_event_carries_the_adr_field_set() {
    let tenant = Uuid::from_u128(0x10);
    let subject = Uuid::from_u128(0x20);
    let payload = mutation_event(
        "upstream.create",
        &ctx(tenant, subject),
        "upstream",
        "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000001",
        None,
        Some("api.openai.com"),
        None,
    );

    assert_eq!(payload["level"], AUDIT_LEVEL);
    assert_eq!(payload["event"], "upstream.create");
    assert_eq!(payload["tenant_id"], tenant.to_string());
    assert_eq!(payload["principal_id"], subject.to_string());
    assert_eq!(payload["resource_type"], "upstream");
    assert_eq!(
        payload["resource_id"],
        "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(payload["alias"], "api.openai.com");
    assert!(payload["request_id"].is_null());
    assert!(payload["detail"].is_null());
    assert!(payload["error_type"].is_null());
    assert!(payload["timestamp"].is_string(), "timestamp is rendered");
}

#[test]
fn mutation_event_is_serialisable_and_target_is_stable() {
    let payload = mutation_event(
        "route.delete",
        &ctx(Uuid::from_u128(0x10), Uuid::from_u128(0x20)),
        "route",
        "gts.cf.core.oagw.route.v1~00000000-0000-0000-0000-000000000002",
        Some("req_1"),
        None,
        Some("cascade from upstream delete"),
    );
    let rendered = serde_json::to_string(&payload).expect("payload is JSON");
    assert!(
        rendered.contains("\"event\":\"route.delete\""),
        "{rendered}"
    );
    assert!(rendered.contains("\"request_id\":\"req_1\""), "{rendered}");
    assert!(rendered.contains("\"detail\":\"cascade from upstream delete\""));
    assert_eq!(AUDIT_TARGET, "oagw.audit");
}

#[test]
fn log_mutation_does_not_panic() {
    log_mutation(
        "plugin.create",
        &ctx(Uuid::from_u128(0x10), Uuid::from_u128(0x20)),
        "plugin",
        "gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000003",
        None,
        None,
        None,
    );
}

/// A synthetic proxied exchange, fully populated.
fn proxied_exchange(outcome: ProxyOutcome) -> ProxyExchange {
    ProxyExchange {
        request_id: Some("req_proxy_1".to_owned()),
        tenant_id: Some(Uuid::from_u128(0x10).to_string()),
        principal_id: Some(Uuid::from_u128(0x20).to_string()),
        alias: "api.openai.com".to_owned(),
        host: "api.openai.com".to_owned(),
        route: "GET /v1/orders".to_owned(),
        endpoint: "https://10.0.0.7:8443".to_owned(),
        method: "GET".to_owned(),
        outbound_path: "/v1/orders/42".to_owned(),
        status: 200,
        duration_ms: 12,
        request_size: 0,
        response_size: 2048,
        outcome,
        error_type: None,
        error_message: None,
    }
}

#[test]
fn proxy_event_carries_the_design_field_set() {
    let payload = proxy_event(&proxied_exchange(ProxyOutcome::Upstream));
    // DESIGN §4.3: every field of the request record is present and non-empty.
    for field in [
        "timestamp",
        "level",
        "event",
        "request_id",
        "tenant_id",
        "principal_id",
        "host",
        "path",
        "method",
        "status",
        "duration_ms",
        "request_size",
        "response_size",
    ] {
        let value = &payload[field];
        assert!(
            !value.is_null(),
            "field '{field}' is missing from the record"
        );
        match value {
            Value::String(text) => assert!(!text.is_empty(), "field '{field}' is empty"),
            Value::Number(number) => {
                assert!(number.as_u64().is_some(), "field '{field}' is not integral")
            }
            other => panic!("field '{field}' is neither a string nor a number: {other}"),
        }
    }
    // The gateway-side fields the design names per proxy concern.
    assert_eq!(payload["event"], PROXY_EVENT);
    assert_eq!(payload["alias"], "api.openai.com");
    assert_eq!(payload["route"], "GET /v1/orders");
    assert_eq!(payload["endpoint"], "https://10.0.0.7:8443");
    assert_eq!(payload["outcome"], "upstream");
    assert!(payload["error_type"].is_null());
    assert!(payload["error_message"].is_null());
}

#[test]
fn proxy_event_reports_gateway_failures() {
    let mut exchange = proxied_exchange(ProxyOutcome::Gateway);
    exchange.status = 429;
    exchange.error_type =
        Some("gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1".to_owned());
    exchange.error_message = Some("the rate budget of this upstream is exhausted".to_owned());
    let payload = proxy_event(&exchange);
    assert_eq!(payload["status"], 429);
    assert_eq!(payload["outcome"], "gateway");
    assert_eq!(
        payload["error_type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(
        payload["error_message"],
        "the rate budget of this upstream is exhausted"
    );
    assert!(serde_json::to_string(&payload).is_ok(), "payload is JSON");
}

#[test]
fn proxy_levels_follow_the_design_table() {
    // An upstream answer is a normal operation unless the upstream failed.
    assert_eq!(proxied_exchange(ProxyOutcome::Upstream).level(), "INFO");
    let mut upstream_client_error = proxied_exchange(ProxyOutcome::Upstream);
    upstream_client_error.status = 404;
    assert_eq!(upstream_client_error.level(), "INFO");
    let mut upstream_redirect = proxied_exchange(ProxyOutcome::Upstream);
    upstream_redirect.status = 302;
    assert_eq!(upstream_redirect.level(), "INFO");
    // §4.3 "ERROR": upstream failures, timeouts.
    let mut upstream_error = proxied_exchange(ProxyOutcome::Upstream);
    upstream_error.status = 503;
    assert_eq!(upstream_error.level(), "ERROR");
    // Gateway answers are graded by cause (§4.3 "Log Levels").
    let mut auth_failure = proxied_exchange(ProxyOutcome::Gateway);
    auth_failure.status = 401;
    assert_eq!(auth_failure.level(), "ERROR");
    let mut rate_limited = proxied_exchange(ProxyOutcome::Gateway);
    rate_limited.status = 429;
    assert_eq!(rate_limited.level(), "WARN");
    let mut upstream_unreachable = proxied_exchange(ProxyOutcome::Gateway);
    upstream_unreachable.status = 502;
    assert_eq!(upstream_unreachable.level(), "ERROR");
    let mut no_route = proxied_exchange(ProxyOutcome::Gateway);
    no_route.status = 404;
    assert_eq!(no_route.level(), "INFO");
}

#[test]
fn log_proxy_does_not_panic() {
    log_proxy(&proxied_exchange(ProxyOutcome::Gateway));
    log_proxy(&proxied_exchange(ProxyOutcome::Upstream));
}
