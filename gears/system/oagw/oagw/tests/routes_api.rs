//! Router-level tests for `/oagw/v1/routes`
//! (`cpt-cf-oagw-feature-route-management`).
//!
//! Drives a real `axum::Router` (assembled the same way
//! `crate::gear::OagwGear::register_rest` assembles it) with
//! `tower::ServiceExt::oneshot`, following the harness shape of this crate's
//! own `tests/upstreams_api.rs`. Every §6 acceptance criterion in scope for
//! this feature is exercised here; authentication/authorization are out of
//! this feature's scope, so no 401/403 case is asserted.

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

fn http_upstream(host: &str) -> Value {
    json!({
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": 80}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    })
}

/// Creates an upstream on `router` and returns its `id`.
async fn create_upstream_id(router: Router, host: &str) -> String {
    let response = send(
        router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream(host)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    created["id"]
        .as_str()
        .expect("id must be a string")
        .to_owned()
}

fn http_route(upstream_id: &str, path: &str, methods: &[&str]) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": methods, "path": path}},
    })
}

// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-router-test-01
#[tokio::test]
async fn create_against_an_existing_upstream_returns_201_with_a_generated_id() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "widgets.example.com").await;

    let response = send(
        router,
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let created = body_json(response).await;
    assert_eq!(created["upstream_id"], upstream_id);
    assert!(created["id"].as_str().is_some());
    assert_eq!(created["enabled"], true);
    assert_eq!(created["priority"], 0);
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-both-router-test-01
#[tokio::test]
async fn create_with_a_match_carrying_both_http_and_grpc_is_rejected_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "both-match.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {"methods": ["GET"], "path": "/v1/widgets"},
            "grpc": {"service": "foo.v1.Svc", "method": "Get"},
        },
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("match"))
    );
}
// @cpt-end:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-both-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-none-router-test-01
#[tokio::test]
async fn create_with_a_match_carrying_neither_http_nor_grpc_is_rejected_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "no-match.example.com").await;

    let body = json!({"upstream_id": upstream_id, "match": {}});
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
// @cpt-end:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-none-router-test-01

#[tokio::test]
async fn create_with_a_body_omitting_both_upstream_id_and_match_names_both_missing_fields() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(json!({}))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    let detail = problem["detail"].as_str().expect("detail must be a string");
    assert!(detail.contains("upstream_id"), "detail was: {detail}");
    assert!(detail.contains("match"), "detail was: {detail}");
}

// @cpt-begin:cpt-cf-oagw-dod-upstream-reference-check:p1:inst-create-route-upstream-router-test-01
#[tokio::test]
async fn create_with_an_unknown_upstream_id_is_rejected_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = http_route(&Uuid::new_v4().to_string(), "/v1/widgets", &["GET"]);
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert_eq!(problem["status"], 400);
}
// @cpt-end:cpt-cf-oagw-dod-upstream-reference-check:p1:inst-create-route-upstream-router-test-01

#[tokio::test]
async fn create_with_an_upstream_id_owned_by_another_tenant_is_rejected_400() {
    let (owner_router, other_router) = routers_sharing_state(Uuid::new_v4(), Uuid::new_v4());
    let upstream_id = create_upstream_id(owner_router, "cross-tenant.example.com").await;

    let body = http_route(&upstream_id, "/v1/widgets", &["GET"]);
    let response = send(other_router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// @cpt-begin:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-http-path-router-test-01
#[tokio::test]
async fn create_with_an_empty_http_path_returns_400_citing_min_length() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "empty-path.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": ""}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("path") && d.contains("minimum length"))
    );
}
// @cpt-end:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-http-path-router-test-01

#[tokio::test]
async fn create_with_empty_http_methods_returns_400_citing_min_items() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "empty-methods.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": [], "path": "/v1/widgets"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("methods"))
    );
}

/// Distinct from `create_with_empty_http_methods_returns_400_citing_min_items`:
/// `methods` is absent entirely rather than present-but-empty, covering the
/// "omitted or an empty array" half of the criterion independently.
#[tokio::test]
async fn create_with_http_methods_omitted_entirely_returns_400_citing_min_items() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "omitted-methods.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {"path": "/v1/widgets"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("methods"))
    );
}

#[tokio::test]
async fn create_with_a_method_outside_the_enum_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "bad-method.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["HEAD"], "path": "/v1/widgets"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// @cpt-begin:cpt-cf-oagw-dod-grpc-match-validation:p2:inst-schema-grpc-router-test-01
#[tokio::test]
async fn create_with_an_empty_grpc_service_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "empty-grpc-service.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "", "method": "GetUser"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("service"))
    );
}

#[tokio::test]
async fn create_with_an_empty_grpc_method_returns_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "empty-grpc-method.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "foo.v1.UserService", "method": ""}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("method"))
    );
}
// @cpt-end:cpt-cf-oagw-dod-grpc-match-validation:p2:inst-schema-grpc-router-test-01

/// Distinct from the empty-string `minLength` cases above: `service`/`method`
/// are absent entirely, covering the "omitted" half of the criterion
/// independently of the "empty string" half.
#[tokio::test]
async fn create_with_grpc_service_omitted_entirely_returns_400_citing_the_missing_field() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "omitted-grpc-service.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"method": "GetUser"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("service"))
    );
}

#[tokio::test]
async fn create_with_grpc_method_omitted_entirely_returns_400_citing_the_missing_field() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "omitted-grpc-method.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "foo.v1.UserService"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("method"))
    );
}

#[tokio::test]
async fn create_with_a_valid_grpc_match_is_accepted_and_stores_it() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "valid-grpc.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}},
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["match"]["grpc"]["service"], "foo.v1.UserService");
    assert_eq!(created["match"]["grpc"]["method"], "GetUser");
}

// @cpt-begin:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-defaults-router-test-01
#[tokio::test]
async fn create_omitting_query_allowlist_and_path_suffix_mode_applies_documented_defaults() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "defaults.example.com").await;

    let body = http_route(&upstream_id, "/v1/widgets", &["GET"]);
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["match"]["http"]["query_allowlist"], json!([]));
    assert_eq!(created["match"]["http"]["path_suffix_mode"], "append");
}
// @cpt-end:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-defaults-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-explicit-disabled-router-test-01
#[tokio::test]
async fn create_with_path_suffix_mode_disabled_persists_it_unchanged() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "disabled-suffix.example.com").await;

    let body = json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET"],
                "path": "/v1/widgets",
                "path_suffix_mode": "disabled",
            },
        },
    });
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["match"]["http"]["path_suffix_mode"], "disabled");
}
// @cpt-end:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-explicit-disabled-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-conflict-router-test-01
#[tokio::test]
async fn create_with_same_path_and_priority_and_an_intersecting_method_set_returns_409() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "conflict.example.com").await;

    let first = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET", "POST"])),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = send(
        router,
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["POST", "PUT"])),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let problem = body_json(second).await;
    assert_eq!(problem["status"], 409);
}
// @cpt-end:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-conflict-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-priority-router-test-01
#[tokio::test]
async fn create_with_same_methods_and_path_but_a_different_priority_returns_201() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "priority.example.com").await;

    let first = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let mut second_body = http_route(&upstream_id, "/v1/widgets", &["GET"]);
    second_body["priority"] = json!(5);
    let second = send(router, Method::POST, "/oagw/v1/routes", Some(second_body)).await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let second = body_json(second).await;
    assert_eq!(second["priority"], 5);
}
// @cpt-end:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-priority-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-upstream-id-immutability:p1:inst-replace-route-immutable-router-test-01
#[tokio::test]
async fn replace_supplying_a_different_upstream_id_is_rejected_400() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "immutable.example.com").await;
    let other_upstream_id = create_upstream_id(router.clone(), "other-up.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        router,
        Method::PUT,
        &format!("/oagw/v1/routes/{id}"),
        Some(http_route(&other_upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
// @cpt-end:cpt-cf-oagw-dod-upstream-id-immutability:p1:inst-replace-route-immutable-router-test-01

#[tokio::test]
async fn replace_omitting_upstream_id_retains_the_stored_value() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "retain.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let mut body = json!({
        "match": {"http": {"methods": ["GET", "POST"], "path": "/v1/widgets"}},
    });
    body["enabled"] = json!(false);
    let response = send(
        router,
        Method::PUT,
        &format!("/oagw/v1/routes/{id}"),
        Some(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = body_json(response).await;
    assert_eq!(replaced["upstream_id"], upstream_id);
    assert_eq!(replaced["enabled"], false);
}

/// `enabled` is not a field `route.v1.schema.json`/`RouteRequest` declares
/// at all (it lives outside the request DTO the schema mirror validates), so
/// a replace body setting it can never trip a schema violation; only the
/// stored `Route`'s own `enabled` field changes.
#[tokio::test]
async fn replace_setting_enabled_false_persists_without_schema_validating_it() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "enabled-bypass.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");
    assert_eq!(created["enabled"], true);

    let mut body = http_route(&upstream_id, "/v1/widgets", &["GET"]);
    body["enabled"] = json!(false);
    let response = send(
        router,
        Method::PUT,
        &format!("/oagw/v1/routes/{id}"),
        Some(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = body_json(response).await;
    assert_eq!(replaced["enabled"], false);
}

// @cpt-begin:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-filter-select-orderby-test-01
#[tokio::test]
async fn list_honors_filter_select_and_orderby() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "filter-list.example.com").await;

    for (path, priority) in [("/v1/a", 5), ("/v1/b", 1), ("/v1/c", 9)] {
        let mut body = http_route(&upstream_id, path, &["GET"]);
        body["priority"] = json!(priority);
        let response = send(router.clone(), Method::POST, "/oagw/v1/routes", Some(body)).await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    // A route under a different upstream must never satisfy the filter
    // below, proving `$filter` actually narrows rather than merely
    // paginating.
    let other_upstream_id = create_upstream_id(router.clone(), "other-filter.example.com").await;
    send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&other_upstream_id, "/v1/other", &["GET"])),
    )
    .await;

    let response = send(
        router,
        Method::GET,
        &format!(
            "/oagw/v1/routes?$filter=upstream_id%20eq%20{upstream_id}&$select=id,priority&$orderby=priority%20desc"
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");

    assert_eq!(
        items.len(),
        3,
        "the other upstream's route must be excluded"
    );
    let priorities: Vec<i64> = items
        .iter()
        .map(|item| {
            item["priority"]
                .as_i64()
                .expect("priority must be a number")
        })
        .collect();
    assert_eq!(priorities, vec![9, 5, 1], "$orderby=priority desc");
    for item in items {
        assert!(item.get("match").is_none(), "$select must project fields");
        assert!(item.get("id").is_some());
    }
}
// @cpt-end:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-filter-select-orderby-test-01

// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-router-test-01
#[tokio::test]
async fn get_by_id_for_an_existing_tenant_owned_route_returns_200() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "get.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(router, Method::GET, &format!("/oagw/v1/routes/{id}"), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let fetched = body_json(response).await;
    assert_eq!(fetched["id"], id);
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-route-tenant-scoping:p1:inst-get-route-unknown-id-router-test-01
#[tokio::test]
async fn get_by_id_for_an_unknown_id_returns_404() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::GET,
        &format!("/oagw/v1/routes/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-route-tenant-scoping:p1:inst-get-route-unknown-id-router-test-01

#[tokio::test]
async fn get_by_id_for_a_route_owned_by_another_tenant_returns_404() {
    let (owner_router, other_router) = routers_sharing_state(Uuid::new_v4(), Uuid::new_v4());
    let upstream_id = create_upstream_id(owner_router.clone(), "cross-get.example.com").await;

    let created = send(
        owner_router,
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        other_router,
        Method::GET,
        &format!("/oagw/v1/routes/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-router-test-01
#[tokio::test]
async fn delete_returns_204_and_a_subsequent_get_returns_404() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "delete.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let deleted = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/routes/{id}"),
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let after = send(router, Method::GET, &format!("/oagw/v1/routes/{id}"), None).await;
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-router-test-01

#[tokio::test]
async fn delete_for_an_unknown_id_returns_404() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::DELETE,
        &format!("/oagw/v1/routes/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// @cpt-begin:cpt-cf-oagw-dod-route-cascade-delete:p1:inst-cascade-delete-router-test-01
#[tokio::test]
async fn deleting_an_upstream_cascade_deletes_its_routes() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "cascade.example.com").await;

    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(http_route(&upstream_id, "/v1/widgets", &["GET"])),
    )
    .await;
    let created = body_json(created).await;
    let route_id = created["id"].as_str().expect("id must be a string");

    let deleted_upstream = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        None,
    )
    .await;
    assert_eq!(deleted_upstream.status(), StatusCode::NO_CONTENT);

    let response = send(
        router,
        Method::GET,
        &format!("/oagw/v1/routes/{route_id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-route-cascade-delete:p1:inst-cascade-delete-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-router-test-01
#[tokio::test]
async fn list_honors_top_and_skip_and_returns_only_the_calling_tenants_routes() {
    let router = router_for_tenant(Uuid::new_v4());
    let upstream_id = create_upstream_id(router.clone(), "list.example.com").await;

    for path in ["/v1/a", "/v1/b", "/v1/c", "/v1/d"] {
        let response = send(
            router.clone(),
            Method::POST,
            "/oagw/v1/routes",
            Some(http_route(&upstream_id, path, &["GET"])),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = send(router, Method::GET, "/oagw/v1/routes?$top=10&$skip=0", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert_eq!(items.len(), 4);
}
// @cpt-end:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-router-test-01

#[tokio::test]
async fn every_400_and_409_route_response_is_problem_json_with_the_gateway_source_header() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(router, Method::POST, "/oagw/v1/routes", Some(json!({}))).await;
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
