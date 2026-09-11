//! HTTP tests for the OAGW REST surface: status codes, the RFC 9457 problem
//! envelope, the ADR-0007 error-source header and the alias rules end to end.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::control_plane::{
    ControlPlane, REASON_PLUGIN_IN_USE, REASON_PLUGIN_NAME_CONFLICT, REASON_ROUTE_MATCH_CONFLICT,
};
use crate::domain::model::{ROUTE_ID_PREFIX, UPSTREAM_ID_PREFIX, route_gts_id, upstream_gts_id};
use crate::error::{
    CONFLICT_TYPE, ERROR_SOURCE_HEADER, GATEWAY_ERROR_SOURCE, INTERNAL_ERROR_TYPE,
    INVALID_TARGET_HOST_TYPE, MISSING_TARGET_HOST_TYPE, PAYLOAD_TOO_LARGE_TYPE,
    ROUTE_NOT_FOUND_TYPE, UNKNOWN_TARGET_HOST_TYPE, VALIDATION_ERROR_TYPE,
};

use super::register_routes;

// ── Harness ──────────────────────────────────────────────────────────────────

fn tenant_a() -> Uuid {
    Uuid::from_u128(0xA1A1)
}

fn tenant_b() -> Uuid {
    Uuid::from_u128(0xB2B2)
}

fn ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xFEED))
        .subject_tenant_id(tenant)
        .build()
        .expect("test security context")
}

fn build_router_with(config: OagwConfig) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(
        Router::new(),
        &openapi,
        Arc::new(ControlPlane::new()),
        config,
    )
}

fn build_router() -> Router {
    build_router_with(OagwConfig::default())
}

/// Build a request with the `SecurityContext` injected as an extension, the way
/// the host middleware does.
fn request(method: &str, uri: &str, body: Option<Value>, ctx: SecurityContext) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(document) => Body::from(serde_json::to_vec(&document).unwrap()),
        None => Body::empty(),
    };
    let mut request = builder.body(body).unwrap();
    request.extensions_mut().insert(ctx);
    request
}

async fn send(router: Router, request: Request<Body>) -> Response {
    router.oneshot(request).await.unwrap()
}

async fn body_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// A minimal valid upstream document: `alias` is only present when given.
fn upstream_document(host: &str, port: u16, alias: Option<&str>) -> Value {
    let mut document = json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": host, "port": port }]
        },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    if let Some(alias) = alias {
        document["alias"] = json!(alias);
    }
    document
}

/// Create one upstream and return its document as echoed by the gateway.
async fn create(router: &Router, ctx: SecurityContext, document: Value) -> (StatusCode, Value) {
    let response = send(
        router.clone(),
        request("POST", "/oagw/v1/upstreams", Some(document), ctx),
    )
    .await;
    let status = response.status();
    (status, body_json(response).await)
}

// ── Creation ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_201_with_the_gts_identifier_and_location() {
    let router = build_router();
    let response = send(
        router,
        request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_document("api.openai.com", 443, None)),
            ctx(tenant_a()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let body = body_json(response).await;
    let id = body["id"].as_str().expect("id member").to_owned();
    assert!(id.starts_with(UPSTREAM_ID_PREFIX), "unexpected id {id}");
    assert_eq!(location, format!("/oagw/v1/upstreams/{id}"));
    // The alias was derived from the endpoint host.
    assert_eq!(body["alias"], json!("api.openai.com"));
    // Members absent from the request keep their documented defaults.
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["tags"], json!([]));
}

#[tokio::test]
async fn every_oagw_response_carries_the_gateway_error_source() {
    let router = build_router();
    let document = upstream_document("grid.example.com", 443, None);

    let created = send(
        router.clone(),
        request(
            "POST",
            "/oagw/v1/upstreams",
            Some(document.clone()),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );
    let id = body_json(created).await["id"].as_str().unwrap().to_owned();

    let listed = send(
        router.clone(),
        request("GET", "/oagw/v1/upstreams", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(
        listed.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );

    let fetched = send(
        router.clone(),
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(
        fetched.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );

    let replaced = send(
        router.clone(),
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(document),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(replaced.status(), StatusCode::OK);
    assert_eq!(
        replaced.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );

    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        deleted.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );

    let missing = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        GATEWAY_ERROR_SOURCE
    );
}

#[tokio::test]
async fn create_derives_the_alias_from_the_endpoints() {
    let router = build_router();
    let (_, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("Vendor.Example.COM", 8443, None),
    )
    .await;
    // The non-standard port is part of the routing alias, and the derived
    // value is normalized to ASCII lowercase.
    assert_eq!(body["alias"], json!("vendor.example.com:8443"));
}

#[tokio::test]
async fn create_rejects_an_alias_that_disagrees_with_the_derivation() {
    let router = build_router();
    let (status, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, Some("api.anthropic.com")),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(body["status"], json!(400));
    assert_eq!(body["instance"], json!("/oagw/v1/upstreams"));
    assert_eq!(body["invalid_value"], json!("api.anthropic.com"));
    assert!(body["detail"].as_str().unwrap().contains("alias"));
}

#[tokio::test]
async fn create_requires_an_alias_for_ip_endpoints() {
    let router = build_router();
    let (status, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("10.0.0.7", 8443, None),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert!(body["detail"].as_str().unwrap().contains("alias"));
}

#[tokio::test]
async fn create_rejects_a_duplicate_alias_with_409() {
    let router = build_router();
    let document = upstream_document("api.openai.com", 443, None);
    let (first, _) = create(&router, ctx(tenant_a()), document.clone()).await;
    assert_eq!(first, StatusCode::CREATED);

    let (status, body) = create(&router, ctx(tenant_a()), document).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["type"], json!(CONFLICT_TYPE));
    assert_eq!(body["status"], json!(409));
    assert_eq!(body["reason"], json!("ALIAS_CONFLICT"));
    assert_eq!(body["alias"], json!("api.openai.com"));
}

#[tokio::test]
async fn create_rejects_a_schema_violation_with_a_problem_document() {
    let router = build_router();
    let response = send(
        router,
        request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "tier": "gold"
            })),
            ctx(tenant_a()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = body_json(response).await;
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(body["title"], json!("Validation Error"));
    assert_eq!(body["status"], json!(400));
    assert!(body["detail"].as_str().unwrap().contains("tier"));
}

#[tokio::test]
async fn create_rejects_a_document_without_a_server() {
    let router = build_router();
    let (status, body) = create(
        &router,
        ctx(tenant_a()),
        json!({ "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
}

#[tokio::test]
async fn create_rejects_an_out_of_range_port() {
    let router = build_router();
    let (status, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 0, None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
}

#[tokio::test]
async fn create_rejects_an_empty_body() {
    let router = build_router();
    let response = send(
        router,
        request("POST", "/oagw/v1/upstreams", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(body["instance"], json!("/oagw/v1/upstreams"));
}

// ── Reads ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_accepts_both_identifier_forms() {
    let router = build_router();
    let (_, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();
    let uuid = id.trim_start_matches(UPSTREAM_ID_PREFIX).to_owned();

    for reference in [uuid.clone(), id.clone()] {
        let response = send(
            router.clone(),
            request(
                "GET",
                &format!("/oagw/v1/upstreams/{reference}"),
                None,
                ctx(tenant_a()),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "reference {reference}");
        let fetched = body_json(response).await;
        assert_eq!(fetched["id"], json!(id), "the GTS form is always echoed");
        assert_eq!(fetched["alias"], json!("api.openai.com"));
    }
}

#[tokio::test]
async fn get_is_tenant_scoped() {
    let router = build_router();
    let (_, body) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();

    let response = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_b()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let problem = body_json(response).await;
    assert_eq!(problem["type"], json!(ROUTE_NOT_FOUND_TYPE));
    assert_eq!(problem["title"], json!("Not Found"));
    assert_eq!(problem["status"], json!(404));
    assert_eq!(problem["upstream_id"], json!(id));
    assert_eq!(
        problem["instance"],
        json!(format!("/oagw/v1/upstreams/{id}"))
    );
}

#[tokio::test]
async fn get_rejects_a_malformed_identifier() {
    let router = build_router();
    let response = send(
        router,
        request(
            "GET",
            "/oagw/v1/upstreams/not-a-uuid",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
}

#[tokio::test]
async fn list_is_ordered_tenant_scoped_and_paginated() {
    let router = build_router();
    for host in ["zeta.example.com", "alpha.example.com", "mid.example.com"] {
        let (status, _) =
            create(&router, ctx(tenant_a()), upstream_document(host, 443, None)).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, _) = create(
        &router,
        ctx(tenant_b()),
        upstream_document("foreign.example.com", 443, None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router.clone(),
        request("GET", "/oagw/v1/upstreams", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let all = body_json(response).await;
    let aliases: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["alias"].as_str().unwrap())
        .collect();
    assert_eq!(
        aliases,
        vec!["alpha.example.com", "mid.example.com", "zeta.example.com"]
    );

    let page = send(
        router,
        request(
            "GET",
            "/oagw/v1/upstreams?$top=1&$skip=1",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    let body = body_json(page).await;
    assert_eq!(
        body.as_array()
            .unwrap()
            .iter()
            .map(|item| item["alias"].clone())
            .collect::<Vec<_>>(),
        vec![json!("mid.example.com")]
    );
}

/// `$select` and `$orderby` are not implemented: they are dropped, so the list
/// returns the full representation in the documented order, and a `$top` that is
/// not a non-negative integer falls back to the default page size.
#[tokio::test]
async fn list_drops_select_and_orderby_and_repairs_an_unusable_top() {
    let router = build_router();
    let (status, _) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router,
        request(
            "GET",
            "/oagw/v1/upstreams?$select=alias&$orderby=alias%20desc&$top=nope",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_json(response).await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["alias"], json!("api.openai.com"));
    assert!(page[0]["server"].is_object(), "the full shape is returned");
}

#[tokio::test]
async fn list_evaluates_a_filter_clause() {
    let router = build_router();
    for (alias, enabled) in [("a.example.com", true), ("b.example.com", false)] {
        let mut document = upstream_document(alias, 443, None);
        document["enabled"] = json!(enabled);
        let (status, _) = create(&router, ctx(tenant_a()), document).await;
        assert_eq!(status, StatusCode::CREATED, "{alias}");
    }

    let matching = send(
        router.clone(),
        request(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20%27b.example.com%27",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(matching.status(), StatusCode::OK);
    let page = body_json(matching).await;
    let aliases: Vec<_> = page
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["alias"].clone())
        .collect();
    assert_eq!(aliases, vec![json!("b.example.com")]);

    // A clause that no document satisfies is an empty page, not an error.
    let empty = send(
        router.clone(),
        request(
            "GET",
            "/oagw/v1/upstreams?$filter=enabled%20eq%20%27true%27",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert!(body_json(empty).await.as_array().unwrap().is_empty());
}

/// An operator the list endpoints do not evaluate is reported rather than
/// silently returning the unfiltered collection.
#[tokio::test]
async fn an_unsupported_filter_operator_is_a_400() {
    let router = build_router();
    let (status, _) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router,
        request(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20ne%20%27x%27",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["type"],
        json!(VALIDATION_ERROR_TYPE)
    );
}

// ── Replacement ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_replaces_the_document_and_keeps_the_alias() {
    let router = build_router();
    let (_, created) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let mut replacement = upstream_document("api.openai.com", 443, None);
    replacement["enabled"] = json!(false);
    replacement["tags"] = json!(["paid"]);

    let response = send(
        router.clone(),
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(replacement),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["alias"], json!("api.openai.com"));
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["tags"], json!(["paid"]));

    // The replacement is durable.
    let fetched = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(body_json(fetched).await["enabled"], json!(false));
}

#[tokio::test]
async fn put_rejects_an_endpoint_change_that_would_move_the_alias() {
    let router = build_router();
    let (_, created) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let response = send(
        router,
        request(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(upstream_document("api.anthropic.com", 443, None)),
            ctx(tenant_a()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(body["alias"], json!("api.openai.com"));
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("delete and re-create"),
        "{}",
        body["detail"]
    );
}

#[tokio::test]
async fn put_never_creates() {
    let router = build_router();
    let response = send(
        router,
        request(
            "PUT",
            &format!(
                "/oagw/v1/upstreams/{}",
                upstream_gts_id(Uuid::from_u128(0x99))
            ),
            Some(upstream_document("api.openai.com", 443, None)),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await["type"],
        json!(ROUTE_NOT_FOUND_TYPE)
    );
}

// ── Deletion ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_returns_204_and_frees_the_alias() {
    let router = build_router();
    let (_, created) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(deleted.into_body(), 16).await.unwrap().is_empty());

    let again = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);

    // The alias is reusable after the delete.
    let (status, _) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// Deleting an upstream takes its routes with it: the route list no longer
/// names them and each route id reads as a 404.
#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let first = post_route(
        &router,
        ctx(tenant_a()),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "/a" } }
        }),
    )
    .await
    .1;
    let second = post_route(
        &router,
        ctx(tenant_a()),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["POST"], "path": "/b" } }
        }),
    )
    .await
    .1;
    let first_id = first["id"].as_str().unwrap().to_owned();
    let second_id = second["id"].as_str().unwrap().to_owned();

    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let listed = send(
        router.clone(),
        request("GET", "/oagw/v1/routes", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    assert!(
        body_json(listed).await.as_array().unwrap().is_empty(),
        "the routes of the deleted upstream are gone"
    );

    for id in [first_id, second_id] {
        let gone = send(
            router.clone(),
            request(
                "GET",
                &format!("/oagw/v1/routes/{id}"),
                None,
                ctx(tenant_a()),
            ),
        )
        .await;
        assert_eq!(gone.status(), StatusCode::NOT_FOUND, "{id}");
    }
}

/// A management body past the configured limit is a 413 raised while the body
/// is still streaming, before the document is parsed.
#[tokio::test]
async fn an_oversized_management_body_is_a_413() {
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(
        Router::new(),
        &openapi,
        Arc::new(ControlPlane::new()),
        OagwConfig {
            body_limit_bytes: 64,
            ..OagwConfig::default()
        },
    );
    // Far past the 64-byte limit, and valid JSON if it were read at all.
    let document = json!({ "alias": "x", "tags": vec!["tag"; 64] });
    let response = send(
        router,
        request(
            "POST",
            "/oagw/v1/upstreams",
            Some(document),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body_json(response).await["type"],
        json!(PAYLOAD_TOO_LARGE_TYPE)
    );
}

#[tokio::test]
async fn delete_is_tenant_scoped() {
    let router = build_router();
    let (_, created) = create(
        &router,
        ctx(tenant_a()),
        upstream_document("api.openai.com", 443, None),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let response = send(
        router,
        request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            ctx(tenant_b()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await["type"],
        json!(ROUTE_NOT_FOUND_TYPE)
    );
}

// ── S2 surfaces ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_route_plugin_and_proxy_surfaces_are_registered() {
    // S2 owns `/oagw/v1/routes`, `/oagw/v1/plugins` and `/oagw/v1/proxy/{alias}`;
    // every one of them answers through an OAGW handler, so even a rejection
    // carries the ADR-0007 error-source header.
    let router = build_router();
    for (method, path, expected) in [
        (
            "POST",
            "/oagw/v1/routes".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            format!("/oagw/v1/routes/{}", route_gts_id(Uuid::from_u128(0x606))),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            format!("/oagw/v1/plugins/{}", Uuid::from_u128(0x607)),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/oagw/v1/proxy/unknown-alias.test".to_owned(),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = send(
            router.clone(),
            request(method, &path, None, ctx(tenant_a())),
        )
        .await;
        assert_eq!(response.status(), expected, "{method} {path}");
        assert_eq!(
            response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
            GATEWAY_ERROR_SOURCE,
            "{method} {path}"
        );
        let problem = body_json(response).await;
        assert_eq!(problem["instance"], json!(path));
    }
}

#[tokio::test]
async fn there_is_deliberately_no_put_on_a_plugin() {
    // A custom plugin is immutable (DESIGN "Plugin Immutability"): the route is
    // simply not registered, so axum answers 405 rather than an OAGW problem.
    let router = build_router();
    let response = send(
        router,
        request(
            "PUT",
            &format!("/oagw/v1/plugins/{}", Uuid::from_u128(0x5A5)),
            Some(json!({ "name": "replacement" })),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(response.headers().get(ERROR_SOURCE_HEADER).is_none());
}

// ── Error envelope ───────────────────────────────────────────────────────────

#[tokio::test]
async fn error_envelope_matches_the_documented_gts_types() {
    let router = build_router();

    let unknown = upstream_gts_id(Uuid::from_u128(0x3C3));
    let cases = [
        (
            request(
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({ "unexpected": true })),
                ctx(tenant_a()),
            ),
            (StatusCode::BAD_REQUEST, VALIDATION_ERROR_TYPE),
        ),
        (
            request("GET", "/oagw/v1/upstreams/zzz", None, ctx(tenant_a())),
            (StatusCode::BAD_REQUEST, VALIDATION_ERROR_TYPE),
        ),
        (
            request(
                "GET",
                &format!("/oagw/v1/upstreams/{unknown}"),
                None,
                ctx(tenant_a()),
            ),
            (StatusCode::NOT_FOUND, ROUTE_NOT_FOUND_TYPE),
        ),
    ];

    for (request, (status, expected)) in cases {
        let response = send(router.clone(), request).await;
        assert_eq!(response.status(), status);
        let body = body_json(response).await;
        assert_eq!(body["type"], json!(expected), "body {body}");
        assert_eq!(body["status"], json!(status.as_u16()));
        assert!(body["title"].is_string());
        assert!(body["detail"].is_string());
        assert!(body["instance"].is_string());
    }
}

#[test]
fn the_proxy_operations_declare_every_status_the_data_plane_can_return() {
    let openapi = OpenApiRegistryImpl::new();
    let plane = Arc::new(ControlPlane::new());
    let config = OagwConfig::default();
    let router = Router::new();
    let _router = register_routes(router, &openapi, plane, config);

    let key = |method: &str| format!("{method}:/oagw/v1/proxy/{{alias}}");
    for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        let spec = openapi
            .operation_specs
            .get(&key(method))
            .unwrap_or_else(|| panic!("the {method} proxy operation must be registered"));
        let statuses: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
        for status in [400, 401, 403, 404, 413, 429, 500, 502, 503, 504] {
            assert!(
                statuses.contains(&status),
                "the {method} proxy operation must declare {status}, declared {statuses:?}"
            );
        }
        // `OPTIONS` is the CORS preflight: it never reaches the upstream, so
        // the transport rejections are not part of its contract.
        if method != "OPTIONS" {
            for status in [502, 504] {
                assert!(statuses.contains(&status));
            }
        }
    }
}

#[tokio::test]
async fn error_types_stay_inside_the_documented_gts_vocabulary() {
    // The documented vocabulary is exhaustive: every constant must be a
    // `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1` identifier.
    for gts_type in [
        VALIDATION_ERROR_TYPE,
        INVALID_TARGET_HOST_TYPE,
        MISSING_TARGET_HOST_TYPE,
        UNKNOWN_TARGET_HOST_TYPE,
        PAYLOAD_TOO_LARGE_TYPE,
        ROUTE_NOT_FOUND_TYPE,
        CONFLICT_TYPE,
        INTERNAL_ERROR_TYPE,
    ] {
        let (head, tail) = gts_type.split_once('~').unwrap();
        assert_eq!(head, "gts.cf.core.errors.err.v1");
        let name = tail.strip_suffix(".v1").unwrap_or(tail);
        assert_ne!(name, tail, "the type must end in `.v1`");
        assert!(name.starts_with("cf.oagw."));
    }
}

// ── Route CRUD ───────────────────────────────────────────────────────────────

/// A minimal valid route document for `upstream_id`.
fn route_document(upstream_id: &str, methods: &[&str], path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "tags": ["api"],
        "match": { "http": { "methods": methods, "path": path } }
    })
}

/// Create an upstream of `tenant` and return its GTS identifier.
async fn create_upstream_id(router: &Router, tenant: Uuid, host: &str, port: u16) -> String {
    let (status, body) = create(router, ctx(tenant), upstream_document(host, port, None)).await;
    assert_eq!(status, StatusCode::CREATED, "the test upstream must exist");
    body["id"].as_str().unwrap().to_owned()
}

/// Create a route and return `(status, body)`.
async fn post_route(router: &Router, ctx: SecurityContext, document: Value) -> (StatusCode, Value) {
    let response = send(
        router.clone(),
        request("POST", "/oagw/v1/routes", Some(document), ctx),
    )
    .await;
    let status = response.status();
    (status, body_json(response).await)
}

/// A custom (Starlark) plugin document; `source` is absent for a named plugin.
fn plugin_document(name: &str, kind: &str, source: Option<&str>) -> Value {
    let mut document = json!({
        "name": name,
        "kind": kind,
        "phases": ["on_request"],
        "config": { "header": "x-tenant" },
    });
    if let Some(source) = source {
        document["source"] = json!(source);
    }
    document
}

/// Create a plugin and return `(status, body)`.
async fn post_plugin(
    router: &Router,
    ctx: SecurityContext,
    document: Value,
) -> (StatusCode, Value) {
    let response = send(
        router.clone(),
        request("POST", "/oagw/v1/plugins", Some(document), ctx),
    )
    .await;
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn create_route_returns_201_with_the_gts_identifier_and_location() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;

    let response = send(
        router.clone(),
        request(
            "POST",
            "/oagw/v1/routes",
            Some(route_document(&upstream, &["GET"], "/v1/chat")),
            ctx(tenant_a()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let body = body_json(response).await;
    let id = body["id"].as_str().expect("id member").to_owned();
    assert!(id.starts_with(ROUTE_ID_PREFIX), "unexpected id {id}");
    assert_eq!(location, format!("/oagw/v1/routes/{id}"));
    assert_eq!(body["upstream_id"], json!(upstream));
    // Members absent from the request keep their documented defaults.
    assert_eq!(body["match"]["http"]["path_suffix_mode"], json!("append"));
    assert_eq!(body["match"]["http"]["query_allowlist"], json!([]));
    assert_eq!(body["tags"], json!(["api"]));
}

#[tokio::test]
async fn create_route_rejects_an_unknown_or_foreign_upstream_reference() {
    let router = build_router();
    let foreign = create_upstream_id(&router, tenant_b(), "foreign.example.com", 443).await;

    // An upstream that does not exist at all.
    let (status, body) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream_gts_id(Uuid::from_u128(0x777)), &["GET"], "/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(
        body["upstream_id"],
        json!(upstream_gts_id(Uuid::from_u128(0x777)))
    );

    // An upstream another tenant owns: the reference is invalid, never a 403.
    let (status, body) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&foreign, &["GET"], "/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
}

#[tokio::test]
async fn create_route_rejects_a_schema_violation() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;

    for document in [
        // A route needs a match member.
        json!({ "upstream_id": upstream }),
        // A path is absolute.
        route_document(&upstream, &["GET"], "v1/chat"),
        // At least one method.
        route_document(&upstream, &[], "/v1/chat"),
        // Only the documented methods.
        route_document(&upstream, &["TRACE"], "/v1/chat"),
        // Exactly one protocol block.
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "/v1" },
                       "grpc": { "service": "cf.example.Echo" } }
        }),
    ] {
        let (status, body) = post_route(&router, ctx(tenant_a()), document.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "document {document}");
        assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));
    }
}

#[tokio::test]
async fn create_route_rejects_overlapping_match_rules_with_409() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let (status, _) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET", "POST"], "/v1/chat"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["POST"], "/v1/chat"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["type"], json!(CONFLICT_TYPE));
    assert_eq!(body["reason"], json!(REASON_ROUTE_MATCH_CONFLICT));
    assert_eq!(body["upstream_id"], json!(upstream));
}

#[tokio::test]
async fn two_routes_may_share_a_path_with_disjoint_method_sets() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;

    let (first, _) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET"], "/v1/chat"),
    )
    .await;
    let (second, _) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["POST"], "/v1/chat"),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);
    assert_eq!(second, StatusCode::CREATED);

    // The same path on a different upstream is a different match space.
    let other = create_upstream_id(&router, tenant_a(), "api.anthropic.com", 443).await;
    let (third, _) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&other, &["GET"], "/v1/chat"),
    )
    .await;
    assert_eq!(third, StatusCode::CREATED);
}

#[tokio::test]
async fn routes_are_listed_by_upstream_and_path_and_paginated() {
    let router = build_router();
    let first = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let second = create_upstream_id(&router, tenant_a(), "api.anthropic.com", 443).await;
    for (upstream, path) in [
        (first.clone(), "/v1/chat"),
        (first.clone(), "/v1"),
        (second.clone(), "/"),
    ] {
        let (status, _) = post_route(
            &router,
            ctx(tenant_a()),
            route_document(&upstream, &["GET"], path),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let foreign = create_upstream_id(&router, tenant_b(), "foreign.example.com", 443).await;
    let (status, _) = post_route(
        &router,
        ctx(tenant_b()),
        route_document(&foreign, &["GET"], "/"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router.clone(),
        request("GET", "/oagw/v1/routes", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    let keys: Vec<(String, String)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|route| {
            (
                route["upstream_id"].as_str().unwrap().to_owned(),
                route["match"]["http"]["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    // Exactly the three routes of the tenant, ordered by upstream, then path.
    assert_eq!(keys.len(), 3);
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);

    let page = send(
        router,
        request(
            "GET",
            "/oagw/v1/routes?$top=1&$skip=1",
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    let page = body_json(page).await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["upstream_id"], json!(keys[1].0));
    assert_eq!(page[0]["match"]["http"]["path"], json!(keys[1].1));
}

#[tokio::test]
async fn get_route_accepts_both_identifier_forms() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let (_, body) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET"], "/v1"),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_owned();
    let uuid = id.trim_start_matches(ROUTE_ID_PREFIX).to_owned();

    for reference in [uuid, id.clone()] {
        let response = send(
            router.clone(),
            request(
                "GET",
                &format!("/oagw/v1/routes/{reference}"),
                None,
                ctx(tenant_a()),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "reference {reference}");
        assert_eq!(body_json(response).await["id"], json!(id));
    }
}

#[tokio::test]
async fn put_route_replaces_the_document_and_keeps_the_upstream() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let other = create_upstream_id(&router, tenant_a(), "api.anthropic.com", 443).await;
    let (_, created) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET"], "/v1"),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // Moving the route to another upstream is rejected: `upstream_id` is
    // immutable.
    let moved = send(
        router.clone(),
        request(
            "PUT",
            &format!("/oagw/v1/routes/{id}"),
            Some(route_document(&other, &["GET"], "/v1")),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(moved.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(moved).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(problem["upstream_id"], json!(upstream));

    let replacement = route_document(&upstream, &["POST", "PATCH"], "/v2/chat");
    let response = send(
        router.clone(),
        request(
            "PUT",
            &format!("/oagw/v1/routes/{id}"),
            Some(replacement),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["upstream_id"], json!(upstream));
    assert_eq!(body["match"]["http"]["path"], json!("/v2/chat"));
    assert_eq!(body["match"]["http"]["methods"], json!(["POST", "PATCH"]));

    let fetched = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/routes/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(
        body_json(fetched).await["match"]["http"]["path"],
        json!("/v2/chat")
    );
}

#[tokio::test]
async fn put_route_never_creates() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let response = send(
        router,
        request(
            "PUT",
            &format!("/oagw/v1/routes/{ROUTE_ID_PREFIX}{}", Uuid::from_u128(0x88)),
            Some(route_document(&upstream, &["GET"], "/v1")),
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await["type"],
        json!(ROUTE_NOT_FOUND_TYPE)
    );
}

#[tokio::test]
async fn delete_route_returns_204_and_frees_the_match_rule() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let (_, created) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET"], "/v1"),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/routes/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(deleted.into_body(), 16).await.unwrap().is_empty());

    // The match rule is free again.
    let (status, _) = post_route(
        &router,
        ctx(tenant_a()),
        route_document(&upstream, &["GET"], "/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let again = send(
        router,
        request(
            "DELETE",
            &format!("/oagw/v1/routes/{id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
}

// ── Plugin CRUD ──────────────────────────────────────────────────────────────

const STARLARK_SOURCE: &str = "def on_request(ctx):\n    ctx.request.headers[\"x-seen\"] = \"1\"\n";

#[tokio::test]
async fn create_plugin_returns_201_with_the_kind_qualified_reference() {
    let router = build_router();

    let response = send(
        router,
        request(
            "POST",
            "/oagw/v1/plugins",
            Some(plugin_document(
                "add-tenant-header",
                "transform",
                Some(STARLARK_SOURCE),
            )),
            ctx(tenant_a()),
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let body = body_json(response).await;
    let plugin_ref = body["plugin_ref"].as_str().expect("plugin_ref").to_owned();
    assert!(plugin_ref.starts_with("gts.cf.core.oagw.transform_plugin.v1~"));
    assert_eq!(body["id"], json!(plugin_ref), "id and plugin_ref agree");
    assert_eq!(location, format!("/oagw/v1/plugins/{plugin_ref}"));
    // The server mints the reference: a client value is ignored.
    assert_eq!(body["source"], json!(STARLARK_SOURCE));
}

#[tokio::test]
async fn create_plugin_rejects_a_duplicate_name_with_409() {
    let router = build_router();
    let (first, _) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("header-guard", "guard", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);

    let (status, body) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("header-guard", "guard", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["type"], json!(CONFLICT_TYPE));
    assert_eq!(body["reason"], json!(REASON_PLUGIN_NAME_CONFLICT));

    // Another tenant may use the same name.
    let (status, _) = post_plugin(
        &router,
        ctx(tenant_b()),
        plugin_document("header-guard", "guard", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_plugin_without_a_source_is_a_named_registry_plugin() {
    let router = build_router();

    // No `source` and no `plugin_ref`: nothing to store, 400.
    let (status, body) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("missing-source", "guard", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body {body}");
    assert_eq!(body["type"], json!(VALIDATION_ERROR_TYPE));

    // Naming a registry plugin is legal and stores no source.
    let named = json!({
        "name": "required-headers",
        "kind": "guard",
        "phases": ["on_request"],
        "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
    });
    let (status, body) = post_plugin(&router, ctx(tenant_a()), named).await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");
    assert_eq!(body["source"], Value::Null);
    let plugin_ref = body["plugin_ref"].as_str().unwrap().to_owned();

    // …and it has no source document to download (`plugin.not_found.v1`).
    let response = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/plugins/{plugin_ref}/source"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(response).await["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1")
    );
}

#[tokio::test]
async fn get_plugin_source_returns_the_starlark_source_as_plain_text() {
    let router = build_router();
    let (_, body) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("add-tenant-header", "transform", Some(STARLARK_SOURCE)),
    )
    .await;
    let plugin_ref = body["plugin_ref"].as_str().unwrap().to_owned();
    let uuid = plugin_ref.split_once('~').unwrap().1.to_owned();

    for reference in [uuid, plugin_ref.clone()] {
        let response = send(
            router.clone(),
            request(
                "GET",
                &format!("/oagw/v1/plugins/{reference}/source"),
                None,
                ctx(tenant_a()),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "reference {reference}");
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        assert_eq!(bytes, STARLARK_SOURCE);
    }

    // The plugin document itself is readable under the GTS form too.
    let response = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["name"],
        json!("add-tenant-header")
    );
}

#[tokio::test]
async fn plugin_deletion_is_refused_while_a_route_binds_it() {
    let router = build_router();
    let upstream = create_upstream_id(&router, tenant_a(), "api.openai.com", 443).await;
    let (created, plugin) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("add-tenant-header", "transform", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(created, StatusCode::CREATED, "body {plugin}");
    let plugin_ref = plugin["plugin_ref"].as_str().unwrap().to_owned();
    let (created, route) = post_route(
        &router,
        ctx(tenant_a()),
        json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "items": [plugin_ref] }
        }),
    )
    .await;
    assert_eq!(created, StatusCode::CREATED, "body {route}");
    assert!(route["id"].as_str().is_some());

    let response = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let problem = body_json(response).await;
    assert_eq!(problem["type"], json!(CONFLICT_TYPE));
    assert_eq!(problem["reason"], json!(REASON_PLUGIN_IN_USE));
    assert_eq!(problem["plugin_id"], json!(plugin_ref));
    assert!(!problem["referenced_by"].as_array().unwrap().is_empty());

    // Unbinding the route frees the plugin.
    let route_id = route["id"].as_str().unwrap().to_owned();
    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let deleted = send(
        router,
        request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_deletion_is_refused_while_an_upstream_binds_it() {
    let router = build_router();
    let (created, plugin) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("add-tenant-header", "transform", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(created, StatusCode::CREATED, "body {plugin}");
    let plugin_ref = plugin["plugin_ref"].as_str().unwrap().to_owned();

    let mut document = upstream_document("api.openai.com", 443, None);
    document["plugins"] = json!({ "items": [plugin_ref] });
    let (status, _) = create(&router, ctx(tenant_a()), document).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router,
        request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(response).await["reason"],
        json!(REASON_PLUGIN_IN_USE)
    );
}

#[tokio::test]
async fn an_unused_plugin_deletes_with_204() {
    let router = build_router();
    let (created, plugin) = post_plugin(
        &router,
        ctx(tenant_a()),
        plugin_document("unused", "guard", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(created, StatusCode::CREATED, "body {plugin}");
    let plugin_ref = plugin["plugin_ref"].as_str().unwrap().to_owned();

    let deleted = send(
        router.clone(),
        request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let response = send(
        router,
        request(
            "GET",
            &format!("/oagw/v1/plugins/{plugin_ref}"),
            None,
            ctx(tenant_a()),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await["type"],
        json!(ROUTE_NOT_FOUND_TYPE)
    );
}

#[tokio::test]
async fn plugins_are_listed_by_name_and_tenant_scoped() {
    let router = build_router();
    for name in ["zeta", "alpha", "mid"] {
        let (status, _) = post_plugin(
            &router,
            ctx(tenant_a()),
            plugin_document(name, "guard", Some(STARLARK_SOURCE)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, _) = post_plugin(
        &router,
        ctx(tenant_b()),
        plugin_document("foreign", "guard", Some(STARLARK_SOURCE)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = send(
        router,
        request("GET", "/oagw/v1/plugins?$top=2", None, ctx(tenant_a())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = body_json(response).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|plugin| plugin["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "mid"]);
}
