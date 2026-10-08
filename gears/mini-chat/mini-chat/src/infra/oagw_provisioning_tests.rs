#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use oagw_sdk::{
    APIKEY_AUTH_PLUGIN_ID, Body, CreateRouteRequest, CreateUpstreamRequest, HTTP_PROTOCOL_ID,
    HttpMatch, HttpMethod, ListQuery, PathSuffixMode, Route, Scheme, SharingMode,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::{Value, json};
use toolkit_canonical_errors::resource_error;
use uuid::Uuid;

use super::*;
use crate::test_support::test_ctx;

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamErr;

#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
struct RouteErr;

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";

// ── Fake gateway ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeGw {
    /// Every `create_upstream` call, including failed ones.
    upstream_calls: Mutex<Vec<CreateUpstreamRequest>>,
    /// Upstreams that exist (created or pre-seeded); returned by `list_upstreams`.
    upstreams: Mutex<Vec<Upstream>>,
    /// Successfully created routes.
    routes: Mutex<Vec<CreateRouteRequest>>,
    /// Scripted errors per alias, consumed in order.
    upstream_errors: Mutex<HashMap<String, VecDeque<CanonicalError>>>,
    /// Scripted error per route path (every attempt).
    route_errors: Mutex<HashMap<String, CanonicalError>>,
}

impl FakeGw {
    fn fail_upstream(&self, alias: &str, err: CanonicalError) {
        self.upstream_errors
            .lock()
            .unwrap()
            .entry(alias.to_owned())
            .or_default()
            .push_back(err);
    }

    fn fail_route(&self, path: &str, err: CanonicalError) {
        self.route_errors
            .lock()
            .unwrap()
            .insert(path.to_owned(), err);
    }

    fn seed_upstream(&self, alias: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.upstreams.lock().unwrap().push(upstream(id, alias));
        id
    }

    fn calls_for(&self, alias: &str) -> usize {
        self.upstream_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.alias() == Some(alias))
            .count()
    }

    fn created(&self, alias: &str) -> CreateUpstreamRequest {
        self.upstream_calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.alias() == Some(alias))
            .cloned()
            .unwrap_or_else(|| panic!("no create_upstream for {alias}"))
    }

    fn upstream_id(&self, alias: &str) -> Uuid {
        self.upstreams
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.alias == alias)
            .map_or_else(|| panic!("no upstream {alias}"), |u| u.id)
    }

    /// `(methods, path, query_allowlist, suffix mode)` of the routes on `upstream_id`.
    fn routes_of(&self, upstream_id: Uuid) -> Vec<HttpMatch> {
        let mut out: Vec<HttpMatch> = self
            .routes
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.upstream_id() == upstream_id)
            .map(|r| r.match_rules().http.clone().unwrap())
            .collect();
        out.sort_by(|a, b| {
            (a.path.clone(), format!("{:?}", a.methods))
                .cmp(&(b.path.clone(), format!("{:?}", b.methods)))
        });
        out
    }
}

fn upstream(id: Uuid, alias: &str) -> Upstream {
    Upstream {
        id,
        tenant_id: Uuid::nil(),
        alias: alias.to_owned(),
        server: oagw_sdk::Server { endpoints: vec![] },
        protocol: HTTP_PROTOCOL_ID.to_owned(),
        enabled: true,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: vec![],
    }
}

fn unused() -> CanonicalError {
    CanonicalError::internal("not used by provisioning").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGw {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.upstream_calls.lock().unwrap().push(req.clone());
        let alias = req.alias().unwrap_or_default().to_owned();
        if let Some(err) = self
            .upstream_errors
            .lock()
            .unwrap()
            .get_mut(&alias)
            .and_then(VecDeque::pop_front)
        {
            return Err(err);
        }
        let mut u = upstream(Uuid::new_v4(), &alias);
        u.server = req.server().clone();
        u.auth = req.auth().cloned();
        self.upstreams.lock().unwrap().push(u.clone());
        Ok(u)
    }

    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        let mut all = self.upstreams.lock().unwrap().clone();
        all.sort_by_key(|u| u.id);
        Ok(all
            .into_iter()
            .skip(query.skip as usize)
            .take(query.top as usize)
            .collect())
    }

    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }

    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let http = req.match_rules().http.clone().unwrap();
        if let Some(err) = self.route_errors.lock().unwrap().get(&http.path) {
            return Err(err.clone());
        }
        self.routes.lock().unwrap().push(req.clone());
        Ok(Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: 0,
            enabled: true,
        })
    }

    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        Err(unused())
    }

    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<Uuid>,
        _: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Err(unused())
    }

    async fn update_route(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused())
    }

    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }

    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unused())
    }

    async fn proxy_request(
        &self,
        _: SecurityContext,
        _: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        Err(unused())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn cfg(providers: &Value) -> MiniChatConfig {
    let mut cfg: MiniChatConfig = serde_json::from_value(json!({
        "client_credentials": {"client_id": "id", "client_secret": "secret"},
        "providers": providers
    }))
    .unwrap();
    cfg.fill_upstream_aliases();
    cfg.validate().unwrap();
    cfg
}

fn setup(providers: &Value) -> (Arc<FakeGw>, OagwProvisioner) {
    let gw = Arc::new(FakeGw::default());
    let p = OagwProvisioner::new(gw.clone(), &cfg(providers));
    (gw, p)
}

fn openai_entry() -> Value {
    json!({
        "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai",
        "auth_plugin_type": APIKEY_AUTH_PLUGIN_ID,
        "auth_config": {"header": "Authorization", "prefix": "Bearer ",
                        "secret_ref": "cred://openai-key"}
    })
}

fn mock_entry(alias: &str) -> Value {
    json!({
        "kind": "openai_responses", "host": "127.0.0.1", "port": 9999, "use_http": true,
        "upstream_alias": alias, "storage_kind": "openai"
    })
}

fn route(methods: Vec<HttpMethod>, path: &str, allow: &[&str]) -> HttpMatch {
    HttpMatch {
        methods,
        path: path.to_owned(),
        query_allowlist: allow.iter().map(|s| (*s).to_owned()).collect(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

/// Chat route + the five RAG routes under `prefix`, sorted like `routes_of`.
fn expected_routes(chat: HttpMatch, prefix: &str, allow: &[&str]) -> Vec<HttpMatch> {
    let mut v = vec![
        chat,
        route(vec![HttpMethod::Post], &format!("{prefix}/files"), allow),
        route(vec![HttpMethod::Delete], &format!("{prefix}/files/"), allow),
        route(
            vec![HttpMethod::Post],
            &format!("{prefix}/vector_stores"),
            allow,
        ),
        route(
            vec![HttpMethod::Delete],
            &format!("{prefix}/vector_stores/"),
            allow,
        ),
        route(
            vec![HttpMethod::Get],
            &format!("{prefix}/vector_stores/"),
            allow,
        ),
    ];
    v.sort_by(|a, b| {
        (a.path.clone(), format!("{:?}", a.methods))
            .cmp(&(b.path.clone(), format!("{:?}", b.methods)))
    });
    v
}

fn secret_not_readable() -> CanonicalError {
    UpstreamErr::failed_precondition()
        .with_precondition_violation("auth.config.secret_ref", "secret not readable", "STATE")
        .create()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn provisions_upstream_and_chat_route_per_entry() {
    let (gw, p) = setup(&json!({"openai": openai_entry(), "mock": mock_entry("mock-openai")}));
    let report = p.provision_all(&test_ctx()).await.unwrap();
    assert!(report.pending.is_empty());
    assert_eq!(gw.upstream_calls.lock().unwrap().len(), 2);

    let oa = gw.created("api.openai.com");
    assert_eq!(oa.protocol(), HTTP_PROTOCOL_ID);
    let ep = &oa.server().endpoints[0];
    assert_eq!(
        (ep.scheme, ep.host.as_str(), ep.port),
        (Scheme::Https, "api.openai.com", 443)
    );
    let auth = oa.auth().expect("auth plugin");
    assert_eq!(auth.plugin_type, APIKEY_AUTH_PLUGIN_ID);
    assert_eq!(auth.sharing, SharingMode::Private);
    assert_eq!(
        auth.config.as_ref().unwrap()["secret_ref"],
        "cred://openai-key"
    );

    let mock = gw.created("mock-openai");
    let ep = &mock.server().endpoints[0];
    assert_eq!(
        (ep.scheme, ep.host.as_str(), ep.port),
        (Scheme::Http, "127.0.0.1", 9999)
    );
    assert!(
        mock.auth().is_none(),
        "no auth_plugin_type -> no auth plugin"
    );

    let chat = route(vec![HttpMethod::Post], "/v1/responses", &[]);
    for alias in ["api.openai.com", "mock-openai"] {
        assert_eq!(
            gw.routes_of(gw.upstream_id(alias)),
            expected_routes(chat.clone(), "/v1", &[]),
            "routes of {alias}"
        );
    }
}

#[tokio::test]
async fn chat_route_strips_query_and_model_placeholder() {
    let (gw, p) = setup(&json!({
        "cc": {"kind": "openai_chat_completions", "host": "cc.example.com",
               "api_path": "/openai/deployments/{model}/chat/completions?api-version=2024-10-21&x=1",
               "storage_kind": "openai"},
        "claude": {"kind": "anthropic_messages", "host": "api.anthropic.com",
                   "api_path": "/v1/messages", "rag_provider": "cc"}
    }));
    p.provision_all(&test_ctx()).await.unwrap();
    let cc = gw.routes_of(gw.upstream_id("cc.example.com"));
    assert!(cc.contains(&route(
        vec![HttpMethod::Post],
        "/openai/deployments/",
        &["api-version", "x"]
    )));
    // An entry without storage_kind (redirected via rag_provider) gets only its chat route.
    assert_eq!(
        gw.routes_of(gw.upstream_id("api.anthropic.com")),
        vec![route(vec![HttpMethod::Post], "/v1/messages", &[])]
    );
}

#[tokio::test]
async fn azure_rag_routes_allow_api_version() {
    let (gw, p) = setup(&json!({"azure": {
        "kind": "openai_responses", "host": "127.0.0.1", "port": 9999, "use_http": true,
        "upstream_alias": "mock-azure", "storage_kind": "azure",
        "api_path": "/openai/v1/responses?api-version=2025-03-01-preview",
        "api_version": "2025-03-01-preview"
    }}));
    p.provision_all(&test_ctx()).await.unwrap();
    assert_eq!(
        gw.routes_of(gw.upstream_id("mock-azure")),
        expected_routes(
            route(
                vec![HttpMethod::Post],
                "/openai/v1/responses",
                &["api-version"]
            ),
            "/openai",
            &["api-version"]
        )
    );
}

#[tokio::test]
async fn already_exists_is_reused() {
    let (gw, p) = setup(&json!({"mock": mock_entry("mock-openai")}));
    let existing = gw.seed_upstream("mock-openai");
    // A few unrelated upstreams so the lookup has to search the list.
    for i in 0..3 {
        gw.seed_upstream(&format!("other-{i}"));
    }
    gw.fail_upstream(
        "mock-openai",
        UpstreamErr::already_exists("alias 'mock-openai' already exists for tenant")
            .with_resource("mock-openai")
            .create(),
    );
    gw.fail_route(
        "/v1/responses",
        RouteErr::already_exists("overlap")
            .with_resource(format!("{existing}:/v1/responses:Post"))
            .create(),
    );
    let report = p.provision_all(&test_ctx()).await.unwrap();
    assert!(report.pending.is_empty());
    let routes = gw.routes_of(existing);
    // Chat route already existed (409 = fine); the RAG routes are added to the reused upstream.
    assert_eq!(routes.len(), 5);
    assert!(routes.iter().all(|r| r.path.starts_with("/v1/")));
}

#[tokio::test]
async fn secret_not_ready_is_pending_not_fatal() {
    let (gw, p) = setup(&json!({"openai": openai_entry(), "mock": mock_entry("mock-openai")}));
    gw.fail_upstream("api.openai.com", secret_not_readable());
    let ctx = test_ctx();
    let report = p.provision_all(&ctx).await.unwrap();
    assert_eq!(report.pending, vec!["openai".to_owned()]);
    // The other entry is fully provisioned.
    assert_eq!(gw.routes_of(gw.upstream_id("mock-openai")).len(), 6);

    // Next attempt succeeds (the fake has no more scripted errors).
    let still = p.provision_pending(&ctx, &report.pending).await;
    assert!(still.is_empty());
    assert_eq!(gw.routes_of(gw.upstream_id("api.openai.com")).len(), 6);
}

#[tokio::test]
async fn misconfigured_entry_fails() {
    let (gw, p) = setup(&json!({"bad": mock_entry("mock-bad")}));
    gw.fail_upstream(
        "mock-bad",
        UpstreamErr::invalid_argument()
            .with_field_violation("alias", "explicit alias rejected", "INVALID_ALIAS")
            .create(),
    );
    let err = p.provision_all(&test_ctx()).await.unwrap_err();
    assert_eq!(err.target, "bad");
    assert!(matches!(
        *err.source,
        CanonicalError::InvalidArgument { .. }
    ));
}

#[tokio::test]
async fn rag_route_failure_is_not_fatal() {
    let (gw, p) = setup(&json!({"mock": mock_entry("mock-openai")}));
    gw.fail_route(
        "/v1/files",
        UpstreamErr::invalid_argument()
            .with_field_violation("upstream_id", "boom", "X")
            .create(),
    );
    let report = p.provision_all(&test_ctx()).await.unwrap();
    assert!(report.pending.is_empty());
    assert_eq!(gw.routes_of(gw.upstream_id("mock-openai")).len(), 5);
}

#[tokio::test]
async fn chat_route_failure_is_fatal() {
    let (gw, p) = setup(&json!({"mock": mock_entry("mock-openai")}));
    gw.fail_route(
        "/v1/responses",
        UpstreamErr::invalid_argument()
            .with_field_violation("upstream_id", "boom", "X")
            .create(),
    );
    assert!(p.provision_all(&test_ctx()).await.is_err());
}

#[tokio::test]
async fn tenant_override_gets_own_upstream() {
    let mut entry = openai_entry();
    entry["tenant_overrides"] = json!({
        TENANT: {"host": "eu.openai.example.com",
                 "auth_config": {"header": "Authorization", "prefix": "Bearer ",
                                 "secret_ref": "cred://tenant-key"}},
        "7a0e3c55-1111-4222-8333-944455556666": {"upstream_alias": "api.openai.com"}
    });
    let (gw, p) = setup(&json!({"openai": entry}));
    let report = p.provision_all(&test_ctx()).await.unwrap();
    assert!(report.pending.is_empty());

    let ov = gw.created("eu.openai.example.com");
    assert_eq!(ov.server().endpoints[0].host, "eu.openai.example.com");
    let auth = ov.auth().unwrap();
    assert_eq!(auth.plugin_type, APIKEY_AUTH_PLUGIN_ID, "plugin inherited");
    assert_eq!(
        auth.config.as_ref().unwrap()["secret_ref"],
        "cred://tenant-key"
    );
    assert_eq!(
        gw.routes_of(gw.upstream_id("eu.openai.example.com")).len(),
        6
    );

    // An override that maps to the entry's own alias does not create a second upstream.
    assert_eq!(gw.calls_for("api.openai.com"), 1);
    assert_eq!(gw.upstream_calls.lock().unwrap().len(), 2);

    // A pending override is reported under its own key.
    let (gw, p) = setup(&json!({"openai": {
        "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai",
        "tenant_overrides": {TENANT: {"host": "eu.openai.example.com"}}
    }}));
    gw.fail_upstream("eu.openai.example.com", secret_not_readable());
    let report = p.provision_all(&test_ctx()).await.unwrap();
    assert_eq!(report.pending, vec![format!("openai@{TENANT}")]);
}

#[tokio::test(start_paused = true)]
async fn reconcile_retries_with_backoff_until_success() {
    let (gw, p) = setup(&json!({"openai": openai_entry()}));
    for _ in 0..3 {
        gw.fail_upstream("api.openai.com", secret_not_readable());
    }
    let ctx = test_ctx();
    let report = p.provision_all(&ctx).await.unwrap();
    assert_eq!(report.pending, vec!["openai".to_owned()]);
    assert_eq!(gw.calls_for("api.openai.com"), 1);

    let cancel = CancellationToken::new();
    let mut workers = JoinSet::new();
    spawn_reconcile(
        &mut workers,
        Arc::new(p),
        ctx,
        report.pending,
        cancel.clone(),
    );

    // Retries at +2 s, +6 s (2+4), +14 s (6+8); the third retry succeeds.
    tokio::time::sleep(Duration::from_millis(1_900)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 2);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 3);
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 4);
    assert_eq!(gw.routes_of(gw.upstream_id("api.openai.com")).len(), 6);

    // Nothing pending any more: the task finishes on its own.
    tokio::time::timeout(Duration::from_secs(1), workers.join_next())
        .await
        .expect("reconcile task finished")
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn reconcile_backoff_caps_at_60s_and_stops_on_cancel() {
    let (gw, p) = setup(&json!({"openai": openai_entry()}));
    for _ in 0..100 {
        gw.fail_upstream("api.openai.com", secret_not_readable());
    }
    let ctx = test_ctx();
    let report = p.provision_all(&ctx).await.unwrap();
    let cancel = CancellationToken::new();
    let mut workers = JoinSet::new();
    spawn_reconcile(
        &mut workers,
        Arc::new(p),
        ctx,
        report.pending,
        cancel.clone(),
    );

    // Delays 2, 4, 8, 16, 32, 60, 60 -> retries at 2, 6, 14, 30, 62, 122, 182 s.
    tokio::time::sleep(Duration::from_secs(181)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 1 + 6);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 1 + 7);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), workers.join_next())
        .await
        .expect("reconcile task stops on cancel")
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(gw.calls_for("api.openai.com"), 1 + 7);
}

#[tokio::test]
async fn pending_retry_keeps_transient_errors_and_drops_invalid_argument() {
    let (gw, p) = setup(&json!({
        "openai": openai_entry(),
        "mock": mock_entry("mock-openai"),
        "bad": mock_entry("mock-bad")
    }));
    for alias in ["api.openai.com", "mock-openai", "mock-bad"] {
        gw.fail_upstream(alias, secret_not_readable());
    }
    let ctx = test_ctx();
    let report = p.provision_all(&ctx).await.unwrap();
    assert_eq!(report.pending.len(), 3);

    // Transient / unknown failures stay pending (DESIGN: retries continue until
    // the gear stops); only a clear InvalidArgument drops the target.
    gw.fail_upstream(
        "api.openai.com",
        CanonicalError::internal("credstore down").create(),
    );
    gw.fail_route(
        "/v1/responses",
        CanonicalError::service_unavailable()
            .with_retry_after_seconds(1)
            .create(),
    );
    gw.fail_upstream(
        "mock-bad",
        UpstreamErr::invalid_argument()
            .with_field_violation("alias", "explicit alias rejected", "INVALID_ALIAS")
            .create(),
    );
    let mut still = p.provision_pending(&ctx, &report.pending).await;
    still.sort();
    // `mock` created its upstream, but the chat route failed: it stays pending too.
    assert_eq!(still, vec!["mock".to_owned(), "openai".to_owned()]);

    gw.route_errors.lock().unwrap().clear();
    let still = p.provision_pending(&ctx, &still).await;
    assert!(still.is_empty());
    assert_eq!(gw.calls_for("mock-bad"), 2, "dropped target is not retried");
}

#[tokio::test]
async fn provisioning_health_is_unhealthy_until_ready_and_after_failure() {
    use toolkit::{Healthcheck, HealthcheckStatus};

    let health = ProvisioningHealth::default();
    assert_eq!(health.name(), "mini-chat-oagw-provisioning");
    let starting = health.check().await;
    assert_eq!(starting.status, HealthcheckStatus::Unhealthy);
    assert_eq!(starting.code.as_deref(), Some("oagw_provisioning_starting"));

    health.mark_ready();
    assert_eq!(health.check().await.status, HealthcheckStatus::Healthy);

    let failed = ProvisioningHealth::default();
    failed.mark_failed();
    let r = failed.check().await;
    assert_eq!(r.status, HealthcheckStatus::Unhealthy);
    assert_eq!(r.code.as_deref(), Some("oagw_provisioning_failed"));
}
