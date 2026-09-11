//! Integration tests of the actual-request CORS enforcement on the proxy path
//! (`cpt-cf-oagw-flow-cors-actual-request`,
//! `cpt-cf-oagw-algo-cors-request-evaluation`,
//! `cpt-cf-oagw-algo-cors-response-headers`).
//!
//! Every test drives the real `OagwGear` through the registered router and a
//! live stub upstream, so the position of the CORS check in the pipeline —
//! after route match, before the plugin chain and the upstream call — is
//! exercised rather than assumed.
// @cpt-dod:cpt-cf-oagw-dod-cors-deny-by-default:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-method-enforcement:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-origin-enforcement:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-response-headers:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use uuid::Uuid;

use oagw::test_support::{
    permissive_surface, route_for, seed_route, seed_upstream, stub_upstream, upstream_at,
};

const ORIGIN_NOT_ALLOWED: &str = "gts://gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
const METHOD_NOT_ALLOWED: &str = "gts://gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

fn proxy_config() -> Option<Value> {
    Some(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream carrying `cors` and one route admitting `GET`
/// and `POST`, seeded over a stub upstream answering `script`.
async fn seeded(
    cors: Option<Value>,
    script: Vec<String>,
) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(script).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let mut upstream = upstream_at(tenant, "api.vendor.com", oagw::domain::dto::EndpointScheme::Http, &host, port);
    upstream.cors = serde_json::from_value(cors.unwrap_or(Value::Null)).unwrap_or(None);
    let id = seed_upstream(&surface, upstream);
    let mut route = route_for(tenant, id, "/v1", &[oagw::domain::dto::HttpMethod::Get, oagw::domain::dto::HttpMethod::Post]);
    route.cors = None;
    seed_route(&surface, route);
    (surface, stub, tenant)
}

fn cors_block(origins: &[&str], methods: &[&str]) -> Value {
    json!({
        "enabled": true,
        "allowed_origins": origins,
        "allowed_methods": methods,
    })
}

#[tokio::test]
async fn an_allowed_origin_is_forwarded_with_the_cors_response_headers() {
    let (surface, _stub, tenant) =
        seeded(Some(cors_block(&["https://app.dev"], &["GET", "POST"])), Vec::new()).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://app.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(exchange.header("vary"), Some("Origin"));
    assert!(exchange.header("access-control-allow-credentials").is_none());
    assert!(exchange.header("access-control-expose-headers").is_none());
}

/// The upstream header set stays authoritative where it already spoke: its own
/// `Access-Control-Allow-Origin` is preserved, and `Origin` is appended to its
/// own `Vary` list rather than overwriting it.
#[tokio::test]
async fn an_upstream_origin_header_is_preserved_and_its_vary_is_appended() {
    let scripted = vec![
        "HTTP/1.1 200 OK\r\naccess-control-allow-origin: https://upstream.dev\r\nvary: Accept-Encoding\r\ncontent-type: text/plain\r\ncontent-length: 7\r\n\r\npayload"
            .to_owned(),
    ];
    let (surface, _stub, tenant) =
        seeded(Some(cors_block(&["https://app.dev"], &["GET", "POST"])), scripted).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://app.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(
        exchange.header("access-control-allow-origin"),
        Some("https://upstream.dev"),
        "the upstream answer is never overwritten"
    );
    assert_eq!(exchange.header("vary"), Some("Accept-Encoding, Origin"));
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_with_the_cors_error_type() {
    let (surface, stub, tenant) =
        seeded(Some(cors_block(&["https://app.dev"], &["GET", "POST"])), Vec::new()).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://evil.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN, "{}", exchange.text());
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert_eq!(exchange.header("vary"), Some("Origin"));
    let body: Value = serde_json::from_slice(&exchange.body).expect("the problem+json body");
    assert_eq!(body["type"], ORIGIN_NOT_ALLOWED, "{body}");
    assert_eq!(body["status"], 403);
    assert!(stub.received().is_empty(), "the upstream never receives a rejected request");
}

#[tokio::test]
async fn a_disallowed_method_is_rejected_only_after_the_origin_matched() {
    let (surface, _stub, tenant) = seeded(Some(cors_block(&["https://app.dev"], &["GET"])), Vec::new()).await;
    for (origin, expected) in
        [("https://evil.dev", ORIGIN_NOT_ALLOWED), ("https://app.dev", METHOD_NOT_ALLOWED)]
    {
        let exchange = surface
            .proxy_for(
                tenant,
                Uuid::new_v4(),
                "POST",
                "/oagw/v1/proxy/api.vendor.com/v1/orders",
                &[("origin", origin)],
                b"",
            )
            .await;
        assert_eq!(exchange.status, http::StatusCode::FORBIDDEN, "{origin}: {}", exchange.text());
        let body: Value = serde_json::from_slice(&exchange.body).expect("the problem+json body");
        assert_eq!(body["type"], expected, "{origin}: {body}");
        assert_eq!(exchange.header("vary"), Some("Origin"));
    }
}

#[tokio::test]
async fn an_allowed_credentialed_request_names_its_exposed_headers() {
    let (surface, _stub, tenant) = seeded(
        Some(json!({
            "enabled": true,
            "allowed_origins": ["https://app.dev"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["X-Request-Id", "X-Rate-Limit"],
            "allow_credentials": true
        })),
        Vec::new(),
    )
    .await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://app.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("https://app.dev"));
    assert_eq!(exchange.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(
        exchange.header("access-control-expose-headers"),
        Some("X-Request-Id, X-Rate-Limit")
    );
    // A wildcard verdict under a credentialed policy is never served.
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://stranger.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_wildcard_policy_answers_the_wildcard_and_never_credentials() {
    let (surface, _stub, tenant) = seeded(
        Some(json!({"enabled": true, "allowed_origins": ["*"], "allowed_methods": ["GET"]})),
        Vec::new(),
    )
    .await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://anything.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("access-control-allow-origin"), Some("*"));
    assert!(exchange.header("access-control-allow-credentials").is_none());
}

#[tokio::test]
async fn a_disabled_configuration_forwards_with_no_cors_header() {
    let (surface, _stub, tenant) =
        seeded(Some(json!({"enabled": false, "allowed_origins": ["https://app.dev"]})), Vec::new()).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://evil.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    assert_eq!(exchange.header("vary"), Some("Origin"));
    assert!(exchange.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

#[tokio::test]
async fn an_upstream_without_a_cors_block_grants_nothing() {
    let (surface, _stub, tenant) = seeded(None, Vec::new()).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://app.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("vary"), Some("Origin"));
    assert!(exchange.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

#[tokio::test]
async fn a_request_without_an_origin_is_not_a_cors_subject() {
    let (surface, _stub, tenant) =
        seeded(Some(cors_block(&["https://app.dev"], &["GET"])), Vec::new()).await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert!(exchange.header("vary").is_none());
    assert!(exchange.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

/// The route layer of the CORS configuration is a descendant layer: it
/// contributes under `sharing: inherit` by unioning its origin set over the
/// upstream's, and the union is add-only in both directions.
#[tokio::test]
async fn the_route_layer_of_the_cors_configuration_is_enforced() {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let mut upstream =
        upstream_at(tenant, "api.vendor.com", oagw::domain::dto::EndpointScheme::Http, &host, port);
    upstream.cors = Some(serde_json::from_value(cors_block(&["https://app.dev"], &["GET"])).unwrap());
    let id = seed_upstream(&surface, upstream);
    let mut route =
        route_for(tenant, id, "/v1", &[oagw::domain::dto::HttpMethod::Get, oagw::domain::dto::HttpMethod::Post]);
    let mut route_cors: oagw::domain::dto::CorsConfig =
        serde_json::from_value(cors_block(&["https://route.dev"], &["GET", "POST"])).unwrap();
    route_cors.sharing = oagw::domain::dto::SharingMode::Inherit;
    route.cors = Some(route_cors);
    seed_route(&surface, route);

    for (origin, expected) in
        [("https://route.dev", http::StatusCode::OK), ("https://app.dev", http::StatusCode::OK)]
    {
        let exchange = surface
            .proxy_for(
                tenant,
                Uuid::new_v4(),
                "GET",
                "/oagw/v1/proxy/api.vendor.com/v1/orders",
                &[("origin", origin)],
                b"",
            )
            .await;
        assert_eq!(exchange.status, expected, "{origin}: {}", exchange.text());
    }
    // A method the union admits, on an origin the union admits.
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "POST",
            "/oagw/v1/proxy/api.vendor.com/v1/orders",
            &[("origin", "https://route.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
}
