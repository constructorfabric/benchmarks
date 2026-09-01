// Created: 2026-08-31 by Constructor Tech
//! Shared harness for the OAGW management-API and proxy integration tests.
//!
//! Builds the gear's router exactly as `RestApiCapability::register_rest`
//! does (a `NoopOpenApiRegistry` stands in for the utoipa collector) and sends
//! requests with `tower::ServiceExt::oneshot`, injecting the
//! `SecurityContext` the tenant-resolver middleware would have produced.
//! [`ProxyHarness`] wires the proxy data plane onto the same router, sharing
//! the store between the control plane and the data plane exactly as
//! `Gear::init` does.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::lifecycle::UpstreamRemoval;
use oagw::domain::proxy::chain::{NoChain, TenantChain};
use oagw::domain::proxy::service::ProxyService;
use oagw::domain::service::OagwService;
use oagw::domain::store::{InMemoryStore, Store};
use oagw::domain::validation::ValidationPolicy;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// Canonical GTS id of the HTTP protocol (upstream schema `protocol`).
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// OAGW error-type prefix (DESIGN §3.3).
pub const ERR_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// GTS prefix of a custom-plugin instance id.
pub const TRANSFORM_PLUGIN_STEM: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// GTS prefix of a custom auth-plugin instance id.
pub const AUTH_PLUGIN_STEM: &str = "gts.cf.core.oagw.auth_plugin.v1~";

/// Canonical GTS id of the built-in `apikey` auth plugin.
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

// ── OpenAPI stand-in ─────────────────────────────────────────────────────

/// Registry that discards every operation; only route wiring is exercised.
pub struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Harness ──────────────────────────────────────────────────────────────

/// A built router plus the helpers the tests need to drive it.
pub struct Harness {
    router: Router,
}

impl Harness {
    /// Router with the graded configuration: plaintext upstreams allowed.
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy(|policy| {
            policy.allow_http_upstream = true;
        })
    }

    /// Router with a mutated policy, for tests that need the strict baseline.
    #[must_use]
    pub fn with_policy(mutate: impl FnOnce(&mut ValidationPolicy)) -> Self {
        let config = OagwConfig::default();
        let mut policy = config.validation_policy();
        mutate(&mut policy);
        let service = OagwService::new(policy, InMemoryStore::new());
        let openapi = NoopOpenApiRegistry;
        let router = oagw::api::routes::register_routes(Router::new(), &openapi, service);
        Self { router }
    }

    /// Send a JSON request as `tenant_id` and collect status, headers, body.
    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        tenant_id: Uuid,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<Reply> {
        send_json(&self.router, method, uri, tenant_id, payload, &[]).await
    }

    /// Send a request with extra headers (used for the trace-id middleware).
    pub async fn call_with_headers(
        &self,
        method: &str,
        uri: &str,
        tenant_id: Uuid,
        payload: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Reply> {
        send_json(&self.router, method, uri, tenant_id, payload, headers).await
    }
}

/// Send a JSON management request and collect the reply.
async fn send_json(
    router: &Router,
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    payload: Option<serde_json::Value>,
    headers: &[(&str, &str)],
) -> anyhow::Result<Reply> {
    let mut explicit = Vec::from(headers);
    let bytes = match &payload {
        Some(value) => {
            explicit.push(("content-type", "application/json"));
            serde_json::to_vec(value)?
        }
        None => Vec::new(),
    };
    send_raw(router, method, uri, tenant_id, &explicit, bytes).await
}

/// Send a request with a raw body and collect status, headers and body.
async fn send_raw(
    router: &Router,
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> anyhow::Result<Reply> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder = builder.header("content-length", body.len().to_string());
    let mut request = builder
        .body(Body::from(body))
        .context("building the request")?;
    request
        .extensions_mut()
        .insert(security_context(tenant_id)?);
    let response = router.clone().oneshot(request).await?;
    Reply::from_response(response).await
}

/// Send a proxy request and return the raw response, body included.
///
/// Unlike [`send_raw`] the body is not collected: the streaming tests need to
/// read the frames as they arrive.
async fn send_stream(
    router: &Router,
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    headers: &[(&str, &str)],
) -> anyhow::Result<axum::http::Response<Body>> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder
        .body(Body::empty())
        .context("building the request")?;
    request
        .extensions_mut()
        .insert(security_context(tenant_id)?);
    Ok(router.clone().oneshot(request).await?)
}

/// Status, headers and decoded body of one call.
pub struct Reply {
    /// Response status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Parsed JSON body (`Value::Null` when the body is empty).
    pub json: serde_json::Value,
    /// Raw body text.
    pub text: String,
}

impl Reply {
    async fn from_response(response: axum::http::Response<Body>) -> anyhow::Result<Self> {
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await?.to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let json = if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
        };
        Ok(Self {
            status: parts.status,
            headers: parts.headers,
            json,
            text,
        })
    }

    /// `application/problem+json` member lookup.
    pub fn problem_field(&self, name: &str) -> Option<&str> {
        self.json.get(name).and_then(serde_json::Value::as_str)
    }

    /// The `type` member of a problem body.
    pub fn problem_type(&self) -> Option<String> {
        self.problem_field("type").map(str::to_owned)
    }

    /// Visible value of a response header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

// ── Payload helpers ──────────────────────────────────────────────────────

/// Full `type` member of a problem body for a DESIGN §3.3 error slug.
pub fn problem_type(slug: &str) -> String {
    format!("{ERR_PREFIX}{slug}")
}

/// An `endpoint` object of the upstream schema.
pub fn endpoint(scheme: &str, host: &str, port: u16) -> serde_json::Value {
    serde_json::json!({ "scheme": scheme, "host": host, "port": port })
}

/// An upstream creation payload with an `https` endpoint pool.
pub fn https_upstream(host: &str, port: u16) -> serde_json::Value {
    serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [endpoint("https", host, port)] }
    })
}

/// An upstream creation payload over an IP pool (alias must be explicit).
pub fn ip_upstream(alias: Option<&str>) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [
            endpoint("https", "10.0.1.1", 443),
            endpoint("https", "10.0.1.2", 443),
        ] }
    });
    if let Some(alias) = alias {
        payload["alias"] = serde_json::Value::String(alias.to_owned());
    }
    payload
}

/// An HTTP route-match rule.
pub fn http_match(methods: &[&str], path: &str) -> serde_json::Value {
    serde_json::json!({ "http": { "methods": methods, "path": path } })
}

/// A route creation payload bound to `upstream_id`.
pub fn route_payload(upstream_id: Uuid, methods: &[&str], path: &str) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id.to_string(),
        "match": http_match(methods, path)
    })
}

/// A custom-plugin definition (ADR-0002 appendix A).
pub fn plugin_payload(name: &str, source: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "plugin_type": "transform",
        "source_code": source,
        "config": { "header": "x-trace" }
    })
}

/// GTS form of a resource id: `gts.cf.core.oagw.<type>.v1~<uuid>`.
pub fn gts_resource_id(kind: &str, id: Uuid) -> String {
    format!("gts.cf.core.oagw.{kind}.v1~{id}")
}

/// `SecurityContext` the tenant-resolver middleware would inject.
pub fn security_context(tenant_id: Uuid) -> anyhow::Result<SecurityContext> {
    Ok(SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(tenant_id)
        .build()?)
}

// ── Proxy harness ────────────────────────────────────────────────────────

/// Router that carries the management API **and** the proxy data plane.
///
/// Both sub-routers share one store, the way `Gear::init` wires them.
pub struct ProxyHarness {
    router: Router,
    store: Arc<dyn Store>,
    /// Tenant every request of the harness is issued from.
    tenant: Uuid,
}

/// Configuration of the graded deployment: plaintext upstreams allowed.
///
/// `proxy_timeout_secs` is lowered so a timeout test does not wait 30 s. The
/// SSRF switch is off because the deployment the gear ships in turns it off,
/// and the harness endpoints are loopback addresses the policy would refuse.
pub fn proxy_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
        },
        ..OagwConfig::default()
    }
}

impl ProxyHarness {
    /// Harness with the graded configuration and no tenant chain.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config_and_chain(&proxy_config(), Arc::new(NoChain))
    }

    /// Harness for `config` with an explicit tenant chain.
    #[must_use]
    pub fn with_config_and_chain(config: &OagwConfig, chain: Arc<dyn TenantChain>) -> Self {
        Self::with_credential_store(config, chain, None)
    }

    /// Harness for `config` with an explicit tenant chain and credential store.
    ///
    /// `None` is the degraded deployment: only the auth plugins that need no
    /// credential store are registered.
    #[must_use]
    pub fn with_credential_store(
        config: &OagwConfig,
        chain: Arc<dyn TenantChain>,
        credstore: Option<std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Self {
        let store: Arc<dyn Store> = InMemoryStore::new();
        let service = OagwService::new(config.validation_policy(), Arc::clone(&store));
        let client = ProxyService::build_client(config)
            .unwrap_or_else(|error| panic!("outbound client must build in tests: {error}"));
        let proxy = ProxyService::new(Arc::clone(&store), chain, client, credstore, config);
        let openapi = NoopOpenApiRegistry;
        // `Gear::init` also makes the data plane an observer of the control
        // plane's cascade deletions, so the harness wires the same seam: a
        // deleted upstream must leave no round-robin cursor and no token
        // bucket behind.
        service.observe_removals(Arc::clone(&proxy) as Arc<dyn UpstreamRemoval>);
        let router = oagw::api::routes::register_routes(Router::new(), &openapi, service);
        let router = oagw::api::routes::register_data_plane(router, &openapi, proxy);
        Self {
            router,
            store,
            tenant: Uuid::now_v7(),
        }
    }

    /// The store both sub-routers share, for direct seeding.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// The tenant the harness acts as.
    #[must_use]
    pub fn tenant(&self) -> Uuid {
        self.tenant
    }

    /// The router of the harness, for the tests that need a real listener:
    /// a handshake cannot be completed over an in-process `oneshot`.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// Insert an upstream record bypassing the write-path validation.
    ///
    /// Used for the fail-closed tests, which need a record the management API
    /// would have rejected.
    pub fn seed_upstream(&self, upstream: oagw::domain::model::Upstream) -> Uuid {
        let id = upstream.id;
        self.store
            .insert_upstream(upstream)
            .unwrap_or_else(|error| panic!("upstream must seed: {error}"));
        id
    }

    /// Send a proxy request with raw headers and body bytes.
    pub async fn proxy(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Reply> {
        send_raw(
            &self.router,
            method,
            uri,
            self.tenant,
            headers,
            Vec::from(body),
        )
        .await
    }

    /// Send a proxy request as an identity the test fixes.
    ///
    /// The token cache of the credential-injection plugins is keyed by the
    /// subject, so the cached-exchange tests need one caller across requests.
    pub async fn proxy_as_identity(
        &self,
        identity: &SecurityContext,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Reply> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder = builder.header("content-length", body.len().to_string());
        let mut request = builder
            .body(Body::from(Vec::from(body)))
            .context("building the request")?;
        request.extensions_mut().insert(identity.clone());
        let response = self.router.clone().oneshot(request).await?;
        Reply::from_response(response).await
    }

    /// Send a management request on the merged router.
    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<Reply> {
        send_json(&self.router, method, uri, self.tenant, payload, &[]).await
    }

    /// Send a proxy request as a tenant the harness does not own.
    pub async fn proxy_as(
        &self,
        method: &str,
        uri: &str,
        tenant_id: Uuid,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> anyhow::Result<Reply> {
        send_raw(
            &self.router,
            method,
            uri,
            tenant_id,
            headers,
            Vec::from(body),
        )
        .await
    }

    /// Send a proxy request and return the response **unconsumed**.
    ///
    /// The streaming tests drive the body themselves, so the reply must not be
    /// buffered into a [`Reply`].
    pub async fn proxy_unbuffered(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<axum::http::Response<Body>> {
        send_stream(&self.router, method, uri, self.tenant, headers).await
    }

    /// Send a proxy request whose body is streamed, i.e. framed chunked.
    ///
    /// A chunked request declares no length, so the body cap can only be
    /// enforced while the frames arrive — which is what the test observes.
    pub async fn proxy_chunked(
        &self,
        method: &str,
        uri: &str,
        chunks: &[&[u8]],
    ) -> anyhow::Result<Reply> {
        let mut builder = Request::builder().method(method).uri(uri);
        builder = builder.header("transfer-encoding", "chunked");
        let mut frames = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            frames.push(bytes::Bytes::copy_from_slice(chunk));
        }
        let body = Body::from_stream(futures_util::stream::iter(
            frames.into_iter().map(Ok::<_, std::convert::Infallible>),
        ));
        let mut request = builder.body(body).context("building the request")?;
        request
            .extensions_mut()
            .insert(security_context(self.tenant)?);
        let response = self.router.clone().oneshot(request).await?;
        Reply::from_response(response).await
    }
}

impl Default for ProxyHarness {
    fn default() -> Self {
        Self::new()
    }
}

// ── Domain-model seeding ─────────────────────────────────────────────────

/// An `http` endpoint on the loopback interface.
#[must_use]
pub fn loopback_endpoint(port: u16) -> oagw::domain::model::Endpoint {
    hostname_endpoint("127.0.0.1", port)
}

/// An `http` endpoint addressed by `host`.
///
/// A host name is what the target-host matrix of ADR-0001 needs: a pool whose
/// alias is derived from the shared suffix demands the pinning header.
#[must_use]
pub fn hostname_endpoint(host: &str, port: u16) -> oagw::domain::model::Endpoint {
    oagw::domain::model::Endpoint {
        scheme: oagw::domain::model::Scheme::Http,
        host: host.to_owned(),
        port,
    }
}

/// An `https` endpoint addressed by `host` on the standard port.
///
/// The standard port keeps the derived alias free of a `:port` suffix, which is
/// what makes an ambiguous pool demand `X-OAGW-Target-Host` (ADR-0001).
#[must_use]
pub fn tls_hostname_endpoint(host: &str) -> oagw::domain::model::Endpoint {
    oagw::domain::model::Endpoint {
        scheme: oagw::domain::model::Scheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

/// Read a streaming body to its end, tolerating a truncated transfer.
///
/// Returns the bytes that arrived and whether the body ended **before** the
/// upstream finished its framing — the observable outcome of a body the data
/// plane had to abort.
pub async fn read_to_end(body: axum::body::Body) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut body = body;
    let mut received = Vec::new();
    let mut truncated = false;
    loop {
        match body.frame().await {
            None => break,
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    received.extend_from_slice(&data);
                }
            }
            Some(Err(_)) => {
                truncated = true;
                break;
            }
        }
    }
    Ok((received, truncated))
}

/// Read frames until `needle` has been seen, without waiting for the body end.
///
/// The streaming tests use it to observe bytes that arrive **while** the
/// upstream is still connected.
///
/// # Errors
/// When the body ends or fails before `needle` was seen.
pub async fn read_until(body: &mut axum::body::Body, needle: &str) -> anyhow::Result<Vec<u8>> {
    let mut received = Vec::new();
    loop {
        if let Some(Ok(frame)) = body.frame().await {
            if let Ok(data) = frame.into_data() {
                received.extend_from_slice(&data);
            }
        } else {
            break;
        }
        if String::from_utf8_lossy(&received).contains(needle) {
            return Ok(received);
        }
    }
    anyhow::bail!(
        "the body ended before '{needle}' arrived; received: {}",
        String::from_utf8_lossy(&received)
    )
}

/// An upstream record over `endpoints` with an explicit `alias`.
///
/// Seeded directly, so the tests control the routing key instead of the
/// alias-derivation rules of the write path.
#[must_use]
pub fn domain_upstream(
    tenant_id: Uuid,
    alias: &str,
    endpoints: Vec<oagw::domain::model::Endpoint>,
    enabled: bool,
) -> oagw::domain::model::Upstream {
    oagw::domain::model::Upstream {
        id: Uuid::now_v7(),
        tenant_id,
        alias: alias.to_owned(),
        enabled,
        protocol: oagw::domain::model::Protocol::Http,
        endpoints,
        tags: Vec::new(),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        timestamps: oagw::domain::model::Timestamps {
            created_at: 0,
            updated_at: 0,
        },
    }
}

/// An enabled `http` route of `upstream_id` matching `path` for `methods`.
#[must_use]
pub fn domain_route(
    tenant_id: Uuid,
    upstream_id: Uuid,
    methods: &[oagw::domain::model::HttpMethod],
    path: &str,
    query_allowlist: &[&str],
) -> oagw::domain::model::Route {
    oagw::domain::model::Route {
        id: Uuid::now_v7(),
        tenant_id,
        upstream_id,
        enabled: true,
        match_rule: oagw::domain::model::RouteMatch {
            http: Some(oagw::domain::model::HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: query_allowlist
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                path_suffix_mode: oagw::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        tags: Vec::new(),
        plugins: None,
        rate_limit: None,
        cors: None,
        timestamps: oagw::domain::model::Timestamps {
            created_at: 0,
            updated_at: 0,
        },
    }
}

/// An event-stream route that allows every method, for the streaming tests.
#[must_use]
pub fn any_method() -> Vec<oagw::domain::model::HttpMethod> {
    Vec::from([
        oagw::domain::model::HttpMethod::Get,
        oagw::domain::model::HttpMethod::Post,
    ])
}

/// Tenant chain that reports the root tenant as the single ancestor.
///
/// Stands in for the `tenant_resolver` client, which no test in this crate can
/// reach without a deployment.
pub struct StaticTenantChain;

#[async_trait::async_trait]
impl TenantChain for StaticTenantChain {
    async fn ancestors(
        &self,
        _context: &SecurityContext,
        tenant_id: Uuid,
    ) -> oagw::OagwResult<Vec<Uuid>> {
        if tenant_id == Uuid::nil() {
            return Ok(Vec::new());
        }
        Ok(Vec::from([Uuid::nil()]))
    }
}

/// `X-OAGW-Target-Host` on the wire.
pub const TARGET_HOST: &str = "x-oagw-target-host";

/// `X-OAGW-Error-Source` on the wire.
pub const ERROR_SOURCE: &str = "x-oagw-error-source";

/// A raw TCP server driven by the test: it answers from a scripted closure.
///
/// `httpmock` covers the ordinary request/response cases; a scripted socket is
/// the only way to hand out a chunked body, an event stream or a connection
/// that never answers.
pub struct RawUpstream {
    listener: tokio::net::TcpListener,
}

impl RawUpstream {
    /// Bind a listener on an ephemeral loopback port.
    ///
    /// # Errors
    /// Propagated from the socket bind.
    pub async fn bind() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        Ok(Self { listener })
    }

    /// The port the listener bound.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |addr| addr.port())
    }

    /// Accept one connection, read its request head and answer with `response`.
    ///
    /// The request head is returned so the test can assert on it.
    ///
    /// # Errors
    /// Propagated from the socket operations.
    pub async fn serve_once(&self, response: &str) -> anyhow::Result<String> {
        let (mut socket, _) = self.listener.accept().await?;
        let received = read_request_head(&mut socket).await?;
        socket.write_all(response.as_bytes()).await?;
        socket.shutdown().await?;
        Ok(received)
    }

    /// Serve requests forever, answering each with `response`.
    ///
    /// The streaming tests keep a listener alive across several proxy calls.
    pub async fn serve_forever(&self, response: String) {
        while let Ok((socket, _)) = self.listener.accept().await {
            if self.answer(socket, &response).await.is_err() {
                break;
            }
        }
    }

    /// Answer a single accepted connection with `response`.
    async fn answer(
        &self,
        mut socket: tokio::net::TcpStream,
        response: &str,
    ) -> anyhow::Result<()> {
        let head = read_request_head(&mut socket).await.unwrap_or_default();
        tracing::debug!(request = %head, "raw upstream received a request");
        socket.write_all(response.as_bytes()).await?;
        if let Err(error) = socket.shutdown().await {
            tracing::debug!(error = %error, "raw upstream could not close the socket");
        }
        Ok(())
    }

    /// Accept one connection and never answer it.
    ///
    /// # Errors
    /// Propagated from the socket accept.
    pub async fn hang(&self) -> anyhow::Result<()> {
        let (socket, _) = self.listener.accept().await?;
        // The connection is dropped by the caller's timeout; parking keeps the
        // socket open so the gateway cannot read a response.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        drop(socket);
        Ok(())
    }

    /// Accept one connection, consume its request head and return the socket.
    ///
    /// The streaming tests hand the socket back to the test so it can dribble
    /// bytes out of the upstream whenever it wants.
    ///
    /// # Errors
    /// Propagated from the socket operations.
    pub async fn hand_over(&self, response: &str) -> anyhow::Result<tokio::net::TcpStream> {
        let (mut socket, _) = self.listener.accept().await?;
        read_request_head(&mut socket).await?;
        socket.write_all(response.as_bytes()).await?;
        Ok(socket)
    }
}

/// Read the request head of a connection.
///
/// The tests send empty bodies, so the first blank line is always the end of
/// the head.
async fn read_request_head(socket: &mut tokio::net::TcpStream) -> anyhow::Result<String> {
    let mut received = Vec::new();
    let mut buffer = [0u8; 2048];
    while !received.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
    }
    Ok(String::from_utf8_lossy(&received).to_string())
}

// ── log capture ──────────────────────────────────────────────────────────

/// A `tracing` subscriber that keeps every formatted event and every span.
///
/// Assertions on what the data plane *reports* need the log lines of a whole
/// proxy call, and there is no `tracing-subscriber` dev-dependency, so the
/// subscriber is written here. The tests that use it run on a single thread,
/// which is what [`tracing::subscriber::set_default`] needs.
///
/// Spans are kept as well, rendered the same way as the events: the audit
/// record of a request is an event emitted *inside* a span, so a test that
/// asserts what the record carries must be able to see what the span carries
/// too — the production subscriber formats both into one ingested line.
/// One `(span id, rendered line)` per span the subscriber saw.
type SpanLines = Vec<(tracing::Id, String)>;

#[derive(Clone, Default)]
pub struct LogCapture {
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// One `(id, rendered line)` per span the subscriber saw.
    spans: std::sync::Arc<std::sync::Mutex<SpanLines>>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl LogCapture {
    /// Every event the subscriber saw, in order.
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map_or_else(|_| Vec::new(), |lines| lines.clone())
    }

    /// Every span the subscriber saw, in the order it was opened.
    ///
    /// A span is rendered as `name field=value field=value`, with the fields
    /// recorded after it was opened (`status`, say) appended to the ones it
    /// was opened with.
    pub fn spans(&self) -> Vec<String> {
        self.spans.lock().map_or_else(
            |_| Vec::new(),
            |spans| spans.iter().map(|(_, line)| line.clone()).collect(),
        )
    }

    /// The id of the next span.
    fn next_id(&self) -> tracing::Id {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        tracing::Id::from_u64(id)
    }
}

/// Collects the message and the fields of one event.
struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.write(format_args!("{value:?}"));
        } else {
            self.write(format_args!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.write(format_args!(" {}={value}", field.name()));
    }
}

impl MessageVisitor {
    /// Appends one rendered field; a `String` never fails a write.
    fn write(&mut self, arguments: std::fmt::Arguments<'_>) {
        let _appended = std::fmt::Write::write_fmt(&mut self.0, arguments);
    }
}

/// Collects the attributes of one span as `(name, value)` pairs.
struct FieldVisitor(Vec<(String, String)>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

/// One span as `name field=value field=value`.
fn rendered_span(name: &str, fields: &[(String, String)]) -> String {
    let mut rendered = name.to_owned();
    for (field, value) in fields {
        append(&mut rendered, field, value);
    }
    rendered
}

/// Appends one `field=value` pair to a rendered span or record.
fn append(rendered: &mut String, field: &str, value: &str) {
    let _appended = write!(rendered, " {field}={value}");
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::Id {
        let id = self.next_id();
        let mut visitor = FieldVisitor(Vec::new());
        attributes.record(&mut visitor);
        if let Ok(mut spans) = self.spans.lock() {
            spans.push((
                id.clone(),
                rendered_span(attributes.metadata().name(), &visitor.0),
            ));
        }
        id
    }

    fn record(&self, span: &tracing::Id, values: &tracing::span::Record<'_>) {
        let mut visitor = FieldVisitor(Vec::new());
        values.record(&mut visitor);
        if let Ok(mut spans) = self.spans.lock()
            && let Some((_, line)) = spans.iter_mut().find(|(id, _)| id == span)
        {
            // A recorded field arrives after the span was opened, so it is
            // appended to the fields the span already carries.
            for (field, value) in visitor.0 {
                append(line, &field, &value);
            }
        }
    }

    fn record_follows_from(&self, _span: &tracing::Id, _follows_from: &tracing::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        if let Ok(mut lines) = self.lines.lock() {
            // The severity leads the line, because the audit record of §4.3 is
            // emitted at a level its outcome chooses and a test asserts that
            // choice on the same string it asserts the fields on.
            lines.push(format!("[{}] {}", event.metadata().level(), visitor.0));
        }
    }

    fn enter(&self, _span: &tracing::Id) {}

    fn exit(&self, _span: &tracing::Id) {}
}
