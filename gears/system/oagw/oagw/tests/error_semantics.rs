//! Error semantics (DESIGN "Error Response Format", PRD
//! `cpt-cf-oagw-fr-error-model`): every gateway failure is an RFC 9457
//! problem document carrying the GTS `type` id, the HTTP status, and the
//! OAGW extension members the client is expected to act on.
//!
//! Each row of the DESIGN table is exercised through the wire, from a real
//! router, so what is asserted is the response the client sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::{APIKEY_PLUGIN, catch_all_route, create_upstream};

const P: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// `GET /oagw/v1/proxy/{alias}{path}` through the management API.
async fn setup(
    h: &common::Harness,
    host: &str,
    port: u16,
    alias: &str,
    mut extra: Value,
) -> String {
    let mut body = common::http_upstream(host, port, Some(alias));
    if let (Some(extra_object), Some(body_object)) = (extra.as_object_mut(), body.as_object_mut()) {
        for (key, value) in extra_object {
            body_object.insert(key.clone(), value.clone());
        }
    }
    let (status, body) = create_upstream(h, body).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let upstream_id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = common::create_route(h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    alias.to_owned()
}

/// Assert the RFC 9457 envelope of a gateway problem document.
fn assert_problem(problem: &Value, problem_type: &str, status: u16) {
    assert_eq!(
        problem["type"].as_str().unwrap_or_default(),
        format!("{P}{problem_type}"),
        "{problem}"
    );
    assert_eq!(problem["status"], json!(status), "{problem}");
    assert!(
        problem["title"].as_str().is_some_and(|t| !t.is_empty()),
        "a problem carries a title: {problem}"
    );
    assert!(
        problem["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "a problem carries a detail: {problem}"
    );
}

/// An upstream that accepts the connection, consumes the request head and then
/// abandons the exchange mid-response: it writes a truncated head and closes.
async fn aborting_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind aborting upstream");
    let port = listener.local_addr().expect("aborting local addr").port();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        // Consume the request head so the dial itself succeeds; the failure is
        // the *response* side of the exchange.
        let mut buffer = [0u8; 8192];
        let mut seen: Vec<u8> = Vec::new();
        loop {
            if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(read)) => seen.extend_from_slice(&buffer[..read]),
            }
        }
        // Half a head, then the connection is gone.
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-ty").await;
        let _ = socket.flush().await;
        drop(socket);
    });
    port
}

/// A silent upstream: accepts, reads the request, never answers.
async fn silent_upstream() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind silent upstream");
    let port = listener.local_addr().expect("silent local addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                while socket.read(&mut buffer).await.unwrap_or(0) > 0 {}
            });
        }
    });
    port
}

// ------------------------------------------------------------------ 400

#[tokio::test]
async fn a_route_validation_failure_is_a_400_with_violations() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("qs"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) =
        common::create_route(&h, common::route_for(&upstream_id, &["GET"], "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/qs/v1?temperature=0.7",
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(&problem, "validation.error.v1", 400);
    assert!(
        problem["violations"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "the offending parameter is named: {problem}"
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_multi_endpoint_common_suffix_alias_without_a_target_host_is_a_400() {
    // The endpoints are never dialled: the request fails before the transport.
    let (_host, port) = common::echo_server().await;
    let h = common::harness();
    // No explicit alias: the hostname endpoints derive `vendor.internal`,
    // which is the common suffix of both and therefore ambiguous.
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "server": {"endpoints": [
                {"scheme": "http", "host": "us.vendor.internal", "port": port},
                {"scheme": "http", "host": "eu.vendor.internal", "port": port}
            ]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let alias = upstream["alias"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_problem(&problem, "routing.missing_target_host.v1", 400);
    assert_eq!(
        problem["upstream_id"].as_str(),
        Some(upstream_id.as_str()),
        "the ambiguous upstream is named"
    );
    assert_eq!(problem["alias"], alias);

    // Naming one endpoint resolves the ambiguity: the request is no longer a
    // routing error (it proceeds to the upstream exchange, which fails because
    // the example hostnames are not resolvable in this environment).
    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1"),
            &[("x-oagw-target-host", "us.vendor.internal")],
            None,
        )
        .await;
    assert_ne!(
        problem["type"],
        problem_type("routing.missing_target_host.v1"),
        "the header resolved the addressing: {problem}"
    );
    assert_ne!(status, StatusCode::BAD_REQUEST, "{problem}");
}

/// The GTS problem-type id for an OAGW suffix.
fn problem_type(suffix: &str) -> String {
    format!("{P}{suffix}")
}

#[tokio::test]
async fn a_target_host_naming_the_configured_endpoint_is_proxied() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let alias = setup(&h, &host, port, "picked", json!({})).await;

    let (status, _, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1"),
            &[("x-oagw-target-host", &host)],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "naming the endpoint resolves it");
}

#[tokio::test]
async fn a_malformed_target_host_is_a_400() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("fmt"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/fmt/v1",
            &[("x-oagw-target-host", "not a hostname!")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_problem(&problem, "routing.invalid_target_host.v1", 400);
    assert_eq!(problem["invalid_value"], "not a hostname!");
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(
        problem["valid_hosts"],
        json!(["127.0.0.1"]),
        "the offending value is corrected with the configured host"
    );
}

#[tokio::test]
async fn an_unknown_target_host_is_a_400_with_the_valid_set() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("tgt"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/tgt/v1",
            &[("x-oagw-target-host", "nowhere.example.net")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_problem(&problem, "routing.unknown_target_host.v1", 400);
    assert_eq!(problem["invalid_value"], "nowhere.example.net");
    assert_eq!(problem["upstream_id"].as_str(), Some(upstream_id.as_str()));
    assert_eq!(
        problem["valid_hosts"],
        json!(["127.0.0.1"]),
        "the only configured host is the one the client should have named"
    );
}

/// A multi-endpoint upstream on `us.`/`eu.` hosts: the shared registrable
/// suffix derives a common-suffix alias, so the derived alias is ambiguous and
/// only `X-OAGW-Target-Host` can disambiguate it.
async fn multi_host_upstream(h: &common::Harness, port: u16) -> (String, String) {
    let (status, upstream) = create_upstream(
        h,
        json!({
            "enabled": true,
            "server": {"endpoints": [
                {"scheme": "http", "host": "us.hosts.example.net", "port": port},
                {"scheme": "http", "host": "eu.hosts.example.net", "port": port}
            ]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let id = upstream["id"].as_str().unwrap().to_owned();
    let alias = upstream["alias"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(h, catch_all_route(&id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);
    (alias, id)
}

#[tokio::test]
async fn an_unknown_target_host_lists_every_configured_host() {
    // ADR 0007 "Unknown Target Host": the problem carries `valid_hosts`, so the
    // client can pick a correct value without a second round trip.
    let (_host, port) = common::echo_server().await;
    let h = common::harness();
    let (alias, upstream_id) = multi_host_upstream(&h, port).await;

    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1"),
            &[("x-oagw-target-host", "apac.hosts.example.net")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_problem(&problem, "routing.unknown_target_host.v1", 400);
    assert_eq!(problem["invalid_value"], "apac.hosts.example.net");
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(
        problem["valid_hosts"],
        json!(["us.hosts.example.net", "eu.hosts.example.net"]),
        "the members are the upstream's endpoint hosts, in declaration order"
    );
}

#[tokio::test]
async fn a_malformed_target_host_also_lists_every_configured_host() {
    let (_host, port) = common::echo_server().await;
    let h = common::harness();
    let (alias, upstream_id) = multi_host_upstream(&h, port).await;

    let (status, problem, _) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1"),
            &[("x-oagw-target-host", "eu.hosts.example.net:443")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_problem(&problem, "routing.invalid_target_host.v1", 400);
    assert_eq!(problem["invalid_value"], "eu.hosts.example.net:443");
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(
        problem["valid_hosts"],
        json!(["us.hosts.example.net", "eu.hosts.example.net"]),
        "an invalid *format* is corrected with the same list"
    );
}

// ------------------------------------------------------------------ 401

/// A guard that rejects every request with 401, so the DESIGN `auth.failed.v1`
/// row is reachable over the wire.
#[derive(Debug, Clone, Copy, Default)]
struct UnauthorizedGuard;

#[async_trait]
impl oagw::domain::plugin::GuardPlugin for UnauthorizedGuard {
    fn id(&self) -> &str {
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test.unauthorized.v1"
    }

    fn plugin_type(&self) -> &str {
        self.id()
    }

    async fn guard_request(
        &self,
        _ctx: &oagw::domain::plugin::RequestContext,
    ) -> oagw::domain::plugin::PluginResult<oagw::domain::plugin::GuardDecision> {
        Ok(oagw::domain::plugin::GuardDecision::reject(
            401,
            "REQUIRED_HEADER_MISSING",
            "the upstream rejected the credentials".to_owned(),
        ))
    }

    async fn guard_response(
        &self,
        _ctx: &oagw::domain::plugin::ResponseContext,
    ) -> oagw::domain::plugin::PluginResult<oagw::domain::plugin::GuardDecision> {
        Ok(oagw::domain::plugin::GuardDecision::allow())
    }
}

#[tokio::test]
async fn a_rejected_authentication_is_a_401() {
    let (host, port) = common::echo_server().await;
    let h = common::harness_with_guard(Arc::new(UnauthorizedGuard));
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "authz",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "plugins": {"sharing": "private",
                        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test.unauthorized.v1"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/authz/v1", &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");
    assert_problem(&problem, "auth.failed.v1", 401);
}

// ------------------------------------------------------------------ 404

#[tokio::test]
async fn an_unresolvable_alias_is_a_404_owned_by_the_gateway() {
    let h = common::harness();
    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/no-such-alias", &[], None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_problem(&problem, "route.not_found.v1", 404);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_missing_route_is_a_404_with_the_proxied_path() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("only"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) =
        common::create_route(&h, common::route_for(&upstream_id, &["GET"], "/only/this")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/only/never", &[], None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_problem(&problem, "route.not_found.v1", 404);
}

// ------------------------------------------------------------------ 413 / 429

#[tokio::test]
async fn an_oversized_body_is_a_413_naming_the_limit() {
    let (host, port) = common::echo_server().await;
    let h = common::harness_with(common::harness_with_small_body());
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("big"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(
            Method::POST,
            "/oagw/v1/proxy/big/upload",
            &[("content-length", "999999")],
            Some(json!({"blob": "x".repeat(2048)})),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{problem}");
    assert_problem(&problem, "payload.too_large.v1", 413);
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|d| d.contains("1024")),
        "the configured limit is quoted: {problem}"
    );
}

#[tokio::test]
async fn an_exhausted_rate_limit_is_a_429_with_retry_guidance() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let mut limit = common::token_bucket(1, 1, "tenant");
    if let Some(object) = limit.as_object_mut() {
        object.insert("sustained".to_owned(), json!({"rate": 1, "window": "hour"}));
    }
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "capped",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "rate_limit": limit
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = h
        .json(Method::GET, "/oagw/v1/proxy/capped/ping", &[], None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/capped/ping", &[], None)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{problem}");
    assert_problem(&problem, "rate_limit.exceeded.v1", 429);
    let retry = problem["retry_after_seconds"]
        .as_u64()
        .expect("retry member");
    assert!(retry >= 1, "the retry guidance is positive: {problem}");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some(retry.to_string().as_str()),
        "the header and the member agree"
    );
    // ADR 0003 "More Information": the 429 advertises the budget the client
    // has exhausted, alongside `Retry-After`.
    for name in [
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
    ] {
        let raw = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("{name} missing on the 429"));
        assert!(
            raw.parse::<u64>().is_ok(),
            "{name} must be a non-negative integer, got {raw}"
        );
    }
    assert_eq!(
        headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("0"),
        "an exhausted budget advertises zero remaining"
    );
}

// ------------------------------------------------------------------ 500

#[tokio::test]
async fn an_unreadable_credential_is_a_500_and_never_proxies() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "keyless",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "auth": {"type": APIKEY_PLUGIN, "config": {"key_ref": "absent"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/keyless/v1", &[], None)
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{problem}");
    assert_problem(&problem, "secret.not_found.v1", 500);
}

// ------------------------------------------------------------------ 502

#[tokio::test]
async fn a_required_response_header_the_upstream_omits_is_a_502() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "typed",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "plugins": {"sharing": "private", "items": [{
                "plugin_ref": common::REQUIRED_HEADERS_PLUGIN,
                "config": {"required_response_headers": "x-must-be-present"}
            }]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/typed/v1", &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_problem(&problem, "protocol.error.v1", 502);
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "the upstream failed to produce a conforming response"
    );
}

/// A connection that dies before a usable response arrived is a `502
/// DownstreamError` (DESIGN error table): the upstream accepted the exchange
/// and then dropped it, so there is no upstream response to pass through and
/// ADR 0007 still attributes the problem document to the gateway.
#[tokio::test]
async fn an_upstream_that_drops_the_connection_mid_response_is_a_502() {
    let port = aborting_upstream().await;
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream("127.0.0.1", port, Some("cut"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/cut/v1/chat", &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_problem(&problem, "downstream.error.v1", 502);
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(problem["path"], json!("/v1/chat"));
    assert!(
        problem["host"]
            .as_str()
            .unwrap_or_default()
            .contains("127.0.0.1"),
        "the endpoint that dropped the exchange is named: {problem}"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_bound_plugin_no_implementation_knows_is_a_503() {
    let (host, port) = common::echo_server().await;
    let h = common::harness();
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "ghosted",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "plugins": {"sharing": "private",
                        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/ghosted/v1", &[], None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_problem(&problem, "plugin.not_found.v1", 503);
}

// ------------------------------------------------------------------ 503

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_owned_by_the_gateway() {
    // DESIGN error table: a dial that fails (refused / reset / unresolvable) is
    // a `502 DownstreamError` — "Upstream service error". ADR 0007 still names
    // the gateway as the source, because the upstream produced no response to
    // pass through.
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream("127.0.0.1", 1, Some("dark"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/dark/anything", &[], None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_problem(&problem, "downstream.error.v1", 502);
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(
        problem["host"].as_str().unwrap_or_default(),
        "127.0.0.1:1",
        "the endpoint that could not be reached is named"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_naming_the_upstream() {
    let h = common::harness();
    let mut body = common::http_upstream("127.0.0.1", 9, Some("paused"));
    body["enabled"] = json!(false);
    let (status, upstream) = create_upstream(&h, body).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/paused/anything", &[], None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_problem(&problem, "upstream.disabled.v1", 503);
    assert_eq!(problem["upstream_id"], json!(upstream_id));
}

#[tokio::test]
async fn a_tripped_circuit_breaker_is_a_503_with_retry_guidance() {
    // Only the port is used: the breaker must see a connection-refused dial.
    let (host, _port) = common::echo_server().await;
    let h = common::harness_with_breaker(2, Duration::from_secs(300));
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, 1, Some("fused"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    // Two failing dials trip the breaker; the third request never dials. A
    // failed dial is a 502 DownstreamError (DESIGN error table).
    for _ in 0..2 {
        let (status, problem, _) = h
            .json(Method::GET, "/oagw/v1/proxy/fused/anything", &[], None)
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
        assert_problem(&problem, "downstream.error.v1", 502);
    }
    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/fused/anything", &[], None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_problem(&problem, "circuit_breaker.open.v1", 503);
    let retry = problem["retry_after_seconds"]
        .as_u64()
        .expect("retry member");
    assert!(retry >= 1, "the breaker quotes its cooldown: {problem}");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some(retry.to_string().as_str())
    );
}

// ------------------------------------------------------------------ 504

#[tokio::test]
async fn an_upstream_that_never_answers_is_a_504_with_request_context() {
    let port = silent_upstream().await;
    let h = common::harness_with(oagw::config::OagwConfig {
        proxy_timeout_secs: 1,
        ..common::test_config()
    });
    let (status, upstream) =
        create_upstream(&h, common::http_upstream("127.0.0.1", port, Some("still"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/still/v1/chat", &[], None)
        .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{problem}");
    assert_problem(&problem, "timeout.request.v1", 504);
    assert_eq!(problem["upstream_id"], json!(upstream_id));
    assert_eq!(problem["path"], json!("/v1/chat"));
    let host = problem["host"].as_str().expect("host member");
    assert!(
        host.contains("127.0.0.1"),
        "the endpoint actually dialled is named: {problem}"
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway"),
        "an incomplete response is a gateway-side error"
    );
}

// ------------------------------------------------------- trace correlation

#[tokio::test]
async fn the_client_trace_id_is_correlated_on_an_error() {
    let h = common::harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream("127.0.0.1", 1, Some("traced"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, _) = common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, problem, headers) = h
        .json(
            Method::GET,
            "/oagw/v1/proxy/traced/anything",
            &[("x-oagw-trace-id", "trace-1234")],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(
        headers.get("x-oagw-trace-id").and_then(|v| v.to_str().ok()),
        Some("trace-1234"),
        "the correlation id is echoed on the response"
    );
}
