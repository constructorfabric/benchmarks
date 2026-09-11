//! AT-1: the management API's CRUD and rejection semantics
//! (`contracts/management-api.md`).

mod common;

use common::{Caller, app, create_route, create_upstream, route_body, send};

fn upstream_with_alias(alias: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "https", "host": "api.vendor.com", "port": 443 }
        ]},
        "protocol": common::PROTOCOL_HTTP,
    })
    .to_string()
}

#[tokio::test]
async fn a_lifecycle_over_the_documented_status_codes() {
    let app = app();
    let caller = Caller::default();

    assert_eq!(
        send(&app, &caller, "GET", "/oagw/v1/upstreams", None)
            .await
            .0,
        axum::http::StatusCode::OK
    );

    let created = create_upstream(&app, &caller, &upstream_with_alias("api.vendor.com")).await;
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["alias"], "api.vendor.com");
    // Server-managed fields are never echoed.
    assert!(created.get("tenant_id").is_none() || created["tenant_id"].is_null());

    let listed = send(&app, &caller, "GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(listed.0, axum::http::StatusCode::OK);
    assert_eq!(listed.2.as_array().map(Vec::len), Some(1));

    let read = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(read.0, axum::http::StatusCode::OK);
    assert_eq!(read.2["alias"], "api.vendor.com");

    // Replacement keeps the stored alias; the endpoints may not drift away
    // from it.
    let mut replacement =
        serde_json::from_str::<serde_json::Value>(&upstream_with_alias("api.vendor.com"))
            .expect("json");
    replacement["tags"] = serde_json::json!(["tier-1"]);
    let (status, _, body) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(replacement.to_string().as_str()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["alias"], "api.vendor.com");
    assert_eq!(body["tags"], serde_json::json!(["tier-1"]));

    // A body whose endpoints would derive a different alias is a `400`, not a
    // silent rename.
    let mut renamed =
        serde_json::from_str::<serde_json::Value>(&upstream_with_alias("api.vendor.com"))
            .expect("json");
    renamed["server"]["endpoints"][0]["host"] = "api2.vendor.com".into();
    let (status, _, body) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        Some(renamed.to_string().as_str()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");

    // A route binds the upstream, then is read, replaced and listed.
    let route_body = route_body(&id, "/v1");
    let route = create_route(&app, &caller, &route_body).await;
    let route_id = route["id"].as_str().expect("route id").to_owned();
    assert_eq!(route["upstream_id"], id.as_str());

    let listed_routes = send(&app, &caller, "GET", "/oagw/v1/routes", None).await;
    assert_eq!(listed_routes.2.as_array().map(Vec::len), Some(1));

    // Deleting the upstream cascades to its routes.
    let (status, _, _) = send(
        &app,
        &caller,
        "DELETE",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let after = send(&app, &caller, "GET", "/oagw/v1/routes", None).await;
    assert_eq!(after.2.as_array().map(Vec::len), Some(0));

    let gone = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/routes/{route_id}"),
        None,
    )
    .await;
    assert_eq!(gone.0, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn schema_violations_and_conflicts_are_rejected() {
    let shared = common::harness(oagw::config::OagwConfig::default());
    let app = shared.app.clone();
    let caller = shared.caller;
    let created = create_upstream(&app, &caller, &upstream_with_alias("api.vendor.com")).await;
    let id = created["id"].as_str().expect("id").to_owned();

    // Unknown field: the published schema forbids additional properties.
    let mut bogus = serde_json::from_str::<serde_json::Value>(&upstream_with_alias("x.vendor.com"))
        .expect("json");
    bogus["nonsense"] = true.into();
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/upstreams",
        Some(bogus.to_string().as_str()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");

    // Second upstream on the same alias: 409.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/upstreams",
        Some(upstream_with_alias("api.vendor.com").as_str()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("api.vendor.com")
    );

    // A route against an unknown upstream: 404.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&route_body("00000000-0000-0000-0000-000000000000", "/v1")),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");

    // Duplicate match rule: 409.
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&route_body(&id, "/v1")),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{body}");

    // A foreign tenant's resource is indistinguishable from a missing one: the
    // caller below reads the very store the upstream was written into.
    let foreign_caller = Caller::default();
    let (status, _, _) = send(
        &app,
        &foreign_caller,
        "GET",
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}
