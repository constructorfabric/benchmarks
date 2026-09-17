//! Data-plane (proxy) integration tests against real [`httpmock`] upstreams.

mod common;

use std::sync::Arc;

use axum::body::Body;
use httpmock::{Method, MockServer};
use serde_json::json;
use uuid::Uuid;

use common::{call, post_json, router, sec, tenant};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Start a mock upstream and create an OAGW gateway whose `gw` alias points
/// at it. `extra` keys are merged into the upstream create body.
async fn gw_for(extra: serde_json::Value) -> (axum::Router, MockServer, Uuid) {
    let t = tenant();
    let server = MockServer::start_async().await;
    let base = server.base_url();
    let (host, port) = base
        .strip_prefix("http://")
        .expect("http mock")
        .rsplit_once(':')
        .expect("host:port");
    let port: u16 = port.parse().expect("numeric port");

    let mut body = json!({
        "enabled": true,
        "alias": "gw",
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
        "protocol": PROTOCOL_HTTP,
        "plugins": { "sharing": "inherit", "items": [] },
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        body[k] = v.clone();
    }

    let app = router(common::test_state(t), sec(t));
    let (status, value) = post_json(&app, "/oagw/v1/upstreams", body).await;
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "upstream creation: {}",
        value
    );
    let uid = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
    (app, server, uid)
}

/// Create a route for `uid`; `extra` keys are merged into the route body.
async fn add_route(
    app: &axum::Router,
    uid: Uuid,
    path: &str,
    methods: &[&str],
    extra: serde_json::Value,
) {
    let mut body = json!({
        "tags": [],
        "upstream_id": uid.to_string(),
        "match": { "http": { "methods": methods, "path": path } },
        "plugins": { "sharing": "inherit", "items": [] },
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        body[k] = v.clone();
    }
    let (status, value) = post_json(app, "/oagw/v1/routes", body).await;
    assert_eq!(
        status,
        axum::http::StatusCode::CREATED,
        "route creation: {}",
        value
    );
}

async fn get(
    app: &axum::Router,
    uri: &str,
) -> (axum::http::StatusCode, axum::http::HeaderMap, serde_json::Value) {
    call(app, "GET", uri, &[], Body::empty()).await
}

#[tokio::test]
async fn proxies_request_with_path_suffix_and_marks_upstream() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/hello");
            then.status(200).body("{\"ok\":true}");
        })
        .await;

    let (status, headers, value) = get(&app, "/oagw/v1/proxy/gw/hello").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(
        headers.get("x-oagw-error-source").unwrap(),
        "upstream",
        "successful proxying is marked as upstream-originated"
    );
    assert_eq!(m.calls_async().await, 1);
}

#[tokio::test]
async fn longest_prefix_route_wins() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(&app, uid, "/v1", &["GET"], json!({})).await;
    add_route(&app, uid, "/v1/users", &["GET"], json!({})).await;

    let m_status = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/v1/status");
            then.status(200);
        })
        .await;
    let m_user = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/v1/users/42");
            then.status(200).body("{\"user\":42}");
        })
        .await;

    let (status, _h, _value) = get(&app, "/oagw/v1/proxy/gw/v1/status").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m_status.calls_async().await, 1);
    assert_eq!(m_user.calls_async().await, 0);

    let (status, _h, value) = get(&app, "/oagw/v1/proxy/gw/v1/users/42").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(value, json!({"user": 42}));
    assert_eq!(m_user.calls_async().await, 1);
}

#[tokio::test]
async fn unknown_alias_is_404_gateway_error() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));
    let (status, headers, value) = get(&app, "/oagw/v1/proxy/ghost/x").await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert!(value["type"]
        .as_str()
        .unwrap()
        .contains("route.not_found"));
}

#[tokio::test]
async fn known_alias_without_route_is_404() {
    let (app, server, _uid) = gw_for(json!({})).await;
    let any = server
        .mock_async(|_when, then| { then.status(599); })
        .await;
    let (status, headers, value) = get(&app, "/oagw/v1/proxy/gw/x").await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert!(value["type"].as_str().unwrap().contains("route.not_found"));
    assert_eq!(any.calls_async().await, 0, "upstream never contacted");
}

#[tokio::test]
async fn method_not_allowed_is_400_validation() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;
    let any = server
        .mock_async(|_when, then| { then.status(599); })
        .await;

    let (status, headers, value) =
        call(&app, "POST", "/oagw/v1/proxy/gw/x", &[], Body::empty()).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert!(value["type"].as_str().unwrap().contains("validation"));
    assert_eq!(any.calls_async().await, 0, "upstream never contacted");
}

#[tokio::test]
async fn query_allowlist_enforced_and_passed_through() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(
        &app,
        uid,
        "/",
        &["GET"],
        json!({ "match": {
            "http": { "methods": ["GET"], "path": "/", "query_allowlist": ["a"] }
        }}),
    )
    .await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/x").query_param("a", "1");
            then.status(200);
        })
        .await;
    let any = server
        .mock_async(|_when, then| { then.status(599); })
        .await;

    // Non-allowlisted query param → rejected before forwarding.
    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/x?a=1&b=2").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(any.calls_async().await, 0, "upstream never contacted");

    // Allowlisted param forwards.
    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/x?a=1").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1);
}

#[tokio::test]
async fn route_without_allowlist_forwards_query_untouched() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/x").query_param("q", "9");
            then.status(200);
        })
        .await;

    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/x?q=9").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1);
}

#[tokio::test]
async fn path_suffix_disabled_rejects_extra_path() {
    let (app, server, uid) = gw_for(json!({})).await;
    add_route(
        &app,
        uid,
        "/base",
        &["GET"],
        json!({ "match": {
            "http": { "methods": ["GET"], "path": "/base", "path_suffix_mode": "disabled" }
        }}),
    )
    .await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/base");
            then.status(200);
        })
        .await;

    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/base").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1);

    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/base/more").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(m.calls_async().await, 1, "no extra upstream call");
}

#[tokio::test]
async fn rate_limit_rejects_with_429_and_headers() {
    let (app, server, uid) = gw_for(json!({
        "rate_limit": {
            "sharing": "inherit",
            "algorithm": "token_bucket",
            "sustained": { "rate": 1, "window": "second" },
            "scope": "global",
            "strategy": "reject",
            "cost": 1,
        }
    }))
    .await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/x");
            then.status(200);
        })
        .await;

    // First request consumes the single-token bucket and reaches upstream.
    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/x").await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // Second request is rejected before any upstream contact — had it been
    // forwarded it would have matched `m` again (calls would be 2).
    let (status, headers, value) = get(&app, "/oagw/v1/proxy/gw/x").await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.get("retry-after").is_some());
    assert_eq!(headers.get("x-rate-limit-limit").unwrap(), "1");
    assert_eq!(headers.get("x-rate-limit-remaining").unwrap(), "0");
    assert!(headers.get("x-rate-limit-reset").is_some());
    assert!(value["type"].as_str().unwrap().contains("rate_limit"));
    assert_eq!(m.calls_async().await, 1, "exactly one upstream call");
}

#[tokio::test]
async fn cors_preflight_is_permissive_204_echo() {
    let t = tenant();
    let app = router(common::test_state(t), sec(t));

    // Unconditional at the handler level — even for an unknown alias.
    let (status, headers, _v) = call(
        &app,
        "OPTIONS",
        "/oagw/v1/proxy/nothing/any",
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "x-tenant"),
        ],
        Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert_eq!(headers.get("access-control-allow-methods").unwrap(), "GET");
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "x-tenant"
    );
    assert_eq!(headers.get("access-control-max-age").unwrap(), "86400");
}

#[tokio::test]
async fn cors_actual_request_blocks_disallowed_origin() {
    let (app, server, uid) = gw_for(json!({
        "cors": {
            "sharing": "inherit",
            "enabled": true,
            "allowed_origins": ["https://ok.example"],
        }
    }))
    .await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/x");
            then.status(200);
        })
        .await;
    let any = server
        .mock_async(|_when, then| { then.status(599); })
        .await;

    // Unknown origin → 403 before the upstream is contacted.
    let (status, headers, _v) = call(
        &app,
        "GET",
        "/oagw/v1/proxy/gw/x",
        &[("origin", "https://evil.example")],
        Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(any.calls_async().await, 0, "upstream never contacted");

    // Allowed origin → proxied, with CORS response headers added.
    let (status, headers, _v) = call(
        &app,
        "GET",
        "/oagw/v1/proxy/gw/x",
        &[("origin", "https://ok.example")],
        Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://ok.example"
    );
    assert_eq!(m.calls_async().await, 1);
}

#[tokio::test]
async fn guard_plugin_requires_header() {
    let t = tenant();
    let server = MockServer::start_async().await;
    let base = server.base_url();
    let (host, port) = base
        .strip_prefix("http://")
        .unwrap()
        .rsplit_once(':')
        .unwrap();
    let port: u16 = port.parse().unwrap();

    let app = router(common::test_state(t), sec(t));

    // Custom guard plugin carrying the required-request-header config
    // (enforcement needs a configurable plugin — required_headers is
    // fail-open when bound as a bare builtin identifier). Bind by the
    // plugin's resource id returned at creation.
    let (status, v) = post_json(
        &app,
        "/oagw/v1/plugins",
        json!({
            "name": "req-tenant",
            "plugin_type": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.custom.v1",
            "config": { "required_request_headers": "x-tenant" },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);
    let pid = v["id"].as_str().unwrap().to_string();
    let plugin_full = format!("gts.cf.core.oagw.guard_plugin.v1~{pid}");

    let upstream = json!({
        "enabled": true,
        "alias": "gw",
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
        "protocol": PROTOCOL_HTTP,
        "headers": { "request": { "passthrough": "all" } },
        "plugins": { "sharing": "inherit", "items": [ plugin_full ] },
    });
    let (status, value) = post_json(&app, "/oagw/v1/upstreams", upstream).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "upstream: {value}");
    let uid = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET).path("/x").header("x-tenant", "v1");
            then.status(200);
        })
        .await;

    // Missing required header → 400 gateway validation, upstream untouched.
    let (status, headers, _v) = get(&app, "/oagw/v1/proxy/gw/x").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
    assert_eq!(m.calls_async().await, 0);

    // Present → proxied and forwarded.
    let (status, _h, _v) = call(
        &app,
        "GET",
        "/oagw/v1/proxy/gw/x",
        &[("x-tenant", "v1")],
        Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1);
}

#[tokio::test]
async fn request_id_transform_injects_header() {
    let (app, server, uid) = gw_for(json!({
        "plugins": { "sharing": "inherit", "items": [
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        ]},
    }))
    .await;
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET)
                .path("/x")
                .header_exists("x-request-id");
            then.status(200);
        })
        .await;

    let (status, _h, _v) = get(&app, "/oagw/v1/proxy/gw/x").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1, "x-request-id was sent upstream");
}

#[tokio::test]
async fn custom_guard_plugin_by_uuid_and_apikey_secret_injection() {
    // Fresh state with a seeded credential store.
    let t = tenant();
    let mut state = common::test_state(t);
    Arc::get_mut(&mut state)
        .unwrap()
        .attach_dependencies(
            Some(Arc::new(
                credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![(
                    "gwkey".to_string(),
                    "s3cr3t".to_string(),
                )]),
            )),
            None,
        );
    let app = router(state, sec(t));

    // Custom guard plugin (UUID instance of the guard base type).
    let plugin_uuid = Uuid::new_v4();
    let plugin_full = format!("gts.cf.core.oagw.guard_plugin.v1~{plugin_uuid}");
    let (status, _v) = post_json(
        &app,
        "/oagw/v1/plugins",
        json!({
            "name": "req-exact",
            "plugin_type": plugin_full,
            "config": { "required_request_headers": "x-exact" },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED);

    let server = MockServer::start_async().await;
    let base = server.base_url();
    let (host, port) = base
        .strip_prefix("http://")
        .unwrap()
        .rsplit_once(':')
        .unwrap();
    let port: u16 = port.parse().unwrap();

    let upstream = json!({
        "enabled": true,
        "alias": "gw",
        "server": { "endpoints": [ { "scheme": "http", "host": host, "port": port } ] },
        "protocol": PROTOCOL_HTTP,
        "auth": {
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "sharing": "inherit",
            "config": { "header": "x-api-key", "secret_ref": "cred://gwkey" },
        },
        "headers": { "request": { "passthrough": "all" } },
        "plugins": { "sharing": "inherit", "items": [ plugin_full ] },
    });
    let (status, value) = post_json(&app, "/oagw/v1/upstreams", upstream).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "upstream: {value}");
    let uid = Uuid::parse_str(value["id"].as_str().unwrap()).unwrap();
    add_route(&app, uid, "/", &["GET"], json!({})).await;

    let m = server
        .mock_async(|when, then| {
            when.method(Method::GET)
                .path("/x")
                .header("x-exact", "yes")
                .header("x-api-key", "s3cr3t");
            then.status(200);
        })
        .await;

    // Custom guard satisfied + apikey credential injected from the store.
    let (status, _h, _v) = call(
        &app,
        "GET",
        "/oagw/v1/proxy/gw/x",
        &[("x-exact", "yes")],
        Body::empty(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(m.calls_async().await, 1, "guard header + api key forwarded");
}
