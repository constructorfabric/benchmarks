// Created: 2026-08-29 by Constructor Tech
//! Hierarchical merge semantics over the wire and through `domain::merge`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, parent, post, root, tenant};
use httpmock::MockServer;
use oagw::domain::merge::{ChainEntry, merge_chain, merge_plugins};
use oagw::domain::model::{PluginItem, PluginsConfig, Sharing, Upstream};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn upstream(alias: &str, server: &MockServer) -> Value {
    json!({
        "alias": alias,
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
    })
}

async fn route_for(harness: &common::Harness, alias: &str) -> String {
    let items = json_body(common::get(harness.router(), "/oagw/v1/upstreams", tenant()).await)
        .await["items"]
        .as_array()
        .unwrap()
        .clone();
    let id = items
        .iter()
        .find(|item| item["alias"] == alias)
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .unwrap_or_default();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

#[tokio::test]
async fn a_disabled_ancestor_disables_the_descendant() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    // Ancestor upstream, registered but disabled.
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("down.example.com", &server),
            parent(),
        )
        .await,
    )
    .await;
    let items = json_body(common::get(harness.router(), "/oagw/v1/upstreams", parent()).await)
        .await["items"]
        .as_array()
        .unwrap()
        .clone();
    let ancestor_id = items[0]["id"].as_str().unwrap().to_owned();
    let disabled = json_body(
        harness
            .send(
                "POST",
                &format!("/oagw/v1/upstreams/{ancestor_id}/disable"),
                Some(json!({ "enabled": false })),
                parent(),
            )
            .await,
    )
    .await;
    assert_eq!(disabled["enabled"], false);

    // The descendant supplies its own (enabled) upstream for the same alias.
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("down.example.com", &server),
            tenant(),
        )
        .await,
    )
    .await;
    let descendant_id = route_for(&harness, "down.example.com").await;
    assert!(!descendant_id.is_empty());

    let response = harness
        .send("GET", "/oagw/v1/proxy/down.example.com/api", None, tenant())
        .await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn cors_origins_are_unioned_when_inherited() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let ancestor = json!({
        "alias": "cors.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "cors": { "sharing": "inherit", "enabled": true, "allowed_origins": ["https://parent.example"] },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", ancestor, parent()).await).await;
    let descendant = json!({
        "alias": "cors.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "cors": { "enabled": true, "allowed_origins": ["https://child.example"] },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", descendant, tenant()).await).await;
    route_for(&harness, "cors.example.com").await;

    for origin in ["https://parent.example", "https://child.example"] {
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/cors.example.com/api")
            .header("origin", origin)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = harness.send_request(request, tenant()).await;
        assert_eq!(response.status(), 200, "origin {origin} must be allowed");
        assert_eq!(
            common::header(&response, "access-control-allow-origin").as_deref(),
            Some(origin)
        );
    }
}

#[tokio::test]
async fn an_enforced_ancestor_cors_block_wins() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = common::Harness::new(common::test_config(), None);
    let ancestor = json!({
        "alias": "enforced-cors.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "cors": { "sharing": "enforce", "enabled": true, "allowed_origins": ["https://parent.example"] },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", ancestor, parent()).await).await;
    let descendant = json!({
        "alias": "enforced-cors.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "cors": { "enabled": true, "allowed_origins": ["https://child.example"] },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", descendant, tenant()).await).await;
    route_for(&harness, "enforced-cors.example.com").await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/enforced-cors.example.com/api")
        .header("origin", "https://child.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(request, tenant()).await;
    assert_eq!(response.status(), 403);
    assert_eq!(
        json_body(response).await["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
}

#[tokio::test]
async fn an_enforced_ancestor_auth_block_survives_shadowing() {
    let server = MockServer::start();
    // Only answers when the ancestor's injected credential arrives.
    let target = server.mock(|when, then| {
        when.header("authorization", "ancestor-secret");
        then.status(200).body("ok");
    });

    let credstore = Arc::new(common::FakeCredStore::new(&[(
        "ancestor-key",
        "ancestor-secret",
    )]));
    let harness = common::Harness::new(common::test_config(), Some(credstore));

    let ancestor = json!({
        "alias": "auth.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "auth": {
            "sharing": "enforce",
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "api_key_ref": "cred://ancestor-key", "header_name": "authorization" },
        },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", ancestor, parent()).await).await;
    // The descendant overrides the endpoint set but cannot override the auth.
    let descendant = json!({
        "alias": "auth.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", descendant, tenant()).await).await;
    route_for(&harness, "auth.example.com").await;

    let response = harness
        .send("GET", "/oagw/v1/proxy/auth.example.com/api", None, tenant())
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        target.calls(),
        1,
        "the enforced ancestor credential must reach the upstream"
    );
}

#[tokio::test]
async fn ancestor_plugins_concatenate_before_route_plugins() {
    let upstream = Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant(),
        alias: "chain.example.com".to_owned(),
        alias_derived: false,
        created_at: "2026-08-29T00:00:00Z".to_owned(),
        updated_at: "2026-08-29T00:00:00Z".to_owned(),
        spec: serde_json::from_value(json!({
            "protocol": PROTOCOL,
            "server": { "endpoints": [ { "scheme": "https", "host": "chain.example.com" } ] },
            "plugins": { "items": ["ancestor-one", "ancestor-two"] },
        }))
        .unwrap(),
    };
    let chain = vec![
        ChainEntry {
            tenant_id: root(),
            upstream: None,
        },
        ChainEntry {
            tenant_id: tenant(),
            upstream: Some(upstream),
        },
    ];
    let effective = merge_chain(&chain);
    assert_eq!(
        effective
            .plugins
            .items
            .iter()
            .map(oagw::domain::model::PluginItem::reference)
            .collect::<Vec<_>>(),
        vec!["ancestor-one", "ancestor-two"]
    );

    // The route's own chain is appended after the upstream's (ADR-0002).
    let route: oagw::domain::model::Route = serde_json::from_value(json!({
        "id": Uuid::new_v4(),
        "tenant_id": tenant(),
        "created_at": "2026-08-29T00:00:00Z",
        "updated_at": "2026-08-29T00:00:00Z",
        "upstream_id": Uuid::new_v4(),
        "enabled": true,
        "match": { "http": { "methods": ["GET"], "path": "/api" } },
        "plugins": { "items": ["route-one"] },
    }))
    .unwrap();
    let merged = oagw::infra::proxy::service::with_route(&effective, &route);
    assert_eq!(
        merged
            .plugins
            .items
            .iter()
            .map(oagw::domain::model::PluginItem::reference)
            .collect::<Vec<_>>(),
        vec!["ancestor-one", "ancestor-two", "route-one"]
    );
}

#[test]
fn plugin_concatenation_dedupes_preserving_first_occurrence() {
    let ancestor = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![
            PluginItem::Reference("shared".to_owned()),
            PluginItem::Reference("a".to_owned()),
        ],
    };
    let descendant = PluginsConfig {
        sharing: Sharing::Private,
        items: vec![
            PluginItem::Reference("shared".to_owned()),
            PluginItem::Reference("b".to_owned()),
        ],
    };
    let merged = merge_plugins(ancestor, Some(descendant));
    assert_eq!(
        merged
            .items
            .iter()
            .map(PluginItem::reference)
            .collect::<Vec<_>>(),
        vec!["shared", "a", "b"]
    );
}

#[tokio::test]
async fn tags_always_union() {
    let server = MockServer::start();
    let harness = common::Harness::new(common::test_config(), None);
    let ancestor = json!({
        "alias": "tags.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "tags": ["ancestor-tag"],
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", ancestor, parent()).await).await;
    let descendant = json!({
        "alias": "tags.example.com",
        "protocol": PROTOCOL,
        "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
        "tags": ["descendant-tag"],
    });
    json_body(post(harness.router(), "/oagw/v1/upstreams", descendant, tenant()).await).await;

    // Tags are per-record, so each record keeps its own list: the union happens
    // in the effective configuration, never on the wire.
    let parent_items =
        json_body(common::get(harness.router(), "/oagw/v1/upstreams?$top=100", parent()).await)
            .await["items"]
            .as_array()
            .unwrap()
            .clone();
    let tenant_items =
        json_body(common::get(harness.router(), "/oagw/v1/upstreams?$top=100", tenant()).await)
            .await["items"]
            .as_array()
            .unwrap()
            .clone();
    assert_eq!(parent_items[0]["tags"], json!(["ancestor-tag"]));
    assert_eq!(tenant_items[0]["tags"], json!(["descendant-tag"]));
}

#[tokio::test]
async fn a_descendant_without_routes_inherits_the_ancestor_route() {
    // The parent owns the alias *and* the route; the child shadows the alias
    // with its own upstream but registers no route of its own. DESIGN §3.3:
    // ancestor routes are inherited at proxy time, while the closest tenant
    // still owns the routing target — so the child's upstream is the one hit.
    let ancestor_target = MockServer::start();
    let ancestor_mock = ancestor_target.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ancestor");
    });
    let descendant_target = MockServer::start();
    descendant_target.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("descendant");
    });

    let harness = common::Harness::new(common::test_config(), None);
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("inherited.example.com", &ancestor_target),
            parent(),
        )
        .await,
    )
    .await;
    // The parent's route for its own upstream.
    let parent_items = json_body(
        common::get(harness.router(), "/oagw/v1/upstreams", parent()).await,
    )
    .await["items"]
        .as_array()
        .unwrap()
        .clone();
    let parent_upstream = parent_items
        .iter()
        .find(|item| item["alias"] == "inherited.example.com")
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .unwrap_or_default();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": parent_upstream, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        parent(),
    )
    .await;

    // The descendant shadows the alias but adds no route.
    json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            upstream("inherited.example.com", &descendant_target),
            tenant(),
        )
        .await,
    )
    .await;

    // The management API still hides the ancestor: the child lists only its own.
    let visible = json_body(common::get(harness.router(), "/oagw/v1/upstreams", tenant()).await)
        .await["items"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(visible, 1, "ancestor resources stay invisible");

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/inherited.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200, "the inherited route matches");
    assert_eq!(
        String::from_utf8_lossy(&common::body_bytes(response).await),
        "descendant",
        "the closest tenant still owns the routing target"
    );
    assert_eq!(
        ancestor_mock.calls(),
        0,
        "the shadowed ancestor upstream is not contacted"
    );
}
