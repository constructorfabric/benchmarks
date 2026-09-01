#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Control-plane (management REST) integration tests.
//!
//! Exercises the `/oagw/v1/{upstreams,routes,plugins}` surface end to end:
//! CRUD, tenant scoping, alias immutability, route-match conflict detection,
//! plugin reference protection, `OData` list parameters and the `RFC 9457`
//! error envelope.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use tower::ServiceExt;
use uuid::Uuid;

use common::{
    TENANT_A, TENANT_B, TENANT_ROOT, build_router, control_with_topology, e2e_config, json_request,
    response_json,
};

use credstore_sdk::test_util::MockCredStoreClient;
use oagw::gts;
use oagw::infra::data_plane::DataPlaneService;

/// Build the management router (data plane with an empty credstore).
fn router() -> axum::Router {
    let control = control_with_topology();
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::empty());
    let data_plane = Arc::new(
        DataPlaneService::new(control.clone(), credstore, None, e2e_config())
            .expect("data plane builds"),
    );
    build_router(control, data_plane)
}

fn upstream_json(alias: &str) -> serde_json::Value {
    serde_json::json!({
        "alias": alias,
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9999 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    })
}

/// Create an upstream and return its id (strips the GTS prefix).
async fn create_upstream(router: &axum::Router, tenant: Uuid, alias: &str) -> (String, Uuid) {
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_json(alias)),
            tenant,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CREATED, "create upstream");
    assert!(resp.headers().contains_key("location"));
    let body = response_json(resp).await;
    let id = body["id"].as_str().expect("id").to_owned();
    let uuid = gts::parse_resource_id(&id).expect("parse id");
    (id, uuid)
}

#[tokio::test]
async fn upstream_full_crud_flow() {
    let router = router();
    let (_gid, uuid) = create_upstream(&router, TENANT_A, "alpha").await;

    // Get by bare uuid.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/oagw/v1/upstreams/{uuid}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_json(resp).await;
    assert_eq!(body["alias"], "alpha");
    assert_eq!(body["enabled"], true);

    // List contains it.
    let resp = router
        .clone()
        .oneshot(json_request("GET", "/oagw/v1/upstreams", None, TENANT_A))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let list = response_json(resp).await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["alias"], "alpha");

    // Update (alias unchanged, port changed).
    let updated = serde_json::json!({
        "alias": "alpha",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 10001 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
        "tags": ["prod"],
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/upstreams/{uuid}"),
            Some(updated),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_json(resp).await;
    assert_eq!(body["server"]["endpoints"][0]["port"], 10001);
    assert_eq!(body["tags"][0], "prod");

    // Alias is immutable -> 400.
    let moved = serde_json::json!({
        "alias": "beta",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9999 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/upstreams/{uuid}"),
            Some(moved),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_VALIDATION);
    assert_eq!(err["status"], 400);

    // Delete -> 204, then 404.
    let resp = router
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{uuid}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/oagw/v1/upstreams/{uuid}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_alias_conflicts() {
    let router = router();
    create_upstream(&router, TENANT_A, "payments").await;

    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_json("payments")),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_CONFLICT);
}

#[tokio::test]
async fn hostname_alias_is_auto_derived() {
    let router = router();
    let payload = serde_json::json!({
        "server": { "endpoints": [ { "host": "api.example.com", "port": 8443 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    // Non-standard ports are part of the derived alias.
    assert_eq!(body["alias"], "api.example.com:8443");

    // A conflicting explicit alias for the same endpoints is rejected (400).
    let payload = serde_json::json!({
        "alias": "custom",
        "server": { "endpoints": [ { "host": "api.example.com", "port": 8443 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/upstreams",
            Some(payload),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let router = router();
    let (_gid, uuid) = create_upstream(&router, TENANT_A, "alpha").await;

    // Tenant B cannot see it.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/oagw/v1/upstreams/{uuid}"),
            None,
            TENANT_B,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = router
        .clone()
        .oneshot(json_request("GET", "/oagw/v1/upstreams", None, TENANT_B))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let list = response_json(resp).await;
    assert_eq!(list.as_array().expect("array").len(), 0);

    // Alias collision across tenants is allowed.
    create_upstream(&router, TENANT_B, "alpha").await;

    // Root tenant is also isolated (descendant resources are invisible upward).
    let resp = router
        .clone()
        .oneshot(json_request("GET", "/oagw/v1/upstreams", None, TENANT_ROOT))
        .await
        .expect("request");
    let list = response_json(resp).await;
    assert_eq!(list.as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn list_supports_odata_params() {
    let router = router();
    create_upstream(&router, TENANT_A, "alpha").await;
    create_upstream(&router, TENANT_A, "beta").await;

    // $filter on alias.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20%27beta%27",
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    let list = response_json(resp).await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["alias"], "beta");

    // $select projects fields.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/oagw/v1/upstreams?$select=alias",
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    let list = response_json(resp).await;
    let first = &list.as_array().expect("array")[0];
    assert!(first.get("alias").is_some());
    assert!(first.get("server").is_none());

    // $orderby desc + $top/$skip.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/oagw/v1/upstreams?$orderby=alias%20desc&$top=1&$skip=1",
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    let list = response_json(resp).await;
    let arr = list.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["alias"], "alpha");

    // Unknown filter field -> 400 problem.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/oagw/v1/upstreams?$filter=nope%20eq%20%27x%27",
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn route_crud_flow_and_conflicts() {
    let router = router();
    let (_uid, uuid) = create_upstream(&router, TENANT_A, "payments").await;

    let route = serde_json::json!({
        "upstream_id": uuid.to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1/charges", "query_allowlist": ["id"] } },
        "tags": ["read"],
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/routes",
            Some(route),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let route_gid = body["id"].as_str().expect("id").to_owned();
    let route_uuid = gts::parse_resource_id(&route_gid).expect("parse id");

    // List routes.
    let resp = router
        .clone()
        .oneshot(json_request("GET", "/oagw/v1/routes", None, TENANT_A))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        response_json(resp).await.as_array().expect("array").len(),
        1
    );

    // Overlapping route -> 409.
    let overlap = serde_json::json!({
        "upstream_id": uuid.to_string(),
        "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/charges" } },
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/routes",
            Some(overlap),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Route on an unknown upstream -> 404.
    let orphan = serde_json::json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/x" } },
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/routes",
            Some(orphan),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Update keeps the upstream binding; changing it -> 400.
    let replacement = serde_json::json!({
        "upstream_id": uuid.to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1/refunds" } },
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/routes/{route_uuid}"),
            Some(replacement),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);

    let rebound = serde_json::json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/x" } },
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/routes/{route_uuid}"),
            Some(rebound),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Delete -> 204.
    let resp = router
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/oagw/v1/routes/{route_uuid}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_crud_and_reference_protection() {
    let router = router();
    let plugin = serde_json::json!({
        "name": "peer-gate",
        "plugin_type": "guard",
        "config_schema": { "type": "object" },
        "source_code": "def handle(ctx):\n    return None\n",
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/oagw/v1/plugins",
            Some(plugin),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = response_json(resp).await;
    let plugin_id = body["id"].as_str().expect("id").to_owned();

    // Source endpoint.
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/oagw/v1/plugins/{plugin_id}/source"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .expect("collect")
        .to_bytes();
    assert!(bytes.starts_with(b"def handle"));

    // Bind it to an upstream -> delete refused with 409.
    let (_uid, uuid) = create_upstream(&router, TENANT_A, "alpha").await;
    let bound = serde_json::json!({
        "alias": "alpha",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9999 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
        "plugins": { "items": [plugin_id.clone()] },
    });
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/upstreams/{uuid}"),
            Some(bound),
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_PLUGIN_IN_USE);
    assert_eq!(error_source.as_deref(), Some("gateway"));

    // Unlink then delete succeeds.
    let unbound = serde_json::json!({
        "alias": "alpha",
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 9999 } ] },
        "protocol": gts::PROTOCOL_HTTP_ID,
        "plugins": null,
    });
    let _ = router
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/oagw/v1/upstreams/{uuid}"),
            Some(unbound),
            TENANT_A,
        ))
        .await
        .expect("request");
    let resp = router
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn error_envelope_is_rfc9457() {
    let router = router();
    let resp = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/oagw/v1/upstreams/not-a-uuid",
            None,
            TENANT_A,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let error_source = resp
        .headers()
        .get("x-oagw-error-source")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let err = response_json(resp).await;
    assert!(err["type"].is_string());
    assert!(err["title"].is_string());
    assert!(err["detail"].is_string());
    assert!(
        err["instance"]
            .as_str()
            .expect("instance")
            .starts_with("urn:uuid:")
    );
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert_eq!(error_source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn management_rejects_malformed_json_and_missing_content_type() {
    let router = router();

    // Malformed JSON with a JSON content type -> 400 validation problem.
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"alias": "broken""#))
        .expect("valid request");
    let mut req = req;
    req.extensions_mut().insert(common::make_security(TENANT_A));
    let resp = router.clone().oneshot(req).await.expect("rejected request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_VALIDATION);
    assert_eq!(err["status"], 400);

    // A JSON-shaped body without the content type -> 415, not a 400.
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .body(axum::body::Body::from("{}"))
        .expect("valid request");
    let mut req = req;
    req.extensions_mut().insert(common::make_security(TENANT_A));
    let resp = router.clone().oneshot(req).await.expect("rejected request");
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let err = response_json(resp).await;
    assert_eq!(err["type"], gts::ERR_VALIDATION);
    assert_eq!(err["status"], 415);
}
