//! Shared harness for the router-level oagw tests.
//!
//! The gear is built exactly as the api-gateway test suite builds its own: a
//! JSON [`ConfigProvider`], a `ClientHub`, `Gear::init`, then
//! `RestApiCapability::register_rest` onto a fresh `Router`. The external
//! upstream is an in-process `axum` server on an ephemeral port, so proxying,
//! streaming and error semantics are all observable without a network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use async_trait::async_trait;
use credstore_sdk::test_util::MockCredStoreClient;
use toolkit::Gear;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::config::ConfigProvider;
use toolkit::context::GearCtx;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwGear;
use toolkit::RestApiCapability;

/// Tenant and subject the test security context carries.
pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const SUBJECT_A: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);

/// Wire GTS identifiers the built-in plugins answer to.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const REQUEST_ID_PLUGIN: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
pub const REQUIRED_HEADERS_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
pub const OAUTH2_PLUGIN: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const APIKEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const REQUIRED_HEADERS_GUARD: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Value of `X-OAGW-Error-Source` on a response the gateway generated itself.
pub const GATEWAY_SOURCE: &str = "gateway";

/// Value of `X-OAGW-Error-Source` on a response the upstream produced.
pub const UPSTREAM_SOURCE: &str = "upstream";

/// Configuration supplied as JSON, the way the server's YAML would be.
#[derive(Clone)]
pub struct JsonConfig {
    oagw: serde_json::Value,
}

impl JsonConfig {
    /// Configuration with the knobs the tests care about.
    #[must_use]
    pub fn new(allow_http: bool, max_body_bytes: usize) -> Self {
        Self {
            oagw: serde_json::json!({
                "config": {
                    "allow_http_upstream": allow_http,
                    "max_body_bytes": max_body_bytes,
                    "proxy_timeout_secs": 5,
                    "connect_timeout_secs": 5,
                    "ssrf_policy": { "enabled": false },
                }
            }),
        }
    }
}

impl ConfigProvider for JsonConfig {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        if gear == "oagw" {
            Some(&self.oagw)
        } else {
            None
        }
    }
}

/// A gateway wired to a real router, plus the upstream it forwards to.
pub struct Harness {
    router: Router,
    upstream: Upstream,
    /// Upstreams created through this harness, by the alias they were given.
    created: std::sync::Mutex<std::collections::BTreeMap<String, Uuid>>,
}

impl Harness {
    /// Builds the gear, initializes it and registers its routes.
    ///
    /// # Panics
    /// Panics when the gear cannot be initialized or its routes registered.
    pub async fn build(config: &JsonConfig, secrets: Vec<(String, String)>) -> (Self, Upstream) {
        Self::build_with_hierarchy(config, secrets, &[]).await
    }

    /// Builds the gear over a fixed tenant hierarchy.
    ///
    /// Each pair is a `(child, parent)` edge, so the tests that need an
    /// ancestor chain can declare one without standing up the resolver.
    ///
    /// # Panics
    /// Panics when the gear cannot be initialized or its routes registered.
    pub async fn build_with_hierarchy(
        config: &JsonConfig,
        secrets: Vec<(String, String)>,
        pairs: &[(Uuid, Uuid)],
    ) -> (Self, Upstream) {
        let upstream = Upstream::start().await;
        let hub = Arc::new(toolkit::ClientHub::new());
        hub.register::<dyn credstore_sdk::CredStoreClientV1>(Arc::new(
            MockCredStoreClient::with_secrets(secrets),
        ));
        hub.register::<dyn tenant_resolver_sdk::TenantResolverClient>(Arc::new(
            MockTenantResolver::from_pairs(pairs),
        ));

        let ctx = GearCtx::new(
            "oagw",
            Uuid::new_v4(),
            Arc::new(config.clone()),
            hub,
            tokio_util::sync::CancellationToken::new(),
        );

        let gear = OagwGear::default();
        gear.init(&ctx).await.expect("oagw gear initializes");

        let router = gear
            .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
            .expect("oagw routes register");

        (
            Self {
                router,
                upstream: upstream.clone(),
                created: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            },
            upstream,
        )
    }

    /// The upstream the harness stands in for.
    #[must_use]
    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// Sends a request to the gateway as tenant A.
    ///
    /// # Panics
    /// Panics when the router cannot be driven.
    pub async fn serve(&self, request: Request) -> Response {
        self.router
            .clone()
            .layer(axum::middleware::from_fn(inject_security_context))
            .oneshot(request)
            .await
            .expect("router serves the request")
    }

    /// Sends a request with a different tenant than the harness default.
    ///
    /// # Panics
    /// Panics when the router cannot be driven.
    pub async fn serve_as(&self, tenant: Uuid, request: Request) -> Response {
        self.router
            .clone()
            .layer(axum::middleware::from_fn(
                move |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let context = SecurityContext::builder()
                        .subject_tenant_id(tenant)
                        .subject_id(SUBJECT_A)
                        .build()
                        .expect("test security context builds");
                    request.extensions_mut().insert(context);
                    next.run(request).await
                },
            ))
            .oneshot(request)
            .await
            .expect("router serves the request")
    }

    /// Sends a request carrying no security context at all.
    ///
    /// # Panics
    /// Panics when the router cannot be driven.
    pub async fn serve_unauthenticated(&self, request: Request) -> Response {
        self.router.clone().oneshot(request).await.expect("router serves")
    }

    /// Serves the gateway over real TCP and returns the address it listens on.
    ///
    /// `oneshot` never hands the handler an `OnUpgrade`, so an upgrade driven
    /// that way is negotiated once and never piped; a socket gives the gateway
    /// both halves of the tunnel to copy between.
    ///
    /// # Panics
    /// Panics when the listener cannot be bound.
    pub async fn serve_tcp(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener binds");
        let address = listener.local_addr().expect("test listener has an address");
        let app = self
            .router
            .clone()
            .layer(axum::middleware::from_fn(inject_security_context));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test gateway serves");
        });
        address
    }

    /// Registers an upstream and a route in one step, returning both ids.
    ///
    /// # Panics
    /// Panics when the management API refuses the pair.
    pub async fn simple_upstream(
        &self,
        alias: &str,
        extra: Option<serde_json::Value>,
    ) -> (Uuid, Uuid) {
        self.upstream_with_route(alias, extra, None).await
    }

    /// The port the stand-in upstream listens on.
    #[must_use]
    pub fn upstream_port(&self) -> u16 {
        self.upstream.port()
    }

    /// The id of an upstream this harness created by alias.
    ///
    /// # Panics
    /// Panics when the alias was never registered.
    #[must_use]
    pub fn upstream_id(&self, alias: &str) -> Uuid {
        self.created
            .lock()
            .expect("the harness registry is never poisoned")
            .get(alias)
            .copied()
            .unwrap_or_else(|| panic!("no upstream registered as `{alias}`"))
    }

    /// Posts an upstream document verbatim and remembers it by alias.
    ///
    /// # Panics
    /// Panics when the management API refuses the document.
    pub async fn register_upstream(&self, alias: &str, document: serde_json::Value) -> &Self {
        let response = self
            .serve(request("POST", "/oagw/v1/upstreams", Some(document)))
            .await;
        let recorded = record(response).await;
        assert_eq!(recorded.status, StatusCode::CREATED, "upstream: {}", recorded.raw);
        let id: Uuid = recorded.body["id"].as_str().unwrap().parse().unwrap();
        self.created
            .lock()
            .expect("the harness registry is never poisoned")
            .insert(alias.to_owned(), id);
        self
    }

    /// Posts a route pointing `alias` at `path`, with an optional override.
    ///
    /// # Panics
    /// Panics when the management API refuses the route.
    pub async fn route_for(&self, alias: &str, path: &str, extra: Option<serde_json::Value>) {
        let mut route = serde_json::json!({
            "upstream_id": self.upstream_id(alias),
            "match": {
                "http": {
                    "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                    "path": path,
                    "query_allowlist": ["a", "b", "allowed", "listed", "q"],
                }
            },
        });
        merge(&mut route, extra);
        let response = self
            .serve(request("POST", "/oagw/v1/routes", Some(route)))
            .await;
        let recorded = record(response).await;
        assert_eq!(recorded.status, StatusCode::CREATED, "route: {}", recorded.raw);
    }

    /// Registers an upstream and a route, shaping either document.
    ///
    /// `upstream_extra` and `route_extra` are merged over the top level of the
    /// respective document, so a test can add credentials, tags, a narrower
    /// method list or a query allowlist without the harness growing a parameter
    /// for each.
    ///
    /// # Panics
    /// Panics when the management API refuses the pair.
    pub async fn upstream_with_route(
        &self,
        alias: &str,
        upstream_extra: Option<serde_json::Value>,
        route_extra: Option<serde_json::Value>,
    ) -> (Uuid, Uuid) {
        let mut document = serde_json::json!({
            "alias": alias,
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [{
                    "scheme": "http",
                    "host": self.upstream.host(),
                    "port": self.upstream.port(),
                }]
            },
        });
        merge(&mut document, upstream_extra);

        let response = self
            .serve(request("POST", "/oagw/v1/upstreams", Some(document)))
            .await;
        let recorded = record(response).await;
        assert_eq!(recorded.status, StatusCode::CREATED, "upstream: {}", recorded.raw);
        let upstream_id: Uuid = recorded.body["id"].as_str().unwrap().parse().unwrap();
        self.created
            .lock()
            .expect("the harness registry is never poisoned")
            .insert(alias.to_owned(), upstream_id);

        let mut route = serde_json::json!({
            "upstream_id": upstream_id,
            "match": {
                "http": {
                    "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                    "path": "/",
                    "query_allowlist": ["a", "b", "allowed", "listed", "q"],
                }
            },
        });
        merge(&mut route, route_extra);

        let response = self
            .serve(request("POST", "/oagw/v1/routes", Some(route)))
            .await;
        let recorded = record(response).await;
        assert_eq!(recorded.status, StatusCode::CREATED, "route: {}", recorded.raw);
        let route_id: Uuid = recorded.body["id"].as_str().unwrap().parse().unwrap();

        (upstream_id, route_id)
    }
}

/// Merges `extra`'s top-level fields into `document`.
pub fn merge(document: &mut serde_json::Value, extra: Option<serde_json::Value>) {
    let Some(extra) = extra.and_then(|value| value.as_object().cloned()) else {
        return;
    };
    let fields = document.as_object_mut().expect("a JSON object");
    for (key, value) in extra {
        fields.insert(key, value);
    }
}

/// A stand-in tenant resolver: a fixed parent map, nothing else.
///
/// Only the ancestor chain is modelled, since that is all the gateway reads.
struct MockTenantResolver {
    parents: std::collections::BTreeMap<Uuid, Uuid>,
}

impl MockTenantResolver {
    /// Builds the resolver over `(child, parent)` edges.
    fn from_pairs(pairs: &[(Uuid, Uuid)]) -> Self {
        Self {
            parents: pairs.iter().copied().collect(),
        }
    }

    /// The chain a resolver would return: closest first, self included.
    fn chain_of(&self, tenant: Uuid) -> Vec<tenant_resolver_sdk::TenantRef> {
        let mut chain = Vec::new();
        let mut current = tenant;
        loop {
            chain.push(tenant_resolver_sdk::TenantRef {
                id: tenant_resolver_sdk::TenantId(current),
                status: tenant_resolver_sdk::TenantStatus::Active,
                tenant_type: None,
                parent_id: self.parents.get(&current).copied().map(tenant_resolver_sdk::TenantId),
                self_managed: false,
            });
            match self.parents.get(&current) {
                Some(parent) => current = *parent,
                None => break,
            }
        }
        chain
    }
}

#[async_trait]
impl tenant_resolver_sdk::TenantResolverClient for MockTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound {
            tenant_id: id,
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError> {
        Err(tenant_resolver_sdk::TenantResolverError::Internal(
            "not modelled by the test resolver".to_owned(),
        ))
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[tenant_resolver_sdk::TenantId],
        _options: &tenant_resolver_sdk::GetTenantsOptions,
    ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError> {
        Ok(Vec::new())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::GetAncestorsOptions,
    ) -> Result<tenant_resolver_sdk::GetAncestorsResponse, tenant_resolver_sdk::TenantResolverError>
    {
        let mut chain = self.chain_of(id.0);
        let tenant = chain.remove(0);
        Ok(tenant_resolver_sdk::GetAncestorsResponse {
            tenant,
            ancestors: chain,
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::GetDescendantsOptions,
    ) -> Result<tenant_resolver_sdk::GetDescendantsResponse, tenant_resolver_sdk::TenantResolverError>
    {
        Ok(tenant_resolver_sdk::GetDescendantsResponse {
            tenant: self.chain_of(id.0).remove(0),
            descendants: Vec::new(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: tenant_resolver_sdk::TenantId,
        descendant_id: tenant_resolver_sdk::TenantId,
        _options: &tenant_resolver_sdk::IsAncestorOptions,
    ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
        Ok(self
            .chain_of(descendant_id.0)
            .iter()
            .skip(1)
            .any(|tenant| tenant.id.0 == ancestor_id.0))
    }
}

/// The extension the api-gateway injects after authenticating a caller.
async fn inject_security_context(mut request: Request, next: axum::middleware::Next) -> Response {
    let context = SecurityContext::builder()
        .subject_tenant_id(TENANT_A)
        .subject_id(SUBJECT_A)
        .bearer_token("e2e-token-tenant-a")
        .build()
        .expect("test security context builds");
    request.extensions_mut().insert(context);
    next.run(request).await
}

/// A stand-in upstream: echoes what it was sent, in the shapes the tests need.
#[derive(Clone)]
pub struct Upstream {
    addr: SocketAddr,
}

impl Upstream {
    /// Starts the in-process upstream.
    ///
    /// # Panics
    /// Panics when the listener cannot be bound or the server cannot start.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, upstream_router()).await.unwrap();
        });
        Self { addr }
    }

    /// `http://127.0.0.1:{port}`
    #[must_use]
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The port the upstream listens on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The host the upstream listens on.
    #[must_use]
    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }
}

/// How many `/sse/long` generators are alive right now.
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times the stand-in `IdP` has granted a token.
static TOKEN_ISSUANCES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many requests the stand-in upstream has answered on `/echo`.
static UPSTREAM_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many requests the stand-in upstream has answered on `/counted`.
static COUNTED_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many requests the stand-in upstream has served so far.
///
/// The counter is shared by every test in the process, so a reading is only
/// meaningful as a difference from an earlier one.
#[must_use]
pub fn upstream_hits() -> usize {
    UPSTREAM_HITS.load(std::sync::atomic::Ordering::SeqCst)
}

/// How many requests the stand-in upstream has served on `/counted` so far.
///
/// Only the test that watches a refusal also drives this route, so a reading
/// here is that test's alone.
#[must_use]
pub fn counted_hits() -> usize {
    COUNTED_HITS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The lifetime the stand-in `IdP` grants, in seconds.
pub const TOKEN_TTL_SECONDS: u64 = 3600;

/// A lifetime already inside the safety margin a token cache must keep.
pub const TOKEN_TTL_SECONDS_SHORT: u64 = 10;

/// Grants one token, named after its ordinal and the client that asked.
///
/// A caller that re-uses a cached token reads back the number it was first
/// given; a client whose credentials never resolved is named by nobody.
///
/// # Panics
/// Panics when the response cannot be serialised.
fn grant(body: &str, expires_in: u64) -> axum::response::Response {
    let issued = TOKEN_ISSUANCES.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let client = form_field(body, "client_id").unwrap_or_default();
    axum::response::IntoResponse::into_response((
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "access_token": format!("issued-{issued}-for-{client}"),
            "token_type": "Bearer",
            "expires_in": expires_in,
        })),
    ))
}

/// Reads one field out of a url-encoded form body.
fn form_field(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

/// How many tokens the stand-in `IdP` has granted so far.
#[must_use]
pub fn token_issuances() -> usize {
    TOKEN_ISSUANCES.load(std::sync::atomic::Ordering::SeqCst)
}

/// Decrements [`ACTIVE`] when the generator it guards is dropped.
struct ActiveStreams;

impl ActiveStreams {
    /// Registers one live generator.
    fn acquire() -> Self {
        ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for ActiveStreams {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Routes the stand-in upstream serves.
fn upstream_router() -> Router {
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use axum::routing::{any, get, post};

    Router::new()
        .route(
            "/echo",
            any(|request: Request| async move {
                UPSTREAM_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (parts, _) = request.into_parts();
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "method": parts.method.as_str(),
                        "path": parts.uri.path(),
                        "query": parts.uri.query(),
                        "headers": headers(&parts.headers),
                    })),
                )
                    .into_response()
            }),
        )
        .route("/post", post(|| async { (StatusCode::CREATED, "created") }))
        .route(
            "/counted",
            get(|| async {
                COUNTED_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::OK, "counted")
            }),
        )
        .route(
            "/host",
            get(|request: Request| async move {
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "host": request.headers().get("host")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default(),
                    })),
                )
            }),
        )
        .route(
            "/sse",
            get(|| async {
                let stream = async_stream::stream! {
                    for i in 0..3 {
                        yield Ok::<_, std::convert::Infallible>(
                            bytes::Bytes::from(format!("event: delta\ndata: {{\"i\":{i}}}\n\n")),
                        );
                    }
                    yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                        b"event: done\ndata: [DONE]\n\n",
                    ));
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("x-upstream-mark", "streamed")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }),
        )
        .route(
            "/sse/long",
            get(|| async {
                let guard = ActiveStreams::acquire();
                let stream = async_stream::stream! {
                    for i in 0..500 {
                        yield Ok::<_, std::convert::Infallible>(
                            bytes::Bytes::from(format!("event: delta\ndata: {{\"i\":{i}}}\n\n")),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    drop(guard);
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }),
        )
        .route("/sse/active", get(|| async {
            axum::Json(serde_json::json!({ "active": ACTIVE.load(std::sync::atomic::Ordering::SeqCst) }))
        }))
        .route(
            "/ws",
            get(|upgrade: axum::extract::WebSocketUpgrade| async move {
                upgrade.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        )
        .route(
            "/status/{code}",
            get(|path: axum::extract::Path<u16>| async move {
                let code = StatusCode::from_u16(path.0).unwrap_or(StatusCode::OK);
                (code, "upstream body")
            }),
        )
        .route(
            "/oauth/token",
            post(|body: String| async move { grant(&body, TOKEN_TTL_SECONDS) }),
        )
        .route(
            "/oauth/short-token",
            post(|body: String| async move { grant(&body, TOKEN_TTL_SECONDS_SHORT) }),
        )
}

/// Every header as `name → value`, so tests can assert what was forwarded.
fn headers(map: &http::HeaderMap) -> serde_json::Map<String, serde_json::Value> {
    let mut json = serde_json::Map::new();
    for (name, value) in map {
        json.insert(
            name.as_str().to_owned(),
            String::from_utf8_lossy(value.as_bytes()).into_owned().into(),
        );
    }
    json
}

/// A request to the gateway, with a JSON body if one is supplied.
#[must_use]
pub fn request(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    match body {
        Some(value) => builder
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// A response with its body already read, so a test can assert on both.
///
/// [`axum::body::Body`] is not `Clone`, so a response is consumed once, here,
/// and everything a test wants is taken out of it in one pass.
pub struct Recorded {
    /// HTTP status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: http::HeaderMap,
    /// The body parsed as JSON, or `Null` when it is not JSON.
    pub body: serde_json::Value,
    /// The body as text.
    pub raw: String,
}

impl Recorded {
    /// A response header value, if present and UTF-8.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Whether the gateway forwarded this response or generated it itself.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.header("x-oagw-error-source")
    }

    /// The problem document a gateway-generated failure carries.
    #[must_use]
    pub fn problem(&self) -> &serde_json::Value {
        &self.body
    }

    /// The problem `detail`, for asserting on the message.
    #[must_use]
    pub fn detail(&self) -> &str {
        self.body["detail"].as_str().unwrap_or_default()
    }
}

/// Reads a response out in one pass.
///
/// # Panics
/// Panics when the body cannot be read.
pub async fn record(response: Response) -> Recorded {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Recorded {
        status: parts.status,
        headers: parts.headers,
        body,
        raw,
    }
}

/// A request built from a mutable header set, for tests that need one.
#[must_use]
pub fn request_with(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}
