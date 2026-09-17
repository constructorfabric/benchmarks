//! Configuration gating tests: the switches in [`crate::config::OagwConfig`]
//! and the SSRF policy that is applied before an upstream is dialed.
//!
//! The behavioural consequences of two switches (`max_request_body_bytes` and
//! the upstream deadline) are covered by the relay tests in
//! [`crate::tests::data_plane`]; this module covers the remaining gates and the
//! serde defaults of the configuration itself.

use std::sync::Arc;

use axum::http::Method;
use credstore_sdk::test_util::MockCredStoreClient;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ERROR_TYPE_BASE, Gateway, Origin, Reply, assert_problem, post_json, proxy_request, route_json,
    send, status_of, upstream_json,
};
use crate::config::{DEFAULT_PROXY_TIMEOUT_SECS, MAX_REQUEST_BODY_BYTES, OagwConfig, SsrfPolicy};
use crate::domain::error::ErrorKind;

/// The tenant the configuration tests register their upstreams in.
fn tenant() -> Uuid {
    Uuid::from_u128(0xcafe)
}

/// A gateway plus a JSON-echo origin registered under `alias` with a `path`
/// route for `methods`, built with `config`.
async fn relay_gateway(
    config: OagwConfig,
    alias: &str,
    path: &str,
    methods: &[&str],
) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = super::gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), alias),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let route = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), path, methods),
    )
    .await;
    assert_eq!(status_of(&route).as_u16(), 201, "route created");
    (gateway, origin)
}

/// The id of the single upstream the test registered.
fn first_upstream_id(gateway: &Gateway) -> Uuid {
    let upstreams = gateway.store.upstreams_of(tenant());
    assert_eq!(upstreams.len(), 1, "exactly one upstream is registered");
    upstreams[0].id
}

/// Send an anonymous proxied request.
async fn proxy(
    gateway: &Gateway,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> axum::response::Response {
    send(
        &gateway.router,
        proxy_request(method, uri, headers, body, None),
    )
    .await
}

/// Configuration that permits the plaintext loopback endpoints tests use.
fn plaintext() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[test]
fn the_documented_defaults_are_stable() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(30));
    assert!(
        !config.allow_http_upstream,
        "plaintext upstreams are opt-in"
    );
    assert_eq!(config.max_request_body_bytes, MAX_REQUEST_BODY_BYTES);
    assert!(config.rate_limit_response_headers);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert_eq!(
        config.ssrf_policy,
        SsrfPolicy {
            enabled: false,
            deny_private_addresses: true,
            allowed_hosts: Vec::new(),
        }
    );
}

#[test]
fn a_missing_config_section_deserializes_to_the_defaults() {
    let config: OagwConfig = serde_json::from_str("{}").expect("an empty section is valid");
    assert_eq!(config, OagwConfig::default());

    let partial: OagwConfig = serde_json::from_str(
        r#"{ "allow_http_upstream": true, "ssrf_policy": { "enabled": true } }"#,
    )
    .expect("a partial section is valid");
    assert!(partial.allow_http_upstream);
    assert!(partial.ssrf_policy.enabled);
    assert!(
        partial.ssrf_policy.deny_private_addresses,
        "the default holds"
    );
    assert!(partial.ssrf_policy.allowed_hosts.is_empty());
}

#[test]
fn deadlines_never_collapse_to_zero() {
    let config = OagwConfig {
        proxy_timeout_secs: 0,
        ..OagwConfig::default()
    };
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(1));
    let config = OagwConfig {
        token_cache_ttl_secs: 0,
        ..OagwConfig::default()
    };
    assert_eq!(config.token_cache_ttl(), std::time::Duration::from_secs(1));
}

#[test]
fn the_body_ceiling_is_the_hard_limit() {
    assert_eq!(MAX_REQUEST_BODY_BYTES, 100 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// Plaintext transport gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plaintext_upstreams_are_refused_until_enabled() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = super::gateway_with(OagwConfig::default());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "plain.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/plain.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    let body: Value = super::body_json(&mut response).await;
    assert_eq!(
        body["type"],
        json!(format!(
            "{ERROR_TYPE_BASE}{}",
            ErrorKind::LinkUnavailable.gts_fragment()
        ))
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("allow_http_upstream"),
        "the refusal names the switch, got: {body}"
    );
    assert_eq!(origin.captured().len(), 0, "no bytes reach the upstream");
}

#[tokio::test]
async fn enabling_the_switch_relays_the_request() {
    let (gateway, origin) = relay_gateway(plaintext(), "open.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/open.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.only().path, "/v1");
}

#[tokio::test]
async fn the_transport_gate_applies_to_every_cleartext_scheme() {
    let engine =
        crate::infra::proxy::ProxyEngine::new(OagwConfig::default()).expect("engine builds");
    // `grpc` dials `http`, so it is gated with `http` and `ws`.
    for scheme in ["http", "ws", "grpc"] {
        let endpoint = crate::domain::model::Endpoint {
            scheme: serde_json::from_value(json!(scheme)).expect("a known scheme"),
            host: "127.0.0.1".to_owned(),
            port: Some(8080),
        };
        let error = engine
            .check_endpoint(&endpoint)
            .await
            .expect_err("plaintext is gated");
        assert_eq!(error.kind(), ErrorKind::LinkUnavailable, "{scheme}");
        assert!(
            error.detail().contains("allow_http_upstream"),
            "{scheme} is refused by the transport gate: {error:?}"
        );
    }
    // The secure spellings are not gated: they never dial plaintext.
    for scheme in ["https", "wss", "wt", "grpcs"] {
        let endpoint = crate::domain::model::Endpoint {
            scheme: serde_json::from_value(json!(scheme)).expect("a known scheme"),
            host: "127.0.0.1".to_owned(),
            port: Some(443),
        };
        assert!(
            engine.check_endpoint(&endpoint).await.is_ok(),
            "{scheme} is not a plaintext scheme"
        );
    }
}

#[tokio::test]
async fn a_grpc_upstream_is_refused_while_the_transport_gate_is_closed() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = super::gateway_with(OagwConfig::default());
    let mut upstream = upstream_json(origin.host(), origin.port(), "grpc.local");
    upstream["server"]["endpoints"][0]["scheme"] = json!("grpc");
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/grpc.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    assert_eq!(
        origin.captured().len(),
        0,
        "a grpc upstream is not dialed either"
    );
}

#[test]
fn grpc_defaults_to_the_documented_standard_port() {
    use crate::domain::model::{Endpoint, EndpointScheme};
    assert_eq!(EndpointScheme::Grpc.default_port(), 443);
    assert_eq!(EndpointScheme::Grpcs.default_port(), 443);
    assert_eq!(EndpointScheme::Http.default_port(), 80);
    let endpoint = Endpoint {
        scheme: EndpointScheme::Grpc,
        host: "grpc.example".to_owned(),
        port: None,
    };
    assert_eq!(endpoint.effective_port(), 443);
    assert!(endpoint.uses_standard_port());
    assert_eq!(endpoint.authority(), "grpc.example");
}

// ---------------------------------------------------------------------------
// SSRF policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_private_upstream_is_denied_when_the_policy_is_enabled() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut config = plaintext();
    config.ssrf_policy.enabled = true;
    config.ssrf_policy.deny_private_addresses = true;
    let gateway = super::gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "guarded.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/guarded.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    let body: Value = super::body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("private"),
        "the refusal explains the policy, got: {body}"
    );
    assert_eq!(origin.captured().len(), 0, "the upstream is never dialed");
}

#[tokio::test]
async fn an_allowlisted_private_upstream_may_be_dialed() {
    let mut config = plaintext();
    config.ssrf_policy.enabled = true;
    config.ssrf_policy.deny_private_addresses = true;
    config.ssrf_policy.allowed_hosts = vec!["127.0.0.1".to_owned()];
    let (gateway, origin) = relay_gateway(config, "allowlisted.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/allowlisted.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.only().path, "/v1");
}

#[tokio::test]
async fn a_public_upstream_is_not_blocked_by_the_private_denial() {
    // `example.invalid` does not resolve, so the relay fails to connect; the
    // point of the test is that the policy does not reject it up front, which
    // is observable through the failure mode: the transport failure is
    // reported as an unavailable link (`ADR 0007`) whose detail is the
    // transport error, not the policy refusal.
    let mut config = plaintext();
    config.ssrf_policy.enabled = true;
    config.ssrf_policy.deny_private_addresses = true;
    let gateway = super::gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        // `.invalid` never resolves, so the relay is bounded by loopback.
        upstream_json("gateway.oagw.invalid", 80, "gateway.oagw.invalid"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/gateway.oagw.invalid/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    let body: Value = super::body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Transport"),
        "the failure is the transport error, not the policy refusal, got: {body}"
    );
}

#[tokio::test]
async fn the_policy_is_off_by_default_so_loopback_is_dialable() {
    let (gateway, origin) =
        relay_gateway(plaintext(), "default-policy.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/default-policy.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.only().path, "/v1");
}

#[tokio::test]
async fn the_policy_checks_are_host_scoped() {
    let config = OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: true,
            deny_private_addresses: true,
            allowed_hosts: vec!["Allowed.Example".to_owned()],
        },
        ..OagwConfig::default()
    };
    let engine = crate::infra::proxy::ProxyEngine::new(config).expect("engine builds");

    let allowlisted = crate::domain::model::Endpoint {
        scheme: crate::domain::model::EndpointScheme::Http,
        host: "allowed.example".to_owned(),
        port: Some(80),
    };
    assert!(
        engine.check_endpoint(&allowlisted).await.is_ok(),
        "the allowlist is normalized"
    );

    let denied = crate::domain::model::Endpoint {
        scheme: crate::domain::model::EndpointScheme::Http,
        host: "10.0.0.5".to_owned(),
        port: Some(80),
    };
    let error = engine
        .check_endpoint(&denied)
        .await
        .expect_err("a private host is denied");
    assert_eq!(error.kind(), ErrorKind::LinkUnavailable);
    assert!(
        error.detail().contains("private address space"),
        "the refusal explains the policy: {error:?}"
    );
}

#[tokio::test]
async fn a_disabled_policy_never_rejects() {
    let config = OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            deny_private_addresses: true,
            allowed_hosts: Vec::new(),
        },
        ..OagwConfig::default()
    };
    let engine = crate::infra::proxy::ProxyEngine::new(config).expect("engine builds");
    for host in [
        "127.0.0.1",
        "10.0.0.5",
        "169.254.1.1",
        "example.com",
        "localhost",
    ] {
        let endpoint = crate::domain::model::Endpoint {
            scheme: crate::domain::model::EndpointScheme::Http,
            host: host.to_owned(),
            port: Some(80),
        };
        assert!(
            engine.check_endpoint(&endpoint).await.is_ok(),
            "{host} is not inspected"
        );
    }
}

#[tokio::test]
async fn the_policy_judges_a_hostname_on_every_address_it_resolves_to() {
    // `localhost.localdomain` is a legal RFC 1123 name that no lexical check can
    // judge: only its resolution (`::1` here) reveals the loopback address it
    // stands for, so the policy has to ask the resolver.
    let origin = Origin::spawn(Reply::Echo).await;
    let mut config = plaintext();
    config.ssrf_policy.enabled = true;
    config.ssrf_policy.deny_private_addresses = true;
    let gateway = super::gateway_with(config);
    // Port 80 is the standard port for `http`, so the derived alias is the bare
    // host name and the proxy path stays readable.
    let upstream = upstream_json("localhost.localdomain", 80, "localhost.localdomain");
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/localhost.localdomain/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    let body: Value = super::body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("private"),
        "the loopback name is refused by the policy, got: {body}"
    );
    assert_eq!(origin.captured().len(), 0, "the upstream is never dialed");
}

#[tokio::test]
async fn the_resolver_reports_every_address_a_name_holds() {
    for name in ["localhost", "localhost.localdomain"] {
        let addresses = crate::infra::proxy::resolve_addresses(name, 80)
            .await
            .unwrap_or_else(|_| panic!("{name} resolves in this environment"));
        assert!(!addresses.is_empty(), "{name} resolves to something");
        assert!(
            addresses
                .iter()
                .all(|address| crate::domain::alias::is_private_address(*address)),
            "every address of `{name}` is private: {addresses:?}"
        );
    }
    // An IP literal is judged without the resolver.
    let literal = crate::infra::proxy::resolve_addresses("10.9.8.7", 80)
        .await
        .expect("a literal needs no resolver");
    assert_eq!(
        literal,
        vec!["10.9.8.7".parse::<std::net::IpAddr>().expect("an ip")]
    );
}

// ---------------------------------------------------------------------------
// Token cache configuration
// ---------------------------------------------------------------------------

#[test]
fn the_token_cache_is_sized_from_the_configuration() {
    let config = OagwConfig {
        token_cache_ttl_secs: 7,
        token_cache_capacity: 3,
        ..OagwConfig::default()
    };
    // The same two numbers reach the OAuth2 plugin cache through
    // `TokenCacheConfig`; the defaults keep the cache usable when the section
    // is absent.
    let cache = crate::domain::plugin::TokenCacheConfig {
        ttl: config.token_cache_ttl(),
        capacity: config.token_cache_capacity,
    };
    assert_eq!(cache.ttl, std::time::Duration::from_secs(7));
    assert_eq!(cache.capacity, 3);
}

#[tokio::test]
async fn the_gateway_is_assembled_from_the_configuration() {
    let mut config = plaintext();
    config.token_cache_capacity = 1;
    let gateway = super::gateway_custom(
        config,
        Arc::new(MockCredStoreClient::empty()),
        crate::domain::plugin::GuardPluginRegistry::with_builtins(),
        crate::domain::plugin::TransformPluginRegistry::with_builtins(),
    );
    assert_eq!(
        gateway.data.config().max_request_body_bytes,
        MAX_REQUEST_BODY_BYTES
    );
    assert_eq!(
        gateway.store.upstreams_of(tenant()).len(),
        0,
        "a gateway assembled from the configuration starts empty"
    );
}
