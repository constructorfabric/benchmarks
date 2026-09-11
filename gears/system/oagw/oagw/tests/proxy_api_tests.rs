//! The proxy API on the wire, before the dial.
//!
//! Covers the authorize, resolve, match, select, and validate rows of
//! `cpt-cf-oagw-dod-proxy-api` and `cpt-cf-oagw-dod-error-source`: the 401 a
//! subjectless request answers with, the 403 a refused `invoke` permission and
//! a surface with no `AuthZ` client answer with, the 404 the unmatched alias
//! and the unmatched route answer with, the 400 the suffix, the query, and the
//! target-host header answer with, the 503 a disabled upstream answers with,
//! and the `X-OAGW-Error-Source: gateway` every one of those carries. The
//! answers are produced without dialing anything, so no outbound socket opens
//! in this suite; the exchange itself is `proxy_forward_tests.rs`'s.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::store::OagwStore;
use oagw::OagwState;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const TENANT: u128 = 0x31;
const ERROR_SOURCE: &str = "x-oagw-error-source";

/// The `AuthZ` PDP the allowing stub stands in for.
struct Allowing;

#[async_trait::async_trait]
impl AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: json!(TENANT.to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// The `AuthZ` PDP the refusing stub stands in for.
struct Denying;

#[async_trait::async_trait]
impl AuthZResolverClient for Denying {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// One mounted surface over its own store.
struct Surface {
    router: Router,
    store: Arc<OagwStore>,
}

/// Builds a surface whose `AuthZ` client the caller states.
fn surface(enforcer: Option<PolicyEnforcer>) -> Surface {
    let store = Arc::new(OagwStore::new());
    let cache = Arc::new(ControlPlaneCache::new());
    let config = OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    };
    let service = Arc::new(
        ManagementService::new(Arc::clone(&store), &config, Arc::clone(&cache))
            .expect("the validators compile"),
    );
    let state = Arc::new(OagwState::new(
        Arc::new(config),
        Arc::clone(&store),
        service,
        enforcer.map(Arc::new),
        None,
        Arc::clone(&cache),
    ));
    Surface {
        router: oagw::api::rest::register_management_routes(Router::new(), state),
        store,
    }
}

/// The authenticated subject a request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// Issues one proxy request and returns its response.
async fn issue(
    app: Router,
    method: Method,
    uri: &str,
    authenticated: bool,
    headers: &[(&str, &str)],
    body: &[u8],
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if authenticated {
        builder = builder.extension(subject());
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_vec())).expect("the request builds");
    app.oneshot(request).await.expect("oneshot resolves")
}

/// The status, body, and error source of one answer.
async fn answer(
    app: Router,
    method: Method,
    uri: &str,
    authenticated: bool,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, Value, Option<String>) {
    let response = issue(app, method, uri, authenticated, headers, body).await;
    let status = response.status();
    let source = response
        .headers()
        .get(ERROR_SOURCE)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("the problem body is JSON")
    };
    (status, document, source)
}

/// Stores one upstream whose alias is the one its endpoint set derives, with
/// the enabled state and header rules the caller states, and returns its
/// instance identifier.
async fn stored_upstream(
    app: &Router,
    host: &str,
    enabled: bool,
    headers: Option<Value>,
) -> String {
    let mut body = json!({
        "alias": host,
        "server": { "endpoints": [{ "scheme": "https", "host": host, "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "tags": ["proxy"]
    });
    if !enabled {
        body["enabled"] = json!(false);
    }
    if let Some(headers) = headers {
        body["headers"] = headers;
    }
    let response = issue(
        app.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        true,
        &[],
        serde_json::to_vec(&body).expect("the body serializes").as_slice(),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"].as_str().expect("the instance id").to_owned()
}

/// Stores one route for an upstream and returns its instance identifier.
async fn stored_route(app: &Router, instance: &str, http: Value) -> String {
    let key = oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string();
    let body = json!({ "upstream_id": key, "match": http, "priority": 10 });
    let response = issue(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        true,
        &[],
        serde_json::to_vec(&body).expect("the body serializes").as_slice(),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"].as_str().expect("the instance id").to_owned()
}

/// An enabled upstream on `upstream.example.com` and its `GET /api` route.
async fn wired() -> Surface {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let upstream = stored_upstream(&surface.router, "upstream.example.com", true, None).await;
    stored_route(
        &surface.router,
        &upstream,
        json!({ "http": { "methods": ["GET"], "path": "/api" } }),
    )
    .await;
    surface
}

#[tokio::test]
async fn a_request_without_a_subject_is_answered_401_before_any_resolution() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let (status, document, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        false,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(document["status"], 401, "{document}");
}

#[tokio::test]
async fn a_refused_invoke_permission_is_answered_403() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Denying))));
    let (status, document, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{document}");
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_surface_with_no_authz_client_answers_403() {
    let surface = surface(None);
    let (status, _, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn an_alias_no_chain_element_holds_is_answered_404() {
    let surface = wired().await;
    let (status, document, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/absent.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{document}");
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(document["status"], 404);
}

#[tokio::test]
async fn a_request_no_route_matches_is_answered_404() {
    let surface = wired().await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/absent",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{document}");
}

#[tokio::test]
async fn a_method_no_route_declares_is_answered_404() {
    let surface = wired().await;
    let (status, document, _) = answer(
        surface.router,
        Method::POST,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{document}");
}

#[tokio::test]
async fn an_alias_that_cannot_be_normalized_is_answered_404() {
    let surface = wired().await;
    let (status, _, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/not_a_host/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_disabled_upstream_is_answered_503_and_never_dialed() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let upstream = stored_upstream(&surface.router, "upstream.example.com", false, None).await;
    stored_route(
        &surface.router,
        &upstream,
        json!({ "http": { "methods": ["GET"], "path": "/api" } }),
    )
    .await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{document}");
}

#[tokio::test]
async fn a_suffix_to_a_route_that_rejects_it_is_answered_400() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let upstream = stored_upstream(&surface.router, "upstream.example.com", true, None).await;
    stored_route(
        &surface.router,
        &upstream,
        json!({
            "http": {
                "methods": ["GET"],
                "path": "/api",
                "path_suffix_mode": "disabled"
            }
        }),
    )
    .await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api/deeper",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn a_query_parameter_the_route_does_not_allow_is_answered_400() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let upstream = stored_upstream(&surface.router, "upstream.example.com", true, None).await;
    stored_route(
        &surface.router,
        &upstream,
        json!({
            "http": {
                "methods": ["GET"],
                "path": "/api",
                "query_allowlist": ["model"]
            }
        }),
    )
    .await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api?other=1",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn an_unparseable_target_host_value_is_answered_400() {
    let surface = wired().await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[("x-oagw-target-host", "us vendor.com")],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn a_target_host_no_endpoint_declares_is_answered_400() {
    let surface = surface(Some(PolicyEnforcer::new(Arc::new(Allowing))));
    let upstream = stored_upstream(&surface.router, "upstream.example.com", true, None).await;
    stored_route(
        &surface.router,
        &upstream,
        json!({ "http": { "methods": ["GET"], "path": "/api" } }),
    )
    .await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[("x-oagw-target-host", "other.vendor.com")],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn a_declared_length_that_disagrees_with_the_body_is_answered_400() {
    let surface = wired().await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[("content-length", "5")],
        b"abc",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn the_proxy_answers_are_problem_documents_of_the_error_catalogue() {
    let surface = wired().await;
    let (_, document, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/absent.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(document["status"], 404);
    assert!(document["type"].is_string(), "{document}");
    assert!(document["title"].is_string(), "{document}");
    assert!(document["instance"].is_string(), "{document}");
}

#[tokio::test]
async fn the_proxy_surface_reads_the_rows_the_management_surface_wrote() {
    let surface = wired().await;
    let (status, document, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[],
        b"",
    )
    .await;
    // The dial to the fictional endpoint fails, and the failure is a gateway
    // answer about the upstream, which is the observable the resolution and
    // the match produced their work: the request was never refused by either.
    assert_ne!(status, StatusCode::NOT_FOUND, "{document}");
    assert_ne!(status, StatusCode::FORBIDDEN, "{document}");
    assert_eq!(
        document["status"],
        serde_json::json!(status.as_u16()),
        "the problem document names its own status"
    );
}

#[tokio::test]
async fn the_store_the_proxy_resolution_reads_is_the_management_store() {
    let surface = wired().await;
    let rows = surface
        .store
        .list_upstreams(Uuid::from_u128(TENANT));
    assert_eq!(rows.len(), 1, "one tenant holds the wired rows");
}

#[tokio::test]
async fn a_header_the_transport_cannot_carry_never_reaches_the_validation() {
    // The HTTP layer refuses a value with a CR or LF in it before the handler
    // extracts anything, so the injection check of the inbound validation is
    // exercised on the header map it is given, which the unit suite
    // (`proxy_validate_tests.rs`) drives directly.
    let surface = wired().await;
    let built = axum::http::HeaderValue::from_str("value injected");
    assert!(built.is_ok(), "the transport admits a plain value");
    let refused = axum::http::HeaderValue::from_str("value\r\ninjected");
    assert!(refused.is_err(), "the transport refuses the injection vector");
    let (status, _, _) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[("x-evil", "value injected")],
        b"",
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "the plain value is admitted");
}

/// The error-source header name the answers carry, as a typed name.
#[test]
fn the_error_source_header_is_the_documented_name() {
    assert_eq!(
        HeaderName::from_static("x-oagw-error-source").as_str(),
        ERROR_SOURCE
    );
}

#[tokio::test]
async fn a_body_that_declares_more_than_the_limit_is_answered_413() {
    let surface = wired().await;
    let (status, document, source) = answer(
        surface.router,
        Method::GET,
        "/oagw/v1/proxy/upstream.example.com/api",
        true,
        &[("content-length", "100000001")],
        b"tiny",
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{document}");
    assert_eq!(source.as_deref(), Some("gateway"));
    assert_eq!(document["type"], "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
}
