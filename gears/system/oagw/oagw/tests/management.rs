// Created: 2026-09-02 by Constructor Tech
//! Management-plane tests: the CRUD contract, the validation rules and the
//! error semantics `DESIGN.md` §3.3 lays out.
//!
//! Exercised through the real router, so the status codes and problem bodies
//! are the ones the wire carries.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use axum::Router;
use serde_json::{Value, json};
use support::{TENANT_A, TENANT_B, default_config, gateway, post_json};
use tower::ServiceExt;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

async fn gw() -> Router {
    gateway(default_config()).await
}

fn upstream_spec(alias: &str, host: &str) -> Value {
    json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": 8081}]},
        "protocol": PROTOCOL,
    })
}

/// Creates an upstream and returns its id as a string.
async fn create_upstream(router: &Router, spec: Value) -> String {
    let (status, body) = post_json(router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(status, 201, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

// ------------------------------------------------------------------- upstreams

#[tokio::test]
async fn create_upstream_returns_201_with_a_gts_id() {
    let router = gw().await;
    let (status, body) =
        post_json(&router, "POST", "/oagw/v1/upstreams", upstream_spec("echo", "127.0.0.1")).await;
    assert_eq!(status, 201);
    assert!(
        body["id"].as_str().unwrap().starts_with("gts.cf.core.oagw.upstream.v1~"),
        "{body}"
    );
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["alias"], json!("echo"));
    assert_eq!(body["protocol"], json!(PROTOCOL));
}

#[tokio::test]
async fn hostname_endpoints_derive_the_alias() {
    let router = gw().await;
    let spec = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com"}]},
        "protocol": PROTOCOL,
    });
    let (_, body) = post_json(&router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(body["alias"], json!("api.vendor.com"));
}

#[tokio::test]
async fn ip_literal_endpoints_require_an_explicit_alias() {
    let router = gw().await;
    let spec = json!({
        "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1"}]},
        "protocol": PROTOCOL,
    });
    let (status, body) = post_json(&router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("alias"), "{body}");
}

#[tokio::test]
async fn http_is_a_legal_scheme() {
    // `allow_http_upstream` governs whether the plaintext connection is made,
    // not which schemes the field accepts.
    let router = gw().await;
    let (status, body) = post_json(
        &router,
        "POST",
        "/oagw/v1/upstreams",
        json!({
            "alias": "plain",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 80}]},
            "protocol": PROTOCOL,
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], json!("http"));
}

#[tokio::test]
async fn duplicate_alias_conflicts() {
    let router = gw().await;
    let spec = upstream_spec("dup", "127.0.0.1");
    let (status, _) = post_json(&router, "POST", "/oagw/v1/upstreams", spec.clone()).await;
    assert_eq!(status, 201);
    let (status, body) = post_json(&router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("conflict"),
        "{body}"
    );
}

#[tokio::test]
async fn unknown_fields_are_rejected() {
    let router = gw().await;
    let mut spec = upstream_spec("unknown-field", "127.0.0.1");
    spec["bogus"] = json!("nope");
    let (status, _) = post_json(&router, "POST", "/oagw/v1/upstreams", spec).await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn get_put_and_delete_round_trip() {
    let router = gw().await;
    let id = create_upstream(&router, upstream_spec("roundtrip", "127.0.0.1")).await;

    let (status, got) = post_json(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(got["alias"], json!("roundtrip"));

    let mut replacement = got.clone();
    replacement["tags"] = json!(["edge"]);
    let (status, put) =
        post_json(&router, "PUT", &format!("/oagw/v1/upstreams/{id}"), replacement).await;
    assert_eq!(status, 200, "{put}");
    assert_eq!(put["tags"], json!(["edge"]));

    let (status, _) = post_json(&router, "DELETE", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;
    assert_eq!(status, 204);
    let (status, _) = post_json(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn missing_upstream_is_a_404_problem() {
    let router = gw().await;
    let (status, body) = post_json(&router, "GET", "/oagw/v1/upstreams/00000000-0000-0000-0000-00000000000f", Value::Null).await;
    assert_eq!(status, 404);
    assert!(body["type"].as_str().unwrap().contains("not_found"), "{body}");
    assert_eq!(body["status"], json!(404));
}

#[tokio::test]
async fn tenants_cannot_read_each_others_upstreams() {
    let router = gw().await;
    let id = create_upstream(&router, upstream_spec("mine", "127.0.0.1")).await;
    let response = router
        .clone()
        .oneshot(support::request("GET", &format!("/oagw/v1/upstreams/{id}"), None, TENANT_B))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

#[tokio::test]
async fn list_projects_and_paginates() {
    let router = gw().await;
    create_upstream(&router, upstream_spec("list-one", "127.0.0.1")).await;
    create_upstream(&router, upstream_spec("list-two", "127.0.0.2")).await;

    let (status, all) = post_json(&router, "GET", "/oagw/v1/upstreams", Value::Null).await;
    assert_eq!(status, 200);
    assert!(all.as_array().unwrap().len() >= 2);

    let response = router
        .clone()
        .oneshot(support::request(
            "GET",
            "/oagw/v1/upstreams?$select=alias&$top=1&$orderby=alias desc",
            None,
            TENANT_A,
        ))
        .await
        .unwrap();
    let body = support::json_of(response).await;
    let first = &body.as_array().unwrap()[0];
    assert_eq!(first.as_object().unwrap().len(), 1, "select projects to one field");
}

#[tokio::test]
async fn malformed_odata_is_a_400() {
    let router = gw().await;
    let response = router
        .clone()
        .oneshot(support::request("GET", "/oagw/v1/upstreams?$filter=nonsense eq", None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
}

#[tokio::test]
async fn alias_is_immutable_across_endpoint_changes() {
    let router = gw().await;
    let spec = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com"}]},
        "protocol": PROTOCOL,
    });
    let id = create_upstream(&router, spec).await;
    let (_, got) = post_json(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;
    assert_eq!(got["alias"], json!("api.vendor.com"));

    let mut moved = got.clone();
    moved["server"]["endpoints"][0]["host"] = json!("eu.vendor.com");
    let (status, body) = post_json(&router, "PUT", &format!("/oagw/v1/upstreams/{id}"), moved).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("alias"), "{body}");
}

#[tokio::test]
async fn an_ip_pool_may_move_its_endpoints() {
    // An IP-literal pool has no derived alias to violate: the operator chose
    // the name, and endpoint churn does not change it.
    let router = gw().await;
    let id = create_upstream(&router, upstream_spec("pinned", "127.0.0.1")).await;
    let (_, got) = post_json(&router, "GET", &format!("/oagw/v1/upstreams/{id}"), Value::Null).await;

    let mut moved = got.clone();
    moved["server"]["endpoints"][0]["host"] = json!("127.0.0.9");
    let (status, body) = post_json(&router, "PUT", &format!("/oagw/v1/upstreams/{id}"), moved).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["alias"], json!("pinned"));
}

// ---------------------------------------------------------------------- routes

async fn create_route(router: &Router, upstream: &str, path: &str) -> Value {
    let spec = json!({
        "upstream_id": upstream,
        "match": {"http": {"methods": ["GET"], "path": path, "query_allowlist": []}},
    });
    let (status, body) = post_json(router, "POST", "/oagw/v1/routes", spec).await;
    assert_eq!(status, 201, "{body}");
    body
}

#[tokio::test]
async fn route_crud_round_trip() {
    let router = gw().await;
    let upstream = create_upstream(&router, upstream_spec("routed", "127.0.0.1")).await;
    let route = create_route(&router, &upstream, "/v1").await;
    assert!(route["id"].as_str().unwrap().starts_with("gts.cf.core.oagw.route.v1~"));

    let (status, _) = post_json(&router, "DELETE", &format!("/oagw/v1/routes/{}", route["id"].as_str().unwrap()), Value::Null).await;
    assert_eq!(status, 204);

    // Deleting the upstream removes its routes with it.
    let (status, _) = post_json(&router, "DELETE", &format!("/oagw/v1/upstreams/{upstream}"), Value::Null).await;
    assert_eq!(status, 204);
    let (_, remaining) = post_json(&router, "GET", "/oagw/v1/routes", Value::Null).await;
    assert!(remaining.as_array().unwrap().is_empty(), "{remaining}");
}

#[tokio::test]
async fn route_for_a_foreign_upstream_is_rejected() {
    let router = gw().await;
    let upstream = create_upstream(&router, upstream_spec("foreign", "127.0.0.1")).await;
    let response = router
        .clone()
        .oneshot(support::request(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream,
                "match": {"http": {"methods": ["GET"], "path": "/"}},
            })),
            TENANT_B,
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404, "the upstream is invisible to tenant B");
}

#[tokio::test]
async fn colliding_match_rules_conflict() {
    let router = gw().await;
    let upstream = create_upstream(&router, upstream_spec("clash", "127.0.0.1")).await;
    create_route(&router, &upstream, "/v1").await;
    let (status, body) = post_json(
        &router,
        "POST",
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream,
            "match": {"http": {"methods": ["GET"], "path": "/v1", "query_allowlist": []}},
        }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
}

#[tokio::test]
async fn a_route_without_a_method_is_rejected() {
    let router = gw().await;
    let upstream = create_upstream(&router, upstream_spec("nomethod", "127.0.0.1")).await;
    let (status, _) = post_json(
        &router,
        "POST",
        "/oagw/v1/routes",
        json!({"upstream_id": upstream, "match": {"http": {"methods": [], "path": "/"}}}),
    )
    .await;
    assert_eq!(status, 400);
}

// --------------------------------------------------------------------- plugins

#[tokio::test]
async fn plugin_crud_and_source_endpoint() {
    let router = gw().await;
    let spec = json!({
        "plugin_type": "guard",
        "name": "require-tenant-header",
        "source_code": "def guard_request(ctx):\n    return None\n",
        "tags": [],
    });
    let (status, plugin) = post_json(&router, "POST", "/oagw/v1/plugins", spec).await;
    assert_eq!(status, 201, "{plugin}");
    let id = plugin["id"].as_str().unwrap();

    let response = router
        .clone()
        .oneshot(support::request("GET", &format!("/oagw/v1/plugins/{id}/source"), None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/x-starlark; charset=utf-8"
    );
    let source = support::text_of(response).await;
    assert!(source.starts_with("def guard_request"), "{source}");

    let (status, _) = post_json(&router, "DELETE", &format!("/oagw/v1/plugins/{id}"), Value::Null).await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn a_plugin_in_use_cannot_be_deleted() {
    let router = gw().await;
    let (_, plugin) = post_json(
        &router,
        "POST",
        "/oagw/v1/plugins",
        json!({"plugin_type": "guard", "name": "bound", "source_code": "x", "tags": []}),
    )
    .await;
    let plugin = plugin["id"].as_str().unwrap().to_owned();
    let upstream = create_upstream(&router, upstream_spec("bound-upstream", "127.0.0.1")).await;
    let (status, _) = post_json(
        &router,
        "PUT",
        &format!("/oagw/v1/upstreams/{upstream}"),
        json!({
            "alias": "bound-upstream",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 8081}]},
            "protocol": PROTOCOL,
            "plugins": {"items": [plugin]},
        }),
    )
    .await;
    assert_eq!(status, 200, "the binding is accepted");

    let response = router
        .clone()
        .oneshot(support::request("DELETE", &format!("/oagw/v1/plugins/{plugin}"), None, TENANT_A))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
}
