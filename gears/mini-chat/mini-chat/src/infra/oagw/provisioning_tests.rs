#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMethod, ListQuery,
    PathSuffixMode, Route, Scheme, Server, ServiceGatewayClientV1, SharingMode,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{Provisioner, ReconcileTimings, RouteSpec, UpstreamTarget, route_specs, upstream_request};
use crate::config::{MiniChatConfig, ProviderEntry, StorageKind, TenantOverride};
use crate::infra::llm::ProviderResolver;
use crate::infra::oagw::s2s::S2sContext;
use crate::testing::TestUser;

const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const TENANT: Uuid = Uuid::from_u128(0xaaaa_aaaa_aaaa_aaaa_aaaa_aaaa_aaaa_aaaa);

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamErr;

// ---------------------------------------------------------------------------
// Recording fake OAGW (control plane only)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    /// Every `create_upstream` request, in order.
    upstream_calls: Vec<CreateUpstreamRequest>,
    /// Remaining `create_upstream` calls answered with `FailedPrecondition`
    /// (secret not readable yet).
    secret_unreadable: usize,
    /// Answer of every `create_upstream` call.
    fail_with: Option<CanonicalError>,
    /// Latency of every `create_upstream` call (an attempt in flight).
    create_delay: Option<Duration>,
}

/// Emulates the OAGW create rules mini-chat relies on: alias collision →
/// `AlreadyExists`; hostname endpoints derive the alias (`host`, or
/// `host:port` for a non-default port) and reject a different explicit alias;
/// route overlap (same upstream, path and method) → `AlreadyExists`.
#[derive(Default)]
struct FakeOagw {
    state: Mutex<State>,
}

impl FakeOagw {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with(f: impl FnOnce(&mut State)) -> Arc<Self> {
        let gw = Self::new();
        f(&mut gw.state.lock().unwrap());
        gw
    }

    fn upstream_calls(&self) -> Vec<CreateUpstreamRequest> {
        self.state.lock().unwrap().upstream_calls.clone()
    }

    fn upstream(&self, alias: &str) -> Option<Upstream> {
        self.state
            .lock()
            .unwrap()
            .upstreams
            .iter()
            .find(|u| u.alias == alias)
            .cloned()
    }

    /// `(method, path, allowlist)` of the routes of `upstream_id`.
    fn routes_of(&self, upstream_id: Uuid) -> Vec<(HttpMethod, String, Vec<String>)> {
        self.state
            .lock()
            .unwrap()
            .routes
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| {
                let h = r.match_rules.http.as_ref().unwrap();
                assert_eq!(h.methods.len(), 1);
                assert_eq!(h.path_suffix_mode, PathSuffixMode::Append);
                (h.methods[0], h.path.clone(), h.query_allowlist.clone())
            })
            .collect()
    }

    fn route_count(&self) -> usize {
        self.state.lock().unwrap().routes.len()
    }
}

fn derived_alias(ep: &Endpoint) -> Option<String> {
    if ep.host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let standard = match ep.scheme {
        Scheme::Http => ep.port == 80,
        _ => ep.port == 443,
    };
    Some(if standard {
        ep.host.to_ascii_lowercase()
    } else {
        format!("{}:{}", ep.host.to_ascii_lowercase(), ep.port)
    })
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let delay = self.state.lock().unwrap().create_delay;
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let mut st = self.state.lock().unwrap();
        st.upstream_calls.push(req.clone());
        if let Some(err) = &st.fail_with {
            return Err(err.clone());
        }
        let ep = req.server().endpoints[0].clone();
        let alias = match (req.alias(), derived_alias(&ep)) {
            (Some(user), Some(derived)) if user.to_ascii_lowercase() != derived => {
                return Err(UpstreamErr::invalid_argument()
                    .with_format(format!(
                        "alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '{derived}')"
                    ))
                    .create());
            }
            (Some(user), _) => user.to_ascii_lowercase(),
            (None, Some(derived)) => derived,
            (None, None) => {
                return Err(UpstreamErr::invalid_argument()
                    .with_format("explicit alias is required for IP-based or heterogeneous-host endpoints")
                    .create());
            }
        };
        if req
            .auth()
            .and_then(|a| a.config.as_ref())
            .is_some_and(|c| c.contains_key("secret_ref"))
            && st.secret_unreadable > 0
        {
            st.secret_unreadable -= 1;
            return Err(UpstreamErr::failed_precondition()
                .with_precondition_violation(
                    "auth.config.secret_ref",
                    "secret_ref is not accessible to this tenant (not provisioned yet, or not shared)",
                    "STATE",
                )
                .create());
        }
        if st.upstreams.iter().any(|u| u.alias == alias) {
            return Err(UpstreamErr::already_exists(format!(
                "upstream with alias '{alias}' already exists"
            ))
            .with_resource(alias)
            .create());
        }
        let up = Upstream {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            alias,
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: true,
            auth: req.auth().cloned(),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: Vec::new(),
        };
        st.upstreams.push(up.clone());
        Ok(up)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .upstreams
            .iter()
            .skip(query.skip as usize)
            .take(query.top as usize)
            .cloned()
            .collect())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn delete_upstream(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let mut st = self.state.lock().unwrap();
        let http = req.match_rules().http.clone().unwrap();
        let overlap = st.routes.iter().any(|r| {
            let h = r.match_rules.http.as_ref().unwrap();
            r.upstream_id == req.upstream_id()
                && h.path == http.path
                && h.methods.iter().any(|m| http.methods.contains(m))
        });
        if overlap {
            return Err(UpstreamErr::already_exists("route overlap")
                .with_resource(http.path)
                .create());
        }
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: Vec::new(),
            priority: 0,
            enabled: true,
        };
        st.routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unreachable!("not used by provisioning")
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unreachable!("not used by provisioning")
    }

    /// Answers only requests whose alias and route are provisioned (OAGW
    /// answers `NotFound` otherwise): an SSE text stream for `…/responses`, a
    /// JSON object with an id for anything else.
    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let st = self.state.lock().unwrap();
        let path = req.uri().path().trim_start_matches('/');
        let (alias, rest) = path.split_once('/').unwrap_or((path, ""));
        let rest = format!("/{rest}");
        let method = match *req.method() {
            http::Method::GET => HttpMethod::Get,
            http::Method::DELETE => HttpMethod::Delete,
            _ => HttpMethod::Post,
        };
        let routed = st.upstreams.iter().find(|u| u.alias == alias).is_some_and(|u| {
            st.routes.iter().any(|r| {
                let h = r.match_rules.http.as_ref().unwrap();
                r.upstream_id == u.id && h.methods.contains(&method) && rest.starts_with(&h.path)
            })
        });
        if !routed {
            return Err(UpstreamErr::not_found(format!("no route for {alias}{rest}"))
                .with_resource(alias.to_owned())
                .create());
        }
        if rest.ends_with("/responses") {
            let body = concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hi\"}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n",
            );
            return Ok(http::Response::builder()
                .status(200)
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(body.to_owned()))
                .unwrap());
        }
        Ok(http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"id":"vs_1"}"#.to_owned()))
            .unwrap())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn openai_entry() -> ProviderEntry {
    MiniChatConfig::default().providers["openai"].clone()
}

fn azure_entry() -> ProviderEntry {
    let mut e = openai_entry();
    e.host = "my.openai.azure.com".to_owned();
    e.api_path = "/openai/v1/responses?api-version=x".to_owned();
    e.storage_kind = StorageKind::Azure;
    e.api_version = Some("2025-03-01-preview".to_owned());
    e
}

fn config(f: impl FnOnce(&mut HashMap<String, ProviderEntry>)) -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    "mini-chat".clone_into(&mut cfg.client_credentials.client_id);
    cfg.client_credentials.client_secret = "secret".to_owned().into();
    f(&mut cfg.providers);
    cfg.apply_defaults();
    cfg.validate().unwrap();
    cfg
}

struct Fixture {
    gw: Arc<FakeOagw>,
    resolver: Arc<ProviderResolver>,
    provisioner: Arc<Provisioner>,
}

fn fixture(gw: Arc<FakeOagw>, cfg: &MiniChatConfig) -> Fixture {
    let resolver = Arc::new(ProviderResolver::new(cfg));
    let s2s = Arc::new(S2sContext::new());
    s2s.set(ctx());
    let provisioner = Arc::new(
        Provisioner::new(
            gw.clone() as Arc<dyn ServiceGatewayClientV1>,
            cfg,
            Arc::clone(&resolver),
            s2s,
        )
        .with_timings(ReconcileTimings {
            first_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(80),
            warn_after: Duration::from_millis(50),
        }),
    );
    Fixture {
        gw,
        resolver,
        provisioner,
    }
}

fn ctx() -> SecurityContext {
    TestUser::S2S.security_context()
}

fn spec(method: HttpMethod, path: &str, allow: &[&str]) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_owned(),
        query_allowlist: allow.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn routes(specs: &[RouteSpec]) -> Vec<(HttpMethod, String, Vec<String>)> {
    specs
        .iter()
        .map(|s| (s.method, s.path.clone(), s.query_allowlist.clone()))
        .collect()
}

fn openai_routes() -> Vec<(HttpMethod, String, Vec<String>)> {
    routes(&route_specs(&openai_entry()))
}

// ---------------------------------------------------------------------------
// Route and upstream specs
// ---------------------------------------------------------------------------

#[test]
fn chat_route_prefix_and_query_allowlist() {
    let azure = route_specs(&azure_entry());
    assert_eq!(
        azure[0],
        spec(HttpMethod::Post, "/openai/v1/responses", &["api-version"])
    );

    let mut e = openai_entry();
    e.api_path = "/v1/chat/{model}/x".to_owned();
    assert_eq!(route_specs(&e)[0], spec(HttpMethod::Post, "/v1/chat", &[]));

    e.api_path = "/openai/deployments/{model}/chat/completions?api-version=1&a=b&api-version=2"
        .to_owned();
    assert_eq!(
        route_specs(&e)[0],
        spec(
            HttpMethod::Post,
            "/openai/deployments",
            &["api-version", "a"]
        )
    );

    assert_eq!(
        route_specs(&openai_entry())[0],
        spec(HttpMethod::Post, "/v1/responses", &[])
    );
}

#[test]
fn rag_routes_for_openai_and_azure() {
    assert_eq!(
        route_specs(&openai_entry())[1..].to_vec(),
        vec![
            spec(HttpMethod::Post, "/v1/files", &[]),
            spec(HttpMethod::Delete, "/v1/files", &[]),
            spec(HttpMethod::Post, "/v1/vector_stores", &[]),
            spec(HttpMethod::Delete, "/v1/vector_stores", &[]),
            spec(HttpMethod::Get, "/v1/vector_stores", &[]),
        ]
    );
    assert_eq!(
        route_specs(&azure_entry())[1..].to_vec(),
        vec![
            spec(HttpMethod::Post, "/openai/files", &["api-version"]),
            spec(HttpMethod::Delete, "/openai/files", &["api-version"]),
            spec(HttpMethod::Post, "/openai/vector_stores", &["api-version"]),
            spec(HttpMethod::Delete, "/openai/vector_stores", &["api-version"]),
            spec(HttpMethod::Get, "/openai/vector_stores", &["api-version"]),
        ]
    );
}

/// Every URI `RagClient` builds is served by one of the RAG routes: the route
/// for the method is a prefix of the path and the query keys are allowed.
#[test]
fn rag_routes_cover_the_storage_client_paths() {
    use crate::infra::llm::storage::storage_uri;

    for entry in [openai_entry(), azure_entry()] {
        let cfg = config(|p| {
            p.insert("openai".to_owned(), entry.clone());
        });
        let resolved = ProviderResolver::new(&cfg)
            .resolve("openai", TENANT)
            .unwrap();
        let specs = route_specs(&entry);
        let calls = [
            (HttpMethod::Post, "/files"),
            (HttpMethod::Delete, "/files/file-1"),
            (HttpMethod::Post, "/vector_stores"),
            (HttpMethod::Post, "/vector_stores/vs_1/files"),
            (HttpMethod::Get, "/vector_stores/vs_1/files/file-1"),
            (HttpMethod::Delete, "/vector_stores/vs_1"),
        ];
        for (method, path) in calls {
            let uri = storage_uri(&resolved, path);
            let rest = uri
                .strip_prefix(&format!("/{}", resolved.alias))
                .unwrap();
            let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
            let route = specs
                .iter()
                .filter(|s| s.method == method && path.starts_with(&s.path))
                .max_by_key(|s| s.path.len())
                .unwrap_or_else(|| panic!("no route for {method:?} {uri}"));
            for key in query.split('&').filter(|q| !q.is_empty()) {
                let key = key.split('=').next().unwrap();
                assert!(
                    route.query_allowlist.iter().any(|k| k == key),
                    "{uri}: query key {key} not allowed"
                );
            }
        }
    }
}

#[test]
fn upstream_uses_http_and_port_when_use_http() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "127.0.0.1".to_owned();
        e.port = Some(8080);
        e.use_http = true;
    });
    let req = upstream_request(&UpstreamTarget::for_entry("openai", &cfg.providers["openai"]));
    assert_eq!(
        req.server(),
        &Server {
            endpoints: vec![Endpoint {
                scheme: Scheme::Http,
                host: "127.0.0.1".to_owned(),
                port: 8080,
            }],
        }
    );
    assert_eq!(req.protocol(), oagw_sdk::HTTP_PROTOCOL_ID);
    assert_eq!(req.alias(), Some("127.0.0.1"));
    let auth = req.auth().unwrap();
    assert_eq!(auth.plugin_type, APIKEY);
    assert_eq!(auth.sharing, SharingMode::Private);
    assert_eq!(
        auth.config.as_ref().unwrap().get("secret_ref").map(String::as_str),
        Some("cred://openai-key")
    );

    // Defaults: https on 443.
    let cfg = config(|_| {});
    let req = upstream_request(&UpstreamTarget::for_entry("openai", &cfg.providers["openai"]));
    assert_eq!(
        req.server().endpoints[0],
        Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: 443,
        }
    );
}

#[test]
fn alias_defaults_to_host() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "llm.example.com".to_owned();
        e.auth_plugin_type = None;
    });
    let req = upstream_request(&UpstreamTarget::for_entry("openai", &cfg.providers["openai"]));
    assert_eq!(req.alias(), Some("llm.example.com"));
    // No auth plugin configured: no auth on the upstream.
    assert!(req.auth().is_none());

    let cfg = config(|p| {
        p.get_mut("openai").unwrap().upstream_alias = Some("custom".to_owned());
    });
    let req = upstream_request(&UpstreamTarget::for_entry("openai", &cfg.providers["openai"]));
    assert_eq!(req.alias(), Some("custom"));
}

#[tokio::test]
async fn tenant_override_gets_own_upstream() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.tenant_overrides.insert(
            TENANT,
            TenantOverride {
                host: Some("tenant.example.com".to_owned()),
                auth_config: Some(HashMap::from([
                    ("header".to_owned(), "api-key".to_owned()),
                    ("secret_ref".to_owned(), "cred://tenant-key".to_owned()),
                ])),
                ..TenantOverride::default()
            },
        );
    });
    let f = fixture(FakeOagw::new(), &cfg);

    let deferred = f.provisioner.provision_all(ctx()).await.unwrap();
    assert!(deferred.is_empty());

    let calls = f.gw.upstream_calls();
    assert_eq!(calls.len(), 2);
    let main = f.gw.upstream("api.openai.com").unwrap();
    let tenant = f.gw.upstream("tenant.example.com").unwrap();
    assert_eq!(tenant.server.endpoints[0].host, "tenant.example.com");
    assert_eq!(tenant.server.endpoints[0].port, 443);
    let auth = tenant.auth.unwrap();
    assert_eq!(auth.plugin_type, APIKEY);
    assert_eq!(
        auth.config.unwrap().get("secret_ref").map(String::as_str),
        Some("cred://tenant-key")
    );
    assert_eq!(f.gw.routes_of(main.id), openai_routes());
    assert_eq!(f.gw.routes_of(tenant.id), openai_routes());
}

#[tokio::test]
async fn already_exists_is_reused() {
    let gw = FakeOagw::new();
    let cfg = config(|_| {});
    // A previous provisioning (same process) left the upstream and its chat route.
    fixture(Arc::clone(&gw), &cfg)
        .provisioner
        .provision_all(ctx())
        .await
        .unwrap();
    let existing = gw.upstream("api.openai.com").unwrap();
    let routes_before = gw.route_count();

    let f = fixture(Arc::clone(&gw), &cfg);
    let deferred = f.provisioner.provision_all(ctx()).await.unwrap();
    assert!(deferred.is_empty());
    assert_eq!(gw.upstream_calls().len(), 2);
    assert_eq!(gw.state.lock().unwrap().upstreams.len(), 1);
    // Routes ensured again on the reused upstream; duplicates are tolerated.
    assert_eq!(gw.route_count(), routes_before);
    assert_eq!(gw.routes_of(existing.id), openai_routes());
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "api.openai.com"
    );
}

#[tokio::test]
async fn two_entries_sharing_a_host_share_one_upstream() {
    let cfg = config(|p| {
        let mut second = p["openai"].clone();
        second.api_path = "/v1/chat/completions".to_owned();
        p.insert("second".to_owned(), second);
    });
    let f = fixture(FakeOagw::new(), &cfg);
    assert!(f.provisioner.provision_all(ctx()).await.unwrap().is_empty());
    let up = f.gw.upstream("api.openai.com").unwrap();
    let routes = f.gw.routes_of(up.id);
    assert!(routes.contains(&(HttpMethod::Post, "/v1/responses".to_owned(), vec![])));
    assert!(routes.contains(&(HttpMethod::Post, "/v1/chat/completions".to_owned(), vec![])));
    // RAG routes once.
    assert_eq!(routes.len(), 7);
}

#[tokio::test]
async fn failed_precondition_is_deferred_not_fatal() {
    let gw = FakeOagw::with(|st| st.secret_unreadable = 3);
    let cfg = config(|_| {});
    let f = fixture(Arc::clone(&gw), &cfg);

    let deferred = f.provisioner.provision_all(ctx()).await.unwrap();
    assert_eq!(deferred, vec!["openai".to_owned()]);
    assert_eq!(gw.route_count(), 0);

    // The background reconcile retries until the secret becomes readable.
    let cancel = CancellationToken::new();
    let handle = Arc::clone(&f.provisioner).spawn_reconcile(deferred, cancel.clone());
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("reconcile finishes once provisioned")
        .unwrap();
    let up = gw.upstream("api.openai.com").unwrap();
    assert_eq!(gw.routes_of(up.id), openai_routes());
    // 1 at start + 2 failed retries + 1 success.
    assert_eq!(gw.upstream_calls().len(), 4);
}

#[tokio::test]
async fn reconcile_stops_on_cancel() {
    let gw = FakeOagw::with(|st| st.secret_unreadable = usize::MAX);
    let cfg = config(|_| {});
    let f = fixture(Arc::clone(&gw), &cfg);
    let deferred = f.provisioner.provision_all(ctx()).await.unwrap();
    let cancel = CancellationToken::new();
    let handle = Arc::clone(&f.provisioner).spawn_reconcile(deferred, cancel.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("reconcile ends on cancel")
        .unwrap();
    assert!(gw.upstream("api.openai.com").is_none());
}

#[tokio::test]
async fn validation_error_is_fatal() {
    let gw = FakeOagw::with(|st| {
        st.fail_with = Some(
            UpstreamErr::invalid_argument()
                .with_format("server must have at least one endpoint")
                .create(),
        );
    });
    let cfg = config(|_| {});
    let f = fixture(Arc::clone(&gw), &cfg);
    let err = f.provisioner.provision_all(ctx()).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("openai"), "{text}");
    assert!(text.contains("at least one endpoint"), "{text}");
    // No retry without alias for other validation errors.
    assert_eq!(gw.upstream_calls().len(), 1);
}

#[tokio::test]
async fn auto_derived_alias_rejection_retries_without_alias() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "mock-llm".to_owned();
        e.port = Some(8080);
        e.use_http = true;
    });
    let f = fixture(FakeOagw::new(), &cfg);

    let deferred = f.provisioner.provision_all(ctx()).await.unwrap();
    assert!(deferred.is_empty());

    let calls = f.gw.upstream_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].alias(), Some("mock-llm"));
    assert_eq!(calls[1].alias(), None);
    let up = f.gw.upstream("mock-llm:8080").unwrap();
    assert_eq!(f.gw.routes_of(up.id), openai_routes());
    // Requests are routed by the alias OAGW derived.
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    assert_eq!(p.alias, "mock-llm:8080");
    assert_eq!(p.chat_uri("gpt"), "/mock-llm:8080/v1/responses");
}

#[tokio::test]
async fn auto_derived_alias_already_existing_is_reused() {
    let gw = FakeOagw::new();
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "mock-llm".to_owned();
        e.port = Some(8080);
        e.use_http = true;
    });
    fixture(Arc::clone(&gw), &cfg)
        .provisioner
        .provision_all(ctx())
        .await
        .unwrap();
    let f = fixture(Arc::clone(&gw), &cfg);
    assert!(f.provisioner.provision_all(ctx()).await.unwrap().is_empty());
    assert_eq!(gw.state.lock().unwrap().upstreams.len(), 1);
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "mock-llm:8080"
    );
}

#[tokio::test]
async fn tenant_override_alias_from_oagw_reaches_the_resolver() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "127.0.0.1".to_owned();
        e.port = Some(9000);
        e.use_http = true;
        e.tenant_overrides.insert(
            TENANT,
            TenantOverride {
                host: Some("tenant-llm".to_owned()),
                ..TenantOverride::default()
            },
        );
    });
    let f = fixture(FakeOagw::new(), &cfg);
    assert!(f.provisioner.provision_all(ctx()).await.unwrap().is_empty());
    assert!(f.gw.upstream("127.0.0.1").is_some());
    assert!(f.gw.upstream("tenant-llm:9000").is_some());
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "tenant-llm:9000"
    );
    assert_eq!(
        f.resolver.resolve("openai", Uuid::nil()).unwrap().alias,
        "127.0.0.1"
    );
}

// ---------------------------------------------------------------------------
// Reuse of an existing upstream must match the target
// ---------------------------------------------------------------------------

#[tokio::test]
async fn override_with_only_alias_and_own_auth_is_not_routed_to_the_entry_upstream() {
    // Hostname endpoint on its default port: OAGW derives the alias, so the
    // override's own alias is rejected and its upstream collides with the
    // entry's one, which carries the platform key.
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "llm.example.com".to_owned();
        e.tenant_overrides.insert(
            TENANT,
            TenantOverride {
                upstream_alias: Some("tenant-llm".to_owned()),
                auth_config: Some(HashMap::from([
                    ("header".to_owned(), "authorization".to_owned()),
                    ("secret_ref".to_owned(), "cred://tenant-key".to_owned()),
                ])),
                ..TenantOverride::default()
            },
        );
    });
    let f = fixture(FakeOagw::new(), &cfg);
    let err = f.provisioner.provision_all(ctx()).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains(&TENANT.to_string()), "{text}");
    assert!(text.contains("llm.example.com"), "{text}");
    // The tenant keeps its configured alias (no override to the entry upstream).
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "tenant-llm"
    );
}

#[tokio::test]
async fn override_with_only_alias_and_same_auth_reuses_the_entry_upstream() {
    let cfg = config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "llm.example.com".to_owned();
        e.tenant_overrides.insert(
            TENANT,
            TenantOverride {
                upstream_alias: Some("tenant-llm".to_owned()),
                ..TenantOverride::default()
            },
        );
    });
    let f = fixture(FakeOagw::new(), &cfg);
    assert!(f.provisioner.provision_all(ctx()).await.unwrap().is_empty());
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "llm.example.com"
    );
}

#[tokio::test]
async fn mismatching_existing_upstream_is_fatal() {
    let cfg = config(|_| {});
    // Same alias, other port and no auth.
    let gw = FakeOagw::with(|st| {
        st.upstreams.push(Upstream {
            id: Uuid::new_v4(),
            tenant_id: TestUser::S2S.tenant_id,
            alias: "api.openai.com".to_owned(),
            server: Server {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: 8443,
                }],
            },
            protocol: oagw_sdk::HTTP_PROTOCOL_ID.to_owned(),
            enabled: true,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: Vec::new(),
        });
    });
    let f = fixture(Arc::clone(&gw), &cfg);
    let err = f.provisioner.provision_all(ctx()).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("provider 'openai'"), "{text}");
    assert!(text.contains("api.openai.com"), "{text}");
    assert_eq!(gw.route_count(), 0);
}

// ---------------------------------------------------------------------------
// On-demand provisioning of a deferred provider
// ---------------------------------------------------------------------------

/// Hostname on a non-default port: the alias OAGW registers (`mock-llm:8080`)
/// differs from the configured one, so the request must also be re-routed.
fn mock_llm_config() -> MiniChatConfig {
    config(|p| {
        let e = p.get_mut("openai").unwrap();
        e.host = "mock-llm".to_owned();
        e.port = Some(8080);
        e.use_http = true;
    })
}

fn llm_request() -> crate::infra::llm::LlmRequest {
    use crate::infra::llm::{LlmMessage, LlmRequest, RequestMetadata, RequestType};
    LlmRequest {
        model: "gpt".to_owned(),
        instructions: String::new(),
        input: vec![LlmMessage::text(crate::domain::model::MessageRole::User, "hi")],
        max_output_tokens: 10,
        tools: Vec::new(),
        max_tool_calls: None,
        api_params: mini_chat_sdk::ModelApiParams::default(),
        user: "u".to_owned(),
        metadata: RequestMetadata::new(TENANT, Uuid::nil(), Uuid::nil(), RequestType::Chat, &[]),
        stream: true,
        tool_rounds: Vec::new(),
    }
}

fn s2s() -> Arc<S2sContext> {
    let s2s = Arc::new(S2sContext::new());
    s2s.set(ctx());
    s2s
}

#[tokio::test]
async fn stream_request_provisions_a_deferred_provider_on_demand() {
    use futures::StreamExt;

    use crate::infra::llm::{LlmClient, LlmEvent};

    let gw = FakeOagw::with(|st| st.secret_unreadable = 1);
    let f = fixture(Arc::clone(&gw), &mock_llm_config());
    assert_eq!(
        f.provisioner.provision_all(ctx()).await.unwrap(),
        vec!["openai".to_owned()]
    );
    // The secret is readable now; no reconcile tick has run.
    let client = LlmClient::new(gw.clone() as Arc<dyn ServiceGatewayClientV1>, s2s())
        .with_provisioner(Arc::clone(&f.provisioner));
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    assert_eq!(p.alias, "mock-llm");

    let events: Vec<LlmEvent> = client
        .stream(&p, &llm_request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events[0], LlmEvent::TextDelta("Hi".to_owned()));
    assert!(matches!(events.last(), Some(LlmEvent::Completed { .. })));
    assert_eq!(
        f.resolver.resolve("openai", TENANT).unwrap().alias,
        "mock-llm:8080"
    );
    // Provisioned: later requests (resolved as callers do, per request) do
    // not provision again.
    let calls = gw.upstream_calls().len();
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    let _ = client
        .stream(&p, &llm_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(gw.upstream_calls().len(), calls);
}

#[tokio::test]
async fn completion_request_provisions_a_deferred_provider_on_demand() {
    use crate::infra::llm::LlmClient;

    let gw = FakeOagw::with(|st| st.secret_unreadable = 1);
    let f = fixture(Arc::clone(&gw), &mock_llm_config());
    f.provisioner.provision_all(ctx()).await.unwrap();
    let client = LlmClient::new(gw.clone() as Arc<dyn ServiceGatewayClientV1>, s2s())
        .with_provisioner(Arc::clone(&f.provisioner));
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    // The fake answers a stream; reaching the provider is what matters here.
    let res = client.complete(&p, &llm_request()).await;
    assert!(
        !matches!(res, Err(crate::infra::llm::LlmError::Unavailable(_))),
        "{res:?}"
    );
    assert!(gw.upstream("mock-llm:8080").is_some());
}

#[tokio::test]
async fn rag_request_provisions_a_deferred_provider_on_demand() {
    use crate::infra::llm::RagClient;

    let gw = FakeOagw::with(|st| st.secret_unreadable = 1);
    let f = fixture(Arc::clone(&gw), &mock_llm_config());
    f.provisioner.provision_all(ctx()).await.unwrap();
    let rag = RagClient::new(gw.clone() as Arc<dyn ServiceGatewayClientV1>, s2s())
        .with_provisioner(Arc::clone(&f.provisioner));
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    assert_eq!(rag.create_vector_store(&p, "chat_1").await.unwrap(), "vs_1");
}

#[tokio::test]
async fn on_demand_attempts_are_rate_limited() {
    use crate::infra::llm::LlmClient;

    let gw = FakeOagw::with(|st| st.secret_unreadable = usize::MAX);
    let f = fixture(Arc::clone(&gw), &mock_llm_config());
    f.provisioner.provision_all(ctx()).await.unwrap();
    let client = LlmClient::new(gw.clone() as Arc<dyn ServiceGatewayClientV1>, s2s())
        .with_provisioner(Arc::clone(&f.provisioner));
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    let before = gw.upstream_calls().len();

    // First request attempts (and fails like before), the next one within a
    // second does not attempt again.
    assert!(client
        .stream(&p, &llm_request(), CancellationToken::new())
        .await
        .is_err());
    let after_first = gw.upstream_calls().len();
    assert!(after_first > before);
    assert!(client
        .stream(&p, &llm_request(), CancellationToken::new())
        .await
        .is_err());
    assert_eq!(gw.upstream_calls().len(), after_first);
}

#[tokio::test]
async fn concurrent_requests_wait_for_in_flight_on_demand_attempt() {
    let gw = FakeOagw::with(|st| st.secret_unreadable = 1);
    let f = fixture(Arc::clone(&gw), &mock_llm_config());
    f.provisioner.provision_all(ctx()).await.unwrap();
    // The secret is readable now; the on-demand attempt takes a while.
    gw.state.lock().unwrap().create_delay = Some(Duration::from_millis(200));
    let p = f.resolver.resolve("openai", TENANT).unwrap();
    assert_eq!(p.alias, "mock-llm");
    let calls = gw.upstream_calls().len();

    // The first request starts the attempt; the second arrives while it is in
    // flight and must not proxy with the stale alias.
    let (first, second) = tokio::join!(
        f.provisioner.ensure_provisioned(&p),
        f.provisioner.ensure_provisioned(&p)
    );

    assert_eq!(
        first.expect("first request provisions").alias,
        "mock-llm:8080"
    );
    assert_eq!(
        second.expect("second request waits and re-resolves").alias,
        "mock-llm:8080"
    );
    // One attempt only: the explicit alias is rejected, the retry without
    // alias registers `mock-llm:8080`.
    assert_eq!(gw.upstream_calls().len(), calls + 2);
}
