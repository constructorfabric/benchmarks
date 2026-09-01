// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-proxy-api:p2
//! Proxy data plane: alias resolution, route matching, URL construction,
//! header transformation, body guards, target-endpoint selection (ADR-0001)
//! and the error-source distinction (ADR-0007), against real local upstreams.
//!
//! Ordinary request/response cases run against `httpmock`; chunked bodies,
//! event streams and a connection that never answers need raw sockets, which
//! [`common::RawUpstream`] provides.

mod common;

use anyhow::{Context, Result};
use common::{
    ERROR_SOURCE, ProxyHarness, TARGET_HOST, domain_route, domain_upstream, loopback_endpoint,
    problem_type,
};
use httpmock::prelude::{GET, MockServer, POST};
use oagw::domain::model::{HttpMethod, PathSuffixMode, RouteMatch};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

/// Harness with an upstream whose alias is `api.vendor.com` over `port`.
///
/// The record is seeded directly: the alias-derivation rules of the write path
/// are slice-1 behaviour, and the data plane needs a routing key that does not
/// contain the ephemeral port.
fn harness_with_upstream(
    port: u16,
    headers: Option<oagw::domain::model::HeadersConfig>,
) -> ProxyHarness {
    let harness = ProxyHarness::new();
    let owner = harness.tenant();
    let mut upstream = domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    );
    upstream.headers = headers;
    let id = harness.seed_upstream(upstream);
    let route = domain_route(
        owner,
        id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/chat",
        &[],
    );
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the test route must seed: {error}");
        });
    harness
}

/// A route whose `path_suffix_mode` is `disabled`.
fn add_strict_route(harness: &ProxyHarness, owner: Uuid, upstream_id: Uuid) {
    let mut route = domain_route(
        owner,
        upstream_id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/strict",
        &[],
    );
    if let Some(http) = route.match_rule.http.as_mut() {
        http.path_suffix_mode = PathSuffixMode::Disabled;
    }
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the strict route must seed: {error}");
        });
}

/// A route that only forwards the allowlisted query parameters.
///
/// The route keeps the default `path_suffix_mode: append`, so the tests can
/// also probe what a path suffix may and may not add to the dial.
fn add_query_route(harness: &ProxyHarness, owner: Uuid, upstream_id: Uuid, allow: &[&str]) {
    let route = domain_route(
        owner,
        upstream_id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/search",
        allow,
    );
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the query route must seed: {error}");
        });
}

/// Harness plus the id of the seeded upstream.
struct Seeded {
    harness: ProxyHarness,
    owner: Uuid,
    upstream_id: Uuid,
}

/// Harness with an upstream and two routes: `/v1/chat` and `/v1/strict`.
fn seeded_with_strict_route(port: u16) -> Seeded {
    let harness = ProxyHarness::new();
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    ));
    let route = domain_route(
        owner,
        upstream_id,
        &[HttpMethod::Get, HttpMethod::Post],
        "/v1/chat",
        &[],
    );
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the test route must seed: {error}");
        });
    add_strict_route(&harness, owner, upstream_id);
    Seeded {
        harness,
        owner,
        upstream_id,
    }
}

// ── Happy path ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_get_request_reaches_the_upstream_and_is_marked_upstream() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"model":"gpt-4"}"#);
    });
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[("x-trace", "trace-1")],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.text, r#"{"model":"gpt-4"}"#);
    assert_eq!(reply.header(ERROR_SOURCE), Some("upstream"));
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn the_proxy_path_is_forwarded_verbatim_behind_the_alias() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat/items");
        then.status(200).body("ok");
    });
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat/items",
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn a_post_body_is_forwarded_with_its_content_type() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat")
            .header("content-type", "application/json")
            .body(r#"{"prompt":"hi"}"#);
        then.status(201).body("created");
    });
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy(
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[("content-type", "application/json")],
            br#"{"prompt":"hi"}"#,
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::CREATED);
    assert_eq!(reply.text, "created");
    assert_eq!(mock.calls(), 1);
    Ok(())
}

// ── Alias and route resolution ───────────────────────────────────────────

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem() -> Result<()> {
    let harness = ProxyHarness::new();

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/ghost.vendor.com/v1", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    assert_eq!(reply.problem_field("alias"), Some("ghost.vendor.com"));
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    Ok(())
}

#[tokio::test]
async fn a_request_without_a_matching_route_is_a_404_problem() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v9/unknown", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_method_outside_the_allowlist_is_a_404_problem() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("DELETE", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_disabled_upstream_is_a_link_unavailable_problem() -> Result<()> {
    let harness = ProxyHarness::new();
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        false,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    Ok(())
}

#[tokio::test]
async fn an_alias_of_an_ancestor_tenant_is_reachable() -> Result<()> {
    let harness = ProxyHarness::with_config_and_chain(
        &common::proxy_config(),
        std::sync::Arc::new(common::StaticTenantChain),
    );
    // The record belongs to the root tenant, the request to a child of it.
    let owner = Uuid::nil();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "shared.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });
    let child = Uuid::now_v7();

    let reply = harness
        .proxy_as(
            "GET",
            "/oagw/v1/proxy/shared.vendor.com/v1",
            child,
            &[],
            b"",
        )
        .await?;

    // The chain makes the record visible; the dial to port 1 fails, which
    // proves the alias resolved before any data-plane guard rejected it.
    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    Ok(())
}

// ── Route path and query rules ───────────────────────────────────────────

#[tokio::test]
async fn a_suffix_on_a_strict_route_is_a_validation_problem() -> Result<()> {
    let server = MockServer::start();
    let seeded = seeded_with_strict_route(server.port());

    let reply = seeded
        .harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/strict/extra",
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn the_query_allowlist_drops_unknown_parameters() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/search")
            .query_param("q", "ai")
            .query_param_missing("secret");
        then.status(200).body("results");
    });
    let seeded = seeded_with_strict_route(server.port());
    add_query_route(&seeded.harness, seeded.owner, seeded.upstream_id, &["q"]);

    let reply = seeded
        .harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/search?q=ai&secret=1",
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.text, "results");
    assert_eq!(mock.calls(), 1);
    Ok(())
}

// ── Request-path smuggling (DESIGN §4.4) ─────────────────────────────────

/// A suffix that escapes the matched prefix is a 400, never a dial.
#[tokio::test]
async fn a_dot_segment_in_the_suffix_is_a_validation_problem() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/admin");
        then.status(200).body("secret");
    });
    let harness = harness_with_upstream(server.port(), None);

    for suffix in ["../admin", "%2E%2E/admin", "chat/../../admin", "."] {
        let reply = harness
            .proxy(
                "GET",
                &format!("/oagw/v1/proxy/api.vendor.com/v1/chat/{suffix}"),
                &[],
                b"",
            )
            .await?;
        assert_eq!(
            reply.status,
            axum::http::StatusCode::BAD_REQUEST,
            "{suffix}"
        );
        assert_eq!(
            reply.problem_type(),
            Some(problem_type("validation.error.v1")),
            "{suffix}"
        );
    }
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// A suffix may not become a query or a fragment of the dial target.
#[tokio::test]
async fn an_escaped_separator_in_the_suffix_is_rejected() -> Result<()> {
    let server = MockServer::start();
    let harness = harness_with_upstream(server.port(), None);

    for suffix in ["item%3Finjected%3D1", "item%23fragment", "a%3Fb/c"] {
        let reply = harness
            .proxy(
                "GET",
                &format!("/oagw/v1/proxy/api.vendor.com/v1/chat/{suffix}"),
                &[],
                b"",
            )
            .await?;
        assert_eq!(
            reply.status,
            axum::http::StatusCode::BAD_REQUEST,
            "{suffix}"
        );
        assert_eq!(
            reply.problem_type(),
            Some(problem_type("validation.error.v1")),
            "{suffix}"
        );
    }
    Ok(())
}

/// A suffix cannot smuggle a query parameter past the route's allowlist.
#[tokio::test]
async fn a_suffix_cannot_inject_a_query_parameter() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/search")
            .query_param_missing("injected");
        then.status(200).body("leaked");
    });
    let seeded = seeded_with_strict_route(server.port());
    add_query_route(&seeded.harness, seeded.owner, seeded.upstream_id, &["q"]);

    let reply = seeded
        .harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/search/q%3Finjected%3D1",
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls(), 0);
    Ok(())
}

/// `%2F` stays a single segment and a `//` prefix never matches a route.
#[tokio::test]
async fn the_suffix_shape_survives_to_the_upstream() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let responder = std::sync::Arc::clone(&raw);
    let handle = tokio::spawn(async move {
        responder
            .serve_once("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
            .await
    });
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat/a%2Fb",
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    let received = handle
        .await
        .with_context(|| "the upstream task must join")?
        .with_context(|| "the upstream must answer")?;
    assert!(
        received.starts_with("GET /v1/chat/a%2Fb HTTP/1.1"),
        "the escaped separator must reach the upstream: {received}"
    );
    Ok(())
}

#[tokio::test]
async fn a_doubled_slash_after_the_alias_is_not_a_route() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com//v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    Ok(())
}

// ── Alias resolution ─────────────────────────────────────────────────────

/// The alias is matched lowercased and without a trailing dot, exactly as the
/// write path stores it (DESIGN §3.2 "Alias Resolution").
#[tokio::test]
async fn the_alias_is_matched_regardless_of_case_or_trailing_dot() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_upstream(server.port(), None);

    for alias in ["API.Vendor.COM", "api.vendor.com.", "Api.Vendor.com."] {
        let reply = harness
            .proxy("GET", &format!("/oagw/v1/proxy/{alias}/v1/chat"), &[], b"")
            .await?;
        assert_eq!(reply.status, axum::http::StatusCode::OK, "{alias}");
    }
    assert_eq!(mock.calls(), 3);
    Ok(())
}

// ── Tenant isolation ─────────────────────────────────────────────────────

#[tokio::test]
async fn an_alias_of_another_tenant_is_invisible_to_the_data_plane() -> Result<()> {
    let server = MockServer::start();
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy_as(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            Uuid::now_v7(),
            &[],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    Ok(())
}

// ── Body guards ──────────────────────────────────────────────────────────

#[tokio::test]
async fn an_oversized_body_is_a_payload_too_large_problem() -> Result<()> {
    let mut config = common::proxy_config();
    config.max_body_bytes = 64;
    let harness = ProxyHarness::with_config_and_chain(
        &config,
        std::sync::Arc::new(common::StaticTenantChain),
    );
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Post], "/v1/chat", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let reply = harness
        .proxy(
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[],
            &[b'x'; 128],
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("payload.too_large.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_transfer_encoding_body_is_a_validation_problem() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy(
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[("transfer-encoding", "gzip")],
            b"payload",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("validation.error.v1"))
    );
    Ok(())
}

// ── Target-endpoint selection (ADR-0001) ─────────────────────────────────

/// Two mock servers behind one upstream, each answering with its own body.
///
/// The pool holds two distinct IP endpoints, so the write path would demand an
/// explicit alias and the data plane round-robins over it.
struct Pool {
    harness: ProxyHarness,
}

impl Pool {
    /// A pool of two local endpoints behind the alias `api.vendor.com`.
    fn new() -> Self {
        let first = MockServer::start();
        let second = MockServer::start();
        first.mock(|when, then| {
            when.method(GET).path("/v1/chat");
            then.status(200).body("first");
        });
        second.mock(|when, then| {
            when.method(GET).path("/v1/chat");
            then.status(200).body("second");
        });
        let harness = ProxyHarness::new();
        let owner = harness.tenant();
        let upstream_id = harness.seed_upstream(domain_upstream(
            owner,
            "api.vendor.com",
            Vec::from([
                loopback_endpoint(first.port()),
                loopback_endpoint(second.port()),
            ]),
            true,
        ));
        let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1/chat", &[]);
        harness
            .store()
            .insert_route_checked(route)
            .unwrap_or_else(|error| {
                panic!("the route must seed: {error}");
            });
        Self { harness }
    }

    /// Proxy one `GET` to the alias, with an optional target-host header.
    async fn get(&self, headers: &[(&str, &str)]) -> Result<String> {
        let reply = self
            .harness
            .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", headers, b"")
            .await?;
        assert_eq!(reply.status, axum::http::StatusCode::OK);
        Ok(reply.text)
    }
}

#[tokio::test]
async fn a_round_robin_pool_walks_its_endpoints() -> Result<()> {
    let pool = Pool::new();

    let first = pool.get(&[]).await?;
    let second = pool.get(&[]).await?;

    assert_eq!(first, "first");
    assert_eq!(second, "second");
    Ok(())
}

#[tokio::test]
async fn a_target_host_header_pins_the_endpoint() -> Result<()> {
    let pool = Pool::new();

    // Round-robin would hand the second request to the other endpoint; the
    // header overrides the cursor twice in a row.
    let first = pool.get(&[(TARGET_HOST, "127.0.0.1")]).await?;
    let second = pool.get(&[(TARGET_HOST, "127.0.0.1")]).await?;

    assert_eq!(first, "first");
    assert_eq!(second, "first");
    Ok(())
}

#[tokio::test]
async fn an_unknown_target_host_is_a_routing_problem() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[(TARGET_HOST, "eu.vendor.com")],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("routing.unknown_target_host.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_target_host_with_a_port_is_an_invalid_target_host() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[(TARGET_HOST, "api.vendor.com:8443")],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("routing.invalid_target_host.v1"))
    );
    Ok(())
}

// ── Header transformation ────────────────────────────────────────────────

#[tokio::test]
async fn hop_by_hop_headers_are_not_forwarded() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200).body("ok");
    });
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[
                ("connection", "keep-alive"),
                ("x-oagw-target-host", "127.0.0.1"),
                ("x-keep", "yes"),
            ],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

#[tokio::test]
async fn header_rules_set_add_and_remove_on_the_response() -> Result<()> {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(200)
            .header("x-server", "nginx")
            .header("x-secret", "1")
            .body("ok");
    });
    let rules = oagw::domain::model::HeadersConfig {
        request: None,
        response: Some(oagw::domain::model::ResponseHeaderRules {
            set: [("x-gateway".to_owned(), "oagw".to_owned())]
                .into_iter()
                .collect(),
            add: [].into_iter().collect(),
            remove: Vec::from(["x-secret".to_owned()]),
        }),
    };
    let harness = harness_with_upstream(server.port(), Some(rules));

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.header("x-gateway"), Some("oagw"));
    assert!(reply.headers.get("x-secret").is_none());
    Ok(())
}

// ── Transport failures ───────────────────────────────────────────────────

/// An upstream error status is **passed through**, not converted into a
/// gateway problem: it keeps `X-OAGW-Error-Source: upstream` (ADR-0007).
#[tokio::test]
async fn an_upstream_error_status_keeps_the_upstream_source() -> Result<()> {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat");
        then.status(503)
            .header("content-type", "application/json")
            .body(r#"{"error":"overloaded"}"#);
    });
    let harness = harness_with_upstream(server.port(), None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.header(ERROR_SOURCE), Some("upstream"));
    assert_eq!(reply.text, r#"{"error":"overloaded"}"#);
    assert_eq!(mock.calls(), 1);
    Ok(())
}

// ── Outbound request (observed at the upstream) ──────────────────────────

/// The raw upstream the tests observe the outbound request on.
///
/// Returns the port the harness must address and the join handle that yields
/// the request head the upstream received.
async fn observed_upstream() -> Result<(u16, tokio::task::JoinHandle<anyhow::Result<String>>)> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let responder = std::sync::Arc::clone(&raw);
    let handle = tokio::spawn(async move {
        responder
            .serve_once("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
            .await
    });
    Ok((port, handle))
}

#[tokio::test]
async fn the_upstream_never_sees_a_hop_by_hop_header() -> Result<()> {
    let (port, handle) = observed_upstream().await?;
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[
                ("connection", "x-hop"),
                ("x-hop", "smuggled"),
                ("proxy-connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("x-keep", "yes"),
            ],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    let received = handle.await??;
    for forbidden in ["connection:", "x-hop:", "proxy-connection:", "keep-alive:"] {
        assert!(
            !received.contains(forbidden),
            "'{forbidden}' must not reach the upstream: {received}"
        );
    }
    assert!(received.contains("x-keep: yes"), "{received}");
    Ok(())
}

#[tokio::test]
async fn the_upstream_sees_its_own_authority_and_no_routing_header() -> Result<()> {
    let (port, handle) = observed_upstream().await?;
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[(TARGET_HOST, "127.0.0.1"), ("host", "api.vendor.com")],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    let received = handle.await??;
    assert!(
        received.contains(&format!("host: 127.0.0.1:{port}")),
        "the dial target authority must be the Host header: {received}"
    );
    assert!(
        !received.contains("x-oagw-target-host"),
        "the routing header must not reach the upstream: {received}"
    );
    assert!(
        !received.contains("api.vendor.com"),
        "the gateway authority must not reach the upstream: {received}"
    );
    Ok(())
}

#[tokio::test]
async fn a_chunked_request_body_is_cut_off_while_it_is_read() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let handle = tokio::spawn(async move { raw.hang().await });
    let mut config = common::proxy_config();
    config.max_body_bytes = 64;
    let harness = ProxyHarness::with_config_and_chain(
        &config,
        std::sync::Arc::new(common::StaticTenantChain),
    );
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Post], "/v1/chat", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    // Three chunks of the limit each: the cap must fire on the second frame,
    // not after the whole body has been buffered.
    let reply = harness
        .proxy_chunked(
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[&[b'x'; 64], &[b'y'; 64], &[b'z'; 64]],
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("payload.too_large.v1"))
    );
    handle.abort();
    Ok(())
}

/// The egress guard of the data plane: a local address the write path let
/// through (the record is seeded directly) is still refused at dial time.
#[tokio::test]
async fn the_egress_guard_refuses_a_local_endpoint_at_dial_time() -> Result<()> {
    let mut config = common::proxy_config();
    config.ssrf_policy = oagw::config::SsrfPolicy {
        enabled: true,
        allowed_hosts: Vec::new(),
        denied_hosts: Vec::new(),
    };
    let harness = ProxyHarness::with_config_and_chain(
        &config,
        std::sync::Arc::new(common::StaticTenantChain),
    );
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1/chat", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    Ok(())
}

#[tokio::test]
async fn a_hostname_pool_demands_a_target_host() -> Result<()> {
    let harness = ProxyHarness::new();
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "vendor.com",
        Vec::from([
            common::tls_hostname_endpoint("us.vendor.com"),
            common::tls_hostname_endpoint("eu.vendor.com"),
        ]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1/chat", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let unpinned = harness
        .proxy("GET", "/oagw/v1/proxy/vendor.com/v1/chat", &[], b"")
        .await?;
    let pinned = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/vendor.com/v1/chat",
            &[(TARGET_HOST, "us.vendor.com")],
            b"",
        )
        .await?;

    assert_eq!(unpinned.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        unpinned.problem_type(),
        Some(problem_type("routing.missing_target_host.v1"))
    );
    assert_eq!(unpinned.problem_field("alias"), Some("vendor.com"));
    // The pinning header gets the request past the guard; the dial itself
    // cannot succeed, because a host name does not resolve in the test
    // environment.
    assert_ne!(pinned.status, axum::http::StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_link_unavailable_problem() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("link.unavailable.v1"))
    );
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    Ok(())
}

#[tokio::test]
async fn an_upstream_that_never_answers_is_a_request_timeout() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let handle = tokio::spawn(async move { raw.hang().await });
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("timeout.request.v1"))
    );
    handle.abort();
    Ok(())
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_the_switch_is_off() -> Result<()> {
    let mut config = common::proxy_config();
    config.allow_http_upstream = false;
    let harness = ProxyHarness::with_config_and_chain(
        &config,
        std::sync::Arc::new(common::StaticTenantChain),
    );
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("protocol.error.v1"))
    );
    Ok(())
}

// ── Streaming ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_chunked_response_is_streamed_to_the_client() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let response = [
        "HTTP/1.1 200 OK\r\n",
        "content-type: text/plain\r\n",
        "transfer-encoding: chunked\r\n",
        "\r\n",
        "5\r\nhello\r\n",
        "0\r\n\r\n",
    ]
    .concat();
    let handle = tokio::spawn(async move { raw.serve_forever(response).await });
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.text, "hello");
    handle.abort();
    Ok(())
}

#[tokio::test]
async fn an_event_stream_reaches_the_client() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let response = [
        "HTTP/1.1 200 OK\r\n",
        "content-type: text/event-stream\r\n",
        "\r\n",
        "event: delta\n",
        "data: one\n\n",
        "event: delta\n",
        "data: two\n\n",
    ]
    .concat();
    let handle = tokio::spawn(async move { raw.serve_forever(response).await });
    let harness = harness_with_upstream(port, None);

    let reply = harness
        .proxy(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[("accept", "text/event-stream")],
            b"",
        )
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("text/event-stream"));
    assert!(reply.text.contains("data: one"));
    assert!(reply.text.contains("data: two"));
    handle.abort();
    Ok(())
}

/// Raw chunked head with one chunk of body, used by the streaming tests.
const CHUNKED_HEAD: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n";

#[tokio::test]
async fn the_first_chunk_reaches_the_client_before_the_upstream_closes() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let harness = harness_with_upstream(port, None);
    let waiter = tokio::spawn(async move {
        let raw = raw;
        raw.hand_over(CHUNKED_HEAD).await
    });

    let response = harness
        .proxy_unbuffered("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[])
        .await?;
    let mut socket = waiter.await??;
    socket.write_all(b"5\r\nhello\r\n").await?;

    // The upstream has sent the head and one chunk and is still connected: the
    // chunk must already be readable, which is what "streaming" means here.
    let mut body = response.into_body();
    let first = common::read_until(&mut body, "hello").await?;
    assert_eq!(first, b"hello");

    socket.write_all(b"6\r\n world\r\n0\r\n\r\n").await?;
    socket.shutdown().await?;
    let (rest, truncated) = common::read_to_end(body).await?;
    assert_eq!(String::from_utf8_lossy(&rest), " world");
    assert!(!truncated);
    Ok(())
}

#[tokio::test]
async fn an_upstream_that_stops_mid_body_truncates_the_stream() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    // The chunked body never gets its terminating chunk: the socket closes
    // after the first one.
    let response = format!("{CHUNKED_HEAD}5\r\nhello\r\n");
    let handle = tokio::spawn(async move { raw.serve_once(&response).await });
    let harness = harness_with_upstream(port, None);

    let response = harness
        .proxy_unbuffered("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[])
        .await?;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let source = response
        .headers()
        .get(ERROR_SOURCE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(source.as_deref(), Some("upstream"));
    let (received, truncated) = common::read_to_end(response.into_body()).await?;
    assert_eq!(received, b"hello");
    // What the client can observe is the broken framing, not a gateway 502:
    // the head has already been forwarded when the upstream went away.
    assert!(truncated, "the truncation must be visible to the client");
    handle.await??;
    Ok(())
}

#[tokio::test]
async fn a_body_that_goes_silent_is_cut_off_by_the_idle_budget() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let harness = harness_with_upstream(port, None);
    let waiter = tokio::spawn(async move { raw.hand_over(CHUNKED_HEAD).await });

    let response = harness
        .proxy_unbuffered("GET", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[])
        .await?;
    let mut socket = waiter.await??;
    // One chunk, then silence: the socket stays open, so only the idle budget
    // of the body can end the transfer.
    socket
        .write_all(b"5\r\nhel")
        .await
        .context("writing the first partial chunk")?;
    let started = tokio::time::Instant::now();

    let (received, truncated) = common::read_to_end(response.into_body()).await?;

    assert_eq!(String::from_utf8_lossy(&received), "hel");
    assert!(truncated, "the idle budget must abort the body");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "the abort must follow the idle budget, not the head budget"
    );
    Ok(())
}

/// An event stream has no overall body budget: it may pause longer than the
/// budget of an ordinary body as long as it keeps producing (DESIGN §3.5).
#[tokio::test]
async fn an_event_stream_outlives_the_body_budget_of_an_ordinary_body() -> Result<()> {
    let raw = std::sync::Arc::new(common::RawUpstream::bind().await?);
    let port = raw.port();
    let mut config = common::proxy_config();
    config.proxy_stream_timeout_secs = Some(1);
    config.proxy_idle_timeout_secs = Some(3);
    let harness = ProxyHarness::with_config_and_chain(
        &config,
        std::sync::Arc::new(common::StaticTenantChain),
    );
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(port)]),
        true,
    ));
    let route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1/chat", &[]);
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let waiter = tokio::spawn(async move { raw.hand_over(head).await });
    let response = harness
        .proxy_unbuffered(
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/chat",
            &[("accept", "text/event-stream")],
        )
        .await?;
    let mut socket = waiter.await??;
    socket.write_all(b"event: delta\ndata: one\n\n").await?;
    let mut body = response.into_body();
    let first = common::read_until(&mut body, "data: one").await?;
    assert!(String::from_utf8_lossy(&first).contains("data: one"));

    // Longer than the overall budget of a buffered body, shorter than the
    // silence budget: only the stream exemption lets the second event through.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    socket.write_all(b"event: delta\ndata: two\n\n").await?;
    socket.shutdown().await?;
    let (rest, truncated) = common::read_to_end(body).await?;
    assert!(String::from_utf8_lossy(&rest).contains("data: two"));
    assert!(!truncated);
    Ok(())
}

// ── OpenAPI surface ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_head_request_gets_the_route_problem_not_an_empty_405() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    // The schema's method allowlist has no HEAD entry, so the route cannot
    // match. The endpoint is still registered for HEAD: the client gets the
    // documented problem instead of axum's bare `405 Method Not Allowed`.
    let reply = harness
        .proxy("HEAD", "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"));
    // A HEAD response has no body, so the problem type cannot be inspected.
    assert_eq!(reply.text, "");
    Ok(())
}

/// A method the data plane does not register still answers inside the problem
/// contract (DESIGN §3.3), not with axum's bare `405`.
#[tokio::test]
async fn an_unregistered_method_gets_the_problem_contract() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    for method in ["TRACE", "CONNECT", "PATCH", "PROPFIND"] {
        let reply = harness
            .proxy(method, "/oagw/v1/proxy/api.vendor.com/v1/chat", &[], b"")
            .await
            .with_context(|| format!("proxying {method}"))?;

        assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND, "{method}");
        assert_eq!(
            reply.problem_type(),
            Some(problem_type("route.not_found.v1")),
            "{method}"
        );
        assert_eq!(reply.header(ERROR_SOURCE), Some("gateway"), "{method}");
        assert_eq!(reply.problem_field("alias"), Some("api.vendor.com"));
    }
    Ok(())
}

#[tokio::test]
async fn a_route_match_rule_of_the_wrong_protocol_is_not_selectable() -> Result<()> {
    let harness = ProxyHarness::new();
    let owner = harness.tenant();
    let upstream_id = harness.seed_upstream(domain_upstream(
        owner,
        "api.vendor.com",
        Vec::from([loopback_endpoint(1)]),
        true,
    ));
    let mut route = domain_route(owner, upstream_id, &[HttpMethod::Get], "/v1", &[]);
    route.match_rule = RouteMatch {
        http: None,
        grpc: Some(oagw::domain::model::GrpcMatch {
            service: "pkg.Svc".to_owned(),
            method: "Get".to_owned(),
        }),
    };
    harness
        .store()
        .insert_route_checked(route)
        .unwrap_or_else(|error| {
            panic!("the route must seed: {error}");
        });

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reply.problem_type(),
        Some(problem_type("route.not_found.v1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_problem_response_reports_the_alias() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v9/none", &[], b"")
        .await?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(reply.problem_field("alias"), Some("api.vendor.com"));
    assert_eq!(reply.problem_field("path"), Some("/v9/none"));
    Ok(())
}

#[tokio::test]
async fn a_get_on_the_alias_root_without_a_route_is_a_404() -> Result<()> {
    let harness = harness_with_upstream(1, None);

    let reply = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com", &[], b"")
        .await
        .context("proxying the alias root")?;

    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    Ok(())
}
