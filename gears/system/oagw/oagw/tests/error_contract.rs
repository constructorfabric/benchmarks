//! Integration tests of the shared error contract
//! (FEATURE `error-handling`, entry 2.5: the DESIGN §3.3 mapping, the
//! problem+json body shape, `X-OAGW-Error-Source`, and the retry guidance).
//!
//! Every assertion here is made against the registered router, so the body
//! shape is the one a client actually receives.
// @cpt-dod:cpt-cf-oagw-dod-error-handling-disabled-upstream-503:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-payload-too-large-413:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-retriability:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-target-host-bodies:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-trace-id:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-validation-rendering:p1

use std::sync::Arc;

use oagw::domain::dto::{EndpointScheme, HttpMethod};
use oagw::test_support::{
    management_surface, permissive_surface, route_for, seed_route, seed_upstream, stub_upstream,
    upstream_at, FakeHierarchyTenantResolver, FakePolicyAuthZ,
};
use uuid::Uuid;

const PROXY: &str = "/oagw/v1/proxy";

/// The `oagw` block the proxy tests need.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream and one route seeded over a stub upstream.
async fn seeded() -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get, HttpMethod::Post]));
    (surface, stub, tenant)
}

/// The problem body of an exchange.
fn problem(exchange: &oagw::test_support::ProxyExchange) -> serde_json::Value {
    serde_json::from_slice(&exchange.body).expect("the gateway error is problem+json")
}

/// The `oagw` block plus a two-endpoint pool whose common suffix *is* the
/// alias, which is the shape the target-host header is required for.
async fn multi_endpoint() -> (oagw::test_support::ManagementSurface, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let tenant = Uuid::new_v4();
    let mut upstream = upstream_at(tenant, "vendor.com", EndpointScheme::Https, "a.vendor.com", 443);
    upstream.server.endpoints.push(oagw::domain::dto::Endpoint {
        scheme: EndpointScheme::Https,
        host: "b.vendor.com".to_owned(),
        port: 443,
    });
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    (surface, tenant)
}

#[tokio::test]
async fn a_gateway_error_is_problem_json_with_the_five_members_and_the_source() {
    let (surface, _stub, tenant) = seeded().await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v9"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert_eq!(exchange.header("content-type"), Some("application/problem+json"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    let document = problem(&exchange);
    for member in ["type", "title", "status", "detail", "instance"] {
        assert!(document.get(member).is_some(), "`{member}` is on the wire: {document}");
    }
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    assert_eq!(document["status"], 404);
    assert_eq!(
        document["instance"], "/oagw/v1/proxy/api.vendor.com/v9",
        "the gear-relative path, with no `/api` segment"
    );
    assert!(document.get("context").is_none(), "extension members are at the top level");
}

#[tokio::test]
async fn the_trace_identifier_the_request_carried_is_echoed() {
    let (surface, _stub, tenant) = seeded().await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            &format!("{PROXY}/api.vendor.com/v9"),
            &[("x-request-id", "trace-42")],
            b"",
        )
        .await;
    let document = problem(&exchange);
    assert_eq!(document["trace_id"], "trace-42");
}

#[tokio::test]
async fn a_method_outside_the_allowlist_is_a_problem_body_with_an_allow_header() {
    let (surface, stub, tenant) = seeded().await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "DELETE", &format!("{PROXY}/api.vendor.com/v1"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(exchange.header("allow"), Some("GET, POST"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    let document = problem(&exchange);
    assert_eq!(document["status"], 405);
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn a_disabled_upstream_is_503_link_unavailable_and_retriable_without_guidance() {
    let (surface, stub, tenant) = seeded().await;
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, _, _) = storage.repositories();
    let mut record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    record.upstream.enabled = false;
    upstreams
        .replace(tenant, oagw::domain::repo::UpstreamRecord { upstream: record.upstream.clone(), plugin_bindings: Vec::new() })
        .expect("the upstream is disabled");
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(stub.received().is_empty(), "a disabled upstream opens no connection");
    let document = problem(&exchange);
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    assert!(
        exchange.header("retry-after").is_none(),
        "the retriable 503 carries no guidance: no header"
    );
    assert!(
        document.get("retry_after_seconds").is_none(),
        "and no `retry_after_seconds` member: {document}"
    );
}

#[tokio::test]
async fn a_timeout_carries_the_configured_proxy_timeout_as_guidance() {
    let surface = permissive_surface(Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 1,
        "max_body_size_bytes": 1_048_576
    })))
    .await;
    // A listener that accepts and never answers, so the budget decides.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("the listener binds");
    let address = listener.local_addr().expect("the address");
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            drop(tokio::spawn(async move {
                let socket = socket;
                let _ = socket;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }));
        }
    });
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, "127.0.0.1", address.port());
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1/feed"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(exchange.header("retry-after"), Some("1"));
    let document = problem(&exchange);
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
    assert_eq!(document["retry_after_seconds"], 1);
}

#[tokio::test]
async fn the_target_host_family_carries_alias_and_valid_hosts() {
    let (surface, tenant) = multi_endpoint().await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/vendor.com/v1"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_REQUEST);
    let document = problem(&exchange);
    assert_eq!(
        document["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(document["alias"], "vendor.com");
    let hosts = document["valid_hosts"].as_array().expect("the valid hosts");
    assert_eq!(hosts.len(), 2, "both pool endpoints are named: {document}");
    assert!(document.get("invalid_value").is_none(), "no header value was supplied");

    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            &format!("{PROXY}/vendor.com/v1"),
            &[("x-oagw-target-host", "nope.vendor.com")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_REQUEST);
    let document = problem(&exchange);
    assert_eq!(
        document["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
    assert_eq!(document["invalid_value"], "nope.vendor.com", "the echoed header value");
    assert_eq!(document["valid_hosts"].as_array().expect("hosts").len(), 2);
}

#[tokio::test]
async fn an_oversized_body_is_413_without_retry_guidance() {
    let surface = permissive_surface(Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 16
    })))
    .await;
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, "backend.example.com", 443);
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Post]));
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "POST",
            &format!("{PROXY}/api.vendor.com/v1/orders"),
            &[("content-type", "application/json"), ("content-length", "32")],
            &[0u8; 32],
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(exchange.header("retry-after").is_none());
    let document = problem(&exchange);
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
}

#[tokio::test]
async fn an_unauthenticated_proxy_request_is_the_oagw_authentication_surface() {
    let (surface, _stub, _tenant) = seeded().await;
    let exchange = surface.proxy("GET", &format!("{PROXY}/api.vendor.com/v1"), &[], b"", None).await;
    assert_eq!(exchange.status, http::StatusCode::UNAUTHORIZED);
    assert_eq!(exchange.header("content-type"), Some("application/problem+json"));
    let document = problem(&exchange);
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
    assert_eq!(document["status"], 401);
}

#[tokio::test]
async fn a_management_not_found_is_the_route_not_found_type_with_the_instance() {
    let surface = management_surface(
        None,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await;
    let tenant = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000001",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    assert_eq!(document["instance"], "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000001");
    assert!(
        document
            .as_object()
            .expect("an object")
            .keys()
            .all(|member| [
                "type", "title", "status", "detail", "instance", "trace_id", "upstream_id", "host",
                "path", "retry_after_seconds", "referenced_by", "alias", "valid_hosts", "invalid_value",
            ]
            .contains(&member.as_str())),
        "the vocabulary is closed on the management surface too: {document}"
    );
}

#[tokio::test]
async fn an_upstream_problem_body_is_passed_through_unmodified() {
    let (surface, _stub, tenant) = seeded().await;
    // The stub answers with its own problem+json and carries no OAGW marker,
    // which is what an upstream that speaks RFC 9457 looks like.
    let upstream_problem = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/problem+json\r\n\r\n{\"type\":\"https://upstream.example/errors\",\"title\":\"Upstream\",\"status\":400}";
    let stub = stub_upstream(vec![upstream_problem.to_owned()]).await;
    let (host, port) = stub.endpoint();
    let storage = surface.gear.storage().expect("the store");
    let (upstreams, routes, _) = storage.repositories();
    let _ = routes;
    let record = upstreams.get_by_alias(tenant, "api.vendor.com").expect("the upstream");
    let mut upstream = record.upstream.clone();
    upstream.server.endpoints = vec![oagw::domain::dto::Endpoint {
        scheme: EndpointScheme::Http,
        host: host.clone(),
        port,
    }];
    upstreams
        .replace(tenant, oagw::domain::repo::UpstreamRecord { upstream, plugin_bindings: Vec::new() })
        .expect("the upstream is re-pointed at the stub");

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", &format!("{PROXY}/api.vendor.com/v1"), &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_REQUEST);
    assert_eq!(
        exchange.header("x-oagw-error-source"),
        Some("upstream"),
        "the upstream produced it, whatever its body format"
    );
    let document: serde_json::Value = serde_json::from_slice(&exchange.body).expect("the body is untouched");
    assert_eq!(document["title"], "Upstream", "the upstream body is not rewritten");
    assert!(document.get("instance").is_none(), "the completing layer never touches a passthrough");
    assert!(document.get("trace_id").is_none());
}

/// The two rejection types owned by `cpt-cf-oagw-feature-cors` (entry 2.8) are
/// rendered through this contract with a `403` and the shared request-context
/// members, so the shape does not fork per rejection source. Entry 2.8 does not
/// yet produce them on the router, so the rendering is asserted against the
/// one serializer every gateway error goes through.
#[tokio::test]
async fn a_cors_rejection_is_rendered_through_the_same_contract() {
    for (error, name) in [
        (
            oagw::domain::error::DomainError::CorsOriginNotAllowed {
                path: Some("/oagw/v1/proxy/api.vendor.com/v1".to_owned()),
                trace_id: Some("trace-cors".to_owned()),
            },
            "origin",
        ),
        (
            oagw::domain::error::DomainError::CorsMethodNotAllowed {
                path: Some("/oagw/v1/proxy/api.vendor.com/v1".to_owned()),
                trace_id: None,
            },
            "method",
        ),
    ] {
        let response = axum::response::IntoResponse::into_response(oagw::api::rest::error::ApiError::Domain(error));
        assert_eq!(response.status(), http::StatusCode::FORBIDDEN, "{name}");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json"),
            "{name}"
        );
        assert_eq!(
            response.headers().get("x-oagw-error-source").and_then(|value| value.to_str().ok()),
            Some("gateway"),
            "{name}"
        );
        assert_eq!(
            response.headers().get("vary").and_then(|value| value.to_str().ok()),
            Some("Origin"),
            "{name}"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body");
        let document: serde_json::Value = serde_json::from_slice(&body).expect("problem+json");
        assert_eq!(document["status"], 403, "{name}");
        assert!(
            document["type"].as_str().expect("a type").ends_with(&format!("cors.{name}_not_allowed.v1")),
            "{document}"
        );
        assert!(document.get("alias").is_none() && document.get("valid_hosts").is_none(),
            "the CORS types carry no routing member: {document}");
    }
}

/// The `429` refusal carries the guidance pair from the rate-limit decision and
/// no `X-RateLimit-*` header from this contract. The limiter itself is entry
/// 2.7's, so the rendering is asserted against the one serializer.
#[tokio::test]
async fn a_rate_limit_refusal_carries_the_guidance_pair_and_no_rate_limit_header() {
    let error = oagw::domain::error::DomainError::RateLimitExceeded {
        upstream_id: Some("api.vendor.com".to_owned()),
        host: Some("a.vendor.com".to_owned()),
        retry_after_seconds: Some(7),
        trace_id: None,
    };
    let response = axum::response::IntoResponse::into_response(oagw::api::rest::error::ApiError::Domain(error));
    assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers().get("retry-after").and_then(|value| value.to_str().ok()),
        Some("7"),
        "the header and the member carry the same value"
    );
    for name in ["x-ratelimit-limit", "x-ratelimit-remaining", "x-ratelimit-reset"] {
        assert!(response.headers().get(name).is_none(), "`{name}` is not this contract's");
    }
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body");
    let document: serde_json::Value = serde_json::from_slice(&body).expect("problem+json");
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
    assert_eq!(document["retry_after_seconds"], 7);
}
