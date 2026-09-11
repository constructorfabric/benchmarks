//! Integration tests of the CORS preflight short-circuit on the proxy path
//! (`cpt-cf-oagw-flow-cors-preflight`,
//! `cpt-cf-oagw-algo-cors-preflight-response`).
//!
//! Every test drives the real `OagwGear` through the registered router, so the
//! position of the preflight short-circuit in the handler — before identity,
//! before the permission gate, before the body and before the upstream — is
//! exercised rather than assumed.
// @cpt-dod:cpt-cf-oagw-dod-cors-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-preflight:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use uuid::Uuid;

use oagw::test_support::{permissive_surface, route_for, seed_route, stub_upstream};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

fn proxy_config() -> Option<Value> {
    Some(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The upstream body of the fixture, with its endpoints pointed at the stub.
fn upstream_body(host: &str, port: u16, cors: Value) -> Value {
    json!({
        "alias": "api.vendor.com",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": host, "port": port, "scheme": "http" } ] },
        "cors": cors
    })
}

/// The surface with one upstream (carrying `cors`) and one route seeded over a
/// stub upstream, plus the stub handle.
async fn seeded(cors: Value) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let (status, created) = surface
        .create(tenant, Uuid::new_v4(), upstream_body(&host, port, cors))
        .await;
    assert_eq!(status, 201, "{created:?}");
    let record: Value = serde_json::from_slice(&created).expect("the created upstream");
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("the identifier");
    seed_route(&surface, route_for(tenant, id, "/v1", &[oagw::domain::dto::HttpMethod::Get]));
    (surface, stub, tenant)
}

#[tokio::test]
async fn a_preflight_is_answered_locally_and_never_forwarded() {
    let (surface, stub, tenant) =
        seeded(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "OPTIONS",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[
                ("origin", "https://app.dev"),
                ("access-control-request-method", "DELETE"),
                ("access-control-request-headers", "x-request-id, content-type"),
            ],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NO_CONTENT, "{:?}", exchange.text());
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(exchange.header("access-control-allow-methods"), Some("DELETE"));
    assert_eq!(
        exchange.header("access-control-allow-headers"),
        Some("x-request-id, content-type")
    );
    assert_eq!(exchange.header("access-control-max-age"), Some("86400"));
    assert_eq!(exchange.header("vary"), Some(PREFLIGHT_VARY));
    assert!(exchange.header("access-control-expose-headers").is_none());
    assert!(exchange.header("access-control-allow-credentials").is_none());
    assert!(exchange.body.is_empty(), "a preflight carries an empty body");
    assert!(stub.received().is_empty(), "a preflight is never forwarded");
}

#[tokio::test]
async fn a_preflight_is_answered_without_a_security_context() {
    let (surface, stub, _tenant) =
        seeded(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    let exchange = surface
        .proxy(
            "OPTIONS",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://app.dev"), ("access-control-request-method", "POST")],
            b"",
            None,
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert!(exchange.header("access-control-allow-credentials").is_none());
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_preflight_under_a_credentialed_policy_names_an_exact_origin_only() {
    let (surface, _stub, tenant) = seeded(json!({
        "enabled": true,
        "allowed_origins": ["https://app.dev", "https://other.dev"],
        "allow_credentials": true
    }))
    .await;
    for (origin, credentialed) in [("https://app.dev", true), ("https://stranger.dev", false)] {
        let exchange = surface
            .proxy_for(
                tenant,
                Uuid::new_v4(),
                "OPTIONS",
                "/oagw/v1/proxy/api.vendor.com/v1",
                &[("origin", origin), ("access-control-request-method", "POST")],
                b"",
            )
            .await;
        assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
        assert_eq!(
            exchange.header("access-control-allow-credentials"),
            credentialed.then_some("true"),
            "{origin}"
        );
        assert_eq!(exchange.header("access-control-allow-origin"), Some(origin));
    }
}

#[tokio::test]
async fn a_preflight_reflects_no_unknown_method() {
    let (surface, stub, tenant) = seeded(json!({"enabled": true, "allowed_origins": ["*"]})).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "OPTIONS",
            "/oagw/v1/proxy/api.vendor.com/v1",
            &[
                ("origin", "https://app.dev"),
                ("access-control-request-method", "TRACE"),
                ("access-control-request-headers", "x-a, x-b"),
            ],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert!(exchange.header("access-control-allow-methods").is_none());
    assert_eq!(exchange.header("access-control-allow-headers"), Some("x-a, x-b"));
    assert_eq!(exchange.header("vary"), Some(PREFLIGHT_VARY));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn an_options_without_the_preflight_signature_falls_through() {
    let (surface, stub, tenant) =
        seeded(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    for headers in [
        vec![("origin", "https://app.dev")],
        vec![("access-control-request-method", "OPTIONS")],
        vec![],
    ] {
        let exchange = surface
            .proxy_for(tenant, Uuid::new_v4(), "OPTIONS", "/oagw/v1/proxy/api.vendor.com/v1", &headers, b"")
            .await;
        assert_eq!(exchange.status, http::StatusCode::METHOD_NOT_ALLOWED, "{headers:?}");
        assert_eq!(exchange.header("allow"), Some("GET"));
        assert!(exchange
            .headers
            .iter()
            .all(|(name, _)| !name.starts_with("access-control")),
            "no permissive CORS header is added");
    }
    assert!(stub.received().is_empty(), "the upstream never sees an OPTIONS");
}

#[tokio::test]
async fn an_options_on_an_unmatched_path_returns_not_found() {
    let (surface, _stub, tenant) =
        seeded(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "OPTIONS",
            "/oagw/v1/proxy/api.vendor.com/other",
            &[("origin", "https://app.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert!(exchange.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

#[tokio::test]
async fn a_preflight_on_an_unknown_alias_stays_permissive() {
    let (surface, _stub, tenant) =
        seeded(json!({"enabled": true, "allowed_origins": ["https://app.dev"]})).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "OPTIONS",
            "/oagw/v1/proxy/unknown.vendor.com/v1",
            &[("origin", "https://app.dev"), ("access-control-request-method", "POST")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(exchange.header("access-control-max-age"), Some("86400"));
}

/// A preflight answer is credential-free unless the policy in force is both
/// credentialed and exactly matched: a wildcard match and a disabled `cors`
/// block both stay credential-free.
#[tokio::test]
async fn a_preflight_stays_credential_free_without_an_exact_credentialed_policy() {
    for cors in [
        json!({"enabled": true, "allowed_origins": ["*"]}),
        json!({
            "enabled": false,
            "allow_credentials": true,
            "allowed_origins": ["https://app.dev"]
        }),
    ] {
        let (surface, _stub, tenant) = seeded(cors.clone()).await;
        let exchange = surface
            .proxy_for(
                tenant,
                Uuid::new_v4(),
                "OPTIONS",
                "/oagw/v1/proxy/api.vendor.com/v1",
                &[("origin", "https://app.dev"), ("access-control-request-method", "POST")],
                b"",
            )
            .await;
        assert_eq!(exchange.status, http::StatusCode::NO_CONTENT);
        assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
        assert!(
            exchange.header("access-control-allow-credentials").is_none(),
            "{cors}: no credential-bearing preflight without an exact credentialed policy"
        );
    }
}

/// The stored origins of an accepted `cors` block are read back in the
/// canonical form, so `:443` and the port-less entry are the same origin.
#[tokio::test]
async fn an_accepted_cors_block_is_stored_in_the_canonical_form() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let (status, created) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            upstream_body(
                &host,
                port,
                json!({"enabled": true, "allowed_origins": ["https://APP.Vendor.com:443"]}),
            ),
        )
        .await;
    assert_eq!(status, 201, "{created:?}");
    let record: Value = serde_json::from_slice(&created).expect("the created upstream");
    assert_eq!(
        record["cors"]["allowed_origins"],
        json!(["https://app.vendor.com"]),
        "the canonical form, with the default port omitted and the host lowercased"
    );
}
