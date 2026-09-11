//! The CORS feature on the wire.
//!
//! Covers `cpt-cf-oagw-dod-cors-preflight`, `cpt-cf-oagw-dod-cors-enforcement`,
//! `cpt-cf-oagw-dod-cors-origin-matching`, `cpt-cf-oagw-dod-cors-headers`, and
//! `cpt-cf-oagw-dod-cors-tests` over the mounted proxy surface: the 204 a
//! preflight is answered with and the header set it carries, its independence
//! from resolution and from the permission check, the hand-back of an
//! `OPTIONS` request that is not a preflight, the decoration of an admitted
//! actual request, the two bare 403 problem bodies with their GTS types and
//! titles, the exact origin matching ADR 0004 demonstrates, the wildcard, the
//! credentials restriction, the empty allowlist, the route-level fold, and the
//! absence of any CORS header on a request with no `Origin`. The upstream is a
//! minimal HTTP/1.1 echo listener, and a counter on it proves a refused
//! request reached nothing.

// @cpt-dod:cpt-cf-oagw-dod-cors-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
const TENANT: u128 = 0x51;
const HOST: &str = "127.0.0.1";
const ERROR_SOURCE: &str = "x-oagw-error-source";
const ORIGIN_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
const METHOD_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
const ROUTE_NOT_FOUND_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

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

/// One mounted proxy surface over its own store and echo upstream.
struct Surface {
    router: Router,
    /// The requests the echo upstream received, which a refusal must leave at
    /// zero.
    received: Arc<AtomicUsize>,
}

/// The authenticated subject a proxy request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// Starts one echo upstream that counts the requests it receives.
async fn upstream() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((HOST, 0)).await.expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    let received = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&received);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                while buffer.len() <= 128 * 1024 {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                            x-upstream-marker: probe\r\ncontent-length: 2\r\n\
                            connection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(b"{}").await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, received)
}

/// Mounts one surface whose upstream carries the CORS object the caller
/// states, with an optional route-level object beside it.
async fn wired(upstream_cors: Option<Value>, route_cors: Option<Value>) -> Surface {
    let (port, received) = upstream().await;
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
        Some(Arc::new(PolicyEnforcer::new(Arc::new(Allowing)))),
        None,
        Arc::clone(&cache),
    ));
    let router = oagw::api::rest::register_management_routes(Router::new(), state);

    let mut body = json!({
        "alias": HOST,
        "server": { "endpoints": [{ "scheme": "http", "host": HOST, "port": port }] },
        "protocol": HTTP_PROTOCOL
    });
    if let Some(cors) = upstream_cors {
        body["cors"] = cors;
    }
    let upstream_instance = created(&router, Method::POST, "/oagw/v1/upstreams", &body).await;

    let mut route = json!({
        "upstream_id": key_of(&upstream_instance),
        "match": { "http": { "methods": ["GET", "POST", "DELETE"], "path": "/api" } },
        "priority": 10
    });
    if let Some(cors) = route_cors {
        route["cors"] = cors;
    }
    created(&router, Method::POST, "/oagw/v1/routes", &route).await;
    Surface { router, received }
}

/// The `upstream_id` key the route create body names its upstream by.
fn key_of(instance: &str) -> String {
    oagw::gts::parse_gts_instance(oagw::UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string()
}

/// Issues one create and returns the instance identifier of the row.
async fn created(app: &Router, method: Method, path: &str, body: &Value) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .extension(subject())
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("the request builds"),
        )
        .await
        .expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document: Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"].as_str().expect("the instance id").to_owned()
}

/// Issues one proxy request and returns its status, headers, body, and the
/// error source it was tagged with.
struct Answer {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Value,
    source: Option<String>,
}

async fn issue(
    surface: &Surface,
    method: Method,
    uri: &str,
    authenticated: bool,
    headers: &[(&str, &str)],
) -> Answer {
    let mut builder = Request::builder().method(method).uri(uri);
    if authenticated {
        builder = builder.extension(subject());
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = surface
        .router
        .clone()
        .oneshot(
            builder
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("oneshot resolves");
    let status = response.status();
    let source = response
        .headers()
        .get(ERROR_SOURCE)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let pairs: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("the body is JSON")
    };
    Answer {
        status,
        headers: pairs,
        body,
        source,
    }
}

/// One header value of an answer, matched case-insensitively.
fn header_of(answer: &Answer, name: &str) -> Option<String> {
    let lowered = name.to_ascii_lowercase();
    answer
        .headers
        .iter()
        .find(|(header, _)| *header == lowered)
        .map(|(_, value)| value.clone())
}

/// A preflight is answered 204 with the header set ADR 0004's example spells.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_is_answered_204_with_the_full_header_set() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "Content-Type, Authorization"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&answer, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header_of(&answer, "access-control-allow-methods").as_deref(),
        Some("POST")
    );
    assert_eq!(
        header_of(&answer, "access-control-allow-headers").as_deref(),
        Some("Content-Type, Authorization")
    );
    assert_eq!(header_of(&answer, "access-control-max-age").as_deref(), Some("86400"));
    assert_eq!(header_of(&answer, "vary").as_deref(), Some(PREFLIGHT_VARY));
    assert_eq!(answer.source.as_deref(), Some("gateway"));
    assert_eq!(surface.received.load(Ordering::SeqCst), 0);
}

/// A preflight that names no request header omits the header answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_without_requested_headers_omits_the_header_answer() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert!(header_of(&answer, "access-control-allow-headers").is_none());
    assert!(header_of(&answer, "access-control-allow-credentials").is_none());
    assert!(header_of(&answer, "access-control-expose-headers").is_none());
}

/// A preflight for an alias that does not resolve is answered the same 204.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_for_an_alias_that_does_not_resolve_is_answered_the_same() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/nobody-uses-this.test/api",
        true,
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&answer, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header_of(&answer, "access-control-allow-methods").as_deref(),
        Some("POST")
    );
}

/// A preflight sent without a bearer token is answered 204 and not 401.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_without_a_bearer_token_is_answered_204() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/127.0.0.1/api",
        false,
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert_eq!(answer.source.as_deref(), Some("gateway"));
}

/// A preflight whose requested method the configuration would refuse, and one
/// for an upstream whose CORS family is disabled, are both answered the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_is_answered_the_same_whatever_the_configuration_says() {
    let surface = wired(
        Some(cors_object(false, &["https://app.example.com"])),
        None,
    )
    .await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[
            ("origin", "https://evil.com"),
            ("access-control-request-method", "DELETE"),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&answer, "access-control-allow-origin").as_deref(),
        Some("https://evil.com")
    );
    assert_eq!(
        header_of(&answer, "access-control-allow-methods").as_deref(),
        Some("DELETE")
    );
}

/// An `OPTIONS` request that is not a preflight is handed back to the proxy
/// path, which matches no route under the shipped method enum.
#[tokio::test(flavor = "multi_thread")]
async fn an_options_request_that_is_not_a_preflight_is_handed_back() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::OPTIONS,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.body["type"], json!(ROUTE_NOT_FOUND_TYPE));
    assert_eq!(answer.source.as_deref(), Some("gateway"));
}

/// An admitted actual cross-origin request is forwarded and decorated.
#[tokio::test(flavor = "multi_thread")]
async fn an_admitted_cross_origin_request_is_forwarded_and_decorated() {
    let surface = wired(
        Some(json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["X-Request-ID"],
            "allow_credentials": true
        })),
        None,
    )
    .await;
    let answer = issue(
        &surface,
        Method::POST,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        header_of(&answer, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(header_of(&answer, "vary").as_deref(), Some("Origin"));
    assert_eq!(
        header_of(&answer, "access-control-allow-credentials").as_deref(),
        Some("true")
    );
    assert_eq!(
        header_of(&answer, "access-control-expose-headers").as_deref(),
        Some("X-Request-ID")
    );
    assert!(header_of(&answer, "access-control-allow-methods").is_none());
    assert!(header_of(&answer, "access-control-max-age").is_none());
    assert_eq!(answer.source.as_deref(), Some("upstream"));
}

/// A disallowed origin is refused 403 before anything is forwarded.
#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_origin_is_refused_403_before_forwarding() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://evil.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.body["type"], json!(ORIGIN_TYPE));
    assert_eq!(answer.body["title"], json!("CORS Origin Not Allowed"));
    assert_eq!(answer.body["status"], json!(403));
    assert_eq!(
        answer.body["detail"],
        json!("Origin 'https://evil.com' not in allowed origins list")
    );
    assert_eq!(header_of(&answer, "vary").as_deref(), Some("Origin"));
    assert_eq!(answer.source.as_deref(), Some("gateway"));
    assert_eq!(surface.received.load(Ordering::SeqCst), 0);
}

/// A disallowed method is refused 403 with the method type.
#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_method_is_refused_403_with_the_method_type() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::DELETE,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.body["type"], json!(METHOD_TYPE));
    assert_eq!(answer.body["title"], json!("CORS Method Not Allowed"));
    assert_eq!(
        answer.body["detail"],
        json!("Method 'DELETE' not in allowed methods list")
    );
    assert_eq!(surface.received.load(Ordering::SeqCst), 0);
}

/// An origin and a method that are both disallowed are answered with the
/// origin reason, and the body names no allowed method.
#[tokio::test(flavor = "multi_thread")]
async fn an_origin_and_a_method_both_disallowed_are_answered_with_the_origin_reason() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::DELETE,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://evil.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.body["type"], json!(ORIGIN_TYPE));
    assert!(
        !answer
            .body["detail"]
            .as_str()
            .expect("the detail")
            .contains("DELETE"),
        "the origin refusal names no method"
    );
}

/// An origin that differs only in port, scheme, case, or a trailing slash is
/// refused, and no suffix admits a lookalike host.
#[tokio::test(flavor = "multi_thread")]
async fn an_origin_that_differs_in_any_part_is_refused() {
    for origin in [
        "https://app.example.com:8080",
        "http://app.example.com",
        "HTTPS://APP.EXAMPLE.COM",
        "https://app.example.com/",
        "https://app.example.com:443",
        "https://evil.com.example.com",
    ] {
        let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
        let answer = issue(
            &surface,
            Method::GET,
            "/oagw/v1/proxy/127.0.0.1/api",
            true,
            &[("origin", origin)],
        )
        .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{origin} is refused");
        assert_eq!(answer.body["type"], json!(ORIGIN_TYPE), "{origin}");
        assert_eq!(surface.received.load(Ordering::SeqCst), 0, "{origin} forwarded nothing");
    }
}

/// A wildcard admits any origin and echoes the origin the request sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_wildcard_admits_any_origin_and_echoes_it() {
    let surface = wired(Some(cors_object(true, &["*"])), None).await;
    let answer = issue(
        &surface,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://anywhere.test")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        header_of(&answer, "access-control-allow-origin").as_deref(),
        Some("https://anywhere.test")
    );
}

/// Credentials are emitted exactly when the configuration allows them.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_are_emitted_exactly_when_the_configuration_allows_them() {
    let credentialed = wired(
        Some(json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allow_credentials": true
        })),
        None,
    )
    .await;
    let answer = issue(
        &credentialed,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(
        header_of(&answer, "access-control-allow-credentials").as_deref(),
        Some("true")
    );

    let plain = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &plain,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(header_of(&answer, "access-control-allow-credentials").is_none());
}

/// An enabled family with no origin at all refuses every origin and forwards
/// nothing, and is not read as a disabled family.
#[tokio::test(flavor = "multi_thread")]
async fn an_enabled_family_with_no_origin_refuses_every_origin() {
    for cors in [
        json!({ "enabled": true, "allowed_methods": ["GET"] }),
        json!({ "enabled": true, "allowed_origins": [], "allowed_methods": ["GET"] }),
    ] {
        let surface = wired(Some(cors), None).await;
        let answer = issue(
            &surface,
            Method::GET,
            "/oagw/v1/proxy/127.0.0.1/api",
            true,
            &[("origin", "https://app.example.com")],
        )
        .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.body["type"], json!(ORIGIN_TYPE));
        assert_eq!(surface.received.load(Ordering::SeqCst), 0);
    }
}

/// A disabled family, and a resource that declares no `cors` object at all,
/// enforce nothing and decorate nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_or_absent_family_enforces_nothing() {
    for cors in [None, Some(json!({ "enabled": false, "allowed_origins": ["*"] }))] {
        let surface = wired(cors.clone(), None).await;
        let answer = issue(
            &surface,
            Method::GET,
            "/oagw/v1/proxy/127.0.0.1/api",
            true,
            &[("origin", "https://evil.com")],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{cors:?} forwards");
        assert!(header_of(&answer, "access-control-allow-origin").is_none());
        assert!(header_of(&answer, "vary").is_none());
    }
}

/// A route-level `cors` object overrides the upstream's for the members it
/// declares.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_level_cors_object_overrides_the_upstream_s() {
    let surface = wired(
        Some(cors_object(true, &["https://app.example.com"])),
        Some(cors_object(true, &["https://admin.example.com"])),
    )
    .await;
    let admitted = issue(
        &surface,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://admin.example.com")],
    )
    .await;
    assert_eq!(admitted.status, StatusCode::OK);
    let refused = issue(
        &surface,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[("origin", "https://app.example.com")],
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

/// A request that carries no `Origin` header is forwarded with no CORS header
/// of any kind on its response.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_without_an_origin_header_carries_no_cors_header() {
    let surface = wired(Some(cors_object(true, &["https://app.example.com"])), None).await;
    let answer = issue(
        &surface,
        Method::GET,
        "/oagw/v1/proxy/127.0.0.1/api",
        true,
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(header_of(&answer, "access-control-allow-origin").is_none());
    assert!(header_of(&answer, "vary").is_none());
}

/// The `cors` object the callers state, with the shipped defaults.
fn cors_object(enabled: bool, origins: &[&str]) -> Value {
    json!({
        "enabled": enabled,
        "allowed_origins": origins,
        "allowed_methods": ["GET", "POST"]
    })
}
