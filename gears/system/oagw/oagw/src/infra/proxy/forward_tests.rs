#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

//! The data plane (`DESIGN` §3.5): one `forward` call from the alias in to the
//! streamed answer out, over a real mock upstream.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use futures_util::StreamExt;

use super::forward::{ForwardOutcome, ForwardRequest, ProxyEngine};
use super::policy::ProxyPolicy;
use super::resolver::StaticHierarchy;
use super::ssrf::SsrfGuard;
use super::transport::UpstreamTransport;
use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::model::{
    CorsConfig, Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode,
    PluginRef, Protocol, RateLimitConfig, ServerConfig, Upstream,
};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
use crate::infra::plugin::registry::AuthPluginRegistry;
use crate::infra::plugin::secrets::StaticSecretResolver;
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0xA001);
const SUBJECT: uuid::Uuid = uuid::Uuid::from_u128(0x51);

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Http,
        host: "127.0.0.1".to_owned(),
        port,
    }
}

fn upstream(port: u16, alias: &str, extras: impl FnOnce(&mut Upstream)) -> Upstream {
    let mut row = Upstream {
        id: uuid::Uuid::from_u128(0x11),
        tenant_id: TENANT,
        alias: alias.to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![endpoint(port)],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    };
    extras(&mut row);
    row
}

fn route(upstream_id: uuid::Uuid, path: &str) -> crate::domain::model::Route {
    crate::domain::model::Route {
        id: uuid::Uuid::from_u128(0x21),
        tenant_id: TENANT,
        upstream_id,
        r#match: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get, HttpMethod::Post],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        priority: 0,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

fn policy() -> ProxyPolicy {
    // Loopback upstreams are plaintext, so the e2e posture of
    // `config/e2e-local.yaml` is the one these tests run under.
    ProxyPolicy::new(5, true)
}

async fn engine(upstream: Upstream, routes: &[crate::domain::model::Route]) -> Arc<ProxyEngine> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let route_repo = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    upstreams.insert(&upstream).await.unwrap();
    for route in routes {
        route_repo.insert(route).await.unwrap();
    }
    let plugin_repo = Arc::new(MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
        Arc::clone(&route_repo) as Arc<dyn crate::domain::repo::RouteRepository>,
    ));
    let _ = plugin_repo;
    let cache_config = TokenCacheConfig::default();
    let engine = ProxyEngine::new(
        upstreams as Arc<dyn UpstreamRepository>,
        route_repo as Arc<dyn crate::domain::repo::RouteRepository>,
        Arc::new(StaticHierarchy::new(Vec::new())),
        AuthPluginRegistry::with_builtins(
            Arc::new(StaticSecretResolver::default()),
            None,
            cache_config,
        ),
        SsrfGuard::disabled(),
        policy(),
        UpstreamTransport::new(&policy()).unwrap(),
    );
    Arc::new(engine)
}

/// An engine whose credential store serves `secrets`: the auth plugins resolve
/// their references against it.
async fn engine_with_secrets(
    upstream: Upstream,
    routes: &[crate::domain::model::Route],
    secrets: crate::infra::plugin::secrets::StaticSecretResolver,
) -> Arc<ProxyEngine> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let route_repo = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    upstreams.insert(&upstream).await.unwrap();
    for route in routes {
        route_repo.insert(route).await.unwrap();
    }
    let engine = ProxyEngine::new(
        upstreams as Arc<dyn UpstreamRepository>,
        route_repo as Arc<dyn crate::domain::repo::RouteRepository>,
        Arc::new(StaticHierarchy::new(Vec::new())),
        AuthPluginRegistry::with_builtins(Arc::new(secrets), None, TokenCacheConfig::default()),
        SsrfGuard::disabled(),
        policy(),
        UpstreamTransport::new(&policy()).unwrap(),
    );
    Arc::new(engine)
}

fn context(alias: &str, path: &str, method: &str, query: Vec<(String, String)>) -> ProxyContext {
    ProxyContext {
        alias: alias.to_owned(),
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        headers: BTreeMap::from([
            ("accept".to_owned(), "application/json".to_owned()),
            ("host".to_owned(), "gateway.example".to_owned()),
        ]),
        trace_id: Some("trace-1".to_owned()),
        tenant: TENANT,
        subject: SUBJECT,
    }
}

/// The error a failed `forward` returns, without requiring a `Debug` outcome.
fn failure(outcome: Result<ForwardOutcome, DomainError>) -> DomainError {
    match outcome {
        Ok(_) => panic!("the call must be refused before the upstream is reached"),
        Err(error) => error,
    }
}

/// The HTTP status the wire problem of `error` carries.
fn status_of(error: DomainError) -> u16 {
    crate::api::rest::error::OagwProblem::from(error).status()
}

fn request(context: ProxyContext) -> ForwardRequest {
    ForwardRequest {
        context,
        method: http::Method::GET,
        target_host: None,
        body: axum::body::Body::empty(),
        upgrade: false,
    }
}

// ── alias → upstream → route → upstream call ───────────────────────────────

#[tokio::test]
async fn a_request_reaches_its_upstream_with_the_route_path_and_the_caller_headers() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .header("accept", "application/json")
                .header("host", format!("127.0.0.1:{}", server.port()));
            then.status(200)
                .header("x-vendor", "vendor-1")
                .body("{\"models\":[]}");
        })
        .await;

    let row = upstream(server.port(), "api.openai.com", |_| {});
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(ForwardRequest {
            context: context("api.openai.com", "/v1/models", "GET", Vec::new()),
            ..request(context("api.openai.com", "/v1/models", "GET", Vec::new()))
        })
        .await
        .unwrap();

    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain request is answered with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-vendor")
            .and_then(|value| value.to_str().ok()),
        Some("vendor-1")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, &b"{\"models\":[]}"[..]);
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_route_not_found() {
    let server = httpmock::MockServer::start_async().await;
    let engine = engine(upstream(server.port(), "api.openai.com", |_| {}), &[]).await;

    let error = failure(
        engine
            .forward(request(context("other.vendor.com", "/", "GET", Vec::new())))
            .await,
    );
    assert!(
        matches!(error, DomainError::RouteNotFound { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 404);
}

#[tokio::test]
async fn a_request_no_route_serves_is_a_404_route_not_found() {
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |_| {});
    let engine = engine(row, &[]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    assert!(
        matches!(error, DomainError::RouteNotFound { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 404);
}

#[tokio::test]
async fn a_method_the_route_does_not_allow_is_not_matched() {
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |_| {});
    let mut route = route(row.id, "/v1");
    route.r#match.http.as_mut().unwrap().methods = vec![HttpMethod::Post];
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(ForwardRequest {
                context: context("api.openai.com", "/v1/models", "GET", Vec::new()),
                ..request(context("api.openai.com", "/v1/models", "GET", Vec::new()))
            })
            .await,
    );
    assert!(
        matches!(error, DomainError::RouteNotFound { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_post_body_is_forwarded_to_the_upstream() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/embeddings")
                .body("{\"input\":\"hi\"}");
            then.status(201).body("[0.1]");
        })
        .await;

    let row = upstream(server.port(), "api.openai.com", |_| {});
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let response = match engine
        .forward(ForwardRequest {
            context: context("api.openai.com", "/v1/embeddings", "POST", Vec::new()),
            method: http::Method::POST,
            body: axum::body::Body::from("{\"input\":\"hi\"}"),
            ..request(context(
                "api.openai.com",
                "/v1/embeddings",
                "POST",
                Vec::new(),
            ))
        })
        .await
        .unwrap()
    {
        ForwardOutcome::Response(response) => response,
        ForwardOutcome::Tunnel { .. } => panic!("a POST is not an upgrade"),
    };
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        &b"[0.1]"[..]
    );
}

#[tokio::test]
async fn a_query_the_route_allowlist_refuses_is_a_400() {
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |_| {});
    let mut route = route(row.id, "/v1");
    route.r#match.http.as_mut().unwrap().query_allowlist = vec!["api-version".to_owned()];
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                vec![("secret".to_owned(), "1".to_owned())],
            )))
            .await,
    );
    assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");
}

#[tokio::test]
async fn an_allowed_query_parameter_reaches_the_upstream() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .query_param("api-version", "2024-01");
            then.status(200).body("[]");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |_| {});
    let mut route = route(row.id, "/v1");
    route.r#match.http.as_mut().unwrap().query_allowlist = vec!["api-version".to_owned()];
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            vec![("api-version".to_owned(), "2024-01".to_owned())],
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn a_target_host_header_pins_the_endpoint_of_the_pool() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("eu");
        })
        .await;
    // Two endpoints of the pool share the loopback address, so both are
    // reachable; only the pinned one carries the alias the caller names.
    let row = upstream(server.port(), "vendor.com", |row| {
        row.server.endpoints = vec![endpoint(server.port()), endpoint(server.port())];
        row.server.endpoints[1].host = "localhost".to_owned();
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let request = ForwardRequest {
        target_host: Some("localhost".to_owned()),
        ..request(context("vendor.com", "/v1/models", "GET", Vec::new()))
    };
    let outcome = engine.forward(request).await.unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_503() {
    let row = upstream(1, "api.openai.com", |_| {});
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    assert!(
        matches!(error, DomainError::LinkUnavailable { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 503);
}

// ── streaming ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_streaming_answer_is_observable_before_the_upstream_finishes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        // Drain the request head before answering.
        let mut buffer = [0u8; 8192];
        drop(reader.read(&mut buffer).await);
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        writer.write_all(head.as_bytes()).await.unwrap();
        writer.write_all(b"data: one\n\n").await.unwrap();
        writer.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(writer.write_all(b"data: two\n\n").await);
        drop(writer.shutdown().await);
    });

    let row = upstream(port, "api.openai.com", |_| {});
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/stream",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("an SSE answer is a response, not a tunnel");
    };
    assert_eq!(response.status().as_u16(), 200);

    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(3), stream.next()).await;
    match first {
        Ok(Some(Ok(chunk))) => assert_eq!(chunk, &b"data: one\n\n"[..]),
        other => panic!("the first chunk must be readable before the stream ends: {other:?}"),
    }
    let second = tokio::time::timeout(Duration::from_secs(3), stream.next()).await;
    match second {
        Ok(Some(Ok(chunk))) => assert_eq!(chunk, &b"data: two\n\n"[..]),
        other => panic!("the rest of the stream is missing: {other:?}"),
    }
}

// ── rate limit ─────────────────────────────────────────────────────────────

fn limiter(rate: u64, capacity: u64, cost: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: crate::domain::model::SharingMode::default(),
        algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
        sustained: crate::domain::model::SustainedRate {
            rate,
            window: crate::domain::model::RateWindow::Minute,
        },
        burst: Some(crate::domain::model::Burst { capacity }),
        scope: crate::domain::model::RateScope::Tenant,
        strategy: crate::domain::model::RateStrategy::Reject,
        cost,
    }
}

#[tokio::test]
async fn an_exhausted_quota_is_a_429_with_retry_after() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        // Two tokens of capacity, two spent per call: the second call is the
        // first one the bucket cannot cover.
        row.rate_limit = Some(limiter(1, 2, 2));
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    let DomainError::RateLimitExceeded { retry_after, .. } = &error else {
        panic!("the third call must be refused, got {error:?}");
    };
    assert_eq!(*retry_after, Some(Duration::from_mins(2)));
    assert_eq!(status_of(error), 429);
    mock.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_route_limit_replaces_the_upstream_limit() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.rate_limit = Some(limiter(1, 1, 1));
    });
    let mut route = route(row.id, "/v1");
    route.rate_limit = Some(limiter(10, 100, 1));
    let engine = engine(row, &[route]).await;

    for _ in 0..5 {
        let outcome = engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await
            .unwrap();
        let ForwardOutcome::Response(response) = outcome else {
            panic!("a plain call answers with a response");
        };
        assert_eq!(
            response.status().as_u16(),
            200,
            "the generous route limit must win"
        );
    }
}

// ── CORS ───────────────────────────────────────────────────────────────────

fn cors() -> CorsConfig {
    CorsConfig {
        sharing: crate::domain::model::SharingMode::default(),
        enabled: true,
        allowed_origins: vec!["https://app.example".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-vendor".to_owned()],
        allow_credentials: false,
    }
}

#[tokio::test]
async fn a_cross_origin_call_from_an_allowed_origin_is_forwarded() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).header("x-vendor", "vendor-1").body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.cors = Some(cors());
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let mut request = request(context("api.openai.com", "/v1/models", "GET", Vec::new()));
    request
        .context
        .headers
        .insert("origin".to_owned(), "https://app.example".to_owned());
    let ForwardOutcome::Response(response) = engine.forward(request).await.unwrap() else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-expose-headers")
            .and_then(|value| value.to_str().ok()),
        Some("x-vendor")
    );
}

#[tokio::test]
async fn a_cross_origin_call_from_a_foreign_origin_is_refused() {
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.cors = Some(cors());
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let mut request = request(context("api.openai.com", "/v1/models", "GET", Vec::new()));
    request
        .context
        .headers
        .insert("origin".to_owned(), "https://evil.example".to_owned());
    let error = failure(engine.forward(request).await);
    assert!(
        matches!(error, DomainError::AccessDenied { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 403);
}

// ── plugins ────────────────────────────────────────────────────────────────

fn guard(required: &str) -> PluginRef {
    PluginRef::Binding {
        plugin_ref: REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        config: Some(serde_json::json!({ "required_request_headers": required })),
    }
}

fn transform() -> PluginRef {
    PluginRef::Id(REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned())
}

/// The plugin chain of one level of the request path.
fn chain(items: Vec<PluginRef>) -> crate::domain::model::PluginsConfig {
    crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::default(),
        items,
    }
}

/// `ADR`-0002: the whole guard phase runs before the whole transform phase, so
/// a guard judges the call as the caller sent it — the upstream stamps the id
/// the route requires, and the route guard must still refuse.
#[tokio::test]
async fn the_guards_run_before_the_transforms_of_the_same_call() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(chain(vec![transform()]));
    });
    let mut route = route(row.id, "/v1");
    route.plugins = Some(chain(vec![guard("x-request-id")]));
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    assert!(
        matches!(error, DomainError::AccessDenied { .. }),
        "the guard runs before the transform, so the id is still missing: {error:?}"
    );
    mock.assert_calls_async(0).await;
}

/// `ADR`-0002: the upstream chain runs before the route chain, so an upstream
/// guard refuses before a route guard is even consulted.
#[tokio::test]
async fn the_upstream_chain_runs_before_the_route_chain() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    // The upstream guard demands `x-api-key`; the route guard only demands a
    // header the caller does carry. The upstream refusal is the one reported.
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(chain(vec![guard("x-api-key")]));
    });
    let mut route = route(row.id, "/v1");
    route.plugins = Some(chain(vec![guard("x-tenant-id")]));
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    let DomainError::AccessDenied { detail } = &error else {
        panic!("the upstream guard must refuse, got {error:?}");
    };
    assert!(detail.contains("x-api-key"), "{detail}");
    mock.assert_calls_async(0).await;
}

#[tokio::test]
async fn a_guard_refusal_stops_the_request_before_the_upstream() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::default(),
            items: vec![guard("x-api-key")],
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    let DomainError::AccessDenied { detail } = &error else {
        panic!("a missing required header must be denied, got {error:?}");
    };
    assert!(detail.contains("x-api-key"), "{detail}");
    mock.assert_calls_async(0).await;
}

#[tokio::test]
async fn a_satisfied_guard_lets_the_request_through() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .header("x-api-key", "k");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::default(),
            items: vec![guard("x-api-key")],
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let mut request = request(context("api.openai.com", "/v1/models", "GET", Vec::new()));
    request
        .context
        .headers
        .insert("x-api-key".to_owned(), "k".to_owned());
    let ForwardOutcome::Response(response) = engine.forward(request).await.unwrap() else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn a_transform_makes_the_request_carry_a_request_id() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .header_exists("x-request-id");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::default(),
            items: vec![transform()],
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(_) = outcome else {
        panic!("a plain call answers with a response");
    };
    // The one call the upstream saw carried the stamped id.
    mock.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_plugin_reference_no_registry_serves_is_a_503() {
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::default(),
            items: vec![PluginRef::Id(format!(
                "gts.cf.core.oagw.guard_plugin.v1~{}",
                uuid::Uuid::from_u128(0x77)
            ))],
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    assert!(
        matches!(error, DomainError::PluginNotFound { .. }),
        "{error:?}"
    );
    assert_eq!(status_of(error), 503);
}

// ── auth ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_noop_auth_plugin_adds_nothing_but_the_call_is_proxied() {
    use crate::domain::model::AuthConfig;
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.auth = Some(AuthConfig {
            plugin_type: Some(crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned()),
            sharing: crate::domain::model::SharingMode::default(),
            config: None,
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let ForwardOutcome::Response(response) = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap()
    else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test]
async fn an_auth_plugin_the_registry_cannot_resolve_is_a_503() {
    use crate::domain::model::AuthConfig;
    let server = httpmock::MockServer::start_async().await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.auth = Some(AuthConfig {
            plugin_type: Some(uuid::Uuid::from_u128(0x88).to_string()),
            sharing: crate::domain::model::SharingMode::default(),
            config: None,
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    assert!(
        matches!(error, DomainError::PluginNotFound { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_admitted_request_reports_the_quota_of_its_bucket() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.rate_limit = Some(limiter(60, 60, 1));
    });
    let route = route(row.id, "/v1");
    let engine = engine(row, &[route]).await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    // `ADR`-0003, More Information: an admitted request still reports the
    // quota, so a caller can pace itself without being refused first.
    assert_eq!(response.headers().get("x-ratelimit-limit").unwrap(), "60");
    assert_eq!(
        response.headers().get("x-ratelimit-remaining").unwrap(),
        "59"
    );
    assert!(response.headers().contains_key("x-ratelimit-reset"));
}

const PARENT_TENANT: uuid::Uuid = uuid::Uuid::from_u128(0xB001);

#[tokio::test]
async fn an_enforced_ancestor_tightens_the_effective_limit() {
    let server = httpmock::MockServer::start_async().await;
    let _mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/models");
            then.status(200).body("ok");
        })
        .await;
    // `DESIGN` §"Hierarchical Configuration": `effective_rate =
    // min(selected_rate, route_rate, all_ancestor_enforced_rates)`. The parent
    // enforces 1 token per minute with a 2-token bucket, and the descendant
    // cannot outspend it by shadowing the alias.
    let mut parent = upstream(server.port(), "api.openai.com", |row| {
        let mut enforced = limiter(1, 2, 2);
        enforced.sharing = crate::domain::model::SharingMode::Enforce;
        row.rate_limit = Some(enforced);
    });
    parent.id = uuid::Uuid::from_u128(0x13);
    parent.tenant_id = PARENT_TENANT;
    let child = upstream(server.port(), "api.openai.com", |row| {
        row.rate_limit = Some(limiter(60, 60, 1));
    });
    let route = route(child.id, "/v1");
    let engine = engine_for_chain(
        &(child, route),
        &(parent, None),
        vec![(TENANT, PARENT_TENANT)],
    )
    .await;

    let first = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(first) = first else {
        panic!("a plain call answers with a response");
    };
    // The bucket the caller spends from is the ancestor's, not the 60 tokens
    // the descendant asked for.
    assert_eq!(first.headers().get("x-ratelimit-limit").unwrap(), "2");
    assert_eq!(
        first.headers().get("x-ratelimit-remaining").unwrap(),
        "1",
        "the enforced ancestor's two-token bucket is the one being spent"
    );

    // The descendant's own 60-token budget would have absorbed many more calls:
    // the second is the last one the ancestor's bucket covers.
    engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let error = failure(
        engine
            .forward(request(context(
                "api.openai.com",
                "/v1/models",
                "GET",
                Vec::new(),
            )))
            .await,
    );
    let DomainError::RateLimitExceeded { .. } = &error else {
        panic!("the enforced ancestor's limit must be reached, got {error:?}");
    };
}

/// An engine over a two-tenant chain: the child owns the routing definition of
/// the alias, the parent only constrains it.
async fn engine_for_chain(
    child: &(Upstream, crate::domain::model::Route),
    parent: &(Upstream, Option<crate::domain::model::Route>),
    edges: Vec<(uuid::Uuid, uuid::Uuid)>,
) -> Arc<ProxyEngine> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let route_repo = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    upstreams.insert(&child.0).await.unwrap();
    route_repo.insert(&child.1).await.unwrap();
    upstreams.insert(&parent.0).await.unwrap();
    if let Some(route) = &parent.1 {
        route_repo.insert(route).await.unwrap();
    }
    let engine = ProxyEngine::new(
        upstreams as Arc<dyn UpstreamRepository>,
        route_repo as Arc<dyn crate::domain::repo::RouteRepository>,
        Arc::new(StaticHierarchy::new(edges)),
        AuthPluginRegistry::with_builtins(
            Arc::new(StaticSecretResolver::default()),
            None,
            TokenCacheConfig::default(),
        ),
        SsrfGuard::disabled(),
        policy(),
        UpstreamTransport::new(&policy()).unwrap(),
    );
    Arc::new(engine)
}

#[tokio::test]
async fn a_plugin_produced_header_reaches_the_upstream_without_a_passthrough_rule() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .header("x-vendor-key", "vendor-token");
            then.status(200).body("ok");
        })
        .await;
    // `DESIGN` §"Headers Transformation" scopes `passthrough` to the caller's
    // headers: the caller's own credential is dropped, the one the auth plugin
    // produced is not.
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.headers = Some(crate::domain::model::HeadersConfig {
            request: Some(crate::domain::model::RequestHeaderRules {
                set: BTreeMap::new(),
                add: BTreeMap::new(),
                remove: Vec::new(),
                passthrough: Some(crate::domain::model::Passthrough::None),
                passthrough_allowlist: Vec::new(),
            }),
            response: None,
        });
        row.auth = Some(crate::domain::model::AuthConfig {
            plugin_type: Some(crate::domain::gts_helpers::API_KEY_AUTH_PLUGIN_ID.to_owned()),
            sharing: crate::domain::model::SharingMode::default(),
            config: Some(serde_json::json!({ "name": "x-vendor-key", "value_ref": "vendor-key" })),
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine_with_secrets(
        row,
        &[route],
        StaticSecretResolver::single("vendor-key", "vendor-token"),
    )
    .await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
    mock.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_query_parameter_a_plugin_adds_reaches_the_upstream() {
    let server = httpmock::MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .query_param("api-key", "vendor-token");
            then.status(200).body("ok");
        })
        .await;
    // `DESIGN` §"Transformation Rules": a plugin may write the request, and the
    // query it produced is the query the upstream sees.
    let row = upstream(server.port(), "api.openai.com", |row| {
        row.auth = Some(crate::domain::model::AuthConfig {
            plugin_type: Some(crate::domain::gts_helpers::API_KEY_AUTH_PLUGIN_ID.to_owned()),
            sharing: crate::domain::model::SharingMode::default(),
            config: Some(
                serde_json::json!({ "location": "query", "name": "api-key", "value_ref": "vendor-key" }),
            ),
        });
    });
    let route = route(row.id, "/v1");
    let engine = engine_with_secrets(
        row,
        &[route],
        StaticSecretResolver::single("vendor-key", "vendor-token"),
    )
    .await;

    let outcome = engine
        .forward(request(context(
            "api.openai.com",
            "/v1/models",
            "GET",
            Vec::new(),
        )))
        .await
        .unwrap();
    let ForwardOutcome::Response(response) = outcome else {
        panic!("a plain call answers with a response");
    };
    assert_eq!(response.status().as_u16(), 200);
    mock.assert_calls_async(1).await;
}
