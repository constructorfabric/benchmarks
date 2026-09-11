//! Sibling unit tests of the built-in CORS handler of entry 2.8 as a step of
//! the proxy pipeline (`cpt-cf-oagw-flow-cors-preflight`,
//! `cpt-cf-oagw-flow-cors-actual-request`).
//!
//! The tests drive the real [`DataPlaneServiceImpl`] over the in-memory store
//! and a live stub upstream, so the position of the CORS check in the pipeline
//! — after route match, before the plugin chain and the upstream call — is
//! exercised rather than assumed.

use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use super::rate_limiter::RateLimiterRegistry;
use super::service::{DataPlaneLimits, DataPlaneServiceImpl};
use crate::domain::dto::{CorsConfig, EndpointScheme, HttpMethod, SharingMode};
use crate::domain::error::DomainError;
use crate::domain::proxy::{ProxyContext, ProxyResponse};
use crate::domain::rate_limit::SharedClock;
use crate::domain::repo::{RouteRecord, UpstreamRecord};
use crate::domain::services::management::{Actor, AncestorResolver, AuthorizeError, ManagementAuthorizer};
use crate::infra::storage::Storage;
use crate::test_support::{route_for, stub_upstream, StubUpstream, upstream_at};

// -- local fakes -------------------------------------------------------------

struct AllowAll;

#[async_trait::async_trait]
impl ManagementAuthorizer for AllowAll {
    async fn authorize(&self, _actor: &Actor, _permission: &str, _resource_id: &str) -> Result<(), AuthorizeError> {
        Ok(())
    }
}

struct NoAncestors;

#[async_trait::async_trait]
impl AncestorResolver for NoAncestors {
    async fn ancestors(&self, _actor: &Actor, _tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        Ok(Vec::new())
    }
}

// -- the fixture -------------------------------------------------------------

fn limits() -> DataPlaneLimits {
    DataPlaneLimits { allow_http_upstream: true, max_body_size_bytes: 1 << 20, proxy_timeout_secs: 5 }
}

fn cors(origins: &[&str], methods: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: Some(origins.iter().map(|origin| (*origin).to_owned()).collect()),
        allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

const DEFAULT_STUB_ANSWER: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 7\r\n\r\npayload";

/// The data plane over one upstream and one route, with the upstream carrying
/// `upstream_cors` and the route `route_cors`, and its endpoints pointed at a
/// stub answering `script` in order.
async fn seeded(
    upstream_cors: Option<CorsConfig>,
    route_cors: Option<CorsConfig>,
    script: Vec<String>,
) -> (DataPlaneServiceImpl, Uuid, StubUpstream) {
    let stub = stub_upstream(script).await;
    let (host, port) = stub.endpoint();
    let storage = Storage::new();
    let (upstreams, routes, _) = storage.repositories();
    let tenant = Uuid::new_v4();
    let mut record = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    record.cors = upstream_cors;
    upstreams
        .create(tenant, UpstreamRecord { upstream: record.clone(), plugin_bindings: Vec::new() })
        .expect("the upstream is seeded");
    let mut route = route_for(tenant, record.id, "/v1", &[HttpMethod::Get, HttpMethod::Post]);
    route.cors = route_cors;
    routes
        .create(tenant, RouteRecord { route, plugin_bindings: Vec::new() })
        .expect("the route is seeded");
    let service = DataPlaneServiceImpl::new(
        upstreams,
        routes,
        Arc::new(NoAncestors),
        Arc::new(AllowAll),
        limits(),
    )
    .with_rate_limiters(Arc::new(RateLimiterRegistry::new(Arc::new(SharedClock::at(0, 1_700_000_000)))));
    (service, tenant, stub)
}

fn context(tenant: Uuid, method: &str, origin: Option<&str>) -> ProxyContext {
    let mut headers = vec![("host".to_owned(), "api.vendor.com".to_owned())];
    if let Some(origin) = origin {
        headers.push(("origin".to_owned(), origin.to_owned()));
    }
    ProxyContext {
        method: method.to_owned(),
        alias: "api.vendor.com".to_owned(),
        path_suffix: Some("/v1".to_owned()),
        query: None,
        headers,
        body: Bytes::new(),
        tenant_id: tenant,
        principal_id: Uuid::new_v4(),
        peer_addr: Some("10.0.0.1:5000".to_owned()),
        trace_id: Some("trace-1".to_owned()),
    }
}

fn header<'a>(response: &'a ProxyResponse, name: &str) -> Option<&'a str> {
    response.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

fn outcome_of(outcome: Result<ProxyResponse, crate::domain::proxy::ProxyFailure>) -> ProxyResponse {
    outcome.expect("the exchange produces a response")
}

// -- the preflight call-in ---------------------------------------------------

#[tokio::test]
async fn a_preflight_for_an_exact_origin_under_a_credentialed_policy_names_it() {
    let mut upstream_cors = cors(&["https://app.dev"], &["GET", "POST"]);
    upstream_cors.allow_credentials = true;
    let (service, tenant, stub) = seeded(Some(upstream_cors), None, Vec::new()).await;
    let headers = vec![
        ("origin".to_owned(), "https://app.dev".to_owned()),
        ("access-control-request-method".to_owned(), "POST".to_owned()),
    ];
    let response = service.preflight(Some("api.vendor.com"), Some(tenant), &headers);
    assert_eq!(response.status, 204);
    assert_eq!(response.source, crate::domain::proxy::ErrorSource::Gateway);
    assert!(matches!(response.body, crate::domain::proxy::ProxyBody::Empty));
    assert!(response
        .headers
        .iter()
        .any(|(name, value)| name == "access-control-allow-origin" && value == "https://app.dev"));
    assert!(response
        .headers
        .iter()
        .any(|(name, value)| name == "access-control-allow-methods" && value == "POST"));
    assert!(response.headers.iter().any(|(name, value)| name == "access-control-max-age" && value == "86400"));
    assert!(response.headers.iter().any(|(name, value)| name == "access-control-allow-credentials"
        && value == "true"));
    assert!(response.headers.iter().all(|(name, _)| name != "access-control-expose-headers"));
    assert_eq!(
        response.observation.cors.as_ref().map(|cors| cors.outcome.as_str()),
        Some("preflight_short_circuit")
    );
    assert!(stub.received().is_empty(), "a preflight is never forwarded");
}

#[tokio::test]
async fn a_preflight_stays_credential_free_without_a_caller_or_an_exact_match() {
    let mut upstream_cors = cors(&["https://app.dev"], &["GET", "POST"]);
    upstream_cors.allow_credentials = true;
    let (service, tenant, _stub) = seeded(Some(upstream_cors), None, Vec::new()).await;
    let headers = vec![
        ("origin".to_owned(), "https://app.dev".to_owned()),
        ("access-control-request-method".to_owned(), "POST".to_owned()),
    ];
    // No caller, or an unknown caller: permissive and credential-free.
    for caller in [None, Some(Uuid::new_v4())] {
        let response = service.preflight(Some("api.vendor.com"), caller, &headers);
        assert!(response.headers.iter().all(|(name, _)| name != "access-control-allow-credentials"));
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "access-control-allow-origin" && value == "https://app.dev"));
    }
    // A caller, but an origin the upstream never allowed.
    let other = vec![
        ("origin".to_owned(), "https://other.dev".to_owned()),
        ("access-control-request-method".to_owned(), "POST".to_owned()),
    ];
    let response = service.preflight(Some("api.vendor.com"), Some(tenant), &other);
    assert!(response.headers.iter().all(|(name, _)| name != "access-control-allow-credentials"));
    assert!(response
        .headers
        .iter()
        .any(|(name, value)| name == "access-control-allow-origin" && value == "https://other.dev"));
}

#[tokio::test]
async fn a_preflight_reflects_nothing_outside_the_reflection_bounds() {
    let (service, tenant, _stub) = seeded(Some(cors(&["https://app.dev"], &["GET", "POST"])), None, Vec::new()).await;
    let headers = vec![
        ("origin".to_owned(), "https://app.dev\r\nX-Injected: 1".to_owned()),
        ("access-control-request-method".to_owned(), "TRACE".to_owned()),
    ];
    let response = service.preflight(Some("api.vendor.com"), Some(tenant), &headers);
    assert!(response.headers.iter().all(|(name, _)| name != "access-control-allow-origin"));
    assert!(response.headers.iter().all(|(name, _)| name != "access-control-allow-methods"));
    assert!(response.headers.iter().any(|(name, value)| name == "vary"
        && value == "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"));
}

// -- the actual-request check ------------------------------------------------

#[tokio::test]
async fn a_disallowed_origin_is_rejected_before_the_upstream() {
    let (service, tenant, stub) =
        seeded(Some(cors(&["https://app.dev"], &["GET", "POST"])), None, Vec::new()).await;
    let response = outcome_of(service.execute(context(tenant, "GET", Some("https://evil.dev")), None).await);
    assert_eq!(response.status, 403);
    assert!(matches!(response.error, Some(DomainError::CorsOriginNotAllowed { .. })), "{:?}", response.error);
    assert_eq!(
        response.observation.cors.as_ref().map(|cors| cors.outcome.as_str()),
        Some("origin_not_allowed")
    );
    assert!(stub.received().is_empty(), "the upstream never receives a rejected request");
}

#[tokio::test]
async fn a_disallowed_method_is_rejected_only_after_the_origin_passed() {
    let (service, tenant, stub) = seeded(Some(cors(&["https://app.dev"], &["GET"])), None, Vec::new()).await;
    let response =
        outcome_of(service.execute(context(tenant, "POST", Some("https://app.dev")), None).await);
    assert_eq!(response.status, 403);
    assert!(matches!(response.error, Some(DomainError::CorsMethodNotAllowed { .. })), "{:?}", response.error);
    assert_eq!(
        response.observation.cors.as_ref().map(|cors| cors.outcome.as_str()),
        Some("method_not_allowed")
    );
    // A disallowed origin is never reported as a method failure.
    let response =
        outcome_of(service.execute(context(tenant, "POST", Some("https://evil.dev")), None).await);
    assert!(matches!(response.error, Some(DomainError::CorsOriginNotAllowed { .. })));
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn an_allowed_request_carries_the_cors_headers_and_the_appended_vary() {
    let scripted = vec![
        "HTTP/1.1 200 OK\r\nvary: Accept-Encoding\r\ncontent-type: text/plain\r\ncontent-length: 7\r\n\r\npayload"
            .to_owned(),
    ];
    let (service, tenant, _stub) =
        seeded(Some(cors(&["https://app.dev"], &["GET", "POST"])), None, scripted).await;
    let response = outcome_of(service.execute(context(tenant, "GET", Some("https://app.dev")), None).await);
    assert_eq!(response.status, 200);
    assert_eq!(header(&response, "access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(header(&response, "vary"), Some("Accept-Encoding, Origin"));
    assert!(response.headers.iter().all(|(name, _)| name != "access-control-allow-credentials"));
    assert!(response.observation.cors.is_none(), "a forwarded request carries no CORS label");
}

#[tokio::test]
async fn a_disabled_configuration_forwards_with_the_vary_only() {
    let mut disabled = cors(&["https://app.dev"], &["GET", "POST"]);
    disabled.enabled = false;
    let (service, tenant, _stub) = seeded(Some(disabled), None, Vec::new()).await;
    let response = outcome_of(service.execute(context(tenant, "GET", Some("https://evil.dev")), None).await);
    assert_eq!(response.status, 200);
    assert_eq!(header(&response, "vary"), Some("Origin"));
    assert!(response.headers.iter().all(|(name, _)| !name.starts_with("access-control-allow")));
    assert!(response.observation.cors.is_none());
}

#[tokio::test]
async fn an_upstream_without_a_cors_block_behaves_as_a_disabled_one() {
    let (service, tenant, _stub) = seeded(None, None, Vec::new()).await;
    let response = outcome_of(service.execute(context(tenant, "GET", Some("https://evil.dev")), None).await);
    assert_eq!(response.status, 200);
    assert_eq!(header(&response, "vary"), Some("Origin"));
    assert!(response
        .headers
        .iter()
        .all(|(name, _)| !name.starts_with("access-control-allow")));
}

#[tokio::test]
async fn a_request_without_an_origin_gains_no_vary() {
    let (service, tenant, _stub) =
        seeded(Some(cors(&["https://app.dev"], &["GET", "POST"])), None, vec![DEFAULT_STUB_ANSWER.to_owned()]).await;
    let response = outcome_of(service.execute(context(tenant, "GET", None), None).await);
    assert_eq!(response.status, 200);
    assert!(response.headers.iter().all(|(name, _)| name != "vary"));
    assert!(response.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

#[tokio::test]
async fn a_merged_configuration_combining_credentials_with_a_wildcard_is_fail_closed() {
    let mut inherited = cors(&["*"], &["GET", "POST"]);
    inherited.allow_credentials = true;
    let (service, tenant, _stub) = seeded(Some(inherited), None, Vec::new()).await;
    let response =
        outcome_of(service.execute(context(tenant, "GET", Some("https://app.dev")), None).await);
    assert_eq!(response.status, 403);
    assert!(matches!(response.error, Some(DomainError::CorsOriginNotAllowed { .. })));
    assert_eq!(
        response.observation.cors.as_ref().map(|cors| cors.outcome.as_str()),
        Some("merged_config_rejected")
    );
}
