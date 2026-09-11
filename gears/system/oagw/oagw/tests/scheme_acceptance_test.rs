//! Scheme acceptance tests.
//!
//! The management API accepts every scheme the model declares, `http`
//! included. Whether a plaintext connection is actually made is a separate
//! question, decided at dispatch time by the connection-permission flag on the
//! configuration; these tests pin that the two concerns stay separate.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::Harness;
use httpmock::prelude::MockServer;
use serde_json::json;

/// The schemes the management API must accept at create time.
const ACCEPTED: &[&str] = &["http", "https", "wss", "grpc"];

#[tokio::test]
async fn the_management_api_accepts_every_declared_scheme() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    for scheme in ACCEPTED {
        let alias = format!("pool-{scheme}");
        let (status, body) = harness
            .json(
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({
                    "alias": alias,
                    "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                    "server": {"endpoints": [
                        {"scheme": scheme, "host": "10.0.4.9", "port": 9000}
                    ]}
                })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "scheme '{scheme}' must be accepted: {body}"
        );
        assert_eq!(body["server"]["endpoints"][0]["scheme"], json!(scheme));
    }
}

#[tokio::test]
async fn an_omitted_scheme_defaults_to_https() {
    let server = MockServer::start();
    let harness = Harness::new(&server);

    let (status, body) = harness
        .json(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "defaults.tls",
                "protocol": oagw::gts_helpers::PROTOCOL_HTTP,
                "server": {"endpoints": [{"host": "10.0.4.9", "port": 9000}]}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["server"]["endpoints"][0]["scheme"], json!("https"));
}

#[tokio::test]
async fn the_connection_permission_is_a_config_decision_not_a_model_one() {
    let server = MockServer::start();
    let harness = Harness::new(&server);
    let upstream = harness.seed_upstream("plain.example", &server).await;
    let alias = upstream["alias"].as_str().unwrap().to_owned();

    // The pool was accepted with an `http` scheme.
    assert_eq!(upstream["server"]["endpoints"][0]["scheme"], json!("http"));

    // Whether the data plane may dial it is reported by the configuration,
    // independently of what the model stored.
    let config = oagw::config::OagwConfig {
        allow_http_upstream: true,
        ..oagw::config::OagwConfig::default()
    };
    assert!(config.permits_plaintext_connection());

    let restricted = oagw::config::OagwConfig::default();
    assert!(!restricted.permits_plaintext_connection());

    // The alias resolves either way: acceptance and permission are distinct.
    let resolved = harness
        .plane
        .resolve_upstream(uuid::Uuid::nil(), &alias)
        .await;
    assert!(
        resolved.is_ok(),
        "an accepted pool is resolvable: {resolved:?}"
    );
}
