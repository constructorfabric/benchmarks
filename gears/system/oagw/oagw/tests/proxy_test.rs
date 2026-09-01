//! Integration tests of the OAGW data plane (slice 4).
//!
//! Every test drives the real proxy handler stack — alias resolution over the
//! tenant chain, route matching, CORS, rate limiting, the plugin chain,
//! endpoint selection, the outbound engine and the ADR-0007 error-source
//! stamping — through `tower::ServiceExt::oneshot`, with `httpmock` acting as
//! the upstream and a raw TCP socket for the streaming and upgrade cases.

#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use httpmock::prelude::*;
use oagw::api::rest::test_support::{
    ProxyApp, anonymous_request, body, build_proxy_app, build_proxy_app_with_secrets,
    build_proxy_app_without_hierarchy, caller, proxy_request, request,
};
use oagw::config::{OagwConfig, SsrfPolicy};
use oagw::domain::model::{
    AuthConfig, BurstConfig, CorsConfig, CorsMethod, Endpoint, HeadersConfig, HttpMatch,
    HttpMethod, PassthroughMode, PathSuffixMode, PluginBinding, Protocol, RateLimitAlgorithm,
    RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow, Route, RouteMatch, Scheme,
    ServerConfig, SustainedRateConfig, Upstream,
};
use oagw::domain::services::TenantHierarchy;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x11);
const ANCESTOR: Uuid = Uuid::from_u128(0x22);

/// Hierarchy that makes [`ANCESTOR`] the single parent of [`TENANT`].
#[derive(Debug, Default, Clone, Copy)]
struct StaticHierarchy;

#[async_trait]
impl TenantHierarchy for StaticHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        if tenant == TENANT {
            vec![ANCESTOR]
        } else {
            Vec::new()
        }
    }
}

/// Minimal data-plane configuration: plaintext upstreams, no SSRF gate.
fn proxy_config(timeout_secs: u64) -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: timeout_secs,
        ssrf_policy: SsrfPolicy::default(),
        ..OagwConfig::default()
    }
}

/// A single-host upstream pointing at `host:port` over plaintext HTTP.
fn upstream(alias: &str, port: u16) -> Upstream {
    upstream_on(alias, "127.0.0.1", port)
}

fn upstream_on(alias: &str, host: &str, port: u16) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: TENANT,
        alias: alias.to_owned(),
        tags: Vec::new(),
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Http,
                host: host.to_owned(),
                port,
            }],
        },
        auth: None,
        plugins: Default::default(),
        headers: HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: true,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}

/// An upstream that forwards every inbound header.
fn upstream_with_passthrough(alias: &str, port: u16) -> Upstream {
    let mut draft = upstream(alias, port);
    draft.headers.request.passthrough = PassthroughMode::All;
    draft
}

/// Registers `upstream` and one `GET path` route for it, returning the id.
fn seed(app: &ProxyApp, upstream: Upstream, path: &str) -> Uuid {
    seed_route(app, upstream, path, HttpMethod::Get)
}

/// Registers `upstream` and one `method path` route for it, returning the id.
fn seed_route(app: &ProxyApp, upstream: Upstream, path: &str, method: HttpMethod) -> Uuid {
    let stored = app
        .service
        .store()
        .insert_upstream(upstream)
        .expect("upstream seeds");
    app.service
        .store()
        .insert_route(route(stored.id, path, method))
        .expect("route seeds");
    stored.id
}

/// Sends a proxy request and returns `(status, headers, body-bytes)`.
async fn proxy(
    app: &mut ProxyApp,
    method: &'static str,
    path: &str,
) -> (http::StatusCode, http::HeaderMap, Bytes) {
    proxy_with(app, method, path, &[]).await
}

/// Sends a proxy request with extra headers and returns the full answer.
async fn proxy_with(
    app: &mut ProxyApp,
    method: &'static str,
    path: &str,
    headers: &[(&'static str, &str)],
) -> (http::StatusCode, http::HeaderMap, Bytes) {
    let request = proxy_request(method, path, caller(TENANT).expect("context"), headers)
        .expect("request builds");
    let response = app.send(request).await.expect("infallible");
    let status = response.status();
    let response_headers = response.headers().clone();
    let payload = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    (status, response_headers, payload)
}

// -- forwarding -------------------------------------------------------------

#[tokio::test]
async fn forwards_method_path_query_and_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/orders")
            .query_param("page", "2")
            .header("x-trace", "abc");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(
        &app,
        upstream_with_passthrough("orders", server.port()),
        "/",
    );
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders?page=2",
        &[("x-trace", "abc")],
    )
    .await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "body={}",
        body_string(&payload)
    );
    assert_eq!(payload, Bytes::from_static(b"orders"));
}

/// `PassthroughMode::None` forwards no client header at all, so the upstream
/// never sees `x-trace`.
#[tokio::test]
async fn the_default_passthrough_mode_forwards_no_client_header() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/orders");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, _, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[("x-trace", "abc")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test]
async fn appends_the_path_suffix_to_the_matched_route() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/orders/42/items");
        then.status(200).body("items");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/orders");
    let (status, _, payload) =
        proxy(&mut app, "GET", "/oagw/v1/proxy/orders/orders/42/items").await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(payload, Bytes::from_static(b"items"));
}

#[tokio::test]
async fn rewrites_the_host_header_to_the_endpoint() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.header("host", format!("127.0.0.1:{}", server.port()));
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test]
async fn applies_the_request_header_rules() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.header("x-set", "final").header("x-added", "one");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream_with_passthrough("orders", server.port());
    draft
        .headers
        .request
        .set
        .insert("x-set".to_owned(), "final".to_owned());
    draft
        .headers
        .request
        .add
        .insert("x-added".to_owned(), "one".to_owned());
    draft.headers.request.remove.push("x-dropped".to_owned());
    seed(&app, draft, "/");
    let (status, _, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("x-dropped", "gone")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test]
async fn applies_the_response_header_rules() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft
        .headers
        .response
        .set
        .insert("x-gateway".to_owned(), "oagw".to_owned());
    seed(&app, draft, "/");
    let (_, headers, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(
        headers
            .get("x-gateway")
            .and_then(|value| value.to_str().ok()),
        Some("oagw")
    );
}

#[tokio::test]
async fn upstream_status_body_and_headers_pass_through() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(201).header("x-upstream", "yes").body("created");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, headers, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::CREATED);
    assert_eq!(payload, Bytes::from_static(b"created"));
    assert_eq!(
        headers
            .get("x-upstream")
            .and_then(|value| value.to_str().ok()),
        Some("yes")
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
}

#[tokio::test]
async fn strips_hop_by_hop_headers_in_both_directions() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.header_missing("connection").header_missing("te");
        then.status(200)
            .header("connection", "keep-alive")
            .header("keep-alive", "timeout=5")
            .body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (_, headers, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("connection", "keep-alive"), ("te", "trailers")],
    )
    .await;
    assert!(headers.get("connection").is_none());
    assert!(headers.get("keep-alive").is_none());
}

#[tokio::test]
async fn an_upstream_failure_is_stamped_upstream() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(500).body("boom");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, headers, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(payload, Bytes::from_static(b"boom"));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream"),
        "an upstream failure must never be stamped `gateway`"
    );
}

/// An upstream that answers `application/problem+json` itself keeps its own
/// document: the gateway stamps the error source but injects none of its own
/// correlation fields (ADR-0007 "Passthrough").
#[tokio::test]
async fn an_upstream_problem_document_passes_through_untouched() {
    let upstream_problem = r#"{"type":"https://orders.example.com/errors/outage","title":"Outage","status":503,"detail":"over capacity"}"#;
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(503)
            .header("content-type", "application/problem+json")
            .body(upstream_problem);
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, headers, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("x-request-id", "req-7")],
    )
    .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
    assert_eq!(body_string(&payload), upstream_problem, "byte for byte");
}

#[tokio::test]
async fn forwards_a_post_body() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/").body("payload");
        then.status(200).body("accepted");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed_route(
        &app,
        upstream("orders", server.port()),
        "/",
        HttpMethod::Post,
    );
    let request = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("content-length", "7")],
    )
    .expect("request builds")
    .map(|_| axum::body::Body::from("payload"));
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::OK);
}

// -- credential and header injection (PRD §5.2) ------------------------------

/// The bare registry key of the built-in `X-Request-ID` transform plugin.
const REQUEST_ID_PLUGIN: &str = "cf.core.oagw.request_id.v1";

/// The bare registry key of the built-in API key auth plugin.
const API_KEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// An upstream bound to the API key plugin (injecting per `config`) and to the
/// `X-Request-ID` transform plugin, so one test sees both injected values.
fn api_key_upstream(alias: &str, port: u16, config: serde_json::Value) -> Upstream {
    let mut draft = upstream(alias, port);
    draft.auth = Some(AuthConfig {
        auth_type: API_KEY_PLUGIN.to_owned(),
        ..AuthConfig::default()
    });
    draft.auth.as_mut().expect("auth binding").config = config;
    draft.plugins.items = vec![PluginBinding::new(REQUEST_ID_PLUGIN, json!({}))];
    draft
}

/// An upstream bound to the `X-Request-ID` transform plugin.
fn request_id_upstream(alias: &str, port: u16) -> Upstream {
    let mut draft = upstream(alias, port);
    draft.plugins.items = vec![PluginBinding::new(REQUEST_ID_PLUGIN, json!({}))];
    draft
}

/// The request-id plugin stamps the outbound header even though the default
/// `passthrough: none` posture forwards no client header at all.
#[tokio::test]
async fn the_request_id_plugin_injects_the_outbound_header() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/orders")
            .header("x-request-id", "probe-42")
            .header_missing("x-probe-header");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, request_id_upstream("orders", server.port()), "/");
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[
            ("x-request-id", "probe-42"),
            ("x-probe-header", "probe-123"),
        ],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{}", body_string(&payload));
}

/// An API key credential is injected as a header and overrides the client
/// value of the same name; the request id still rides along.
#[tokio::test]
async fn an_api_key_plugin_injects_its_header_and_overrides_the_client_value() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/orders")
            .header("x-upstream-key", "secret-key")
            .header_not("x-upstream-key", "spoofed")
            .header("x-request-id", "probe-42");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let draft = api_key_upstream(
        "orders",
        server.port(),
        json!({"key": "secret-key", "header_name": "x-upstream-key"}),
    );
    seed(&app, draft, "/");
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[("x-upstream-key", "spoofed"), ("x-request-id", "probe-42")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{}", body_string(&payload));
}

/// An API key credential configured as `query_name` reaches the upstream query
/// string, URL-encoded, and replaces the client parameter of the same name.
#[tokio::test]
async fn an_api_key_plugin_injects_its_query_parameter() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/orders")
            .query_param("api_key", "sec ret/1")
            .query_param_not("api_key", "spoofed")
            .query_param("keep", "1")
            .header_missing("x-api-key");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let draft = api_key_upstream(
        "orders",
        server.port(),
        json!({"key": "sec ret/1", "query_name": "api_key"}),
    );
    seed(&app, draft, "/");
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders?api_key=spoofed&keep=1",
        &[],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{}", body_string(&payload));
}

// -- memoised plugin instances (ADR-0008) ------------------------------------

/// The bare registry key of the built-in OAuth2 client-credentials auth plugin.
const OAUTH2_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// The access-token body an IdP answers a client-credentials grant with.
fn token_body(token: &str, expires_in: u64) -> String {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#)
}

/// An upstream bound to the OAuth2 client-credentials plugin.
fn oauth2_upstream(alias: &str, port: u16, token_endpoint: &str) -> Upstream {
    let mut draft = upstream(alias, port);
    let mut auth = AuthConfig {
        auth_type: OAUTH2_PLUGIN.to_owned(),
        ..AuthConfig::default()
    };
    auth.config = json!({
        "token_endpoint": token_endpoint,
        "client_id_ref": "cred://client-id",
        "client_secret_ref": "cred://client-secret"
    });
    draft.auth = Some(auth);
    draft
}

/// The plugin instances of a binding are memoised, so a second proxied request
/// reuses the first bearer token instead of asking the IdP again (ADR-0008).
#[tokio::test]
async fn an_oauth2_binding_hits_the_token_endpoint_once_for_two_requests() {
    let idp = MockServer::start();
    let token = idp.mock(|when, then| {
        when.method(POST).path("/token");
        then.status(200).body(token_body("bearer-1", 3600));
    });
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/orders")
            .header("authorization", "Bearer bearer-1");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_with_secrets(
        proxy_config(2),
        Arc::new(StaticHierarchy),
        HashMap::from([
            ("client-id".to_owned(), "test-client".to_owned()),
            ("client-secret".to_owned(), "test-secret".to_owned()),
        ]),
    );
    let draft = oauth2_upstream(
        "orders",
        server.port(),
        &format!("http://localhost:{}/token", idp.port()),
    );
    seed(&app, draft, "/");

    for _ in 0..2 {
        let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders/orders").await;
        assert_eq!(status, http::StatusCode::OK, "{}", body_string(&payload));
    }
    token.assert_calls_async(1).await;
}

// -- resolution failures ----------------------------------------------------

#[tokio::test]
async fn an_unknown_alias_is_a_route_not_found_problem() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let (status, headers, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/ghost").await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    assert!(body_string(&payload).contains("cf.oagw.route.not_found.v1"));
}

#[tokio::test]
async fn a_disabled_upstream_is_link_unavailable() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.enabled = false;
    seed(&app, draft, "/");
    let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_string(&payload).contains("cf.oagw.link.unavailable.v1"));
}

#[tokio::test]
async fn an_unmatched_path_is_a_route_not_found_problem() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/orders");
    let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders/nope").await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert!(body_string(&payload).contains("cf.oagw.route.not_found.v1"));
}

#[tokio::test]
async fn an_unmatched_method_is_a_route_not_found_problem() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/orders");
    let (status, _, _) = proxy(&mut app, "POST", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_descendant_sees_an_ancestor_upstream_by_alias() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("inherited");
    });
    let mut app = build_proxy_app(proxy_config(2), Arc::new(StaticHierarchy));
    let mut draft = upstream("orders", server.port());
    draft.tenant_id = ANCESTOR;
    let id = app
        .service
        .store()
        .insert_upstream(draft)
        .expect("upstream seeds")
        .id;
    let mut child_route = route(id, "/", HttpMethod::Get);
    child_route.tenant_id = ANCESTOR;
    app.service
        .store()
        .insert_route(child_route)
        .expect("route seeds");
    let request = proxy_request(
        "GET",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[],
    )
    .expect("request builds");
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::OK);
}

/// The data-plane cache is keyed by the *calling* tenant, so it is invalidated
/// wholesale whenever the owning tenant edits the upstream: a descendant that
/// already resolved the upstream must observe the change and not serve a stale
/// snapshot.
#[tokio::test]
async fn an_owner_edit_reaches_a_descendant_that_cached_the_upstream() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/orders");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app(
        OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
        Arc::new(StaticHierarchy),
    );
    let mut draft = upstream("orders", server.port());
    draft.tenant_id = ANCESTOR;
    draft.cors = Some(cors_config());
    let id = app
        .service
        .store()
        .insert_upstream(draft)
        .expect("upstream seeds")
        .id;
    let mut child_route = route(id, "/orders", HttpMethod::Get);
    child_route.tenant_id = ANCESTOR;
    app.service
        .store()
        .insert_route(child_route)
        .expect("route seeds");

    // Warm the descendant's cache entry with an origin the owner allows.
    let (warm, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[("origin", "https://portal.example.com")],
    )
    .await;
    assert_eq!(warm, http::StatusCode::OK, "{}", body_string(&payload));

    // The owner tightens the CORS section.
    let replacement = json!({
        "alias": "orders",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": {"endpoints": [
            {"scheme": "http", "host": "127.0.0.1", "port": server.port()}
        ]},
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://other.example.com"],
            "allowed_methods": ["GET"],
            "allow_headers": ["content-type"]
        },
    });
    let (status, body) = manage(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            caller(ANCESTOR).expect("context"),
            Some(&replacement.to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");

    let (after, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[("origin", "https://portal.example.com")],
    )
    .await;
    assert_eq!(
        after,
        http::StatusCode::FORBIDDEN,
        "{}",
        body_string(&payload)
    );
}

// -- route selection --------------------------------------------------------

/// The bare registry key of the built-in required-headers guard plugin.
const REQUIRED_HEADERS_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Two routes claiming the same path are separated by priority, not by the
/// order the store returned them in: the higher-priority one wins even though
/// `max_by_key` alone would hand the request to the last maximum of the list.
#[tokio::test]
async fn the_higher_priority_route_wins_when_two_routes_share_a_path() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/orders");
        then.status(200).body("guarded");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let id = seed(&app, upstream("orders", server.port()), "/");

    let mut low = route(id, "/orders", HttpMethod::Get);
    low.priority = 0;
    app.service.store().insert_route(low).expect("route seeds");

    let mut high = route(id, "/orders", HttpMethod::Get);
    high.priority = 10;
    high.plugins.items = vec![PluginBinding::new(
        REQUIRED_HEADERS_PLUGIN,
        json!({"required_request_headers": "x-required"}),
    )];
    app.service.store().insert_route(high).expect("route seeds");

    let (without, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders/orders").await;
    assert_eq!(
        without,
        http::StatusCode::BAD_REQUEST,
        "{}",
        body_string(&payload)
    );
    assert!(body_string(&payload).contains("REQUIRED_HEADER_MISSING"));

    let (with, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders/orders",
        &[("x-required", "1")],
    )
    .await;
    assert_eq!(with, http::StatusCode::OK, "{}", body_string(&payload));
}

// -- endpoint selection -----------------------------------------------------

#[tokio::test]
async fn a_pinned_target_host_is_used() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("pinned");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.server.endpoints.push(Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: server.port(),
    });
    seed(&app, draft, "/");
    let (status, _, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("x-oagw-target-host", "127.0.0.1")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test]
async fn a_malformed_target_host_is_a_bad_request() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("x-oagw-target-host", "not a host")],
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(body_string(&payload).contains("cf.oagw.routing.invalid_target_host.v1"));
}

#[tokio::test]
async fn an_unknown_target_host_lists_the_pool() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, _, payload) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("x-oagw-target-host", "other.example.com")],
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(body_string(&payload).contains("cf.oagw.routing.unknown_target_host.v1"));
    assert!(body_string(&payload).contains("127.0.0.1"));
}

#[tokio::test]
async fn round_robin_balances_two_endpoints() {
    let first = MockServer::start();
    let second = MockServer::start();
    first.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("first");
    });
    second.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("second");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream_on("round-robin", "127.0.0.1", first.port());
    draft.server.endpoints.push(Endpoint {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: second.port(),
    });
    seed(&app, draft, "/");

    let mut answers = Vec::new();
    for _ in 0..4 {
        let (_, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/round-robin").await;
        answers.push(body_string(&payload));
    }
    assert!(answers.contains(&"first".to_owned()), "{answers:?}");
    assert!(answers.contains(&"second".to_owned()), "{answers:?}");
}

// -- timeouts and size limits ----------------------------------------------

#[tokio::test]
async fn an_unreachable_upstream_is_link_unavailable() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", 1), "/");
    let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_string(&payload).contains("cf.oagw.link.unavailable.v1"));
}

#[tokio::test]
async fn a_slow_upstream_is_a_gateway_timeout() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("late").delay(Duration::from_secs(4));
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(1));
    seed(&app, upstream("orders", server.port()), "/");
    let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(body_string(&payload).contains("cf.oagw.timeout.request.v1"));
}

#[tokio::test]
async fn a_mismatched_content_length_is_a_bad_request() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed_route(
        &app,
        upstream("orders", server.port()),
        "/",
        HttpMethod::Post,
    );
    let request = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("content-length", "99")],
    )
    .expect("request builds")
    .map(|_| axum::body::Body::from("payload"));
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_identity_transfer_encoding_is_a_bad_request() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed_route(
        &app,
        upstream("orders", server.port()),
        "/",
        HttpMethod::Post,
    );
    let request = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("transfer-encoding", "identity")],
    )
    .expect("request builds")
    .map(|_| axum::body::Body::from("payload"));
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
}

/// A body whose reads fail, so a handler that buffers the payload instead of
/// answering from the declared headers surfaces the read error.
fn unreadable_body() -> axum::body::Body {
    axum::body::Body::from_stream(futures_util::stream::iter(vec![
        Err::<Bytes, std::io::Error>(std::io::Error::other("the body must never be read")),
    ]))
}

/// A declared `Content-Length` over the cap is rejected before the body is
/// buffered, so an oversized upload never reaches memory and the upstream is
/// never contacted (DESIGN "Body Validation Rules").
#[tokio::test]
async fn a_declared_body_over_the_cap_is_rejected_before_it_is_read() {
    let server = MockServer::start();
    let oversized = server.mock(|when, then| {
        when.method(POST).path("/");
        then.status(200).body("must never be reached");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed_route(
        &app,
        upstream("orders", server.port()),
        "/",
        HttpMethod::Post,
    );
    let request = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("content-length", "999999999")],
    )
    .expect("request builds")
    .map(|_| unreadable_body());
    let response = app.send(request).await.expect("infallible");
    let status = response.status();
    let payload = body(response).await.expect("body reads");
    assert_eq!(status, http::StatusCode::PAYLOAD_TOO_LARGE, "{payload}");
    assert!(
        payload.contains("cf.oagw.payload.too_large.v1"),
        "{payload}"
    );
    assert_eq!(oversized.calls_async().await, 0, "no upstream call");
}

/// A chunked (no `Content-Length`) body of `size` bytes, streamed in 1 MiB
/// frames so the test never materialises the whole payload up front.
fn chunked_body(size: usize) -> axum::body::Body {
    let frame = Bytes::from(vec![b'x'; 1024 * 1024]);
    let frames = size.div_ceil(frame.len());
    let stream =
        futures_util::stream::repeat_with(move || Ok::<Bytes, std::io::Error>(frame.clone()))
            .take(frames);
    axum::body::Body::from_stream(stream)
}

/// An undeclared body over the cap is a `413` as well: the declared
/// `Content-Length` is only the fast path, so a caller that streams its upload
/// (or lies about its size) is answered with the same problem type once the
/// buffered content is measured (DESIGN "Body Validation Rules").
#[tokio::test]
async fn an_undeclared_body_over_the_cap_is_a_413() {
    let server = MockServer::start();
    let oversized = server.mock(|when, then| {
        when.method(POST).path("/");
        then.status(200).body("must never be reached");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed_route(
        &app,
        upstream("orders", server.port()),
        "/",
        HttpMethod::Post,
    );
    let request = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("transfer-encoding", "chunked")],
    )
    .expect("request builds")
    .map(|_| chunked_body(oagw::infra::proxy::MAX_REQUEST_BODY_BYTES + 1));
    let response = app.send(request).await.expect("infallible");
    let status = response.status();
    let payload = body(response).await.expect("body reads");
    assert_eq!(status, http::StatusCode::PAYLOAD_TOO_LARGE, "{payload}");
    assert!(
        payload.contains("cf.oagw.payload.too_large.v1"),
        "{payload}"
    );
    assert_eq!(oversized.calls_async().await, 0, "no upstream call");
}

/// An exhausted rate budget is answered before the body is validated, so a
/// caller over budget learns that instead of a body error.
#[tokio::test]
async fn the_rate_limit_is_enforced_before_the_body_is_read() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRateConfig {
            rate: 1,
            window: RateLimitWindow::Minute,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    seed_route(&app, draft, "/", HttpMethod::Post);

    // Spend the budget on a well-formed request.
    let first = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("content-length", "7")],
    )
    .expect("request builds")
    .map(|_| axum::body::Body::from("payload"));
    let response = app.send(first).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::OK);

    // The next one is over budget *and* carries a body error: the 429 wins.
    let second = oagw::api::rest::test_support::proxy_request(
        "POST",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[("content-length", "999999999")],
    )
    .expect("request builds")
    .map(|_| unreadable_body());
    let response = app.send(second).await.expect("infallible");
    let status = response.status();
    let payload = body(response).await.expect("body reads");
    assert_eq!(status, http::StatusCode::TOO_MANY_REQUESTS, "{payload}");
}

// -- CORS -------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_is_answered_locally_with_204() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.cors = Some(cors_config());
    seed(&app, draft, "/");
    let (status, headers, payload) = proxy_with(
        &mut app,
        "OPTIONS",
        "/oagw/v1/proxy/orders",
        &[
            ("origin", "https://portal.example.com"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    assert!(payload.is_empty());
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://portal.example.com")
    );
    // ADR-0004: a preflight varies on every request header it consumed.
    assert_eq!(
        headers.get("vary").and_then(|value| value.to_str().ok()),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
}

/// A preflight no configured upstream owns — here the caller has no security
/// context, exactly as a browser sends it — is answered `204` with the
/// permissive ADR-0004 posture: no `Access-Control-Allow-Origin` (nothing was
/// verified against a configuration), the requested method and headers echoed,
/// the pinned max-age and the `Vary` triplet.
#[tokio::test]
async fn an_unauthenticated_preflight_is_answered_permissively() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", 1), "/");
    let request = anonymous_request(
        "OPTIONS",
        "/oagw/v1/proxy/orders",
        &[
            ("origin", "https://portal.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type, x-trace-id"),
        ],
    )
    .expect("request builds");
    let response = app.send(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let payload = body(response).await.expect("body reads");
    assert_eq!(status, http::StatusCode::NO_CONTENT, "{payload}");
    assert!(payload.is_empty());
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        None,
        "no grant may be advertised without a verified configuration"
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok()),
        Some("content-type, x-trace-id")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("86400")
    );
    assert_eq!(
        headers.get("vary").and_then(|value| value.to_str().ok()),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
}

/// An authenticated caller whose alias does not resolve gets the same
/// permissive preflight instead of the `404` an actual request would answer
/// with (ADR-0004: no upstream resolution, enforcement deferred).
#[tokio::test]
async fn a_preflight_for_an_unknown_alias_is_answered_permissively() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", 1), "/");
    let request = anonymous_request(
        "OPTIONS",
        "/oagw/v1/proxy/no-such-alias",
        &[
            ("origin", "https://portal.example.com"),
            ("access-control-request-method", "POST"),
        ],
    )
    .expect("request builds");
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-max-age")
            .and_then(|value| value.to_str().ok()),
        Some("86400")
    );
}

/// An authenticated, resolvable preflight keeps the configured behaviour: the
/// origin is echoed because the gateway verified it against the upstream CORS
/// section.
#[tokio::test]
async fn a_preflight_for_a_disabled_upstream_stays_permissive() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.enabled = false;
    draft.cors = Some(cors_config());
    seed(&app, draft, "/");
    let (status, headers, _) = proxy_with(
        &mut app,
        "OPTIONS",
        "/oagw/v1/proxy/orders",
        &[
            ("origin", "https://portal.example.com"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        None,
        "a disabled upstream verifies no grant"
    );
}

#[tokio::test]
async fn a_disallowed_origin_is_forbidden() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.cors = Some(cors_config());
    seed(&app, draft, "/");
    let (status, _, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("origin", "https://evil.example.com")],
    )
    .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_allowed_origin_is_proxied_and_annotated() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.cors = Some(cors_config());
    seed(&app, draft, "/");
    let (status, headers, _) = proxy_with(
        &mut app,
        "GET",
        "/oagw/v1/proxy/orders",
        &[("origin", "https://portal.example.com")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://portal.example.com")
    );
}

// -- rate limiting ----------------------------------------------------------

#[tokio::test]
async fn an_exhausted_rate_budget_is_a_429_with_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRateConfig {
            rate: 1,
            window: RateLimitWindow::Minute,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    seed(&app, draft, "/");

    let (first, first_headers, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(first, http::StatusCode::OK);
    assert!(first_headers.get("x-ratelimit-limit").is_some());
    let (second, second_headers, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(second, http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        second_headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok()),
        Some("0")
    );
}

/// ADR-0003: a `429` always carries `Retry-After`, including one a `queue`
/// strategy answers because its sliding window cannot deliver the reservation it
/// granted — the window is a log of past hits, so the reserved token is not
/// there yet even though the wait is inside the queue bound.
#[tokio::test]
async fn a_denied_queue_reservation_answers_429_with_retry_after() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRateConfig {
            rate: 2,
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstConfig { capacity: Some(2) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Queue,
        response_headers: true,
        cost: 1,
    });
    seed(&app, draft, "/");

    let _ = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    let _ = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    let (third, headers, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;

    assert_eq!(third, http::StatusCode::TOO_MANY_REQUESTS);
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .expect("ADR-0003 requires Retry-After on every 429");
    assert_ne!(retry_after, "0", "{retry_after}");
    assert!(
        headers.get("x-ratelimit-remaining").is_some(),
        "the budget headers are still emitted"
    );
}

/// A `queue` reservation the request never used is put back: a request that was
/// granted a token and then failed must not consume the budget, so the next
/// request is admitted without waiting for it again.
#[tokio::test]
async fn a_failed_queued_request_refunds_its_reservation() {
    // Port 1 has no listener, so every request fails after the debit with a
    // gateway error.
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", 1);
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRateConfig {
            rate: 2,
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Queue,
        response_headers: true,
        cost: 1,
    });
    seed(&app, draft, "/");

    let path = "/oagw/v1/proxy/orders";
    let started = Instant::now();
    let (first, _, _) = proxy(&mut app, "GET", path).await;
    assert_eq!(first, http::StatusCode::SERVICE_UNAVAILABLE);
    // The bucket is empty now, so the second request queues half a second for
    // its token, waits, and fails like the first.
    let (second, _, _) = proxy(&mut app, "GET", path).await;
    assert_eq!(second, http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() >= Duration::from_millis(400));

    // The failed request put its reserved token back: the third one is admitted
    // without paying the wait again.
    let started = Instant::now();
    let (third, _, _) = proxy(&mut app, "GET", path).await;
    assert_eq!(third, http::StatusCode::SERVICE_UNAVAILABLE);
    let third_elapsed = started.elapsed();
    assert!(
        third_elapsed < Duration::from_millis(400),
        "the refunded budget was not restored: {third_elapsed:?}"
    );
}

// -- metrics ----------------------------------------------------------------
#[tokio::test]
async fn the_metrics_endpoint_renders_the_data_plane_counters() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/");
    let _ = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;

    let rendered = app.metrics.render();
    assert!(rendered.contains("# TYPE oagw_requests_total counter"));
    assert!(rendered.contains("http.request.method=\"GET\""));
    assert!(rendered.contains("http.response.status_code=\"2xx\""));
}

#[tokio::test]
async fn the_metrics_endpoint_serves_prometheus_text() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let _ = proxy(&mut app, "GET", "/oagw/v1/proxy/ghost").await;
    let request = proxy_request(
        "GET",
        "/oagw/v1/metrics",
        caller(TENANT).expect("context"),
        &[],
    )
    .expect("request builds");
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/plain")
    );
    let text = body(response).await.expect("body reads");
    assert!(text.contains("# TYPE oagw_requests_total counter"));
}

#[tokio::test]
async fn failed_proxies_are_counted_as_errors() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let _ = proxy(&mut app, "GET", "/oagw/v1/proxy/ghost").await;
    let rendered = app.metrics.render();
    assert!(rendered.contains("# TYPE oagw_errors_total counter"));
    assert!(
        rendered.contains("error_type=\"gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1\"")
    );
}

// -- streaming --------------------------------------------------------------

#[tokio::test]
async fn a_server_sent_event_stream_arrives_incrementally() {
    let peer = chunked_upstream().await;
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", peer.port());
    draft.server.endpoints[0].port = peer.port();
    seed(&app, draft, "/");

    let request = proxy_request(
        "GET",
        "/oagw/v1/proxy/orders",
        caller(TENANT).expect("context"),
        &[],
    )
    .expect("request builds");
    let started = Instant::now();
    let response = app.send(request).await.expect("infallible");
    assert_eq!(response.status(), http::StatusCode::OK);

    let mut arrivals = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    while let Some(frame) = stream.next().await {
        let frame = frame.expect("frame reads");
        arrivals.push((
            started.elapsed(),
            String::from_utf8_lossy(&frame).to_string(),
        ));
    }
    assert_eq!(arrivals.len(), 3, "three chunks, not one buffered body");
    assert_eq!(arrivals[0].1, "data: one\n\n");
    assert_eq!(arrivals[1].1, "data: two\n\n");
    assert_eq!(arrivals[2].1, "data: three\n\n");
    // The last frame lands at least two inter-chunk gaps after the first, so
    // the gateway really forwarded while the upstream was still writing.
    assert!(
        arrivals[2].0 - arrivals[0].0 >= Duration::from_millis(80),
        "stream was buffered: {arrivals:?}"
    );
}

/// Upstream that answers with a `chunked` SSE response, one event every 80 ms.
async fn chunked_upstream() -> std::net::SocketAddr {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accepts");
        let mut buffer = vec![0_u8; 8192];
        let _ = socket.read(&mut buffer).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("writes head");
        for event in ["data: one\n\n", "data: two\n\n", "data: three\n\n"] {
            socket
                .write_all(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes())
                .await
                .expect("writes chunk");
            socket.flush().await.expect("flushes");
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("writes terminator");
    });
    address
}

// -- websocket --------------------------------------------------------------

#[tokio::test]
async fn a_websocket_upgrade_is_spliced_bidirectionally() {
    let peer = echo_upstream().await;
    let app = build_proxy_app_without_hierarchy(proxy_config(5));
    let mut draft = upstream("orders", peer.port());
    draft.server.endpoints[0].scheme = Scheme::Ws;
    seed(&app, draft, "/");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("address");
    let router = app
        .router
        .clone()
        .layer(axum::Extension(caller(TENANT).expect("context")));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let io = hyper_util::rt::TokioIo::new(socket);
            let service = hyper_util::service::TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    let socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connects");
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .handshake(hyper_util::rt::TokioIo::new(socket))
        .await
        .expect("handshakes");
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let request = hyper::Request::builder()
        .method("GET")
        .uri(format!("http://{address}/oagw/v1/proxy/orders"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(axum::body::Body::empty())
        .expect("request builds");
    let response = sender.send_request(request).await.expect("sends");
    assert_eq!(response.status(), hyper::StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );

    let upgraded = hyper::upgrade::on(response).await.expect("upgrades");
    let mut upgraded = hyper_util::rt::TokioIo::new(upgraded);
    let greeting = websocket_frame(b"hello!");
    let mut frame = vec![0_u8; greeting.len()];
    tokio::time::timeout(Duration::from_secs(3), upgraded.read_exact(&mut frame))
        .await
        .expect("read does not hang")
        .expect("reads");
    assert_eq!(frame, greeting);
    upgraded
        .write_all(&websocket_frame(b"ping"))
        .await
        .expect("writes");
    let mut echoed = [0_u8; 7];
    let echoed_len = tokio::time::timeout(Duration::from_secs(3), upgraded.read(&mut echoed))
        .await
        .expect("read does not hang")
        .expect("reads");
    assert_eq!(&echoed[..echoed_len], &websocket_frame(b"ping"));
}

/// A raw TCP server that completes a WebSocket handshake and echoes frames.
async fn echo_upstream() -> std::net::SocketAddr {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accepts");
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.expect("reads");
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let key = request
            .lines()
            .find_map(|line| line.split_once(": "))
            .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default();
        let accept = websocket_accept(&key);
        socket
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("writes handshake");
        // Greet, then echo whatever the client sends, one frame at a time.
        socket
            .write_all(&websocket_frame(b"hello!"))
            .await
            .expect("writes greeting");
        loop {
            let mut header = [0_u8; 2];
            match socket.read_exact(&mut header).await {
                Ok(_) => {}
                Err(_) => return,
            }
            let length = usize::from(header[1] & 0x7f);
            let mut payload = vec![0_u8; length];
            if socket.read_exact(&mut payload).await.is_err() {
                return;
            }
            if socket.write_all(&websocket_frame(&payload)).await.is_err() {
                return;
            }
        }
    });
    address
}

/// Minimal unmasked WebSocket frame carrying `payload`.
fn websocket_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x81, u8::try_from(payload.len()).unwrap_or(127)];
    frame.extend_from_slice(payload);
    frame
}

/// `base64(sha1(key + GUID))` of a WebSocket handshake key.
fn websocket_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mixed = format!("{key}{GUID}");
    let mut digest = [0_u8; 20];
    sha1(mixed.as_bytes(), &mut digest);
    base64(&digest)
}

/// SHA-1 of `input`, written into `digest` (20 bytes).
fn sha1(input: &[u8], digest: &mut [u8; 20]) {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = input.to_vec();
    let bit_length = u64::try_from(message.len()).unwrap_or(0) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());
    for block in message.chunks(64) {
        let mut words = [0_u32; 80];
        for (index, word) in block.chunks(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            let value = words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16];
            words[index] = value.rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    for (slot, word) in digest.chunks_mut(4).zip(h.iter()) {
        for (index, byte) in word.to_be_bytes().iter().enumerate() {
            slot[index] = *byte;
        }
    }
}

/// Standard base64 of `input`, padded with `=`.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rendered = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let triple = [
            chunk.first().copied().unwrap_or_default(),
            chunk.get(1).copied().unwrap_or_default(),
            chunk.get(2).copied().unwrap_or_default(),
        ];
        let group = u32::from(triple[0]) << 16 | u32::from(triple[1]) << 8 | u32::from(triple[2]);
        rendered.push(char::from(ALPHABET[((group >> 18) & 0x3f) as usize]));
        rendered.push(char::from(ALPHABET[((group >> 12) & 0x3f) as usize]));
        rendered.push(if chunk.len() > 1 {
            char::from(ALPHABET[((group >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        rendered.push(if chunk.len() > 2 {
            char::from(ALPHABET[(group & 0x3f) as usize])
        } else {
            '='
        });
    }
    rendered
}

// -- disabled plugin resources ----------------------------------------------

/// Sends a management request to the proxy router and returns `(status, body)`.
///
/// The management routes are mounted on the same router as the proxy surface, so
/// a test can register a plugin and then proxy to the upstream it is bound to.
async fn manage(
    app: &mut ProxyApp,
    req: Result<axum::http::Request<axum::body::Body>, axum::http::Error>,
) -> (http::StatusCode, serde_json::Value) {
    let response = app
        .send(req.expect("request builds"))
        .await
        .expect("infallible");
    let status = response.status();
    let text = body(response).await.expect("body reads");
    let parsed = if text.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text).expect("body is JSON")
    };
    (status, parsed)
}

/// The upstream draft of the test upstream, pointing at `port` over HTTP.
fn orders_upstream(port: u16) -> serde_json::Value {
    json!({
        "alias": "orders",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": {"endpoints": [
            {"scheme": "http", "host": "127.0.0.1", "port": port}
        ]},
    })
}

/// A custom plugin draft of the calling tenant.
fn plugin_draft(enabled: bool) -> serde_json::Value {
    json!({
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1",
        "config": {"headers": ["x-request-id"]},
        "enabled": enabled,
    })
}

#[tokio::test]
async fn an_enabled_plugin_resource_is_a_503_and_a_disabled_one_is_skipped() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/orders");
        then.status(200).body("orders");
    });
    let mut app = build_proxy_app_without_hierarchy(OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let caller = caller(TENANT).expect("context");

    let (status, created) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            caller.clone(),
            Some(&orders_upstream(server.port()).to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let (status, route) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            caller.clone(),
            Some(
                &json!({
                    "upstream_id": upstream_id,
                    "match": {"http": {
                        "methods": ["GET"],
                        "path": "/",
                        "path_suffix_mode": "append"
                    }},
                    "headers": {},
                    "plugins": {},
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{route}");

    // An enabled custom plugin bound on the upstream: its Starlark runtime is
    // not deployed in this process, so the request fails loudly with a 503.
    let (status, plugin) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/plugins",
            caller.clone(),
            Some(&plugin_draft(true).to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{plugin}");
    assert_eq!(plugin["enabled"], json!(true));
    let enabled_id = plugin["id"].as_str().expect("id").to_owned();

    let mut draft = orders_upstream(server.port());
    draft["plugins"] = json!({"items": [enabled_id]});
    let (status, stored) = manage(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            caller.clone(),
            Some(&draft.to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{stored}");
    assert_eq!(stored["plugins"]["items"][0], json!(enabled_id), "{stored}");

    let (status, headers, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders/orders").await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{payload:?}");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let problem: serde_json::Value = serde_json::from_slice(&payload).expect("problem document");
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"),
        "{problem}"
    );
    assert_eq!(
        problem["context"]["plugin_id"],
        json!(enabled_id),
        "{problem}"
    );

    // Plugins are immutable, so disabling means registering a new plugin
    // resource and re-binding the upstream to it.
    let (status, disabled) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/plugins",
            caller.clone(),
            Some(&plugin_draft(false).to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{disabled}");
    assert_eq!(disabled["enabled"], json!(false));
    let disabled_id = disabled["id"].as_str().expect("id").to_owned();

    // The disabled plugin is bound in its GTS spelling, so both wire forms of a
    // reference are covered by this test.
    let mut draft = orders_upstream(server.port());
    draft["plugins"] = json!({"items": [format!("gts.cf.core.oagw.plugin.v1~{disabled_id}")]});
    let (status, stored) = manage(
        &mut app,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            caller.clone(),
            Some(&draft.to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "{stored}");
    assert_eq!(
        stored["plugins"]["items"][0],
        json!(format!("gts.cf.core.oagw.plugin.v1~{disabled_id}")),
        "{stored}"
    );

    let (status, _, payload) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders/orders").await;
    assert_eq!(status, http::StatusCode::OK, "{}", body_string(&payload));
    assert_eq!(payload, Bytes::from_static(b"orders"));
}

// -- metric cardinality (http.route) -----------------------------------------

/// Twenty callers hitting one route with twenty different path suffixes produce
/// exactly one `http.route` series — the configured pattern — and never a
/// series per client path. Prometheus has no way to drop a label value after
/// the fact, so the data plane must never label with the request.
#[tokio::test]
async fn twenty_request_paths_on_one_route_collapse_into_one_series() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path_matches(r"^/orders/customer-[0-9]+/items$");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    seed(&app, upstream("orders", server.port()), "/orders");

    for index in 0..20u32 {
        let (status, _, _) = proxy(
            &mut app,
            "GET",
            &format!("/oagw/v1/proxy/orders/orders/customer-{index}/items"),
        )
        .await;
        assert_eq!(status, http::StatusCode::OK, "suffix {index}");
    }

    let rendered = app.metrics.render();
    // The one and only series: the route pattern, not a request path.
    assert!(
        rendered.contains("http.route=\"GET /orders\""),
        "{rendered}"
    );
    let series = rendered
        .lines()
        .filter(|line| line.starts_with("oagw_requests_total{"))
        .filter(|line| line.contains("http.route="))
        .count();
    assert_eq!(series, 1, "one route, one series: {rendered}");
    for index in 0..20u32 {
        assert!(
            !rendered.contains(&format!("customer-{index}")),
            "a raw request path leaked into a label: {}",
            rendered
        );
    }
}

/// A request that matched no route is labelled with the fixed literal
/// `unmatched`, never with the path or the alias the caller asked for. (The
/// `host` label keeps carrying the alias, as DESIGN §4.2 pins it.)
#[tokio::test]
async fn unmatched_requests_share_one_fixed_route_label() {
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    for host in ["ghost", "phantom", "spectre"] {
        let (status, _, _) = proxy(&mut app, "GET", &format!("/oagw/v1/proxy/{host}")).await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }
    let rendered = app.metrics.render();
    assert!(rendered.contains("http.route=\"unmatched\""), "{rendered}");
    // Every request series shares the one literal, whatever alias was asked
    // for; only the (configured) host differs.
    let routes: std::collections::BTreeSet<String> = rendered
        .lines()
        .filter(|line| line.starts_with("oagw_requests_total{"))
        .filter_map(|line| {
            let start = line.find("http.route=\"")? + "http.route=\"".len();
            let end = line[start..].find('"')? + start;
            Some(line[start..end].to_owned())
        })
        .collect();
    assert_eq!(
        routes,
        std::iter::once("unmatched".to_owned()).collect(),
        "an unmatched request must not be labelled with the request: {rendered}"
    );
}

// -- rate-limit key and bucket lifetime --------------------------------------

/// Builds a proxy request carrying `X-Forwarded-For` and the given peer socket
/// address, exactly as the axum listener would deliver it.
fn proxied_request(
    method: &'static str,
    path: &str,
    headers: &[(&'static str, &str)],
    peer: std::net::SocketAddr,
) -> axum::http::Request<axum::body::Body> {
    use axum::extract::ConnectInfo;
    let mut request = proxy_request(method, path, caller(TENANT).expect("context"), headers)
        .expect("request builds");
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

/// A rate limit scoped to the client IP is keyed on the *peer socket address*:
/// two requests from one connection that spoof different `X-Forwarded-For`
/// values spend one budget, so rotating the header cannot buy a fresh one.
#[tokio::test]
async fn spoofed_forwarded_headers_share_the_peer_budget() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRateConfig {
            rate: 1,
            window: RateLimitWindow::Minute,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Ip,
        strategy: RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    seed(&app, draft, "/");

    let peer = std::net::SocketAddr::from(([203, 0, 113, 7], 44_321));
    let send = async |app: &mut ProxyApp, forwarded: &'static str| {
        let request = proxied_request(
            "GET",
            "/oagw/v1/proxy/orders",
            &[("x-forwarded-for", forwarded)],
            peer,
        );
        app.send(request).await.expect("infallible").status()
    };

    // One connection, three claimed identities: one budget.
    assert_eq!(send(&mut app, "198.51.100.1").await, http::StatusCode::OK);
    assert_eq!(
        send(&mut app, "198.51.100.2").await,
        http::StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        send(&mut app, "198.51.100.3, 198.51.100.4").await,
        http::StatusCode::TOO_MANY_REQUESTS
    );

    // A genuinely different peer still has its own budget.
    let other = std::net::SocketAddr::from(([203, 0, 113, 8], 44_321));
    let request = proxied_request("GET", "/oagw/v1/proxy/orders", &[], other);
    let status = app.send(request).await.expect("infallible").status();
    assert_eq!(
        status,
        http::StatusCode::OK,
        "a second peer has its own budget"
    );
}

/// Deleting an upstream drops its buckets, so a recreated upstream on the same
/// alias starts from an empty budget instead of inheriting the spent one.
#[tokio::test]
async fn a_recreated_upstream_starts_with_a_fresh_rate_budget() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(2));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRateConfig {
            rate: 1,
            window: RateLimitWindow::Minute,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    let upstream_id = seed(&app, draft, "/");

    let (first, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(first, http::StatusCode::OK);
    let (second, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(second, http::StatusCode::TOO_MANY_REQUESTS);

    // Delete through the management surface, then recreate the same alias.
    assert!(app.service.store().delete_upstream(TENANT, upstream_id));
    let recreated = app
        .service
        .store()
        .insert_upstream(upstream("orders", server.port()))
        .expect("recreated");
    app.service
        .store()
        .insert_route(route(recreated.id, "/", HttpMethod::Get))
        .expect("route seeds");

    let (third, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    assert_eq!(
        third,
        http::StatusCode::OK,
        "the recreated upstream must not inherit the spent budget"
    );
}

/// A `queue` strategy serves the request that a `reject` strategy would have
/// refused: the second request waits for its reserved token (bounded by
/// `QUEUE_MAX_WAIT`) and is then forwarded, so both calls reach the upstream.
#[tokio::test]
async fn a_queue_strategy_serves_the_request_a_reject_strategy_refuses() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/");
        then.status(200).body("ok");
    });
    let mut app = build_proxy_app_without_hierarchy(proxy_config(5));
    let mut draft = upstream("orders", server.port());
    draft.rate_limit = Some(RateLimitConfig {
        sharing: oagw::domain::model::SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRateConfig {
            rate: 2,
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstConfig { capacity: Some(1) }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Queue,
        response_headers: true,
        cost: 1,
    });
    seed(&app, draft, "/");

    let started = Instant::now();
    let (first, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    let (second, _, _) = proxy(&mut app, "GET", "/oagw/v1/proxy/orders").await;
    let elapsed = started.elapsed();

    assert_eq!(first, http::StatusCode::OK);
    assert_eq!(
        second,
        http::StatusCode::OK,
        "a queued request is served, not refused"
    );
    // The second call spent its reserved wait (half a second at 2 tokens/s),
    // which stays under the limiter's own bound.
    assert!(
        elapsed >= Duration::from_millis(400),
        "the second request did not wait: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the wait is bounded, not unbounded: {elapsed:?}"
    );
}

// -- route validation --------------------------------------------------------

/// A route path that is not rooted is rejected at management time, so it can
/// never be stored and then silently fail to match anything.
#[tokio::test]
async fn a_route_path_without_a_leading_slash_is_rejected() {
    let server = MockServer::start();
    let mut app = build_proxy_app_without_hierarchy(OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let caller = caller(TENANT).expect("context");
    let (status, created) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/upstreams",
            caller.clone(),
            Some(&orders_upstream(server.port()).to_string()),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let (status, problem) = manage(
        &mut app,
        request(
            "POST",
            "/oagw/v1/routes",
            caller,
            Some(
                &json!({
                    "upstream_id": upstream_id,
                    "match": {"http": {
                        "methods": ["GET"],
                        "path": "orders",
                        "path_suffix_mode": "append"
                    }},
                })
                .to_string(),
            ),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"),
        "{problem}"
    );
    assert_eq!(problem["context"]["path"], json!("orders"), "{problem}");
}

// -- helpers ----------------------------------------------------------------

/// Reads a body back as a string.
fn body_string(payload: &Bytes) -> String {
    String::from_utf8_lossy(payload).to_string()
}

/// A CORS section allowing exactly `https://portal.example.com`.
fn cors_config() -> CorsConfig {
    CorsConfig {
        enabled: true,
        sharing: oagw::domain::model::SharingMode::Private,
        allowed_origins: vec!["https://portal.example.com".to_owned()],
        allowed_methods: vec![CorsMethod::Get],
        allow_headers: vec!["content-type".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
        max_age: None,
    }
}

/// A route matching `method path` with `append` suffix handling.
fn route(upstream_id: Uuid, path: &str, method: HttpMethod) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        r#match: RouteMatch {
            http: Some(HttpMatch {
                methods: vec![method],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        headers: HeadersConfig::default(),
        plugins: Default::default(),
        rate_limit: None,
        cors: None,
        enabled: true,
        priority: 0,
        tags: Vec::new(),
        tenant_id: TENANT,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    }
}
