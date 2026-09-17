//! Hierarchical configuration: the data plane walks the caller's tenant chain
//! and folds the graded configuration of the ancestors above it.
//!
//! The chain is provided by a static parent map (`StaticChain`), which makes
//! shadowing, inheritance and the disable rules deterministic without a
//! `tenant-resolver` gear. `ResolverChain` — the implementation the gear wires
//! to the platform resolver — is exercised through its degrade path and the
//! merge table is covered by the unit tests in `domain::hierarchy`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use httpmock::prelude::GET;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::extractors::ApiState;
use oagw::api::rest::{BASE, register_routes};
use oagw::config::OagwConfig;
use oagw::domain::hierarchy::StaticChain;
use oagw::domain::services::{PluginService, RouteService, UpstreamService};
use oagw::infra::proxy::ProxyEngine;
use oagw::infra::storage::InMemoryStore;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// Tenant ids shaped like the hierarchy of `config/e2e-local.yaml`.
const ROOT: Uuid = Uuid::from_u128(0x0001);
const L1A: Uuid = Uuid::from_u128(0x0002);
const L1B: Uuid = Uuid::from_u128(0x0005);
const L2B: Uuid = Uuid::from_u128(0x0004);
const DELETED: Uuid = Uuid::from_u128(0x0003);
/// A child of the deleted tenant: the walk has to step over its ancestor.
const ORPHAN: Uuid = Uuid::from_u128(0x0007);
/// A sibling hierarchy that must stay completely isolated.
const OUTSIDER: Uuid = Uuid::from_u128(0x9001);

/// Fully-qualified built-in transform plugin id.
const REQUEST_ID_PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

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

struct Fixture {
    router: Router,
    #[allow(dead_code)]
    cfg: Arc<OagwConfig>,
}

/// `ROOT → [L1A, L1B]`, `L1A → L2B`, plus a deleted leaf under `L1A` with a
/// child of its own and a separate branch rooted at `OUTSIDER`.
fn hierarchy() -> StaticChain {
    StaticChain::new(BTreeMap::from([
        (L2B, L1A),
        (L1A, ROOT),
        (L1B, ROOT),
        (ORPHAN, DELETED),
        (DELETED, L1A),
        (OUTSIDER, ROOT),
    ]))
    .with_inactive(DELETED)
}

fn build() -> Fixture {
    build_with_chain(hierarchy())
}

fn build_with_chain(chain: StaticChain) -> Fixture {
    let cfg = Arc::new(OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 5,
        connect_timeout_secs: 3,
        ..OagwConfig::default()
    });
    let store = InMemoryStore::new();
    let registry = Arc::new(oagw::infra::plugin::builtin_registry(
        oagw::infra::plugin::CredentialSource::inline_only(),
        64,
    ));
    let upstreams = Arc::new(UpstreamService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&cfg),
        Arc::clone(&registry),
        Arc::new(chain),
    ));
    let routes = Arc::new(RouteService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::RouteRepo>,
        Arc::clone(&store) as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&registry),
    ));
    let plugins = Arc::new(PluginService::new(
        Arc::clone(&store) as Arc<dyn oagw::domain::PluginRepo>
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
        plugin_registry: Arc::clone(&registry),
        proxy: engine,
        config: Arc::clone(&cfg),
    };
    let openapi = NoopOpenApiRegistry;
    let router = register_routes(Router::new(), &openapi, state);
    Fixture { router, cfg }
}

fn ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(tenant)
        .subject_type("user")
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

fn request(tenant: Uuid, method: &str, uri: &str, body: Option<String>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = Body::from(body.unwrap_or_default());
    let mut req = builder.body(body).expect("request");
    req.extensions_mut().insert(ctx(tenant));
    req
}

async fn send(
    fixture: &Fixture,
    tenant: Uuid,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let payload = body.map(|value| value.to_string());
    let response = fixture
        .router
        .clone()
        .oneshot(request(tenant, method, uri, payload))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Like [`send`], but returns the raw body: a proxied response is *not* JSON.
async fn body_of(
    fixture: &Fixture,
    tenant: Uuid,
    method: &str,
    uri: &str,
) -> (axum::http::StatusCode, Vec<u8>) {
    let response = fixture
        .router
        .clone()
        .oneshot(request(tenant, method, uri, None))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, bytes.to_vec())
}

async fn create(fixture: &Fixture, tenant: Uuid, path: &str, payload: serde_json::Value) -> String {
    let (status, body) = send(
        fixture,
        tenant,
        "POST",
        &format!("{BASE}{path}"),
        Some(payload),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{path}: {body}");
    body["id"].as_str().expect("id").to_owned()
}

/// An upstream at `http://127.0.0.1:{port}` with the explicit alias `target`,
/// with `extra` merged into the payload.
fn target(server: &httpmock::MockServer, extra: serde_json::Value) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "enabled": true,
        "alias": "target",
        "server": {
            "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ]
        }
    });
    if let (Some(base), Some(add)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in add {
            base.insert(key.clone(), value.clone());
        }
    }
    payload
}

/// Create the shared upstream plus the single route that serves `/hello`.
async fn wire(
    fixture: &Fixture,
    tenant: Uuid,
    server: &httpmock::MockServer,
    extra: serde_json::Value,
) -> String {
    let id = create(fixture, tenant, "/upstreams", target(server, extra)).await;
    create(
        fixture,
        tenant,
        "/routes",
        serde_json::json!({
            "upstream_id": id,
            "match": { "http": { "methods": ["GET"], "path": "/hello" } }
        }),
    )
    .await;
    id
}

/// Flip the shared upstream's `enabled` flag off through the management API.
async fn disable(fixture: &Fixture, tenant: Uuid, id: &str, server: &httpmock::MockServer) {
    let (status, body) = send(
        fixture,
        tenant,
        "PUT",
        &format!("{BASE}/upstreams/{id}"),
        Some(serde_json::json!({
            "enabled": false,
            "alias": "target",
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ] }
        })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
}

// ── Inheritance ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_descendant_proxies_through_an_ancestor_upstream() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("from-root");
    });
    let fixture = build();
    wire(&fixture, ROOT, &server, serde_json::Value::Null).await;

    // Neither L1A nor its leaf L2B defines `target`; both inherit the root's.
    for tenant in [L1A, L2B] {
        let (status, body) = body_of(
            &fixture,
            tenant,
            "GET",
            &format!("{BASE}/proxy/target/hello"),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{tenant}");
        assert_eq!(String::from_utf8_lossy(&body), "from-root");
    }
    hits.assert_calls(2);
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_auth_injects_the_ancestor_credential() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/hello")
            .header("x-api-key", "sk-root-key");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(
        &fixture,
        ROOT,
        &server,
        serde_json::json!({ "auth": {
            "type": "apikey",
            "sharing": "inherit",
            "config": { "value": "sk-root-key", "header": "x-api-key" }
        }}),
    )
    .await;

    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_descendant_with_its_own_credentials_overrides_inherit() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/hello")
            .header("x-api-key", "sk-leaf-key");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(
        &fixture,
        ROOT,
        &server,
        serde_json::json!({ "auth": {
            "type": "apikey",
            "sharing": "inherit",
            "config": { "value": "sk-root-key", "header": "x-api-key" }
        }}),
    )
    .await;
    // The leaf shadows the alias and brings its own key.
    wire(
        &fixture,
        L1A,
        &server,
        serde_json::json!({ "auth": {
            "type": "apikey",
            "sharing": "private",
            "config": { "value": "sk-leaf-key", "header": "x-api-key" }
        }}),
    )
    .await;

    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn enforced_ancestor_credentials_cannot_be_overridden() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET)
            .path("/hello")
            .header("x-api-key", "sk-enforced-key");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(
        &fixture,
        ROOT,
        &server,
        serde_json::json!({ "auth": {
            "type": "apikey",
            "sharing": "enforce",
            "config": { "value": "sk-enforced-key", "header": "x-api-key" }
        }}),
    )
    .await;
    wire(
        &fixture,
        L1A,
        &server,
        serde_json::json!({ "auth": {
            "type": "apikey",
            "sharing": "private",
            "config": { "value": "sk-leaf-key", "header": "x-api-key" }
        }}),
    )
    .await;

    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    hits.assert();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_enforced_ancestor_rate_limit_caps_a_looser_descendant() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(
        &fixture,
        ROOT,
        &server,
        serde_json::json!({ "rate_limit": {
            "sharing": "enforce",
            "algorithm": "token_bucket",
            "sustained": { "rate": 1, "window": "minute" },
            "burst": { "capacity": 1 },
            "scope": "ip"
        }}),
    )
    .await;
    wire(
        &fixture,
        L1A,
        &server,
        serde_json::json!({ "rate_limit": {
            "sharing": "private",
            "algorithm": "token_bucket",
            "sustained": { "rate": 1_000, "window": "second" },
            "burst": { "capacity": 1_000 },
            "scope": "ip"
        }}),
    )
    .await;

    // The ancestor's 1/minute wins over the descendant's 1 000/second.
    let (status, _) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS, "{body}");
    hits.assert_calls(1);
}

#[tokio::test(flavor = "multi_thread")]
async fn enforced_ancestor_plugins_cannot_be_dropped_by_shadowing() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/hello");
        // The upstream does *not* send the header the ancestor's guard demands.
        then.status(200).body("ok");
    });
    let fixture = build();
    // The ancestor stores the guard as a custom plugin row and binds it with
    // `sharing: enforce`.
    let guard = create(
        &fixture,
        ROOT,
        "/plugins",
        serde_json::json!({
            "plugin_type": "guard",
            "name": "response-signature",
            "source": "function guard(ctx, req) { return true; }",
            "config": { "type": "required_headers", "required_response_headers": "x-signature" }
        }),
    )
    .await;
    wire(
        &fixture,
        ROOT,
        &server,
        serde_json::json!({ "plugins": {
            "sharing": "enforce",
            "items": [guard]
        }}),
    )
    .await;
    wire(
        &fixture,
        L1A,
        &server,
        serde_json::json!({ "plugins": {
            "sharing": "private",
            "items": [REQUEST_ID_PLUGIN]
        }}),
    )
    .await;

    // The ancestor's guard must still police the response even though the
    // leaf's own binding does not name it — otherwise shadowing would be a way
    // to drop an enforced plugin. The row lives in the ancestor's tenant, so
    // the binding has to resolve through the chain as well.
    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(
        body["error_code"], "cf.oagw.required_header.missing",
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inherited_upstream_keeps_its_ancestors_routes() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("from-root");
    });
    let fixture = build();
    // The root defines the upstream *and* its route; a descendant only
    // inherits, so the ancestor's route has to be found for its own upstream.
    wire(&fixture, ROOT, &server, serde_json::Value::Null).await;

    let (status, body) = body_of(&fixture, L1A, "GET", &format!("{BASE}/proxy/target/hello")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(String::from_utf8_lossy(&body), "from-root");
    hits.assert();
}

// ── Shadowing ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_closer_definition_shadows_the_ancestor_target() {
    let root_server = httpmock::MockServer::start();
    let root_hits = root_server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("from-root");
    });
    let leaf_server = httpmock::MockServer::start();
    let leaf_hits = leaf_server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("from-leaf");
    });
    let fixture = build();
    wire(&fixture, ROOT, &root_server, serde_json::Value::Null).await;
    wire(&fixture, L1A, &leaf_server, serde_json::Value::Null).await;

    let (status, body) = body_of(&fixture, L1A, "GET", &format!("{BASE}/proxy/target/hello")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(String::from_utf8_lossy(&body), "from-leaf");
    leaf_hits.assert();

    // A sibling branch that never shadowed the alias still reaches the root's.
    let (status, body) = body_of(&fixture, L1B, "GET", &format!("{BASE}/proxy/target/hello")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(String::from_utf8_lossy(&body), "from-root");
    root_hits.assert_calls(1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_outside_the_chain_cannot_reach_a_foreign_upstream() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("ok");
    });
    let fixture = build();
    wire(&fixture, L1A, &server, serde_json::Value::Null).await;

    // `OUTSIDER` sits in a different branch: L1A is not one of its ancestors,
    // so the alias is unknown there.
    let (status, body) = send(
        &fixture,
        OUTSIDER,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");
    hits.assert_calls(0);
}

// ── Enable / disable ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_ancestor_disabled_upstream_is_disabled_for_all_descendants() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("ok");
    });
    let fixture = build();
    let id = wire(&fixture, ROOT, &server, serde_json::Value::Null).await;
    disable(&fixture, ROOT, &id, &server).await;

    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{body}"
    );
    let (status, _) = send(
        &fixture,
        L2B,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    hits.assert_calls(0);
}

#[tokio::test(flavor = "multi_thread")]
async fn shadowing_cannot_re_enable_an_ancestor_disabled_upstream() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("ok");
    });
    let fixture = build();
    let id = wire(&fixture, ROOT, &server, serde_json::Value::Null).await;
    disable(&fixture, ROOT, &id, &server).await;

    // The leaf defines its own *enabled* upstream with the same alias — the
    // ancestor's disable still wins.
    wire(&fixture, L1A, &server, serde_json::Value::Null).await;
    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{body}"
    );
    hits.assert_calls(0);

    // And the ancestor's own tenant is equally blocked.
    let (status, _) = send(
        &fixture,
        ROOT,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_upstream_is_a_503_in_its_own_tenant_too() {
    let server = httpmock::MockServer::start();
    let fixture = build();
    let id = wire(&fixture, L1A, &server, serde_json::Value::Null).await;
    disable(&fixture, L1A, &id, &server).await;

    let (status, body) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{body}"
    );
    assert_eq!(body["error_code"], "cf.oagw.link.unavailable", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_tenant_contributes_nothing_to_the_walk() {
    let server = httpmock::MockServer::start();
    let hits = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200).body("ok");
    });
    let fixture = build();
    // The deleted tenant sits between `ORPHAN` and the rest of the tree.
    wire(&fixture, DELETED, &server, serde_json::Value::Null).await;

    let (status, body) = send(
        &fixture,
        ORPHAN,
        "GET",
        &format!("{BASE}/proxy/target/hello"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");
    hits.assert_calls(0);
}

// ── Visibility ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_shows_shared_ancestor_definitions_but_not_private_ones() {
    let server = httpmock::MockServer::start();
    let fixture = build();
    wire(&fixture, ROOT, &server, serde_json::Value::Null).await;

    // A `private` definition — the default — stays invisible to the descendant.
    let (status, body) = send(&fixture, L1A, "GET", &format!("{BASE}/upstreams"), None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["count"], 0, "private ancestors are not listed: {body}");

    // A shared one is visible from below.
    let id = create(&fixture, ROOT, "/upstreams", serde_json::json!({
        "enabled": true,
        "alias": "shared-service",
        "rate_limit": {
            "sharing": "enforce",
            "sustained": { "rate": 1, "window": "minute" },
            "burst": { "capacity": 1 }
        },
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ] }
    })).await;

    let (status, body) = send(&fixture, L1A, "GET", &format!("{BASE}/upstreams"), None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let aliases: Vec<&str> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["alias"].as_str())
        .collect();
    assert!(
        aliases.contains(&"shared-service"),
        "the inherited definition is listed: {aliases:?}"
    );

    // The owner's own listing still holds both of its definitions.
    let (status, body) = send(&fixture, ROOT, "GET", &format!("{BASE}/upstreams"), None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["count"], 2, "{body}");

    // …but a shared ancestor resource is not addressable by id from below.
    let (status, _) = send(
        &fixture,
        L1A,
        "GET",
        &format!("{BASE}/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

// ── Degradation ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_resolution_without_a_security_context_stays_in_its_own_tenant() {
    let cfg = Arc::new(OagwConfig::default());
    let store = InMemoryStore::new();
    let registry = Arc::new(oagw::infra::plugin::builtin_registry(
        oagw::infra::plugin::CredentialSource::inline_only(),
        64,
    ));
    let upstreams = Arc::new(UpstreamService::new(
        store as Arc<dyn oagw::domain::UpstreamRepo>,
        Arc::clone(&cfg),
        Arc::clone(&registry),
        Arc::new(hierarchy()),
    ));

    let config = oagw::domain::UpstreamConfig {
        alias: Some(String::from("target")),
        server: oagw::domain::Server {
            // An IP endpoint is non-derivable, so the explicit alias is legal.
            endpoints: vec![oagw::domain::Endpoint {
                scheme: oagw::domain::EndpointScheme::Https,
                host: String::from("10.0.0.5"),
                port: None,
            }],
        },
        ..oagw::domain::UpstreamConfig::default()
    };
    upstreams
        .create(L1A, config.clone())
        .await
        .expect("upstream");

    // Without a security context there is no chain to walk, so an alias owned
    // by an ancestor is simply unknown — never a cross-tenant leak.
    let resolved = upstreams.resolve_effective(None, L2B, "target").await;
    assert!(
        matches!(resolved, Err(oagw::domain::DomainError::UpstreamNotFound)),
        "expected a 404, got {resolved:?}"
    );
    // …while the owning tenant still resolves it.
    let resolved = upstreams.resolve_effective(None, L1A, "target").await;
    assert!(resolved.is_ok(), "{resolved:?}");
    let resolved = resolved.expect("resolved");
    // Degraded: without a context there is no chain to walk, so the caller's
    // own tenant is the whole chain.
    assert_eq!(resolved.chain, vec![L1A]);
}
