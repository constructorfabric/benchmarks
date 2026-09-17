//! Control-plane REST tests using the `Router::oneshot` pattern.
//!
//! These exercise the management API (`/oagw/v1/upstreams`, `/routes`,
//! `/plugins`) exactly as the gateway would: a `SecurityContext` extension
//! carries the tenant.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::extractors::ApiState;
use oagw::api::rest::{BASE, register_routes};
use oagw::config::OagwConfig;
use oagw::domain::plugin::PluginRegistry;
use oagw::domain::services::{PluginService, RouteService, UpstreamService};
use oagw::infra::proxy::ProxyEngine;
use oagw::infra::storage::InMemoryStore;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;

/// Registry that accepts everything: schemas are irrelevant to these tests.
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

const TENANT_A: Uuid = Uuid::from_u128(0xa);
const TENANT_B: Uuid = Uuid::from_u128(0xb);

type TestRouter = Router;

struct Fixture {
    router: TestRouter,
}

fn config(allow_http: bool) -> Arc<OagwConfig> {
    let mut config = OagwConfig::default();
    config.allow_http_upstream = allow_http;
    config.proxy_timeout_secs = 2;
    Arc::new(config)
}

/// Build the whole OAGW REST surface over an in-memory store.
fn build(allow_http: bool) -> Fixture {
    let cfg = config(allow_http);
    let store = InMemoryStore::new();
    let registry = Arc::new(PluginRegistry::new());
    let upstreams = Arc::new(UpstreamService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&cfg),
        Arc::clone(&registry),
        Arc::new(oagw::domain::hierarchy::SingleTenantChain),
    ));
    let routes = Arc::new(RouteService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::RouteRepo>,
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&registry),
    ));
    let plugins = Arc::new(PluginService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::PluginRepo>,
    ));
    let engine = Arc::new(ProxyEngine::new(
        Arc::clone(&cfg),
        Arc::clone(&upstreams),
        Arc::clone(&routes),
        Arc::clone(&registry),
        Arc::clone(&store) as Arc<dyn oagw::domain::PluginRepo>,
    ));
    let state = ApiState {
        upstreams,
        routes,
        plugins,
        plugin_registry: registry,
        proxy: engine,
        config: cfg,
    };

    let openapi = NoopOpenApiRegistry;
    let router = register_routes(Router::new(), &openapi, state);
    Fixture { router }
}

fn ctx(tenant: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(tenant)
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

fn request(method: &str, uri: &str, body: Option<serde_json::Value>, tenant: Uuid) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).expect("json")),
        None => Body::empty(),
    };
    let mut req = builder.body(body).expect("request");
    req.extensions_mut().insert(ctx(tenant));
    req
}

async fn send(router: &TestRouter, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn https_upstream(alias: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] }
    });
    if let Some(alias) = alias {
        body["alias"] = serde_json::Value::String(alias.to_owned());
    }
    body
}

/// An IP-based upstream. DESIGN §"Alias Enforcement Rules" requires an
/// explicit alias here (IP endpoints are not derivable), and the alias is then
/// free-form — which is what the alias-uniqueness, tenancy and immutability
/// tests below exercise.
fn ip_upstream(alias: &str) -> serde_json::Value {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.1" } ] }
    })
}

async fn create_upstream(
    router: &TestRouter,
    body: serde_json::Value,
    tenant: Uuid,
) -> (StatusCode, serde_json::Value) {
    send(router, request("POST", &format!("{BASE}/upstreams"), Some(body), tenant)).await
}

// ── Upstreams ───────────────────────────────────────────────────────────

#[tokio::test]
async fn create_upstream_returns_201_and_the_derived_alias() {
    let fixture = build(true);
    let (status, body) = create_upstream(&fixture.router, https_upstream(None), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["tenant_id"], TENANT_A.to_string());
    assert!(body["id"].as_str().is_some());
}

#[tokio::test]
async fn http_scheme_is_accepted_when_allowed() {
    let fixture = build(true);
    let body = serde_json::json!({
        "enabled": true,
        "alias": "mock",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8080 } ] }
    });
    let (status, body) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

#[tokio::test]
async fn http_scheme_is_rejected_when_not_allowed() {
    let fixture = build(false);
    let body = serde_json::json!({
        "enabled": true,
        "alias": "mock",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 8080 } ] }
    });
    let (status, body) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn upstream_without_endpoints_is_rejected() {
    let fixture = build(true);
    let body = serde_json::json!({ "enabled": true, "server": { "endpoints": [] } });
    let (status, body) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn ip_endpoints_require_an_explicit_alias() {
    let fixture = build(true);
    let body = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.5" } ] }
    });
    let (status, body) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn missing_body_is_a_400() {
    let fixture = build(true);
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/upstreams"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let fixture = build(true);
    let req = Request::builder()
        .method("POST")
        .uri(format!("{BASE}/upstreams"))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .expect("request");
    let mut req = req;
    req.extensions_mut().insert(ctx(TENANT_A));
    let (status, body) = send(&fixture.router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn a_null_body_is_a_400() {
    let fixture = build(true);
    let (status, body) = create_upstream(&fixture.router, serde_json::Value::Null, TENANT_A).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn duplicate_alias_conflicts_with_409() {
    let fixture = build(true);
    let (first, _) = create_upstream(&fixture.router, ip_upstream("api"), TENANT_A).await;
    assert_eq!(first, StatusCode::CREATED);
    let (second, body) =
        create_upstream(&fixture.router, ip_upstream("API."), TENANT_A).await;
    assert_eq!(second, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn the_same_alias_is_allowed_for_a_different_tenant() {
    let fixture = build(true);
    let (a, _) = create_upstream(&fixture.router, ip_upstream("shared"), TENANT_A).await;
    let (b, body) = create_upstream(&fixture.router, ip_upstream("shared"), TENANT_B).await;
    assert_eq!(a, StatusCode::CREATED);
    assert_eq!(b, StatusCode::CREATED, "body: {body}");
}

#[tokio::test]
async fn get_unknown_upstream_is_404() {
    let fixture = build(true);
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams/{}", Uuid::now_v7()),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

#[tokio::test]
async fn list_returns_an_odata_envelope() {
    let fixture = build(true);
    let _ = create_upstream(&fixture.router, ip_upstream("one"), TENANT_A).await;
    let _ = create_upstream(&fixture.router, ip_upstream("two"), TENANT_A).await;
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2);
    assert_eq!(body["value"].as_array().expect("value").len(), 2);
    assert_eq!(body["items"].as_array().expect("items").len(), 2);
}

#[tokio::test]
async fn tenants_do_not_see_each_other() {
    let fixture = build(true);
    let _ = create_upstream(&fixture.router, ip_upstream("mine"), TENANT_A).await;
    let (status, _) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams"),
            None,
            TENANT_B,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams"), None, TENANT_B),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn secrets_are_redacted_in_responses() {
    let fixture = build(true);
    let body = serde_json::json!({
        "enabled": true,
        "alias": "secure",
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.6" } ] },
        "auth": {
            "type": "apikey",
            "config": { "header": "x-api-key", "value": "sk-super-secret" }
        }
    });
    let (status, body) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rendered = serde_json::to_string(&body).expect("json");
    assert!(!rendered.contains("sk-super-secret"), "secret leaked: {rendered}");
    assert_eq!(body["auth"]["config"]["value"], "[REDACTED]");
}

#[tokio::test]
async fn delete_upstream_returns_204_then_404() {
    let fixture = build(true);
    let (status, body) =
        create_upstream(&fixture.router, ip_upstream("gone"), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let id = body["id"].as_str().expect("id").to_owned();

    let (status, _) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/upstreams/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/upstreams/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_gts_instance_id_is_accepted_as_the_resource_id() {
    let fixture = build(true);
    let (_, body) = create_upstream(&fixture.router, ip_upstream("gts"), TENANT_A).await;
    let id = body["id"].as_str().expect("id").to_owned();
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams/gts.cf.core.oagw.upstream.v1~{id}"),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["id"], id);
}

// ── Routes ──────────────────────────────────────────────────────────────

async fn seed_upstream(fixture: &Fixture, alias: &str) -> String {
    let (status, body) =
        create_upstream(&fixture.router, ip_upstream(alias), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "seeding '{alias}' failed: {body}");
    body["id"].as_str().expect("id").to_owned()
}

#[tokio::test]
async fn create_route_returns_201() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "chat").await;
    let body = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/chat" } }
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(body), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["upstream_id"], upstream);
}

#[tokio::test]
async fn a_route_for_an_unknown_upstream_is_rejected() {
    let fixture = build(true);
    let body = serde_json::json!({
        "upstream_id": Uuid::now_v7(),
        "match": { "http": { "methods": ["GET"], "path": "/x" } }
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(body), TENANT_A),
    )
    .await;
    // The referenced upstream is addressed as a resource, so a missing one is a 404.
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

#[tokio::test]
async fn overlapping_routes_conflict() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "dup").await;
    let route = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let (status, _) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route.clone()), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_empty_path_is_rejected() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "empty").await;
    let body = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": "relative" } }
    });
    let (status, _) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(body), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn listing_routes_of_an_unknown_upstream_is_404() {
    let fixture = build(true);
    let (status, _) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams/{}/routes", Uuid::now_v7()),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_lifecycle_round_trip() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "round").await;
    let payload = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": "/v1/embeddings" } }
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(payload), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_str().expect("id").to_owned();

    let (status, body) = send(
        &fixture.router,
        request("PUT", &format!("{BASE}/routes/{id}"), Some(serde_json::json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["POST"], "path": "/v1/embeddings" } }
        })), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["match"]["http"]["methods"][0],
        serde_json::Value::String(String::from("POST"))
    );

    let (status, _) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams/{upstream}/routes"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/routes/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ── Plugins ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_builtin_catalog_is_advertised() {
    let fixture = build(true);
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins/catalog"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // With an empty registry every catalog-only family entry stays reserved.
    let reserved = &body["reserved"];
    assert_eq!(
        reserved["auth"].as_array().expect("reserved auth").len(),
        2,
        "body: {body}"
    );
    assert_eq!(
        reserved["guard"].as_array().expect("reserved guard").len(),
        2
    );
    assert_eq!(
        reserved["transform"]
            .as_array()
            .expect("reserved transform")
            .len(),
        2
    );
}

#[tokio::test]
async fn a_tenant_plugin_round_trips() {
    let fixture = build(true);
    let payload = serde_json::json!({
        "plugin_type": "transform",
        "name": "add-trace",
        "source": "function transform(ctx, req) { return req; }"
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/plugins"), Some(payload), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let id = body["id"].as_str().expect("id").to_owned();

    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "add-trace");

    let (status, _) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/plugins/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn an_empty_plugin_name_is_rejected() {
    let fixture = build(true);
    let payload = serde_json::json!({ "plugin_type": "transform", "name": "  " });
    let (status, _) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/plugins"), Some(payload), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Error surface ───────────────────────────────────────────────────────

#[tokio::test]
async fn errors_are_problem_documents_with_a_gts_type() {
    let fixture = build(true);
    let (status, body, source) = send_and_read_headers(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams/{}", Uuid::now_v7()),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["status"], 404);
    assert_eq!(body["title"], "Upstream not found");
    let problem_type = body["type"].as_str().expect("type");
    assert!(problem_type.starts_with("gts://gts.cf.core.errors.err.v1~cf.oagw."), "{problem_type}");
    assert_eq!(body["error_domain"], "oagw.v1");
    // ADR-0007: a gateway-generated error names its origin.
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(
        body["error_code"], "cf.oagw.upstream.conflict",
        "body: {body}"
    );
    assert!(body["detail"].as_str().is_some(), "body: {body}");
}

/// A response's body plus the value of `X-OAGW-Error-Source`.
async fn send_and_read_headers(
    router: &TestRouter,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value, Option<String>) {
    let response = router.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let source = response
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, source)
}

#[tokio::test]
async fn an_invalid_resource_id_is_a_400() {
    let fixture = build(true);
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams/not-a-uuid"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

// ── PUT semantics ───────────────────────────────────────────────────────
// DESIGN §"Upstream"/"Route" REST semantics: PUT is a **full replacement**
// and the alias is the routing key, so it survives a replacement that does
// not mention it.

#[tokio::test]
async fn put_replaces_the_whole_upstream_configuration() {
    let fixture = build(true);
    let body = serde_json::json!({
        "enabled": true,
        "alias": "rich",
        "tags": ["llm", "prod"],
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.9" } ] },
        "auth": { "type": "apikey", "config": { "header": "x-api-key", "value": "sk-secret" } },
        "rate_limit": {
            "sustained": { "rate": 5, "window": "minute" },
            "burst": { "capacity": 10 }
        },
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"]
        }
    });
    let (status, created) = create_upstream(&fixture.router, body, TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();

    // Only `server` and `alias` are supplied: every other facet reverts to its
    // default.
    let (status, replaced) = send(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/upstreams/{id}"),
            Some(serde_json::json!({
                "enabled": false,
                "alias": "rich",
                "server": { "endpoints": [ { "scheme": "https", "host": "api2.example.com" } ] }
            })),
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {replaced}");
    assert_eq!(replaced["id"], id.as_str());
    assert_eq!(replaced["enabled"], false);
    assert_eq!(
        replaced["tags"].as_array().expect("tags").len(),
        0,
        "tags are cleared by a full replacement: {replaced}"
    );
    assert!(replaced["auth"].is_null(), "auth must be dropped: {replaced}");
    assert!(replaced["rate_limit"].is_null(), "rate limit must be dropped: {replaced}");
    assert!(replaced["cors"].is_null(), "cors must be dropped: {replaced}");
    assert_eq!(replaced["server"]["endpoints"][0]["host"], "api2.example.com");
    // The alias is the routing key: it is kept when the payload omits it.
    assert_eq!(replaced["alias"], "rich");
}

#[tokio::test]
async fn a_replaced_upstream_keeps_its_identity_and_tenant() {
    let fixture = build(true);
    let (status, created) =
        create_upstream(&fixture.router, ip_upstream("ident"), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, replaced) = send(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/upstreams/{id}"),
            Some(ip_upstream("ident")),
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {replaced}");
    assert_eq!(replaced["id"], id.as_str());
    assert_eq!(replaced["tenant_id"], TENANT_A.to_string());
    assert_eq!(replaced["created_at"], created["created_at"], "created_at is immutable");
}

// ── OData list parameters ───────────────────────────────────────────────

#[tokio::test]
async fn list_supports_filter_top_and_skip() {
    let fixture = build(true);
    for alias in ["one", "two", "three"] {
        let (status, _) =
            create_upstream(&fixture.router, ip_upstream(alias), TENANT_A).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // $filter narrows to the matching alias.
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams?filter=alias%20eq%20%27two%27"),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], 1, "body: {body}");
    assert_eq!(body["value"][0]["alias"], "two");

    // $top limits the page while `count` still reports the whole set.
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams?top=1"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 3);
    assert_eq!(body["value"].as_array().expect("value").len(), 1);

    // $skip pages past the head of the list.
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams?skip=1"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 3);
    assert_eq!(body["value"].as_array().expect("value").len(), 2);
}

#[tokio::test]
async fn list_search_narrows_the_page() {
    let fixture = build(true);
    for alias in ["alpha", "beta"] {
        let (status, _) =
            create_upstream(&fixture.router, ip_upstream(alias), TENANT_A).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams?search=beta"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["count"], 1, "body: {body}");
    assert_eq!(body["value"][0]["alias"], "beta");
}

#[tokio::test]
async fn an_unsupported_filter_is_a_400() {
    let fixture = build(true);
    let _ = create_upstream(&fixture.router, ip_upstream("one"), TENANT_A).await;
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams?filter=name%20eq%20%27one%27"),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.validation.error.v1"),
        "body: {body}"
    );
}

/// DESIGN §"List Query Parameters" names the OData parameters with a `$`
/// prefix on every list endpoint.
#[tokio::test]
async fn list_honours_the_odata_dollar_prefixed_parameters() {
    let fixture = build(true);
    for alias in ["one", "two", "three"] {
        let (status, _) =
            create_upstream(&fixture.router, ip_upstream(alias), TENANT_A).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams?$top=1"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["value"].as_array().expect("value").len(),
        1,
        "?$top=1 was ignored: {body}"
    );
}

/// DESIGN §"List Query Parameters" requires `$orderby` on every list endpoint.
#[tokio::test]
async fn list_honours_orderby() {
    let fixture = build(true);
    for alias in ["aaa", "bbb", "ccc"] {
        let (status, _) =
            create_upstream(&fixture.router, ip_upstream(alias), TENANT_A).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams?orderby=alias%20desc"),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let aliases: Vec<&str> = body["value"]
        .as_array()
        .expect("value")
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .collect();
    assert_eq!(aliases, vec!["ccc", "bbb", "aaa"], "$orderby=alias desc was ignored");
}

// ── Tenancy ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn cross_tenant_access_is_a_404_not_a_403() {
    let fixture = build(true);
    let (_, created) =
        create_upstream(&fixture.router, ip_upstream("mine"), TENANT_A).await;
    let id = created["id"].as_str().expect("id").to_owned();

    for method in ["GET", "PUT", "DELETE"] {
        let (status, body) = send(
            &fixture.router,
            request(
                method,
                &format!("{BASE}/upstreams/{id}"),
                if method == "PUT" { Some(ip_upstream("mine")) } else { None },
                TENANT_B,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} must be a 404: {body}");
    }
}

#[tokio::test]
async fn cross_tenant_routes_are_invisible() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "owned").await;
    let route = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let (status, created) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, _) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/routes/{id}"), None, TENANT_B),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/upstreams/{upstream}/routes"),
            None,
            TENANT_B,
        ),
    )
    .await;
    // Another tenant's upstream does not exist here either.
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

// ── Referential integrity ───────────────────────────────────────────────

#[tokio::test]
async fn an_upstream_referenced_by_a_route_cannot_be_deleted() {
    let fixture = build(true);
    let upstream = seed_upstream(&fixture, "busy").await;
    let route = serde_json::json!({
        "upstream_id": upstream,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let (status, _) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/upstreams/{upstream}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.plugin.in_use.v1"),
        "the in-use error reuses the PluginInUse GTS id: {body}"
    );
    assert_eq!(body["context"]["referenced_by"], 1, "body: {body}");
    // The upstream is still there.
    let (status, _) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/upstreams/{upstream}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_plugin_bound_to_an_upstream_cannot_be_deleted() {
    let fixture = build(true);
    let payload = serde_json::json!({
        "plugin_type": "transform",
        "name": "bound",
        "source": "function transform(ctx, req) { return req; }"
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/plugins"), Some(payload), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let plugin = body["id"].as_str().expect("id").to_owned();

    let upstream = serde_json::json!({
        "enabled": true,
        "alias": "with-plugin",
        "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.7" } ] },
        "plugins": { "items": [plugin] }
    });
    let (status, body) = create_upstream(&fixture.router, upstream, TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");

    let (status, body) = send(
        &fixture.router,
        request("DELETE", &format!("{BASE}/plugins/{plugin}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert!(bare_type(&body).ends_with("cf.oagw.plugin.in_use.v1"), "body: {body}");
}

/// ADR-0002 / DESIGN §"Plugins": "Plugins are immutable (no PUT)".
#[tokio::test]
async fn plugins_are_immutable_after_creation() {
    let fixture = build(true);
    let payload = serde_json::json!({
        "plugin_type": "transform",
        "name": "frozen",
        "source": "function transform(ctx, req) { return req; }"
    });
    let (status, created) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/plugins"), Some(payload), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = send(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/plugins/{id}"),
            Some(serde_json::json!({
                "plugin_type": "transform",
                "name": "rewritten",
                "source": "function transform(ctx, req) { return null; }"
            })),
            TENANT_A,
        ),
    )
    .await;
    // A plugin has no update operation: the platform must not accept one.
    assert_ne!(
        status,
        StatusCode::OK,
        "PUT on a plugin mutated an immutable definition: {body}"
    );
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/plugins/{id}"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "frozen", "the plugin definition changed");
}

// ── gRPC surface ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_grpc_route_is_stored_but_not_proxied() {
    let fixture = build(true);
    let payload = serde_json::json!({
        "enabled": true,
        "alias": "grpc",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        "server": { "endpoints": [ { "scheme": "grpc", "host": "10.0.0.8" } ] }
    });
    let (status, body) = create_upstream(&fixture.router, payload, TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let upstream = body["id"].as_str().expect("id").to_owned();

    let route = serde_json::json!({
        "upstream_id": upstream,
        "match": { "grpc": { "service": "foo.v1.UserService", "method": "GetUser" } }
    });
    let (status, body) = send(
        &fixture.router,
        request("POST", &format!("{BASE}/routes"), Some(route), TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");

    // gRPC proxying is not available in this build, and the failure is a
    // 503 LinkUnavailable rather than a silent 404.
    let (status, body) = send(
        &fixture.router,
        request("GET", &format!("{BASE}/proxy/grpc/foo.v1.UserService/GetUser"), None, TENANT_A),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.link.unavailable.v1"),
        "body: {body}"
    );
}

// ── Error surface ───────────────────────────────────────────────────────

/// The bare GTS id behind a problem document's `type` URI.
fn bare_type(body: &serde_json::Value) -> String {
    body["type"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("gts://")
        .to_owned()
}

#[tokio::test]
async fn a_route_404_uses_its_own_gts_id() {
    let fixture = build(true);
    let (status, body) = send(
        &fixture.router,
        request(
            "GET",
            &format!("{BASE}/routes/{}", Uuid::now_v7()),
            None,
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.route.not_found.v1"),
        "body: {body}"
    );
}

#[tokio::test]
async fn an_alias_conflict_names_the_alias_in_its_context() {
    let fixture = build(true);
    let _ = create_upstream(&fixture.router, ip_upstream("taken"), TENANT_A).await;
    let (status, body) =
        create_upstream(&fixture.router, ip_upstream("taken"), TENANT_A).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.upstream.conflict.v1"),
        "body: {body}"
    );
    assert_eq!(body["context"]["alias"], "taken", "body: {body}");
}

#[tokio::test]
async fn an_invalid_alias_is_a_validation_error() {
    let fixture = build(true);
    let (status, body) =
        create_upstream(&fixture.router, ip_upstream("Not A Valid Alias"), TENANT_A).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.validation.error.v1"),
        "body: {body}"
    );
}

#[tokio::test]
async fn unusable_endpoint_hosts_report_the_routing_gts_ids() {
    let fixture = build(true);
    // No host at all → MissingTargetHost.
    let (status, body) = create_upstream(
        &fixture.router,
        serde_json::json!({
            "enabled": true,
            "alias": "no-host",
            "server": { "endpoints": [ { "scheme": "https", "host": "" } ] }
        }),
        TENANT_A,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.routing.missing_target_host.v1"),
        "body: {body}"
    );

    // A malformed hostname → InvalidTargetHost.
    let (status, body) = create_upstream(
        &fixture.router,
        serde_json::json!({
            "enabled": true,
            "alias": "bad-host",
            "server": { "endpoints": [ { "scheme": "https", "host": "-bad-.example.com" } ] }
        }),
        TENANT_A,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        bare_type(&body).ends_with("cf.oagw.routing.invalid_target_host.v1"),
        "body: {body}"
    );
    assert_eq!(body["context"]["host"], "-bad-.example.com", "body: {body}");
}

#[tokio::test]
async fn a_bare_public_suffix_suffix_pool_is_not_derivable() {
    let fixture = build(true);
    // `co.uk` is a public suffix, not a registrable domain, so the derivation
    // is rejected and an explicit alias is required.
    let (status, body) = create_upstream(
        &fixture.router,
        serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [
                { "scheme": "https", "host": "foo.co.uk" },
                { "scheme": "https", "host": "bar.co.uk" }
            ] }
        }),
        TENANT_A,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("alias is required"),
        "body: {body}"
    );

    // Supplying one makes the upstream acceptable.
    let (status, body) = create_upstream(
        &fixture.router,
        serde_json::json!({
            "enabled": true,
            "alias": "uk-pool",
            "server": { "endpoints": [
                { "scheme": "https", "host": "foo.co.uk" },
                { "scheme": "https", "host": "bar.co.uk" }
            ] }
        }),
        TENANT_A,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["alias"], "uk-pool");
}

/// A PUT is a full replacement, but the alias is the routing key in
/// `/v1/proxy/{alias}/...`: DESIGN §"Alias Update Behavior" makes it immutable.
#[tokio::test]
async fn a_put_that_omits_the_alias_keeps_the_routing_key() {
    let fixture = build(true);
    let (status, created) =
        create_upstream(&fixture.router, ip_upstream("kept"), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, body) = send(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/upstreams/{id}"),
            // DESIGN §"Alias Update Behavior": IP → hostname would re-key the
            // alias, so the PUT keeps the original IP endpoints and simply
            // omits the alias field.
            Some(serde_json::json!({
                "enabled": true,
                "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.1" } ] }
            })),
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["alias"], "kept",
        "the routing key disappeared after a PUT that omitted it: {body}"
    );
}

/// DESIGN §"Alias Enforcement Rules": on hostname endpoints the alias is
/// always auto-derived, and a differing user-provided alias is a 400; only the
/// exact derived value is tolerated, for idempotency.
#[tokio::test]
async fn a_differing_explicit_alias_on_hostname_endpoints_is_rejected() {
    let fixture = build(true);
    let (status, body) = create_upstream(
        &fixture.router,
        https_upstream(Some("not-the-derived-alias")),
        TENANT_A,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an explicit alias that differs from the derived one was accepted: {body}"
    );

    // The exact derived value is tolerated for idempotency.
    let (status, body) =
        create_upstream(&fixture.router, https_upstream(Some("api.openai.com")), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["alias"], "api.openai.com");
}

/// DESIGN §"Alias Update Behavior": an endpoint change that would alter the
/// derived alias is rejected — the operator must delete and re-create.
#[tokio::test]
async fn an_endpoint_change_that_changes_the_derived_alias_is_rejected() {
    let fixture = build(true);
    let (status, created) =
        create_upstream(&fixture.router, https_upstream(None), TENANT_A).await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["alias"], "api.openai.com");

    let (status, body) = send(
        &fixture.router,
        request(
            "PUT",
            &format!("{BASE}/upstreams/{id}"),
            Some(serde_json::json!({
                "enabled": true,
                "server": { "endpoints": [ { "scheme": "https", "host": "api.another-vendor.net" } ] }
            })),
            TENANT_A,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the derived alias silently changed from api.openai.com: {body}"
    );
}
