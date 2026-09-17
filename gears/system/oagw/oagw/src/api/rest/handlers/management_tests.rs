//! Handler tests for the management API, driven through a router with the
//! security context and shared state injected as extensions.

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use credstore_sdk::test_util::MockCredStoreClient;

use crate::api::rest::routes::register_routes;
use crate::api::rest::state::OagwState;
use crate::config::OagwConfig;
use crate::domain::repo::{PluginRepository, RouteRepository};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::credentials::SecretResolver;
use crate::infra::storage::memory::MemoryStore;

fn resolver() -> SecretResolver {
    SecretResolver::new(Arc::new(MockCredStoreClient::empty()))
}

fn router() -> Router {
    let store = MemoryStore::new();
    let control_plane = Arc::new(ControlPlaneService::new(
        store.clone(),
        store.clone() as Arc<dyn RouteRepository>,
        store as Arc<dyn PluginRepository>,
    ));
    let state = Arc::new(OagwState::assemble(
        control_plane,
        &resolver(),
        Arc::new(OagwConfig::default()),
    ));
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, state);
    router.layer(Extension(security_context()))
}

fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(TENANT_A)
        .build()
        .expect("context")
}

/// Deterministic tenant used by every request.
static TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_000a);

async fn request(
    router: Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = router.oneshot(request).await.expect("response");
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .map(|value| value.to_str().expect("ascii").to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("json body: {error}; status={status:?}"))
    };
    (status, parsed, location)
}

fn upstream_body(host: &str, port: u16) -> Value {
    json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": host, "port": port }]
        },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

#[tokio::test]
async fn create_upstream_derives_the_alias_and_returns_201() {
    let router = router();
    let (status, body, location) = request(
        router,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["protocol"], HTTP_PROTOCOL);
    let id = body["id"].as_str().expect("id").to_owned();
    assert_eq!(
        location.as_deref(),
        Some(format!("/oagw/v1/upstreams/{id}").as_str())
    );
    assert!(id.starts_with("gts.cf.core.oagw.upstream.v1~"));
}

#[tokio::test]
async fn duplicate_alias_returns_409_with_the_contract_type() {
    let router = router();
    let _ = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let (status, body, _) = request(
        router,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.resource.conflict.v1"
    );
}

#[tokio::test]
async fn get_upstream_accepts_gts_and_bare_ids() {
    let router = router();
    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let gts_id = created["id"].as_str().expect("gts id").to_owned();
    let uuid = gts_id.rsplit('~').next().expect("uuid").to_owned();

    let (status, by_gts, _) = request(
        router.clone(),
        "GET",
        &format!("/oagw/v1/upstreams/{gts_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_gts["id"], gts_id);

    let (status, by_uuid, _) = request(
        router.clone(),
        "GET",
        &format!("/oagw/v1/upstreams/{uuid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_uuid["id"], gts_id);

    let (status, body, _) = request(
        router,
        "GET",
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1"
    );
}

#[tokio::test]
async fn replace_upstream_keeps_the_alias() {
    let router = router();
    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (status, replaced, _) = request(
        router,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replaced["alias"], "api.openai.com");
}

#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let router = router();
    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();
    let (_, route, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
        })),
    )
    .await;
    let route_id = route["id"].as_str().expect("id").to_owned();

    let (status, _, _) = request(
        router.clone(),
        "DELETE",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = request(router, "GET", &format!("/oagw/v1/routes/{route_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_crud_roundtrip() {
    let router = router();
    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let (status, route, location) = request(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let route_id = route["id"].as_str().expect("id").to_owned();
    assert_eq!(
        location.as_deref(),
        Some(format!("/oagw/v1/routes/{route_id}").as_str())
    );
    assert_eq!(route["upstream_id"], upstream_id);

    let (status, _, _) = request(
        router.clone(),
        "DELETE",
        &format!("/oagw/v1/routes/{}", route["id"].as_str().expect("id")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_in_use_returns_409() {
    let router = router();
    let (_, plugin, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/plugins",
        Some(json!({
            "name": "my-transform",
            "type": "transform",
            "source_code": "def apply(ctx):\n    return ctx\n"
        })),
    )
    .await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();
    assert!(plugin_id.starts_with("gts.cf.core.oagw.transform_plugin.v1~"));

    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();
    let (status, _, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Bind the plugin on the upstream so the delete has to be refused.
    let (status, _, _) = request(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        Some(json!({
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
            },
            "protocol": HTTP_PROTOCOL,
            "plugins": { "items": [{ "plugin_ref": plugin_id }] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body, _) = request(
        router,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
}

#[tokio::test]
async fn odata_top_and_filter_are_honoured() {
    let router = router();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        let _ = request(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(host, 443)),
        )
        .await;
    }

    let (status, page, _) = request(
        router.clone(),
        "GET",
        "/oagw/v1/upstreams?$top=2&$skip=1&$orderby=alias%20desc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["alias"], "b.example.com");
    assert_eq!(items[1]["alias"], "a.example.com");

    let (status, filtered, _) = request(
        router,
        "GET",
        "/oagw/v1/upstreams?$filter=alias%20eq%20%27a.example.com%27",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = filtered.as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["alias"], "a.example.com");
}

#[tokio::test]
async fn unknown_odata_option_is_rejected() {
    let router = router();
    let (status, body, _) = request(
        router,
        "GET",
        "/oagw/v1/upstreams?$filtre=alias%20eq%20%27x%27",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn an_ancestor_tenant_cannot_see_descendant_resources() {
    let router = router();
    let (_, created, _) = request(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body("api.openai.com", 443)),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // A different tenant's request hits the same store but never sees the row.
    let other = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("context");
    let openapi = OpenApiRegistryImpl::new();
    let store = MemoryStore::new();
    let _ = store;
    let foreign_router = register_routes(
        Router::new(),
        &openapi,
        Arc::new(OagwState::assemble(
            foreign_control_plane(),
            &resolver(),
            Arc::new(OagwConfig::default()),
        )),
    )
    .layer(Extension(other));

    let (status, _, _) = request(
        foreign_router,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn foreign_control_plane() -> Arc<ControlPlaneService> {
    let store = MemoryStore::new();
    Arc::new(ControlPlaneService::new(
        store.clone(),
        store.clone() as Arc<dyn RouteRepository>,
        store as Arc<dyn PluginRepository>,
    ))
}
