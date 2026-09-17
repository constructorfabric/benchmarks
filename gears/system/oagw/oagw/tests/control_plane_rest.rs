//! Integration tests for the OAGW control-plane REST surface
//! (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`) driven
//! through the real axum router with an allow-all PEP mock.
//!
//! The management handlers are tenant-scoped: the platform system tenant
//! (`DEFAULT_TENANT_ID`) is in scope while any other tenant gets a 403.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use common::{SYSTEM, json_status, router, run, security};
use oagw::domain::repository::ControlPlaneService;

fn control() -> Arc<ControlPlaneService> {
    Arc::new(ControlPlaneService::new(0, Duration::from_secs(2), true))
}

#[test]
fn create_list_get_update_delete_upstream_roundtrip() {
    let app = router(control(), security(SYSTEM.0, SYSTEM.1));

    // Create.
    let (status, body) = run(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({
            "alias": "api-backend",
            "name": "API backend",
            "host": "api.example.com",
            "port": 443,
            "scheme": "https",
            "path_prefix": "/v1",
            "enabled": true,
            "timeout_secs": 3,
        })),
    );
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = json_status((status, body));
    assert_eq!(body["alias"], "api-backend");

    // List.
    let (status, body) = run(app.clone(), "GET", "/oagw/v1/upstreams", None);
    assert_eq!(status, StatusCode::OK);
    let (_, body) = json_status((status, body));
    let aliases: Vec<&str> = body
        .as_array()
        .expect("array body")
        .iter()
        .filter_map(|v| v.get("alias").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(aliases, vec!["api-backend"]);

    // Get.
    let (status, _) = run(app.clone(), "GET", "/oagw/v1/upstreams/api-backend", None);
    assert_eq!(status, StatusCode::OK);

    // Update.
    let (status, _) = run(
        app.clone(),
        "PUT",
        "/oagw/v1/upstreams/api-backend",
        Some(json!({
            "alias": "api-backend",
            "host": "api2.example.com",
            "scheme": "https",
            "enabled": false,
        })),
    );
    assert_eq!(status, StatusCode::OK);
    let (_, body) = json_status(run(
        app.clone(),
        "GET",
        "/oagw/v1/upstreams/api-backend",
        None,
    ));
    assert_eq!(body["host"], "api2.example.com");
    assert_eq!(body["enabled"], serde_json::Value::Bool(false));

    // Delete.
    let (status, _) = run(
        app.clone(),
        "DELETE",
        "/oagw/v1/upstreams/api-backend",
        None,
    );
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = run(app.clone(), "GET", "/oagw/v1/upstreams/api-backend", None);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn unknown_alias_yields_404_problem() {
    let app = router(control(), security(SYSTEM.0, SYSTEM.1));
    for uri in [
        "/oagw/v1/upstreams/nope",
        "/oagw/v1/routes/nope",
        "/oagw/v1/plugins/nope",
    ] {
        let (status, _) = run(app.clone(), "GET", uri, None);
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri} must 404");
    }
}

#[test]
fn route_referencing_unknown_upstream_is_rejected() {
    let app = router(control(), security(SYSTEM.0, SYSTEM.1));
    let (status, _) = run(
        app.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({
            "alias": "shop",
            "upstream_alias": "missing-upstream",
            "enabled": true,
        })),
    );
    assert!(
        status.is_client_error(),
        "unknown upstream must be rejected"
    );
}

#[test]
fn plugin_create_bind_unbind_roundtrip() {
    let app = router(control(), security(SYSTEM.0, SYSTEM.1));

    // Upstream + route to bind against.
    run(
        app.clone(),
        "POST",
        "/oagw/v1/upstreams",
        Some(json!({ "alias": "svc", "host": "svc.example.com", "scheme": "https" })),
    );
    run(
        app.clone(),
        "POST",
        "/oagw/v1/routes",
        Some(json!({ "alias": "shop", "upstream_alias": "svc", "enabled": true })),
    );

    // Create a plugin.
    let (status, _) = run(
        app.clone(),
        "POST",
        "/oagw/v1/plugins",
        Some(json!({
            "alias": "reqid",
            "kind": "request_id",
            "enabled": true,
            "config": {}
        })),
    );
    assert_eq!(status, StatusCode::CREATED);

    // Bind to the route.
    let (status, _) = run(
        app.clone(),
        "POST",
        "/oagw/v1/plugins/reqid/bind",
        Some(json!({ "route": "shop" })),
    );
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Ambiguous bind (both targets) must be rejected.
    let (status, _) = run(
        app.clone(),
        "POST",
        "/oagw/v1/plugins/reqid/bind",
        Some(json!({ "route": "shop", "upstream": "svc" })),
    );
    assert!(status.is_client_error());

    // Unbind.
    let (status, _) = run(
        app.clone(),
        "POST",
        "/oagw/v1/plugins/reqid/unbind",
        Some(json!({ "route": "shop" })),
    );
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Listing plugins shows the instance.
    let (_, body) = json_status(run(app.clone(), "GET", "/oagw/v1/plugins", None));
    assert!(
        body.as_array()
            .expect("array body")
            .iter()
            .any(|p| p["alias"] == "reqid")
    );
}

#[test]
fn foreign_tenant_is_denied() {
    let foreign = uuid::Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap();
    let app = router(control(), security(foreign, foreign));
    for uri in ["/oagw/v1/upstreams", "/oagw/v1/routes", "/oagw/v1/plugins"] {
        let (status, _) = run(app.clone(), "GET", uri, None);
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "GET {uri} must 403 for foreign tenant"
        );
    }
}

/// The PEP authz gate (not just tenant scope) must deny management calls with
/// a gateway-sourced 403 problem response (`inst-authz-forbidden` /
/// `inst-authz-error`).
#[test]
fn pep_denial_blocks_management_calls_with_403() {
    let app = common::router_with(
        control(),
        security(SYSTEM.0, SYSTEM.1),
        common::deny_enforcer(),
    );
    for (method, uri) in [
        ("GET", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/plugins"),
    ] {
        let (status, body) = json_status(run(app.clone(), method, uri, None));
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must 403 on PEP denial"
        );
        let problem = body;
        assert_eq!(problem["status"], 403);
        // Management-plane PEP denials surface the canonical authorized
        // `permission_denied` problem (the data-plane gate uses `auth.failed`).
        assert!(
            problem["type"]
                .as_str()
                .unwrap_or_default()
                .contains("permission_denied"),
            "{method} {uri} problem type: {}",
            problem["type"]
        );
        // The deny reason is surfaced as the canonical problem `reason`
        // (`PEP_DENIED: ...`), per the authorized management-error contract.
        let rendered = problem.to_string();
        assert!(
            rendered.contains("denied"),
            "{method} {uri} problem must carry the deny reason: {rendered}"
        );
    }
}
