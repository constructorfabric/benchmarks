//! Data-plane integration tests: the pingora bridge + [`DataPlaneGate`]
//! pipeline driven end to end through the internal HTTP/1.1 relay
//! ([`relay_request`]) and an httpmock upstream.
//!
//! Covers feature-data-plane transport stages:
//!
//! - proxied request relay (status/headers/body round trip, upstream-sourced
//!   `X-OAGW-Error-Source: upstream` on success);
//! - token-bucket rate limiting (`429` + `Retry-After`/`X-RateLimit-*`, gateway
//!   error source);
//! - unknown alias → gateway-sourced `404`;
//! - data plane not ready (unbound bridge port) → gateway-sourced `503`
//!   authored by the axum proxy surface.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::pep::PolicyEnforcer;
use httpmock::prelude::{GET, MockServer};
use oagw::domain::credentials::CredentialResolver;
use oagw::domain::models::{
    CorsConfig, Plugin, PluginKind, RateLimitConfig, Route, Upstream, UpstreamScheme,
};
use oagw::domain::rate_limit::RateLimiterRegistry;
use oagw::domain::repository::ControlPlaneService;
use oagw::infra::proxy::relay::{RelayIdentity, RelayRequest, relay_request};
use oagw::infra::proxy::{DataPlaneGate, DataPlaneService, ProxyBridgeHandle, spawn_proxy_bridge};
use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};

use common::{SYSTEM, json_status, router, run, security};

/// Seeds an upstream pointing at the httpmock server plus an enabled route.
fn world(mock_port: u16, rate_limit: Option<RateLimitConfig>) -> Arc<ControlPlaneService> {
    let control = Arc::new(ControlPlaneService::new(0, Duration::from_secs(5), true));
    control
        .upsert_upstream(Upstream {
            alias: "echo".to_owned(),
            name: "echo upstream".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: mock_port,
            scheme: UpstreamScheme::Http,
            path_prefix: String::new(),
            enabled: true,
            timeout_secs: 0,
        })
        .expect("upstream seeded");
    control
        .upsert_route(Route {
            alias: "echo".to_owned(),
            upstream_alias: Some("echo".to_owned()),
            methods: None,
            http_matches: vec![],
            rate_limit,
            cors: CorsConfig::default(),
            enabled: true,
            priority: 5,
        })
        .expect("route seeded");
    control
}

/// Spawns the pingora data-plane bridge for `control` (allow-http on, SSRF
/// off, rate limiting on) and returns its relay port.
fn spawn(control: Arc<ControlPlaneService>) -> (u16, ProxyBridgeHandle) {
    spawn_with_enforcer(control, common::enforcer())
}

/// Spawns the data-plane bridge with an explicit PEP enforcer so authz-gate
/// outcomes (allow vs deny) can be exercised end to end.
fn spawn_with_enforcer(
    control: Arc<ControlPlaneService>,
    enforcer: Arc<PolicyEnforcer>,
) -> (u16, ProxyBridgeHandle) {
    let resolver = Arc::new(CredentialResolver::new(
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        10_000,
        Duration::from_secs(300),
    ));
    let gate = Arc::new(DataPlaneGate::new(
        control,
        enforcer,
        resolver,
        Arc::new(RateLimiterRegistry::new(true)),
        true,
        false,
        Duration::from_secs(5),
    ));
    let service = DataPlaneService::new(gate);
    let bridge = spawn_proxy_bridge(service).expect("bridge spawned");
    let port = bridge.port;
    wait_ready(port);
    (port, bridge)
}

/// Waits until the bridge listener accepts TCP connections (the pingora
/// thread binds asynchronously after the handle is returned).
fn wait_ready(port: u16) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let reachable = tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        });
        if reachable {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "data-plane bridge never became reachable on port {port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Relays one GET through the bridge, returning status, headers and body.
/// `secret` is the per-bridge relay secret the gate requires.
type RelayedResponse = (axum::http::StatusCode, Vec<(String, String)>, Vec<u8>);
fn relay(port: u16, secret: &str, uri: &str, alias: &str) -> RelayedResponse {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let request = RelayRequest {
                method: "GET".to_owned(),
                uri: uri.to_owned(),
                headers: axum::http::HeaderMap::new(),
                identity: RelayIdentity {
                    alias: alias.to_owned(),
                    subject_id: DEFAULT_SUBJECT_ID,
                    tenant_id: DEFAULT_TENANT_ID,
                    bearer: None,
                    scopes: vec!["*".to_owned()],
                    relay_secret: secret.to_owned(),
                },
                body: axum::body::Body::empty(),
            };
            let resp = relay_request(port, request).await.expect("relay succeeds");
            let status = resp.status();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_owned(),
                        v.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("read body")
                .to_vec();
            (status, headers, body)
        })
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[test]
fn proxy_relay_roundtrips_through_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/oagw/v1/proxy/echo/hello");
        then.status(201)
            .header("X-Upstream", "yes")
            .body("echo-body");
    });

    let (port, bridge) = spawn(world(server.port(), None));
    let (status, headers, body) = relay(
        port,
        &bridge.relay_secret,
        "/oagw/v1/proxy/echo/hello?q=1",
        "echo",
    );

    assert_eq!(status, axum::http::StatusCode::CREATED);
    assert_eq!(body, b"echo-body");
    // Upstream-sourced response is attributed to the upstream (ADR 0007).
    assert_eq!(header(&headers, "X-OAGW-Error-Source"), Some("upstream"));
    // Upstream headers pass through.
    assert_eq!(header(&headers, "X-Upstream"), Some("yes"));
    // Internal routing headers never leak upstream (nor back to the caller).
    assert!(header(&headers, "x-oagw-backend-alias").is_none());
    assert!(header(&headers, "x-oagw-subject-id").is_none());

    // The upstream received the full proxied path (no alias stripping).
    mock.assert_calls(1);
}

#[test]
fn rate_limited_route_returns_429_with_retry_hints() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });

    // A tiny refill keeps the bucket drained across both calls (tokens refill
    // at 1000/s, far slower than the test executes) while satisfying the
    // control-plane `refill_per_sec > 0` validation.
    let limit = RateLimitConfig {
        capacity: 1,
        refill_per_sec: 0.001,
    };
    let (port, bridge) = spawn(world(server.port(), Some(limit)));

    // First call admits and forwards; second is rejected before upstream.
    let (status, _, _) = relay(port, &bridge.relay_secret, "/oagw/v1/proxy/echo/a", "echo");
    assert_eq!(status, axum::http::StatusCode::OK);

    // After exhaustion: 429 with the retry contract.
    let (status, headers, body) =
        relay(port, &bridge.relay_secret, "/oagw/v1/proxy/echo/b", "echo");
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&headers, "X-OAGW-Error-Source"), Some("gateway"));
    let retry_after = header(&headers, "Retry-After").unwrap_or_default();
    assert!(!retry_after.is_empty(), "429 must carry Retry-After");
    assert_eq!(header(&headers, "X-RateLimit-Remaining"), Some("0"));
    let reset = header(&headers, "X-RateLimit-Reset").unwrap_or_default();
    assert!(!reset.is_empty(), "429 must carry X-RateLimit-Reset");

    let (_, json) = json_status((status, body));
    assert_eq!(json["status"], 429);
    // No upstream traffic was generated for the rejected call.
    assert_eq!(mock.calls(), 1, "rejected request must not reach upstream");
}

#[test]
fn unknown_alias_returns_gateway_404() {
    let (port, bridge) = spawn(world(1, None));
    let (status, headers, body) =
        relay(port, &bridge.relay_secret, "/oagw/v1/proxy/nope/x", "nope");

    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(header(&headers, "X-OAGW-Error-Source"), Some("gateway"));
    let (_, json) = json_status((status, body));
    assert_eq!(json["status"], 404);
}

#[test]
fn data_plane_not_ready_returns_gateway_503() {
    // The management router's ProxyPort cell is unbound → the proxy surface
    // reports "data plane not ready" before any bridging is attempted.
    let app = router(world(1, None), security(SYSTEM.0, SYSTEM.1));
    let (status, body) = run(app, "GET", "/oagw/v1/proxy/echo/hello", None);

    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let (_, json) = json_status((status, body));
    assert_eq!(json["status"], 503);
    assert_eq!(json["title"], "Data plane not ready");
}

/// The data-plane PEP gate must deny a proxied request with a
/// gateway-sourced 403 problem (`inst-pep-deny` / `inst-pep-deny-return`).
#[test]
fn pep_denial_returns_gateway_403() {
    let mock = MockServer::start();
    mock.mock(|when, then| {
        when.method(GET);
        then.status(200).body("unreachable-under-deny");
    });
    let control = world(mock.port(), None);
    let (port, mut bridge) = spawn_with_enforcer(control, common::deny_enforcer());
    let (status, headers, body) = relay(port, &bridge.relay_secret, "/anything", "echo");
    bridge.stop();

    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        header(&headers, "X-OAGW-Error-Source"),
        Some("gateway"),
        "PEP denials are gateway-sourced"
    );
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(
        problem["type"].as_str().unwrap_or_default(),
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
    assert_eq!(problem["status"], 403);
}

/// A guard plugin (`required_headers`) must reject a request missing the
/// declared header with a gateway-sourced 400 problem
/// (`inst-guard-reject` / `inst-guard-block`).
#[test]
fn guard_plugin_rejects_missing_required_header() {
    let mock = MockServer::start();
    mock.mock(|when, then| {
        when.method(GET).header("x-token", "ok");
        then.status(200).body("guarded-ok");
    });
    let control = world(mock.port(), None);
    control
        .upsert_plugin(Plugin {
            alias: "require-token".to_owned(),
            kind: PluginKind::RequiredHeaders,
            enabled: true,
            config: serde_json::json!({ "headers": ["x-token"] }),
        })
        .expect("guard plugin seeded");
    control
        .bind_plugin_to_route("echo", "require-token")
        .expect("guard bound to route");

    let (port, mut bridge) = spawn(control);
    // Missing the required header → 400 gateway problem.
    let (status, headers, body) = relay(port, &bridge.relay_secret, "/anything", "echo");
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(header(&headers, "X-OAGW-Error-Source"), Some("gateway"));
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(problem["status"], 400);
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("x-token"),
        "guard detail must name the missing header"
    );
    bridge.stop();
}

/// The relay must stream an upstream body larger than any fixed staging
/// buffer byte-for-byte (exercises the proxy surface → relay → pingora
/// streaming path end to end).
#[test]
fn large_upstream_body_streams_byte_for_byte() {
    let payload = "x".repeat(512 * 1024);
    let mock = MockServer::start();
    mock.mock(|when, then| {
        when.method(GET);
        then.status(200)
            .header("x-stream", "big")
            .body(payload.clone());
    });
    let control = world(mock.port(), None);
    let (port, mut bridge) = spawn(control);
    let (status, headers, body) = relay(port, &bridge.relay_secret, "/big", "echo");
    bridge.stop();

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(header(&headers, "x-stream"), Some("big"));
    assert_eq!(body.len(), payload.len());
    assert_eq!(String::from_utf8_lossy(&body), payload);
}

/// The internal bridge is unauthenticated loopback transport, so a request
/// that does not present the per-bridge relay secret must be rejected before
/// any caller identity is trusted (RF-008: no header spoofing by a loopback
/// port probe).
#[test]
fn internal_request_without_relay_secret_is_rejected() {
    let server = MockServer::start();
    let m = server.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });
    let control = world(server.port(), None);
    let (port, mut bridge) = spawn(control);
    // Empty secret = no relay credential presented.
    let (status, headers, body) = relay(port, "", "/oagw/v1/proxy/echo/x", "echo");
    bridge.stop();

    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(header(&headers, "X-OAGW-Error-Source"), Some("gateway"));
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem body");
    assert_eq!(problem["status"], 401);
    assert_eq!(
        m.calls(),
        0,
        "unauthenticated internal request must not reach upstream"
    );
}

/// `ProxyBridgeHandle::stop` must actually terminate the pingora server:
/// the loopback listener has to be released (no ghost listener), which is
/// the RF-007 regression this test locks in.
#[test]
fn bridge_stop_releases_the_listener() {
    let mock = MockServer::start();
    mock.mock(|when, then| {
        when.method(GET);
        then.status(200).body("ok");
    });
    let control = world(mock.port(), None);
    let (port, mut bridge) = spawn(control);

    let reachable = || -> bool {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        })
    };
    assert!(reachable(), "bridge must be listening after spawn");

    bridge.stop();

    let mut released = false;
    for _ in 0..40 {
        if !reachable() {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        released,
        "bridge listener must be released after stop() (ghost listener)"
    );
}
