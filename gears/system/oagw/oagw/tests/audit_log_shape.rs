//! Integration tests of the audit-record shape the real surfaces emit
//! (`cpt-cf-oagw-dod-observability-and-state-audit-log`,
//! `cpt-cf-oagw-flow-observability-and-state-audit-record`,
//! `cpt-cf-oagw-flow-observability-and-state-trace-identifiers`).
//!
//! The tests drive the real `OagwGear` over a captured audit sink, so every
//! assertion reads the line a real proxy exchange or a real management write
//! produced rather than a record a fixture built.
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-audit-log:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-trace-propagation:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::domain::dto::{EndpointScheme, HttpMethod};
use oagw::test_support::{
    audited_surface, route_for, seed_route, seed_upstream, security_context, stub_upstream,
    upstream_at,
};

const PROXY: &str = "/oagw/v1/proxy";
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// The `oagw` block the proxy tests need: `http` upstreams admitted.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The records of the sink as parsed JSON objects.
fn documents(audit: &oagw::infra::audit::AuditSink) -> Vec<Value> {
    audit.lines().iter().map(|line| serde_json::from_str(line).expect("one JSON object")).collect()
}

/// The surface with one upstream and one route matching `/v1` over a stub
/// upstream.
async fn seeded(
    surface: &oagw::test_support::ManagementSurface,
) -> (oagw::test_support::StubUpstream, Uuid, Uuid) {
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(surface, upstream);
    let mut route = route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get, HttpMethod::Post]);
    // A query allowlist, so the exchange below carries a query string the
    // gateway forwards rather than rejects.
    route.match_.http.as_mut().expect("http").query_allowlist = vec!["token".to_owned()];
    seed_route(surface, route);
    (stub, tenant, upstream_id)
}

/// One proxied exchange whose request carries a body, a query and a credential
/// header, so every line the exchange emits can be checked against them.
async fn exchanged_with_secrets(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
) -> oagw::test_support::ProxyExchange {
    surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "POST",
            &format!("{PROXY}/api.vendor.com/v1/orders?token=super-secret-token"),
            &[
                ("content-type", "application/json"),
                ("content-length", "25"),
                ("authorization", "Bearer super-secret-bearer"),
                ("x-api-key", "super-secret-api-key"),
            ],
            b"{\"card\":\"4242-4242-4242\"}",
        )
        .await
}

/// A proxied exchange emits exactly one unsampled record with the fourteen
/// declared fields, the correlation identifier and no `error_message`
/// (`inst-os-audit-1`, `inst-os-audit-2`, `inst-os-algo-audit-4`).
#[tokio::test]
async fn a_proxied_exchange_emits_one_fourteen_field_record() {
    let (surface, audit) = audited_surface(proxy_config()).await;
    let (_stub, tenant, _upstream_id) = seeded(&surface).await;
    audit.set_sample_rate(1_000);

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());

    let lines = audit.lines();
    assert_eq!(lines.len(), 1, "the proxy class is never sampled away: {lines:?}");
    let document: Value = serde_json::from_str(&lines[0]).expect("one JSON object");
    let object = document.as_object().expect("one JSON object");
    for field in [
        "timestamp", "level", "event", "request_id", "tenant_id", "principal_id", "host", "path",
        "method", "status", "duration_ms", "request_size", "response_size", "error_type",
    ] {
        assert!(object.contains_key(field), "`{field}` is carried: {document}");
    }
    assert_eq!(object.len(), 14, "the field set is exactly fourteen: {document}");
    assert_eq!(document["event"], "proxy_request");
    assert_eq!(document["host"], "api.vendor.com");
    assert_eq!(document["method"], "GET");
    assert_eq!(document["status"], 200, "the numeric fields are JSON numbers");
    assert_eq!(document["tenant_id"], tenant.to_string());
    assert!(
        document["request_id"].as_str().is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "the minted identifier: {document}"
    );
}

/// No emitted line carries a request body, a query string or any credential
/// material, whatever the record class
/// (`cpt-cf-oagw-dod-observability-and-state-audit-log`).
#[tokio::test]
async fn no_line_carries_a_body_a_query_or_credential_material() {
    let (surface, audit) = audited_surface(proxy_config()).await;
    let (_stub, tenant, _upstream_id) = seeded(&surface).await;

    let exchange = exchanged_with_secrets(&surface, tenant).await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    // A rejected write of another class, so more than one record class is on
    // the wire before the assertion.
    let rejected = surface.proxy("GET", "/metrics", &[], b"", None).await;
    assert_eq!(rejected.status, http::StatusCode::UNAUTHORIZED);

    let lines = audit.lines();
    assert!(!lines.is_empty(), "records were emitted");
    for line in &lines {
        for secret in ["4242-4242-4242", "super-secret-bearer", "super-secret-api-key", "token=super-secret-token"] {
            assert!(!line.contains(secret), "the line carries secret material `{secret}`: {line}");
        }
        let document: Value = serde_json::from_str(line.as_str()).expect("one JSON object");
        assert!(document.get("error_message").is_none(), "no free-form message: {document}");
        assert!(document.get("body").is_none(), "no body field: {document}");
        assert!(document.get("headers").is_none(), "no header field: {document}");
    }
}

/// The `trace_id` a rendered problem body carries is the `request_id` of the
/// audit record of the same exchange
/// (`cpt-cf-oagw-flow-observability-and-state-trace-identifiers`).
#[tokio::test]
async fn the_problem_trace_id_is_the_audit_request_id() {
    let (surface, audit) = audited_surface(proxy_config()).await;
    let (_stub, tenant, _upstream_id) = seeded(&surface).await;

    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            &format!("{PROXY}/api.vendor.com/v9"),
            &[("x-request-id", "trace-42")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND, "{}", exchange.text());
    let document: Value = serde_json::from_slice(&exchange.body).expect("problem+json");
    assert_eq!(document["trace_id"], "trace-42", "the carried identifier is echoed");

    let lines = audit.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let record: Value = serde_json::from_str(&lines[0]).expect("one JSON object");
    assert_eq!(record["request_id"], "trace-42", "the same identifier the body carries");
    assert_eq!(record["status"], 404);
    assert_eq!(record["event"], "proxy_request");
}

/// A request the authentication surface rejected emits an `auth_failure`
/// record carrying the rejected request's context
/// (`inst-os-algo-audit-2c`).
#[tokio::test]
async fn a_rejected_request_is_an_auth_failure_record() {
    let (surface, audit) = audited_surface(proxy_config()).await;
    let (_stub, _tenant, _upstream_id) = seeded(&surface).await;

    let exchange = surface.proxy("POST", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"", None).await;
    assert_eq!(exchange.status, http::StatusCode::UNAUTHORIZED);

    let documents = documents(&audit);
    assert_eq!(documents.len(), 1, "{documents:?}");
    let record = &documents[0];
    assert_eq!(record["event"], "auth_failure");
    assert_eq!(record["level"], "ERROR");
    assert_eq!(record["status"], 401);
    assert_eq!(record["host"], "api.vendor.com");
    assert_eq!(record["duration_ms"], Value::Null, "the class leaves the durations null");
    assert_eq!(record["request_size"], Value::Null);
    assert_eq!(record["response_size"], Value::Null);
}

/// An accepted management write emits one `config_change` record whose `path`
/// is the management resource path of the affected record
/// (`inst-os-algo-audit-2b`).
#[tokio::test]
async fn a_management_write_is_a_config_change_record() {
    let (surface, audit) = audited_surface(None).await;
    let tenant = Uuid::new_v4();

    let (status, created) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": "api.vendor.com" } ] } }),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created:?}");
    let created: Value = serde_json::from_slice(&created).expect("the created record");
    let id: Uuid = serde_json::from_value(created["id"].clone()).expect("the identifier");

    let lines = audit.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let record: Value = serde_json::from_str(&lines[0]).expect("one JSON object");
    assert_eq!(record["event"], "config_change");
    assert_eq!(record["level"], "INFO");
    assert_eq!(record["status"], 201, "the status is the one the write produced");
    assert_eq!(record["path"], format!("/oagw/v1/upstreams/{id}"), "the resource path");
    assert_eq!(record["host"], Value::Null, "the class leaves the request fields null");
    assert_eq!(record["method"], Value::Null);
    assert_eq!(record["duration_ms"], Value::Null);
    assert_eq!(record["tenant_id"], tenant.to_string());
}

/// A `config_change` record for a management write the gear emitted is sampled
/// by the build-time policy, so a rate of `1` emits every one of them
/// (`inst-os-audit-5`).
#[tokio::test]
async fn the_sampling_policy_applies_to_the_management_class() {
    let (surface, audit) = audited_surface(None).await;
    let tenant = Uuid::new_v4();
    audit.set_sample_rate(2);

    for index in 0..4 {
        let (status, body) = surface
            .create(
                tenant,
                Uuid::new_v4(),
                json!({ "protocol": PROTOCOL_HTTP, "server": { "endpoints": [ { "host": format!("host-{index}.vendor.com") } ] } }),
            )
            .await;
        assert_eq!(status, http::StatusCode::CREATED, "{body:?}");
    }
    assert_eq!(audit.lines().len(), 2, "the policy sampled every other one");
}

/// The audit record a proxied exchange emits is attributed to the caller the
/// platform edge resolved, with no tenant label on the metric families
/// (`inst-os-algo-label-5`).
#[tokio::test]
async fn the_audit_record_carries_the_caller_context() {
    let (surface, audit) = audited_surface(proxy_config()).await;
    let (_stub, tenant, _upstream_id) = seeded(&surface).await;
    let principal = Uuid::new_v4();

    let exchange = surface
        .proxy_for(tenant, principal, "GET", &format!("{PROXY}/api.vendor.com/v1/orders"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);

    let documents = documents(&audit);
    assert_eq!(documents.len(), 1, "{documents:?}");
    assert_eq!(documents[0]["tenant_id"], tenant.to_string());
    assert_eq!(documents[0]["principal_id"], principal.to_string());
    assert_eq!(
        surface
            .proxy("GET", "/metrics", &[], b"", Some(security_context(tenant, principal)))
            .await
            .status,
        http::StatusCode::OK
    );
    let exposition = surface
        .proxy("GET", "/metrics", &[], b"", Some(security_context(tenant, principal)))
        .await
        .text();
    assert!(!exposition.contains(&tenant.to_string()), "no tenant label on a series: {exposition}");
}
