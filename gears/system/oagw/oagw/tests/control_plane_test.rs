//! Control-plane (management API) integration tests.
//!
//! Exercises the `/oagw/v1/upstreams`, `/routes` and `/plugins` CRUD
//! surface end to end: status codes, RFC 9457 problem documents, alias
//! derivation rules, OData list pipelines, and scope enforcement.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use axum::http::StatusCode;
use uuid::Uuid;

use common::{PROTOCOL_HTTP, admin_ctx, create_route, create_upstream, expect_problem, response_json, send};

fn upstream_body(host: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut body = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [{ "scheme": "https", "host": host }] },
        "protocol": PROTOCOL_HTTP,
        "tags": ["t1"],
    });
    if let Some(obj) = extra.as_object() {
        body.as_object_mut()
            .unwrap()
            .extend(obj.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    body
}

// =====================================================================
//                              Upstreams
// =====================================================================

#[tokio::test]
async fn create_upstream_returns_201_with_location_and_derived_alias() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let record = create_upstream(&app, tenant, upstream_body("api.example.com", serde_json::json!({})))
        .await;

    assert_eq!(record["alias"], "api.example.com");
    assert_eq!(record["enabled"], true);
    assert_eq!(record["protocol"], PROTOCOL_HTTP);
    assert!(record.get("id").and_then(|v| v.as_str()).is_some());
    assert!(record.get("created_at").is_some());
    // Server-managed tenant id is serialized for the created record.
    assert_eq!(record["tenant_id"].as_str().unwrap().parse::<Uuid>().unwrap(), tenant);
}

#[tokio::test]
async fn create_upstream_duplicate_alias_returns_409() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    create_upstream(&app, tenant, upstream_body("dup.example.com", serde_json::json!({}))).await;

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/upstreams",
        &ctx,
        Some(upstream_body("dup.example.com", serde_json::json!({}))),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::CONFLICT).await;
    assert_eq!(body["type"], oagw::gts::ERR_ALREADY_EXISTS);
    assert!(body["detail"].as_str().unwrap().contains("dup.example.com"));
}

#[tokio::test]
async fn ip_endpoint_requires_explicit_alias() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    // IP endpoints cannot auto-derive an alias → 400 without an explicit one.
    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/upstreams",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 8080 }] },
            "protocol": PROTOCOL_HTTP
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("explicit alias required"));

    // With an explicit alias the same endpoint is accepted.
    let record = create_upstream(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "alias": "local-svc",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 8080 }] },
            "protocol": PROTOCOL_HTTP
        }),
    )
    .await;
    assert_eq!(record["alias"], "local-svc");
}

#[tokio::test]
async fn explicit_alias_must_match_derived_alias() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/upstreams",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "alias": "mismatch.example.com",
            "server": { "endpoints": [{ "scheme": "https", "host": "real.example.com" }] },
            "protocol": PROTOCOL_HTTP
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("differs from the auto-derived alias"));
}

#[tokio::test]
async fn get_upstream_returns_404_for_unknown() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::NOT_FOUND).await;
    assert_eq!(body["type"], oagw::gts::ERR_ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn replace_upstream_updates_but_alias_is_immutable() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let created = create_upstream(&app, tenant, upstream_body("svc-a.test", serde_json::json!({}))).await;
    let id = created["id"].as_str().unwrap().to_owned();

    // PUT with the same (derived) alias updates the payload.
    // `updated_at` has millisecond precision; let a few ms elapse so the
    // replaced record's timestamp is provably newer than the original.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        &ctx,
        Some(serde_json::json!({
            "enabled": false,
            "tags": ["renamed"],
            "server": { "endpoints": [{ "scheme": "https", "host": "svc-a.test" }] },
            "protocol": PROTOCOL_HTTP
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replaced = response_json(resp).await;
    assert_eq!(replaced["alias"], "svc-a.test");
    assert_eq!(replaced["enabled"], false);
    assert_eq!(replaced["tags"], serde_json::json!(["renamed"]));
    assert_ne!(replaced["updated_at"], created["updated_at"]);

    // Changing the endpoints so the derived alias differs → 400.
    let resp = send(
        &app.router,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        &ctx,
        Some(upstream_body("svc-b.test", serde_json::json!({}))),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("alias is immutable"));
}

#[tokio::test]
async fn delete_upstream_cascades_to_routes() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let upstream = create_upstream(&app, tenant, upstream_body("cascade.test", serde_json::json!({}))).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let route = create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/" } }
        }),
    )
    .await;
    let route_id = route["id"].as_str().unwrap().to_owned();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Referenced route was deleted with the upstream.
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/routes/{route_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Second delete → 404.
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    expect_problem(resp, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn list_upstreams_supports_odata_filter_orderby_and_select() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    create_upstream(&app, tenant, upstream_body("svc-a.test", serde_json::json!({}))).await;
    create_upstream(&app, tenant, upstream_body("svc-b.test", serde_json::json!({}))).await;
    create_upstream(&app, tenant, upstream_body("svc-c.test", serde_json::json!({"enabled": false}))).await;

    let ctx = admin_ctx(tenant);

    // $filter on alias.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/upstreams?$filter=alias%20eq%20'svc-a.test'",
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_json(resp).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["alias"], "svc-a.test");

    // $filter on boolean field.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/upstreams?$filter=enabled%20eq%20false",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = response_json(resp).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["alias"], "svc-c.test");

    // $orderby=alias desc.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/upstreams?$orderby=alias%20desc",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = response_json(resp).await;
    let aliases: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["alias"].as_str().unwrap())
        .collect();
    assert_eq!(aliases, vec!["svc-c.test", "svc-b.test", "svc-a.test"]);

    // $select projects only the requested fields.
    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/upstreams?$select=id,alias",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = response_json(resp).await;
    let first = &body["items"][0];
    assert!(first.get("id").is_some());
    assert!(first.get("alias").is_some());
    assert!(first.get("enabled").is_none());
    assert!(first.get("protocol").is_none());
}

#[tokio::test]
async fn list_upstreams_pages_via_skiptoken() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    for i in 0..3 {
        create_upstream(
            &app,
            tenant,
            upstream_body(&format!("s{i}.paging.test"), serde_json::json!({})),
        )
        .await;
    }

    let ctx = admin_ctx(tenant);

    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/upstreams?$top=2",
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page1 = response_json(resp).await;
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    let cursor = page1["page_info"]["next_cursor"].as_str().unwrap().to_owned();

    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/upstreams?$skiptoken={cursor}"),
        &ctx,
        None,
        &[],
    )
    .await;
    let page2 = response_json(resp).await;
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert!(page2["page_info"]["next_cursor"].is_null());
}

#[tokio::test]
async fn management_api_enforces_token_scopes() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = common::make_ctx(tenant, &[]);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/upstreams",
        &ctx,
        Some(upstream_body("scoped.test", serde_json::json!({}))),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::FORBIDDEN).await;
    assert_eq!(body["type"], oagw::gts::ERR_PERMISSION_DENIED);
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains(oagw::domain::services::SCOPE_UPSTREAM_CREATE));
}

// =====================================================================
//                               Routes
// =====================================================================

#[tokio::test]
async fn route_crud_lifecycle() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let upstream = create_upstream(&app, tenant, upstream_body("rt.test", serde_json::json!({}))).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let created = create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "tags": ["api"],
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET", "POST"], "path": "/v1" } }
        }),
    )
    .await;
    let route_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["upstream_id"].as_str().unwrap(), upstream_id);
    assert_eq!(created["match"]["http"]["path"], "/v1");

    // Get one.
    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/routes/{route_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Replace (upstream_id is immutable — not part of the update DTO).
    let resp = send(
        &app.router,
        "PUT",
        &format!("/oagw/v1/routes/{route_id}"),
        &ctx,
        Some(serde_json::json!({
            "enabled": false,
            "match": { "http": { "methods": ["POST"], "path": "/v2" } }
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replaced = response_json(resp).await;
    assert_eq!(replaced["upstream_id"].as_str().unwrap(), upstream_id);
    assert_eq!(replaced["match"]["http"]["path"], "/v2");
    assert_eq!(replaced["enabled"], false);

    // Delete.
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/routes/{route_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Gone.
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/routes/{route_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    expect_problem(resp, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn route_with_unknown_upstream_is_404() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/routes",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/" } }
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::NOT_FOUND).await;
    assert_eq!(body["type"], oagw::gts::ERR_ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn route_without_match_is_400() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&app, tenant, upstream_body("nomat.test", serde_json::json!({}))).await;

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/routes",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "upstream_id": upstream["id"].as_str().unwrap(),
            "match": {}
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("http or grpc"));
}

#[tokio::test]
async fn duplicate_route_match_overlapping_method_is_409() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();
    let upstream = create_upstream(&app, tenant, upstream_body("dup-rt.test", serde_json::json!({}))).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    create_route(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET", "POST"], "path": "/api" } }
        }),
    )
    .await;

    // Same path, overlapping method → 409 conflict.
    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/routes",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["POST"], "path": "/api" } }
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::CONFLICT).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);

    // Same path but disjoint methods → accepted.
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/routes",
        &ctx,
        Some(serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["DELETE"], "path": "/api" } }
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// =====================================================================
//                               Plugins
// =====================================================================

#[tokio::test]
async fn plugin_lifecycle_and_source() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/plugins",
        &ctx,
        Some(serde_json::json!({
            "name": "custom-hmac",
            "type": "transform",
            "source": "def transform():\n    return True\n",
            "config_schema": { "type": "object" }
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = response_json(resp).await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["kind"], "transform");
    assert_eq!(created["name"], "custom-hmac");

    // Get one.
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/plugins/{id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Source endpoint returns the raw Starlark source (JSON-encoded string).
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/plugins/{id}/source"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let source = response_json(resp).await;
    assert!(source.as_str().unwrap().contains("def transform"));

    // Delete.
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/plugins/{id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Gone.
    let resp = send(
        &app.router,
        "GET",
        &format!("/oagw/v1/plugins/{id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    expect_problem(resp, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn plugin_validation_rejects_empty_name() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/plugins",
        &ctx,
        Some(serde_json::json!({
            "name": "",
            "type": "auth",
            "source": "def auth():\n    pass\n"
        })),
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["type"], oagw::gts::ERR_VALIDATION);
    assert!(body["detail"].as_str().unwrap().contains("name must not be empty"));
}

#[tokio::test]
async fn delete_referenced_plugin_is_409() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    let resp = send(
        &app.router,
        "POST",
        "/oagw/v1/plugins",
        &ctx,
        Some(serde_json::json!({
            "name": "bound-plugin",
            "type": "auth",
            "source": "def auth():\n    pass\n"
        })),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plugin = response_json(resp).await;
    let plugin_id = plugin["id"].as_str().unwrap().to_owned();
    let plugin_ref = format!(
        "gts.cf.core.oagw.auth_plugin.v1~{plugin_id}"
    );

    // Bind the plugin to an upstream via its custom-instance ref.
    let upstream = create_upstream(
        &app,
        tenant,
        serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [{ "scheme": "https", "host": "ref.test" }] },
            "protocol": PROTOCOL_HTTP,
            "plugins": { "items": [plugin_ref] }
        }),
    )
    .await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    // Still referenced → 409.
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    let body = expect_problem(resp, StatusCode::CONFLICT).await;
    assert_eq!(body["type"], oagw::gts::ERR_PLUGIN_IN_USE);
    assert!(body["detail"].as_str().unwrap().contains(&format!("upstream/{upstream_id}")));

    // Deleting the upstream frees the plugin.
    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(
        &app.router,
        "DELETE",
        &format!("/oagw/v1/plugins/{plugin_id}"),
        &ctx,
        None,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn list_plugins_filters_by_kind() {
    let app = common::TestApp::new().await;
    let tenant = Uuid::new_v4();

    let ctx = admin_ctx(tenant);
    for (name, kind) in [("pa", "auth"), ("pg", "guard"), ("pt", "transform")] {
        let resp = send(
            &app.router,
            "POST",
            "/oagw/v1/plugins",
            &ctx,
            Some(serde_json::json!({
                "name": name,
                "type": kind,
                "source": "def x():\n    pass\n"
            })),
            &[],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let resp = send(
        &app.router,
        "GET",
        "/oagw/v1/plugins?$filter=kind%20eq%20'guard_plugin'",
        &ctx,
        None,
        &[],
    )
    .await;
    let body = response_json(resp).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["name"], "pg");
}
