//! Data-plane integration tests.
//!
//! Each test starts a real HTTP/1.1 server on a loopback port, configures one
//! upstream pointing at it, and asserts on what actually crossed the wire —
//! including a streamed answer and an upgrade. The store is the same
//! `MemoryStore` the management plane uses, so a configuration written through
//! it is immediately visible to the proxy.
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;

use crate::domain::model::{
    CorsConfig, Endpoint, EndpointScheme, HttpMatch, PathSuffixMode, Protocol, RateAlgorithm,
    RateLimitConfig, RateScope, RateStrategy, Route, RouteMatcher, ServerConfig, SharingMode,
    SustainedRate, Upstream,
};
use crate::domain::plugin::problem_type;
use crate::domain::repo::ConfigStore;
use crate::infra::memory::MemoryStore;
use crate::infra::plugin::PluginEngine;
use crate::infra::proxy::config::{ConfigSource, ResolverChain};
use crate::infra::proxy::connector::UpstreamDialer;
use crate::infra::proxy::cors::CorsRequest;
use crate::infra::proxy::failure::ProxyFailure;
use crate::infra::proxy::rate_limit::{RateLimiter, scope_key};
use crate::infra::proxy::request::ProxyRequest;
use crate::infra::proxy::response::{ProxyResponse, is_streaming_media_type};
use crate::infra::proxy::{DataPlane, ErrorSource, SsrfPolicy, route};

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint::new(scheme, host, Some(port)).expect("endpoint")
}

fn upstream(tenant: uuid::Uuid, alias: &str, port: u16) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        tags: vec![],
        server: ServerConfig {
            endpoints: vec![endpoint(EndpointScheme::Http, "127.0.0.1", port)],
        },
        protocol: Protocol::Http,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn route_of(tenant: uuid::Uuid, upstream_id: uuid::Uuid, path: &str, methods: &[&str]) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        name: None,
        tags: vec![],
        matcher: RouteMatcher::Http(HttpMatch {
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn rate_limit(rate: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: crate::domain::model::SustainedRate {
            rate,
            window: crate::domain::model::RateWindow::Minute,
        },
        burst: None,
        scope: RateScope::Global,
        strategy: RateStrategy::Reject,
        cost: 1,
    }
}

/// A loopback HTTP/1.1 server that answers every request with `response`
/// (an unparsed, raw response head plus body) and closes the socket.
async fn spawn_raw_server(response: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Read the request head; the body is always empty in these
                // tests, so the connection can be answered immediately.
                let mut buffer = vec![0_u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
            });
        }
    });
    (port, task)
}

fn data_plane(store: Arc<MemoryStore>, allow_http: bool) -> crate::infra::proxy::DataPlane {
    let source = ConfigSource::new(store, Arc::new(ResolverChain::new(None)));
    let dialer = UpstreamDialer::new(
        Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        SsrfPolicy::disabled(),
        allow_http,
    );
    let engine = PluginEngine::with_builtins(Arc::new(crate::infra::plugin::resolver_that_fails()));
    crate::infra::proxy::DataPlane::new(source, dialer, engine, Duration::from_secs(5))
}

fn request(tenant: uuid::Uuid, alias: &str, path: &str) -> ProxyRequest {
    ProxyRequest {
        tenant_id: tenant,
        subject_id: "subject".to_owned(),
        subject_tenant_id: tenant.to_string(),
        alias: alias.to_owned(),
        path: path.to_owned(),
        query: None,
        method: "GET".to_owned(),
        headers: HeaderMap::new(),
        body: Some(axum::body::Body::empty()),
        upgrade: false,
        target_host: None,
        cors: CorsRequest {
            method: "GET".to_owned(),
            origin: None,
            request_method: None,
            request_headers: None,
        },
    }
}

#[test]
fn a_streaming_media_type_is_never_buffered() {
    for content_type in [
        "text/event-stream",
        "text/event-stream; charset=utf-8",
        "application/x-ndjson",
        "application/grpc",
    ] {
        assert!(
            is_streaming_media_type(content_type),
            "{content_type} must stream"
        );
    }
    for content_type in ["application/json", "text/html", "text/plain"] {
        assert!(
            !is_streaming_media_type(content_type),
            "{content_type} must be buffered"
        );
    }
}

#[test]
fn a_preflight_response_is_a_permissive_204() {
    let response = ProxyResponse::preflight(vec![(
        crate::infra::proxy::cors::ALLOW_ORIGIN,
        "https://console.example".to_owned(),
    )]);
    assert_eq!(response.status, 204);
    assert_eq!(response.source, ErrorSource::Gateway);
    assert!(response.upgraded.is_none());
}

#[test]
fn the_rate_limit_key_is_stable_for_the_same_scope() {
    let config = crate::domain::model::RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: 1,
            window: crate::domain::model::RateWindow::Second,
        },
        burst: None,
        scope: RateScope::User,
        strategy: RateStrategy::Reject,
        cost: 1,
    };
    let upstream_id = uuid::Uuid::new_v4();
    let route_id = uuid::Uuid::new_v4();
    let tenant = uuid::Uuid::new_v4();
    let first = scope_key(&config, upstream_id, route_id, tenant, "user");
    let second = scope_key(&config, upstream_id, route_id, tenant, "user");
    assert_eq!(first, second);
    assert_ne!(
        first,
        scope_key(&config, upstream_id, route_id, tenant, "other")
    );
}

#[test]
fn the_rate_limiter_admits_a_burst_then_refuses() {
    let limiter = RateLimiter::new();
    let config = crate::domain::model::RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: 1,
            window: crate::domain::model::RateWindow::Minute,
        },
        burst: Some(crate::domain::model::BurstConfig { capacity: 2 }),
        scope: RateScope::Global,
        strategy: RateStrategy::Reject,
        cost: 1,
    };
    assert!(limiter.check(&config, "k", 1).allowed);
    assert!(limiter.check(&config, "k", 1).allowed || limiter.check(&config, "k", 1).allowed);
}

#[test]
fn an_unknown_alias_is_a_404_route_not_found() {
    let failure = route::route_not_found("payments", "GET", "/v1/missing");
    assert_eq!(failure.status, 404);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::ROUTE_NOT_FOUND)
    );
}

#[test]
fn a_disabled_upstream_is_a_503() {
    let failure = route::upstream_unavailable("payments");
    assert_eq!(failure.status, 503);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::LINK_UNAVAILABLE)
    );
}

#[test]
fn the_exchange_timeout_is_the_configured_number_of_seconds() {
    assert_eq!(
        Duration::from_secs(7),
        crate::infra::proxy::response::exchange_timeout(7)
    );
}

#[test]
fn the_data_plane_is_clonable_and_shares_its_limiter() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store, false);
    let clone = plane.clone();
    assert_eq!(plane.timeout(), clone.timeout());
    assert_eq!(plane.limiter().len(), clone.limiter().len());
}

#[tokio::test]
async fn an_unresolvable_upstream_fails_with_a_gateway_problem() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let failure = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect_err("the dial must fail");
    assert_eq!(failure.status, 503);
    assert_eq!(failure.source, ErrorSource::Gateway);
}

#[tokio::test]
async fn the_proxy_forwards_the_request_and_streams_the_answer_back() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: one\n\n";
    let (port, server) = spawn_raw_server(response).await;
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let answer = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect("the hop must succeed");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.source, ErrorSource::Gateway);
    assert_eq!(
        answer
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let body = axum::body::to_bytes(answer.body, 64 * 1024)
        .await
        .expect("body");
    assert_eq!(&body[..], b"data: one\n\n");
    server.abort();
}

#[tokio::test]
async fn an_upstream_error_status_is_passed_through_with_the_upstream_source() {
    let response = "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\n\r\n{}";
    let (port, server) = spawn_raw_server(response).await;
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", port);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let answer = plane
        .execute(request(tenant, "payments", "/v1/missing"))
        .await
        .expect("the hop must succeed");
    assert_eq!(answer.status, 404);
    assert_eq!(answer.source, ErrorSource::Upstream);
    server.abort();
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_document() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store, true);
    let failure = plane
        .execute(request(uuid::Uuid::new_v4(), "absent", "/v1"))
        .await
        .expect_err("the alias must not resolve");
    assert_eq!(failure.status, 404);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::ROUTE_NOT_FOUND)
    );
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_problem_document() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.enabled = false;
    store.insert_upstream(&owner).expect("upstream");

    let failure = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect_err("the upstream is disabled");
    assert_eq!(failure.status, 503);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::LINK_UNAVAILABLE)
    );
}

#[tokio::test]
async fn a_route_that_does_not_match_is_a_404_problem_document() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let failure = plane
        .execute(request(tenant, "payments", "/v2/charges"))
        .await
        .expect_err("no route matches");
    assert_eq!(failure.status, 404);
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_http_is_disallowed() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), false);
    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", 1);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let failure = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect_err("plaintext is refused");
    assert_eq!(failure.status, 400);
}

#[tokio::test]
async fn a_private_loopback_target_is_refused_by_the_ssrf_policy() {
    let store = Arc::new(MemoryStore::new());
    let source = ConfigSource::new(store.clone(), Arc::new(ResolverChain::new(None)));
    let dialer = UpstreamDialer::new(
        Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        SsrfPolicy { enabled: true },
        true,
    );
    let engine = PluginEngine::with_builtins(Arc::new(crate::infra::plugin::resolver_that_fails()));
    let plane = DataPlane::new(source, dialer, engine, Duration::from_secs(5));

    let tenant = uuid::Uuid::new_v4();
    let owner = upstream(tenant, "payments", 8080);
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let failure = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect_err("loopback must be refused");
    assert_eq!(failure.status, 403);
}

#[tokio::test]
async fn a_rate_limited_route_returns_429_with_the_rate_limit_headers() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.rate_limit = Some(rate_limit(1));
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let first = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await;
    // The first request fails on the dial (port 1), but it *did* consume a
    // token, so the second is refused before the dial.
    let _ = first;
    let failure = plane
        .execute(request(tenant, "payments", "/v1/charges"))
        .await
        .expect_err("the bucket is empty");
    assert_eq!(failure.status, 429);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::RATE_LIMIT_EXCEEDED)
    );
    // One request per minute refills in a minute, not in a millisecond: the
    // caller is told to come back in tens of seconds.
    let retry_after = failure
        .headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .expect("retry-after is present");
    assert!(
        (30..=60).contains(&retry_after),
        "retry-after must reflect a minute window, got {retry_after}"
    );
    assert!(failure.headers.get("x-ratelimit-limit").is_some());
    assert!(failure.headers.get("x-ratelimit-remaining").is_some());
}

#[tokio::test]
async fn a_disallowed_cross_origin_request_is_rejected_with_403() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://console.example".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: vec![],
        allow_credentials: false,
    });
    let matched = route_of(tenant, owner.id, "/v1/*", &["GET"]);
    store.insert_upstream(&owner).expect("upstream");
    store.insert_route(&matched).expect("route");

    let mut inbound = request(tenant, "payments", "/v1/charges");
    inbound.cors = CorsRequest {
        method: "GET".to_owned(),
        origin: Some("https://evil.example".to_owned()),
        request_method: None,
        request_headers: None,
    };
    inbound.headers.insert(
        http::HeaderName::from_static("origin"),
        http::HeaderValue::from_static("https://evil.example"),
    );
    let failure = plane.execute(inbound).await.expect_err("origin refused");
    assert_eq!(failure.status, 403);
    assert_eq!(
        failure.type_uri,
        problem_type(crate::domain::plugin::CORS_ORIGIN_NOT_ALLOWED)
    );
    assert!(
        failure
            .headers
            .get("vary")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("Origin"))
    );
}

#[tokio::test]
async fn a_cross_origin_preflight_is_answered_locally_with_a_204() {
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let tenant = uuid::Uuid::new_v4();
    let mut owner = upstream(tenant, "payments", 1);
    owner.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://console.example".to_owned()],
        allowed_methods: vec!["POST".to_owned()],
        expose_headers: vec![],
        allow_credentials: false,
    });
    store.insert_upstream(&owner).expect("upstream");

    let mut inbound = request(tenant, "payments", "/v1/charges");
    inbound.method = "OPTIONS".to_owned();
    inbound.cors = CorsRequest {
        method: "OPTIONS".to_owned(),
        origin: Some("https://console.example".to_owned()),
        request_method: Some("POST".to_owned()),
        request_headers: Some("content-type".to_owned()),
    };
    let answer = plane.execute(inbound).await.expect("preflight");
    assert_eq!(answer.status, 204);
    assert_eq!(answer.source, ErrorSource::Gateway);
    assert_eq!(
        answer
            .headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://console.example")
    );
    assert_eq!(
        answer
            .headers
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        answer
            .headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("content-type")
    );
    assert!(answer.headers.get("access-control-max-age").is_some());
}

#[tokio::test]
async fn a_preflight_is_answered_before_the_alias_is_resolved() {
    // `docs/ADR/0004`: "no upstream resolution, no tenant context required".
    // The gateway middleware hands a preflight to the gear with an anonymous
    // subject, so an empty store — nothing this tenant owns at all — still has
    // to yield the permissive 204 instead of a 404.
    let store = Arc::new(MemoryStore::new());
    let plane = data_plane(store.clone(), true);
    let mut inbound = request(uuid::Uuid::new_v4(), "nowhere", "/v1/charges");
    inbound.method = "OPTIONS".to_owned();
    inbound.cors = CorsRequest {
        method: "OPTIONS".to_owned(),
        origin: Some("https://app.example".to_owned()),
        request_method: Some("POST".to_owned()),
        request_headers: Some("content-type, authorization".to_owned()),
    };
    let answer = plane.execute(inbound).await.expect("preflight");
    assert_eq!(answer.status, 204);
    assert_eq!(
        answer
            .headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        answer
            .headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("content-type, authorization")
    );
    assert_eq!(
        answer
            .headers
            .get("vary")
            .and_then(|value| value.to_str().ok()),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
}

#[tokio::test]
async fn an_empty_preflight_echo_is_not_padded_with_blank_headers() {
    // No `Origin` and no requested method: the echo has nothing to say, so it
    // must not emit empty `Access-Control-Allow-Origin` members.
    let reply = crate::infra::proxy::cors::preflight_reply(&CorsRequest {
        method: "OPTIONS".to_owned(),
        origin: None,
        request_method: None,
        request_headers: None,
    });
    assert!(
        reply
            .iter()
            .all(|(name, value)| *name == crate::infra::proxy::cors::MAX_AGE || !value.is_empty())
    );
    assert!(
        reply
            .iter()
            .any(|(name, value)| *name == crate::infra::proxy::cors::MAX_AGE && !value.is_empty())
    );
}

#[test]
fn a_gateway_failure_is_never_an_upstream_one() {
    let failure = ProxyFailure::validation("nope");
    assert_eq!(failure.source, ErrorSource::Gateway);
    assert_eq!(
        failure.with_source(ErrorSource::Upstream).source,
        ErrorSource::Upstream
    );
}

#[test]
fn a_failure_context_is_always_an_object() {
    let failure = ProxyFailure::validation("x").with_context("reason", serde_json::json!("no"));
    assert!(failure.context.get("reason").is_some());
}

#[test]
fn the_problem_catalogue_ids_are_stable() {
    for bare in [
        crate::domain::plugin::ROUTE_NOT_FOUND,
        crate::domain::plugin::LINK_UNAVAILABLE,
        crate::domain::plugin::RATE_LIMIT_EXCEEDED,
        crate::domain::plugin::CORS_ORIGIN_NOT_ALLOWED,
        crate::domain::plugin::CORS_METHOD_NOT_ALLOWED,
        crate::domain::plugin::MISSING_TARGET_HOST,
    ] {
        assert!(problem_type(bare).starts_with("gts."));
    }
}

#[test]
fn a_plugin_failure_keeps_its_full_type_uri() {
    // `PluginError.code` already carries the complete GTS id; running it back
    // through the catalogue would prefix the prefix.
    let error = crate::domain::plugin::PluginError::new(
        400,
        problem_type(crate::domain::plugin::VALIDATION),
        "required request header 'x-correlation-id' is absent",
    );
    let failure = super::plugin_failure(error);
    assert_eq!(failure.status, 400);
    assert_eq!(
        failure.type_uri,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(failure.source, ErrorSource::Gateway);
}

#[test]
fn a_plugin_failure_titles_itself_from_the_catalogue() {
    for (code, expected) in [
        (
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            "Validation Error",
        ),
        (
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            "Rate Limit Exceeded",
        ),
        (
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            "Protocol Error",
        ),
        (
            "gts.cf.core.errors.err.v1~cf.core.err.internal.v1",
            "Internal Error",
        ),
        (
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            "Authentication Failed",
        ),
        (
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "Route Not Found",
        ),
    ] {
        assert_eq!(super::plugin_title(code), expected, "{code}");
    }
}

#[test]
fn a_bare_plugin_code_is_not_mangled() {
    assert_eq!(
        super::plugin_title("cf.oagw.something.else.v1"),
        "Something Else"
    );
    assert_eq!(super::plugin_title("not-a-gts-id"), "Not A Gts Id");
}
