//! Plugin CRUD and the in-use rule (T052).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

const OAGW: &str = "/oagw/v1/plugins";

fn body(name: &str) -> serde_json::Value {
    json!({
        "kind": "guard",
        "name": name,
        "config": { "required_request_headers": "x-correlation-id" },
        "source": "def plugin(ctx):\n    return None\n"
    })
}

#[tokio::test]
async fn a_plugin_is_created_and_listed() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request("POST", OAGW, Some(body("correlation-guard"))))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = read_json(response).await;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    assert!(Uuid::parse_str(&id).is_ok(), "a plugin is addressed by UUID");
    assert_eq!(created["kind"], "guard");
    assert_eq!(created["name"], "correlation-guard");

    let listed = read_json(harness.send(harness.request("GET", OAGW, None)).await).await;
    let values = listed["value"].as_array().cloned().unwrap_or_default();
    assert!(values.iter().any(|p| p["id"] == created["id"]), "{values:?}");

    let fetched = harness
        .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
        .await;
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(read_json(fetched).await["name"], "correlation-guard");
}

#[tokio::test]
async fn the_source_document_is_served() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request("POST", OAGW, Some(body("sourced"))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let response = harness
        .send(harness.request("GET", &format!("{OAGW}/{id}/source"), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = read_json(response).await;
    assert_eq!(body["id"], id);
    assert!(body["source"].as_str().unwrap_or_default().contains("def plugin"), "{body}");
}

#[tokio::test]
async fn an_unreferenced_plugin_deletes_with_204() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request("POST", OAGW, Some(body("free"))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(
        harness
            .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        harness
            .send(harness.request("GET", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_referenced_plugin_deletes_with_409_and_names_its_referencers() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request("POST", OAGW, Some(body("bound"))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let upstream_id: String = {
        let response = harness
            .send(harness.request(
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({
                    "alias": "plugin-holder",
                    "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.7", "port": 443 } ] },
                    "protocol": "http",
                    "plugins": { "items": [id.clone()] }
                })),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        read_json(response).await["id"].as_str().unwrap_or_default().to_string()
    };
    // The route binds the plugin too, so both kinds of referencer are named.
    let route_id = {
        let response = harness
            .send(harness.request(
                "POST",
                "/oagw/v1/routes",
                Some(json!({
                    "upstream_id": upstream_id,
                    "match": { "http": { "methods": ["GET"], "path": "/v1/a", "path_suffix_mode": "append" } },
                    "plugins": { "items": [id.clone()] }
                })),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        read_json(response).await["id"].as_str().unwrap_or_default().to_string()
    };

    let deleted = harness
        .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
        .await;
    assert_eq!(deleted.status(), StatusCode::CONFLICT);
    let problem = read_json(deleted).await;
    assert_eq!(problem["status"], 409);
    assert_eq!(problem["plugin_id"], id, "the plugin is named");
    let referenced = &problem["referenced_by"];
    // The referencers are named by the ids the management API hands out.
    assert!(referenced["upstreams"].as_array().unwrap_or(&vec![]).iter().any(|v| *v == *upstream_id), "{problem}");
    assert!(referenced["routes"].as_array().unwrap_or(&vec![]).iter().any(|v| *v == *route_id), "{problem}");

    // Freeing both referencers makes the delete legal again.
    assert_eq!(
        harness
            .send(harness.request("DELETE", &format!("/oagw/v1/routes/{route_id}"), None))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let released = harness
        .send(harness.request("PUT", &format!("/oagw/v1/upstreams/{upstream_id}"), Some(json!({
            "alias": "plugin-holder",
            "server": { "endpoints": [ { "scheme": "https", "host": "10.0.0.7", "port": 443 } ] },
            "protocol": "http"
        }))))
        .await;
    assert_eq!(released.status(), StatusCode::OK);
    assert_eq!(
        harness
            .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn a_foreign_or_missing_plugin_is_not_found() {
    let harness = Harness::default_gear();
    let created = harness
        .send(harness.request("POST", OAGW, Some(body("mine"))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let foreign = harness.request_for("GET", &format!("{OAGW}/{id}"), None, other_tenant());
    assert_eq!(harness.send(foreign).await.status(), StatusCode::NOT_FOUND);

    let foreign_delete = harness.request_for("DELETE", &format!("{OAGW}/{id}"), None, other_tenant());
    assert_eq!(harness.send(foreign_delete).await.status(), StatusCode::NOT_FOUND);

    let unknown = harness.request("GET", &format!("{OAGW}/{}", Uuid::new_v4()), None);
    assert_eq!(harness.send(unknown).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_kind_is_rejected() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({ "kind": "scheduler", "name": "nope" })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
