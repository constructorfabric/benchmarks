use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    CreateUpstreamRequest, HttpMatch, HttpMethod, PathSuffixMode, Route, Scheme, Server,
    ServiceGatewayClientV1, Upstream,
};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, OAGW_APIKEY_AUTH_PLUGIN};
use crate::test_support::FakeOagw;
use crate::test_support::fake_oagw::invalid_argument_error;

const TENANT_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";

fn config(providers: &Value) -> MiniChatConfig {
    let mut cfg: MiniChatConfig = serde_json::from_value(json!({
        "client_credentials": {"client_id": "c", "client_secret": "s"},
        "providers": providers,
    }))
    .unwrap();
    cfg.validate().unwrap();
    cfg
}

fn openai_entry() -> Value {
    json!({
        "kind": "openai_responses",
        "host": "api.openai.com",
        "storage_kind": "openai",
        "auth_plugin_type": OAGW_APIKEY_AUTH_PLUGIN,
        "auth_config": {"header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key"},
        "tenant_overrides": {
            TENANT_A: {"host": "tenant-a.example", "auth_config": {"header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://tenant-a"}}
        }
    })
}

fn azure_entry() -> Value {
    json!({
        "kind": "openai_responses",
        "host": "127.0.0.1",
        "port": 8443,
        "use_http": true,
        "upstream_alias": "azure-local",
        "api_path": "/openai/deployments/{model}/responses?api-version=2025-01-01",
        "storage_kind": "azure",
        "api_version": "2025-03-01-preview",
    })
}

fn s2s() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .unwrap()
}

fn upstream<'a>(ups: &'a [Upstream], alias: &str) -> &'a Upstream {
    ups.iter()
        .find(|u| u.alias == alias)
        .unwrap_or_else(|| panic!("no upstream {alias}"))
}

fn http(r: &Route) -> &HttpMatch {
    r.match_rules.http.as_ref().unwrap()
}

/// `(method, path, suffix, query_allowlist)` of every route of `alias`.
fn route_set(fake: &FakeOagw, alias: &str) -> BTreeSet<(String, String, String, Vec<String>)> {
    fake.routes_for_alias(alias)
        .iter()
        .map(|r| {
            let h = http(r);
            assert_eq!(h.methods.len(), 1);
            (
                format!("{:?}", h.methods[0]),
                h.path.clone(),
                format!("{:?}", h.path_suffix_mode),
                h.query_allowlist.clone(),
            )
        })
        .collect()
}

fn rag_routes(prefix: &str, query: &[&str]) -> Vec<(String, String, String, Vec<String>)> {
    let q: Vec<String> = query.iter().map(|s| (*s).to_owned()).collect();
    vec![
        (
            "Post".into(),
            format!("{prefix}/files"),
            "Disabled".into(),
            q.clone(),
        ),
        (
            "Delete".into(),
            format!("{prefix}/files/"),
            "Append".into(),
            q.clone(),
        ),
        (
            "Post".into(),
            format!("{prefix}/vector_stores"),
            "Append".into(),
            q.clone(),
        ),
        (
            "Delete".into(),
            format!("{prefix}/vector_stores/"),
            "Append".into(),
            q.clone(),
        ),
        (
            "Get".into(),
            format!("{prefix}/vector_stores/"),
            "Append".into(),
            q,
        ),
    ]
}

#[tokio::test]
async fn creates_upstream_and_routes_per_entry_and_override() {
    let fake = FakeOagw::new();
    let cfg = config(&json!({"openai": openai_entry(), "azure": azure_entry()}));
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert!(report.deferred.is_empty(), "{report:?}");
    let mut ok = report.ok;
    ok.sort();
    assert_eq!(ok, vec!["azure", "openai", &format!("openai@{TENANT_A}")]);

    let ups = fake.upstreams();
    assert_eq!(ups.len(), 3);

    let base = upstream(&ups, "api.openai.com");
    assert_eq!(
        base.server.endpoints,
        vec![Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".into(),
            port: 443,
        }]
    );
    assert_eq!(base.protocol, oagw_sdk::HTTP_PROTOCOL_ID);
    let auth = base.auth.as_ref().unwrap();
    assert_eq!(auth.plugin_type, OAGW_APIKEY_AUTH_PLUGIN);
    let auth_cfg = auth.config.as_ref().unwrap();
    assert_eq!(auth_cfg["secret_ref"], "cred://openai-key");
    assert_eq!(auth_cfg["header"], "Authorization");
    assert_eq!(auth_cfg["prefix"], "Bearer ");

    // The override inherits the plugin type and uses its own config and host.
    let tenant = upstream(&ups, "tenant-a.example");
    assert_eq!(tenant.server.endpoints[0].host, "tenant-a.example");
    let t_auth = tenant.auth.as_ref().unwrap();
    assert_eq!(t_auth.plugin_type, OAGW_APIKEY_AUTH_PLUGIN);
    assert_eq!(
        t_auth.config.as_ref().unwrap()["secret_ref"],
        "cred://tenant-a"
    );

    let azure = upstream(&ups, "azure-local");
    assert_eq!(
        azure.server.endpoints,
        vec![Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".into(),
            port: 8443,
        }]
    );
    assert!(azure.auth.is_none());

    let mut expected: BTreeSet<_> = rag_routes("/v1", &[]).into_iter().collect();
    expected.insert((
        "Post".into(),
        "/v1/responses".into(),
        "Disabled".into(),
        Vec::new(),
    ));
    assert_eq!(route_set(&fake, "api.openai.com"), expected);
    assert_eq!(route_set(&fake, "tenant-a.example"), expected);

    // `{model}` is a path suffix; the query allowlist comes from `api_path`.
    let azure_routes = route_set(&fake, "azure-local");
    assert!(azure_routes.contains(&(
        "Post".into(),
        "/openai/deployments/".into(),
        "Append".into(),
        vec!["api-version".to_owned()],
    )));
}

#[tokio::test]
async fn existing_alias_is_reused() {
    let fake = FakeOagw::new();
    let existing = fake
        .create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                Server {
                    endpoints: vec![Endpoint {
                        scheme: Scheme::Https,
                        host: "api.openai.com".into(),
                        port: 443,
                    }],
                },
                oagw_sdk::HTTP_PROTOCOL_ID,
            )
            .build(),
        )
        .await
        .unwrap();
    let mut entry = openai_entry();
    entry.as_object_mut().unwrap().remove("tenant_overrides");
    let cfg = config(&json!({"openai": entry}));

    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.ok, vec!["openai"]);
    let ups = fake.upstreams();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].id, existing.id);
    let routes = fake.routes();
    assert_eq!(routes.len(), 6);
    assert!(routes.iter().all(|r| r.upstream_id == existing.id));

    // A second pass (restart) reuses the upstream and the routes.
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.ok, vec!["openai"]);
    assert_eq!(fake.upstreams().len(), 1);
    assert_eq!(fake.routes().len(), 6);
}

#[tokio::test]
async fn azure_rag_routes_have_api_version() {
    let fake = FakeOagw::new();
    let mut entry = azure_entry();
    entry["api_path"] = json!("/openai/v1/responses");
    let cfg = config(&json!({"azure": entry}));
    provision_all(&fake, &s2s(), &cfg).await.unwrap();
    let mut expected: BTreeSet<_> = rag_routes("/openai", &["api-version"])
        .into_iter()
        .collect();
    expected.insert((
        "Post".into(),
        "/openai/v1/responses".into(),
        "Disabled".into(),
        Vec::new(),
    ));
    assert_eq!(route_set(&fake, "azure-local"), expected);
}

#[tokio::test]
async fn unreadable_secret_is_deferred() {
    let fake = FakeOagw::new();
    fake.set_secret_unavailable("cred://openai-key", true);
    let cfg = config(&json!({"openai": openai_entry()}));
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.deferred, vec!["openai"]);
    assert_eq!(report.ok, vec![format!("openai@{TENANT_A}")]);
    assert!(fake.routes_for_alias("api.openai.com").is_empty());

    fake.set_secret_unavailable("cred://openai-key", false);
    let pending: Vec<UpstreamSpec> = plan(&cfg.providers)
        .into_iter()
        .filter(|s| report.deferred.contains(&s.label))
        .collect();
    let report = provision(&fake, &s2s(), &pending).await.unwrap();
    assert_eq!(report.ok, vec!["openai"]);
    assert_eq!(fake.routes_for_alias("api.openai.com").len(), 6);
}

#[tokio::test]
async fn deterministic_misconfiguration_fails() {
    let fake = FakeOagw::new();
    fake.fail_create_upstream("api.openai.com", invalid_argument_error("bad alias"));
    let mut entry = openai_entry();
    entry.as_object_mut().unwrap().remove("tenant_overrides");
    let cfg = config(&json!({"openai": entry}));
    let err = provision_all(&fake, &s2s(), &cfg).await.unwrap_err();
    assert!(format!("{err:#}").contains("openai"), "{err:#}");
}

#[test]
fn retry_delay_doubles_from_2s_to_60s() {
    let secs: Vec<u64> = (0..8).map(|n| retry_delay(n).as_secs()).collect();
    assert_eq!(secs, vec![2, 4, 8, 16, 32, 60, 60, 60]);
}

#[tokio::test(start_paused = true)]
async fn reconcile_retries_deferred_until_provisioned() {
    let fake = Arc::new(FakeOagw::new());
    fake.set_secret_unavailable("cred://openai-key", true);
    let mut entry = openai_entry();
    entry.as_object_mut().unwrap().remove("tenant_overrides");
    let cfg = config(&json!({"openai": entry}));
    let specs = plan(&cfg.providers);
    let cancel = CancellationToken::new();
    let s2s_provider = Arc::new(S2sContextProvider::fixed(s2s()));

    let task = {
        let (fake, s2s_provider, cancel) = (fake.clone(), s2s_provider.clone(), cancel.clone());
        tokio::spawn(async move {
            reconcile_deferred(fake.as_ref(), &s2s_provider, specs, cancel).await;
        })
    };
    // First attempt after 2 s: still not readable.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(fake.upstreams().is_empty());
    fake.set_secret_unavailable("cred://openai-key", false);
    // Second attempt 4 s later succeeds and the loop ends.
    tokio::time::sleep(Duration::from_secs(4)).await;
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("reconcile loop finished")
        .unwrap();
    assert_eq!(fake.upstreams().len(), 1);
    assert_eq!(fake.routes_for_alias("api.openai.com").len(), 6);
}

#[tokio::test(start_paused = true)]
async fn reconcile_stops_on_cancel() {
    let fake = Arc::new(FakeOagw::new());
    fake.set_secret_unavailable("cred://openai-key", true);
    let cfg = config(&json!({"openai": openai_entry()}));
    let cancel = CancellationToken::new();
    let s2s_provider = S2sContextProvider::fixed(s2s());
    let fut = reconcile_deferred(
        fake.as_ref(),
        &s2s_provider,
        plan(&cfg.providers),
        cancel.clone(),
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), fut)
        .await
        .expect("loop stops on cancel");
}

#[test]
fn plan_labels_entries_and_overrides() {
    let cfg = config(&json!({"openai": openai_entry()}));
    let labels: HashMap<String, String> = plan(&cfg.providers)
        .into_iter()
        .map(|s| (s.label, s.alias))
        .collect();
    assert_eq!(labels["openai"], "api.openai.com");
    assert_eq!(labels[&format!("openai@{TENANT_A}")], "tenant-a.example");
}

// ---------------------------------------------------------------------------
// Review fix round 1: aliases, reconciliation of reused objects, RAG retries
// ---------------------------------------------------------------------------

fn server(scheme: Scheme, host: &str, port: u16) -> Server {
    Server {
        endpoints: vec![Endpoint {
            scheme,
            host: host.into(),
            port,
        }],
    }
}

#[tokio::test]
async fn fake_oagw_enforces_the_oagw_alias_rule() {
    let fake = FakeOagw::new();
    // Hostname endpoint: an explicit alias must equal the derived one.
    let err = fake
        .create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                server(Scheme::Http, "localhost", 8080),
                oagw_sdk::HTTP_PROTOCOL_ID,
            )
            .alias("localhost")
            .build(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        toolkit_canonical_errors::CanonicalError::InvalidArgument { .. }
    ));
    // IP endpoint without alias: rejected (explicit alias required).
    assert!(
        fake.create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                server(Scheme::Https, "10.0.0.1", 443),
                oagw_sdk::HTTP_PROTOCOL_ID
            )
            .build(),
        )
        .await
        .is_err()
    );
    // Derived alias is scheme-aware about the standard port.
    let up = fake
        .create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                server(Scheme::Http, "svc", 443),
                oagw_sdk::HTTP_PROTOCOL_ID,
            )
            .build(),
        )
        .await
        .unwrap();
    assert_eq!(up.alias, "svc:443");
}

#[tokio::test]
async fn hostname_on_custom_port_provisions_under_resolver_alias() {
    let fake = FakeOagw::new();
    let cfg = config(&json!({"mock": {
        "kind": "openai_responses", "storage_kind": "openai",
        "host": "localhost", "port": 8080, "use_http": true,
    }}));
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.ok, vec!["mock"]);
    let resolver = crate::infra::llm::provider_resolver::ProviderResolver::new(&cfg.providers);
    let target = resolver.resolve("mock", Uuid::new_v4()).unwrap();
    assert_eq!(target.alias, "localhost:8080");
    let ups = fake.upstreams();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].alias, target.alias);
    assert_eq!(fake.routes_for_alias("localhost:8080").len(), 6);
}

#[tokio::test]
async fn custom_alias_on_hostname_is_registered_as_derived() {
    let fake = FakeOagw::new();
    let cfg = config(&json!({"custom": {
        "kind": "openai_responses", "storage_kind": "openai",
        "host": "api.example.com", "upstream_alias": "my-alias",
    }}));
    provision_all(&fake, &s2s(), &cfg).await.unwrap();
    let resolver = crate::infra::llm::provider_resolver::ProviderResolver::new(&cfg.providers);
    assert_eq!(
        resolver.resolve("custom", Uuid::new_v4()).unwrap().alias,
        "api.example.com"
    );
    assert_eq!(fake.upstreams()[0].alias, "api.example.com");
}

#[tokio::test]
async fn reused_upstream_with_other_credentials_is_updated() {
    let fake = FakeOagw::new();
    let old_auth = oagw_sdk::AuthConfig {
        plugin_type: OAGW_APIKEY_AUTH_PLUGIN.into(),
        sharing: oagw_sdk::SharingMode::Private,
        config: Some(HashMap::from([(
            "secret_ref".to_owned(),
            "cred://old".to_owned(),
        )])),
    };
    let existing = fake
        .create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                server(Scheme::Https, "api.openai.com", 443),
                oagw_sdk::HTTP_PROTOCOL_ID,
            )
            .auth(old_auth)
            .tags(vec!["operator".into()])
            .build(),
        )
        .await
        .unwrap();
    let mut entry = openai_entry();
    entry.as_object_mut().unwrap().remove("tenant_overrides");
    let cfg = config(&json!({"openai": entry}));
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.ok, vec!["openai"]);
    let ups = fake.upstreams();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].id, existing.id);
    let auth = ups[0].auth.as_ref().unwrap();
    assert_eq!(
        auth.config.as_ref().unwrap()["secret_ref"],
        "cred://openai-key"
    );
    // Fields mini-chat does not manage are preserved.
    assert_eq!(ups[0].tags, vec!["operator".to_owned()]);
}

#[tokio::test]
async fn reused_ip_upstream_with_other_endpoint_is_updated() {
    let fake = FakeOagw::new();
    fake.create_upstream(
        s2s(),
        CreateUpstreamRequest::builder(
            server(Scheme::Https, "127.0.0.1", 9999),
            oagw_sdk::HTTP_PROTOCOL_ID,
        )
        .alias("azure-local")
        .build(),
    )
    .await
    .unwrap();
    let cfg = config(&json!({"azure": azure_entry()}));
    provision_all(&fake, &s2s(), &cfg).await.unwrap();
    let ups = fake.upstreams();
    assert_eq!(ups.len(), 1);
    assert_eq!(
        ups[0].server.endpoints,
        vec![Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".into(),
            port: 8443,
        }]
    );
}

#[tokio::test]
async fn existing_route_with_narrower_match_is_updated() {
    let fake = FakeOagw::new();
    let up = fake
        .create_upstream(
            s2s(),
            CreateUpstreamRequest::builder(
                server(Scheme::Http, "127.0.0.1", 8443),
                oagw_sdk::HTTP_PROTOCOL_ID,
            )
            .alias("azure-local")
            .build(),
        )
        .await
        .unwrap();
    let rules = oagw_sdk::MatchRules {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Delete],
            path: "/openai/files/".into(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Disabled,
        }),
        grpc: None,
    };
    fake.create_route(
        s2s(),
        oagw_sdk::CreateRouteRequest::builder(up.id, rules).build(),
    )
    .await
    .unwrap();

    let cfg = config(&json!({"azure": azure_entry()}));
    provision_all(&fake, &s2s(), &cfg).await.unwrap();
    let matching: Vec<Route> = fake
        .routes_for_alias("azure-local")
        .into_iter()
        .filter(|r| http(r).path == "/openai/files/")
        .collect();
    assert_eq!(matching.len(), 1);
    let h = http(&matching[0]);
    assert_eq!(h.path_suffix_mode, PathSuffixMode::Append);
    assert_eq!(h.query_allowlist, vec!["api-version".to_owned()]);
}

#[tokio::test]
async fn transient_rag_route_failure_stays_pending() {
    let fake = FakeOagw::new();
    fake.fail_create_route_once(
        "/v1/files",
        crate::test_support::fake_oagw::unavailable_error(),
    );
    let mut entry = openai_entry();
    entry.as_object_mut().unwrap().remove("tenant_overrides");
    let cfg = config(&json!({"openai": entry}));
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert_eq!(report.deferred, vec!["openai"]);
    assert!(report.ok.is_empty());
    // The other routes, including the chat route, are in place already.
    assert_eq!(fake.routes_for_alias("api.openai.com").len(), 5);

    let report = provision(&fake, &s2s(), &plan(&cfg.providers))
        .await
        .unwrap();
    assert_eq!(report.ok, vec!["openai"]);
    assert_eq!(fake.routes_for_alias("api.openai.com").len(), 6);
}

// ---------------------------------------------------------------------------
// Final review: entries sharing one OAGW alias
// ---------------------------------------------------------------------------

/// `openai` and `azure_openai` on the same mock listener under one alias,
/// with different credentials (one has none).
fn shared_mock_config() -> MiniChatConfig {
    config(&json!({
        "openai": {
            "kind": "openai_responses", "storage_kind": "openai",
            "host": "127.0.0.1", "port": 9000, "use_http": true, "upstream_alias": "mock",
            "auth_plugin_type": OAGW_APIKEY_AUTH_PLUGIN,
            "auth_config": {"header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key"}
        },
        "azure_openai": {
            "kind": "openai_responses", "storage_kind": "azure", "api_version": "2025-03-01-preview",
            "host": "127.0.0.1", "port": 9000, "use_http": true, "upstream_alias": "mock",
            "api_path": "/openai/deployments/{model}/responses?api-version=2025-03-01-preview"
        }
    }))
}

#[test]
fn plan_gives_specs_sharing_an_alias_the_first_entrys_upstream() {
    let specs = plan(&shared_mock_config().providers);
    assert_eq!(specs.len(), 2);
    // `azure_openai` sorts first and owns the upstream.
    assert_eq!(specs[0].label, "azure_openai");
    assert!(specs[0].auth.is_none());
    assert_eq!(specs[1].label, "openai");
    assert_eq!(specs[1].alias, "mock");
    assert_eq!(specs[1].endpoint, specs[0].endpoint);
    assert_eq!(specs[1].auth, specs[0].auth);
    // Each entry keeps its own routes.
    assert_eq!(specs[1].chat_route.path, "/v1/responses");
    assert_eq!(specs[0].chat_route.path, "/openai/deployments/");
}

#[tokio::test]
async fn shared_alias_provisions_one_upstream_without_flapping() {
    let fake = FakeOagw::new();
    let cfg = shared_mock_config();
    let report = provision_all(&fake, &s2s(), &cfg).await.unwrap();
    assert!(report.deferred.is_empty(), "{report:?}");
    assert_eq!(report.ok, vec!["azure_openai", "openai"]);

    let ups = fake.upstreams();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].alias, "mock");
    assert!(ups[0].auth.is_none(), "first entry's (absent) auth wins");
    assert_eq!(ups[0].server, server(Scheme::Http, "127.0.0.1", 9000));

    // Both entries' chat and RAG routes live on the shared upstream.
    let routes = route_set(&fake, "mock");
    let mut expected: BTreeSet<_> = rag_routes("/v1", &[]).into_iter().collect();
    expected.extend(rag_routes("/openai", &["api-version"]));
    expected.insert((
        "Post".into(),
        "/v1/responses".into(),
        "Disabled".into(),
        vec![],
    ));
    expected.insert((
        "Post".into(),
        "/openai/deployments/".into(),
        "Append".into(),
        vec!["api-version".into()],
    ));
    assert_eq!(routes, expected);

    // Repeated passes (restart / reconcile) never rewrite the shared upstream.
    assert_eq!(fake.upstream_updates(), 0);
    for _ in 0..3 {
        let report = provision(&fake, &s2s(), &plan(&cfg.providers))
            .await
            .unwrap();
        assert_eq!(report.ok.len(), 2);
    }
    assert_eq!(fake.upstream_updates(), 0);
    assert_eq!(fake.upstreams().len(), 1);
    assert!(fake.upstreams()[0].auth.is_none());
}
