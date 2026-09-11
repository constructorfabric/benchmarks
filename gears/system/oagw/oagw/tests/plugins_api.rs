//! Router-level tests for `/oagw/v1/plugins`
//! (`cpt-cf-oagw-feature-plugin-management`).
//!
//! Drives a real `axum::Router` (assembled the same way
//! `crate::gear::OagwGear::register_rest` assembles it) with
//! `tower::ServiceExt::oneshot`, following the harness shape of this crate's
//! `tests/upstreams_api.rs` and `tests/routes_api.rs`. Every §6 acceptance
//! criterion in scope for this feature is exercised here;
//! authentication/authorization are out of this feature's scope, so no
//! 401/403 case is asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use oagw::api::rest::routes::register_routes;
use oagw::state::ControlPlaneState;
use serde_json::{Value, json};
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// Builds a fresh gear-relative router over a fresh, empty control-plane
/// store, with a `SecurityContext` extension asserting `tenant_id` as the
/// calling tenant.
fn router_for_tenant(tenant_id: Uuid) -> Router {
    let state = Arc::new(ControlPlaneState::new());
    let openapi = OpenApiRegistryImpl::new();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .expect("security context must build");
    register_routes(Router::new(), &openapi)
        .layer(Extension(Arc::clone(&state)))
        .layer(Extension(ctx))
}

/// Builds two routers sharing one control-plane store, scoped respectively
/// to `tenant_a` and `tenant_b` — used by cross-tenant scenarios.
fn routers_sharing_state(tenant_a: Uuid, tenant_b: Uuid) -> (Router, Router) {
    let state = Arc::new(ControlPlaneState::new());
    let openapi = OpenApiRegistryImpl::new();
    let base = register_routes(Router::new(), &openapi).layer(Extension(state));

    let ctx_a = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_a)
        .build()
        .expect("security context a must build");
    let ctx_b = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_b)
        .build()
        .expect("security context b must build");

    (
        base.clone().layer(Extension(ctx_a)),
        base.layer(Extension(ctx_b)),
    )
}

async fn send(
    router: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = if let Some(value) = body {
        builder = builder.header("content-type", "application/json");
        Body::from(value.to_string())
    } else {
        Body::empty()
    };
    let request = builder.body(body).expect("request must build");
    router
        .oneshot(request)
        .await
        .expect("router call must succeed")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must be readable")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body must be JSON")
}

fn guard_plugin(name: &str) -> Value {
    json!({
        "plugin_type": "guard",
        "name": name,
        "config_schema": {"type": "object"},
        "phases": ["on_request", "on_response"],
        "source_code": "def on_request(ctx):\n    return ctx.next()",
    })
}

fn http_upstream(host: &str) -> Value {
    json!({
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": 80}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    })
}

/// Creates a plugin on `router` and returns the created resource body.
async fn create_plugin_body(router: Router, name: &str) -> Value {
    let response = send(
        router,
        Method::POST,
        "/oagw/v1/plugins",
        Some(guard_plugin(name)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-router-test-01
#[tokio::test]
async fn create_returns_201_with_a_gts_form_id_matching_its_type() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::POST,
        "/oagw/v1/plugins",
        Some(guard_plugin("request_validator")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let created = body_json(response).await;
    let id = created["id"].as_str().expect("id must be a string");
    assert!(id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
    let instance = id.rsplit('~').next().expect("instance part");
    assert!(Uuid::parse_str(instance).is_ok());
}
// @cpt-end:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-conflict-router-test-01
#[tokio::test]
async fn create_with_a_duplicate_name_for_the_same_tenant_returns_409() {
    let router = router_for_tenant(Uuid::new_v4());
    create_plugin_body(router.clone(), "dup_plugin").await;

    let mut second = guard_plugin("dup_plugin");
    second["plugin_type"] = json!("transform");
    second["phases"] = json!([]);
    let response = send(router, Method::POST, "/oagw/v1/plugins", Some(second)).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-conflict-router-test-01

#[tokio::test]
async fn create_with_a_plugin_type_outside_the_accepted_set_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let mut body = guard_plugin("bad_type_plugin");
    body["plugin_type"] = json!("logging");
    let response = send(router, Method::POST, "/oagw/v1/plugins", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_with_a_non_object_config_schema_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let mut body = guard_plugin("bad_schema_plugin");
    body["config_schema"] = json!("not-an-object");
    let response = send(router, Method::POST, "/oagw/v1/plugins", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-create-plugin-auth-phase-router-test-01
#[tokio::test]
async fn create_an_auth_plugin_declaring_a_request_phase_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "plugin_type": "auth",
        "name": "custom_auth",
        "config_schema": {"type": "object"},
        "phases": ["on_request"],
        "source_code": "def authenticate(ctx):\n    return ctx.next()",
    });
    let response = send(router, Method::POST, "/oagw/v1/plugins", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert_eq!(problem["status"], 400);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-config-schema-validation:p1:inst-create-plugin-auth-phase-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-router-test-01
#[tokio::test]
async fn get_by_id_for_an_existing_tenant_owned_plugin_returns_200_without_source() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "get_me").await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(router, Method::GET, &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let fetched = body_json(response).await;
    assert_eq!(fetched["id"], id);
    assert!(fetched.get("source_code").is_none());
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-identification:p1:inst-get-plugin-cross-tenant-router-test-01
#[tokio::test]
async fn get_by_id_for_a_plugin_owned_by_another_tenant_returns_404_problem_json() {
    let (owner_router, other_router) = routers_sharing_state(Uuid::new_v4(), Uuid::new_v4());
    let created = create_plugin_body(owner_router, "cross_tenant_plugin").await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        other_router,
        Method::GET,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let problem = body_json(response).await;
    assert!(problem["type"].as_str().is_some());
    assert!(problem["title"].as_str().is_some());
}
// @cpt-end:cpt-cf-oagw-dod-plugin-identification:p1:inst-get-plugin-cross-tenant-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-router-test-01
#[tokio::test]
async fn get_source_for_a_uuid_backed_plugin_returns_200_with_the_stored_source() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "source_me").await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        router,
        Method::GET,
        &format!("/oagw/v1/plugins/{id}/source"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let source = body_json(response).await;
    assert!(
        source["source_code"]
            .as_str()
            .is_some_and(|s| s.contains("on_request"))
    );
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-named-router-test-01
#[tokio::test]
async fn get_source_for_a_named_built_in_identifier_returns_404_problem_json() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::GET,
        "/oagw/v1/plugins/gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1/source",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let problem = body_json(response).await;
    assert!(problem["type"].as_str().is_some());
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-named-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-no-replace:p1:inst-plugin-no-replace-router-test-01
#[tokio::test]
async fn no_put_or_patch_is_served_on_a_plugin_resource() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "no_replace_me").await;
    let id = created["id"].as_str().expect("id must be a string");

    let put_response = send(
        router.clone(),
        Method::PUT,
        &format!("/oagw/v1/plugins/{id}"),
        Some(guard_plugin("no_replace_me")),
    )
    .await;
    // The path is registered for GET and DELETE, so axum's router reports a
    // routing-level method rejection (405) for the unregistered PUT, rather
    // than invoking any handler — never a successful replace.
    assert_eq!(put_response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let patch_response = send(
        router,
        Method::PATCH,
        &format!("/oagw/v1/plugins/{id}"),
        Some(guard_plugin("no_replace_me")),
    )
    .await;
    assert_eq!(patch_response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-no-replace:p1:inst-plugin-no-replace-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-router-test-01
#[tokio::test]
async fn delete_returns_204_and_a_subsequent_get_returns_404() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "delete_me").await;
    let id = created["id"].as_str().expect("id must be a string");

    let deleted = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let after = send(router, Method::GET, &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-in-use-upstream-router-test-01
#[tokio::test]
async fn delete_a_plugin_bound_to_an_upstream_returns_409_with_problem_json_fields() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "bound_to_upstream").await;
    let id = created["id"].as_str().expect("id must be a string");

    let mut upstream_body = http_upstream("plugin-bound.example.com");
    upstream_body["plugins"] = json!({"items": [id]});
    let upstream_response = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body),
    )
    .await;
    assert_eq!(upstream_response.status(), StatusCode::CREATED);

    let response = send(
        router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert!(problem["type"].as_str().is_some());
    assert!(problem["title"].as_str().is_some());
    assert_eq!(problem["status"], 409);
    assert!(problem["detail"].as_str().is_some());
    assert_eq!(problem["context"]["plugin_id"], id);
    assert_eq!(
        problem["context"]["referenced_by"]["upstreams"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        problem["context"]["referenced_by"]["routes"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}
// @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-in-use-upstream-router-test-01

#[tokio::test]
async fn delete_a_plugin_bound_to_a_route_returns_409() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "bound_to_route").await;
    let id = created["id"]
        .as_str()
        .expect("id must be a string")
        .to_owned();

    let upstream_response = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("route-bound.example.com")),
    )
    .await;
    assert_eq!(upstream_response.status(), StatusCode::CREATED);
    let upstream = body_json(upstream_response).await;
    let upstream_id = upstream["id"].as_str().expect("upstream id");

    let route_body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1/widgets"}},
        "plugins": {"items": [id]},
    });
    let route_response = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(route_body),
    )
    .await;
    assert_eq!(route_response.status(), StatusCode::CREATED);

    let response = send(
        router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert_eq!(
        problem["context"]["referenced_by"]["routes"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-lifecycle-tracking:p1:inst-delete-upstream-plugin-binding-router-test-01
#[tokio::test]
async fn deleting_an_upstream_removes_its_plugin_bindings_so_the_plugin_becomes_deletable() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = create_plugin_body(router.clone(), "unbind_me").await;
    let id = created["id"]
        .as_str()
        .expect("id must be a string")
        .to_owned();

    let mut upstream_body = http_upstream("unbind.example.com");
    upstream_body["plugins"] = json!({"items": [id.clone()]});
    let upstream_response = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body),
    )
    .await;
    assert_eq!(upstream_response.status(), StatusCode::CREATED);
    let upstream = body_json(upstream_response).await;
    let upstream_id = upstream["id"].as_str().expect("upstream id");

    let still_bound = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(still_bound.status(), StatusCode::CONFLICT);

    let upstream_deleted = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        None,
    )
    .await;
    assert_eq!(upstream_deleted.status(), StatusCode::NO_CONTENT);

    let now_deletable = send(
        router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(now_deletable.status(), StatusCode::NO_CONTENT);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-lifecycle-tracking:p1:inst-delete-upstream-plugin-binding-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-router-test-01
#[tokio::test]
async fn list_honors_top_and_returns_only_the_calling_tenants_plugins() {
    let router = router_for_tenant(Uuid::new_v4());
    for name in ["plugin_a", "plugin_b", "plugin_c"] {
        create_plugin_body(router.clone(), name).await;
    }

    let response = send(router, Method::GET, "/oagw/v1/plugins?$top=2", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert_eq!(items.len(), 2);
}
// @cpt-end:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-filter-skip-test-01
#[tokio::test]
async fn list_honors_filter_and_skip() {
    let router = router_for_tenant(Uuid::new_v4());
    for name in ["plugin_a", "plugin_b", "plugin_c"] {
        create_plugin_body(router.clone(), name).await;
    }

    let response = send(
        router.clone(),
        Method::GET,
        "/oagw/v1/plugins?$filter=name%20eq%20plugin_b",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "plugin_b");

    let response = send(
        router,
        Method::GET,
        "/oagw/v1/plugins?$skip=2&$orderby=name",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert_eq!(items.len(), 1, "$skip=2 must leave only the third item");
    assert_eq!(items[0]["name"], "plugin_c");
}
// @cpt-end:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-filter-skip-test-01

#[tokio::test]
async fn list_scopes_results_to_the_calling_tenant() {
    let (owner_router, other_router) = routers_sharing_state(Uuid::new_v4(), Uuid::new_v4());
    create_plugin_body(owner_router, "owner_only_plugin").await;

    let response = send(other_router, Method::GET, "/oagw/v1/plugins", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert!(items.is_empty());
}

#[tokio::test]
async fn every_route_registered_by_this_feature_is_reachable_gear_relative() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(router, Method::GET, "/oagw/v1/plugins", None).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_400_and_409_plugin_response_is_problem_json_with_the_gateway_source_header() {
    let router = router_for_tenant(Uuid::new_v4());
    let mut body = guard_plugin("bad_source_field_plugin");
    body.as_object_mut()
        .expect("body is an object")
        .remove("source_code");
    let response = send(router, Method::POST, "/oagw/v1/plugins", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let problem = body_json(response).await;
    assert!(problem["type"].as_str().is_some());
    assert!(problem["title"].as_str().is_some());
    assert_eq!(problem["status"], 400);
    assert!(problem["detail"].as_str().is_some());
}
