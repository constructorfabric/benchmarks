//! Integration tests for the plugin runtime and rate limiting
//! (`cpt-cf-oagw-feature-plugin-runtime`).
//!
//! Drives a real `axum::Router` (assembled the same way `OagwGear::register_rest`
//! assembles it) with `tower::ServiceExt::oneshot`, using `httpmock` both as
//! the proxied upstream and, for the `OAuth2` client-credentials tests, as the
//! token endpoint, so each test can assert both what the upstream actually
//! received and how many times the token endpoint was called. Every §6
//! acceptance criterion is exercised here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::test_util::MockCredStoreClient;
use http_body_util::BodyExt;
use httpmock::MockServer;
use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::model::{
    Algorithm, AuthConfig, Burst, Endpoint, PluginItem, PluginType, PluginsConfig, Protocol,
    RateLimitConfig, RateLimitScope, Route, RouteMethod, Scheme, ServerConfig, Sharing, Strategy,
    Sustained, Upstream, Window, gts_plugin_id,
};
use oagw::domain::plugin::registry::{
    APIKEY_AUTH_PLUGIN_NAME, OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME,
    OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME, REQUEST_ID_TRANSFORM_PLUGIN_NAME,
    REQUIRED_HEADERS_GUARD_PLUGIN_NAME,
};
use oagw::state::ControlPlaneState;
use serde_json::json;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared fixtures.
// ---------------------------------------------------------------------------

fn router_for(
    state: Arc<ControlPlaneState>,
    config: OagwConfig,
    tenant_id: Uuid,
    subject_id: Uuid,
    credstore: Arc<dyn CredStoreClientV1>,
) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.proxy_timeout_secs.max(1)))
        .build()
        .expect("client must build");
    let ctx = SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("security context must build");

    register_routes(Router::new(), &openapi)
        .layer(Extension(state))
        .layer(Extension(Arc::new(config)))
        .layer(Extension(Arc::new(client)))
        .layer(Extension(ctx))
        .layer(Extension(credstore))
}

fn default_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

fn http_endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: host.to_owned(),
        port: Some(port),
    }
}

fn mock_endpoint(server: &MockServer) -> Endpoint {
    http_endpoint(&server.host(), server.port())
}

fn upstream_with(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.to_owned(),
        tags: Vec::new(),
        server: ServerConfig { endpoints },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn simple_route(upstream_id: Uuid, path: &str, methods: &[RouteMethod]) -> Route {
    use oagw::domain::model::{HttpMatch, MatchConfig, PathSuffixMode};
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        tags: Vec::new(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        enabled: true,
        priority: 0,
    }
}

fn install(state: &ControlPlaneState, tenant_id: Uuid, upstream: Upstream, route: Route) {
    state
        .tenant(tenant_id)
        .upstreams
        .insert(upstream.id, upstream);
    state.tenant(tenant_id).routes.insert(route.id, route);
}

async fn send(router: Router, request: Request<Body>) -> axum::response::Response {
    router
        .oneshot(request)
        .await
        .expect("router call must succeed")
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).expect("body must be JSON")
}

fn plugin_ref(plugin_type: PluginType, name: &str) -> String {
    gts_plugin_id(plugin_type, name)
}

fn auth_config(plugin_type_name: &str, config: serde_json::Value) -> AuthConfig {
    AuthConfig {
        auth_type: Some(plugin_ref(PluginType::Auth, plugin_type_name)),
        sharing: Sharing::Private,
        config: Some(config),
    }
}

fn guard_binding(config: serde_json::Value) -> PluginItem {
    PluginItem::WithConfig {
        plugin_ref: plugin_ref(PluginType::Guard, REQUIRED_HEADERS_GUARD_PLUGIN_NAME),
        config: Some(config),
    }
}

fn plugins_config(items: Vec<PluginItem>) -> PluginsConfig {
    PluginsConfig {
        sharing: Sharing::Private,
        items,
    }
}

fn rate_limit(rate: u32, window: Window, burst: Option<u32>, cost: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: Sharing::Private,
        algorithm: Algorithm::TokenBucket,
        sustained: Sustained { rate, window },
        burst: burst.map(|capacity| Burst { capacity }),
        scope: RateLimitScope::Tenant,
        strategy: Strategy::Reject,
        cost,
    }
}

fn token_json(token: &str, expires_in: u64) -> String {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
}

// ---------------------------------------------------------------------------
// Api-key auth injection (`cpt-cf-oagw-algo-apikey-injection`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-plugin-runtime-it-apikey-header-01
#[tokio::test]
async fn an_apikey_header_binding_injects_the_resolved_credential() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header("x-api-key", "sk-resolved-value");
        then.status(200).body("ok");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("apikey.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"}),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved-value".to_owned(),
        )]));
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-plugin-runtime-it-apikey-header-01

#[tokio::test]
async fn an_apikey_query_binding_sets_the_query_parameter_and_injects_no_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .query_param("api_key", "sk-resolved-value")
            .header_missing("x-api-key");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("apikey-query.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "query", "name": "api_key", "credential_ref": "cred://openai-key"}),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved-value".to_owned(),
        )]));
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey-query.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}

// @cpt-begin:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-plugin-runtime-it-apikey-replace-01
#[tokio::test]
async fn an_apikey_header_binding_replaces_a_caller_supplied_value() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header("x-api-key", "sk-resolved-value");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("apikey-replace.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"}),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved-value".to_owned(),
        )]));
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey-replace.example.com/v1/items")
        .header("x-api-key", "caller-supplied")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}
// @cpt-end:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-plugin-runtime-it-apikey-replace-01

// @cpt-begin:cpt-cf-oagw-algo-plugin-failure-mapping:p2:inst-plugin-runtime-it-upstream-401-01
/// A credentialed request an upstream itself refuses (its own `401`, not a
/// gateway-side authentication failure) still passes through unmodified,
/// exactly like any other upstream status code, with exactly one upstream
/// call made.
#[tokio::test]
async fn an_upstream_401_passes_through_with_upstream_source_and_exactly_one_call() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header("x-api-key", "sk-resolved-value");
        then.status(401).body(r#"{"error":"invalid_token"}"#);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("apikey-401.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"}),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-resolved-value".to_owned(),
        )]));
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey-401.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    assert_eq!(mock.calls(), 1);
    let body = body_bytes(response).await;
    assert_eq!(body, br#"{"error":"invalid_token"}"#);
}
// @cpt-end:cpt-cf-oagw-algo-plugin-failure-mapping:p2:inst-plugin-runtime-it-upstream-401-01

// @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-no-leak-on-rejection-01
/// Given any chain rejection, no problem document field or response header
/// contains a resolved credential value. Log-record inspection is out of
/// reach of this integration harness (no capture hook exists); the response
/// surface is the only channel a caller (or an attacker reading the wire)
/// could actually observe, so it is asserted exhaustively here: the whole
/// raw body, and every header value.
#[tokio::test]
async fn a_chain_rejection_never_leaks_the_resolved_credential_in_the_response() {
    const RESOLVED_CREDENTIAL: &str = "sk-must-never-leak-12345";

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with(
        "apikey-guard-reject.example.com",
        vec![mock_endpoint(&server)],
    );
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "cred://openai-key"}),
    ));
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": "x-must-be-present"}),
    )]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            RESOLVED_CREDENTIAL.to_owned(),
        )]));
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    // Satisfies neither the guard (missing `x-must-be-present`) — auth runs
    // first in the chain and injects the credential into the request headers
    // before the guard rejects, so the credential is present in-flight by
    // the time the rejection is rendered.
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey-guard-reject.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);

    for (name, value) in response.headers() {
        assert!(
            value
                .to_str()
                .is_ok_and(|v| !v.contains(RESOLVED_CREDENTIAL)),
            "header '{name}' must never carry the resolved credential"
        );
    }
    let body = body_bytes(response).await;
    let body_text = String::from_utf8_lossy(&body);
    assert!(
        !body_text.contains(RESOLVED_CREDENTIAL),
        "the rejection body must never carry the resolved credential"
    );
}
// @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-no-leak-on-rejection-01

// @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-inline-secret-01
#[tokio::test]
async fn an_inline_apikey_credential_is_rejected_with_503_plugin_not_found() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("apikey-inline.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(auth_config(
        APIKEY_AUTH_PLUGIN_NAME,
        json!({"placement": "header", "name": "X-Api-Key", "credential_ref": "sk-inline-value"}),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let credstore: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::always_failing());
    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        credstore,
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/apikey-inline.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-inline-secret-01

// ---------------------------------------------------------------------------
// OAuth2 client-credentials (`cpt-cf-oagw-algo-oauth2-token-acquisition`).
// ---------------------------------------------------------------------------

fn oauth2_config(token_server: &MockServer) -> serde_json::Value {
    json!({
        "token_endpoint": format!("http://{}:{}/oauth/token", token_server.host(), token_server.port()),
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret",
    })
}

fn oauth2_credstore() -> Arc<dyn CredStoreClientV1> {
    Arc::new(MockCredStoreClient::with_secrets(vec![
        ("client-id".to_owned(), "the-client-id".to_owned()),
        ("client-secret".to_owned(), "the-client-secret".to_owned()),
    ]))
}

// @cpt-begin:cpt-cf-oagw-dod-oauth2-client-cred-auth:p2:inst-plugin-runtime-it-oauth2-cache-01
#[tokio::test]
async fn a_second_request_reuses_the_cached_token_with_exactly_one_token_endpoint_call() {
    let upstream_server = MockServer::start();
    let upstream_mock = upstream_server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });
    let token_server = MockServer::start();
    let token_mock = token_server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/oauth/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-cached", 3600));
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let subject_id = Uuid::new_v4();
    let mut up = upstream_with("oauth2.example.com", vec![mock_endpoint(&upstream_server)]);
    up.auth = Some(auth_config(
        OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME,
        oauth2_config(&token_server),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    for _ in 0..2 {
        let router = router_for(
            Arc::clone(&state),
            default_config(),
            tenant_id,
            subject_id,
            oauth2_credstore(),
        );
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/oauth2.example.com/v1/items")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(upstream_mock.calls(), 2);
    assert_eq!(token_mock.calls(), 1);
}
// @cpt-end:cpt-cf-oagw-dod-oauth2-client-cred-auth:p2:inst-plugin-runtime-it-oauth2-cache-01

// @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-plugin-runtime-it-oauth2-short-lived-01
#[tokio::test]
async fn a_token_shorter_than_the_safety_margin_causes_a_second_token_call() {
    let upstream_server = MockServer::start();
    upstream_server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });
    let token_server = MockServer::start();
    let token_mock = token_server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/oauth/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-short", 20));
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let subject_id = Uuid::new_v4();
    let mut up = upstream_with(
        "oauth2-short.example.com",
        vec![mock_endpoint(&upstream_server)],
    );
    up.auth = Some(auth_config(
        OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME,
        oauth2_config(&token_server),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    for _ in 0..2 {
        let router = router_for(
            Arc::clone(&state),
            default_config(),
            tenant_id,
            subject_id,
            oauth2_credstore(),
        );
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/oauth2-short.example.com/v1/items")
            .body(Body::empty())
            .unwrap();
        send(router, request).await;
    }

    assert_eq!(token_mock.calls(), 2);
}
// @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-plugin-runtime-it-oauth2-short-lived-01

// @cpt-begin:cpt-cf-oagw-dod-failure-mapping:p2:inst-plugin-runtime-it-oauth2-failure-01
#[tokio::test]
async fn a_token_endpoint_failure_yields_401_uncached_and_retries_next_request() {
    let upstream_server = MockServer::start();
    let upstream_mock = upstream_server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });
    let token_server = MockServer::start();
    let token_mock = token_server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/oauth/token");
        then.status(500).body("boom");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let subject_id = Uuid::new_v4();
    let mut up = upstream_with(
        "oauth2-fail.example.com",
        vec![mock_endpoint(&upstream_server)],
    );
    up.auth = Some(auth_config(
        OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME,
        oauth2_config(&token_server),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    for _ in 0..2 {
        let router = router_for(
            Arc::clone(&state),
            default_config(),
            tenant_id,
            subject_id,
            oauth2_credstore(),
        );
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/oauth2-fail.example.com/v1/items")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
        );
    }

    assert_eq!(token_mock.calls(), 2);
    assert_eq!(upstream_mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-failure-mapping:p2:inst-plugin-runtime-it-oauth2-failure-01

// @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-plugin-runtime-it-oauth2-tenant-01
#[tokio::test]
async fn two_tenants_sharing_one_binding_each_cause_their_own_token_call() {
    let upstream_server = MockServer::start();
    upstream_server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });
    let token_server = MockServer::start();
    let token_mock = token_server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/oauth/token");
        then.status(200)
            .header("content-type", "application/json")
            .body(token_json("tok-multi", 3600));
    });

    let mut up = upstream_with(
        "oauth2-multi.example.com",
        vec![mock_endpoint(&upstream_server)],
    );
    up.auth = Some(auth_config(
        OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME,
        oauth2_config(&token_server),
    ));

    // Exercise both tenants against one shared `ControlPlaneState` (and its
    // one shared token cache), proving the cache key isolates them.
    let state = Arc::new(ControlPlaneState::new());
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let up_a = {
        let mut u = up.clone();
        u.id = Uuid::new_v4();
        u.alias = "oauth2-multi-a.example.com".to_owned();
        u
    };
    let up_b = {
        let mut u = up.clone();
        u.id = Uuid::new_v4();
        u.alias = "oauth2-multi-b.example.com".to_owned();
        u
    };
    install(
        &state,
        tenant_a,
        up_a.clone(),
        simple_route(up_a.id, "/v1/items", &[RouteMethod::Get]),
    );
    install(
        &state,
        tenant_b,
        up_b.clone(),
        simple_route(up_b.id, "/v1/items", &[RouteMethod::Get]),
    );

    for (tenant_id, alias) in [
        (tenant_a, "oauth2-multi-a.example.com"),
        (tenant_b, "oauth2-multi-b.example.com"),
    ] {
        let router = router_for(
            Arc::clone(&state),
            default_config(),
            tenant_id,
            Uuid::new_v4(),
            oauth2_credstore(),
        );
        let request = Request::builder()
            .method("GET")
            .uri(format!("/oagw/v1/proxy/{alias}/v1/items"))
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(token_mock.calls(), 2);
}
// @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-plugin-runtime-it-oauth2-tenant-01

// @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-oauth2-both-endpoints-01
#[tokio::test]
async fn an_oauth2_binding_naming_both_endpoint_and_issuer_is_rejected_with_no_call() {
    let upstream_server = MockServer::start();
    let upstream_mock = upstream_server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with(
        "oauth2-both.example.com",
        vec![mock_endpoint(&upstream_server)],
    );
    up.auth = Some(auth_config(
        OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME,
        json!({
            "token_endpoint": "https://token.example.com/oauth/token",
            "issuer_url": "https://issuer.example.com",
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret",
        }),
    ));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        oauth2_credstore(),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/oauth2-both.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(upstream_mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-plugin-runtime-it-oauth2-both-endpoints-01

// ---------------------------------------------------------------------------
// Required-headers guard (`cpt-cf-oagw-algo-required-headers-check`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-plugin-runtime-it-guard-missing-01
#[tokio::test]
async fn a_missing_required_request_header_is_rejected_with_400_and_no_upstream_call() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("guard.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": "x-correlation-id,accept"}),
    )]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guard.example.com/v1/items")
        .header("accept", "*/*")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let body = body_json(response).await;
    assert_eq!(body["error_code"], "REQUIRED_HEADER_MISSING");
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-plugin-runtime-it-guard-missing-01

#[tokio::test]
async fn matching_headers_admit_the_request_case_insensitively() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("guard-ok.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": "x-correlation-id,accept"}),
    )]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guard-ok.example.com/v1/items")
        .header("x-correlation-id", "abc")
        .header("Accept", "*/*")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}

// @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-plugin-runtime-it-guard-response-01
#[tokio::test]
async fn a_missing_required_response_header_is_rejected_with_502() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200).body("ok");
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("guard-resp.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_response_headers": "content-type"}),
    )]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guard-resp.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = body_json(response).await;
    assert_eq!(body["error_code"], "REQUIRED_HEADER_MISSING");
}
// @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-plugin-runtime-it-guard-response-01

#[tokio::test]
async fn a_blank_only_guard_configuration_admits_every_request() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("guard-blank.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": ", , ,"}),
    )]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guard-blank.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

// @cpt-begin:cpt-cf-oagw-dod-chain-order:p2:inst-plugin-runtime-it-guard-order-01
#[tokio::test]
async fn an_upstream_level_guard_rejection_is_attributed_before_a_route_level_guard_runs() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("guard-order.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": "x-upstream-required"}),
    )]));
    let mut route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    route.plugins = Some(plugins_config(vec![guard_binding(
        json!({"required_request_headers": "x-route-required"}),
    )]));
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    // Satisfies the route-level guard but not the upstream-level one: if the
    // route guard ran first (or alone), this would be admitted.
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guard-order.example.com/v1/items")
        .header("x-route-required", "1")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-chain-order:p2:inst-plugin-runtime-it-guard-order-01

// ---------------------------------------------------------------------------
// Request-id transform (`cpt-cf-oagw-algo-request-id-propagation`).
// ---------------------------------------------------------------------------

fn transform_binding() -> PluginItem {
    plugin_ref(PluginType::Transform, REQUEST_ID_TRANSFORM_PLUGIN_NAME).into()
}

// @cpt-begin:cpt-cf-oagw-dod-request-id-transform:p2:inst-plugin-runtime-it-reqid-generate-01
#[tokio::test]
async fn a_missing_inbound_request_id_is_generated_and_returned() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("reqid.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![transform_binding()]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/reqid.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id must be present");
    assert!(!request_id.is_empty());
}
// @cpt-end:cpt-cf-oagw-dod-request-id-transform:p2:inst-plugin-runtime-it-reqid-generate-01

#[tokio::test]
async fn an_inbound_request_id_is_propagated_unchanged() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/items")
            .header("x-request-id", "abc-123");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("reqid-keep.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![transform_binding()]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/reqid-keep.example.com/v1/items")
        .header("x-request-id", "abc-123")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("abc-123")
    );
}

// ---------------------------------------------------------------------------
// Catalogue-only identifiers (`cpt-cf-oagw-dod-runtime-plugin-resolution`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-plugin-runtime-it-bearer-catalogue-01
#[tokio::test]
async fn a_catalogue_only_bearer_auth_identifier_yields_503_and_no_upstream_call() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("bearer.example.com", vec![mock_endpoint(&server)]);
    up.auth = Some(AuthConfig {
        auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1".to_owned()),
        sharing: Sharing::Private,
        config: None,
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/bearer.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(mock.calls(), 0);
}
// @cpt-end:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-plugin-runtime-it-bearer-catalogue-01

#[tokio::test]
async fn a_catalogue_only_cors_guard_identifier_yields_503_rather_than_admitting_unchecked() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("cors-guard.example.com", vec![mock_endpoint(&server)]);
    up.plugins = Some(plugins_config(vec![
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1".into(),
    ]));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let router = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/cors-guard.example.com/v1/items")
        .body(Body::empty())
        .unwrap();
    let response = send(router, request).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert_eq!(mock.calls(), 0);
}

// ---------------------------------------------------------------------------
// Rate limiting (`cpt-cf-oagw-algo-token-bucket-admission`).
// ---------------------------------------------------------------------------

// @cpt-begin:cpt-cf-oagw-dod-rate-limit-response:p2:inst-plugin-runtime-it-ratelimit-basic-01
#[tokio::test]
async fn a_second_immediate_request_against_a_one_capacity_bucket_is_rejected_with_429() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("rl.example.com", vec![mock_endpoint(&server)]);
    up.rate_limit = Some(rate_limit(1, Window::Second, Some(1), 1));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let request = || {
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/rl.example.com/v1/items")
            .body(Body::empty())
            .unwrap()
    };

    let router1 = router_for(
        Arc::clone(&state),
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let first = send(router1, request()).await;
    assert_eq!(first.status(), StatusCode::OK);

    let router2 = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let second = send(router2, request()).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        second
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let retry_after: u64 = second
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("retry-after must be present");
    assert!(retry_after >= 1);
    assert_eq!(
        second
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        second
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0")
    );
    assert!(second.headers().contains_key("x-ratelimit-reset"));
    let body = body_json(second).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
    assert_eq!(mock.calls(), 1);
}
// @cpt-end:cpt-cf-oagw-dod-rate-limit-response:p2:inst-plugin-runtime-it-ratelimit-basic-01

// @cpt-begin:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-plugin-runtime-it-ratelimit-cost-01
#[tokio::test]
async fn a_high_cost_request_exhausts_a_matching_capacity_bucket_in_one_shot() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("rl-cost.example.com", vec![mock_endpoint(&server)]);
    up.rate_limit = Some(rate_limit(10, Window::Second, Some(10), 10));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let request = || {
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/rl-cost.example.com/v1/items")
            .body(Body::empty())
            .unwrap()
    };

    let router1 = router_for(
        Arc::clone(&state),
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(send(router1, request()).await.status(), StatusCode::OK);

    let router2 = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(
        send(router2, request()).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
// @cpt-end:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-plugin-runtime-it-ratelimit-cost-01

#[tokio::test]
async fn a_five_per_minute_limit_rejects_the_sixth_request_in_the_window() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("rl-minute.example.com", vec![mock_endpoint(&server)]);
    up.rate_limit = Some(rate_limit(5, Window::Minute, None, 1));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    for i in 0..6 {
        let router = router_for(
            Arc::clone(&state),
            default_config(),
            tenant_id,
            Uuid::new_v4(),
            Arc::new(MockCredStoreClient::empty()),
        );
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/rl-minute.example.com/v1/items")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;
        if i < 5 {
            assert_eq!(response.status(), StatusCode::OK, "request {i} must admit");
        } else {
            assert_eq!(
                response.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "the sixth request must be rejected"
            );
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-rate-limit-response:p2:inst-plugin-runtime-it-ratelimit-queue-01
#[tokio::test]
async fn a_queue_strategy_falls_back_to_reject_semantics() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_id = Uuid::new_v4();
    let mut up = upstream_with("rl-queue.example.com", vec![mock_endpoint(&server)]);
    up.rate_limit = Some(RateLimitConfig {
        strategy: Strategy::Queue,
        ..rate_limit(1, Window::Second, Some(1), 1)
    });
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_id, up, route);

    let request = || {
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/rl-queue.example.com/v1/items")
            .body(Body::empty())
            .unwrap()
    };

    let router1 = router_for(
        Arc::clone(&state),
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(send(router1, request()).await.status(), StatusCode::OK);

    let router2 = router_for(
        state,
        default_config(),
        tenant_id,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    let second = send(router2, request()).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = second
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("retry-after must be present even for the queue strategy fallback");
    assert!(retry_after >= 1);
}
// @cpt-end:cpt-cf-oagw-dod-rate-limit-response:p2:inst-plugin-runtime-it-ratelimit-queue-01

// @cpt-begin:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-plugin-runtime-it-ratelimit-scope-01
#[tokio::test]
async fn a_tenant_scoped_bucket_leaves_a_second_tenant_unaffected() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/items");
        then.status(200);
    });

    let state = Arc::new(ControlPlaneState::new());
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let mut up = upstream_with("rl-scope.example.com", vec![mock_endpoint(&server)]);
    up.rate_limit = Some(rate_limit(1, Window::Second, Some(1), 1));
    let route = simple_route(up.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_a, up, route);
    // A second tenant needs its own upstream/route pair (aliases are
    // per-tenant), so mirror the same rate limit under `tenant_b`.
    let mut up_b = upstream_with("rl-scope.example.com", vec![mock_endpoint(&server)]);
    up_b.rate_limit = Some(rate_limit(1, Window::Second, Some(1), 1));
    let route_b = simple_route(up_b.id, "/v1/items", &[RouteMethod::Get]);
    install(&state, tenant_b, up_b, route_b);

    let request = || {
        Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/rl-scope.example.com/v1/items")
            .body(Body::empty())
            .unwrap()
    };

    let tenant_a_router_first = router_for(
        Arc::clone(&state),
        default_config(),
        tenant_a,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(
        send(tenant_a_router_first, request()).await.status(),
        StatusCode::OK
    );

    let tenant_a_router_second = router_for(
        Arc::clone(&state),
        default_config(),
        tenant_a,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(
        send(tenant_a_router_second, request()).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    let tenant_b_router = router_for(
        state,
        default_config(),
        tenant_b,
        Uuid::new_v4(),
        Arc::new(MockCredStoreClient::empty()),
    );
    assert_eq!(
        send(tenant_b_router, request()).await.status(),
        StatusCode::OK,
        "tenant b's own bucket must be unaffected by tenant a's exhaustion"
    );
}
// @cpt-end:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-plugin-runtime-it-ratelimit-scope-01
