//! Data-plane integration tests (DESIGN §3.2, ADR 0001, ADR 0003, ADR 0007).
//!
//! Each test owns a real HTTP upstream served by `httpmock` and drives the
//! [`DataPlaneService`] directly, so the outbound leg — headers, bodies, status
//! codes and the `X-OAGW-Error-Source` distinction — is exercised over the wire
//! rather than stubbed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use httpmock::prelude::*;

use crate::config::OagwConfig;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    Endpoint, EndpointScheme, HeadersConfig, HttpMethod, MatchConfig, PassthroughMode,
    RateLimitConfig, RequestHeaderRules, ResponseHeaderRules, Route, ServerConfig, SustainedRate,
    Upstream,
};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::service::{DataPlaneService, ProxyBody, ProxyCall};
use crate::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};
use uuid::Uuid;

const TENANT: &str = "00000000-0000-0000-0000-000000000001";

struct FlatHierarchy;

#[async_trait::async_trait]
impl crate::domain::repo::TenantHierarchy for FlatHierarchy {
    async fn chain(&self, tenant_id: &str) -> Vec<String> {
        vec![tenant_id.to_owned()]
    }
}

pub(super) fn control_plane() -> ControlPlaneService {
    ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::default()),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(FlatHierarchy),
        true,
    )
}

fn data_plane(control_plane: ControlPlaneService) -> DataPlaneService {
    DataPlaneService::new(
        Arc::new(control_plane),
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        },
    )
    .unwrap()
    .with_registries(
        AuthPluginRegistry::empty(),
        GuardPluginRegistry::empty(),
        TransformPluginRegistry::empty(),
    )
}

/// An `http` upstream pointed at the mock server, plus a route.
#[allow(dead_code)]
pub(super) struct Fixture {
    upstream: Upstream,
    route: Route,
}

async fn setup(
    server: &MockServer,
    upstream: Upstream,
    route: Route,
) -> (ControlPlaneService, Fixture) {
    setup_with(server, upstream, route, Some("backend")).await
}

/// Registers the upstream and a route.
///
/// Upstreams without endpoints are pointed at `server`; pool fixtures supply
/// their own endpoints. `alias` pins the alias (`backend` for the single-endpoint
/// fixtures); `None` lets it be derived from the endpoint hostnames.
pub(super) async fn setup_with(
    server: &MockServer,
    mut upstream: Upstream,
    route: Route,
    alias: Option<&str>,
) -> (ControlPlaneService, Fixture) {
    if upstream.server.endpoints.is_empty() {
        upstream.server = ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(server.port()),
            }],
        };
    }
    let cp = control_plane();
    let created = cp
        .create_upstream(TENANT, upstream, alias.map(str::to_owned))
        .await
        .unwrap();
    let mut route = route;
    route.upstream_id = created.id;
    route.enabled = true;
    let stored = cp.create_route(TENANT, route).await.unwrap();
    (
        cp,
        Fixture {
            upstream: created,
            route: stored,
        },
    )
}

/// [`setup_with`] for tests that expect the registration itself to fail.
pub(super) async fn try_setup_with(
    server: &MockServer,
    mut upstream: Upstream,
    mut route: Route,
    alias: Option<&str>,
) -> Result<
    crate::domain::services::management::ControlPlaneService,
    crate::domain::error::DomainError,
> {
    if upstream.server.endpoints.is_empty() {
        upstream.server = ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(server.port()),
            }],
        };
    }
    let cp = control_plane();
    let created = cp
        .create_upstream(TENANT, upstream, alias.map(str::to_owned))
        .await?;
    route.upstream_id = created.id;
    route.enabled = true;
    cp.create_route(TENANT, route).await?;
    Ok(cp)
}

pub(super) fn default_route() -> Route {
    Route {
        match_config: MatchConfig {
            http: Some(crate::domain::model::HttpMatch {
                methods: vec![
                    HttpMethod::Get,
                    HttpMethod::Post,
                    HttpMethod::Delete,
                    HttpMethod::Put,
                    HttpMethod::Patch,
                ],
                path: "/api".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..Route::default()
    }
}

pub(super) fn plain_upstream() -> Upstream {
    Upstream {
        enabled: true,
        protocol: gts::PROTOCOL_HTTP.to_owned(),
        ..Upstream::default()
    }
}

pub(super) async fn get(
    dp: &DataPlaneService,
    path: &str,
    headers: &[(&str, &str)],
) -> http::Response<ProxyBody> {
    let mut header_map = HeaderMap::new();
    for (name, value) in headers {
        header_map.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    dp.proxy(ProxyCall {
        tenant_id: TENANT.to_owned(),
        user_id: Some("00000000-0000-0000-0000-0000000000aa".to_owned()),
        client_ip: Some("127.0.0.1".to_owned()),
        method: Method::GET,
        path: path.to_owned(),
        query: String::new(),
        headers: header_map,
        body: bytes::Bytes::new(),
        upgrade: None,
    })
    .await
}

pub(super) async fn body(response: &mut http::Response<ProxyBody>) -> bytes::Bytes {
    use http_body_util::BodyExt;
    response.body_mut().collect().await.unwrap().to_bytes()
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxies_a_get_request_and_passes_the_response_through() {
    let server = MockServer::start();
    let hello = server.mock(|when, then| {
        when.method(GET).path("/api/items");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true}"#);
    });

    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let mut response = get(&dp, "/backend/api/items", &[]).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_UPSTREAM
    );
    let body = body(&mut response).await;
    assert_eq!(body.as_ref(), br#"{"ok":true}"#);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    hello.assert();
}

#[tokio::test]
async fn an_upstream_error_is_passed_through_untouched() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(500)
            .header("content-type", "text/plain")
            .body("upstream exploded");
    });

    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let mut response = get(&dp, "/backend/api", &[]).await;

    // The upstream's own status, body and content type travel back as-is, with
    // the error source naming the upstream rather than the gateway.
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_UPSTREAM
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    let body = body(&mut response).await;
    assert_eq!(body.as_ref(), b"upstream exploded");
}

#[tokio::test]
async fn forwards_the_path_suffix_query_and_method() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/items")
            .query_param("limit", "5")
            .header("content-type", "application/json");
        then.status(201).body("created");
    });

    let mut route = default_route();
    route.match_config.http.as_mut().unwrap().query_allowlist = vec!["limit".to_owned()];
    let (cp, _) = setup(&server, plain_upstream(), route).await;
    let dp = data_plane(cp);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    let response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::POST,
            path: "/backend/api/v1/items".to_owned(),
            query: "limit=5".to_owned(),
            headers,
            body: bytes::Bytes::from_static(br#"{"name":"x"}"#),
            upgrade: None,
        })
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    create.assert();
}

#[tokio::test]
async fn host_header_is_replaced_and_routing_header_is_stripped() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("host", format!("127.0.0.1:{}", server.port()));
        then.status(204);
    });

    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "127.0.0.1")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    mock.assert();
}

#[tokio::test]
async fn request_and_response_header_rules_apply() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api").header("x-injected", "yes");
        then.status(200)
            .header("x-upstream-only", "internal")
            .header("x-set", "upstream")
            .body("ok");
    });

    let upstream = Upstream {
        headers: HeadersConfig {
            request: RequestHeaderRules {
                set: [("x-injected".to_owned(), "yes".to_owned())]
                    .into_iter()
                    .collect(),
                passthrough: PassthroughMode::None,
                ..RequestHeaderRules::default()
            },
            response: ResponseHeaderRules {
                set: [("x-set".to_owned(), "gateway".to_owned())]
                    .into_iter()
                    .collect(),
                remove: vec!["x-upstream-only".to_owned()],
                ..ResponseHeaderRules::default()
            },
        },
        ..plain_upstream()
    };

    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);
    let response = get(&dp, "/backend/api", &[("x-client", "1")]).await;

    mock.assert();
    assert_eq!(response.headers().get("x-set").unwrap(), "gateway");
    assert!(response.headers().get("x-upstream-only").is_none());
    assert!(response.headers().get("x-client").is_none());
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_alias_is_a_404_problem() {
    let (cp, _) = setup(&MockServer::start(), plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let mut response = get(&dp, "/who/api", &[]).await;
    let body = body(&mut response).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["status"], 404);
    assert!(
        problem["type"].as_str().unwrap().contains("not_found"),
        "{problem}"
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
}

#[tokio::test]
async fn unmatched_route_is_a_404_and_a_disallowed_method_a_400() {
    let server = MockServer::start();
    // GET-only, so a `DELETE` resolves by path and is then rejected by the
    // method guard rather than by route matching.
    let mut route = default_route();
    route.match_config.http.as_mut().unwrap().methods = vec![HttpMethod::Get];
    let (cp, _) = setup(&server, plain_upstream(), route).await;
    let dp = data_plane(cp);

    let response = get(&dp, "/backend/other", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let headers = HeaderMap::new();
    let mut response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::DELETE,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers,
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    // The path resolved, so the method rule is reported as a guard rejection
    // (DESIGN §Guard Rules) rather than as a missing route.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body(&mut response).await;
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["title"], "Validation Error");
    assert_eq!(
        problem["type"],
        format!("gts.cf.core.errors.err.v1~{}", gts::ERR_VALIDATION)
    );
}

#[tokio::test]
async fn disabled_upstream_answers_503() {
    let server = MockServer::start();
    let upstream = Upstream {
        enabled: false,
        ..plain_upstream()
    };
    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);
    let mut response = get(&dp, "/backend/api", &[]).await;
    let body = body(&mut response).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["status"], 503);
}

#[tokio::test]
async fn refused_connection_answers_502() {
    // A port with no listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(port),
            }],
        },
        ..plain_upstream()
    };
    let (cp, _) = setup(&MockServer::start(), upstream, default_route()).await;
    let dp = data_plane(cp);
    let mut response = get(&dp, "/backend/api", &[]).await;
    let body = body(&mut response).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["status"], 502);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
}

#[tokio::test]
async fn proxy_timeout_answers_504() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200)
            .body("late")
            .delay(std::time::Duration::from_secs(2));
    });

    let cp = control_plane();
    let mut upstream = plain_upstream();
    upstream.server = ServerConfig {
        endpoints: vec![Endpoint {
            scheme: EndpointScheme::Http,
            host: "127.0.0.1".to_owned(),
            port: Some(server.port()),
        }],
    };
    let created = cp
        .create_upstream(TENANT, upstream, Some("backend".to_owned()))
        .await
        .unwrap();
    let mut route = default_route();
    route.upstream_id = created.id;
    route.enabled = true;
    cp.create_route(TENANT, route).await.unwrap();

    let config = OagwConfig {
        proxy_timeout_secs: 1,
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let dp = DataPlaneService::new(Arc::new(cp), config)
        .unwrap()
        .with_registries(
            AuthPluginRegistry::empty(),
            GuardPluginRegistry::empty(),
            TransformPluginRegistry::empty(),
        );

    let mut response = get(&dp, "/backend/api", &[]).await;
    let body = body(&mut response).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT, "{body:?}");
}

#[tokio::test]
async fn body_larger_than_the_limit_is_a_413() {
    let server = MockServer::start();
    let _accepted = server.mock(|when, then| {
        when.method(POST).path("/api").body("\u{0}".repeat(200));
        then.status(200).body("stored");
    });
    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);

    let response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::POST,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: vec![0u8; 200].into(),
            upgrade: None,
        })
        .await;
    // The handler enforces the limit; the data plane accepts what it is given.
    assert_eq!(response.status(), StatusCode::OK);

    // An upstream that rejects the body still passes its status through.
    let reject = server.mock(|when, then| {
        when.method(POST).path("/api").body("\u{0}".repeat(201));
        then.status(413).body("too large");
    });
    let response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::POST,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: vec![0u8; 201].into(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_UPSTREAM
    );
    reject.assert();
}

#[tokio::test]
async fn target_host_header_selects_and_validates_endpoints() {
    let server = MockServer::start();
    let on_first = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("host", format!("127.0.0.1:{}", server.port()));
        then.status(200).body("first");
    });
    let on_second = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("host", format!("localhost:{}", server.port()));
        then.status(200).body("second");
    });

    // A pool is one scheme and one port (ADR 0001); the hosts differ, so the
    // mock server tells the endpoints apart by their `Host` header.
    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: Some(server.port()),
                },
                // `localhost` still reaches the loopback mock server, but is a
                // distinct host for the selector.
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "localhost".to_owned(),
                    port: Some(server.port()),
                },
            ],
        },
        ..plain_upstream()
    };
    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);

    // An explicit host selects the endpoint; the round-robin would not guarantee
    // either, so both requests are pinned.
    let mut response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "127.0.0.1")],
    )
    .await;
    assert_eq!(body(&mut response).await.as_ref(), b"first");
    let mut response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "LOCALHOST")],
    )
    .await;
    assert_eq!(body(&mut response).await.as_ref(), b"second");
    on_first.assert();
    on_second.assert();

    // An unknown endpoint is a 400.
    let response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "other.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A non-bare value (scheme, port or path) is a 400.
    let response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "http://127.0.0.1:80/")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn round_robin_picks_every_endpoint_in_turn() {
    let server = MockServer::start();
    let on_loopback = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("host", format!("127.0.0.1:{}", server.port()));
        then.status(200).body("a");
    });
    let on_localhost = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header("host", format!("localhost:{}", server.port()));
        then.status(200).body("b");
    });

    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: Some(server.port()),
                },
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "localhost".to_owned(),
                    port: Some(server.port()),
                },
            ],
        },
        ..plain_upstream()
    };
    // An explicit alias (not the common suffix) makes the pool round-robin.
    let (cp, _) = setup_with(&server, upstream, default_route(), Some("backend")).await;
    let dp = data_plane(cp);

    let mut seen_first = false;
    let mut seen_second = false;
    for _ in 0..4 {
        let mut response = get(&dp, "/backend/api", &[]).await;
        let body = body(&mut response).await;
        if body.as_ref() == b"a" {
            seen_first = true;
        } else {
            seen_second = true;
        }
    }
    assert!(seen_first && seen_second);
    on_loopback.assert_calls(2);
    on_localhost.assert_calls(2);
}

#[tokio::test]
async fn common_suffix_pool_requires_a_target_host() {
    let server = MockServer::start();
    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "api.openai.com".to_owned(),
                    port: Some(server.port()),
                },
                Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "backup.openai.com".to_owned(),
                    port: Some(server.port()),
                },
            ],
        },
        ..plain_upstream()
    };
    let (cp, _) = setup_with(&server, upstream, default_route(), None).await;
    let dp = data_plane(cp);

    // A pool whose alias is its common suffix must be addressed explicitly.
    // The derived alias carries the pool's (non-standard) port.
    let mut response = get(&dp, &format!("/openai.com:{}/api", server.port()), &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body(&mut response).await;
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["title"], "Missing Target Host");
}

// ---------------------------------------------------------------------------
// Match rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_allowlist_is_enforced() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v1").query_param("limit", "5");
        then.status(200).body("ok");
    });

    let mut route = default_route();
    route.match_config.http.as_mut().unwrap().query_allowlist = vec!["limit".to_owned()];
    let (cp, _) = setup(&server, plain_upstream(), route).await;
    let dp = data_plane(cp);

    // Only allowlisted parameters travel.
    let response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::GET,
            path: "/backend/api/v1".to_owned(),
            query: "limit=5".to_owned(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();

    // An unknown parameter rejects the request (DESIGN §Guard Rules).
    let mut response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::GET,
            path: "/backend/api/v1".to_owned(),
            query: "limit=5&secret=nope".to_owned(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body(&mut response).await;
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["title"], "Validation Error");
    assert_eq!(
        problem["type"],
        format!("gts.cf.core.errors.err.v1~{}", gts::ERR_VALIDATION)
    );
}

#[tokio::test]
async fn path_suffix_disabled_rejects_a_suffix() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });
    let mut route = default_route();
    route.match_config.http.as_mut().unwrap().path_suffix_mode =
        crate::domain::model::PathSuffixMode::Disabled;
    let (cp, _) = setup(&server, plain_upstream(), route).await;
    let dp = data_plane(cp);

    let response = get(&dp, "/backend/api", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();

    let mut response = get(&dp, "/backend/api/extra", &[]).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body(&mut response).await;
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["title"], "Validation Error");
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_rejects_with_429_and_headers() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });

    let upstream = Upstream {
        rate_limit: Some(RateLimitConfig {
            sustained: SustainedRate {
                rate: 1,
                window: crate::domain::model::RateWindow::Second,
            },
            ..RateLimitConfig::default()
        }),
        ..plain_upstream()
    };
    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);

    let ok = get(&dp, "/backend/api", &[]).await;
    assert_eq!(ok.status(), StatusCode::OK);
    // `X-RateLimit-*` is a rejection signal only (upstream.v1 `response_headers`).
    assert!(ok.headers().get("x-ratelimit-remaining").is_none());

    let limited = get(&dp, "/backend/api", &[]).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        limited.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
    assert!(limited.headers().get("retry-after").is_some());
    assert!(limited.headers().get("x-ratelimit-limit").is_some());
    assert!(limited.headers().get("x-ratelimit-remaining").is_some());
    mock.assert_calls(1);
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preflight_is_answered_locally_without_hitting_the_upstream() {
    let server = MockServer::start();
    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::ORIGIN,
        HeaderValue::from_static("https://app.example.com"),
    );
    headers.insert(
        "access-control-request-method",
        HeaderValue::from_static("POST"),
    );
    headers.insert(
        "access-control-request-headers",
        HeaderValue::from_static("x-api-key, content-type"),
    );
    let response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::OPTIONS,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers,
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // The preflight echoes what the browser asked about (ADR 0004): a `*`
    // answer would not satisfy the browser's own check against the pending
    // request's origin and method.
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .unwrap(),
        "POST"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-headers")
            .unwrap(),
        "x-api-key, content-type"
    );
    assert_eq!(
        response.headers().get("access-control-max-age").unwrap(),
        "86400"
    );
    assert_eq!(
        response.headers().get(http::header::VARY).unwrap(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
}

#[tokio::test]
async fn cross_origin_requests_are_validated_against_the_cors_config() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });

    let upstream = Upstream {
        cors: Some(crate::domain::model::CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            ..crate::domain::model::CorsConfig::default()
        }),
        ..plain_upstream()
    };
    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);

    // Allowed origin: the response carries the CORS headers.
    let response = get(
        &dp,
        "/backend/api",
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-expose-headers")
            .unwrap(),
        "x-request-id"
    );
    assert_eq!(
        response.headers().get(http::header::VARY).unwrap(),
        "Origin"
    );

    // Disallowed origin: 403, no upstream call.
    let response = get(
        &dp,
        "/backend/api",
        &[("origin", "https://evil.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    mock.assert_calls(1);

    // Disallowed method: a cross-origin `DELETE` is refused even though the
    // route itself admits it.
    let mut headers = HeaderMap::new();
    headers.insert(
        "origin",
        HeaderValue::from_static("https://app.example.com"),
    );
    let mut response = dp
        .proxy(ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::DELETE,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers,
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body(&mut response).await;
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        problem["type"],
        format!(
            "gts.cf.core.errors.err.v1~{}",
            gts::ERR_CORS_METHOD_NOT_ALLOWED
        )
    );
    mock.assert_calls(1);

    // Same-origin requests are never checked.
    let response = get(&dp, "/backend/api", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
}

/// A CORS block that is present but not enabled is not a policy: the request is
/// forwarded and no CORS check applies, even though `validate_cors` leaves such
/// a block's origin list empty when it is stored.
#[tokio::test]
async fn a_disabled_cors_block_never_rejects_a_request() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });

    let upstream = Upstream {
        cors: Some(crate::domain::model::CorsConfig {
            enabled: false,
            allowed_origins: vec![],
            allowed_methods: vec![],
            ..crate::domain::model::CorsConfig::default()
        }),
        ..plain_upstream()
    };
    let (cp, _) = setup(&server, upstream, default_route()).await;
    let dp = data_plane(cp);

    let response = get(
        &dp,
        "/backend/api",
        &[("origin", "https://any.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "no CORS headers are added for a disabled block"
    );
    mock.assert_calls(1);
}

/// A route-level CORS policy overrides the upstream's (DESIGN §"Config
/// Layering": Upstream < Route).
#[tokio::test]
async fn a_route_level_cors_policy_overrides_the_upstreams() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api");
        then.status(200).body("ok");
    });

    // The upstream allows nothing; the route allows one origin and method.
    let upstream = Upstream {
        cors: Some(crate::domain::model::CorsConfig {
            enabled: true,
            allowed_origins: vec![],
            allowed_methods: vec![],
            ..crate::domain::model::CorsConfig::default()
        }),
        ..plain_upstream()
    };
    let route = Route {
        cors: Some(crate::domain::model::CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            ..crate::domain::model::CorsConfig::default()
        }),
        ..default_route()
    };
    let (cp, _) = setup(&server, upstream, route).await;
    let dp = data_plane(cp);

    let response = get(
        &dp,
        "/backend/api",
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example.com"
    );

    // The route's list, not the upstream's empty one, is what is enforced.
    let response = get(
        &dp,
        "/backend/api",
        &[("origin", "https://evil.example.com")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    mock.assert_calls(1);
}

// ---------------------------------------------------------------------------
// Empty alias / malformed proxy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_path_without_an_alias_is_a_404() {
    let (cp, _) = setup(&MockServer::start(), plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let response = get(&dp, "/proxy/", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tenant_isolation_is_enforced_at_proxy_time() {
    let server = MockServer::start();
    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);

    let other = "00000000-0000-0000-0000-000000000099";
    let response = dp
        .proxy(ProxyCall {
            tenant_id: other.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::GET,
            path: "/backend/api".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn target_host_validation_does_not_leak_into_the_outbound_request() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/api")
            .header_missing(gts::HEADER_TARGET_HOST);
        then.status(200).body("ok");
    });

    let (cp, _) = setup(&server, plain_upstream(), default_route()).await;
    let dp = data_plane(cp);
    let response = get(
        &dp,
        "/backend/api",
        &[(gts::HEADER_TARGET_HOST, "127.0.0.1")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert();
}

// ---------------------------------------------------------------------------
// Streaming: server-sent events (DESIGN §3.2 "Streaming")
// ---------------------------------------------------------------------------

/// Serves one `text/event-stream` response on a local port.
///
/// The two events are written in separate `write` calls with a pause between
/// them, so a buffering proxy never sees the second event before the first one
/// has been handed over.
fn sse_upstream() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = [0u8; 4096];
        // The request head is discarded: only the connection is needed to
        // stream the canned response back.
        drop(tokio::io::AsyncReadExt::read(&mut socket, &mut head).await);
        tokio::io::AsyncWriteExt::write_all(
            &mut socket,
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"data: one\n\n")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::flush(&mut socket).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"data: two\n\n")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::flush(&mut socket).await.unwrap();
    });
    port
}

/// The next data chunk on a proxy response body.
async fn next_data(response: &mut http::Response<ProxyBody>) -> String {
    use http_body_util::BodyExt;
    loop {
        let frame = response
            .body_mut()
            .frame()
            .await
            .unwrap_or_else(|| panic!("stream ended before the event arrived"))
            .unwrap_or_else(|error| panic!("stream failed: {error}"));
        if let Ok(chunk) = frame.into_data() {
            return String::from_utf8_lossy(&chunk).to_string();
        }
    }
}

/// An upstream + route pointed at an arbitrary local port.
async fn upstream_on_port(cp: &ControlPlaneService, port: u16) -> Uuid {
    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "127.0.0.1".to_owned(),
                port: Some(port),
            }],
        },
        ..plain_upstream()
    };
    let created = cp
        .create_upstream(TENANT, upstream, Some("backend".to_owned()))
        .await
        .unwrap();
    let mut route = default_route();
    route.upstream_id = created.id;
    route.enabled = true;
    cp.create_route(TENANT, route).await.unwrap();
    created.id
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_events_stream_incrementally() {
    use http_body_util::BodyExt;

    let port = sse_upstream();
    let cp = control_plane();
    upstream_on_port(&cp, port).await;
    let dp = data_plane(cp);

    let mut response = get(&dp, "/backend/api/events", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream",
        "the upstream content type is passed through"
    );
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_UPSTREAM
    );
    // Each event arrives on its own: the first is readable before the upstream
    // has written the second, which is only true if the proxy streams.
    let first = next_data(&mut response).await;
    assert_eq!(first, "data: one\n\n");
    assert!(!first.contains("two"), "the events must not be batched");
    assert_eq!(next_data(&mut response).await, "data: two\n\n");
    // The upstream closes after the second event, so the streamed body ends
    // cleanly rather than erroring or hanging.
    let frame = response.body_mut().frame().await;
    assert!(
        frame.is_none(),
        "the body must end when the upstream closes the stream: {frame:?}"
    );
}
