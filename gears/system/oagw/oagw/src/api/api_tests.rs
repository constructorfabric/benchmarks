#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines, clippy::cognitive_complexity, clippy::similar_names)]
//! Router-level tests: management surface, error semantics and the data plane.
//!
//! Everything runs against the real [`crate::api::router`] over in-memory stores. The upstream side
//! of the proxy is served by an in-process `axum` app (`mock`), so the buffered, the streaming and
//! the WebSocket path are all exercised over real HTTP connections.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use super::{ApiState, router};
use crate::domain::ControlPlane;
use crate::domain::gts_helpers::{ERR_VALIDATION, PROTOCOL_HTTP, PROTOCOL_GRPC};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::infra::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::infra::proxy::service::{DataPlane, DataPlaneSettings};
use crate::infra::storage::memory::MemoryStores;

#[path = "api_tests/mock.rs"]
mod mock;

const TENANT: Uuid = Uuid::from_u128(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10);
const OTHER_TENANT: Uuid = Uuid::from_u128(0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111);
const MAX_BODY: usize = 1 << 20;

/// A fully wired API state over in-memory stores, with plaintext upstreams legal.
fn harness() -> ApiState {
    harness_with(Arc::new(super::NoAncestors))
}

/// The same harness, with a caller-supplied tenant hierarchy.
fn harness_with(ancestors: Arc<dyn super::AncestorSource>) -> ApiState {
    let stores = Arc::new(MemoryStores::new());
    let settings = DataPlaneSettings {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        max_body_bytes: MAX_BODY,
    };
    let resolver: crate::infra::credentials::SecretResolver =
        Arc::new(crate::infra::credentials::MissingResolver);
    let data = Arc::new(DataPlane::new(
        stores,
        AuthPluginRegistry::with_builtins(Arc::clone(&resolver)),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
        settings,
        Some(resolver),
    ));
    let upstream_repo: Arc<dyn UpstreamRepository> = Arc::clone(&data.stores().upstreams) as _;
    let route_repo: Arc<dyn RouteRepository> = Arc::clone(&data.stores().routes) as _;
    let plugin_repo: Arc<dyn PluginRepository> = Arc::clone(&data.stores().plugins) as _;
    let control = Arc::new(ControlPlane::new(upstream_repo, route_repo, plugin_repo, true));
    ApiState::new(control, data, ancestors)
}

/// Inject the caller's `SecurityContext` the way the platform gateway does.
fn with_context(mut request: axum::http::Request<Body>, tenant: Uuid) -> axum::http::Request<Body> {
    request
        .extensions_mut()
        .insert(SecurityContext::builder().subject_id(tenant).subject_tenant_id(tenant).build().unwrap());
    request
}

/// The variant of [`call_with`] a handler actually reads the tenant from.
async fn call_tenant(state: &ApiState, method: &'static str, path: &str, tenant: Uuid) -> Response {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .map(|request| with_context(request, tenant))
        .unwrap();
    router(state.clone()).oneshot(request).await.unwrap()
}

async fn send(state: &ApiState, request: axum::http::Request<Body>) -> Response {
    router(state.clone()).oneshot(request).await.unwrap()
}

async fn json(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// POST as the public tenant — the shape a raw WebSocket client presents.
async fn post_public(state: &ApiState, path: &str, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(path)
        .body(Body::from(body.to_string()))
        .unwrap();
    json(send(state, request).await).await
}

async fn post(state: &ApiState, path: &str, body: Value) -> (StatusCode, Value) {
    post_tenant(state, path, body, TENANT).await
}

/// POST as a specific tenant, the way an operator of that tenant would.
async fn post_tenant(
    state: &ApiState,
    path: &str,
    body: Value,
    tenant: Uuid,
) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(path)
        .body(Body::from(body.to_string()))
        .map(|request| with_context(request, tenant))
        .unwrap();
    json(send(state, request).await).await
}

async fn get(state: &ApiState, path: &str) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(path)
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    json(send(state, request).await).await
}

async fn delete(state: &ApiState, path: &str) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri(path)
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    json(send(state, request).await).await
}

async fn put(state: &ApiState, path: &str, body: Value) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::PUT)
        .uri(path)
        .body(Body::from(body.to_string()))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    json(send(state, request).await).await
}

/// One endpoint of the in-process mock upstream.
fn endpoint_of(addr: SocketAddr) -> Value {
    json!({"scheme": "http", "host": addr.ip().to_string(), "port": addr.port()})
}

fn upstream_payload(addr: SocketAddr, alias: &str) -> Value {
    json!({
        "server": {"endpoints": [endpoint_of(addr)]},
        "protocol": PROTOCOL_HTTP,
        "alias": alias,
        "headers": {"request": {"passthrough": "allowlist", "passthrough_allowlist": ["x-call-marker"], "set": {"x-gateway": "oagw"}}},
        "tags": ["payments"],
    })
}

async fn wire_on(state: &ApiState, mock_addr: SocketAddr, path_prefix: &str) -> (String, String, Uuid) {
    let (status, upstream) = post(
        state,
        "/oagw/v1/upstreams",
        upstream_payload(mock_addr, "payments.example.com"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, route) = post(
        state,
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET", "POST"], "path": path_prefix}},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    let route_id = route["id"].as_str().unwrap().to_string();
    (upstream_id, route_id, TENANT)
}

// ---------------------------------------------------------------------------
// management: upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_an_upstream_derives_the_alias_and_returns_a_gts_id() {
    let state = harness();
    let _mock = mock::spawn(mock::app).await;
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 8443}]},
        "protocol": PROTOCOL_HTTP,
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "api.example.com:8443");
    assert!(body["gts_id"].as_str().unwrap().starts_with("gts.cf.core.oagw.upstream.v1~"));
    assert_eq!(body["enabled"], true, "an upstream is enabled unless told otherwise");
    assert_eq!(body["tenant_id"], TENANT.to_string());
}

#[tokio::test]
async fn an_explicit_alias_that_disagrees_with_the_derivation_is_rejected() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "alias": "not-the-derivation",
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], ERR_VALIDATION);
}

#[tokio::test]
async fn two_upstreams_in_one_tenant_cannot_share_an_alias() {
    let state = harness();
    let payload = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
    });
    let (first, _) = post(&state, "/oagw/v1/upstreams", payload.clone()).await;
    assert_eq!(first, StatusCode::CREATED);
    let (second, body) = post(&state, "/oagw/v1/upstreams", payload).await;
    assert_eq!(second, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn the_same_alias_in_another_tenant_is_free() {
    let state = harness();
    let payload = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
    });
    let (a, _) = post(&state, "/oagw/v1/upstreams", payload.clone()).await;
    assert_eq!(a, StatusCode::CREATED);
    // The other tenant posts through its own context.
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/oagw/v1/upstreams")
        .body(Body::from(payload.to_string()))
        .map(|request| with_context(request, OTHER_TENANT))
        .unwrap();
    let (status, _) = json(send(&state, request).await).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn plaintext_endpoints_are_accepted_when_the_policy_allows_them() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.5", "port": 80}]},
        "protocol": PROTOCOL_HTTP,
        "alias": "plaintext.internal",
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["alias"], "plaintext.internal");
}

#[tokio::test]
async fn an_unknown_protocol_is_rejected() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.websocket.v1",
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn an_unknown_request_field_is_a_validation_problem() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
        "unexpected": true,
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid-payload");
}

#[tokio::test]
async fn a_malformed_body_is_rendered_as_a_problem() {
    let state = harness();
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/oagw/v1/upstreams")
        .body(Body::from("}not json{"))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let (status, body) = json(send(&state, request).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], ERR_VALIDATION);
}

#[tokio::test]
async fn an_empty_body_is_rejected() {
    let state = harness();
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/oagw/v1/upstreams")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let (status, body) = json(send(&state, request).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn the_upstream_list_is_scoped_to_the_callers_tenant() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, body) = get(&state, "/oagw/v1/upstreams").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/upstreams")
        .body(Body::empty())
        .map(|request| with_context(request, OTHER_TENANT))
        .unwrap();
    let (status, body) = json(send(&state, request).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(0), "another tenant sees nothing");
}

#[tokio::test]
async fn an_upstream_round_trips_through_get_put_and_delete() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let path = format!("/oagw/v1/upstreams/{upstream_id}");

    let (status, body) = get(&state, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alias"], "payments.example.com");

    let (status, replaced) = post(
        &state,
        &format!("{path}/enabled"),
        json!({"enabled": false}),
    ).await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], false);

    let (status, deleted) = delete(&state, &path).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{deleted}");
    let (status, _) = get(&state, &path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_missing_upstream_is_not_found() {
    let state = harness();
    let (status, body) = delete(&state, &format!("/oagw/v1/upstreams/{}", Uuid::now_v7())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn an_upstream_can_be_replaced_without_its_alias_moving() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let path = format!("/oagw/v1/upstreams/{upstream_id}");

    // A replacement that repeats the alias keeps it.
    let (status, body) = put(&state, &path, json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "tags": ["payments", "retried"],
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alias"], "payments.example.com");
    assert_eq!(body["tags"], Value::Array(vec![json!("payments"), json!("retried")]));

    // So does one that omits it: the endpoints cannot derive an alias, so the one the upstream
    // already carries is kept.
    let (status, body) = put(&state, &path, json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alias"], "payments.example.com");
}

#[tokio::test]
async fn an_endpoint_change_that_would_rename_the_upstream_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let port = mock.address().port();

    // A hostname upstream derives its alias from its endpoints, so an endpoint change that would
    // derive a different one is a rename.
    let (status, created) = post(
        &state,
        "/oagw/v1/upstreams",
        json!({
            "server": {"endpoints": [{"scheme": "http", "host": "payments.example.com", "port": port}]},
            "protocol": PROTOCOL_HTTP,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let upstream_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["alias"], format!("payments.example.com:{port}"));
    let path = format!("/oagw/v1/upstreams/{upstream_id}");

    let (status, body) = put(
        &state,
        &path,
        json!({
            "server": {"endpoints": [{"scheme": "http", "host": "elsewhere.example.com", "port": port}]},
            "protocol": PROTOCOL_HTTP,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("delete and re-create"),
        "{body}"
    );

    let (_, after) = get(&state, &path).await;
    assert_eq!(
        after["alias"],
        format!("payments.example.com:{port}"),
        "the refused replacement left the alias alone"
    );

    // The same endpoints are accepted: the derivation repeats what the upstream carries.
    let (status, body) = put(
        &state,
        &path,
        json!({
            "server": {"endpoints": [{"scheme": "http", "host": "payments.example.com", "port": port}]},
            "protocol": PROTOCOL_HTTP,
            "enabled": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alias"], format!("payments.example.com:{port}"));
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn a_replacement_cannot_rename_the_upstream() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, body) = put(
        &state,
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        json!({
            "server": {"endpoints": [endpoint_of(mock.address())]},
            "protocol": PROTOCOL_HTTP,
            "alias": "renamed.example.com",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ---------------------------------------------------------------------------
// management: routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_route_requires_a_known_upstream() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": Uuid::now_v7(),
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_UPSTREAM_NOT_FOUND);
}

#[tokio::test]
async fn a_route_round_trips_and_is_listed() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, route_id, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, list) = get(&state, "/oagw/v1/routes").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    let entry = &list.as_array().unwrap()[0];
    assert_eq!(entry["id"], route_id);
    assert!(entry["gts_id"].as_str().unwrap().starts_with("gts.cf.core.oagw.route.v1~"));
    assert_eq!(entry["upstream_id"], Value::String(upstream_id));
}

#[tokio::test]
async fn a_route_match_without_methods_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": [], "path": "/v1"}},
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_route_for_an_unsupported_method_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["TRACE"], "path": "/v1"}},
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_grpc_route_on_an_http_upstream_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"grpc": {"service": "pkg.Svc", "method": "Call"}},
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let _ = PROTOCOL_GRPC;
}

#[tokio::test]
async fn the_same_match_rule_is_a_conflict_within_an_upstream_but_not_across_them() {
    let state = harness();
    let first = mock::spawn(mock::app).await;
    let second = mock::spawn(mock::app).await;
    let (first_id, _, _) = wire_on(&state, first.address(), "/v1").await;

    // The same rule on the same upstream is a conflict.
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": first_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The same rule on a different upstream is a legitimate second route.
    let (status, other) = post(&state, "/oagw/v1/upstreams", upstream_payload(second.address(), "other.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    let (status, body) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": other["id"],
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ---------------------------------------------------------------------------
// management: plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_catalogue_lists_the_builtins_and_the_tenants_own_definitions() {
    let state = harness();
    let (status, catalogue) = get(&state, "/oagw/v1/plugins").await;
    assert_eq!(status, StatusCode::OK, "{catalogue}");
    let builtins = catalogue["builtins"].as_array().unwrap();
    assert_eq!(builtins.len(), 12, "{}", serde_json::to_string_pretty(&catalogue).unwrap());
    assert!(
        builtins.iter().all(|e| e["resolvable"].is_boolean()),
        "{}",
        serde_json::to_string_pretty(&catalogue).unwrap()
    );
    let noop = builtins.iter().find(|e| e["kind"] == "auth" && e["resolvable"] == true).unwrap();
    assert!(noop["id"].as_str().unwrap().starts_with("gts.cf.core.oagw.auth_plugin.v1~"));
    assert_eq!(catalogue["plugins"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_plugin_round_trips_and_is_guarded_while_in_use() {
    let state = harness();
    let (status, created) = post(&state, "/oagw/v1/plugins", json!({
        "name": "tenant header guard",
        "description": "requires a tenant header",
        "type": "guard",
        "phases": ["guard"],
        "config_schema": {"type": "object"},
        "source_code": "def guard(ctx):\n    return ctx",
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let plugin_id = created["id"].as_str().unwrap().to_string();
    assert!(created["in_use"] == false);

    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, bound) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/guarded"}},
        "plugins": {"items": [plugin_id]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{bound}");

    let path = format!("/oagw/v1/plugins/{plugin_id}");
    let (status, body) = get(&state, &path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["in_use"], true);
    assert!(!body["referenced_by"].as_array().unwrap().is_empty());

    let (status, refused) = delete(&state, &path).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    let (status, source) = get(&state, &format!("{path}/source")).await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert!(source["source"].as_str().unwrap().contains("def guard"));
}

#[tokio::test]
async fn a_plugin_can_be_deleted_once_it_is_unused() {
    let state = harness();
    let (status, created) = post(&state, "/oagw/v1/plugins", json!({
        "name": "orphan",
        "phases": ["transform"],
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let path = format!("/oagw/v1/plugins/{}", created["id"].as_str().unwrap());
    let (status, deleted) = delete(&state, &path).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{deleted}");
}

#[tokio::test]
async fn a_catalog_only_auth_plugin_is_not_bindable() {
    let state = harness();
    let basic = crate::domain::gts_helpers::AUTH_BASIC;
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
        "auth": {"type": basic, "config": {"secret_ref": "cred://x"}},
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_registered_auth_plugin_is_bindable() {
    let state = harness();
    let (status, body) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        "protocol": PROTOCOL_HTTP,
        "auth": {"type": crate::domain::gts_helpers::AUTH_APIKEY,
                 "config": {"secret_ref": "cred://gateway/key", "header": "x-api-key"}},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ---------------------------------------------------------------------------
// error shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn errors_carry_the_problem_media_type_and_the_gateway_stamp() {
    let state = harness();
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/upstreams/00000000-0000-0000-0000-000000000001")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(
        response.headers().get("x-oagw-error-source").map(|v| v.to_str().unwrap()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_proxied_request_to_an_unknown_alias_is_a_gateway_404() {
    let state = harness();
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/ghost.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers().get("x-oagw-error-source").map(|v| v.to_str().unwrap()), Some("gateway"));
    let (status, body) = json(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["type"],
        crate::domain::gts_helpers::ERR_ROUTE_NOT_FOUND,
        "the DESIGN error table carries a single 404 type"
    );
    assert_eq!(body["alias"], "ghost.example.com");
}

// ---------------------------------------------------------------------------
// proxy: buffered
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_proxied_call_appends_the_suffix_and_returns_the_upstream_body() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1/charges", TENANT).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);

    let calls = mock.snapshot();
    assert_eq!(calls.len(), 1, "{:?}", calls);
    assert_eq!(calls[0].path, "/v1/charges");
    assert_eq!(calls[0].method, "GET");
}

#[tokio::test]
async fn upstream_responses_are_stamped_as_passthrough() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-oagw-error-source").map(|v| v.to_str().unwrap()), Some("upstream"));
    assert_eq!(mock::body_of(response).await, "upstream-ok");
}

#[tokio::test]
async fn a_disabled_upstream_is_not_proxied() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, _) = post(&state, &format!("/oagw/v1/upstreams/{upstream_id}/enabled"), json!({"enabled": false})).await;
    assert_eq!(status, StatusCode::OK);
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mock.snapshot().len(), 0, "no call may reach the upstream");
}

#[tokio::test]
async fn a_descendant_cannot_reenable_an_ancestors_upstream() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, _) = post(&state, &format!("/oagw/v1/upstreams/{upstream_id}/enabled"), json!({"enabled": false})).await;
    assert_eq!(status, StatusCode::OK);

    // The ancestor's upstream does not exist in the descendant's tenant, so a descendant that
    // reaches for it is told it is not there rather than being able to flip it back on.
    let (status, _) = post_tenant(
        &state,
        &format!("/oagw/v1/upstreams/{upstream_id}/enabled"),
        json!({"enabled": true}),
        OTHER_TENANT,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{status}");
    let (_, body) = json(
        call_tenant(&state, "GET", &format!("/oagw/v1/upstreams/{upstream_id}"), OTHER_TENANT).await,
    )
    .await;
    assert_eq!(body["status"], 404, "{body}");
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "still disabled");
}

#[tokio::test]
async fn a_method_outside_the_route_match_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "DELETE", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_VALIDATION, "{body}");
}

#[tokio::test]
async fn a_body_whose_declared_length_disagrees_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("content-length", "5")
        .body(Body::from("this body is longer than declared"))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_VALIDATION, "{body}");
}

#[tokio::test]
async fn a_transfer_encoding_the_gateway_does_not_speak_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("transfer-encoding", "gzip")
        .body(Body::from("body"))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_VALIDATION, "{body}");
}

#[tokio::test]
async fn a_body_over_the_configured_ceiling_is_payload_too_large() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .body(Body::from(vec![b'x'; MAX_BODY + 1]))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_PAYLOAD_TOO_LARGE, "{body}");
}

#[tokio::test]
async fn a_path_without_any_route_is_still_a_404() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/unrelated", TENANT).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{}", mock::body_of(response).await);
    assert_eq!(mock.snapshot().len(), 0);
}

#[tokio::test]
async fn the_query_allowlist_rejects_unlisted_parameters() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1", "query_allowlist": ["page"]}},
    }))
    .await;
    let _ = status;
    let response = call_tenant(
        &state,
        "GET",
        "/oagw/v1/proxy/payments.example.com/v1?secret=leak",
        TENANT,
    ).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    assert_eq!(mock.snapshot().len(), 0, "nothing reaches the upstream");
}

#[tokio::test]
async fn an_empty_query_allowlist_permits_no_query_parameters() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1?page=2", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
}

#[tokio::test]
async fn an_allowed_query_parameter_reaches_the_upstream() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1", "query_allowlist": ["page"]}},
    }))
    .await;
    let _ = status;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1?page=2", TENANT).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);
    let calls = mock.snapshot();
    assert_eq!(calls[0].query, "page=2", "{:?}", calls[0].query);
}

#[tokio::test]
async fn a_route_whose_allowlist_is_absent_allows_no_parameters() {
    // The route schema defaults `query_allowlist` to `[]`, and "if empty, allow none".
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1?any=thing", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    let bare = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(bare.status(), StatusCode::OK, "{}", mock::body_of(bare).await);
}

#[tokio::test]
async fn request_header_rules_shape_the_outbound_call() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "headers": {"request": {"passthrough": "allowlist", "passthrough_allowlist": ["x-call-marker"], "set": {"x-gateway": "oagw"}, "remove": ["x-secret"]}},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("x-call-marker", "present")
        .header("x-secret", "must-not-travel")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = mock.snapshot();
    assert_eq!(calls[0].header("x-call-marker").as_deref(), Some("present"), "allowlisted headers travel");
    assert_eq!(calls[0].header("x-gateway").as_deref(), Some("oagw"), "set headers travel");
    assert_eq!(calls[0].header("x-secret"), None, "removed headers must not travel");
    assert_ne!(
        calls[0].header("host").as_deref(),
        Some("gateway.internal"),
        "the inbound host must not travel"
    );
}

#[tokio::test]
async fn the_outbound_call_carries_exactly_one_host_header() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    // A request that arrived through a real gateway always carries a Host; the call to the
    // upstream may not end up with two of them.
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("host", "gateway.internal")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);
    let calls = mock.snapshot();
    assert_eq!(calls[0].header_count("host"), 1, "{:?}", calls[0].headers);
    assert_eq!(calls[0].header("host").as_deref(), Some(mock.address().to_string().as_str()));
}

#[tokio::test]
async fn the_request_body_is_forwarded_verbatim() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["POST"], "path": "/v1"}},
    }))
    .await;
    let _ = status;
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/oagw/v1/proxy/payments.example.com/v1/echo")
        .body(Body::from("{\"amount\": 42}"))
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock::body_of(response).await, "{\"amount\": 42}");
}

#[tokio::test]
async fn required_headers_guard_refuses_a_request_without_them() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        // Guards run on the outbound request, so the header they check has to survive the
        // transformation rules first (DESIGN §Transformation Rules).
        "headers": {"request": {"passthrough": "all"}},
        "plugins": {"items": [{"plugin_ref": crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS,
                               "config": {"required_request_headers": "x-tenant"}}]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let bare = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(bare.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(bare).await);
    assert_eq!(mock.snapshot().len(), 0, "a refused call must not reach the upstream");

    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("x-tenant", "acme")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let allowed = send(&state, request).await;
    assert_eq!(allowed.status(), StatusCode::OK, "{}", mock::body_of(allowed).await);
}

#[tokio::test]
async fn guards_run_before_the_transform_plugins() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "plugins": {"items": [
            {"plugin_ref": crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS,
             "config": {"required_request_headers": "x-request-id"}},
            {"plugin_ref": crate::domain::gts_helpers::TRANSFORM_REQUEST_ID},
        ]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    // The request id transform would supply the very header the guard asks for, so a call that
    // arrives without one is only refused if guards really do run first (ADR-0002).
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    let (_, body) = json(response).await;
    assert_eq!(body["code"], "REQUIRED_HEADER_MISSING", "{body}");
    assert_eq!(body["context"]["code"], "REQUIRED_HEADER_MISSING", "{body}");
    assert_eq!(mock.snapshot().len(), 0, "a refused call must not reach the upstream");
}

#[tokio::test]
async fn an_upstreams_plugin_runs_before_a_routes_plugin() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "plugins": {"items": [{"plugin_ref": crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS,
                               "config": {"required_request_headers": "x-upstream-probe"}}]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, route) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        "plugins": {"items": [{"plugin_ref": crate::domain::gts_helpers::GUARD_REQUIRED_HEADERS,
                               "config": {"required_request_headers": "x-route-probe"}}]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    // Both guards refuse, and the one that runs first is the one bound to the upstream.
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{}", mock::body_of(response).await);
    let (_, body) = json(response).await;
    assert!(body["detail"].as_str().is_some_and(|detail| detail.contains("x-upstream-probe")), "{body}");
}

#[tokio::test]
async fn a_rate_limited_route_answers_429_with_a_retry_hint() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "rate_limit": {"sustained": {"rate": 1, "window": "second"}, "burst": {"capacity": 1}},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let first = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", mock::body_of(first).await);
    let second = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS, "{}", mock::body_of(second).await);
    let (status, body) = json(second).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_RATE_LIMIT);
    assert!(body["retry_after_seconds"].as_u64().unwrap() >= 1, "{body}");
}

/// Ancestor whose enforced limit outlives a descendant's shadowing upstream.
struct Chain(Vec<Uuid>);

#[async_trait::async_trait]
impl super::AncestorSource for Chain {
    async fn ancestors(&self, _tenant: Uuid) -> Vec<Uuid> {
        self.0.clone()
    }
}

#[tokio::test]
async fn an_ancestor_enforced_limit_applies_across_a_shadowing_upstream() {
    let ancestor = Uuid::from_u128(0x0f10);
    let state = harness_with(Arc::new(Chain(vec![ancestor])));
    let mock = mock::spawn(mock::app).await;

    // The ancestor publishes the alias with an enforced, strict limit.
    let (status, upstream) = post_tenant(
        &state,
        "/oagw/v1/upstreams",
        json!({
            "server": {"endpoints": [endpoint_of(mock.address())]},
            "protocol": PROTOCOL_HTTP,
            "alias": "shared.example.com",
            "rate_limit": {"sustained": {"rate": 1, "window": "second"}, "burst": {"capacity": 1},
                           "sharing": "enforce"},
        }),
        ancestor,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");

    // The descendant shadows it with a far more permissive limit of its own.
    let (status, upstream) = post_tenant(
        &state,
        "/oagw/v1/upstreams",
        json!({
            "server": {"endpoints": [endpoint_of(mock.address())]},
            "protocol": PROTOCOL_HTTP,
            "alias": "shared.example.com",
            "rate_limit": {"sustained": {"rate": 1000, "window": "second"},
                           "burst": {"capacity": 1000}},
        }),
        TENANT,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let (status, _) = post_tenant(
        &state,
        "/oagw/v1/routes",
        json!({"upstream_id": upstream["id"].as_str().unwrap(),
               "match": {"http": {"methods": ["GET"], "path": "/v1"}}}),
        TENANT,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let first = call_tenant(&state, "GET", "/oagw/v1/proxy/shared.example.com/v1", TENANT).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", mock::body_of(first).await);
    let second = call_tenant(&state, "GET", "/oagw/v1/proxy/shared.example.com/v1", TENANT).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS, "the ancestor's limit still applies");
}

#[tokio::test]
async fn a_cors_preflight_is_answered_without_reaching_the_upstream() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let denied = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("origin", "https://evil.example.com")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, denied).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{}", mock::body_of(response).await);
    assert_eq!(mock.snapshot().len(), 0);

    let allowed = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, allowed).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);
}

#[tokio::test]
async fn a_cors_preflight_is_answered_locally_with_204() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");

    // A preflight reaches the handler without a security context, as a browser would send it.
    let preflight = axum::http::Request::builder()
        .method(axum::http::Method::OPTIONS)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "Content-Type, Authorization")
        .body(Body::empty())
        .unwrap();
    let response = send(&state, preflight).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "{}", mock::body_of(response).await);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").map(|v| v.to_str().unwrap()),
        Some("https://app.example.com")
    );
    assert_eq!(headers.get("access-control-allow-methods").map(|v| v.to_str().unwrap()), Some("POST"));
    assert_eq!(
        headers.get("access-control-allow-headers").map(|v| v.to_str().unwrap()),
        Some("Content-Type, Authorization")
    );
    assert!(headers.get("access-control-max-age").is_some(), "{headers:?}");
    let vary = headers.get("vary").map(|v| v.to_str().unwrap()).unwrap_or_default();
    assert!(vary.contains("Origin"), "{vary}");
    assert_eq!(mock.snapshot().len(), 0, "the preflight must not reach the upstream");
}

#[tokio::test]
async fn a_preflight_for_an_unknown_alias_is_still_answered_locally() {
    // No tenant context is available for a preflight, so resolution is skipped entirely.
    let state = harness();
    let preflight = axum::http::Request::builder()
        .method(axum::http::Method::OPTIONS)
        .uri("/oagw/v1/proxy/ghost.example.com/v1")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    let response = send(&state, preflight).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "{}", mock::body_of(response).await);
}

#[tokio::test]
async fn an_allowed_cross_origin_call_receives_the_cors_response_headers() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let allowed = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, allowed).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("access-control-allow-origin").map(|v| v.to_str().unwrap()),
        Some("https://app.example.com")
    );
    assert!(headers
        .get("vary")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("origin")), "{headers:?}");
}

#[tokio::test]
async fn a_disallowed_method_on_a_cross_origin_call_is_rejected() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "payments.example.com",
        "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET", "DELETE"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let denied = axum::http::Request::builder()
        .method(axum::http::Method::DELETE)
        .uri("/oagw/v1/proxy/payments.example.com/v1")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, denied).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{}", mock::body_of(response).await);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_CORS_METHOD, "{body}");
}

#[tokio::test]
async fn a_multi_endpoint_upstream_requires_a_target_host() {
    let state = harness();
    // Two hostnames that share a registrable suffix derive `example.com`; the ports have to agree
    // for the derivation to succeed, so neither carries one.
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [
            {"scheme": "http", "host": "a.example.com"},
            {"scheme": "http", "host": "b.example.com"},
        ]},
        "protocol": PROTOCOL_HTTP,
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    assert_eq!(upstream["alias"], "example.com", "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let missing = call_tenant(&state, "GET", "/oagw/v1/proxy/example.com/v1", TENANT).await;
    let (status, body) = json(missing).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_MISSING_TARGET_HOST);
    let hosts = body["valid_hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2, "{body}");

    let unknown = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/example.com/v1")
        .header("x-oagw-target-host", "nope.example.com")
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, unknown).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (_, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_UNKNOWN_TARGET_HOST);
}

#[tokio::test]
async fn an_explicit_alias_selects_an_endpoint_without_a_target_host() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    // A pool's members share scheme and port, so both entries name the same listener.
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [endpoint_of(mock.address()), endpoint_of(mock.address())]},
        "protocol": PROTOCOL_HTTP,
        "alias": "pool.example.com",
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    // An explicit alias names no endpoint, so the call is balanced without a target host.
    let first = call_tenant(&state, "GET", "/oagw/v1/proxy/pool.example.com/v1", TENANT).await;
    assert_eq!(first.status(), StatusCode::OK, "{}", mock::body_of(first).await);
    assert_eq!(mock.calls(), 1, "exactly one endpoint is called");

    let host = format!("127.0.0.1:{}", mock.address().port());
    let pinned = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/proxy/pool.example.com/v1")
        .header("x-oagw-target-host", &host)
        .body(Body::empty())
        .map(|request| with_context(request, TENANT))
        .unwrap();
    let response = send(&state, pinned).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", mock::body_of(response).await);
    assert!(mock.calls() >= 2, "the target host picks its endpoint");
}

#[tokio::test]
async fn an_unreachable_upstream_surfaces_as_a_bad_gateway() {
    let state = harness();
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", json!({
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 1}]},
        "protocol": PROTOCOL_HTTP,
        "alias": "unreachable.example.com",
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    }))
    .await;
    let _ = status;

    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/unreachable.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let (status, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_DOWNSTREAM);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_disabled_route_is_not_matched_and_answers_route_not_found() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (upstream_id, route_id, _) = wire_on(&state, mock.address(), "/v1").await;
    let (status, _) = post(&state, &format!("/oagw/v1/routes/{route_id}/enabled"), json!({"enabled": false})).await;
    assert_eq!(status, StatusCode::OK);
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{}", mock::body_of(response).await);
    let (status, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_ROUTE_NOT_FOUND);
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(mock.is_empty(), "a call with no matching route must not reach the upstream");
    let _ = upstream_id;
}

#[tokio::test]
async fn a_call_that_matches_no_route_is_route_not_found() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (_, _, _) = wire_on(&state, mock.address(), "/v1").await;
    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/other", TENANT).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{}", mock::body_of(response).await);
    let (status, body) = json(response).await;
    assert_eq!(body["type"], crate::domain::gts_helpers::ERR_ROUTE_NOT_FOUND);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_route_with_a_disabled_suffix_mode_refuses_a_deeper_path() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/v1", "path_suffix_mode": "disabled"}},
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let _ = upstream_id;

    let exact = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1", TENANT).await;
    assert_eq!(exact.status(), StatusCode::OK, "{}", mock::body_of(exact).await);
    assert_eq!(mock.snapshot().len(), 1, "the exact path reaches the upstream");
    let deeper = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/v1/charges", TENANT).await;
    assert_eq!(deeper.status(), StatusCode::BAD_REQUEST, "{} the route refuses a suffix", mock::body_of(deeper).await);
    assert_eq!(mock.snapshot().len(), 1, "the refused call never reaches the upstream");
}

// ---------------------------------------------------------------------------
// proxy: streaming and WebSocket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_chunks_reach_the_caller_as_they_arrive() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/sse"}},
    }))
    .await;
    let _ = status;

    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/sse", TENANT).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").map(|v| v.to_str().unwrap()),
        Some("text/event-stream"),
        "the media type must survive the proxy"
    );
    let mut stream = response.into_body().into_data_stream();
    let first = futures_util::StreamExt::next(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, "event: start\ndata: one\n\n");
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), futures_util::StreamExt::next(&mut stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second, "data: two\n\n");
    // The upstream ends its stream here, so the proxied response has to end with it.
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), futures_util::StreamExt::next(&mut stream))
        .await
        .unwrap();
    assert!(end.is_none(), "the client connection ends when the upstream closes the stream");
}

#[tokio::test]
async fn an_sse_response_is_stamped_as_coming_from_the_upstream() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "payments.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/sse"}},
    }))
    .await;
    let _ = status;

    let response = call_tenant(&state, "GET", "/oagw/v1/proxy/payments.example.com/sse", TENANT).await;
    assert_eq!(
        response.headers().get("x-oagw-error-source").map(|v| v.to_str().unwrap()),
        Some("upstream"),
        "a streamed passthrough carries the same stamp as a buffered one"
    );
}

#[tokio::test]
async fn a_websocket_upgrade_is_proxied_in_both_directions() {
    let state = harness();
    let mock = mock::spawn(mock::ws_app).await;
    // A raw WebSocket client carries no SecurityContext, so the whole exchange runs as the public
    // tenant: the configuration is created the way the client will read it.
    let (status, upstream) = post_public(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "sockets.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post_public(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/"}},
    }))
    .await;
    let _ = status;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(server_state)).await;
    });

    let (mut upstream_ws, response) = tokio_tungstenite::connect_async(format!(
        "ws://{gateway}/oagw/v1/proxy/sockets.example.com/ws"
    ))
    .await
    .unwrap();
    assert_eq!(
        response.headers().get("x-oagw-error-source").map(|v| v.to_str().unwrap()),
        Some("upstream"),
        "the upgrade is answered by the upstream, not by the gear"
    );
    use tokio_tungstenite::tungstenite::Message as Ws;
    use futures_util::{SinkExt, StreamExt};
    upstream_ws.send(Ws::Text("hello through the gateway".into())).await.unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), upstream_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echoed, Ws::Text("echo:hello through the gateway".into()));
    let _ = upstream_ws.close(None).await;
    let _ = upstream_id;
}

#[tokio::test]
async fn a_websocket_request_to_a_plain_http_upstream_that_refuses_it_is_a_gateway_problem() {
    let state = harness();
    let mock = mock::spawn(mock::app).await;
    let (status, upstream) = post_public(&state, "/oagw/v1/upstreams", upstream_payload(mock.address(), "noupgrade.example.com")).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_string();
    let (status, _) = post_public(&state, "/oagw/v1/routes", json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": ["GET"], "path": "/"}},
    }))
    .await;
    let _ = status;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(server_state)).await;
    });
    let error = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/oagw/v1/proxy/noupgrade.example.com/ws"
    ))
    .await
    .expect_err("an upstream without an upgrade endpoint must not complete the handshake");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected an HTTP rejection, got {error:?}");
    };
    assert!(
        response.status().is_client_error() || response.status().is_server_error(),
        "an upstream without an upgrade endpoint must not answer 101: {}",
        response.status()
    );
}

