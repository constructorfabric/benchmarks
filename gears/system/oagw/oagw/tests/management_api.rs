//! Management-API integration tests: upstream / route / plugin CRUD with the
//! spec's validation, tenant-scoping, and conflict semantics.

mod common;

use common::{call, post_json, put_json, router, sec, tenant, upstream_body};
use serde_json::json;
use uuid::Uuid;

/// POST an upstream and return its `(status, id, body)`.
async fn create_upstream(
    app: &axum::Router,
    body: serde_json::Value,
) -> (axum::http::StatusCode, Uuid, serde_json::Value) {
    let (status, value) = post_json(app, "/oagw/v1/upstreams", body).await;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or(Uuid::nil());
    (status, id, value)
}

#[tokio::test]
async fn create_upstream_derives_alias_and_returns_201() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, _id, body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["alias"], json!("api.example.com:8443"));
    assert_eq!(body["enabled"], json!(true));
    assert!(body["id"].as_str().unwrap().len() >= 36);
    assert!(body["created_at"].is_string());
    assert!(body["updated_at"].is_string());
}

#[tokio::test]
async fn http_endpoint_standard_port_derives_bare_host() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, _id, body) = create_upstream(&app, upstream_body("svc.local", Some(80), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["alias"], json!("svc.local"));
}

#[tokio::test]
async fn ip_endpoint_requires_explicit_alias() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    // No alias on an IP endpoint → validation failure.
    let (status, _id, body) = create_upstream(&app, upstream_body("10.0.0.1", Some(8080), None)).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], json!(400));

    // An explicit alias makes it valid.
    let (status, _id, body) =
        create_upstream(&app, upstream_body("10.0.0.1", Some(8080), Some("edge-api"))).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body["alias"], json!("edge-api"));
}

#[tokio::test]
async fn explicit_alias_must_match_derived_alias() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, _id, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(443), Some("other-name"))).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn duplicate_alias_is_409() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (s1, _id, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(s1, axum::http::StatusCode::CREATED);
    let (s2, _id, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(s2, axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn unknown_fields_are_rejected_400() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let mut body = upstream_body("api.example.com", Some(8443), None);
    body["bogus_field"] = json!(42);
    let (status, _v) = post_json(&app, "/oagw/v1/upstreams", body).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn upstream_is_tenant_scoped() {
    let t = tenant();
    let other = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, id, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    // A different tenant cannot see or delete it.
    let other_app = router(common::test_state(other), sec(other));
    let (status, _headers, _v) = call(
        &other_app,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // The owning tenant can.
    let (status, _headers, _v) = call(
        &app,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn put_replaces_upstream_preserving_id() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, id, body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let mut replacement = upstream_body("api.example.com", Some(9443), None);
    replacement["enabled"] = json!(false);
    replacement["tags"] = json!(["prod"]);
    let (status, value) = put_json(&app, &format!("/oagw/v1/upstreams/{id}"), replacement).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(value["id"], body["id"]);
    assert_eq!(value["alias"], json!("api.example.com:9443"));
    assert_eq!(value["enabled"], json!(false));
}

#[tokio::test]
async fn put_to_other_tenants_upstream_is_404() {
    let t = tenant();
    let other = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, id, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let other_app = router(common::test_state(other), sec(other));
    let (status, _v) = put_json(
        &other_app,
        &format!("/oagw/v1/upstreams/{id}"),
        upstream_body("api.example.com", Some(8443), None),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_upstream_cascades_routes() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, uid, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let route = json!({
        "tags": [],
        "upstream_id": uid.to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "plugins": { "sharing": "inherit", "items": [] },
    });
    let (status, v) = post_json(&app, "/oagw/v1/routes", route).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let rid = v["id"].as_str().unwrap().to_string();

    let (status, _h, _v) = call(
        &app,
        "DELETE",
        &format!("/oagw/v1/upstreams/{uid}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    let (status, _h, _v) = call(
        &app,
        "GET",
        &format!("/oagw/v1/routes/{rid}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_supports_filter_orderby_and_top() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    for (host, port, alias) in [
        ("api.example.com", Some(8443), None),
        ("api.example.org", Some(8443), None),
        ("api.example.net", Some(8443), None),
    ] {
        let (status, _id, _body) =
            create_upstream(&app, upstream_body(host, port, alias)).await;
        assert_eq!(status, axum::http::StatusCode::CREATED);
    }

    // $filter by alias.
    let (status, _h, v) = call(
        &app,
        "GET",
        "/oagw/v1/upstreams?$filter=alias%20eq%20'api.example.org:8443'",
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(v["total"], json!(1));
    assert_eq!(v["value"][0]["alias"], json!("api.example.org:8443"));

    // $orderby desc + $top 2.
    let (status, _h, v) = call(
        &app,
        "GET",
        "/oagw/v1/upstreams?$orderby=alias%20desc&$top=2",
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 2);
    assert_eq!(v["value"][0]["alias"], json!("api.example.org:8443"));
    assert!(v["next_cursor"].is_string());
}

#[tokio::test]
async fn route_requires_tenant_owned_upstream() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let route = json!({
        "tags": [],
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "plugins": { "sharing": "inherit", "items": [] },
    });
    let (status, v) = post_json(&app, "/oagw/v1/routes", route).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(v["status"], json!(404));
}

#[tokio::test]
async fn duplicate_route_path_and_method_is_409() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, uid, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let route = |methods: &[&str]| {
        json!({
            "tags": [],
            "upstream_id": uid.to_string(),
            "match": { "http": { "methods": methods, "path": "/v1/things" } },
            "plugins": { "sharing": "inherit", "items": [] },
        })
    };
    let (s1, _v) = post_json(&app, "/oagw/v1/routes", route(&["GET", "POST"])).await;
    assert_eq!(s1, axum::http::StatusCode::CREATED);
    // Overlapping method on the same path → conflict.
    let (s2, _v) = post_json(&app, "/oagw/v1/routes", route(&["POST"])).await;
    assert_eq!(s2, axum::http::StatusCode::CONFLICT);
    // Same path, disjoint methods → allowed.
    let (s3, _v) = post_json(&app, "/oagw/v1/routes", route(&["DELETE"])).await;
    assert_eq!(s3, axum::http::StatusCode::CREATED);
}

#[tokio::test]
async fn invalid_route_shape_is_400() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, uid, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    // No methods.
    let route = json!({
        "tags": [],
        "upstream_id": uid.to_string(),
        "match": { "http": { "methods": [], "path": "/v1" } },
        "plugins": { "sharing": "inherit", "items": [] },
    });
    let (status, v) = post_json(&app, "/oagw/v1/routes", route).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(v["status"], json!(400));
}

#[tokio::test]
async fn route_put_rejects_unknown_upstream_id_field() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let (status, uid, _body) =
        create_upstream(&app, upstream_body("api.example.com", Some(8443), None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let route = json!({
        "tags": [],
        "upstream_id": uid.to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "plugins": { "sharing": "inherit", "items": [] },
    });
    let (status, v) = post_json(&app, "/oagw/v1/routes", route).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let rid = v["id"].as_str().unwrap().to_string();

    // `upstream_id` is not part of RoutePut → 422, not silently ignored.
    let (status, _v) = put_json(
        &app,
        &format!("/oagw/v1/routes/{rid}"),
        json!({
            "upstream_id": uid.to_string(),
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "sharing": "inherit", "items": [] },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn plugin_crud_and_delete_in_use() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let plugin = json!({
        "name": "req-hdrs",
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.custom.v1",
        "config": { "required_request_headers": "x-tenant" },
    });
    let (status, v) = post_json(&app, "/oagw/v1/plugins", plugin).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(v["name"], json!("req-hdrs"));
    let pid = v["id"].as_str().unwrap().to_string();

    // List + get.
    let (status, _h, list) = call(
        &app,
        "GET",
        "/oagw/v1/plugins?$filter=name%20eq%20'req-hdrs'",
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(list["total"], json!(1));

    let (status, _h, _v) = call(
        &app,
        "GET",
        &format!("/oagw/v1/plugins/{pid}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // Unreferenced plugin deletes fine.
    {
        let (status, _h, _v) = call(
            &app,
            "DELETE",
            &format!("/oagw/v1/plugins/{pid}"),
            &[],
            axum::body::Body::empty(),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    }
}

#[tokio::test]
async fn plugin_delete_in_use_is_409() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let plugin = json!({
        "name": "req-hdrs",
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.custom.v1",
        "config": { "required_request_headers": "x-tenant" },
    });
    let (status, v) = post_json(&app, "/oagw/v1/plugins", plugin).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let pid = v["id"].as_str().unwrap().to_string();

    // Bind it to an upstream by UUID-backed identifier, then attempt deletion.
    let mut upstream = upstream_body("api.example.com", Some(8443), None);
    upstream["plugins"] =
        json!({ "sharing": "inherit", "items": [format!("gts.cf.core.oagw.guard_plugin.v1~{pid}")] });
    let (status, _uid, _v) = create_upstream(&app, upstream).await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let (status, _h, _v) = call(
        &app,
        "DELETE",
        &format!("/oagw/v1/plugins/{pid}"),
        &[],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn invalid_plugin_type_is_400() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    let plugin = json!({
        "name": "bogus",
        "plugin_type": "gts.cf.core.oagw.nope.v1~whatever",
        "config": {},
    });
    let (status, v) = post_json(&app, "/oagw/v1/plugins", plugin).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(v["status"], json!(400));
}
