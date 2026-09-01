//! Unit tests for [`super::transport`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{ensure_allowed_scheme, request_target, websocket_target};
use crate::domain::models::{Endpoint, EndpointScheme, Protocol, ServerConfig, Upstream};

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint::new(scheme, host, port)
}

fn upstream(endpoints: &[Endpoint]) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        alias: "api.example.com".to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: endpoints.to_vec(),
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn https_targets_use_the_standard_scheme() {
    let target = request_target(&endpoint(EndpointScheme::Https, "api.example.com", 443), "/v1/x", None);
    assert_eq!(target, "https://api.example.com/v1/x");
}

#[test]
fn a_non_standard_port_is_preserved_in_the_target() {
    let target = request_target(&endpoint(EndpointScheme::Http, "127.0.0.1", 8099), "/v1", None);
    assert_eq!(target, "http://127.0.0.1:8099/v1");
}

#[test]
fn a_query_string_is_appended_when_present() {
    let target = request_target(
        &endpoint(EndpointScheme::Https, "api.example.com", 443),
        "/v1/x",
        Some("a=1&b=2"),
    );
    assert_eq!(target, "https://api.example.com/v1/x?a=1&b=2");
    let without = request_target(
        &endpoint(EndpointScheme::Https, "api.example.com", 443),
        "/v1/x",
        Some(""),
    );
    assert_eq!(without, "https://api.example.com/v1/x");
}

#[test]
fn websocket_targets_use_the_ws_scheme() {
    assert_eq!(
        websocket_target(&endpoint(EndpointScheme::Https, "api.example.com", 443), "/ws", None),
        "wss://api.example.com/ws"
    );
    assert_eq!(
        websocket_target(&endpoint(EndpointScheme::Http, "127.0.0.1", 9000), "/ws", None),
        "ws://127.0.0.1:9000/ws"
    );
}

#[test]
fn plaintext_egress_is_refused_by_default() {
    let pool = upstream(&[endpoint(EndpointScheme::Http, "api.example.com", 80)]);
    let rejected = ensure_allowed_scheme(&pool, &pool.server.endpoints[0], false).unwrap_err();
    assert_eq!(rejected.status(), 502);
    assert!(ensure_allowed_scheme(&pool, &pool.server.endpoints[0], true).is_ok());
}

#[test]
fn tls_egress_is_always_allowed() {
    let pool = upstream(&[endpoint(EndpointScheme::Https, "api.example.com", 443)]);
    assert!(ensure_allowed_scheme(&pool, &pool.server.endpoints[0], false).is_ok());
}
