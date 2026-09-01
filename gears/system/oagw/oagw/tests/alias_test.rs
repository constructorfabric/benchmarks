// Created: 2026-08-29 by Constructor Tech
//! Alias derivation over the management API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{get, json_body, post, put, security_for, tenant};
use serde_json::{Value, json};
use uuid::Uuid;

fn upstream(alias: Option<&str>, hosts: &[(&str, u16)]) -> Value {
    let endpoints: Vec<Value> = hosts
        .iter()
        .map(|(host, port)| json!({ "scheme": "https", "host": host, "port": port }))
        .collect();
    json!({
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": endpoints },
    })
}

fn page_items(body: Value) -> Value {
    body["items"].clone()
}

#[tokio::test]
async fn single_host_standard_port_is_derived() {
    let harness = common::Harness::new(common::test_config(), None);
    let body = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, &[("api.openai.com", 443)]),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(body["alias"], "api.openai.com");
}

#[tokio::test]
async fn single_host_non_standard_port_gets_the_port() {
    let harness = common::Harness::new(common::test_config(), None);
    let body = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, &[("api.openai.com", 8443)]),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(body["alias"], "api.openai.com:8443");
}

#[tokio::test]
async fn common_suffix_is_derived_from_the_pool() {
    let harness = common::Harness::new(common::test_config(), None);
    let body = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, &[("us.vendor.com", 443), ("eu.vendor.com", 443)]),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(body["alias"], "vendor.com");
}

#[tokio::test]
async fn bare_public_suffix_requires_an_alias() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(None, &[("foo.co.uk", 443), ("bar.co.uk", 443)]),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn ip_endpoints_require_an_alias() {
    let harness = common::Harness::new(common::test_config(), None);
    let response = post(
        harness.router(),
        "/oagw/v1/upstreams",
        upstream(None, &[("10.0.1.1", 443)]),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);

    let supplied = upstream(Some("my-service"), &[("10.0.1.1", 443)]);
    let body =
        json_body(post(harness.router(), "/oagw/v1/upstreams", supplied, tenant()).await).await;
    assert_eq!(body["alias"], "my-service");
}

#[tokio::test]
async fn differing_alias_is_rejected_with_400() {
    let harness = common::Harness::new(common::test_config(), None);
    let payload = upstream(Some("wrong.example.com"), &[("api.openai.com", 443)]);
    let response = post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn exact_alias_is_tolerated() {
    let harness = common::Harness::new(common::test_config(), None);
    let payload = upstream(Some("api.openai.com"), &[("api.openai.com", 443)]);
    let response = post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await;
    assert_eq!(response.status(), 201);
}

#[tokio::test]
async fn alias_is_immutable_on_replace() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, &[("api.openai.com", 443)]),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let changed = upstream(Some("api.openai.com"), &[("api.other.com", 443)]);
    let response = put(
        harness.router(),
        &format!("/oagw/v1/upstreams/{id}"),
        changed,
        tenant(),
    )
    .await;
    assert_eq!(
        response.status(),
        400,
        "endpoint change that moves the alias must be rejected"
    );

    let same = upstream(Some("api.openai.com"), &[("api.openai.com", 443)]);
    let response = put(
        harness.router(),
        &format!("/oagw/v1/upstreams/{id}"),
        same,
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn alias_normalization_is_lowercase_without_trailing_dot() {
    let harness = common::Harness::new(common::test_config(), None);
    let payload = upstream(Some("My-Service."), &[("10.0.1.1", 443)]);
    let body =
        json_body(post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await).await;
    assert_eq!(body["alias"], "my-service");
}

#[tokio::test]
async fn rfc1123_rejections() {
    let harness = common::Harness::new(common::test_config(), None);
    for bad in [
        "-leading.example.com",
        "trailing-.example.com",
        "under_score.example.com",
        "",
    ] {
        let payload = upstream(Some(bad), &[("10.0.1.1", 443)]);
        let response = post(harness.router(), "/oagw/v1/upstreams", payload, tenant()).await;
        assert_eq!(response.status(), 400, "alias '{bad}' must be rejected");
    }
}

#[tokio::test]
async fn tenant_scoping_keeps_upstreams_separate() {
    let harness = common::Harness::new(common::test_config(), None);
    let other = Uuid::new_v4();
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(None, &[("api.openai.com", 443)]),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let list = json_body(get(harness.router(), "/oagw/v1/upstreams", other).await).await;
    assert_eq!(page_items(list), json!([]));
    let response = get(harness.router(), &format!("/oagw/v1/upstreams/{id}"), other).await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn descendant_shadows_ancestor_alias() {
    let harness = common::Harness::new(common::test_config(), None);
    let ancestor = security_for(common::parent());
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(Some("shared-service"), &[("10.0.0.1", 443)]),
            ancestor.subject_tenant_id(),
        )
        .await,
    )
    .await;
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream(Some("shared-service"), &[("10.0.0.2", 443)]),
            tenant(),
        )
        .await,
    )
    .await;
    let list = json_body(get(harness.router(), "/oagw/v1/upstreams", tenant()).await).await;
    assert_eq!(page_items(list).as_array().unwrap().len(), 1);
}
