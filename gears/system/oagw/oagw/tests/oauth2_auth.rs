//! Data-plane tests for slice S6: the OAuth2 client-credentials auth plugin
//! (ADR-0008) over the proxy path.
//!
//! The harness is the slice-S5 one, with the token-cache configuration plumbed
//! through: the plugin engine is built the way [`oagw::OagwGear`] builds it —
//! [`oagw::PluginRegistries::with_builtins_and_config`] with the credential
//! store and the token-cache settings — and handed to [`oagw::ProxyHooks`], so
//! what runs here is the wiring the gear ships, not a reimplementation. Only the
//! tenant hierarchy and the credential store are injected, which is how the
//! tenant isolation and the fail-closed behaviour are asserted.
//!
//! Two local servers play the two ends the plugin sits between:
//!
//! * the **identity provider** is an `httpmock` server, because a token request
//!   is behaviour to assert (method, credential placement, grant type) and its
//!   call count is the observable that proves a cache hit;
//! * the **upstream** is the raw-socket echo server of the policy-layer tests,
//!   because "the upstream received this bearer" has to be read off what the
//!   upstream actually got.
//!
//! # FIPS
//!
//! The whole file is compiled out under `--features fips`: every test here
//! reaches its identity provider over a plaintext loopback socket, and under
//! fips the `token_endpoint` preset is [`toolkit_http::TransportSecurity::
//! TlsOnly`], so the fetch cannot be made against a plaintext mock at all —
//! the same reason `toolkit-http` compiles its own plaintext-HTTP test module
//! out. The unit-level fips half of that contract lives in
//! `the_token_request_stays_tls_only_under_fips` in the plugin's test module,
//! and the preset's posture is asserted by
//! `toolkit-http/tests/fips_default_transport.rs`.

#![cfg(not(feature = "fips"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use credstore_sdk::test_util::MockCredStoreClient;
use http_body_util::BodyExt;
use httpmock::MockServer;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::DataPlaneService;
use oagw::OagwConfig;
use oagw::PluginEngineService;
use oagw::PluginRegistries;
use oagw::ProxyHooks;
use oagw::RateLimitLimiter;
use oagw::RateLimitService;
use oagw::TenantHierarchy;
use oagw::TokenCacheConfig;
use oagw::api::rest::proxy_routes::register_proxy_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use oagw::domain::storage::{RouteStore, UpstreamStore};
use oagw::domain::types::{
    AuthConfig, Endpoint, HttpMatch, PathSuffixMode, Protocol, Route, RouteMatch, RouteMethod,
    RouteSpec, Scheme, ServerConfig, SharingMode, Upstream, UpstreamSpec,
};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// The proxy path (gear-relative, without `/api`).
const PROXY: &str = "/oagw/v1/proxy";

/// The calling tenant of most of this file's requests.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0002);

/// A second tenant, for the cache-isolation tests.
const OTHER_TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0004);

/// The subject most of this file's requests are made by.
const SUBJECT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0005);

/// A second subject of the same tenant.
const OTHER_SUBJECT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0006);

/// The GTS type id of a 500 for an unresolvable `cred://` reference.
const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";

/// The GTS type id of a 502 for an identity provider that refused the exchange.
const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";

/// The OAuth2 client-credentials plugin identifier with credentials in the body.
const FORM_REF: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// The OAuth2 client-credentials plugin identifier with credentials in the
/// `Authorization` header.
const BASIC_REF: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// The credential reference the OAuth2 bindings name for the client identifier.
const CLIENT_ID_REFERENCE: &str = "cred://vendor-client-id";

/// The credential reference the OAuth2 bindings name for the client secret.
const CLIENT_SECRET_REFERENCE: &str = "cred://vendor-client-secret";

/// The secret the credential store holds for [`CLIENT_ID_REFERENCE`].
const CLIENT_ID: &str = "the-client-id";

/// The secret the credential store holds for [`CLIENT_SECRET_REFERENCE`].
const CLIENT_SECRET: &str = "the-client-secret";

/// `base64("the-client-id:the-client-secret")` (RFC 6749 §2.3.1).
const BASIC_CREDENTIALS: &str = "Basic dGhlLWNsaWVudC1pZDp0aGUtY2xpZW50LXNlY3JldA==";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Minimal OpenAPI registry: records nothing, returns the schema name.
struct NoopOpenApiRegistry;

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

/// Ancestor-chain stub: the chain is what the proxy is allowed to see.
struct StubHierarchy {
    chain: Vec<Uuid>,
}

#[async_trait]
impl TenantHierarchy for StubHierarchy {
    async fn chain(&self, _security: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        let mut chain = vec![tenant];
        chain.extend(self.chain.iter().copied());
        chain
    }
}

/// A proxy stack plus the stores a test seeds configuration into.
struct Harness {
    router: Router,
    upstreams: Arc<UpstreamStore>,
    routes: Arc<RouteStore>,
}

/// A proxy stack whose plugin engine resolves credentials through `credstore`
/// and caches tokens for `token_cache`.
fn harness_with(
    ancestors: Vec<Uuid>,
    credstore: Option<Arc<dyn credstore_sdk::api::CredStoreClientV1>>,
    token_cache: TokenCacheConfig,
) -> Harness {
    // The loopback mock is plaintext, so the gate the schema ships with is open
    // here, as the e2e configuration opens it.
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let control_plane = Arc::new(ControlPlaneService::new(config));
    let upstreams = Arc::clone(control_plane.upstream_store());
    let routes = Arc::clone(control_plane.route_store());

    let hierarchy: Arc<dyn TenantHierarchy> = Arc::new(StubHierarchy { chain: ancestors });

    // The same three hooks, in the same construction order, the gear installs.
    let hooks = ProxyHooks::new(
        Some(Arc::new(RateLimitService::new(Arc::new(
            RateLimitLimiter::new(),
        )))),
        Some(Arc::new(oagw::CorsService)),
        Some(Arc::new(PluginEngineService::new(
            PluginRegistries::with_builtins_and_config(credstore, token_cache),
            Arc::clone(control_plane.plugin_store()),
        ))),
    );

    let data_plane = Arc::new(
        DataPlaneService::new(
            config,
            Arc::clone(&control_plane),
            Arc::clone(&upstreams),
            Arc::clone(&routes),
        )
        .with_tenant_hierarchy(hierarchy)
        .with_hooks(hooks),
    );

    Harness {
        router: register_proxy_routes(Router::new(), &NoopOpenApiRegistry, data_plane),
        upstreams,
        routes,
    }
}

/// A proxy stack with the default token cache and the given credential store.
fn harness(credstore: Option<Arc<dyn credstore_sdk::api::CredStoreClientV1>>) -> Harness {
    harness_with(Vec::new(), credstore, TokenCacheConfig::default())
}

/// A credential store holding the two secrets the OAuth2 bindings name.
///
/// The store is keyed the way [`oagw::SecretResolver`] looks references up:
/// without the `cred://` scheme.
fn credential_store() -> Arc<dyn credstore_sdk::api::CredStoreClientV1> {
    let reference = |value: &str| value.trim_start_matches("cred://").to_owned();
    Arc::new(MockCredStoreClient::with_secrets(vec![
        (reference(CLIENT_ID_REFERENCE), CLIENT_ID.to_owned()),
        (reference(CLIENT_SECRET_REFERENCE), CLIENT_SECRET.to_owned()),
    ]))
}

/// An `http` endpoint pointing at `server`.
fn http_endpoint(server: &MockServer) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: server.port(),
    }
}

/// An HTTP server on a loopback port that answers every request with the headers
/// it received, as JSON.
///
/// httpmock matches requests against expectations, which is the right tool for
/// the *identity provider*; these tests need to *see* the forwarded credential,
/// so the upstream socket is handled directly.
struct EchoServer {
    endpoint: Endpoint,
}

/// Start an echo server on a loopback port.
async fn echo_server() -> EchoServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback address binds");
    let port = listener.local_addr().expect("the address is known").port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let _ = echo_once(&mut socket).await;
        }
    });

    EchoServer {
        endpoint: Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port,
        },
    }
}

/// Read one request, answer it with the headers it carried.
async fn echo_once(socket: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let mut headers = std::collections::BTreeMap::new();
    for line in String::from_utf8_lossy(&raw).split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_owned());
        }
    }

    let body = serde_json::to_string(&json!({ "headers": headers })).expect("the echo is JSON");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

impl EchoServer {
    /// The endpoint to configure the upstream with.
    fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// The header the upstream reports having received.
    fn received(&self, body: &[u8], name: &str) -> Option<String> {
        let received: Value = serde_json::from_slice(body).expect("the echo is JSON");
        received["headers"][name].as_str().map(ToOwned::to_owned)
    }
}

/// A mock that answers every request with a 200 and a fixed body.
fn mock_ok(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|_when, then| {
        then.status(200).body("ok");
    })
}

/// The token endpoint of `server`, as an `auth.config` value.
fn token_endpoint(server: &MockServer) -> String {
    format!("http://127.0.0.1:{}/token", server.port())
}

/// An `auth` binding of the OAuth2 client-credentials plugin.
fn oauth2_auth(plugin_type: &str, config: Value) -> AuthConfig {
    AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing: SharingMode::Private,
        config: Some(config),
    }
}

/// The `auth.config` of a binding that exchanges credentials at `server`.
fn direct_config(server: &MockServer) -> Value {
    json!({
        "token_endpoint": token_endpoint(server),
        "client_id_ref": CLIENT_ID_REFERENCE,
        "client_secret_ref": CLIENT_SECRET_REFERENCE,
        "scopes": "read write",
    })
}

/// The `auth.config` of a binding that discovers its token endpoint at `server`
/// through OIDC discovery.
fn issuer_config(server: &MockServer) -> Value {
    json!({
        "issuer_url": format!("http://127.0.0.1:{}", server.port()),
        "client_id_ref": CLIENT_ID_REFERENCE,
        "client_secret_ref": CLIENT_SECRET_REFERENCE,
        "scopes": "read write",
    })
}

/// Seed an upstream record pointing at `endpoint`, whose spec is `amend`.
///
/// The spec is validated first, exactly as a management write would be, so a
/// test that seeds an invalid binding fails here rather than passing vacuously.
fn seed_upstream(
    harness: &Harness,
    tenant: Uuid,
    alias: &str,
    endpoint: Endpoint,
    amend: impl FnOnce(&mut UpstreamSpec),
) -> Uuid {
    let mut spec = UpstreamSpec {
        alias: Some(alias.to_owned()),
        server: ServerConfig {
            endpoints: vec![endpoint],
        },
        protocol: Protocol::Http,
        ..UpstreamSpec::default()
    };
    amend(&mut spec);
    let spec = spec.validate().expect("the upstream spec is valid");

    harness
        .upstreams
        .insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        })
        .expect("the upstream inserts")
        .id
}

/// Seed a plain `GET` route for `path`.
fn seed_route(harness: &Harness, tenant: Uuid, upstream: Uuid, path: &str) -> Uuid {
    harness
        .routes
        .insert(Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: upstream,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id: upstream,
                match_rules: RouteMatch {
                    http: Some(HttpMatch {
                        methods: vec![RouteMethod::Get],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        })
        .expect("the route inserts")
        .id
}

/// A `SecurityContext` for `tenant` with an explicit subject.
fn security_context(tenant: Uuid, subject: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// Send a proxied request and return `(status, headers, body)`.
async fn proxy(
    harness: &Harness,
    tenant: Uuid,
    subject: Uuid,
    path_suffix: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("{PROXY}/{path_suffix}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .extension(security_context(tenant, subject))
        .body(Body::empty())
        .expect("the request builds");

    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("the router responds");
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body is readable")
        .to_bytes()
        .to_vec();

    (status, response_headers, bytes)
}

/// The problem document of a gateway error.
fn problem(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("gateway errors are problem+json")
}

/// The `name` header of a response, as a string.
fn header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A token request mock that answers `token` with `expires_in` seconds.
fn token_mock<'a>(server: &'a MockServer, token: &str, expires_in: u64) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#
            ));
    })
}

// ---------------------------------------------------------------------------
// The token cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_identity_provider_call_serves_two_consecutive_requests() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-cached", 3600);
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (first, _, first_body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;
    let (second, _, second_body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    assert_eq!(token.calls(), 1, "one exchange, not one per request");

    assert_eq!(
        upstream.received(&first_body, "authorization").as_deref(),
        Some("Bearer tok-cached")
    );
    assert_eq!(
        upstream.received(&second_body, "authorization").as_deref(),
        Some("Bearer tok-cached"),
        "the second request is served the same cached token: {second_body:?}"
    );
}

#[tokio::test]
async fn the_callers_own_credential_is_replaced_by_the_gateways() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-gateway", 3600);
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(BASIC_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(
        &harness,
        TENANT,
        SUBJECT,
        "api.graph.com/v1",
        &[("authorization", "Bearer caller-token")],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(token.calls(), 1);
    assert_eq!(
        upstream.received(&body, "authorization").as_deref(),
        Some("Bearer tok-gateway"),
        "the upstream sees the gateway's credential, not the caller's: {body:?}"
    );
}

#[tokio::test]
async fn another_subject_fetches_its_own_token() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-per-subject", 3600);
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    for subject in [SUBJECT, SUBJECT, OTHER_SUBJECT, SUBJECT] {
        let (status, _, body) = proxy(&harness, TENANT, subject, "api.graph.com/v1", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            upstream.received(&body, "authorization").as_deref(),
            Some("Bearer tok-per-subject")
        );
    }

    // Two subjects, so two entries: the repeat of the first subject is served
    // from its entry rather than costing a third exchange.
    assert_eq!(
        token.calls(),
        2,
        "one entry per subject, not one per upstream"
    );
}

#[tokio::test]
async fn another_tenant_never_receives_another_tenants_token() {
    // Two identity providers, one per tenant, issuing different tokens: what
    // one tenant's upstream receives can then only be its own tenant's token.
    let idp_of_tenant = MockServer::start();
    let idp_of_other = MockServer::start();
    let tenant_token = token_mock(&idp_of_tenant, "tok-tenant-a", 3600);
    let other_token = token_mock(&idp_of_other, "tok-tenant-b", 3600);
    let upstream = echo_server().await;
    let other_upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp_of_tenant)));
        },
    );
    let other_id = seed_upstream(
        &harness,
        OTHER_TENANT,
        "api.graph.com",
        other_upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp_of_other)));
        },
    );
    seed_route(&harness, TENANT, id, "/");
    seed_route(&harness, OTHER_TENANT, other_id, "/");

    let (_, _, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;
    let (_, _, other_body) = proxy(&harness, OTHER_TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(tenant_token.calls(), 1);
    assert_eq!(other_token.calls(), 1);
    assert_eq!(
        upstream.received(&body, "authorization").as_deref(),
        Some("Bearer tok-tenant-a"),
        "the calling tenant's own token: {body:?}"
    );
    assert_eq!(
        other_upstream
            .received(&other_body, "authorization")
            .as_deref(),
        Some("Bearer tok-tenant-b"),
        "never the other tenant's token: {other_body:?}"
    );
}

#[tokio::test]
async fn a_different_configuration_fetches_its_own_token() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-per-config", 3600);
    let upstream = echo_server().await;
    // The token cache is as small as the gear configuration allows it to be
    // made, so the two bindings cannot both stay cached.
    let harness = harness_with(
        Vec::new(),
        Some(credential_store()),
        TokenCacheConfig {
            ttl: Duration::from_secs(300),
            capacity: 1,
        },
    );

    let mut narrow = direct_config(&idp);
    let mut wide = direct_config(&idp);
    narrow["scopes"] = json!("read");
    wide["scopes"] = json!("read write");

    let narrow_id = seed_upstream(
        &harness,
        TENANT,
        "api.narrow.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, narrow));
        },
    );
    let wide_id = seed_upstream(
        &harness,
        TENANT,
        "api.wide.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, wide));
        },
    );
    seed_route(&harness, TENANT, narrow_id, "/");
    seed_route(&harness, TENANT, wide_id, "/");

    for _ in 0..2 {
        let (status, _, _) = proxy(&harness, TENANT, SUBJECT, "api.narrow.com/v1", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = proxy(&harness, TENANT, SUBJECT, "api.wide.com/v1", &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    // Two entries that one slot cannot hold: every request re-fetches, which is
    // what the configured capacity costs, and never a shared token.
    assert_eq!(token.calls(), 4);
}

#[tokio::test]
async fn a_cache_entry_expires_and_the_next_request_exchanges_again() {
    let idp = MockServer::start();
    // `expires_in: 31` leaves 31s minus the 30s safety margin: an entry that
    // lives for one second, the shortest TTL the gear-level configuration can
    // produce for a token the IdP reports a lifetime for.
    let token = token_mock(&idp, "tok-expiring", 31);
    let upstream = echo_server().await;
    let harness = harness_with(
        Vec::new(),
        Some(credential_store()),
        TokenCacheConfig {
            ttl: Duration::from_secs(300),
            capacity: 16,
        },
    );

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (first, _, _) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;
    let (second, _, _) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    assert_eq!(token.calls(), 1, "two requests, one exchange");

    // One and a half seconds: past the entry's one-second TTL, and still quick
    // enough for a test.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let (third, _, third_body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(third, StatusCode::OK);
    assert_eq!(
        token.calls(),
        2,
        "the expired entry cost a second exchange, so nothing is cached forever"
    );
    assert_eq!(
        upstream.received(&third_body, "authorization").as_deref(),
        Some("Bearer tok-expiring"),
        "the third request is served the token of the second exchange"
    );
}

#[tokio::test]
async fn an_issuer_url_binding_resolves_its_token_endpoint_through_oidc_discovery() {
    let idp = MockServer::start();
    // The discovery document names the token endpoint the same server serves,
    // which is what lets the two mocks be asserted independently: the exchange
    // can only have gone where the document said.
    let discovery = idp.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/.well-known/openid-configuration");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"token_endpoint":"{}"}}"#,
                token_endpoint(&idp)
            ));
    });
    let token = token_mock(&idp, "tok-discovered", 3600);
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, issuer_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(status, StatusCode::OK);
    discovery.assert();
    token.assert();
    assert_eq!(
        upstream.received(&body, "authorization").as_deref(),
        Some("Bearer tok-discovered"),
        "the token of the endpoint the discovery document named: {body:?}"
    );
}

// ---------------------------------------------------------------------------
// Credential placement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_basic_variant_authenticates_in_the_authorization_header() {
    let idp = MockServer::start();
    let token = idp.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .header("authorization", BASIC_CREDENTIALS)
            .body_includes("grant_type=client_credentials")
            .body_includes("scope=read+write");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-basic","expires_in":3600,"token_type":"Bearer"}"#);
    });
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.basic.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(BASIC_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.basic.com/v1", &[]).await;

    assert_eq!(status, StatusCode::OK);
    token.assert();
    assert_eq!(
        upstream.received(&body, "authorization").as_deref(),
        Some("Bearer tok-basic")
    );
}

#[tokio::test]
async fn the_form_variant_authenticates_in_the_request_body() {
    let idp = MockServer::start();
    // A mock that *requires* an `Authorization` header must not be hit by a
    // `Form` client, which is what proves the credentials are not in it.
    let basic_only = idp.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .header_exists("authorization");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-wrong-variant"}"#);
    });
    let form = idp.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body_includes("grant_type=client_credentials")
            .body_includes("scope=read+write")
            .body_includes(format!("client_id={CLIENT_ID}"))
            .body_includes(format!("client_secret={CLIENT_SECRET}"));
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"access_token":"tok-form","expires_in":3600,"token_type":"Bearer"}"#);
    });
    let upstream = echo_server().await;
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.form.com",
        upstream.endpoint(),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.form.com/v1", &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        basic_only.calls(),
        0,
        "Form must not send Basic credentials"
    );
    form.assert();
    assert_eq!(
        upstream.received(&body, "authorization").as_deref(),
        Some("Bearer tok-form")
    );
}

// ---------------------------------------------------------------------------
// Failing closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unresolvable_credential_reference_fails_closed() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-never-fetched", 3600);
    let upstream = MockServer::start();
    let served = mock_ok(&upstream);
    // The store holds nothing: the reference cannot be resolved.
    let harness = harness(Some(Arc::new(MockCredStoreClient::empty())));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        http_endpoint(&upstream),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, headers, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("gateway"));
    let document = problem(&body);
    assert_eq!(document["type"], SECRET_NOT_FOUND);
    assert_eq!(served.calls(), 0, "no unauthenticated request is forwarded");
    assert_eq!(
        token.calls(),
        0,
        "no token is fetched for a secret that is not there"
    );
}

#[tokio::test]
async fn a_gateway_without_a_credential_store_fails_closed() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-never-fetched", 3600);
    let upstream = MockServer::start();
    let served = mock_ok(&upstream);
    // The host published no credential store: the request that needs one must
    // not be forwarded without it.
    let harness = harness(None);

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        http_endpoint(&upstream),
        |spec| {
            spec.auth = Some(oauth2_auth(BASIC_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(problem(&body)["type"], SECRET_NOT_FOUND);
    assert_eq!(served.calls(), 0, "nothing is forwarded unauthenticated");
    assert_eq!(token.calls(), 0);
}

#[tokio::test]
async fn an_identity_provider_that_refuses_the_client_fails_closed_and_is_not_cached() {
    let idp = MockServer::start();
    let refused = idp.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/token");
        then.status(401).body(r#"{"error":"invalid_client"}"#);
    });
    let upstream = MockServer::start();
    let served = mock_ok(&upstream);
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        http_endpoint(&upstream),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    for _ in 0..2 {
        let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let document = problem(&body);
        assert_eq!(document["type"], DOWNSTREAM_ERROR);
        assert_eq!(served.calls(), 0, "nothing is forwarded unauthenticated");
        let detail = document["detail"].as_str().unwrap_or_default();
        assert!(
            !detail.contains(CLIENT_SECRET),
            "the client secret never reaches the problem document: {detail}"
        );
        assert!(
            !detail.contains("invalid_client"),
            "nor does the identity provider's response body: {detail}"
        );
    }

    assert_eq!(
        refused.calls(),
        2,
        "a failed fetch is never cached: both requests retry the identity provider"
    );
}

#[tokio::test]
async fn an_unreachable_token_endpoint_fails_closed() {
    // Nothing listens on port 1: the identity provider is simply not there.
    let unreachable = json!({
        "token_endpoint": "http://127.0.0.1:1/token",
        "client_id_ref": CLIENT_ID_REFERENCE,
        "client_secret_ref": CLIENT_SECRET_REFERENCE,
    });
    let upstream = MockServer::start();
    let served = mock_ok(&upstream);
    let harness = harness(Some(credential_store()));

    let id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        http_endpoint(&upstream),
        |spec| {
            spec.auth = Some(oauth2_auth(BASIC_REF, unreachable));
        },
    );
    seed_route(&harness, TENANT, id, "/");

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(problem(&body)["type"], DOWNSTREAM_ERROR);
    assert_eq!(served.calls(), 0, "nothing is forwarded unauthenticated");
}

// ---------------------------------------------------------------------------
// Where the token may and may not appear
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cached_token_never_reaches_an_error_document() {
    let idp = MockServer::start();
    let token = token_mock(&idp, "tok-must-stay-hidden", 3600);
    let harness = harness(Some(credential_store()));

    // The binding that caches the token...
    let healthy = MockServer::start();
    mock_ok(&healthy);
    let served_id = seed_upstream(
        &harness,
        TENANT,
        "api.graph.com",
        http_endpoint(&healthy),
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, served_id, "/");

    // ...and a second binding with the *same* configuration, whose upstream is
    // dead. Same tenant, same subject, same config: the token is served from
    // the cache, and then the request fails on the upstream.
    let dead_id = seed_upstream(
        &harness,
        TENANT,
        "api.dead.com",
        Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: 1,
        },
        |spec| {
            spec.auth = Some(oauth2_auth(FORM_REF, direct_config(&idp)));
        },
    );
    seed_route(&harness, TENANT, dead_id, "/");

    let (status, _, _) = proxy(&harness, TENANT, SUBJECT, "api.graph.com/v1", &[]).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = proxy(&harness, TENANT, SUBJECT, "api.dead.com/v1", &[]).await;
    assert!(
        status.is_server_error(),
        "the second request fails on its upstream: {status}"
    );
    assert_eq!(token.calls(), 1, "the token came from the cache");

    let document = String::from_utf8_lossy(&body).into_owned();
    assert!(
        !document.contains("tok-must-stay-hidden"),
        "the bearer token never reaches the response: {document}"
    );
    assert!(
        !document.contains(CLIENT_SECRET),
        "the client secret never reaches the response: {document}"
    );
    assert!(
        !document.contains(CLIENT_ID),
        "the client identifier never reaches the response: {document}"
    );
}
