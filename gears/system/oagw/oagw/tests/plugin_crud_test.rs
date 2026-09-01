// Created: 2026-08-29 by Constructor Tech
//! Custom-plugin registration, immutability and reference protection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{get, json_body, post, tenant};
use serde_json::{Value, json};
use uuid::Uuid;

const SOURCE: &str = "def apply(ctx):\n    return ctx\n";

fn plugin(plugin_type: &str) -> Value {
    json!({
        "plugin_type": plugin_type,
        "name": "my-guard",
        "source_code": SOURCE,
        "config_schema": { "type": "object" },
    })
}

#[tokio::test]
async fn register_list_get_delete() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/plugins",
            plugin("guard_plugin"),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(created["plugin_type"], "guard_plugin");
    assert_eq!(created["name"], "my-guard");
    assert_eq!(created["config_schema"]["type"], "object");
    let id = created["id"].as_str().unwrap().to_owned();

    let list = json_body(get(harness.router(), "/oagw/v1/plugins", tenant()).await).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], id.as_str());

    let fetched = json_body(
        get(
            harness.router(),
            &format!("/oagw/v1/plugins/{id}"),
            tenant(),
        )
        .await,
    )
    .await;
    assert_eq!(fetched["name"], "my-guard");

    let response = harness
        .send("DELETE", &format!("/oagw/v1/plugins/{id}"), None, tenant())
        .await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        get(
            harness.router(),
            &format!("/oagw/v1/plugins/{id}"),
            tenant()
        )
        .await
        .status(),
        404
    );
}

#[tokio::test]
async fn source_returns_starlark_text() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/plugins",
            plugin("transform_plugin"),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let response = get(
        harness.router(),
        &format!("/oagw/v1/plugins/{id}/source"),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        common::header(&response, "content-type").as_deref(),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        common::body_bytes(response).await.as_ref(),
        SOURCE.as_bytes()
    );

    let missing = Uuid::new_v4();
    assert_eq!(
        get(
            harness.router(),
            &format!("/oagw/v1/plugins/{missing}/source"),
            tenant()
        )
        .await
        .status(),
        404
    );
}

#[tokio::test]
async fn delete_is_refused_while_referenced() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/plugins",
            plugin("transform_plugin"),
            tenant(),
        )
        .await,
    )
    .await;
    let plugin_id = created["id"].as_str().unwrap().to_owned();

    let upstream = json!({
        "alias": "plugin-user.example.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "plugin-user.example.com", "port": 443 } ] },
        "plugins": { "items": [plugin_id] },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();

    let response = harness
        .send(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 409);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    assert_eq!(body["referenced_by"]["upstreams"], json!([upstream_id]));
    assert_eq!(body["referenced_by"]["routes"], json!([]));

    // Still registered after the refused delete.
    assert_eq!(
        get(
            harness.router(),
            &format!("/oagw/v1/plugins/{plugin_id}"),
            tenant()
        )
        .await
        .status(),
        200
    );
}

#[tokio::test]
async fn delete_is_refused_for_a_route_reference() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/plugins",
            plugin("guard_plugin"),
            tenant(),
        )
        .await,
    )
    .await;
    let plugin_id = created["id"].as_str().unwrap().to_owned();

    let upstream = json!({
        "alias": "route-plugin-user.example.com",
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "scheme": "https", "host": "route-plugin-user.example.com", "port": 443 } ] },
    });
    let upstream =
        json_body(post(harness.router(), "/oagw/v1/upstreams", upstream, tenant()).await).await;
    let route = json!({
        "upstream_id": upstream["id"],
        "plugins": { "items": [plugin_id] },
        "match": { "http": { "methods": ["GET"], "path": "/api" } },
    });
    let route = json_body(post(harness.router(), "/oagw/v1/routes", route, tenant()).await).await;

    let response = harness
        .send(
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin_id}"),
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 409);
    let body = json_body(response).await;
    assert_eq!(body["referenced_by"]["routes"], json!([route["id"]]));
}

#[tokio::test]
async fn plugin_validation() {
    let harness = common::Harness::new(common::test_config(), None);
    let cases: Vec<(Value, u16)> = vec![
        (plugin("cronjob"), 400),
        (
            json!({ "plugin_type": "guard_plugin", "name": "   ", "source_code": "x" }),
            400,
        ),
        (
            json!({ "plugin_type": "guard_plugin", "name": "no-source", "source_code": "" }),
            201,
        ),
    ];
    for (payload, expected) in cases {
        let response = post(harness.router(), "/oagw/v1/plugins", payload, tenant()).await;
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn plugins_are_tenant_scoped() {
    let harness = common::Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/plugins",
            plugin("guard_plugin"),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let other = Uuid::new_v4();
    assert_eq!(
        get(harness.router(), &format!("/oagw/v1/plugins/{id}"), other)
            .await
            .status(),
        404
    );
    let list = json_body(get(harness.router(), "/oagw/v1/plugins", other).await).await;
    assert_eq!(list["items"], json!([]));
}
