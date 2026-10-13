#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    APIKEY_AUTH_PLUGIN_ID, HTTP_PROTOCOL_ID, HttpMatch, HttpMethod, PathSuffixMode, Scheme,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderEntry};
use crate::infra::llm::client::fake_gateway::{Call, FakeGateway, errs, upstream};
use crate::infra::s2s::S2sContextProvider;

fn s2s() -> Arc<S2sContextProvider> {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(7))
        .subject_tenant_id(Uuid::from_u128(1))
        .build()
        .unwrap();
    Arc::new(S2sContextProvider::fixed(ctx))
}

fn fast() -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        warn_after: Duration::from_millis(10),
        ctx_attempts: 2,
        ctx_backoff: Duration::from_millis(1),
    }
}

fn cfg_with(entries: Vec<(&str, serde_json::Value)>) -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    cfg.providers.clear();
    for (id, v) in entries {
        let e: ProviderEntry = serde_json::from_value(v).unwrap();
        cfg.providers.insert(id.to_owned(), e);
    }
    cfg.fill_aliases();
    cfg
}

fn openai_cfg() -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    cfg.fill_aliases();
    cfg
}

fn azure_entry() -> serde_json::Value {
    json!({
        "kind": "openai_responses",
        "host": "res.openai.azure.com",
        "api_path": "/openai/v1/responses?api-version=preview",
        "auth_plugin_type": APIKEY_AUTH_PLUGIN_ID,
        "auth_config": {"header": "api-key", "secret_ref": "cred://azure-key"},
        "storage_kind": "azure",
        "api_version": "2025-04-01-preview"
    })
}

async fn run(
    cfg: &MiniChatConfig,
    gw: &Arc<FakeGateway>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    provision_with(
        cfg,
        Arc::clone(gw) as Arc<dyn ServiceGatewayClientV1>,
        s2s(),
        CancellationToken::new(),
        fast(),
    )
    .await
}

fn http(method: HttpMethod, path: &str, allow: &[&str]) -> HttpMatch {
    HttpMatch {
        methods: vec![method],
        path: path.to_owned(),
        query_allowlist: allow.iter().map(|s| (*s).to_owned()).collect(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

#[tokio::test]
async fn openai_entry_upstream_and_routes() {
    let gw = FakeGateway::new();
    let handle = run(&openai_cfg(), &gw).await.unwrap();
    assert!(handle.is_none());

    let ups = gw.upstream_calls();
    assert_eq!(ups.len(), 1);
    let Call::CreateUpstream {
        alias,
        scheme,
        host,
        port,
        protocol,
        auth,
    } = &ups[0]
    else {
        unreachable!()
    };
    assert_eq!(alias.as_deref(), Some("api.openai.com"));
    assert_eq!(*scheme, Scheme::Https);
    assert_eq!(host, "api.openai.com");
    assert_eq!(*port, 443);
    assert_eq!(protocol, HTTP_PROTOCOL_ID);
    let auth = auth.as_ref().unwrap();
    assert_eq!(auth.plugin_type, APIKEY_AUTH_PLUGIN_ID);
    assert_eq!(auth.sharing, oagw_sdk::SharingMode::Private);
    assert_eq!(
        auth.config
            .as_ref()
            .unwrap()
            .get("secret_ref")
            .map(String::as_str),
        Some("cred://openai-key")
    );

    let up_id = gw.upstreams.lock()[0].id;
    let routes = gw.route_calls();
    assert!(routes.iter().all(|(id, _)| *id == up_id));
    let matches: Vec<HttpMatch> = routes.into_iter().map(|(_, m)| m).collect();
    assert_eq!(
        matches,
        vec![
            http(HttpMethod::Post, "/v1/responses", &[]),
            http(HttpMethod::Post, "/v1/files", &[]),
            http(HttpMethod::Delete, "/v1/files", &[]),
            http(HttpMethod::Post, "/v1/vector_stores", &[]),
            http(HttpMethod::Delete, "/v1/vector_stores", &[]),
            http(HttpMethod::Get, "/v1/vector_stores", &[]),
        ]
    );
}

#[tokio::test]
async fn azure_entry_routes_with_api_version() {
    let gw = FakeGateway::new();
    run(&cfg_with(vec![("azure", azure_entry())]), &gw)
        .await
        .unwrap();
    let matches: Vec<HttpMatch> = gw.route_calls().into_iter().map(|(_, m)| m).collect();
    assert_eq!(
        matches,
        vec![
            http(HttpMethod::Post, "/openai/v1/responses", &["api-version"]),
            http(HttpMethod::Post, "/openai/files", &["api-version"]),
            http(HttpMethod::Delete, "/openai/files", &["api-version"]),
            http(HttpMethod::Post, "/openai/vector_stores", &["api-version"]),
            http(
                HttpMethod::Delete,
                "/openai/vector_stores",
                &["api-version"]
            ),
            http(HttpMethod::Get, "/openai/vector_stores", &["api-version"]),
        ]
    );
    let Call::CreateUpstream { auth, .. } = &gw.upstream_calls()[0] else {
        unreachable!()
    };
    assert_eq!(
        auth.as_ref()
            .unwrap()
            .config
            .as_ref()
            .unwrap()
            .get("header")
            .map(String::as_str),
        Some("api-key")
    );
}

#[test]
fn chat_route_prefix_and_allowlist() {
    assert_eq!(
        chat_route("/v1/responses"),
        ("/v1/responses".to_owned(), vec![])
    );
    assert_eq!(
        chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21&x=1"),
        (
            "/openai/deployments".to_owned(),
            vec!["api-version".to_owned(), "x".to_owned()]
        )
    );
    assert_eq!(chat_route("/{model}"), ("/".to_owned(), vec![]));
}

#[tokio::test]
async fn http_ip_entry_uses_explicit_alias_and_no_auth() {
    let gw = FakeGateway::new();
    let cfg = cfg_with(vec![(
        "mock",
        json!({"kind": "openai_responses", "host": "127.0.0.1", "port": 8099, "use_http": true, "storage_kind": "openai"}),
    )]);
    run(&cfg, &gw).await.unwrap();
    let Call::CreateUpstream {
        alias,
        scheme,
        port,
        auth,
        ..
    } = &gw.upstream_calls()[0]
    else {
        unreachable!()
    };
    assert_eq!(alias.as_deref(), Some("127.0.0.1:8099"));
    assert_eq!(*scheme, Scheme::Http);
    assert_eq!(*port, 8099);
    assert!(auth.is_none());
}

#[tokio::test]
async fn rejected_alias_for_hostname_is_retried_without_alias() {
    let gw = FakeGateway::new();
    gw.upstream_results
        .lock()
        .push_back(Some(errs::invalid_argument("alias is auto-derived")));
    let cfg = cfg_with(vec![(
        "custom",
        json!({"kind": "openai_responses", "host": "llm.example.com", "upstream_alias": "my-llm", "storage_kind": "openai"}),
    )]);
    assert!(run(&cfg, &gw).await.unwrap().is_none());
    let aliases: Vec<Option<String>> = gw
        .upstream_calls()
        .into_iter()
        .map(|c| match c {
            Call::CreateUpstream { alias, .. } => alias,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(aliases, vec![Some("my-llm".to_owned()), None]);
    assert_eq!(gw.route_calls().len(), 6);
}

#[tokio::test]
async fn invalid_argument_for_ip_host_fails_startup() {
    let gw = FakeGateway::new();
    gw.upstream_results
        .lock()
        .push_back(Some(errs::invalid_argument("bad secret ref")));
    let cfg = cfg_with(vec![(
        "mock",
        json!({"kind": "openai_responses", "host": "10.0.0.1", "storage_kind": "openai"}),
    )]);
    let err = run(&cfg, &gw).await.unwrap_err();
    assert!(err.to_string().contains("mock"), "{err}");
    assert_eq!(gw.upstream_calls().len(), 1);
    assert!(gw.route_calls().is_empty());
}

#[tokio::test]
async fn invalid_argument_twice_for_hostname_fails_startup() {
    let gw = FakeGateway::new();
    gw.upstream_results.lock().extend([
        Some(errs::invalid_argument("x")),
        Some(errs::invalid_argument("x")),
    ]);
    assert!(run(&openai_cfg(), &gw).await.is_err());
}

#[tokio::test]
async fn already_existing_upstream_is_reused() {
    let gw = FakeGateway::new();
    let existing = upstream("API.openai.com", "api.openai.com");
    let other = upstream("other", "other.example");
    gw.upstreams.lock().extend([other, existing.clone()]);
    gw.upstream_results
        .lock()
        .push_back(Some(errs::already_exists()));
    assert!(run(&openai_cfg(), &gw).await.unwrap().is_none());
    assert!(gw.calls().iter().any(|c| matches!(c, Call::ListUpstreams)));
    let routes = gw.route_calls();
    assert_eq!(routes.len(), 6);
    assert!(routes.iter().all(|(id, _)| *id == existing.id));
}

#[tokio::test]
async fn existing_routes_are_ignored() {
    let gw = FakeGateway::new();
    gw.route_results
        .lock()
        .extend((0..6).map(|_| Some(errs::already_exists())));
    assert!(run(&openai_cfg(), &gw).await.unwrap().is_none());
    assert_eq!(gw.route_calls().len(), 6);
}

#[tokio::test]
async fn invalid_rag_route_is_best_effort_but_chat_route_is_fatal() {
    let gw = FakeGateway::new();
    gw.route_results
        .lock()
        .extend([None, Some(errs::invalid_argument("bad rag route"))]);
    assert!(run(&openai_cfg(), &gw).await.unwrap().is_none());
    assert_eq!(gw.route_calls().len(), 6);

    let gw = FakeGateway::new();
    gw.route_results
        .lock()
        .push_back(Some(errs::invalid_argument("bad chat route")));
    assert!(run(&openai_cfg(), &gw).await.is_err());
}

#[tokio::test]
async fn unreadable_secret_is_deferred_and_retried() {
    let gw = FakeGateway::new();
    gw.upstream_results.lock().extend([
        Some(errs::failed_precondition()),
        Some(errs::failed_precondition()),
    ]);
    let handle = run(&openai_cfg(), &gw)
        .await
        .unwrap()
        .expect("retry task spawned");
    // First pass created no routes.
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("retry loop finishes once provisioned")
        .unwrap();
    assert_eq!(gw.upstream_calls().len(), 3);
    assert_eq!(gw.route_calls().len(), 6);
}

#[tokio::test]
async fn deferred_route_failure_is_retried() {
    let gw = FakeGateway::new();
    gw.route_results.lock().push_back(Some(errs::unavailable()));
    let handle = run(&openai_cfg(), &gw).await.unwrap().expect("deferred");
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    // Second pass: upstream created again (fake has no duplicate detection) and all routes.
    assert_eq!(gw.route_calls().len(), 1 + 6);
}

#[tokio::test]
async fn retry_loop_stops_on_cancel() {
    let gw = FakeGateway::new();
    gw.upstream_results
        .lock()
        .extend((0..10_000).map(|_| Some(errs::failed_precondition())));
    let cancel = CancellationToken::new();
    let handle = provision_with(
        &openai_cfg(),
        Arc::clone(&gw) as Arc<dyn ServiceGatewayClientV1>,
        s2s(),
        cancel.clone(),
        fast(),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    assert!(gw.upstream_calls().len() >= 2);
    assert!(gw.route_calls().is_empty());
}

#[tokio::test]
async fn tenant_override_gets_its_own_upstream() {
    let gw = FakeGateway::new();
    let tenant = Uuid::from_u128(42).to_string();
    let mut entry = azure_entry();
    entry["tenant_overrides"] = json!({ tenant.clone(): {"host": "tenant.openai.azure.com"} });
    let cfg = cfg_with(vec![("azure", entry)]);
    assert!(run(&cfg, &gw).await.unwrap().is_none());
    let ups: Vec<(Option<String>, String, bool)> = gw
        .upstream_calls()
        .into_iter()
        .map(|c| match c {
            Call::CreateUpstream {
                alias, host, auth, ..
            } => (alias, host, auth.is_some()),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        ups,
        vec![
            (
                Some("res.openai.azure.com".to_owned()),
                "res.openai.azure.com".to_owned(),
                true
            ),
            (
                Some("tenant.openai.azure.com".to_owned()),
                "tenant.openai.azure.com".to_owned(),
                true
            ),
        ]
    );
    assert_eq!(gw.route_calls().len(), 12);
}

#[test]
fn targets_skip_overrides_without_host_or_alias() {
    let mut entry = azure_entry();
    entry["tenant_overrides"] =
        json!({ Uuid::from_u128(1).to_string(): {"auth_config": {"header": "x"}} });
    let cfg = cfg_with(vec![("azure", entry)]);
    assert_eq!(targets(&cfg).len(), 1);
}
