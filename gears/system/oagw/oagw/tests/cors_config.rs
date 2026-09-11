//! Integration tests of the CORS configuration-validation slice on the
//! management write path (`cpt-cf-oagw-flow-cors-config-validation`,
//! `cpt-cf-oagw-algo-cors-config-validate`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use uuid::Uuid;

use oagw::test_support::{permissive_surface, stub_upstream};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const VALIDATION_ERROR: &str = "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";

fn proxy_config() -> Option<Value> {
    Some(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

fn upstream_body(cors: Value) -> Value {
    json!({
        "alias": "api.vendor.com",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": "127.0.0.1", "port": 8080, "scheme": "http" } ] },
        "cors": cors
    })
}

async fn create(cors: Value) -> (http::StatusCode, Value) {
    let surface = permissive_surface(proxy_config()).await;
    let (status, body) = surface.create(Uuid::new_v4(), Uuid::new_v4(), upstream_body(cors)).await;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn a_cors_block_without_enabled_is_rejected() {
    let (status, body) = create(json!({"allowed_origins": ["https://app.dev"]})).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], VALIDATION_ERROR, "{body}");
}

#[tokio::test]
async fn credentials_with_a_wildcard_origin_are_rejected_at_validation_time() {
    let (status, body) = create(json!({
        "enabled": true,
        "allow_credentials": true,
        "allowed_origins": ["*"]
    }))
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], VALIDATION_ERROR, "{body}");
    assert!(
        body["detail"].as_str().expect("detail").contains("wildcard"),
        "the rejection names the conflict: {body}"
    );
}

#[tokio::test]
async fn a_method_outside_the_documented_set_is_rejected() {
    let (status, body) =
        create(json!({"enabled": true, "allowed_methods": ["TRACE"]})).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert!(body["detail"].as_str().expect("detail").contains("cors.allowed_methods"), "{body}");
}

#[tokio::test]
async fn an_origin_that_is_neither_the_wildcard_nor_a_uri_is_rejected() {
    for origin in ["app.dev", "https://app.dev/path", "https://*.app.dev", "*.app.dev"] {
        let (status, body) =
            create(json!({"enabled": true, "allowed_origins": [origin]})).await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST, "{origin}: {body}");
        assert!(body["detail"].as_str().expect("detail").contains("cors.allowed_origins"), "{body}");
    }
}

#[tokio::test]
async fn an_accepted_block_carries_the_documented_defaults() {
    let (status, body) = create(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    assert_eq!(status, http::StatusCode::CREATED, "{body}");
    assert_eq!(body["cors"]["sharing"], json!("private"));
    assert_eq!(body["cors"]["allowed_methods"], json!(["GET", "POST"]));
    assert_eq!(body["cors"]["expose_headers"], json!([]));
    assert_eq!(body["cors"]["allow_credentials"], json!(false));
    assert_eq!(body["cors"]["enabled"], json!(true), "`enabled` is never defaulted");
}

#[tokio::test]
async fn a_sharing_value_outside_the_documented_modes_is_rejected() {
    let (status, body) = create(json!({"enabled": true, "sharing": "public"})).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["type"], VALIDATION_ERROR, "{body}");
}

/// A rejected configuration is stored nowhere: the list stays empty.
#[tokio::test]
async fn a_rejected_cors_block_is_stored_nowhere() {
    let surface = permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let (status, body) = surface
        .create(tenant, Uuid::new_v4(), upstream_body(json!({"enabled": true, "allow_credentials": true, "allowed_origins": ["*"]})))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("the list body");
    assert_eq!(listed["count"], json!(0), "no record was stored: {listed}");
}

/// A `cors` block on a route is validated by the same rules.
#[tokio::test]
async fn a_route_cors_block_is_validated_by_the_same_rules() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let (status, created) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            json!({
                "alias": "api.vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [ { "host": host, "port": port, "scheme": "http" } ] }
            }),
        )
        .await;
    assert_eq!(status, 201, "{created:?}");
    let record: Value = serde_json::from_slice(&created).expect("the created upstream");
    let upstream_id: Uuid = serde_json::from_value(record["id"].clone()).expect("the identifier");

    let (status, body) = surface
        .create_route(
            tenant,
            Uuid::new_v4(),
            json!({
                "upstream_id": upstream_id,
                "match": { "http": { "path": "/v1", "methods": ["GET"] } },
                "cors": { "enabled": true, "allow_credentials": true, "allowed_origins": ["*"] }
            }),
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{body:?}");
    let body: Value = serde_json::from_slice(&body).expect("the rejection body");
    assert_eq!(body["type"], VALIDATION_ERROR, "{body}");
}
