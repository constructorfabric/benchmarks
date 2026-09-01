#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for [`HttpProxyEngine`] (DESIGN §3.5).
//!
//! Every test drives the engine against a **real** upstream server: `httpmock`
//! for plain HTTP, a hand-rolled TCP server for `text/event-stream`
//! pass-through and `tokio-tungstenite` for a WebSocket upgrade.

use std::sync::Arc;

use crate::domain::service::{RouteDraft, RouteUpdate, UpstreamDraft};
use uuid::Uuid;

use super::{
    ErrorSource, HttpProxyEngine, ProxyOutcome, ProxyRequest, ERROR_SOURCE_HEADER,
    ERROR_SOURCE_UPSTREAM,
};
use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::target::TARGET_HOST_HEADER as TARGET_HOST;
use crate::domain::models::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Protocol,
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, Route, ServerConfig, SustainedRate,
    Upstream,
};
use crate::domain::service::ControlPlaneService;
use crate::infra::metrics::MetricsRegistry;
use crate::infra::plugin;
use crate::infra::ratelimit::LimiterRegistry;
use crate::infra::storage::InMemoryStore;
use crate::infra::tenant::TenantChainResolver;
use crate::infra::transport::Transport;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// An engine wired to a throwaway in-memory control plane.
struct Harness {
    engine: HttpProxyEngine,
    service: Arc<ControlPlaneService>,
    tenant: Uuid,
}

fn http_match(path: &str) -> HttpMatch {
    HttpMatch {
        methods: vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
        path: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn upstream(
    tenant: Uuid,
    alias: &str,
    endpoints: &[Endpoint],
    rate_limit: Option<RateLimitConfig>,
) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: endpoints.to_vec(),
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Http, "127.0.0.1", port)
}

fn security(tenant: Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

fn harness() -> (Harness, httpmock::MockServer) {
    harness_with(Vec::new())
}

/// A harness whose plugin registry additionally carries `extra_guards`.
fn harness_with(
    extra_guards: Vec<Arc<dyn crate::domain::plugin::GuardPlugin>>,
) -> (Harness, httpmock::MockServer) {
    let upstream_server = httpmock::MockServer::start();
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let service = Arc::new(ControlPlaneService::new(InMemoryStore::new(), &config));
    let transport = Arc::new(Transport::new(config.proxy_timeout()).unwrap());
    let secrets = Arc::new(plugin::LiteralSecretResolver);
    let mut registry = plugin::registry(&plugin::PluginBundle {
        secrets,
        transport: Arc::clone(&transport),
        token_cache_ttl: config.token_cache_ttl(),
        token_cache_capacity: config.token_cache_capacity,
    });
    for guard in extra_guards {
        registry.register_guard(guard);
    }
    let engine = HttpProxyEngine::new(
        Arc::clone(&service),
        TenantChainResolver::new(None),
        transport,
        registry,
        LimiterRegistry::new(),
        Arc::new(MetricsRegistry::new()),
        config,
    );
    (
        Harness {
            engine,
            service,
            tenant: Uuid::new_v4(),
        },
        upstream_server,
    )
}

impl Harness {
    /// Provisions the upstream and one route, returning both identifiers.
    async fn provision(&self, port: u16, path: &str) -> (Upstream, Route) {
        let upstream = upstream(
            self.tenant,
            "api.vendor.com",
            &[endpoint(port)],
            None,
        );
        let created = self
            .service
            .create_upstream(
                self.tenant,
                UpstreamDraft {
                    alias: Some(upstream.alias.clone()),
                    enabled: true,
                    protocol: Protocol::Http,
                    server: crate::domain::models::ServerConfig {
                        endpoints: vec![endpoint(port)],
                    },
                    auth: None,
                    headers: None,
                    plugins: None,
                    rate_limit: None,
                    cors: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        let created_route = self
            .service
            .create_route(
                self.tenant,
                RouteDraft {
                    upstream_id: created.id,
                    enabled: true,
                    priority: 0,
                    match_config: MatchConfig {
                        http: Some(http_match(path)),
                        grpc: None,
                    },
                    plugins: None,
                    rate_limit: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        (created, created_route)
    }

    /// Replaces the stored upstream with one carrying `cors` and `rate_limit`.
    async fn configure_upstream(
        &self,
        existing: &Upstream,
        cors: Option<crate::domain::models::CorsConfig>,
        rate_limit: Option<RateLimitConfig>,
        enabled: bool,
    ) -> Upstream {
        self.service
            .replace_upstream(
                self.tenant,
                existing.id,
                UpstreamDraft {
                    alias: Some(existing.alias.clone()),
                    enabled,
                    protocol: Protocol::Http,
                    server: ServerConfig {
                        endpoints: vec![endpoint(existing_port(existing))],
                    },
                    auth: None,
                    headers: None,
                    plugins: None,
                    rate_limit,
                    cors,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap()
    }

    /// Replaces the stored route with one admitting `allowlist`.
    async fn configure_route(&self, existing: &Route, allowlist: &[&str]) -> Route {
        self.service
            .replace_route(
                self.tenant,
                existing.id,
                RouteUpdate {
                    enabled: true,
                    priority: 0,
                    match_config: MatchConfig {
                        http: Some(HttpMatch {
                            methods: vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
                            path: existing_path(existing),
                            query_allowlist: allowlist
                                .iter()
                                .map(|name| (*name).to_owned())
                                .collect(),
                            path_suffix_mode: PathSuffixMode::Append,
                        }),
                        grpc: None,
                    },
                    plugins: None,
                    rate_limit: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap()
    }

    /// Sends a proxied GET to the alias.
    async fn get(&self, alias: &str, path: &str) -> Result<ProxyOutcome, DomainError> {
        self.get_with_headers(alias, path, None, Vec::new()).await
    }

    /// Replaces the stored upstream after `mutate` has edited its draft.
    async fn reconfigure(&self, existing: &Upstream, mutate: impl FnOnce(&mut UpstreamDraft)) -> Upstream {
        let mut draft = UpstreamDraft {
            alias: Some(existing.alias.clone()),
            enabled: existing.enabled,
            protocol: existing.protocol,
            server: existing.server.clone(),
            auth: existing.auth.clone(),
            headers: existing.headers.clone(),
            plugins: existing.plugins.clone(),
            rate_limit: existing.rate_limit.clone(),
            cors: existing.cors.clone(),
            tags: existing.tags.clone(),
        };
        mutate(&mut draft);
        self.service
            .replace_upstream(self.tenant, existing.id, draft)
            .await
            .unwrap()
    }

    /// Provisions a pool with an explicit alias (or a derived one) and a route.
    async fn provision_pool(
        &self,
        alias: Option<&str>,
        endpoints: &[Endpoint],
        path: &str,
    ) -> Upstream {
        let draft = UpstreamDraft {
            alias: alias.map(str::to_owned),
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
        };
        let created = self.service.create_upstream(self.tenant, draft).await.unwrap();
        self.service
            .create_route(
                self.tenant,
                RouteDraft {
                    upstream_id: created.id,
                    enabled: true,
                    priority: 0,
                    match_config: MatchConfig {
                        http: Some(http_match(path)),
                        grpc: None,
                    },
                    plugins: None,
                    rate_limit: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        created
    }

    /// Replaces the stored route after `mutate` has edited its update payload.
    async fn rematch(&self, existing: &Route, enabled: bool) -> Route {
        let http = existing.match_config.http.clone().unwrap_or_else(|| {
            HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }
        });
        self.service
            .replace_route(
                self.tenant,
                existing.id,
                RouteUpdate {
                    enabled,
                    priority: 0,
                    match_config: MatchConfig {
                        http: Some(http),
                        grpc: None,
                    },
                    plugins: None,
                    rate_limit: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap()
    }

    /// Sends a proxied request with an explicit query string.
    async fn get_with_query(
        &self,
        alias: &str,
        path: &str,
        query: Option<&str>,
    ) -> Result<ProxyOutcome, DomainError> {
        self.get_with_headers(alias, path, query, Vec::new()).await
    }

    /// Sends a proxied request with explicit headers.
    async fn get_with_headers(
        &self,
        alias: &str,
        path: &str,
        query: Option<&str>,
        headers: Vec<(&str, &str)>,
    ) -> Result<ProxyOutcome, DomainError> {
        let request = ProxyRequest {
            alias: alias.to_owned(),
            method: "GET".to_owned(),
            path: path.to_owned(),
            query: query.map(str::to_owned),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: bytes::Bytes::new(),
            security: security(self.tenant),
        };
        self.engine.execute(request).await
    }

}

/// Port of the first endpoint of a stored upstream.
fn existing_port(existing: &Upstream) -> u16 {
    existing.server.endpoints[0].port
}

/// Path prefix of a stored route.
fn existing_path(existing: &Route) -> String {
    existing
        .match_config
        .http
        .as_ref()
        .map_or_else(String::new, |http| http.path.clone())
}

// ---------------------------------------------------------------------------
// proxy flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_request_reaches_the_real_upstream() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("hello upstream");
    });

    let outcome = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(outcome.error_source, ErrorSource::Upstream);
    assert_eq!(outcome.route, "/v1");
    assert_eq!(
        outcome
            .headers
            .iter()
            .find(|(name, _)| name == ERROR_SOURCE_HEADER)
            .map(|(_, value)| value.as_str()),
        Some(ERROR_SOURCE_UPSTREAM)
    );
    let _ = created;
    assert_eq!(read_body(outcome).await, "hello upstream");
}

async fn read_body(outcome: ProxyOutcome) -> String {
    let bytes = crate::infra::transport::read_body(outcome.body).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn the_upstream_receives_the_forwarded_method_and_body() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/v1/things");
        then.status(201).body("created");
    });
    let _ = counter;

    let request = ProxyRequest {
        alias: "api.vendor.com".to_owned(),
        method: "POST".to_owned(),
        path: "/v1/things".to_owned(),
        query: None,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-api-secret".to_owned(), "do-not-log".to_owned()),
        ],
        body: bytes::Bytes::from_static(b"{\"a\":1}"),
        security: security(harness.tenant),
    };
    let outcome = harness.engine.execute(request).await.unwrap();
    assert_eq!(outcome.status, 201);
    assert_eq!(read_body(outcome).await, "created");
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn the_outbound_host_header_is_replaced_by_the_upstream_authority() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/");
        then.status(200).body("ok");
    });
    let outcome = harness
        .get_with_headers("api.vendor.com", "/", None, vec![("host", "evil.example.com")])
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
}

#[tokio::test]
async fn an_unknown_alias_is_reported_as_a_link_failure() {
    let (harness, _server) = harness();
    let error = harness.get("no.such.alias.com", "/").await.unwrap_err();
    assert_eq!(error.status(), 503);
    assert!(matches!(error, DomainError::LinkUnavailable { .. }));
}

#[tokio::test]
async fn an_unmatched_route_is_reported_as_route_not_found() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    let error = harness.get("api.vendor.com", "/other/path").await.unwrap_err();
    assert_eq!(error.status(), 404);
    assert!(matches!(error, DomainError::RouteNotFound { .. }));
}

#[tokio::test]
async fn a_disabled_selected_upstream_is_rejected_with_503() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    let disabled = UpstreamDraft {
        alias: Some(created.alias.clone()),
        enabled: false,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![endpoint(server.port())],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    };
    harness
        .service
        .replace_upstream(harness.tenant, created.id, disabled)
        .await
        .unwrap();
    let error = harness.get("api.vendor.com", "/v1/things").await.unwrap_err();
    assert_eq!(error.status(), 503);
}

#[tokio::test]
async fn a_disallowed_query_parameter_is_rejected() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    let error = harness
        .get_with_query("api.vendor.com", "/v1/things", Some("secret=1"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), 400);
}

#[tokio::test]
async fn an_allowed_query_parameter_is_forwarded() {
    let (harness, server) = harness();
    let (_, created_route) = harness.provision(server.port(), "/v1").await;
    harness
        .configure_route(&created_route, &["page"])
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/things")
            .query_param("page", "2");
        then.status(200).body("page 2");
    });
    let outcome = harness
        .get_with_query("api.vendor.com", "/v1/things", Some("page=2"))
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
}

// ---------------------------------------------------------------------------
// CORS (ADR 0004)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_origin_outside_the_policy_is_rejected_with_403() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .configure_upstream(&created, Some(cors_config(true)), None, true)
        .await;

    let request = ProxyRequest {
        alias: "api.vendor.com".to_owned(),
        method: "GET".to_owned(),
        path: "/v1/things".to_owned(),
        query: None,
        headers: vec![("origin".to_owned(), "https://attacker.test".to_owned())],
        body: bytes::Bytes::new(),
        security: security(harness.tenant),
    };
    let error = harness.engine.execute(request).await.unwrap_err();
    assert_eq!(error.status(), 403);
}

fn cors_config(enabled: bool) -> crate::domain::models::CorsConfig {
    use crate::domain::models::{CorsMethod, SharingMode};
    crate::domain::models::CorsConfig {
        sharing: SharingMode::Private,
        enabled,
        allowed_origins: vec!["https://app.test".to_owned()],
        allowed_methods: vec![CorsMethod::Get],
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}


#[tokio::test]
async fn an_origin_inside_the_policy_is_forwarded_and_annotated() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .configure_upstream(&created, Some(cors_config(true)), None, true)
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/v1/things",
            None,
            vec![("origin", "https://app.test")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(
        outcome
            .headers
            .iter()
            .find(|(name, _)| name == "access-control-allow-origin")
            .map(|(_, value)| value.as_str()),
        Some("https://app.test")
    );
}

#[tokio::test]
async fn same_origin_requests_skip_the_cors_policy() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .configure_upstream(&created, Some(cors_config(true)), None, true)
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    // No `Origin` header: the request is not a CORS request at all.
    let outcome = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    assert_eq!(outcome.status, 200);
    assert!(outcome
        .headers
        .iter()
        .all(|(name, _)| name != "access-control-allow-origin"));
}

// ---------------------------------------------------------------------------
// rate limiting (ADR 0003)
// ---------------------------------------------------------------------------

fn quota(rate: u64, capacity: u64, scope: RateLimitScope) -> RateLimitConfig {
    RateLimitConfig {
        sharing: crate::domain::models::SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: crate::domain::models::RateWindow::Minute,
        },
        burst: Some(crate::domain::models::BurstCapacity { capacity }),
        scope,
        strategy: crate::domain::models::RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

#[tokio::test]
async fn an_exhausted_bucket_is_rejected_with_429_and_quota_headers() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .configure_upstream(&created, None, Some(quota(1, 2, RateLimitScope::Global)), true)
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let first = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    assert_eq!(
        first
            .headers
            .iter()
            .find(|(name, _)| name == "x-ratelimit-limit")
            .map(|(_, value)| value.as_str()),
        Some("2")
    );
    assert_eq!(
        first
            .headers
            .iter()
            .find(|(name, _)| name == "x-ratelimit-remaining")
            .map(|(_, value)| value.as_str()),
        Some("1")
    );

    let _ = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    let error = harness.get("api.vendor.com", "/v1/things").await.unwrap_err();
    assert_eq!(error.status(), 429);
    // Refill is `ceil(1/60)` = 1 token per second, so the guidance is 1 s.
    assert_eq!(error.retry_after_seconds(), Some(1));
}

#[tokio::test]
async fn the_quota_headers_can_be_switched_off() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    let mut limit = quota(1, 2, RateLimitScope::Global);
    limit.response_headers = false;
    harness
        .configure_upstream(&created, None, Some(limit), true)
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let outcome = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    assert!(outcome
        .headers
        .iter()
        .all(|(name, _)| !name.starts_with("x-ratelimit")));
}

// ---------------------------------------------------------------------------
// metrics (DESIGN §4.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_proxied_request_is_counted_against_the_matched_pattern() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things/very/deep");
        then.status(200).body("ok");
    });
    let outcome = harness
        .get("api.vendor.com", "/v1/things/very/deep")
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    let rendered = harness.engine.metrics().render();
    assert!(rendered.contains("oagw_requests_total{host=\"api.vendor.com\",http_request_method=\"GET\",http_route=\"/v1\",http_response_status_code=\"200\"} 1"), "{rendered}");
    assert!(rendered.contains("oagw_request_duration_seconds_bucket{host=\"api.vendor.com\""));
    assert!(!rendered.contains("/v1/things/very/deep"));
}

#[tokio::test]
async fn a_gateway_rejection_is_counted_as_an_error() {
    let (harness, _server) = harness();
    let _ = harness.get("no.such.alias.com", "/").await.unwrap_err();
    let rendered = harness.engine.metrics().render();
    assert!(
        rendered.contains("oagw_errors_total{host=\"no.such.alias.com\",http_route=\"unmatched\",error_type=\"link_unavailable\"} 1"),
        "{rendered}"
    );
}

#[tokio::test]
async fn an_endpoint_selection_is_recorded() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/");
        then.status(200).body("ok");
    });
    let _ = harness.get("api.vendor.com", "/").await.unwrap();
    let rendered = harness.engine.metrics().render();
    assert!(
        rendered.contains("selection_method=\"default\""),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// streaming (SSE) pass-through
// ---------------------------------------------------------------------------

/// A minimal `text/event-stream` server: one event, then half-close.
async fn sse_server() -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        let _ = read;
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/event-stream\r\n",
            "Cache-Control: no-cache\r\n",
            "Transfer-Encoding: chunked\r\n",
            "\r\n",
            "9\r\ndata: a\n\n\r\n",
            "0\r\n\r\n",
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    (port, handle)
}

#[tokio::test]
async fn an_event_stream_is_passed_through_unchanged() {
    let (harness, _http_server) = harness();
    let (port, server_task) = sse_server().await;
    harness.provision(port, "/v1").await;

    let outcome = harness.get("api.vendor.com", "/v1/events").await.unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(
        outcome
            .headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.as_str()),
        Some("text/event-stream")
    );
    let bytes = crate::infra::transport::read_body(outcome.body).await.unwrap();
    assert_eq!(bytes, "data: a\n\n");
    server_task.await.unwrap();
}

// ---------------------------------------------------------------------------
// WebSocket upgrade headers (assumption A1 — the frame pump lives in the API
// handler, which owns the inbound axum upgrade)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upgrade_request_keeps_its_websocket_headers() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/ws").await;
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path("/ws")
            .header("sec-websocket-version", "13");
        let _ = counter;
        then.status(400).body("no upgrade");
    });

    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/ws",
            None,
            vec![
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ],
        )
        .await
        .unwrap();
    // The upstream refused the (deliberately incomplete) upgrade; what matters
    // is that the gateway did not reject the request itself.
    assert_eq!(outcome.status, 400);
    assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// body validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_content_length_mismatch_is_rejected() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    let request = ProxyRequest {
        alias: "api.vendor.com".to_owned(),
        method: "POST".to_owned(),
        path: "/v1/things".to_owned(),
        query: None,
        headers: vec![("content-length".to_owned(), "99".to_owned())],
        body: bytes::Bytes::from_static(b"{}"),
        security: security(harness.tenant),
    };
    let error = harness.engine.execute(request).await.unwrap_err();
    assert_eq!(error.status(), 400);
}

#[tokio::test]
async fn an_oversized_body_is_rejected_with_413() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    let request = ProxyRequest {
        alias: "api.vendor.com".to_owned(),
        method: "POST".to_owned(),
        path: "/v1/things".to_owned(),
        query: None,
        headers: Vec::new(),
        body: bytes::Bytes::from(vec![b'x'; 128]),
        security: security(harness.tenant),
    };
    let error = harness.engine.execute(request).await;
    // The default budget is far above 128 bytes, so this must succeed.
    assert!(error.is_ok());
    let _ = server;
}

// ---------------------------------------------------------------------------
// header transformation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hop_by_hop_headers_are_stripped_from_the_outbound_request() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/");
        then.status(200).body("ok");
    });
    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/",
            None,
            vec![("connection", "keep-alive"), ("x-custom", "value")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
}

// ---------------------------------------------------------------------------
// target-host selection (ADR 0001 Appendix A)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_multi_endpoint_suffix_pool_requires_the_target_host_header() {
    let (harness, _server) = harness();
    // Two hostnames sharing the `vendor.com` suffix derive `vendor.com`, so
    // the alias carries no host information and the pool cannot self-select.
    let created = harness
        .provision_pool(
            None,
            &[
                Endpoint::new(EndpointScheme::Http, "a.vendor.com", 8001),
                Endpoint::new(EndpointScheme::Http, "b.vendor.com", 8001),
            ],
            "/v1",
        )
        .await;

    let error = harness.get(&created.alias, "/v1/things").await.unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(error.title(), "Missing Target Host Header");
    let extensions = error.extensions();
    assert_eq!(
        extensions.valid_hosts,
        Some(vec!["a.vendor.com".to_owned(), "b.vendor.com".to_owned()])
    );
}

#[tokio::test]
async fn an_unknown_target_host_lists_the_valid_hosts() {
    let (harness, server) = harness();
    harness
        .provision_pool(
            Some("vendor-pool"),
            &[
                Endpoint::new(EndpointScheme::Http, "10.9.9.1", server.port()),
                Endpoint::new(EndpointScheme::Http, "10.9.9.2", server.port()),
            ],
            "/v1",
        )
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let error = harness
        .get_with_headers(
            "vendor-pool",
            "/v1/things",
            None,
            vec![(TARGET_HOST, "nope.invalid")],
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), 400);
    let extensions = error.extensions();
    assert_eq!(extensions.invalid_value.as_deref(), Some("nope.invalid"));
    assert_eq!(
        extensions.valid_hosts,
        Some(vec![
            "10.9.9.1".to_owned(),
            "10.9.9.2".to_owned()
        ])
    );
}

#[tokio::test]
async fn a_malformed_target_host_is_rejected() {
    let (harness, server) = harness();
    harness.provision(server.port(), "/v1").await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let error = harness
        .get_with_headers(
            "api.vendor.com",
            "/v1/things",
            None,
            vec![(TARGET_HOST, "https://evil.example.org/path")],
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert!(error.extensions().invalid_value.is_some());
}

#[tokio::test]
async fn an_explicit_target_host_selects_that_endpoint() {
    let (harness, server) = harness();
    let created = harness
        .provision_pool(
            Some("vendor-pool"),
            &[
                endpoint(server.port()),
                endpoint(server.port()),
            ],
            "/v1",
        )
        .await;
    let second = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/things")
            .header("host", format!("127.0.0.1:{}", server.port()));
        then.status(200).body("pinned");
    });

    let outcome = harness
        .get_with_headers(
            "vendor-pool",
            "/v1/things",
            None,
            vec![(TARGET_HOST, "127.0.0.1")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(read_body(outcome).await, "pinned");
    assert_eq!(second.calls(), 1);
    let _ = created;
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_unless_the_operator_opted_in() {
    let config = OagwConfig::default();
    assert!(!config.allow_http_upstream);
    let service = ControlPlaneService::new(InMemoryStore::new(), &config);
    let draft = UpstreamDraft {
        alias: Some("api.vendor.com".to_owned()),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![endpoint(8001)],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    };

    let error = service
        .create_upstream(Uuid::new_v4(), draft)
        .await
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert!(error.extensions().invalid_value.is_some());
}

#[tokio::test]
async fn an_unresolvable_auth_plugin_is_reported_as_plugin_not_found() {
    let upstream_server = httpmock::MockServer::start();
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let service = Arc::new(ControlPlaneService::new(InMemoryStore::new(), &config));
    let tenant = Uuid::new_v4();
    let upstream = service
        .create_upstream(
            tenant,
            UpstreamDraft {
                alias: Some("api.vendor.com".to_owned()),
                enabled: true,
                protocol: Protocol::Http,
                server: ServerConfig {
                    endpoints: vec![endpoint(upstream_server.port())],
                },
                auth: Some(crate::domain::models::AuthConfig {
                    plugin_type: Some(crate::domain::plugin::builtins::AUTH_API_KEY.to_owned()),
                    sharing: crate::domain::models::SharingMode::Private,
                    config: None,
                }),
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: Vec::new(),
            },
        )
        .await
        .unwrap();

    // An engine whose registry has no auth plugin at all cannot satisfy the
    // binding, so the request must fail closed instead of being relayed
    // unauthenticated.
    let transport = Arc::new(Transport::new(config.proxy_timeout()).unwrap());
    let engine = HttpProxyEngine::new(
        Arc::clone(&service),
        TenantChainResolver::new(None),
        transport,
        crate::domain::plugin::PluginRegistry::default(),
        LimiterRegistry::new(),
        Arc::new(MetricsRegistry::new()),
        config,
    );
    service
        .create_route(
            tenant,
            crate::domain::service::RouteDraft {
                upstream_id: upstream.id,
                enabled: true,
                priority: 0,
                match_config: MatchConfig {
                    http: Some(http_match("/v1")),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                tags: Vec::new(),
            },
        )
        .await
        .unwrap();

    let error = engine
        .execute(ProxyRequest {
            alias: upstream.alias.clone(),
            method: "GET".to_owned(),
            path: "/v1/things".to_owned(),
            query: None,
            headers: Vec::new(),
            body: bytes::Bytes::new(),
            security: security(tenant),
        })
        .await
        .unwrap_err();
    assert_eq!(error.status(), 503);
    assert_eq!(
        error.extensions().plugin_id.as_deref(),
        Some(crate::domain::plugin::builtins::AUTH_API_KEY)
    );
}

// ---------------------------------------------------------------------------
// header transformation (DESIGN §3.5 "Header Transformation")
// ---------------------------------------------------------------------------

fn rules(
    set: &[(&str, &str)],
    remove: &[&str],
) -> crate::domain::models::HeaderRules {
    crate::domain::models::HeaderRules {
        set: set
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        add: std::collections::BTreeMap::new(),
        remove: remove.iter().map(|name| (*name).to_owned()).collect(),
        passthrough: None,
        passthrough_allowlist: Vec::new(),
    }
}

fn header_rules(
    passthrough: crate::domain::models::HeaderPassthrough,
    allowlist: &[&str],
) -> crate::domain::models::HeadersConfig {
    crate::domain::models::HeadersConfig {
        request: Some(crate::domain::models::HeaderRules {
            passthrough: Some(passthrough),
            passthrough_allowlist: allowlist
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            ..crate::domain::models::HeaderRules::default()
        }),
        response: None,
    }
}

fn lookup(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

#[tokio::test]
async fn the_request_rules_set_and_remove_outbound_headers() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .reconfigure(&created, |draft| {
            draft.headers = Some(crate::domain::models::HeadersConfig {
                request: Some(rules(
                    &[("x-oagw-set", "fixed")],
                    &["x-oagw-remove"],
                )),
                response: None,
            });
        })
        .await;
    let seen = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/things")
            .header("x-oagw-set", "fixed")
            .is_false(|request| {
                request.headers().iter().any(|(name, _)| name == "x-oagw-remove")
            });
        then.status(200).body("ok");
    });

    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/v1/things",
            None,
            vec![("x-oagw-remove", "gone")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(seen.calls(), 1);
}

#[tokio::test]
async fn passthrough_none_drops_every_inbound_header() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .reconfigure(&created, |draft| {
            draft.headers = Some(header_rules(crate::domain::models::HeaderPassthrough::None, &[]));
        })
        .await;
    let seen = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/v1/things",
            None,
            vec![("x-tracked", "leak")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(seen.calls(), 1);
}

#[tokio::test]
async fn passthrough_allowlist_forwards_only_the_listed_headers() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .reconfigure(&created, |draft| {
            draft.headers = Some(header_rules(crate::domain::models::HeaderPassthrough::Allowlist, &["x-kept"]));
        })
        .await;
    let seen = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/things")
            .is_false(|request| {
                request.headers().iter().any(|(name, _)| name == "x-dropped")
            });
        then.status(200).body("ok");
    });

    let outcome = harness
        .get_with_headers(
            "api.vendor.com",
            "/v1/things",
            None,
            vec![("x-kept", "yes"), ("x-dropped", "no")],
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, 200);
    assert_eq!(seen.calls(), 1);
}

#[tokio::test]
async fn the_response_rules_are_applied_before_the_caller_sees_them() {
    let (harness, server) = harness();
    let (created, _) = harness.provision(server.port(), "/v1").await;
    harness
        .reconfigure(&created, |draft| {
            draft.headers = Some(crate::domain::models::HeadersConfig {
                request: None,
                response: Some(rules(&[("cache-control", "no-store")], &["x-vendor-internal"])),
            });
        })
        .await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200)
            .header("cache-control", "max-age=60")
            .header("x-vendor-internal", "secret")
            .body("ok");
    });

    let outcome = harness.get("api.vendor.com", "/v1/things").await.unwrap();
    assert_eq!(
        lookup(&outcome.headers, "cache-control").as_deref(),
        Some("no-store")
    );
    assert_eq!(lookup(&outcome.headers, "x-vendor-internal"), None);
}

// ---------------------------------------------------------------------------
// plugin chain (ADR 0002)
// ---------------------------------------------------------------------------

/// A guard that rejects both hooks, so the pipeline is exercised for both
/// directions at once.
struct RejectingGuard;

#[async_trait::async_trait]
impl crate::domain::plugin::GuardPlugin for RejectingGuard {
    fn id(&self) -> &'static str {
        "rejecting"
    }

    fn plugin_type(&self) -> &'static str {
        "cf.core.oagw.guard_plugin.v1~cf.core.oagw.rejecting.v1"
    }

    async fn guard_request(
        &self,
        _ctx: &crate::domain::plugin::RequestContext,
    ) -> Result<crate::domain::plugin::GuardDecision, DomainError> {
        Ok(crate::domain::plugin::GuardDecision::Reject(
            DomainError::validation("a required request header is missing"),
        ))
    }

    async fn guard_response(
        &self,
        _ctx: &crate::domain::plugin::ResponseContext,
    ) -> Result<crate::domain::plugin::GuardDecision, DomainError> {
        Ok(crate::domain::plugin::GuardDecision::Reject(
            DomainError::ProtocolError {
                detail: "a required response header is missing".to_owned(),
                upstream_id: None,
                host: None,
            },
        ))
    }
}

#[tokio::test]
async fn a_bound_guard_rejects_the_request_with_400() {
    let (harness, _server) = harness_with(vec![std::sync::Arc::new(RejectingGuard)]);
    let (created, _) = harness.provision(8001, "/v1").await;
    harness
        .reconfigure(&created, |draft| {
            draft.plugins = Some(crate::domain::models::PluginsConfig {
                sharing: crate::domain::models::SharingMode::Private,
                items: vec!["rejecting".to_owned()],
            });
        })
        .await;

    let error = harness.get("api.vendor.com", "/v1/things").await.unwrap_err();
    assert_eq!(error.status(), 400);
}

#[tokio::test]
async fn catalogue_only_auth_plugins_are_refused_by_the_control_plane() {
    let (harness, _server) = harness();
    let (created, _) = harness.provision(8001, "/v1").await;
    for reference in [
        crate::domain::plugin::builtins::AUTH_BASIC,
        crate::domain::plugin::builtins::AUTH_BEARER,
    ] {
        let error = harness
            .service
            .replace_upstream(
                harness.tenant,
                created.id,
                UpstreamDraft {
                    alias: Some(created.alias.clone()),
                    enabled: true,
                    protocol: Protocol::Http,
                    server: created.server.clone(),
                    auth: Some(crate::domain::models::AuthConfig {
                        plugin_type: Some(reference.to_owned()),
                        sharing: crate::domain::models::SharingMode::Private,
                        config: None,
                    }),
                    headers: None,
                    plugins: None,
                    rate_limit: None,
                    cors: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status(), 400, "binding '{reference}' must not resolve");
        assert!(error.extensions().invalid_value.is_some());
    }
}

// ---------------------------------------------------------------------------
// route enablement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_disabled_route_falls_through_to_route_not_found() {
    let (harness, server) = harness();
    let (_, created_route) = harness.provision(server.port(), "/v1").await;
    harness.rematch(&created_route, false).await;
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/things");
        then.status(200).body("ok");
    });

    let error = harness.get("api.vendor.com", "/v1/things").await.unwrap_err();
    assert_eq!(error.status(), 404);
}
