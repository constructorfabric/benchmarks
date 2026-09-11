//! Router-level tests for the OAGW management API.
//!
//! The gear's own router is built and driven directly, so these exercise the
//! real routes, status codes and bodies without booting the whole server.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt;

use super::register_routes;
use crate::config::OagwConfig;
use crate::gear::OagwState;

fn router() -> Router {
    let openapi = OpenApiRegistryImpl::new();
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    register_routes(Router::new(), &openapi, state)
}

async fn call(r: &Router, method: &str, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn upstream_body(alias: Option<&str>, scheme: &str, host: &str, port: u16) -> serde_json::Value {
    let mut v = serde_json::json!({
        "server": {"endpoints": [{"scheme": scheme, "host": host, "port": port}]},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    if let Some(a) = alias {
        v["alias"] = serde_json::Value::String(a.to_owned());
    }
    v
}

// ---- upstreams ---------------------------------------------------------

#[tokio::test]
async fn creating_a_plaintext_http_upstream_succeeds_with_201() {
    // The task's scheme-widening override: `http` must be accepted at create
    // time even though the frozen schema's enum lists only the TLS family.
    let r = router();
    let (s, body) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(None, "http", "example.com", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(body["alias"], "example.com");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "http");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn creating_a_plaintext_websocket_upstream_succeeds_with_201() {
    let r = router();
    let (s, _) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(Some("ws-host"), "ws", "10.0.0.7", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
}

#[tokio::test]
async fn a_tls_upstream_still_succeeds() {
    let r = router();
    let (s, body) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(None, "https", "secure.example", 443)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(body["alias"], "secure.example");
}

#[tokio::test]
async fn an_ip_literal_upstream_without_an_alias_is_400() {
    let r = router();
    let (s, body) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(None, "http", "10.0.0.1", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn a_contradicting_alias_is_400() {
    let r = router();
    let (s, _) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(Some("something-else"), "http", "example.com", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_duplicate_alias_is_409() {
    let r = router();
    let b = upstream_body(None, "http", "dupe.example", 80);
    let (s1, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(b.clone())).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(b)).await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_unknown_protocol_is_400() {
    let r = router();
    let mut b = upstream_body(None, "http", "example.com", 80);
    b["protocol"] = serde_json::Value::String("gts.cf.core.oagw.protocol.v1~nope".to_owned());
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(b)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_empty_endpoint_pool_is_400() {
    let r = router();
    let b = serde_json::json!({
        "server": {"endpoints": []},
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(b)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wildcard_origin_with_credentials_is_rejected_at_write_time() {
    let r = router();
    let mut b = upstream_body(None, "http", "cors.example", 80);
    b["cors"] = serde_json::json!({
        "enabled": true, "allowed_origins": ["*"], "allow_credentials": true
    });
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(b)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_list_and_delete_round_trip() {
    let r = router();
    let (_, created) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(None, "http", "round.example", 80)),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (s, got) = call(&r, "GET", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["alias"], "round.example");

    let (s, page) = call(&r, "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    let (s, _) = call(&r, "DELETE", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, _) = call(&r, "GET", &format!("/oagw/v1/upstreams/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_upstream_is_404() {
    let r = router();
    let (s, _) = call(
        &r,
        "GET",
        "/oagw/v1/upstreams/11111111-1111-1111-1111-111111111111",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replacing_an_upstream_keeps_the_alias_and_rejects_a_change() {
    let r = router();
    let (_, created) = call(
        &r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(None, "http", "keep.example", 80)),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // Same host: accepted, and the port change lands.
    let (s, replaced) = call(
        &r,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(upstream_body(None, "http", "keep.example", 8080)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(replaced["server"]["endpoints"][0]["port"], 8080);
    assert_eq!(replaced["alias"], "keep.example");

    // A different host would derive a different alias: refused.
    let (s, _) = call(
        &r,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(upstream_body(None, "http", "other.example", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// ---- routes ------------------------------------------------------------

async fn make_upstream(r: &Router, alias: &str) -> String {
    let (s, created) = call(
        r,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_body(Some(alias), "http", "10.0.0.9", 80)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    created["id"].as_str().unwrap().to_owned()
}

fn route_body(upstream_id: &str, methods: &[&str], path: &str) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id,
        "match": {"http": {"methods": methods, "path": path}}
    })
}

#[tokio::test]
async fn creating_a_route_succeeds_with_201() {
    let r = router();
    let up = make_upstream(&r, "r1").await;
    let (s, body) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(&up, &["GET"], "/v1")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(body["upstream_id"], up);
    assert_eq!(body["match"]["http"]["path_suffix_mode"], "append");
}

#[tokio::test]
async fn a_route_naming_an_unresolvable_upstream_is_400_not_404() {
    let r = router();
    let (s, _) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(
            "11111111-1111-1111-1111-111111111111",
            &["GET"],
            "/v1",
        )),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_colliding_route_match_is_409() {
    let r = router();
    let up = make_upstream(&r, "r2").await;
    let b = route_body(&up, &["GET"], "/v1");
    let (s1, _) = call(&r, "POST", "/oagw/v1/routes", Some(b.clone())).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, _) = call(&r, "POST", "/oagw/v1/routes", Some(b)).await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_route_differing_only_by_method_does_not_collide() {
    let r = router();
    let up = make_upstream(&r, "r3").await;
    let (s1, _) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(&up, &["GET"], "/v1")),
    )
    .await;
    let (s2, _) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(&up, &["POST"], "/v1")),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::CREATED);
}

#[tokio::test]
async fn a_route_with_neither_match_kind_is_400() {
    let r = router();
    let up = make_upstream(&r, "r4").await;
    let (s, _) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(serde_json::json!({"upstream_id": up, "match": {}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn replacing_a_route_rejects_an_upstream_id_in_the_body() {
    let r = router();
    let up = make_upstream(&r, "r5").await;
    let (_, created) = call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(&up, &["GET"], "/v1")),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    // `upstream_id` is immutable and therefore not part of the replace shape.
    let (s, _) = call(
        &r,
        "PUT",
        &format!("/oagw/v1/routes/{id}"),
        Some(route_body(&up, &["GET"], "/v2")),
    )
    .await;
    assert!(
        s == StatusCode::BAD_REQUEST || s == StatusCode::UNPROCESSABLE_ENTITY,
        "an immutable upstream_id in the body must be rejected, got {s}"
    );
}

#[tokio::test]
async fn deleting_an_upstream_removes_its_routes() {
    let r = router();
    let up = make_upstream(&r, "r6").await;
    call(
        &r,
        "POST",
        "/oagw/v1/routes",
        Some(route_body(&up, &["GET"], "/v1")),
    )
    .await;
    let (_, page) = call(&r, "GET", "/oagw/v1/routes", None).await;
    assert_eq!(page["total"], 1);

    call(&r, "DELETE", &format!("/oagw/v1/upstreams/{up}"), None).await;
    let (_, page) = call(&r, "GET", "/oagw/v1/routes", None).await;
    assert_eq!(page["total"], 0);
}

// ---- plugins -----------------------------------------------------------

fn plugin_body(name: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "plugin_type": kind,
        "source_code": "def on_request(ctx): ctx.next()"
    })
}

#[tokio::test]
async fn a_custom_plugin_definition_round_trips() {
    let r = router();
    let (s, created) = call(
        &r,
        "POST",
        "/oagw/v1/plugins",
        Some(plugin_body("redactor", "transform")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();

    let (s, got) = call(&r, "GET", &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["name"], "redactor");

    let (s, _) = call(&r, "GET", &format!("/oagw/v1/plugins/{id}/source"), None).await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = call(&r, "DELETE", &format!("/oagw/v1/plugins/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn there_is_no_put_for_a_plugin_definition() {
    let r = router();
    let (_, created) = call(
        &r,
        "POST",
        "/oagw/v1/plugins",
        Some(plugin_body("immutable", "guard")),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let (s, _) = call(
        &r,
        "PUT",
        &format!("/oagw/v1/plugins/{id}"),
        Some(plugin_body("immutable", "guard")),
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_duplicate_plugin_name_is_409() {
    let r = router();
    call(&r, "POST", "/oagw/v1/plugins", Some(plugin_body("dup", "guard"))).await;
    let (s, _) = call(&r, "POST", "/oagw/v1/plugins", Some(plugin_body("dup", "guard"))).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn deleting_a_bound_plugin_is_409_naming_what_references_it() {
    let r = router();
    let (_, plugin) = call(
        &r,
        "POST",
        "/oagw/v1/plugins",
        Some(plugin_body("bound", "guard")),
    )
    .await;
    let pid = plugin["id"].as_str().unwrap().to_owned();

    let mut up = upstream_body(Some("bindme"), "http", "10.0.0.5", 80);
    up["plugins"] = serde_json::json!({"items": [pid]});
    let (s, created_up) = call(&r, "POST", "/oagw/v1/upstreams", Some(up)).await;
    assert_eq!(s, StatusCode::CREATED);
    let up_id = created_up["id"].as_str().unwrap().to_owned();

    let (s, body) = call(&r, "DELETE", &format!("/oagw/v1/plugins/{pid}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // The body names the referencing upstream.
    let rendered = body.to_string();
    assert!(
        rendered.contains(&up_id) || rendered.contains("referenced"),
        "409 body should identify what references the plugin: {rendered}"
    );
}

#[tokio::test]
async fn binding_an_unknown_plugin_is_400() {
    let r = router();
    let mut up = upstream_body(Some("badbind"), "http", "10.0.0.6", 80);
    up["plugins"] = serde_json::json!({"items": ["not-a-plugin"]});
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(up)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn binding_a_catalog_only_plugin_is_400() {
    let r = router();
    let mut up = upstream_body(Some("catonly"), "http", "10.0.0.8", 80);
    up["plugins"] = serde_json::json!({
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"]
    });
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(up)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn binding_a_served_builtin_guard_succeeds() {
    let r = router();
    let mut up = upstream_body(Some("goodbind"), "http", "10.0.0.11", 80);
    up["plugins"] = serde_json::json!({
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]
    });
    let (s, _) = call(&r, "POST", "/oagw/v1/upstreams", Some(up)).await;
    assert_eq!(s, StatusCode::CREATED);
}

#[tokio::test]
async fn the_catalog_marks_which_entries_are_served() {
    let r = router();
    let (s, body) = call(&r, "GET", "/oagw/v1/plugins/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["served"] == true));
    assert!(items.iter().any(|i| i["served"] == false));
}

// ---- paging ------------------------------------------------------------

#[tokio::test]
async fn listing_pages_with_top_and_skip() {
    let r = router();
    for i in 0..5 {
        call(
            &r,
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None, "http", &format!("h{i}.example"), 80)),
        )
        .await;
    }
    let (_, page) = call(&r, "GET", "/oagw/v1/upstreams?$top=2&$skip=1", None).await;
    assert_eq!(page["total"], 5);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
}

// ---- health ------------------------------------------------------------

#[tokio::test]
async fn the_gear_health_route_answers_200() {
    let r = router();
    let (s, body) = call(&r, "GET", "/oagw/v1/health", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["gear"], "oagw");
}

#[tokio::test]
async fn routes_are_registered_gear_relative_without_an_api_prefix() {
    // The whole point of the gear-relative override: the `/api`-prefixed form
    // must NOT be what this gear registers.
    let r = router();
    let (s, _) = call(&r, "GET", "/api/oagw/v1/health", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&r, "GET", "/oagw/v1/health", None).await;
    assert_eq!(s, StatusCode::OK);
}
