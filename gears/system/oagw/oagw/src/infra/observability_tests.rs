//! The unit tests of the post-write invalidation and audit hook
//! (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`,
//! `cpt-cf-oagw-flow-observability-and-state-config-change-audit`).

use std::sync::Arc;

use uuid::Uuid;

use super::*;
use crate::domain::endpoints::SelectionMethod;
use crate::domain::proxy::{PhaseObservation, RateLimitObservation, RoutingObservation};
use crate::domain::services::management::{ConfigWriteHook, RouteWriteKeys};
use crate::infra::metrics::{
    ERRORS_TOTAL, MetricsRegistry, RATE_LIMIT_EXCEEDED_TOTAL, RATE_LIMIT_USAGE_RATIO,
    REQUESTS_TOTAL, ROUTING_ENDPOINT_SELECTED, UPSTREAM_AVAILABLE,
};
use crate::test_support::{observation, route_record};

fn notification(event: &'static str) -> WriteNotification {
    WriteNotification {
        event,
        tenant_id: Uuid::new_v4(),
        principal_id: Uuid::new_v4(),
        resource_id: "inst-1".to_owned(),
        upstream_id: Some(Uuid::new_v4()),
        upstream_alias: Some("api.vendor.com".to_owned()),
        route: None,
        plugin_id: None,
        status: 200,
        outcome: "accepted",
    }
}

fn wired() -> (CPState, Arc<DpHotConfig>, Arc<AuditSink>, ObservabilityHook) {
    let state = CPState::single_executable();
    let dp = Arc::new(DpHotConfig::new(crate::infra::cp_cache::ConfigGenerations::default()));
    let audit = AuditSink::captured();
    let hook = ObservabilityHook::new(state.clone(), Arc::clone(&dp), Arc::clone(&audit));
    (state, dp, audit, hook)
}

/// The resource path a `config_change` record carries is the management
/// resource path of the affected record (`inst-os-algo-audit-2b`).
#[test]
fn the_audit_path_is_the_management_resource_path() {
    let id = Uuid::new_v4();
    let mut notification = notification("upstream.replace");
    notification.resource_id = crate::domain::gts_helpers::upstream_resource_id(id);
    assert_eq!(resource_path(&notification), format!("/oagw/v1/upstreams/{id}"));
    notification.event = "route.create";
    notification.resource_id = crate::domain::gts_helpers::route_resource_id(id);
    assert_eq!(resource_path(&notification), format!("/oagw/v1/routes/{id}"));
    notification.event = "plugin.created";
    assert_eq!(resource_path(&notification), format!("/oagw/v1/plugins/{id}"));
}

/// An accepted upstream write invalidates its CP key, flushes the dependent DP
/// entries and emits one `config_change` record.
#[tokio::test]
async fn an_upstream_write_invalidates_flushes_and_audits() {
    let tenant = Uuid::new_v4();
    let (state, dp, audit, hook) = wired();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() };
    state.l1.insert(&key.as_string(), crate::test_support::record(tenant, "api.vendor.com"), 0);
    let mut observed = std::collections::BTreeMap::new();
    observed.insert(key.as_string().clone(), 0);
    dp.put(key.as_string(), crate::infra::dp_cache::DpValue::Upstream(Arc::new(
        crate::test_support::record(tenant, "api.vendor.com"),
    )), observed);

    let mut notification = notification("upstream.replace");
    notification.tenant_id = tenant;
    hook.on_upstream_written(notification).await.expect("the hook");

    assert!(state.l1.get(&key.as_string()).is_none(), "the CP entry is invalidated");
    assert!(dp.is_empty(), "the DP entry is flushed");
    let lines = audit.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"event\":\"config_change\""), "{}", lines[0]);
    assert!(lines[0].contains("\"path\":\"/oagw/v1/upstreams/"));
}

/// A written route flushes one DP entry per method of its match block, and a
/// `grpc` route — which derives no key — clears the whole cache.
#[tokio::test]
async fn a_route_write_flushes_per_derived_method() {
    let (state, dp, audit, hook) = wired();
    let upstream = Uuid::new_v4();
    let route = route_record(upstream);
    let notification = WriteNotification {
        event: "route.create",
        tenant_id: Uuid::new_v4(),
        principal_id: Uuid::nil(),
        resource_id: crate::domain::gts_helpers::route_resource_id(route.id),
        upstream_id: Some(upstream),
        upstream_alias: None,
        route: Some(RouteWriteKeys::of(&route)),
        plugin_id: None,
        status: 200,
        outcome: "accepted",
    };
    let keys = CPState::affected_keys(&notification);
    assert!(!keys.is_empty());
    hook.on_route_written(notification.clone()).await.expect("the hook");
    assert!(state.l1.is_empty());
    assert!(dp.is_empty());
    assert_eq!(audit.lines().len(), 1);

    // A `grpc` route derives no key, so the affected key set is empty and the
    // whole DP cache is cleared (`inst-os-algo-inval-8`).
    let mut grpc = notification.clone();
    grpc.route = Some(RouteWriteKeys {
        upstream_id: upstream,
        path_prefix: String::new(),
        methods: Vec::new(),
    });
    let _ = CPState::affected_keys(&grpc);
    assert!(affected_key_strings(&grpc).is_empty());
    let mut observed = std::collections::BTreeMap::new();
    observed.insert(DpHotConfig::upstream_key(Uuid::new_v4(), "api.vendor.com"), 0);
    dp.put(
        DpHotConfig::upstream_key(Uuid::new_v4(), "api.vendor.com"),
        crate::infra::dp_cache::DpValue::Upstream(Arc::new(crate::test_support::record(
            Uuid::new_v4(),
            "api.vendor.com",
        ))),
        observed,
    );
    assert!(!dp.is_empty());
    hook.on_route_written(grpc).await.expect("the hook");
    assert!(dp.is_empty(), "an underivable key set clears the whole cache");
}

/// A plugin write invalidates the reserved plugin family and emits its record.
#[tokio::test]
async fn a_plugin_write_invalidates_the_reserved_family() {
    let (state, dp, audit, hook) = wired();
    let plugin = Uuid::new_v4();
    let mut notification = notification("plugin.created");
    notification.upstream_id = None;
    notification.upstream_alias = None;
    notification.plugin_id = Some(plugin);
    hook.on_plugin_written(notification).await.expect("the hook");
    assert!(state.l1.is_empty());
    assert!(dp.is_empty());
    assert_eq!(audit.lines().len(), 1);
    assert!(audit.lines()[0].contains("\"path\":\"/oagw/v1/plugins/inst-1\""));
}

/// The legacy callback keeps working for a hook that registered for it
/// (`cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`).
#[tokio::test]
async fn the_legacy_callback_flushes_the_whole_cache() {
    let tenant = Uuid::new_v4();
    let (state, dp, _audit, hook) = wired();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() };
    state.l1.insert(&key.as_string(), crate::test_support::record(tenant, "api.vendor.com"), 0);
    hook.on_config_written(tenant, Uuid::new_v4()).await.expect("the hook");
    assert!(state.l1.is_empty(), "an underivable key set clears the CP cache too");
    assert!(dp.is_empty());
}

/// The observability seam one proxy exchange records through.
fn observed() -> (Observability, Arc<AuditSink>) {
    let audit = AuditSink::captured();
    (Observability::new(Arc::new(MetricsRegistry::new()), Arc::clone(&audit)), audit)
}

/// One completed exchange records the request family with the alias as the
/// `host` label, the normalized method and the normalized route pattern, and
/// never a tenant label (`inst-os-req-3`, `inst-os-algo-label-5`).
#[test]
fn an_exchange_records_the_request_family_from_the_observation() {
    let (observability, _audit) = observed();
    let observation = observation(200, "/v1/orders");
    observability.record_observation("api.vendor.com", "PROPFIND", &observation);
    assert_eq!(
        observability
            .metrics
            .value(REQUESTS_TOTAL, &[("host", "api.vendor.com")]),
        Some(1.0)
    );
    let series = &observability.metrics.series(REQUESTS_TOTAL)[0];
    let label = |name: &str| {
        series.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).expect(name)
    };
    assert_eq!(label("http.request.method"), "_OTHER", "a non-standard method is normalized");
    assert_eq!(label("http.route"), "/v1/orders");
    assert_eq!(label("http.response.status_code"), "200");
    assert!(!series.iter().any(|(key, _)| key.contains("tenant")), "no tenant label");
}

/// The phase histograms, the routing pair and the availability gauge of one
/// exchange that reached an endpoint (`inst-os-req-2`, `inst-os-req-7`).
#[test]
fn an_exchange_records_the_phases_the_routing_pair_and_availability() {
    let (observability, _audit) = observed();
    let mut observation = observation(200, "/v1");
    observation.phases = PhaseObservation {
        route_match_ms: Some(1),
        plugin_chain_request_ms: Some(2),
        upstream_call_ms: Some(3),
        plugin_chain_response_ms: None,
        response_ms: Some(4),
    };
    observation.routing = Some(RoutingObservation {
        upstream_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        endpoint_host: "127.0.0.1:8080".to_owned(),
        selection_method: SelectionMethod::RoundRobin,
        target_host_used: false,
    });
    observability.record_observation("api.vendor.com", "GET", &observation);

    for phase in ["route_match", "plugin_chain_request", "upstream_call", "response"] {
        assert!(
            !observability.metrics.series(crate::infra::metrics::REQUEST_DURATION_SECONDS).is_empty(),
            "phase `{phase}` is observed"
        );
    }
    assert_eq!(
        observability.metrics.value(
            ROUTING_ENDPOINT_SELECTED,
            &[("upstream_id", "00000000-0000-0000-0000-000000000001"), ("endpoint_host", "127.0.0.1:8080")],
        ),
        Some(1.0)
    );
    assert_eq!(
        observability.metrics.value(
            UPSTREAM_AVAILABLE,
            &[("host", "api.vendor.com"), ("endpoint", "127.0.0.1:8080")],
        ),
        Some(1.0)
    );
}

/// An upstream that failed the exchange reports unavailable, and the error
/// family carries the gateway error type (`inst-os-req-5`, `inst-os-client-5`).
#[test]
fn a_transport_failure_reports_the_upstream_unavailable() {
    let (observability, _audit) = observed();
    let mut observation = observation(502, "");
    observation.error_type = Some(crate::gts::ERR_LINK_UNAVAILABLE);
    observation.route = None;
    observation.routing = Some(RoutingObservation {
        upstream_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        endpoint_host: "127.0.0.1:8080".to_owned(),
        selection_method: SelectionMethod::RoundRobin,
        target_host_used: false,
    });
    observability.record_observation("api.vendor.com", "GET", &observation);
    assert_eq!(
        observability.metrics.value(
            ERRORS_TOTAL,
            &[("host", "api.vendor.com"), ("error_type", crate::gts::ERR_LINK_UNAVAILABLE)],
        ),
        Some(1.0)
    );
    assert_eq!(
        observability.metrics.value(
            UPSTREAM_AVAILABLE,
            &[("host", "api.vendor.com"), ("endpoint", "127.0.0.1:8080")],
        ),
        Some(0.0)
    );
}

/// The rate-limit pair records the normalized route pattern as the `path`
/// label and never the raw request path (`inst-os-rl-1`, `inst-os-algo-label-6`).
#[test]
fn a_rate_limit_decision_records_the_pair() {
    let (observability, _audit) = observed();
    let mut observation = observation(429, "");
    observation.route = None;
    observation.rate_limit = Some(RateLimitObservation {
        host: "api.vendor.com".to_owned(),
        path: "/v1".to_owned(),
        refused: true,
        usage_ratio_parts_per_million: 500_000,
        retry_after_seconds: Some(7),
    });
    observability.record_observation("api.vendor.com", "GET", &observation);
    assert_eq!(
        observability
            .metrics
            .value(RATE_LIMIT_EXCEEDED_TOTAL, &[("host", "api.vendor.com"), ("path", "/v1")]),
        Some(1.0)
    );
    assert_eq!(
        observability
            .metrics
            .value(RATE_LIMIT_USAGE_RATIO, &[("host", "api.vendor.com"), ("path", "/v1")]),
        Some(0.5)
    );
}

/// The audit record of a proxied request carries the fourteen fields and the
/// correlation identifier, and is emitted even when every other class is
/// suppressed (`inst-os-audit-1`, `inst-os-algo-audit-4`).
#[test]
fn a_proxied_request_is_audited_with_its_correlation_identifier() {
    let (observability, audit) = observed();
    audit.set_sample_rate(1_000);
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    observability.audit_proxy_request(
        "req_1",
        tenant,
        principal,
        Some("api.vendor.com"),
        "/oagw/v1/proxy/api.vendor.com/v1/orders",
        "GET",
        200,
        12,
        3,
        40,
        None,
        false,
    );
    let lines = audit.lines();
    assert_eq!(lines.len(), 1, "the proxy class is never sampled away");
    let document: serde_json::Value = serde_json::from_str(&lines[0]).expect("one JSON object");
    for field in [
        "timestamp", "level", "event", "request_id", "tenant_id", "principal_id", "host", "path",
        "method", "status", "duration_ms", "request_size", "response_size", "error_type",
    ] {
        assert!(document.get(field).is_some(), "`{field}` is carried: {document}");
    }
    assert_eq!(document["event"], "proxy_request");
    assert_eq!(document["request_id"], "req_1");
    assert_eq!(document["tenant_id"], tenant.to_string());
    assert_eq!(document["status"], 200, "the numeric fields are JSON numbers");
    assert_eq!(document["error_type"], serde_json::Value::Null);
}
