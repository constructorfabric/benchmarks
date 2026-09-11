//! Integration tests for the plugin management API (`/oagw/v1/plugins`).

mod common;

use common::{base_config, build_router, create, empty_request, json_request, send};
use http::{Method, StatusCode};
use oagw::domain::model::PROTOCOL_HTTP;
use serde_json::json;

#[tokio::test]
async fn create_returns_201_and_the_id_renders_as_a_gts_identifier() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "plugin_type": "guard",
        "name": "my-guard",
        "source_code": "console.log('hi');",
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/plugins", &body),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let created = resp.json();
    assert_eq!(
        created["id"].as_str().expect("id present"),
        format!(
            "gts.cf.core.oagw.guard_plugin.v1~{}",
            created["uuid"].as_str().expect("uuid present")
        )
    );
}

#[tokio::test]
async fn get_returns_the_full_record_including_source_code() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "plugin_type": "transform",
        "name": "my-transform",
        "phases": ["on_request"],
        "source_code": "const SOURCE = 'exact-body';",
    });
    let created = create(&router, "/oagw/v1/plugins", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let resp = send(
        &router,
        empty_request(Method::GET, &format!("/oagw/v1/plugins/{id}")),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let fetched = resp.json();
    assert_eq!(
        fetched["source_code"],
        json!("const SOURCE = 'exact-body';")
    );
    assert_eq!(fetched["name"], json!("my-transform"));
}

#[tokio::test]
async fn get_source_returns_the_source_verbatim_as_text_plain() {
    let (router, _state) = build_router(base_config());
    let source = "line one\nline two\n";
    let body = json!({
        "plugin_type": "guard",
        "name": "verbatim-guard",
        "source_code": source,
    });
    let created = create(&router, "/oagw/v1/plugins", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let resp = send(
        &router,
        empty_request(Method::GET, &format!("/oagw/v1/plugins/{id}/source")),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert!(
        resp.header("content-type")
            .expect("content-type present")
            .starts_with("text/plain"),
        "unexpected content-type: {:?}",
        resp.header("content-type")
    );
    assert_eq!(
        resp.text(),
        source,
        "source must round-trip verbatim, byte for byte"
    );
}

#[tokio::test]
async fn delete_of_an_unreferenced_plugin_returns_204() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "plugin_type": "guard",
        "name": "unreferenced-guard",
        "source_code": "x",
    });
    let created = create(&router, "/oagw/v1/plugins", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let resp = send(
        &router,
        empty_request(Method::DELETE, &format!("/oagw/v1/plugins/{id}")),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
}

#[tokio::test]
async fn delete_of_a_plugin_bound_in_an_upstream_returns_409_naming_it() {
    let (router, _state) = build_router(base_config());
    let plugin_body = json!({
        "plugin_type": "guard",
        "name": "bound-guard",
        "source_code": "x",
    });
    let plugin = create(&router, "/oagw/v1/plugins", &plugin_body).await;
    let plugin_id = plugin["id"].as_str().expect("gts id present").to_owned();

    let upstream_body = json!({
        "alias": "svc-with-plugin",
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.2.1", "port": 443}]},
        "protocol": PROTOCOL_HTTP,
        "plugins": {"items": [plugin_id]},
    });
    let upstream = create(&router, "/oagw/v1/upstreams", &upstream_body).await;
    let upstream_uuid = upstream["uuid"].as_str().expect("uuid present").to_owned();

    let plugin_uuid = plugin["uuid"].as_str().expect("uuid present");
    let resp = send(
        &router,
        empty_request(Method::DELETE, &format!("/oagw/v1/plugins/{plugin_uuid}")),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());
    let body = resp.json();
    let names_upstream = body["context"]["referenced_by"]["upstreams"]
        .as_array()
        .expect("referenced_by.upstreams is an array")
        .iter()
        .any(|v| v.as_str() == Some(upstream_uuid.as_str()));
    assert!(
        names_upstream,
        "referenced_by.upstreams must name the binding upstream: {body}"
    );
}

#[tokio::test]
async fn put_to_a_plugin_does_not_reach_a_handler() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "plugin_type": "guard",
        "name": "put-target-guard",
        "source_code": "x",
    });
    let created = create(&router, "/oagw/v1/plugins", &body).await;
    let id = created["uuid"].as_str().expect("uuid present");

    let resp = send(
        &router,
        json_request(
            Method::PUT,
            &format!("/oagw/v1/plugins/{id}"),
            &json!({"name": "renamed"}),
        ),
    )
    .await;
    assert!(
        resp.status == StatusCode::METHOD_NOT_ALLOWED || resp.status == StatusCode::NOT_FOUND,
        "PUT must not be routed to a handler; got {}",
        resp.status
    );
}

#[tokio::test]
async fn a_transform_plugin_with_empty_phases_is_rejected() {
    let (router, _state) = build_router(base_config());
    let body = json!({
        "plugin_type": "transform",
        "name": "phaseless-transform",
        "source_code": "x",
    });
    let resp = send(
        &router,
        json_request(Method::POST, "/oagw/v1/plugins", &body),
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "a transform plugin needs at least one phase: {}",
        resp.text()
    );
}
