// Created: 2026-08-29 by Constructor Tech
//! Upstream management CRUD, validation and OData paging.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{get, header, json_body, post, put, tenant};
use serde_json::{Value, json};
use uuid::Uuid;

fn upstream(alias: Option<&str>, host: &str) -> Value {
    json!({
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": host, "port": 443 } ] },
    })
}

#[tokio::test]
async fn create_returns_201_with_location_and_dto() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(None, "api.example.com"),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 201);
    let location = header(&response, "location").expect("location header");
    let body = json_body(response).await;
    assert_eq!(body["alias"], "api.example.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["tenant_id"], tenant().to_string());
    assert!(body["created_at"].is_string());
    let id = body["id"].as_str().unwrap();
    assert!(
        location.ends_with(id),
        "location '{location}' must end with '{id}'"
    );
}

#[tokio::test]
async fn list_get_put_delete_round_trip() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, "round.trip.example.com"),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let list = json_body(get(harness.router(), "/oagw/v1/upstreams", tenant()).await).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["page_info"]["limit"], 50);
    assert_eq!(list["items"][0]["id"], id.as_str());

    let fetched = json_body(
        get(
            harness.router(),
            &format!("/oagw/v1/upstreams/{id}"),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(fetched["alias"], "round.trip.example.com");

    let replaced = json!({
        "alias": "round.trip.example.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "round.trip.example.com", "port": 443 } ] },
        "tags": ["edge"],
    });
    let replaced = json_body(
        put(
            harness.router(),
            &format!("/oagw/v1/upstreams/{id}"),
            replaced,
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(replaced["tags"], json!(["edge"]));
    assert_eq!(replaced["id"], id.as_str());

    let response = harness
        .send(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 204);
    let response = get(
        harness.router(),
        &format!("/oagw/v1/upstreams/{id}"),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 404);
    let response = harness
        .send(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn missing_resource_is_404() {
    let harness = common::Harness::new(common::test_config(), None);
    let missing = Uuid::new_v4();
    let response = get(
        harness.router(),
        &format!("/oagw/v1/upstreams/{missing}"),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 404);
    let response = put(
        harness.router(),
        &format!("/oagw/v1/upstreams/{missing}"),
        upstream(None, "x.example.com"),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn enable_and_disable_toggle_the_flag() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, "toggle.example.com"),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let disabled = json_body(
        harness
            .send(
                "POST",
                &format!("/oagw/v1/upstreams/{id}/disable"),
                Some(json!({"enabled": false})),
                tenant(),
            )
            .await,
    )
    .await;
    assert_eq!(disabled["enabled"], false);

    let enabled = json_body(
        harness
            .send(
                "POST",
                &format!("/oagw/v1/upstreams/{id}/enable"),
                Some(json!({"enabled": true})),
                tenant(),
            )
            .await,
    )
    .await;
    assert_eq!(enabled["enabled"], true);
}

#[tokio::test]
async fn validation_rejects_bad_payloads() {
    let harness = common::Harness::new(common::test_config(), None);
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "alias": "no-server.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" }),
            "missing server",
        ),
        (
            json!({ "alias": "no-protocol.example.com", "server": { "endpoints": [ { "scheme": "https", "host": "h" } ] } }),
            "missing protocol",
        ),
        (
            json!({ "alias": "empty-pool.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "server": { "endpoints": [] } }),
            "empty endpoint pool",
        ),
        (
            json!({ "alias": "bad-port.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "server": { "endpoints": [ { "scheme": "https", "host": "h.example.com", "port": 0 } ] } }),
            "port out of range",
        ),
        (
            json!({ "alias": "bad-scheme.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "server": { "endpoints": [ { "scheme": "ftp", "host": "h.example.com" } ] } }),
            "unknown scheme",
        ),
        (
            json!({ "alias": "bad-tag.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "server": { "endpoints": [ { "scheme": "https", "host": "h.example.com" } ] }, "tags": ["Bad Tag"] }),
            "tag pattern",
        ),
        (
            json!({ "alias": "unknown.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.mqtt.v1", "server": { "endpoints": [ { "scheme": "https", "host": "h.example.com" } ] } }),
            "unknown protocol",
        ),
        (
            json!({ "alias": "extra.example.com", "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1", "server": { "endpoints": [ { "scheme": "https", "host": "h.example.com" } ] }, "surprise": 1 }),
            "unknown field",
        ),
    ];
    for (payload, why) in cases {
        let response = post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await;
        assert_eq!(response.status(), 400, "expected 400 for {why}");
        let body = json_body(response).await;
        assert_eq!(body["status"], 400, "problem document for {why}");
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }
}

#[tokio::test]
async fn duplicate_alias_is_a_conflict() {
    let harness = common::Harness::new(common::test_config(), None);
    let first = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(Some("dup.example.com"), "dup.example.com"),
        tenant(),
    )
    .await;
    assert_eq!(first.status(), 201);
    let second = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(Some("dup.example.com"), "other.example.com"),
        tenant(),
    )
    .await;
    assert_eq!(second.status(), 400);
    let body = json_body(second).await;
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("dup.example.com"),
        "detail must name the alias: {detail}"
    );
}

#[tokio::test]
async fn top_is_clamped_and_defaults_to_fifty() {
    let harness = common::Harness::new(common::test_config(), None);
    for index in 0..3 {
        let payload = upstream(None, &format!("host-{index}.example.com"));
        let response = post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await;
        assert_eq!(response.status(), 201);
    }

    let default_page = json_body(get(harness.router(), "/oagw/v1/upstreams", tenant()).await).await;
    assert_eq!(default_page["page_info"]["limit"], 50);
    assert_eq!(default_page["items"].as_array().unwrap().len(), 3);

    let clamped =
        json_body(get(harness.router(), "/oagw/v1/upstreams?$top=500", tenant()).await).await;
    assert_eq!(clamped["page_info"]["limit"], 100);

    let offset = json_body(
        get(
            harness.router(),
            "/oagw/v1/upstreams?$top=2&$skip=1",
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(offset["page_info"]["limit"], 2);
    assert_eq!(offset["items"].as_array().unwrap().len(), 2);

    let ordered = json_body(
        get(
            harness.router(),
            "/oagw/v1/upstreams?$orderby=created_at%20desc",
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(ordered["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn malformed_json_is_a_validation_error() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({ "alias": "half" })),
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 400);
}
