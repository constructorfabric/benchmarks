use std::collections::BTreeMap;
use std::time::Duration;

use oagw_sdk::{HttpMatch, HttpMethod, PathSuffixMode, Scheme};

use super::{
    ErrorClass, MAX_RETRY_DELAY, PENDING_WARNING_AFTER, SystemContextError, UpstreamPlan,
    chat_route, classify, pending_warning_due, plan_upstreams, provision, rag_routes, retry_delay,
    retry_pending,
};
use crate::config::{MiniChatConfig, ProviderEntry, StorageKind};
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, deadline_exceeded, lock, secret_not_accessible,
    upstream_exists, validation,
};

const TENANT_A: &str = "a0000000-0000-4000-8000-00000000000a";

fn providers(raw: &serde_json::Value) -> BTreeMap<String, ProviderEntry> {
    let mut config: MiniChatConfig =
        serde_json::from_value(serde_json::json!({ "providers": raw })).unwrap();
    config.fill_upstream_aliases();
    config.providers
}

/// The e2e provider entry (mock provider over plain HTTP).
fn e2e_providers() -> BTreeMap<String, ProviderEntry> {
    providers(&serde_json::json!({
        "openai": {
            "kind": "openai_responses",
            "storage_kind": "openai",
            "host": "127.0.0.1",
            "port": 18080,
            "use_http": true,
            "api_path": "/v1/responses",
            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "auth_config": {
                "header": "Authorization",
                "prefix": "Bearer ",
                "secret_ref": "cred://openai-key",
            },
        }
    }))
}

fn azure_with_overrides() -> BTreeMap<String, ProviderEntry> {
    providers(&serde_json::json!({
        "azure": {
            "kind": "openai_responses",
            "storage_kind": "azure",
            "api_version": "2025-04-01-preview",
            "host": "shared.openai.azure.com",
            "api_path": "/openai/deployments/{model}/responses?api-version=2025-04-01-preview",
            "auth_plugin_type": "plugin-a",
            "auth_config": { "secret_ref": "cred://shared" },
            "tenant_overrides": {
                TENANT_A: { "host": "tenant-a.openai.azure.com", "auth_config": { "secret_ref": "cred://a" } },
                "b0000000-0000-4000-8000-00000000000b": { "upstream_alias": "shared.openai.azure.com" },
            },
        }
    }))
}

fn http(route: &super::RoutePlan) -> HttpMatch {
    route.match_rules().http.unwrap()
}

// ── Plans and route shapes ───────────────────────────────────────────────

#[test]
fn chat_route_without_model_placeholder_is_exact() {
    let route = chat_route("/v1/responses").unwrap();
    assert_eq!(route.methods, vec![HttpMethod::Post]);
    assert_eq!(route.path, "/v1/responses");
    assert!(route.query_allowlist.is_empty());
    assert_eq!(route.path_suffix_mode, PathSuffixMode::Disabled);
}

#[test]
fn chat_route_with_model_placeholder_and_query() {
    let route =
        chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21&x=1&x=2")
            .unwrap();
    assert_eq!(route.path, "/openai/deployments/");
    assert_eq!(route.path_suffix_mode, PathSuffixMode::Append);
    assert_eq!(route.query_allowlist, vec!["api-version", "x"]);
    assert!(chat_route("v1/responses").is_err());
}

#[test]
fn rag_routes_per_storage_kind() {
    let openai: Vec<(Vec<HttpMethod>, String, PathSuffixMode)> = rag_routes(StorageKind::Openai)
        .iter()
        .map(|route| {
            (
                route.methods.clone(),
                route.path.clone(),
                route.path_suffix_mode,
            )
        })
        .collect();
    assert_eq!(
        openai,
        vec![
            (
                vec![HttpMethod::Post],
                "/v1/files".to_owned(),
                PathSuffixMode::Disabled
            ),
            (
                vec![HttpMethod::Delete],
                "/v1/files/".to_owned(),
                PathSuffixMode::Append
            ),
            (
                vec![HttpMethod::Post],
                "/v1/vector_stores".to_owned(),
                PathSuffixMode::Append
            ),
            (
                vec![HttpMethod::Get, HttpMethod::Delete],
                "/v1/vector_stores/".to_owned(),
                PathSuffixMode::Append
            ),
        ]
    );
    for route in rag_routes(StorageKind::Azure) {
        assert!(route.path.starts_with("/openai/"), "{}", route.path);
        assert_eq!(route.query_allowlist, vec!["api-version"]);
    }
}

#[test]
fn plan_for_the_e2e_entry() {
    let plans = plan_upstreams(&e2e_providers()).unwrap();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.label, "openai");
    assert_eq!(plan.alias, "127.0.0.1");
    assert_eq!(plan.endpoint.scheme, Scheme::Http);
    assert_eq!(plan.endpoint.host, "127.0.0.1");
    assert_eq!(plan.endpoint.port, 18080);
    let auth = plan.auth.as_ref().unwrap();
    assert_eq!(
        auth.plugin_type,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    let config = auth.config.as_ref().unwrap();
    assert_eq!(config["secret_ref"], "cred://openai-key");
    assert_eq!(config["prefix"], "Bearer ");
    assert_eq!(plan.chat_route.path, "/v1/responses");
    assert_eq!(plan.rag_routes.len(), 4);
}

#[test]
fn plans_include_overrides_with_their_own_alias() {
    let plans = plan_upstreams(&azure_with_overrides()).unwrap();
    let labels: Vec<&str> = plans.iter().map(|plan| plan.label.as_str()).collect();
    // Tenant B's override reuses the provider alias: no separate upstream.
    assert_eq!(
        labels,
        vec!["azure".to_owned(), format!("azure/{TENANT_A}")]
    );

    let base = &plans[0];
    assert_eq!(base.alias, "shared.openai.azure.com");
    assert_eq!(base.endpoint.port, 443);
    assert_eq!(base.endpoint.scheme, Scheme::Https);

    let tenant = &plans[1];
    assert_eq!(tenant.alias, "tenant-a.openai.azure.com");
    assert_eq!(tenant.endpoint.host, "tenant-a.openai.azure.com");
    let auth = tenant.auth.as_ref().unwrap();
    assert_eq!(
        auth.plugin_type, "plugin-a",
        "plugin type falls back to the entry"
    );
    assert_eq!(auth.config.as_ref().unwrap()["secret_ref"], "cred://a");
    assert_eq!(tenant.chat_route, base.chat_route);
}

#[test]
fn plan_rejects_relative_api_path() {
    let entries = providers(&serde_json::json!({
        "bad": { "kind": "openai_responses", "storage_kind": "openai", "host": "h.example", "api_path": "responses" }
    }));
    let err = plan_upstreams(&entries).unwrap_err();
    assert_eq!(err.label, "bad");
}

// ── Classification ───────────────────────────────────────────────────────

#[test]
fn error_classification() {
    assert_eq!(classify(&upstream_exists("a")), ErrorClass::Exists);
    assert_eq!(classify(&secret_not_accessible()), ErrorClass::Transient);
    assert_eq!(classify(&deadline_exceeded()), ErrorClass::Transient);
    assert_eq!(
        classify(&validation(
            "alias is auto-derived for hostname-based endpoints"
        )),
        ErrorClass::Deterministic
    );
    assert_eq!(
        classify(&validation(
            "secret_ref 'cred://k' validation failed: no plugin available"
        )),
        ErrorClass::Transient
    );
    assert_eq!(
        classify(&validation("invalid secret_ref 'x': bad")),
        ErrorClass::Deterministic
    );
}

// ── Provisioning against the fake gateway ────────────────────────────────

fn plans() -> Vec<UpstreamPlan> {
    plan_upstreams(&e2e_providers()).unwrap()
}

#[tokio::test]
async fn provision_creates_upstream_and_routes() {
    let gateway = FakeGateway::default();
    let report = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap();

    assert!(report.pending.is_empty());
    assert_eq!(report.registered.len(), 1);
    let registered = &report.registered[0];
    assert_eq!(registered.alias, "127.0.0.1");
    assert!(!registered.reused);

    let upstreams = lock(&gateway.upstreams).clone();
    assert_eq!(upstreams.len(), 1);
    assert_eq!(upstreams[0].protocol, oagw_sdk::HTTP_PROTOCOL_ID);
    let routes = gateway.routes_of(registered.upstream_id);
    let paths: Vec<String> = routes
        .iter()
        .map(|r| r.http.clone().unwrap().path)
        .collect();
    assert_eq!(
        paths,
        vec![
            "/v1/responses",
            "/v1/files",
            "/v1/files/",
            "/v1/vector_stores",
            "/v1/vector_stores/"
        ]
    );
    assert_eq!(
        routes[0].http.clone().unwrap(),
        http(&plans()[0].chat_route)
    );
}

#[tokio::test]
async fn provision_reuses_an_existing_upstream_and_routes() {
    let gateway = FakeGateway::default();
    let first = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap();
    let second = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap();

    assert!(second.pending.is_empty());
    assert!(second.registered[0].reused);
    assert_eq!(
        second.registered[0].upstream_id,
        first.registered[0].upstream_id
    );
    assert_eq!(lock(&gateway.upstreams).len(), 1);
    assert_eq!(lock(&gateway.routes).len(), 5, "existing routes are reused");
}

#[tokio::test]
async fn unreadable_secret_leaves_the_entry_pending_until_retried() {
    let gateway = FakeGateway::default();
    lock(&gateway.create_upstream_errors).push_back(secret_not_accessible());
    let system = StaticSystemContext::default();

    let report = provision(&gateway, &system, &plans()).await.unwrap();
    assert!(report.registered.is_empty());
    assert_eq!(report.pending.len(), 1);
    assert!(report.pending[0].reason.contains("create upstream"));

    let still_pending = retry_pending(&gateway, &system, report.pending_plans()).await;
    assert!(still_pending.is_empty());
    assert_eq!(lock(&gateway.upstreams).len(), 1);
}

#[tokio::test]
async fn deterministic_failure_fails_startup() {
    let gateway = FakeGateway::default();
    lock(&gateway.create_upstream_errors).push_back(validation("alias is auto-derived"));
    let err = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap_err();
    assert_eq!(err.label, "openai");
}

#[tokio::test]
async fn rag_route_failure_is_best_effort() {
    let gateway = FakeGateway::default();
    lock(&gateway.failing_route_paths).push("/v1/files".to_owned());
    let report = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap();
    assert_eq!(report.registered.len(), 1);
    assert!(report.pending.is_empty());
    assert_eq!(
        lock(&gateway.routes).len(),
        4,
        "only the failed RAG route is missing"
    );
}

#[tokio::test]
async fn chat_route_failure_is_not_best_effort() {
    let gateway = FakeGateway::default();
    lock(&gateway.failing_route_paths).push("/v1/responses".to_owned());
    let err = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap_err();
    assert!(err.detail.contains("create route"), "{err}");

    let gateway = FakeGateway::default();
    lock(&gateway.create_route_errors).push_back(deadline_exceeded());
    let report = provision(&gateway, &StaticSystemContext::default(), &plans())
        .await
        .unwrap();
    assert_eq!(
        report.pending.len(),
        1,
        "a transient chat route failure is retried"
    );
}

#[tokio::test]
async fn s2s_failures() {
    let gateway = FakeGateway::default();
    let system = StaticSystemContext::default();

    *lock(&system.fail_with) = Some(SystemContextError::Unavailable("idp down".to_owned()));
    let report = provision(&gateway, &system, &plans()).await.unwrap();
    assert_eq!(report.pending.len(), 1);
    let still = retry_pending(&gateway, &system, report.pending_plans()).await;
    assert_eq!(still.len(), 1, "no context: everything stays pending");

    *lock(&system.fail_with) = Some(SystemContextError::Rejected("bad secret".to_owned()));
    let err = provision(&gateway, &system, &plans()).await.unwrap_err();
    assert_eq!(err.label, "client_credentials");
}

#[tokio::test]
async fn reconcile_drops_deterministic_failures() {
    let gateway = FakeGateway::default();
    lock(&gateway.create_upstream_errors).push_back(validation("bad endpoint"));
    let still = retry_pending(&gateway, &StaticSystemContext::default(), plans()).await;
    assert!(still.is_empty());
    assert!(lock(&gateway.upstreams).is_empty());
}

// ── Retry scheduling ─────────────────────────────────────────────────────

#[test]
fn retry_delay_doubles_from_two_seconds_up_to_sixty() {
    let delays: Vec<u64> = (0..8)
        .map(|attempt| retry_delay(attempt).as_secs())
        .collect();
    assert_eq!(delays, vec![2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(retry_delay(40), MAX_RETRY_DELAY);
    assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY);
}

#[test]
fn pending_warning_is_due_once_after_two_minutes() {
    assert!(!pending_warning_due(Duration::from_secs(119), false));
    assert!(pending_warning_due(PENDING_WARNING_AFTER, false));
    assert!(!pending_warning_due(Duration::from_secs(600), true));
}
