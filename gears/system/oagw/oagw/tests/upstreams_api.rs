//! Router-level tests for `/oagw/v1/upstreams`
//! (`cpt-cf-oagw-feature-upstream-management`).
//!
//! Drives a real `axum::Router` (assembled the same way
//! `crate::gear::OagwGear::register_rest` assembles it) with
//! `tower::ServiceExt::oneshot`, following the harness shape of
//! `api-gateway`'s `tests/health_endpoints.rs` and this crate's own
//! `tests/router_mount.rs`. Every §6 acceptance criterion in scope for this
//! feature is exercised here; authentication/authorization are out of this
//! feature's scope, so no 401/403 case is asserted.

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

const ROOT_TENANT_ID: Uuid = Uuid::nil();

/// Builds a fresh gear-relative router over a fresh, empty control-plane
/// store, with a `SecurityContext` extension asserting `tenant_id` as the
/// calling tenant — the same extractor pattern the handlers read
/// (`SecurityContext::subject_tenant_id()`).
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
/// to `tenant_a` and `tenant_b` — used by cross-tenant and
/// ancestor-disable scenarios.
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

// @cpt-begin:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-router-test-01
#[tokio::test]
async fn create_with_http_scheme_is_accepted_and_alias_derives_from_the_hostname() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "http", "host": "internal.example.com", "port": 80}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let created = body_json(response).await;
    assert_eq!(created["alias"], "internal.example.com");
    assert!(created["id"].as_str().is_some());
}
// @cpt-end:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-schema-validation:p1:inst-create-upstream-ws-scheme-test-01
#[tokio::test]
async fn create_with_ws_scheme_is_accepted() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "ws", "host": "streams.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}
// @cpt-end:cpt-cf-oagw-dod-schema-validation:p1:inst-create-upstream-ws-scheme-test-01

// @cpt-begin:cpt-cf-oagw-dod-schema-validation:p1:inst-create-upstream-default-port-test-01
#[tokio::test]
async fn default_ports_apply_per_scheme() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "http", "host": "plain.example.com"},
            {"scheme": "https", "host": "plain.example.com"},
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "plain.example.com",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["server"]["endpoints"][0]["port"], 80);
    assert_eq!(created["server"]["endpoints"][1]["port"], 443);
}
// @cpt-end:cpt-cf-oagw-dod-schema-validation:p1:inst-create-upstream-default-port-test-01

#[tokio::test]
async fn create_rejects_a_scheme_outside_the_accepted_set_with_400_status_field() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "ftp", "host": "api.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert_eq!(problem["status"], 400);
}

// @cpt-begin:cpt-cf-oagw-dod-error-responses:p1:inst-create-upstream-missing-server-test-01
#[tokio::test]
async fn create_omitting_server_names_the_field_as_missing() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({"protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"});

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("server"))
    );
}
// @cpt-end:cpt-cf-oagw-dod-error-responses:p1:inst-create-upstream-missing-server-test-01

#[tokio::test]
async fn create_omitting_protocol_names_the_field_as_missing() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("protocol"))
    );
}

// CODE1-F-003: `upstream.v1.schema.json`'s `auth` object declares no
// `additionalProperties: false` (unlike every sibling object, which does),
// so an extra property on it is schema-conformant and must be accepted.
#[tokio::test]
async fn create_accepts_an_auth_object_with_an_extra_property_beyond_the_schema() {
    let router = router_for_tenant(Uuid::new_v4());
    let mut body = http_upstream("auth-extra-field.example.com");
    body["auth"] = json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        "extra_field": "not declared by the schema",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_rejects_an_unknown_top_level_property() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "unexpected_field": true,
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// @cpt-begin:cpt-cf-oagw-dod-alias-derivation-normalization:p1:inst-create-upstream-common-suffix-test-01
#[tokio::test]
async fn create_derives_the_common_registrable_suffix_across_two_hostnames() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "us.vendor.com", "port": 443},
            {"scheme": "https", "host": "eu.vendor.com", "port": 443},
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await;
    assert_eq!(created["alias"], "vendor.com");
}
// @cpt-end:cpt-cf-oagw-dod-alias-derivation-normalization:p1:inst-create-upstream-common-suffix-test-01

#[tokio::test]
async fn create_rejects_a_diverging_user_supplied_alias() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "something-else.example.com",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_with_ip_endpoints_and_no_alias_requires_an_explicit_alias() {
    let router = router_for_tenant(Uuid::new_v4());
    let body = json!({
        "server": {"endpoints": [
            {"scheme": "https", "host": "10.0.1.1"},
            {"scheme": "https", "host": "10.0.1.2"},
        ]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });

    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// @cpt-begin:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-create-upstream-conflict-router-test-01
#[tokio::test]
async fn duplicate_alias_for_the_same_tenant_is_rejected_with_409() {
    let router = router_for_tenant(Uuid::new_v4());
    let first = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("dup.example.com")),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = send(
        router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("dup.example.com")),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let problem = body_json(second).await;
    assert_eq!(problem["status"], 409);
}
// @cpt-end:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-create-upstream-conflict-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-router-test-01
#[tokio::test]
async fn list_honors_top_and_skip_and_returns_only_the_calling_tenants_upstreams() {
    let router = router_for_tenant(Uuid::new_v4());
    for host in [
        "a.example.com",
        "b.example.com",
        "c.example.com",
        "d.example.com",
    ] {
        let response = send(
            router.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            Some(http_upstream(host)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = send(
        router,
        Method::GET,
        "/oagw/v1/upstreams?$top=10&$skip=0",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    let items = items.as_array().expect("list body must be a JSON array");
    assert!(items.len() <= 10);
    assert_eq!(items.len(), 4);
}
// @cpt-end:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-unknown-id-test-01
#[tokio::test]
async fn get_returns_404_for_an_unknown_id() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-unknown-id-test-01

#[tokio::test]
async fn get_for_an_identifier_owned_by_a_different_tenant_returns_404() {
    let (owner_router, other_router) = routers_sharing_state(Uuid::new_v4(), Uuid::new_v4());

    let created = send(
        owner_router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("owned.example.com")),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        other_router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// @cpt-begin:cpt-cf-oagw-dod-alias-immutability:p1:inst-replace-upstream-alias-change-router-test-01
#[tokio::test]
async fn replace_changing_the_hostname_such_that_the_alias_would_change_is_rejected() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("stable.example.com")),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let response = send(
        router.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(http_upstream("changed.example.com")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let unchanged = send(
        router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(unchanged.status(), StatusCode::OK);
    let unchanged = body_json(unchanged).await;
    assert_eq!(unchanged["alias"], "stable.example.com");
}
// @cpt-end:cpt-cf-oagw-dod-alias-immutability:p1:inst-replace-upstream-alias-change-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-enable-disable:p1:inst-replace-upstream-ancestor-disable-router-test-01
#[tokio::test]
async fn replace_setting_enabled_true_while_an_ancestors_same_alias_upstream_is_disabled_is_rejected()
 {
    let (root_router, tenant_router) = routers_sharing_state(ROOT_TENANT_ID, Uuid::new_v4());

    let mut disabled_root = http_upstream("shadowed.example.com");
    disabled_root["enabled"] = json!(false);
    let root_created = send(
        root_router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(disabled_root),
    )
    .await;
    assert_eq!(root_created.status(), StatusCode::CREATED);

    let mut disabled_descendant = http_upstream("shadowed.example.com");
    disabled_descendant["enabled"] = json!(false);
    let descendant_created = send(
        tenant_router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(disabled_descendant),
    )
    .await;
    assert_eq!(descendant_created.status(), StatusCode::CREATED);
    let descendant_created = body_json(descendant_created).await;
    let id = descendant_created["id"]
        .as_str()
        .expect("id must be a string");

    let mut re_enable = http_upstream("shadowed.example.com");
    re_enable["enabled"] = json!(true);
    let response = send(
        tenant_router,
        Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(re_enable),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
// @cpt-end:cpt-cf-oagw-dod-enable-disable:p1:inst-replace-upstream-ancestor-disable-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-enable-disable:p1:inst-create-upstream-ancestor-disable-router-test-01
#[tokio::test]
async fn create_under_a_disabled_ancestor_alias_with_no_enabled_field_is_rejected_and_persists_nothing()
 {
    let (root_router, tenant_router) = routers_sharing_state(ROOT_TENANT_ID, Uuid::new_v4());

    let mut disabled_root = http_upstream("ancestor-shadow.example.com");
    disabled_root["enabled"] = json!(false);
    let root_created = send(
        root_router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(disabled_root),
    )
    .await;
    assert_eq!(root_created.status(), StatusCode::CREATED);

    // No `enabled` field at all: defaults to `true`.
    let body = http_upstream("ancestor-shadow.example.com");
    let response = send(
        tenant_router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let list = send(tenant_router, Method::GET, "/oagw/v1/upstreams", None).await;
    let items = body_json(list).await;
    assert!(items.as_array().expect("list must be an array").is_empty());
}
// @cpt-end:cpt-cf-oagw-dod-enable-disable:p1:inst-create-upstream-ancestor-disable-router-test-01

// @cpt-begin:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-router-test-01
#[tokio::test]
async fn delete_returns_204_and_a_subsequent_get_returns_404() {
    let router = router_for_tenant(Uuid::new_v4());
    let created = send(
        router.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("gone.example.com")),
    )
    .await;
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    let deleted = send(
        router.clone(),
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let after = send(
        router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-router-test-01

#[tokio::test]
async fn delete_returns_404_for_an_unknown_id() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_a_security_context_extension_the_gear_still_functions_via_the_nil_tenant() {
    let state = Arc::new(ControlPlaneState::new());
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi).layer(Extension(state));

    let response = send(
        router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("no-ctx.example.com")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

// @cpt-begin:cpt-cf-oagw-dod-error-source-header:p1:inst-upstreams-success-source-header-test-01
/// `cpt-cf-oagw-dod-error-source-header`'s criterion is unqualified: "Every
/// response returned by the gear, success or failure, includes an
/// `X-OAGW-Error-Source` response header" — not just error responses.
#[tokio::test]
async fn a_successful_create_upstream_response_still_carries_the_error_source_header() {
    let router = router_for_tenant(Uuid::new_v4());
    let response = send(
        router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("source-header.example.com")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway"),
        "a successful management response is gateway-originated, not an upstream passthrough"
    );
}
// @cpt-end:cpt-cf-oagw-dod-error-source-header:p1:inst-upstreams-success-source-header-test-01

// @cpt-begin:cpt-cf-oagw-dod-control-plane-state:p2:inst-cp-no-external-persistence-test-01
/// Covers `cpt-cf-oagw-dod-control-plane-state`'s "no database connection ...
/// held entirely in-process" criterion: two independently constructed
/// `ControlPlaneState`s (standing in for two separate process lifetimes,
/// since this crate opens no database and has no other persistence layer to
/// point at) never see each other's writes. If any hidden shared store
/// existed (e.g. a file, a process-wide static, or a real database), the
/// second router would see the first upstream created against the first.
#[tokio::test]
async fn separate_control_plane_instances_share_no_state() {
    let tenant_id = Uuid::new_v4();

    let router_a = router_for_tenant(tenant_id);
    let created = send(
        router_a,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("only-in-instance-a.example.com")),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = body_json(created).await;
    let id = created["id"].as_str().expect("id must be a string");

    // A brand-new `ControlPlaneState` (a fresh router, not `.clone()`d from
    // the one above) never sees the id created against the first.
    let router_b = router_for_tenant(tenant_id);
    let response = send(
        router_b,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-control-plane-state:p2:inst-cp-no-external-persistence-test-01

// @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-create-upstream-allow-http-upstream-independence-test-01
/// The graded configuration depends on `allow_http_upstream` governing only
/// whether a plaintext *connection* is later opened at proxy time, never
/// whether an `http`/`ws` endpoint is *accepted* at create time
/// (`cpt-cf-oagw-dod-plaintext-connection-policy`). Asserted here with the
/// flag explicitly set to `false` on the extended config, rather than
/// relying on it merely defaulting to `false`.
#[tokio::test]
async fn create_with_http_scheme_is_accepted_even_when_allow_http_upstream_is_disabled() {
    let state = Arc::new(ControlPlaneState::new());
    let openapi = OpenApiRegistryImpl::new();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context must build");
    let config = oagw::config::OagwConfig {
        allow_http_upstream: false,
        ..oagw::config::OagwConfig::default()
    };
    let router = register_routes(Router::new(), &openapi)
        .layer(Extension(state))
        .layer(Extension(Arc::new(config)))
        .layer(Extension(ctx));

    let response = send(
        router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(http_upstream("still-accepted.example.com")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_with_ws_scheme_is_accepted_even_when_allow_http_upstream_is_disabled() {
    let state = Arc::new(ControlPlaneState::new());
    let openapi = OpenApiRegistryImpl::new();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context must build");
    let config = oagw::config::OagwConfig {
        allow_http_upstream: false,
        ..oagw::config::OagwConfig::default()
    };
    let router = register_routes(Router::new(), &openapi)
        .layer(Extension(state))
        .layer(Extension(Arc::new(config)))
        .layer(Extension(ctx));

    let body = json!({
        "server": {"endpoints": [{"scheme": "ws", "host": "still-accepted-ws.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let response = send(router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}
// @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-create-upstream-allow-http-upstream-independence-test-01
