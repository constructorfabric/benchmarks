use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, HttpMethod, ListQuery, PathSuffixMode, Route,
    Scheme, ServiceGatewayClientV1, SharingMode, UpdateRouteRequest, UpdateUpstreamRequest,
    Upstream,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderConfig, ProviderKind, StorageKind, TenantOverride};
use crate::testing::{ctx_a1, test_config};

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamErr;

fn already_exists(alias: &str) -> CanonicalError {
    UpstreamErr::already_exists("upstream exists")
        .with_resource(alias)
        .create()
}

fn secret_not_readable() -> CanonicalError {
    UpstreamErr::failed_precondition()
        .with_precondition_violation("auth.config.secret_ref", "secret not accessible", "STATE")
        .create()
}

fn validation() -> CanonicalError {
    UpstreamErr::invalid_argument()
        .with_field_violation("alias", "alias is auto-derived", "INVALID_ALIAS")
        .create()
}

/// In-memory OAGW management fake (alias uniqueness like OAGW, scripted create errors).
#[derive(Default)]
struct FakeGateway {
    upstreams: Mutex<Vec<Upstream>>,
    routes: Mutex<Vec<Route>>,
    create_upstream_calls: Mutex<Vec<CreateUpstreamRequest>>,
    /// Errors returned by `create_upstream`, keyed by endpoint host (popped per call).
    upstream_errors: Mutex<HashMap<String, VecDeque<CanonicalError>>>,
}

impl FakeGateway {
    fn fail_host(&self, host: &str, errs: Vec<CanonicalError>) {
        self.upstream_errors
            .lock()
            .unwrap()
            .insert(host.to_owned(), errs.into());
    }

    fn insert_upstream(&self, alias: &str) -> Upstream {
        let u = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: alias.to_owned(),
            server: Server { endpoints: vec![] },
            protocol: HTTP_PROTOCOL_ID.to_owned(),
            enabled: true,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
        };
        self.upstreams.lock().unwrap().push(u.clone());
        u
    }

    fn routes_of(&self, alias: &str) -> Vec<HttpMatch> {
        let id = self
            .upstreams
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.alias == alias)
            .map(|u| u.id)
            .unwrap();
        self.routes
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.upstream_id == id)
            .filter_map(|r| r.match_rules.http.clone())
            .collect()
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.create_upstream_calls.lock().unwrap().push(req.clone());
        let ep = req.server().endpoints[0].clone();
        if let Some(e) = self
            .upstream_errors
            .lock()
            .unwrap()
            .get_mut(&ep.host)
            .and_then(VecDeque::pop_front)
        {
            return Err(e);
        }
        let alias = req
            .alias()
            .map_or_else(|| ep.alias_contribution(), str::to_owned);
        if self
            .upstreams
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.alias == alias)
        {
            return Err(already_exists(&alias));
        }
        let mut u = self.insert_upstream(&alias);
        u.server = req.server().clone();
        u.auth = req.auth().cloned();
        let mut ups = self.upstreams.lock().unwrap();
        let last = ups.len() - 1;
        ups[last] = u.clone();
        Ok(u)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!()
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        q: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(self
            .upstreams
            .lock()
            .unwrap()
            .iter()
            .skip(q.skip as usize)
            .take(q.top as usize)
            .cloned()
            .collect())
    }

    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!()
    }

    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        unimplemented!()
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let r = Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: req.priority(),
            enabled: req.enabled(),
        };
        self.routes.lock().unwrap().push(r.clone());
        Ok(r)
    }

    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        unimplemented!()
    }

    async fn list_routes(
        &self,
        _: SecurityContext,
        upstream: Option<Uuid>,
        q: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Ok(self
            .routes
            .lock()
            .unwrap()
            .iter()
            .filter(|r| upstream.is_none_or(|u| r.upstream_id == u))
            .skip(q.skip as usize)
            .take(q.top as usize)
            .cloned()
            .collect())
    }

    async fn update_route(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!()
    }

    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        unimplemented!()
    }

    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unimplemented!()
    }

    async fn proxy_request(
        &self,
        _: SecurityContext,
        _: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        unimplemented!()
    }
}

fn entry(host: &str) -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::OpenaiResponses,
        host: host.to_owned(),
        port: None,
        use_http: false,
        upstream_alias: None,
        api_path: "/v1/responses".to_owned(),
        auth_plugin_type: None,
        auth_config: None,
        storage_kind: None,
        storage_backend: None,
        api_version: None,
        rag_provider: None,
        tenant_overrides: HashMap::new(),
    }
}

fn cfg_with(providers: Vec<(&str, ProviderConfig)>) -> MiniChatConfig {
    let mut cfg = test_config();
    cfg.providers = providers
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    cfg
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(5),
        max: Duration::from_millis(20),
        warn_after: Duration::from_millis(30),
    }
}

fn post(path: &str, query: &[&str]) -> HttpMatch {
    HttpMatch {
        methods: vec![HttpMethod::Post],
        path: path.to_owned(),
        query_allowlist: query.iter().map(|s| (*s).to_owned()).collect(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

// ───────────────────────────── spec building ─────────────────────────────

#[test]
fn chat_route_rules() {
    assert_eq!(chat_route("/v1/responses"), post("/v1/responses", &[]));
    assert_eq!(
        chat_route("/openai/v1/responses?api-version=preview"),
        post("/openai/v1/responses", &["api-version"])
    );
    assert_eq!(
        chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21&x=1"),
        post("/openai/deployments", &["api-version", "x"])
    );
    assert_eq!(
        chat_route("/v1/models/{model}:generate"),
        post("/v1/models", &[])
    );
}

#[test]
fn rag_route_rules() {
    let r = rag_routes(StorageKind::Openai);
    assert_eq!(r[0].path, "/v1/files");
    assert_eq!(r[0].methods, vec![HttpMethod::Post, HttpMethod::Delete]);
    assert_eq!(r[1].path, "/v1/vector_stores");
    assert_eq!(
        r[1].methods,
        vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete]
    );
    assert!(
        r.iter()
            .all(|m| m.query_allowlist.is_empty() && m.path_suffix_mode == PathSuffixMode::Append)
    );
    let r = rag_routes(StorageKind::Azure);
    assert_eq!(r[0].path, "/openai/files");
    assert_eq!(r[1].path, "/openai/vector_stores");
    assert!(
        r.iter()
            .all(|m| m.query_allowlist == vec!["api-version".to_owned()])
    );
}

#[test]
fn alias_rules() {
    let mut ip = entry("127.0.0.1");
    ip.port = Some(9000);
    ip.use_http = true;
    let mut local = entry("localhost");
    local.port = Some(9000);
    local.use_http = true;
    let mut custom = entry("my.host");
    custom.upstream_alias = Some("custom-alias".into());
    let specs = build_specs(&cfg_with(vec![
        ("a_ip", ip),
        ("b_local", local),
        ("c_std", entry("api.openai.com")),
        ("d_custom", custom),
    ]));
    let by = |l: &str| specs.iter().find(|s| s.label == l).unwrap().clone();

    let s = by("a_ip");
    assert_eq!((s.alias.as_str(), s.send_alias), ("127.0.0.1", true));
    assert_eq!(
        s.endpoint,
        Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".into(),
            port: 9000
        }
    );
    let s = by("b_local");
    assert_eq!((s.alias.as_str(), s.send_alias), ("localhost:9000", false));
    let s = by("c_std");
    assert_eq!((s.alias.as_str(), s.send_alias), ("api.openai.com", false));
    assert_eq!(
        s.endpoint,
        Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".into(),
            port: 443
        }
    );
    let s = by("d_custom");
    assert_eq!((s.alias.as_str(), s.send_alias), ("custom-alias", true));
}

#[test]
fn spec_auth_routes_and_overrides() {
    let mut p = entry("res.openai.azure.com");
    p.api_path = "/openai/v1/responses?api-version=preview".into();
    p.storage_kind = Some(StorageKind::Azure);
    p.api_version = Some("2025-04-01-preview".into());
    p.auth_plugin_type = Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".into());
    p.auth_config = Some(
        [("header".to_owned(), "api-key".to_owned())]
            .into_iter()
            .collect(),
    );
    p.tenant_overrides.insert(
        "11111111-1111-1111-1111-111111111111".into(),
        TenantOverride {
            host: Some("other.openai.azure.com".into()),
            ..TenantOverride::default()
        },
    );
    let specs = build_specs(&cfg_with(vec![("azure", p)]));
    assert_eq!(specs.len(), 2);
    let base = &specs[0];
    assert_eq!(base.alias, "res.openai.azure.com");
    let auth = base.auth.as_ref().unwrap();
    assert_eq!(auth.sharing, SharingMode::Private);
    assert_eq!(auth.config.as_ref().unwrap()["header"], "api-key");
    assert_eq!(base.routes.len(), 3);
    assert_eq!(
        base.routes[0],
        post("/openai/v1/responses", &["api-version"])
    );
    let ov = &specs[1];
    assert_eq!(ov.alias, "other.openai.azure.com");
    assert!(ov.label.contains("tenant"));
    assert_eq!(ov.auth, base.auth, "override inherits the entry's auth");
    assert_eq!(ov.routes, base.routes);
}

#[test]
fn entries_sharing_an_alias_are_merged() {
    let mut a = entry("api.openai.com");
    a.storage_kind = Some(StorageKind::Openai);
    let mut b = entry("api.openai.com");
    b.kind = ProviderKind::OpenaiChatCompletions;
    b.api_path = "/v1/chat/completions".into();
    let specs = build_specs(&cfg_with(vec![("a", a), ("b", b)]));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].routes.len(), 4);
}

// ───────────────────────────── provisioning ─────────────────────────────

#[tokio::test]
async fn provisions_upstreams_and_routes() {
    let gw = FakeGateway::default();
    let mut p = entry("127.0.0.1");
    p.port = Some(8080);
    p.use_http = true;
    p.storage_kind = Some(StorageKind::Openai);
    let specs = build_specs(&cfg_with(vec![("mock", p)]));
    let report = provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.ready, vec!["mock".to_owned()]);
    let calls = gw.create_upstream_calls.lock().unwrap().clone();
    assert_eq!(calls[0].alias(), Some("127.0.0.1"));
    assert_eq!(calls[0].protocol(), HTTP_PROTOCOL_ID);
    assert!(calls[0].auth().is_none());
    let routes = gw.routes_of("127.0.0.1");
    assert_eq!(routes.len(), 3);
    assert_eq!(routes[0], post("/v1/responses", &[]));
}

#[tokio::test]
async fn hostname_alias_is_not_sent() {
    let gw = FakeGateway::default();
    let mut p = entry("localhost");
    p.port = Some(9999);
    let specs = build_specs(&cfg_with(vec![("h", p)]));
    provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    let calls = gw.create_upstream_calls.lock().unwrap().clone();
    assert_eq!(calls[0].alias(), None);
    assert_eq!(gw.upstreams.lock().unwrap()[0].alias, "localhost:9999");
}

#[tokio::test]
async fn existing_upstream_is_reused_without_duplicate_routes() {
    let gw = FakeGateway::default();
    let mut p = entry("api.openai.com");
    p.storage_kind = Some(StorageKind::Openai);
    let specs = build_specs(&cfg_with(vec![("openai", p)]));
    // First run creates everything; a second run (e.g. after restart of the gear) reuses it.
    provision_all(
        &gw,
        &ctx_a1(),
        specs.clone(),
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    let report = provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.ready, vec!["openai".to_owned()]);
    assert_eq!(gw.upstreams.lock().unwrap().len(), 1);
    assert_eq!(gw.routes.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn pre_existing_upstream_gets_missing_routes() {
    let gw = FakeGateway::default();
    gw.insert_upstream("api.openai.com");
    let specs = build_specs(&cfg_with(vec![("openai", entry("api.openai.com"))]));
    let report = provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.ready.len(), 1);
    assert_eq!(
        gw.routes_of("api.openai.com"),
        vec![post("/v1/responses", &[])]
    );
}

#[tokio::test]
async fn secret_precondition_is_retried_until_success() {
    let gw = FakeGateway::default();
    gw.fail_host(
        "api.openai.com",
        vec![secret_not_readable(), secret_not_readable()],
    );
    let specs = build_specs(&cfg_with(vec![("openai", entry("api.openai.com"))]));
    let report = provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.ready, vec!["openai".to_owned()]);
    assert_eq!(gw.create_upstream_calls.lock().unwrap().len(), 3);
    assert_eq!(gw.routes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn deterministic_error_is_not_retried_and_others_continue() {
    let gw = FakeGateway::default();
    gw.fail_host("bad.host", vec![validation()]);
    let specs = build_specs(&cfg_with(vec![
        ("a_bad", entry("bad.host")),
        ("b_good", entry("good.host")),
    ]));
    let report = provision_all(
        &gw,
        &ctx_a1(),
        specs,
        fast_policy(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.failed, vec!["a_bad".to_owned()]);
    assert_eq!(report.ready, vec!["b_good".to_owned()]);
    assert_eq!(gw.create_upstream_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pending_entries_warn_and_stop_on_cancel() {
    let gw = FakeGateway::default();
    gw.fail_host(
        "api.openai.com",
        (0..1000).map(|_| secret_not_readable()).collect(),
    );
    let specs = build_specs(&cfg_with(vec![("openai", entry("api.openai.com"))]));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });
    let report = provision_all(&gw, &ctx_a1(), specs, fast_policy(), &cancel).await;
    assert_eq!(report.pending, vec!["openai".to_owned()]);
    assert!(report.warned);
    let calls = gw.create_upstream_calls.lock().unwrap().len();
    // 5, 10, 20, 20, ... ms → several retries but bounded by the 20 ms cap.
    assert!((3..=40).contains(&calls), "calls = {calls}");
}

#[tokio::test]
async fn default_retry_policy_matches_design() {
    let p = RetryPolicy::default();
    assert_eq!(p.initial, Duration::from_secs(2));
    assert_eq!(p.max, Duration::from_secs(60));
    assert_eq!(p.warn_after, Duration::from_secs(120));
}
