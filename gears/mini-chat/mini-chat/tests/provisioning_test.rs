//! OAGW provisioning: S2S client-credentials exchange, upstream and route
//! shapes, deferral while the credential is not readable, on-demand retry
//! from the request path and the background reconcile loop.
#![allow(clippy::unwrap_used, clippy::type_complexity, clippy::non_ascii_literal)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest};
use mini_chat::config::{ClientCredentialsConfig, MiniChatConfig};
use mini_chat::infra::llm::provisioning::{Provisioner, upstream_specs};
use mini_chat::infra::llm::{ProviderResolver, ProvisionHook, S2sContext};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::{CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest, UpdateUpstreamRequest, Upstream};
use parking_lot::Mutex;
use secrecy::ExposeSecret;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamRes;

#[derive(Default)]
struct ProvisionGateway {
    secret_ready: AtomicBool,
    upstreams: Mutex<Vec<(CreateUpstreamRequest, Uuid)>>,
    routes: Mutex<Vec<CreateRouteRequest>>,
    subjects: Mutex<Vec<Uuid>>,
}

fn err() -> CanonicalError {
    CanonicalError::internal("unsupported").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for ProvisionGateway {
    async fn create_upstream(&self, ctx: SecurityContext, req: CreateUpstreamRequest) -> Result<Upstream, CanonicalError> {
        self.subjects.lock().push(ctx.subject_id());
        if !self.secret_ready.load(Ordering::SeqCst) {
            return Err(UpstreamRes::failed_precondition()
                .with_precondition_violation("auth.config.secret_ref", "not accessible", "STATE")
                .create());
        }
        let id = Uuid::new_v4();
        let up = Upstream {
            id,
            tenant_id: ctx.subject_tenant_id(),
            alias: req.alias().unwrap_or("derived").to_owned(),
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: true,
            auth: req.auth().cloned(),
            headers: req.headers().cloned(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: req.tags().to_vec(),
        };
        self.upstreams.lock().push((req, id));
        Ok(up)
    }
    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        Err(err())
    }
    async fn list_upstreams(&self, _: SecurityContext, _: &ListQuery) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_upstream(&self, _: SecurityContext, _: Uuid, _: UpdateUpstreamRequest) -> Result<Upstream, CanonicalError> {
        Err(err())
    }
    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn create_route(&self, ctx: SecurityContext, req: CreateRouteRequest) -> Result<Route, CanonicalError> {
        let r = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: req.tags().to_vec(),
            priority: 0,
            enabled: true,
        };
        self.routes.lock().push(req);
        Ok(r)
    }
    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        Err(err())
    }
    async fn list_routes(&self, _: SecurityContext, _: Option<Uuid>, _: &ListQuery) -> Result<Vec<Route>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_route(&self, _: SecurityContext, _: Uuid, _: UpdateRouteRequest) -> Result<Route, CanonicalError> {
        Err(err())
    }
    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn resolve_proxy_target(&self, _: SecurityContext, _: &str, _: &str, _: &str) -> Result<(Upstream, Route), CanonicalError> {
        Err(err())
    }
    async fn proxy_request(&self, _: SecurityContext, _: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        Err(err())
    }
}

struct TestAuthN {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, String)>>,
    subject: Uuid,
}

#[async_trait]
impl AuthNResolverClient for TestAuthN {
    async fn authenticate(&self, _token: &str) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::Unauthorized("n/a".to_owned()))
    }
    async fn exchange_client_credentials(
        &self,
        req: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .push((req.client_id.clone(), req.client_secret.expose_secret().to_owned()));
        Ok(AuthenticationResult {
            security_context: SecurityContext::builder()
                .subject_id(self.subject)
                .subject_tenant_id(Uuid::new_v4())
                .build()
                .unwrap(),
        })
    }
}

fn setup() -> (Arc<ProvisionGateway>, Arc<TestAuthN>, Arc<ProviderResolver>, Arc<Provisioner>, MiniChatConfig) {
    let cfg: MiniChatConfig = serde_json::from_value(json!({
        "client_credentials": {"client_id": "mini-chat", "client_secret": "s3cret"},
        "providers": {"openai": {"kind": "openai_responses", "host": "127.0.0.1", "port": 18999, "use_http": true,
            "upstream_alias": "mock-openai", "storage_kind": "openai",
            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "auth_config": {"header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key"}}}
    }))
    .unwrap();
    let gw = Arc::new(ProvisionGateway::default());
    let authn = Arc::new(TestAuthN {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        subject: Uuid::new_v4(),
    });
    let resolver = Arc::new(ProviderResolver::new(cfg.providers.clone()));
    let creds: ClientCredentialsConfig = cfg.client_credentials.clone();
    let p = Arc::new(Provisioner::new(
        gw.clone(),
        Some(authn.clone()),
        creds,
        Arc::clone(&resolver),
        Arc::new(S2sContext::default()),
    ));
    resolver.set_hook(Arc::clone(&p) as Arc<dyn ProvisionHook>);
    (gw, authn, resolver, p, cfg)
}

#[tokio::test]
async fn provisions_upstream_and_routes_with_s2s_context() {
    let (gw, authn, resolver, p, cfg) = setup();
    gw.secret_ready.store(true, Ordering::SeqCst);
    let specs = upstream_specs(&cfg.providers);
    assert!(!p.start(&specs).await, "nothing pending");
    assert_eq!(authn.calls.load(Ordering::SeqCst), 1);
    assert_eq!(authn.seen.lock()[0], ("mini-chat".to_owned(), "s3cret".to_owned()));
    assert!(gw.subjects.lock().iter().all(|s| *s == authn.subject), "provisioned with the S2S context");
    let ups = gw.upstreams.lock();
    assert_eq!(ups.len(), 1);
    let req = &ups[0].0;
    assert_eq!(req.alias(), Some("mock-openai"));
    let ep = &req.server().endpoints[0];
    assert_eq!(ep.host, "127.0.0.1");
    assert_eq!(ep.port, 18999);
    assert_eq!(format!("{:?}", ep.scheme).to_lowercase(), "http");
    let auth = req.auth().unwrap();
    assert_eq!(auth.plugin_type, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
    assert_eq!(auth.config.as_ref().unwrap()["secret_ref"], "cred://openai-key");
    let routes: Vec<String> = gw
        .routes
        .lock()
        .iter()
        .map(|r| r.match_rules().http.as_ref().unwrap().path.clone())
        .collect();
    assert_eq!(routes, vec!["/v1/responses", "/v1/files", "/v1/vector_stores"]);
    assert!(gw.routes.lock().iter().all(|r| r.upstream_id() == ups[0].1));
    assert!(!resolver.is_pending("mock-openai"));
}

#[tokio::test]
async fn deferred_upstream_is_provisioned_on_demand() {
    let (gw, _authn, resolver, p, cfg) = setup();
    let specs = upstream_specs(&cfg.providers);
    assert!(p.start(&specs).await, "credential not readable yet → deferred");
    assert!(resolver.is_pending("mock-openai"));
    // still not readable: stays pending
    resolver.ensure_ready("mock-openai").await;
    assert!(resolver.is_pending("mock-openai"));
    // credential appears; the next request provisions without waiting for the loop
    gw.secret_ready.store(true, Ordering::SeqCst);
    resolver.ensure_ready("mock-openai").await;
    assert!(!resolver.is_pending("mock-openai"));
    assert_eq!(gw.upstreams.lock().len(), 1);
    let before = gw.subjects.lock().len();
    resolver.ensure_ready("mock-openai").await;
    assert_eq!(gw.subjects.lock().len(), before, "no further calls once ready");
}

#[tokio::test]
async fn reconcile_loop_retries_deferred_upstreams() {
    let (gw, _authn, resolver, p, cfg) = setup();
    let specs = upstream_specs(&cfg.providers);
    assert!(p.start(&specs).await);
    gw.secret_ready.store(true, Ordering::SeqCst);
    let cancel = CancellationToken::new();
    let h = tokio::spawn(Arc::clone(&p).reconcile(cancel.clone()));
    tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .expect("loop ends once everything is provisioned")
        .unwrap();
    assert!(!resolver.is_pending("mock-openai"));
    assert_eq!(gw.upstreams.lock().len(), 1);
}
