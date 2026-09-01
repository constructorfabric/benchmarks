//! End-to-end tests for the 15 management routes (DESIGN §3.3).
//!
//! The router under test is built exactly as the gear builds it
//! ([`crate::api::rest::routes::register_routes`]), so the assertions cover the
//! registration, the wire shapes, the status codes, the pagination contract and
//! the RFC 9457 error rendering.
//!
//! Review evidence (privilege boundary — tenant scoping):
//! * Guardrail: DESIGN §3.3 "Tenant Scoping" — every operation is scoped to the
//!   caller's tenant; ancestors are invisible and not addressable.
//! * Rationale: the REST layer is the only place where the tenant identity
//!   enters the domain, so the tests drive it through the same `SecurityContext`
//!   extension the production middleware installs.
//! * Validation performed: `tenant_isolation_*` tests assert that a second
//!   tenant gets 404 for both the resource id and the alias conflict.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_routes;
use crate::config::OagwConfig;
use crate::domain::service::ControlPlaneService;
use crate::infra::storage::InMemoryStore;
use toolkit_security::context::SecurityContext;

const BASE: &str = "/api/oagw/v1";

fn service() -> Arc<ControlPlaneService> {
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    ControlPlaneService::new(InMemoryStore::new(), &config).into()
}

fn router() -> Router {
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, service())
}

fn ctx(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("valid SecurityContext")
}

async fn call(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    tenant_id: Uuid,
) -> (axum::http::StatusCode, axum::http::HeaderMap, Value) {
    let request = request(method, uri, body, tenant_id);
    let response = router.oneshot(request).await.expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, value)
}

fn request(
    method: &str,
    uri: &str,
    body: Option<Value>,
    tenant_id: Uuid,
) -> Request<axum::body::Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(value) => axum::body::Body::from(serde_json::to_vec(&value).unwrap()),
        None => axum::body::Body::empty(),
    };
    let mut request = builder.body(body).expect("request builds");
    request.extensions_mut().insert(ctx(tenant_id));
    request
}

fn upstream_payload(alias: Option<&str>, host: &str) -> Value {
    upstream_pool_payload(alias, &[host])
}

/// Builds an upstream payload whose endpoint pool is `hosts`.
///
/// A pool of several hostnames under the same registrable domain derives the
/// domain itself as the alias (`a.example.org` + `b.example.org` →
/// `example.org`), which is how the tests obtain a non-hostname alias.
fn upstream_pool_payload(alias: Option<&str>, hosts: &[&str]) -> Value {
    let endpoints: Vec<Value> = hosts
        .iter()
        .map(|host| json!({ "host": host }))
        .collect();
    let mut body = json!({
        "server": { "endpoints": endpoints },
        "tags": ["test"]
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

fn route_payload(upstream_id: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match_config": {
            "http": { "methods": ["GET"], "path": "/v1/chat" }
        }
    })
}

async fn create_upstream(router: &Router, tenant_id: Uuid, alias: Option<&str>) -> Value {
    create_upstream_with(router, tenant_id, alias, &["api.openai.com"]).await
}

/// Creates an upstream for a specific endpoint pool, asserting success.
async fn create_upstream_with(
    router: &Router,
    tenant_id: Uuid,
    alias: Option<&str>,
    hosts: &[&str],
) -> Value {
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/upstreams"),
        Some(upstream_pool_payload(alias, hosts)),
        tenant_id,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    body
}

// ---------------------------------------------------------------------------
// Upstreams (5 operations)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_crud_round_trip() {
    let router = router();
    let tenant = Uuid::new_v4();

    // POST -> 201 + Location
    let (status, headers, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/upstreams"),
        Some(upstream_payload(None, "api.openai.com")),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let location = headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("Location header");
    assert!(location.starts_with("/api/oagw/v1/upstreams/"), "{location}");
    let id = body["id"].as_str().expect("id").to_owned();
    assert!(id.starts_with("gts.cf.core.oagw.upstream.v1~"), "{id}");
    // `Location` is the request path plus the anonymous GTS id.
    assert_eq!(location, format!("{BASE}/upstreams/{id}"));
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["tags"], json!(["test"]));

    // GET by full GTS id
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams/{id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["id"], id);

    // GET by bare UUID
    let bare = id.rsplit('~').next().expect("uuid");
    let (status, _, _) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams/{bare}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // PUT -> full replacement. DESIGN §3.1 "Alias Update Behavior": the alias
    // is immutable, so only a replacement whose pool still derives the same
    // alias is accepted.
    let replace = json!({
        "server": { "endpoints": [{ "host": "api.openai.com" }] },
        "tags": ["replaced"],
        "enabled": false
    });
    let (status, _, body) = call(
        router.clone(),
        "PUT",
        &format!("{BASE}/upstreams/{id}"),
        Some(replace),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["tags"], json!(["replaced"]));
    assert_eq!(body["enabled"], false);
    assert_eq!(body["id"], id);

    // A replacement that would change the derived alias is rejected.
    let moved = json!({
        "server": { "endpoints": [{ "host": "api.anthropic.com" }] }
    });
    let (status, _, body) = call(
        router.clone(),
        "PUT",
        &format!("{BASE}/upstreams/{id}"),
        Some(moved),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["invalid_value"], "api.openai.com");
    assert_eq!(body["alias"], "api.anthropic.com");

    // DELETE -> 204
    let (status, _, body) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/upstreams/{id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);

    // GET afterwards -> 404
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams/{id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
}

#[tokio::test]
async fn list_upstreams_supports_filter_select_orderby_top_skip() {
    let router = router();
    let tenant = Uuid::new_v4();
    create_upstream(&router, tenant, None).await;
    create_upstream_with(&router, tenant, None, &["a.example.org", "b.example.org"]).await;

    // $filter
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams?%24filter=alias%20eq%20%27example.org%27"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 1);
    assert_eq!(body["items"][0]["alias"], "example.org");

    // $select keeps only the requested members
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams?%24select=alias"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert!(items[0].get("id").is_none(), "select dropped `id`");
    assert!(items[0].get("alias").is_some());

    // $top + $skip
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams?%24top=1&%24skip=1&%24orderby=alias"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 1);
    assert_eq!(body["items"][0]["alias"], "example.org");
    assert_eq!(body["page_info"]["next_cursor"], Value::Null);

    // Unknown option -> 400
    let (status, _, _) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams?%24count=true"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upstream_alias_conflict_returns_409() {
    let router = router();
    let tenant = Uuid::new_v4();
    create_upstream_with(&router, tenant, None, &["a.example.org", "b.example.org"]).await;

    // A second pool under the same registrable domain derives the same alias,
    // so the insert must collide on `(tenant_id, alias)`.
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/upstreams"),
        Some(upstream_pool_payload(Some("Example.ORG"), &["a2.example.org", "b2.example.org"])),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["type"], "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1");
    assert_eq!(body["title"], "Alias Conflict");
    assert!(body["upstream_id"].is_string(), "extensions flattened");
}

#[tokio::test]
async fn upstream_ip_pool_requires_explicit_alias() {
    let router = router();
    let tenant = Uuid::new_v4();
    let payload = upstream_payload(None, "198.51.100.7");
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/upstreams"),
        Some(payload),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    // DESIGN §3.3 error table: an IP-based pool is a `ValidationError` (400).
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["title"], "Alias Validation Error");
    assert_eq!(body["valid_hosts"], json!(["198.51.100.7"]));
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn upstream_mismatched_explicit_alias_returns_400() {
    let router = router();
    let tenant = Uuid::new_v4();
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/upstreams"),
        Some(upstream_payload(Some("other.example.org"), "api.openai.com")),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    // DESIGN §3.3 error table: a user-supplied alias that differs from the
    // derived one is a `ValidationError` (400).
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["invalid_value"], "other.example.org");
    assert_eq!(body["alias"], "api.openai.com");
}

#[tokio::test]
async fn upstream_plaintext_requires_opt_in() {
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let config = OagwConfig::default();
    let router = register_routes(
        Router::new(),
        &openapi,
        ControlPlaneService::new(InMemoryStore::new(), &config).into(),
    );
    let tenant = Uuid::new_v4();
    let payload = json!({
        "server": { "endpoints": [{ "scheme": "http", "host": "api.openai.com" }] }
    });
    let (status, _, body) = call(
        router,
        "POST",
        &format!("{BASE}/upstreams"),
        Some(payload),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], 400);
}

// ---------------------------------------------------------------------------
// Routes (5 operations)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_round_trip() {
    let router = router();
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&router, tenant, None).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let bare = upstream_id.rsplit('~').next().expect("uuid").to_owned();

    // POST -> 201
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&bare)),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    assert_eq!(body["upstream_id"], upstream["id"]);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["priority"], 0);
    let route_id = body["id"].as_str().expect("id").to_owned();

    // GET list filtered by upstream_id
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/routes?%24filter=upstream_id%20eq%20%27{bare}%27"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 1);

    // PUT -> full replacement
    let replace = json!({
        "enabled": false,
        "priority": 7,
        "match_config": { "http": { "methods": ["POST"], "path": "/v1/embeddings" } }
    });
    let (status, _, body) = call(
        router.clone(),
        "PUT",
        &format!("{BASE}/routes/{route_id}"),
        Some(replace),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["priority"], 7);
    assert_eq!(body["match_config"]["http"]["path"], "/v1/embeddings");
    assert_eq!(body["upstream_id"], upstream["id"], "upstream_id immutable");

    // DELETE -> 204
    let (status, _, _) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/routes/{route_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn route_for_foreign_upstream_returns_404() {
    let router = router();
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();

    // An upstream owned by another tenant is not addressable.
    let _ = create_upstream(&router, other, None).await;
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&other.to_string())),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
}

#[tokio::test]
async fn route_match_conflict_returns_409() {
    let router = router();
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&router, tenant, None).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (status, _, _) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&upstream_id)),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&upstream_id)),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["type"], "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1");
}

#[tokio::test]
async fn route_unknown_upstream_returns_404() {
    let router = router();
    let tenant = Uuid::new_v4();
    let missing = Uuid::new_v4();
    let (status, _, body) = call(
        router,
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&missing.to_string())),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert!(body["detail"].as_str().is_some_and(|detail| !detail.is_empty()));
}

#[tokio::test]
async fn tenant_isolation_for_read_and_delete() {
    let router = router();
    let owner = Uuid::new_v4();
    let stranger = Uuid::new_v4();
    let upstream = create_upstream(&router, owner, None).await;
    let id = upstream["id"].as_str().expect("id").to_owned();

    let (status, _, _) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams/{id}"),
        None,
        stranger,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    let (status, _, _) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/upstreams/{id}"),
        None,
        stranger,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // The owner still sees the resource.
    let (status, _, _) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/upstreams/{id}"),
        None,
        owner,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Plugins (5 operations)
// ---------------------------------------------------------------------------

fn plugin_payload(name: &str) -> Value {
    json!({
        "type": "guard",
        "name": name,
        "source_code": "def guard_request(ctx):\n    return {\"decision\": \"allow\"}\n"
    })
}

#[tokio::test]
async fn plugin_crud_round_trip() {
    let router = router();
    let tenant = Uuid::new_v4();

    // POST -> 201
    let (status, _, body) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/plugins"),
        Some(plugin_payload("block-secrets")),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "body: {body}");
    assert_eq!(body["plugin_type"], "guard");
    assert_eq!(body["name"], "block-secrets");
    let plugin_id = body["id"].as_str().expect("id").to_owned();
    assert!(plugin_id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));

    // GET /plugins/{id}
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/plugins/{plugin_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["id"], plugin_id);

    // GET /plugins/{id}/source
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/plugins/{plugin_id}/source"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["id"], plugin_id);
    assert!(body["source_code"]
        .as_str()
        .is_some_and(|source| source.contains("guard_request")));

    // GET /plugins list
    let (status, _, body) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/plugins?%24filter=type%20eq%20%27guard%27"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 1);

    // DELETE -> 204
    let (status, _, _) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/plugins/{plugin_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_has_no_put_endpoint() {
    let router = router();
    let tenant = Uuid::new_v4();
    let (status, _, _) = call(
        router,
        "PUT",
        &format!("{BASE}/plugins/{}", Uuid::new_v4()),
        Some(plugin_payload("anything")),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn plugin_in_use_returns_409_with_references() {
    let router = router();
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&router, tenant, None).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (_, _, plugin) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/plugins"),
        Some(plugin_payload("block-secrets")),
        tenant,
    )
    .await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    // Bind the plugin to an upstream...
    let bind = json!({
        "server": { "endpoints": [{ "host": "api.openai.com" }] },
        "plugins": { "items": [plugin_id] }
    });
    let (status, _, _) = call(
        router.clone(),
        "PUT",
        &format!("{BASE}/upstreams/{upstream_id}"),
        Some(bind),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "binding failed");

    // ...and a route.
    let route = json!({
        "upstream_id": upstream_id,
        "match_config": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "plugins": { "items": [plugin_id] }
    });
    let (status, _, _) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let (status, _, body) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/plugins/{plugin_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["plugin_id"], plugin_id);
    assert_eq!(body["referenced_by"]["upstreams"], json!([upstream_id]));
    assert_eq!(body["referenced_by"]["routes"].as_array().expect("routes").len(), 1);
}

#[tokio::test]
async fn delete_upstream_cascades_its_routes() {
    let router = router();
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&router, tenant, None).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (_, _, route) = call(
        router.clone(),
        "POST",
        &format!("{BASE}/routes"),
        Some(route_payload(&upstream_id)),
        tenant,
    )
    .await;
    let route_id = route["id"].as_str().expect("id").to_owned();

    let (status, _, _) = call(
        router.clone(),
        "DELETE",
        &format!("{BASE}/upstreams/{upstream_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    let (status, _, _) = call(
        router.clone(),
        "GET",
        &format!("{BASE}/routes/{route_id}"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Error contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn errors_are_problem_json_with_gateway_source() {
    let router = router();
    let tenant = Uuid::new_v4();
    let missing = Uuid::new_v4();
    let (status, headers, body) = call(
        router,
        "GET",
        &format!("{BASE}/upstreams/{missing}"),
        None,
        tenant,
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        headers.get("x-oagw-error-source").and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    for member in ["type", "title", "status", "detail", "instance"] {
        assert!(body.get(member).is_some(), "missing `{member}` in {body}");
    }
    assert_eq!(body["status"], 404);
    assert_eq!(body["title"], "Upstream Not Found");
    assert_eq!(body["instance"], String::new());
}

#[tokio::test]
async fn validation_errors_carry_problem_json() {
    let router = router();
    let tenant = Uuid::new_v4();
    // Empty endpoint pool.
    let (status, headers, body) = call(
        router,
        "POST",
        &format!("{BASE}/upstreams"),
        Some(json!({ "server": { "endpoints": [] } })),
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(body["status"], 400);
    assert!(body["detail"].as_str().is_some_and(|detail| !detail.is_empty()));
}

#[tokio::test]
async fn unknown_resource_id_shape_returns_400() {
    let router = router();
    let tenant = Uuid::new_v4();
    let (status, _, body) = call(
        router,
        "GET",
        &format!("{BASE}/upstreams/not-a-uuid"),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
}

#[tokio::test]
async fn route_not_found_for_unknown_route_id() {
    let router = router();
    let tenant = Uuid::new_v4();
    let (status, _, body) = call(
        router,
        "GET",
        &format!("{BASE}/routes/{}", Uuid::new_v4()),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["title"], "Route Not Found");
}

#[tokio::test]
async fn plugin_not_found_for_unknown_plugin_id() {
    let router = router();
    let tenant = Uuid::new_v4();
    let (status, _, body) = call(
        router,
        "GET",
        &format!("{BASE}/plugins/{}", Uuid::new_v4()),
        None,
        tenant,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["title"], "Plugin Not Found");
}

/// Registration proof for all 15 control-plane operations (DESIGN §3.3).
///
/// No probe may answer 404 or 405: those are the only statuses axum produces
/// for an unregistered path or method. The `{id}` probes additionally use a
/// non-identifier, so a registered handler answers 400 from the body-less
/// `GET` requests, which do reach the handler.
#[tokio::test]
async fn all_fifteen_management_routes_are_registered() {
    const BOGUS: &str = "not-an-identifier";
    let router = router();
    let tenant = Uuid::new_v4();
    let probes: [(&str, &str, bool); 15] = [
        ("POST", "/upstreams", false),
        ("GET", "/upstreams", false),
        ("GET", "/upstreams/{id}", true),
        ("PUT", "/upstreams/{id}", true),
        ("DELETE", "/upstreams/{id}", true),
        ("POST", "/routes", false),
        ("GET", "/routes", false),
        ("GET", "/routes/{id}", true),
        ("PUT", "/routes/{id}", true),
        ("DELETE", "/routes/{id}", true),
        ("POST", "/plugins", false),
        ("GET", "/plugins", false),
        ("GET", "/plugins/{id}", true),
        ("GET", "/plugins/{id}/source", true),
        ("DELETE", "/plugins/{id}", true),
    ];
    for (method, template, has_id) in probes {
        let path = template.replace("{id}", BOGUS);
        let (status, _, _) = call(router.clone(), method, &format!("{BASE}{path}"), None, tenant).await;
        assert_ne!(
            status,
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            "{method} {template} is not registered"
        );
        assert_ne!(
            status,
            axum::http::StatusCode::NOT_FOUND,
            "{method} {template} is not registered"
        );
        // A body-less request with a bogus identifier reaches the handler, so
        // the identifier guard answers instead of the router.
        if has_id && method == "GET" {
            assert_eq!(
                status,
                axum::http::StatusCode::BAD_REQUEST,
                "{method} {template} did not reach its handler: {status}"
            );
        }
    }
}
