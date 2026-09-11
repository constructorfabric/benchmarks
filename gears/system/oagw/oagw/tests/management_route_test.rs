//! Route CRUD contract (T023).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

const OAGW: &str = "/oagw/v1/routes";

async fn an_upstream() -> (Harness, String) {
    let harness = Harness::default_gear();
    let id = create_upstream(&harness, "shared-ip", "10.0.0.5", 443, "https").await;
    (harness, id)
}

#[tokio::test]
async fn a_route_is_created_with_a_generated_id() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/models", &["GET", "POST"]))))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = read_json(response).await;
    assert_eq!(body["upstream_id"], upstream_id);
    assert_eq!(body["match"]["http"]["path"], "/v1/models");
    assert_eq!(body["match"]["http"]["methods"], json!(["GET", "POST"]));
    assert!(body["id"].as_str().is_some());
}

#[tokio::test]
async fn an_unknown_upstream_id_is_rejected_with_400() {
    let harness = Harness::default_gear();
    let response = harness
        .send(harness.request("POST", OAGW, Some(route_body(&Uuid::new_v4().to_string(), "/v1/x", &["GET"]))))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_foreign_upstream_id_is_rejected_with_400() {
    let (harness, upstream_id) = an_upstream().await;
    let foreign = harness.request_for(
        "POST",
        OAGW,
        Some(route_body(&upstream_id, "/v1/x", &["GET"])),
        other_tenant(),
    );
    assert_eq!(harness.send(foreign).await.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_empty_method_list_is_rejected() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/x", &[]))))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_method_is_rejected() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["TRACE"], "path": "/v1/x" } }
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_empty_path_is_rejected() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "", &["GET"]))))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn both_match_rules_are_rejected() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "upstream_id": upstream_id,
                "match": {
                    "http": { "methods": ["GET"], "path": "/v1/x" },
                    "grpc": { "service": "svc.Example", "method": "Get" }
                }
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn neither_match_rule_is_rejected() {
    let (harness, upstream_id) = an_upstream().await;
    let response = harness
        .send(harness.request(
            "POST",
            OAGW,
            Some(json!({
                "upstream_id": upstream_id,
                "match": {}
            })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_duplicate_match_rule_is_a_conflict() {
    let (harness, upstream_id) = an_upstream().await;
    let first = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/x", &["GET"]))))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let duplicate = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/x", &["GET"]))))
        .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    let body = read_json(duplicate).await;
    assert_eq!(body["status"], 409);
}

#[tokio::test]
async fn a_foreign_or_missing_id_is_not_found() {
    let (harness, upstream_id) = an_upstream().await;
    let created = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/x", &["GET"]))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let foreign = harness.request_for("GET", &format!("{OAGW}/{id}"), None, other_tenant());
    assert_eq!(harness.send(foreign).await.status(), StatusCode::NOT_FOUND);

    let unknown = harness.request("GET", &format!("{OAGW}/{}", Uuid::new_v4()), None);
    assert_eq!(harness.send(unknown).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_put_preserves_the_upstream_and_revalidates_uniqueness() {
    let (harness, upstream_id) = an_upstream().await;
    let created = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/first", &["GET"]))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();

    let updated = harness
        .send(harness.request(
            "PUT",
            &format!("{OAGW}/{id}"),
            Some(route_body(&upstream_id, "/v1/second", &["POST"])),
        ))
        .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let body = read_json(updated).await;
    assert_eq!(body["upstream_id"], upstream_id);
    assert_eq!(body["match"]["http"]["path"], "/v1/second");

    // A colliding path on another route is rejected.
    harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/third", &["GET"]))))
        .await;
    let colliding = harness
        .send(harness.request(
            "PUT",
            &format!("{OAGW}/{id}"),
            Some(route_body(&upstream_id, "/v1/third", &["GET"])),
        ))
        .await;
    assert_eq!(colliding.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_delete_answers_204() {
    let (harness, upstream_id) = an_upstream().await;
    let created = harness
        .send(harness.request("POST", OAGW, Some(route_body(&upstream_id, "/v1/x", &["GET"]))))
        .await;
    let id = read_json(created).await["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(
        harness
            .send(harness.request("DELETE", &format!("{OAGW}/{id}"), None))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn the_list_filters_by_upstream() {
    let (harness, upstream_id) = an_upstream().await;
    let other = create_upstream(&harness, "second-ip", "10.0.0.6", 443, "https").await;
    create_route(&harness, &upstream_id, "/v1/a", &["GET"]).await;
    create_route(&harness, &other, "/v1/b", &["GET"]).await;

    let response = harness
        .send(harness.request(
            "GET",
            &format!("{OAGW}?$filter=upstream_id%20eq%20'{upstream_id}'"),
            None,
        ))
        .await;
    let body = read_json(response).await;
    let values = body["value"].as_array().cloned().unwrap_or_default();
    assert!(values.len() >= 1, "{values:?}");
    assert!(
        values.iter().all(|r| r["upstream_id"] == upstream_id),
        "every row belongs to the filtered upstream: {values:?}"
    );
}
