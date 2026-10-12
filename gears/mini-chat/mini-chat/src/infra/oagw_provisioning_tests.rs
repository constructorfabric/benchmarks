#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, ListQuery, Route,
    Scheme, Server, ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{chat_route_match, provision, targets_of};
use crate::config::{ProviderEntry, StorageKind, TenantOverride};
use crate::test_support::{FakeOagw, FakeProvider};

#[resource_error(gts_id!("cf.core.mini_chat.test_oagw.v1~"))]
struct StubError;

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .unwrap()
}

fn providers(entry: ProviderEntry) -> HashMap<String, ProviderEntry> {
    HashMap::from([("openai".to_owned(), entry)])
}

#[test]
fn the_model_placeholder_and_everything_after_it_is_a_path_suffix() {
    assert_eq!(
        chat_route_match("/v1/responses"),
        ("/v1/responses".to_owned(), vec![])
    );
    assert_eq!(
        chat_route_match("/deployments/{model}/responses"),
        ("/deployments".to_owned(), vec![])
    );
    assert_eq!(
        chat_route_match("/openai/v1/responses?api-version=2025-03-01-preview"),
        (
            "/openai/v1/responses".to_owned(),
            vec!["api-version".to_owned()]
        )
    );
    assert_eq!(chat_route_match("/{model}"), ("/".to_owned(), vec![]));
}

#[test]
fn every_provider_and_every_tenant_override_with_its_own_endpoint_is_a_target() {
    let mut entry = ProviderEntry::default_openai();
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            host: Some("tenant.example".to_owned()),
            ..TenantOverride::default()
        },
    );
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            auth_plugin_type: Some("x".to_owned()),
            ..TenantOverride::default()
        },
    );

    let targets = targets_of(&providers(entry));

    assert_eq!(
        targets.len(),
        2,
        "an override without host or alias shares the entry's upstream"
    );
    assert_eq!(targets[0].alias, "api.openai.com");
    assert_eq!(targets[1].alias, "tenant.example");
}

#[tokio::test]
async fn provisioning_creates_the_upstream_the_chat_route_and_the_rag_routes() {
    let gateway = FakeOagw::new(FakeProvider::default());
    let mut entry = ProviderEntry::default_openai();
    entry.host = "127.0.0.1".to_owned();
    entry.port = Some(9000);
    entry.use_http = true;
    entry.storage_kind = StorageKind::Openai;

    let pending = provision(&gateway, &ctx(), targets_of(&providers(entry)))
        .await
        .unwrap();

    assert!(pending.is_empty());
    let upstreams = gateway
        .list_upstreams(ctx(), &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(upstreams.len(), 1);
    assert_eq!(upstreams[0].alias, "127.0.0.1:9000");
    let routes = gateway
        .list_routes(ctx(), None, &ListQuery::default())
        .await
        .unwrap();
    let paths: Vec<String> = routes
        .iter()
        .filter_map(|route| {
            route
                .match_rules
                .http
                .as_ref()
                .map(|http| http.path.clone())
        })
        .collect();
    assert_eq!(routes.len(), 3);
    for expected in ["/v1/responses", "/v1/files", "/v1/vector_stores"] {
        assert!(paths.iter().any(|path| path == expected), "{paths:?}");
    }
}

/// Fails `create_upstream` with `error` for the first `failures` calls, then delegates.
struct Flaky {
    inner: FakeOagw,
    failures: AtomicU32,
    error: fn() -> CanonicalError,
}

#[async_trait]
impl ServiceGatewayClientV1 for Flaky {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        if self.failures.load(Ordering::SeqCst) > 0 {
            self.failures.fetch_sub(1, Ordering::SeqCst);
            return Err((self.error)());
        }
        self.inner.create_upstream(ctx, req).await
    }

    async fn get_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.inner.get_upstream(ctx, id).await
    }

    async fn list_upstreams(
        &self,
        ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        self.inner.list_upstreams(ctx, query).await
    }

    async fn update_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.inner.update_upstream(ctx, id, req).await
    }

    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.inner.delete_upstream(ctx, id).await
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.inner.create_route(ctx, req).await
    }

    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.inner.get_route(ctx, id).await
    }

    async fn list_routes(
        &self,
        ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        self.inner.list_routes(ctx, upstream_id, query).await
    }

    async fn update_route(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.inner.update_route(ctx, id, req).await
    }

    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.inner.delete_route(ctx, id).await
    }

    async fn resolve_proxy_target(
        &self,
        ctx: SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        self.inner
            .resolve_proxy_target(ctx, alias, method, path)
            .await
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        self.inner.proxy_request(ctx, req).await
    }
}

fn flaky(failures: u32, error: fn() -> CanonicalError) -> Flaky {
    Flaky {
        inner: FakeOagw::new(FakeProvider::default()),
        failures: AtomicU32::new(failures),
        error,
    }
}

fn secret_not_ready() -> CanonicalError {
    StubError::failed_precondition()
        .with_precondition_violation("auth.config.secret_ref", "not provisioned yet", "STATE")
        .create()
}

fn bad_config() -> CanonicalError {
    StubError::invalid_argument()
        .with_field_violation("alias", "alias is invalid", "INVALID_VALUE")
        .create()
}

#[tokio::test]
async fn an_unreadable_credential_defers_the_provider_instead_of_failing_startup() {
    let gateway = flaky(1, secret_not_ready);

    let pending = provision(
        &gateway,
        &ctx(),
        targets_of(&providers(ProviderEntry::default_openai())),
    )
    .await
    .unwrap();

    assert_eq!(pending.len(), 1);
    let retried = provision(&gateway, &ctx(), pending).await.unwrap();
    assert!(retried.is_empty(), "the retry provisions the provider");
}

#[tokio::test]
async fn a_deterministic_misconfiguration_fails_startup() {
    let gateway = flaky(1, bad_config);

    let result = provision(
        &gateway,
        &ctx(),
        targets_of(&providers(ProviderEntry::default_openai())),
    )
    .await;

    let message = result.unwrap_err().to_string();
    assert!(message.contains("openai"), "{message}");
}

#[tokio::test]
async fn an_existing_upstream_is_reused() {
    let gateway = FakeOagw::new(FakeProvider::default());
    let targets = targets_of(&providers(ProviderEntry::default_openai()));

    provision(&gateway, &ctx(), targets.clone()).await.unwrap();
    // OAGW answers the duplicate alias with already-exists; the upstream is found and reused.
    let pending = provision(&gateway, &ctx(), targets).await.unwrap();

    assert!(pending.is_empty());
}

fn entry_at(host: &str, port: Option<u16>, use_http: bool, alias: Option<&str>) -> ProviderEntry {
    let mut entry = ProviderEntry::default_openai();
    entry.host = host.to_owned();
    entry.port = port;
    entry.use_http = use_http;
    entry.upstream_alias = alias.map(str::to_owned);
    entry
}

/// Provisions the entry against the fake that applies OAGW's alias rules and returns the aliases of
/// the registered upstreams; the routing alias of every target must be one of them.
async fn provisioned_aliases(entry: ProviderEntry) -> Vec<String> {
    let gateway = FakeOagw::new(FakeProvider::default());
    let ctx = ctx();
    let targets = targets_of(&providers(entry));
    let routing: Vec<String> = targets.iter().map(|t| t.alias.clone()).collect();

    let pending = provision(&gateway, &ctx, targets).await.unwrap();

    assert!(pending.is_empty());
    let mut aliases: Vec<String> = gateway
        .list_upstreams(ctx, &ListQuery::default())
        .await
        .unwrap()
        .into_iter()
        .map(|upstream| upstream.alias)
        .collect();
    aliases.sort();
    for alias in routing {
        assert!(aliases.contains(&alias), "{alias} not in {aliases:?}");
    }
    aliases
}

#[tokio::test]
async fn a_hostname_without_an_alias_provisions_on_any_port_and_scheme() {
    for (port, use_http, expected) in [
        (None, false, "llm.test"),
        (None, true, "llm.test"),
        (Some(8443), false, "llm.test:8443"),
        (Some(8080), true, "llm.test:8080"),
        (Some(443), true, "llm.test:443"),
        (Some(80), false, "llm.test:80"),
    ] {
        let aliases = provisioned_aliases(entry_at("llm.test", port, use_http, None)).await;
        assert_eq!(aliases, [expected], "port {port:?} http {use_http}");
    }
}

#[tokio::test]
async fn a_configured_alias_on_a_hostname_is_not_sent_and_requests_route_by_the_derived_alias() {
    let aliases = provisioned_aliases(entry_at("LLM.Test.", None, false, Some("custom"))).await;
    assert_eq!(aliases, ["llm.test"]);
    let aliases = provisioned_aliases(entry_at("llm.test", Some(8443), true, Some("custom"))).await;
    assert_eq!(aliases, ["llm.test:8443"]);
}

#[tokio::test]
async fn an_ip_host_registers_an_explicit_alias_with_the_port_unless_it_is_standard() {
    for (port, use_http, expected) in [
        (None, true, "127.0.0.1"),
        (None, false, "127.0.0.1"),
        (Some(80), true, "127.0.0.1"),
        (Some(443), false, "127.0.0.1"),
        (Some(443), true, "127.0.0.1:443"),
        (Some(80), false, "127.0.0.1:80"),
        (Some(9000), true, "127.0.0.1:9000"),
        (Some(9000), false, "127.0.0.1:9000"),
    ] {
        let aliases = provisioned_aliases(entry_at("127.0.0.1", port, use_http, None)).await;
        assert_eq!(aliases, [expected], "port {port:?} http {use_http}");
    }
    let aliases = provisioned_aliases(entry_at("127.0.0.1", Some(9000), true, Some("Mock"))).await;
    assert_eq!(aliases, ["mock"]);
}

#[tokio::test]
async fn tenant_overrides_provision_with_their_own_alias() {
    let mut entry = entry_at("127.0.0.1", Some(9000), true, Some("shared"));
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            upstream_alias: Some("tenant-alias".to_owned()),
            ..TenantOverride::default()
        },
    );
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            host: Some("tenant.example".to_owned()),
            ..TenantOverride::default()
        },
    );

    let aliases = provisioned_aliases(entry).await;

    assert_eq!(aliases, ["shared", "tenant-alias", "tenant.example:9000"]);
}

#[tokio::test]
async fn an_existing_upstream_is_found_by_its_normalized_alias() {
    let gateway = FakeOagw::new(FakeProvider::default());
    let ctx = ctx();
    let targets = targets_of(&providers(entry_at(
        "llm.test",
        None,
        false,
        Some("custom"),
    )));

    provision(&gateway, &ctx, targets.clone()).await.unwrap();
    // The duplicate alias is answered with already-exists and the upstream is found again.
    let pending = provision(&gateway, &ctx, targets).await.unwrap();

    assert!(pending.is_empty());
    let upstreams = gateway
        .list_upstreams(ctx, &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(upstreams.len(), 1);
}

fn auth_config(secret: &str) -> HashMap<String, String> {
    HashMap::from([("secret_ref".to_owned(), secret.to_owned())])
}

fn two_entries(a: ProviderEntry, b: ProviderEntry) -> HashMap<String, ProviderEntry> {
    HashMap::from([("a".to_owned(), a), ("b".to_owned(), b)])
}

#[tokio::test]
async fn a_hostname_override_with_only_an_alias_and_another_credential_fails_startup() {
    let mut entry = entry_at("llm.test", None, false, None);
    entry.auth_config = Some(auth_config("cred://shared"));
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            upstream_alias: Some("tenant-alias".to_owned()),
            auth_config: Some(auth_config("cred://tenant")),
            ..TenantOverride::default()
        },
    );
    let gateway = FakeOagw::new(FakeProvider::default());

    let result = provision(&gateway, &ctx(), targets_of(&providers(entry))).await;

    let message = result.unwrap_err().to_string();
    assert!(message.contains("same OAGW alias `llm.test`"), "{message}");
    assert!(message.contains("openai (tenant "), "{message}");
    assert!(message.contains("different host"), "{message}");
    let upstreams = gateway
        .list_upstreams(ctx(), &ListQuery::default())
        .await
        .unwrap();
    assert!(
        upstreams.is_empty(),
        "nothing is provisioned on a collision"
    );
}

#[tokio::test]
async fn a_hostname_override_with_a_different_host_gets_its_own_upstream_and_credential() {
    let mut entry = entry_at("llm.test", None, false, None);
    entry.auth_config = Some(auth_config("cred://shared"));
    entry.tenant_overrides.insert(
        Uuid::new_v4().to_string(),
        TenantOverride {
            host: Some("tenant.test".to_owned()),
            auth_config: Some(auth_config("cred://tenant")),
            ..TenantOverride::default()
        },
    );
    let gateway = FakeOagw::new(FakeProvider::default());

    let pending = provision(&gateway, &ctx(), targets_of(&providers(entry)))
        .await
        .unwrap();

    assert!(pending.is_empty());
    let mut upstreams = gateway
        .list_upstreams(ctx(), &ListQuery::default())
        .await
        .unwrap();
    upstreams.sort_by(|a, b| a.alias.cmp(&b.alias));
    let secrets: Vec<(String, Option<String>)> = upstreams
        .into_iter()
        .map(|upstream| {
            let secret = upstream
                .auth
                .and_then(|auth| auth.config)
                .and_then(|config| config.get("secret_ref").cloned());
            (upstream.alias, secret)
        })
        .collect();
    assert_eq!(
        secrets,
        [
            ("llm.test".to_owned(), Some("cred://shared".to_owned())),
            ("tenant.test".to_owned(), Some("cred://tenant".to_owned())),
        ]
    );
}

#[tokio::test]
async fn two_ip_providers_on_one_address_get_distinct_upstreams_and_requests_reach_each() {
    let provider = FakeProvider::default();
    let gateway = FakeOagw::new(provider.clone());
    let ctx = ctx();
    let providers = two_entries(
        entry_at("127.0.0.1", Some(18099), true, None),
        entry_at("127.0.0.1", Some(18100), true, None),
    );
    let targets = targets_of(&providers);
    assert_eq!(targets[0].alias, "127.0.0.1:18099");
    assert_eq!(targets[1].alias, "127.0.0.1:18100");

    let pending = provision(&gateway, &ctx, targets.clone()).await.unwrap();

    assert!(pending.is_empty());
    let mut aliases: Vec<String> = gateway
        .list_upstreams(ctx.clone(), &ListQuery::default())
        .await
        .unwrap()
        .into_iter()
        .map(|upstream| upstream.alias)
        .collect();
    aliases.sort();
    assert_eq!(aliases, ["127.0.0.1:18099", "127.0.0.1:18100"]);
    for target in &targets {
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("/{}/v1/responses", target.alias))
            .body(Body::Empty)
            .unwrap();
        gateway.proxy_request(ctx.clone(), request).await.unwrap();
    }
    let ports: Vec<u16> = provider
        .requests()
        .into_iter()
        .map(|request| request.upstream.unwrap().server.endpoints[0].port)
        .collect();
    assert_eq!(ports, [18099, 18100]);
}

#[tokio::test]
async fn an_ip_provider_with_a_configured_alias_routes_by_that_alias() {
    let providers = two_entries(
        entry_at("127.0.0.1", Some(18099), true, Some("Mock-A.")),
        entry_at("127.0.0.1", Some(18100), true, Some("mock-b")),
    );
    let gateway = FakeOagw::new(FakeProvider::default());
    let targets = targets_of(&providers);

    provision(&gateway, &ctx(), targets.clone()).await.unwrap();

    assert_eq!(targets[0].alias, "mock-a");
    assert_eq!(targets[1].alias, "mock-b");
    let upstreams = gateway
        .list_upstreams(ctx(), &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(upstreams.len(), 2);
}

#[tokio::test]
async fn identical_targets_share_one_upstream_without_an_error() {
    let gateway = FakeOagw::new(FakeProvider::default());
    let mut second = ProviderEntry::default_openai();
    second.api_path = "/v1/other".to_owned();
    let providers = two_entries(ProviderEntry::default_openai(), second);

    let pending = provision(&gateway, &ctx(), targets_of(&providers))
        .await
        .unwrap();

    assert!(pending.is_empty());
    let upstreams = gateway
        .list_upstreams(ctx(), &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(upstreams.len(), 1);
    let routes = gateway
        .list_routes(ctx(), None, &ListQuery::default())
        .await
        .unwrap();
    assert!(
        routes.iter().any(|route| route
            .match_rules
            .http
            .as_ref()
            .is_some_and(|http| http.path == "/v1/other")),
        "the second target's chat route is provisioned on the shared upstream"
    );
}

#[tokio::test]
async fn two_hostname_providers_with_different_credentials_on_one_host_fail_startup() {
    let mut other = ProviderEntry::default_openai();
    other.auth_config = Some(auth_config("cred://other"));
    let providers = two_entries(ProviderEntry::default_openai(), other);
    let gateway = FakeOagw::new(FakeProvider::default());

    let result = provision(&gateway, &ctx(), targets_of(&providers)).await;

    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("`a`") && message.contains("`b`"),
        "{message}"
    );
}

#[tokio::test]
async fn an_existing_upstream_with_another_endpoint_is_not_reused() {
    let gateway = FakeOagw::new(FakeProvider::default());
    let ctx = ctx();
    gateway
        .create_upstream(
            ctx.clone(),
            CreateUpstreamRequest::builder(
                Server {
                    endpoints: vec![Endpoint {
                        scheme: Scheme::Http,
                        host: "127.0.0.1".to_owned(),
                        port: 9001,
                    }],
                },
                HTTP_PROTOCOL_ID,
            )
            .alias("127.0.0.1:9000")
            .build(),
        )
        .await
        .unwrap();

    let result = provision(
        &gateway,
        &ctx,
        targets_of(&providers(entry_at("127.0.0.1", Some(9000), true, None))),
    )
    .await;

    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("already exists with the endpoint"),
        "{message}"
    );
}
