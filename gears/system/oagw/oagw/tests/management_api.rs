//! Management-API behaviour: CRUD, validation, conflicts and tenant scoping.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Harness, OTHER_TENANT, TENANT, assert_problem, context_for, json, tenant_context};
use serde_json::{Value, json};

fn upstream_body(alias: Option<&str>, host: &str, port: Option<u16>) -> Value {
    let mut body = json!({
        "enabled": true,
        "server": { "endpoints": [
            { "scheme": "https", "host": host, "port": port }
        ]}
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

fn route_body(upstream_id: &str, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } }
    })
}

#[tokio::test]
async fn create_upstream_derives_the_alias_from_the_endpoint_pool() {
    let harness = Harness::new();
    let mut response = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(None, "api.openai.com", None),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json(&mut response).await;
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(
        body["protocol"],
        oagw::domain::gts_helpers::PROTOCOL_HTTP_ID
    );
    assert_eq!(body["server"]["endpoints"][0]["scheme"], "https");
    assert!(body["id"].as_str().is_some());
    assert!(body["created_at"].as_i64().is_some());
}

#[tokio::test]
async fn create_upstream_requires_an_alias_when_the_pool_is_not_derivable() {
    let harness = Harness::new();
    let mut response = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(None, "10.0.0.9", Some(8080)),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn create_upstream_rejects_an_unknown_scheme_and_a_bad_host() {
    let harness = Harness::new();

    let mut response = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "server": { "endpoints": [
                    { "scheme": "gopher", "host": "api.example", "port": 443 }
                ]}
            }),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("unknown endpoint scheme 'gopher'"))
    );

    // `http` is a legal wire scheme (documented override), so it must be accepted.
    let response = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "server": { "endpoints": [
                    { "scheme": "http", "host": "api.example", "port": 80 }
                ]}
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut response = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            json!({
                "server": { "endpoints": [
                    { "scheme": "https", "host": "-bad.example", "port": 443 }
                ]}
            }),
        )
        .await;
    assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn duplicate_aliases_conflict_and_are_case_insensitive() {
    let harness = Harness::new();
    let first = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("vendor.example"), "10.0.0.9", None),
        )
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let mut second = harness
        .json(
            tenant_context(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("VENDOR.EXAMPLE."), "10.0.0.10", None),
        )
        .await;
    let body = assert_problem(&mut second, StatusCode::CONFLICT).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1"
    );
}

#[tokio::test]
async fn upstream_crud_round_trip() {
    let harness = Harness::new();
    let ctx = tenant_context();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("vendor.example"), "10.0.0.9", None),
        )
        .await;
    let created = json(&mut response).await;
    let id = created["id"].as_str().expect("id").to_owned();

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams", &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = json(&mut response).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams?alias=vendor.example", &[]),
        )
        .await;
    let filtered = json(&mut response).await;
    assert_eq!(filtered.as_array().map(Vec::len), Some(1));

    let mut response = harness
        .json(
            ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            json!({
                "alias": "vendor.example",
                "enabled": false,
                "tags": ["prod"],
                "server": { "endpoints": [
                    { "scheme": "https", "host": "10.0.0.9", "port": 8443 }
                ]}
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = json(&mut response).await;
    assert_eq!(replaced["enabled"], false);
    assert_eq!(replaced["alias"], "vendor.example");
    assert_eq!(replaced["server"]["endpoints"][0]["port"], 8443);

    let mut response = harness
        .json(
            ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            json!({
                "alias": "other.example",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "10.0.0.9", "port": 8443 }
                ]}
            }),
        )
        .await;
    assert_problem(&mut response, StatusCode::BAD_REQUEST).await;

    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::DELETE, &format!("/oagw/v1/upstreams/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, &format!("/oagw/v1/upstreams/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_crud_round_trip_with_conflict_and_unknown_upstream() {
    let harness = Harness::new();
    let ctx = tenant_context();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("vendor.example"), "10.0.0.9", None),
        )
        .await;
    let upstream = json(&mut response).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            route_body(&upstream_id, "/v1/pets"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let route = json(&mut response).await;
    let route_id = route["id"].as_str().expect("id").to_owned();
    assert_eq!(route["upstream_id"], Value::String(upstream_id.clone()));
    assert_eq!(route["match"]["http"]["path"], "/v1/pets");

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            route_body(&upstream_id, "/v1/pets"),
        )
        .await;
    assert_problem(&mut response, StatusCode::CONFLICT).await;

    let response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": "00000000-0000-0000-0000-00000000000f",
                "match": { "http": { "methods": ["GET"], "path": "/v1/other" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/routes",
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["get"], "path": "v1/bad" } }
            }),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::BAD_REQUEST).await;
    assert!(body["detail"].as_str().is_some());

    let mut response = harness
        .json(
            ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/routes/{route_id}"),
            json!({
                "upstream_id": upstream_id,
                "enabled": false,
                "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/pets" } }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = json(&mut response).await;
    assert_eq!(replaced["enabled"], false);
    assert_eq!(replaced["match"]["http"]["methods"], json!(["GET", "POST"]));

    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, &format!("/oagw/v1/routes/{route_id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::DELETE, &format!("/oagw/v1/routes/{route_id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Deleting the upstream now succeeds because nothing references it.
    let response = harness
        .send(
            ctx.clone(),
            common::request(
                Method::DELETE,
                &format!("/oagw/v1/upstreams/{upstream_id}"),
                &[],
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_crud_and_source_endpoint() {
    let harness = Harness::new();
    let ctx = tenant_context();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/plugins",
            json!({ "name": "partner-guard", "plugin_type": "guard", "source": "def guard(req):\n    return None\n" }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let plugin = json(&mut response).await;
    let id = plugin["id"].as_str().expect("id").to_owned();
    assert_eq!(plugin["plugin_type"], "guard");

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, &format!("/oagw/v1/plugins/{id}/source"), &[]),
        )
        .await;
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/x-python; charset=utf-8")
    );
    let source = common::text(&mut response).await;
    assert!(source.contains("def guard"));

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/plugins",
            json!({ "name": " ", "plugin_type": "guard" }),
        )
        .await;
    assert_problem(&mut response, StatusCode::BAD_REQUEST).await;

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/plugins?plugin_type=guard", &[]),
        )
        .await;
    let listed = json(&mut response).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::DELETE, &format!("/oagw/v1/plugins/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = harness
        .send(
            ctx.clone(),
            common::request(Method::DELETE, &format!("/oagw/v1/plugins/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_delete_is_refused_while_bound() {
    let harness = Harness::new();
    let ctx = tenant_context();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("vendor.example"), "10.0.0.9", None),
        )
        .await;
    let upstream = json(&mut response).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let mut response = harness
        .json(
            ctx.clone(),
            Method::POST,
            "/oagw/v1/plugins",
            json!({ "name": "partner-guard", "plugin_type": "guard" }),
        )
        .await;
    let plugin = json(&mut response).await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let response = harness
        .json(
            ctx.clone(),
            Method::PUT,
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            json!({
                "alias": "vendor.example",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "10.0.0.9", "port": 443 }
                ]},
                "plugins": { "items": [ { "id": plugin_id } ] }
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(
                Method::DELETE,
                &format!("/oagw/v1/plugins/{plugin_id}"),
                &[],
            ),
        )
        .await;
    let body = assert_problem(&mut response, StatusCode::CONFLICT).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
}

#[tokio::test]
async fn tenants_are_isolated_across_the_management_api() {
    let harness = Harness::new();
    let owner = context_for(TENANT);

    let mut response = harness
        .json(
            owner.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            upstream_body(Some("vendor.example"), "10.0.0.9", None),
        )
        .await;
    let created = json(&mut response).await;
    let id = created["id"].as_str().expect("id").to_owned();

    let stranger = context_for(OTHER_TENANT);
    let response = harness
        .send(
            stranger.clone(),
            common::request(Method::GET, &format!("/oagw/v1/upstreams/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let mut response = harness
        .send(
            stranger.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams", &[]),
        )
        .await;
    let listed = json(&mut response).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(0));

    let response = harness
        .send(
            stranger.clone(),
            common::request(Method::DELETE, &format!("/oagw/v1/upstreams/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = harness
        .json(
            stranger.clone(),
            Method::POST,
            "/oagw/v1/routes",
            route_body(&id, "/v1/pets"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The owner still sees the upstream.
    let response = harness
        .send(
            owner.clone(),
            common::request(Method::GET, &format!("/oagw/v1/upstreams/{id}"), &[]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn list_endpoints_honor_pagination() {
    let harness = Harness::new();
    let ctx = tenant_context();

    for index in 0..3 {
        let response = harness
            .json(
                ctx.clone(),
                Method::POST,
                "/oagw/v1/upstreams",
                upstream_body(
                    Some(&format!("vendor{index}.example")),
                    "10.0.0.9",
                    Some(index as u16 + 8000),
                ),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams?top=2", &[]),
        )
        .await;
    let page = json(&mut response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(2));

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams?skip=2&top=10", &[]),
        )
        .await;
    let tail = json(&mut response).await;
    assert_eq!(tail.as_array().map(Vec::len), Some(1));

    let mut response = harness
        .send(
            ctx.clone(),
            common::request(Method::GET, "/oagw/v1/upstreams?alias=vendor1.example", &[]),
        )
        .await;
    let filtered = json(&mut response).await;
    assert_eq!(filtered[0]["alias"], "vendor1.example");
}

#[tokio::test]
async fn missing_resources_render_the_oagw_problem_type() {
    let harness = Harness::new();
    let missing = uuid::Uuid::new_v4();

    for path in [
        format!("/oagw/v1/upstreams/{missing}"),
        format!("/oagw/v1/routes/{missing}"),
        format!("/oagw/v1/plugins/{missing}"),
    ] {
        let mut response = harness
            .send(tenant_context(), common::request(Method::GET, &path, &[]))
            .await;
        let body = assert_problem(&mut response, StatusCode::NOT_FOUND).await;
        assert_eq!(
            body["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "404 surfaces the route-not-found problem type"
        );
    }
}
