//! Route-level behaviour exercised through the router the gear actually registers.
//!
//! Every other oagw suite calls the handlers or `match_route` directly. These tests go
//! the way a deployment does: build `ProxyService`, hand it to `api::register_routes`,
//! put the caller's [`SecurityContext`] on the router as a layer, and drive whole HTTP
//! requests through with `tower::ServiceExt::oneshot`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http::Method;
use httpmock::prelude::{HttpMockRequest, HttpMockResponse, MockServer};
use oagw::config::OagwConfig;
use oagw::proxy::ProxyService;
use oagw::ratelimit::{RateLimiter, SystemClock};
use oagw::security::NoopCredentialResolver;
use oagw::store::OagwStore;
use serde_json::{Value, json};
use tower::ServiceExt;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// What the mock upstream saw, so a test can assert on the relayed request.
#[derive(Default)]
struct Dialled {
    /// One `<method> <path>` plus body per request the upstream accepted.
    requests: Mutex<Vec<(String, String)>>,
    /// The `x-request-id` the relay sent, newest last.
    request_ids: Mutex<Vec<String>>,
    /// The `host` header the relay sent, newest last.
    hosts: Mutex<Vec<String>>,
    /// The routing headers the relay sent, newest last.
    routing: Mutex<Vec<String>>,
}

impl Dialled {
    /// Records one request's method, path and body.
    fn record(&self, request: &HttpMockRequest) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push((
                format!("{} {}", request.method_str(), request.uri().path()),
                request.body_string(),
            ));
        }
        if let Ok(mut ids) = self.request_ids.lock() {
            ids.push(
                request
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        if let Ok(mut hosts) = self.hosts.lock() {
            hosts.push(
                request
                    .headers()
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        if let Ok(mut routing) = self.routing.lock() {
            routing.push(
                request
                    .headers()
                    .get("x-oagw-target-host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
    }

    /// The `host` header values the upstream was given, newest last.
    fn hosts(&self) -> Vec<String> {
        self.hosts.lock().map(|seen| seen.clone()).unwrap_or_default()
    }

    /// The `x-oagw-target-host` values the upstream was given, newest last.
    fn routing(&self) -> Vec<String> {
        self.routing.lock().map(|seen| seen.clone()).unwrap_or_default()
    }

    /// Every recorded request line, newest last.
    fn seen(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|seen| seen.iter().map(|(line, _)| line.clone()).collect())
            .unwrap_or_default()
    }

    /// The bodies of every recorded request, newest last.
    fn bodies(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|seen| seen.iter().map(|(_, body)| body.clone()).collect())
            .unwrap_or_default()
    }

    /// The `x-request-id` values the upstream was given, newest last.
    fn request_ids(&self) -> Vec<String> {
        self.request_ids
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

/// A mock upstream that answers `200` with `reply` and records what it was asked.
///
/// The recorded log is the hit count as well: the mock accepts exactly the requests the
/// relay sends, and [`Dialled::record`] sees each one.
fn listening_upstream(reply: &'static str) -> (MockServer, Arc<Dialled>) {
    listening_upstream_with(reply, &[("content-type", "text/plain")])
}

/// The same upstream answering without the named headers, for the tests of a guard that
/// demands one the upstream does not send.
fn listening_upstream_with(
    reply: &'static str,
    headers: &'static [(&'static str, &'static str)],
) -> (MockServer, Arc<Dialled>) {
    let server = MockServer::start();
    let dialled = Arc::new(Dialled::default());
    let recorder = Arc::clone(&dialled);
    let headers: Vec<(String, String)> =
        headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
    server.mock(move |when, then| {
        when.any_request();
        let recorder = Arc::clone(&recorder);
        let headers = headers.clone();
        then.respond_with(move |request: &HttpMockRequest| {
            recorder.record(request);
            let mut response = HttpMockResponse::builder().status(200);
            for (name, value) in &headers {
                response = response.header(name.as_str(), value.as_str());
            }
            response.body(reply).build()
        });
    });
    (server, dialled)
}

/// A registry that records nothing: the router is under test, not the OpenAPI document.
struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A service built the way `gear.rs` builds it, over an empty store and no resolver.
fn service(config: OagwConfig) -> Arc<ProxyService> {
    Arc::new(ProxyService::new(
        Arc::new(OagwStore::new()),
        Arc::new(NoopCredentialResolver),
        Arc::new(oagw::plugins::token_cache::TokenCache::new(16, std::time::Duration::from_mins(1))),
        Arc::new(RateLimiter::new(Arc::new(SystemClock))),
        config,
        None,
        oagw::proxy::build_client().expect("the outbound client builds"),
    ))
}

/// The router the gear registers, with the caller's context layered onto it.
///
/// In a deployment the platform's authentication layer supplies that extension; the tests
/// stand in for it so the request travels the same code path a real one does.
fn router(svc: Arc<ProxyService>) -> Router {
    let tenant = uuid::Uuid::new_v4();
    let ctx = SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous());
    oagw::api::register_routes(Router::new(), &NoopOpenApiRegistry, svc)
        .layer(axum::Extension(ctx))
}

/// A request carrying a JSON body.
fn json_request(method: Method, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A request with no body.
fn bare_request(method: Method, uri: &str) -> Request<Body> {
    Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()
}

/// Sends a request through the router and returns the status, headers and body.
async fn send(
    router: &Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::String(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    };
    (status, headers, body)
}

/// An upstream definition the gear accepts, pointed at `host` and `port` over `http`.
fn upstream_body(alias: &str, host: &str, port: u16) -> Value {
    json!({
        "alias": alias,
        "protocol": oagw::types::HTTP_PROTOCOL_TYPE,
        "server": {
            "endpoints": [
                {"scheme": "http", "host": host, "port": port}
            ]
        }
    })
}

/// A route on `upstream` forwarding `path`.
fn route_body(upstream: &str, path: &str, methods: &[&str]) -> Value {
    json!({
        "upstream_id": upstream,
        "match": {"http": {"path": path, "methods": methods}},
        "priority": 1
    })
}

/// The upstream the router created from `body`, read back through the router itself.
async fn create_upstream(router: &Router, body: &Value) -> Value {
    let (status, _, created) = send(router, json_request(Method::POST, "/oagw/v1/upstreams", body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "the router creates the upstream: {created}");
    created
}

/// The route the router created from `body`.
async fn create_route(router: &Router, body: &Value) -> Value {
    let (status, _, created) = send(router, json_request(Method::POST, "/oagw/v1/routes", body)).await;
    assert_eq!(status, StatusCode::CREATED, "the router creates the route: {created}");
    created
}

/// Management CRUD travels the registered router and answers with the documented codes.
#[tokio::test]
async fn management_crud_travels_the_registered_router() {
    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("echo.local", "echo.example.com", 8443),
    )
    .await;
    let id = created["id"].as_str().expect("the created upstream carries an id");
    assert_eq!(created["alias"], "echo.local");

    let (status, _, list) = send(&router, bare_request(Method::GET, "/oagw/v1/upstreams")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1, "the upstream created above is the only one: {list}");
    assert_eq!(list["items"][0]["id"], *id);

    let (status, _, read) =
        send(&router, bare_request(Method::GET, &format!("/oagw/v1/upstreams/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["alias"], "echo.local");

    let (status, _, updated) = send(
        &router,
        json_request(
            Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            &upstream_body("irrelevant.local", "echo.example.com", 8443),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the router replaces the upstream: {updated}");
    // The alias is the lookup key, so a replacement keeps it.
    assert_eq!(updated["alias"], "echo.local");

    let (status, _, _) =
        send(&router, bare_request(Method::DELETE, &format!("/oagw/v1/upstreams/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) =
        send(&router, bare_request(Method::GET, &format!("/oagw/v1/upstreams/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the router deleted it");
}

/// A proxied request driven through the router reaches the live upstream, which is the
/// whole point of the gear: the router hands the call to the relay and the relay dials.
#[tokio::test]
async fn a_proxied_request_is_relayed_to_the_upstream_through_the_router() {
    let expected = "the upstream spoke";
    let (upstream_server, dialled) = listening_upstream(expected);

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("probe.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    let route = create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;
    assert!(
        route["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the router bound the route to the upstream: {route}"
    );

    let (status, _, body) = send(
        &router,
        json_request(
            Method::POST,
            "/oagw/v1/proxy/probe.local/v1/echo",
            &json!({"ping": true}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, expected);
    assert_eq!(dialled.seen(), vec!["POST /v1/echo"], "the relay dialled once");
    assert!(
        dialled.bodies().first().is_some_and(|body| body.contains("ping")),
        "the relay forwarded the body unchanged: {:?}",
        dialled.bodies()
    );
}

/// A refusal the gateway takes before dialling: the proxy answers from the router itself,
/// names itself as the error source, and the upstream is never contacted.
#[tokio::test]
async fn a_request_the_gateway_refuses_never_reaches_the_upstream() {
    let (upstream_server, dialled) = listening_upstream("must not be reached");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("loopback.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, headers, body) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/loopback.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a loopback upstream is refused by the SSRF policy: {body}"
    );
    assert_eq!(
        headers[http::HeaderName::from_static("x-oagw-error-source")],
        "gateway",
        "the gateway names itself as the source"
    );
    assert!(
        body["type"].as_str().is_some_and(|kind| kind.ends_with("validation.error.v1")),
        "the refusal carries the canonical problem type: {body}"
    );
    assert!(dialled.seen().is_empty(), "the gateway refused before dialling");
}

/// An alias the caller's tenant chain cannot resolve is a `404` naming the gateway, not a
/// dial to some other tenant's upstream.
#[tokio::test]
async fn an_unknown_alias_is_reported_by_the_router() {
    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let (status, headers, body) = send(
        &router,
        bare_request(Method::GET, "/oagw/v1/proxy/nobody.local/v1/echo"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(headers[http::HeaderName::from_static("x-oagw-error-source")], "gateway");
    assert!(
        body["type"].as_str().is_some_and(|kind| kind.ends_with("route.not_found.v1")),
        "the problem type names the unresolved alias: {body}"
    );
}

/// A path no route on the upstream names is a `404` from the router, whatever the upstream
/// itself would have answered (US2/AC3).
#[tokio::test]
async fn an_unrouted_path_is_not_relayed() {
    let (upstream_server, dialled) = listening_upstream("must not be reached");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("probe.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, _, body) = send(
        &router,
        bare_request(Method::GET, "/oagw/v1/proxy/probe.local/v1/elsewhere"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(dialled.seen().is_empty(), "nothing is dialled when no route matches");
}

/// An upstream definition with plugin bindings, for the tests of the plugin phases.
fn bound_upstream_body(alias: &str, host: &str, port: u16, plugins: Value) -> Value {
    let mut body = upstream_body(alias, host, port);
    body["plugins"] = plugins;
    body
}

/// FR-017's response half, end to end: an answer that omits a header the binding demands
/// is refused with a `502` naming the gateway, not relayed to the caller.
#[tokio::test]
async fn an_answer_missing_a_required_header_is_refused_through_the_router() {
    let (upstream_server, dialled) = listening_upstream_with("no content type here", &[]);

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &bound_upstream_body(
            "guarded.local",
            &upstream_server.host(),
            upstream_server.port(),
            json!([
                {
                    "name": "required_headers",
                    "config": {"required_response_headers": "content-type"}
                }
            ]),
        ),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, headers, body) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/guarded.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "the guard refuses the upstream's answer: {body}"
    );
    assert_eq!(headers[http::HeaderName::from_static("x-oagw-error-source")], "gateway");
    assert!(
        body["type"].as_str().is_some_and(|kind| kind.ends_with("downstream.error.v1")),
        "the problem type names the downstream fault: {body}"
    );
    assert!(body["detail"].as_str().is_some_and(|d| d.contains("content-type")), "{body}");
    assert_eq!(dialled.seen().len(), 1, "the upstream was dialled once");
}

/// FR-018 end to end: a caller who sent no identifier is handed the one the gateway
/// generated, which is also the one the upstream saw.
#[tokio::test]
async fn the_caller_is_handed_the_identifier_the_request_was_given() {
    let (upstream_server, dialled) = listening_upstream("relayed");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &bound_upstream_body(
            "traced.local",
            &upstream_server.host(),
            upstream_server.port(),
            json!([{"name": "request_id", "config": {}}]),
        ),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, headers, _) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/traced.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let handed = headers["x-request-id"].to_str().expect("a plain value").to_owned();
    assert!(handed.starts_with("req_"), "the response carries a generated id: {handed}");
    assert_eq!(
        dialled.request_ids(),
        vec![handed],
        "the caller is handed the identifier the upstream saw"
    );
}

/// A caller who supplied their own identifier keeps it on the way out.
#[tokio::test]
async fn a_caller_supplied_identifier_travels_both_ways() {
    let (upstream_server, dialled) = listening_upstream("relayed");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &bound_upstream_body(
            "traced.local",
            &upstream_server.host(),
            upstream_server.port(),
            json!([{"name": "request_id", "config": {}}]),
        ),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/traced.local/v1/echo")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::HeaderName::from_static("x-request-id"), "mine-1234")
        .body(Body::from(r#"{"a":1}"#))
        .unwrap();
    let (status, headers, _) = send(&router, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-request-id"], "mine-1234");
    assert_eq!(dialled.request_ids(), vec!["mine-1234"]);
}

/// FR-019 end to end: an upstream carrying a limit throttles the caller, answers `429`
/// with the quota headers, and has dialled the upstream only for the admitted request.
#[tokio::test]
async fn a_throttled_upstream_answers_429_with_its_quota() {
    let (upstream_server, dialled) = listening_upstream("relayed");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let mut created = upstream_body("capped.local", &upstream_server.host(), upstream_server.port());
    created["rate_limit"] = json!({
        "algorithm": "token_bucket",
        "sustained": {"rate": 1, "window": "minute"},
        "strategy": "reject"
    });
    let created = create_upstream(&router, &created).await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let uri = "/oagw/v1/proxy/capped.local/v1/echo";
    let (first, _, _) = send(&router, json_request(Method::POST, uri, &json!({}))).await;
    assert_eq!(first, StatusCode::OK, "the first call is inside the limit");

    let (status, headers, body) = send(&router, json_request(Method::POST, uri, &json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.get("retry-after").is_some(), "the throttle names when it refills");
    assert!(headers.get("x-ratelimit-limit").is_some(), "the throttle names its capacity");
    assert!(headers.get("x-ratelimit-remaining").is_some(), "the throttle names what is left");
    assert!(
        headers[http::HeaderName::from_static("x-oagw-error-source")] == "gateway",
        "a throttle is the gateway's own answer"
    );
    assert_eq!(dialled.seen().len(), 1, "the refused call never reached the upstream");
}

/// FR-028: an upstream that is switched off is refused with `503` before any route is
/// consulted, and the caller is told the gateway made that decision.
#[tokio::test]
async fn a_disabled_upstream_is_refused_with_503() {
    let (upstream_server, dialled) = listening_upstream("must not be reached");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("offline.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    create_route(&router, &route_body(&id, "/v1/echo", &["POST"])).await;

    // The upstream exists and resolves, so the refusal is about its `enabled` flag.
    let mut switched_off = upstream_body(
        "offline.local",
        &upstream_server.host(),
        upstream_server.port(),
    );
    switched_off["enabled"] = json!(false);
    let (status, _, updated) = send(
        &router,
        json_request(Method::PUT, &format!("/oagw/v1/upstreams/{id}"), &switched_off),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["enabled"], false);

    let (status, headers, body) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/offline.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        headers[http::HeaderName::from_static("x-oagw-error-source")],
        "gateway",
        "the gateway refuses its own upstream"
    );
    assert!(
        body["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("link.unavailable.v1")),
        "the refusal names the upstream as unreachable: {body}"
    );
    assert!(
        dialled.seen().is_empty(),
        "a disabled upstream is never dialled"
    );
}

/// FR-009: the endpoint field accepts `http` whatever the policy says, but a policy that
/// forbids plaintext upstreams refuses the dial at proxy time with `503`.
#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_the_policy_forbids_it() {
    let (upstream_server, dialled) = listening_upstream("cleartext answer");

    let svc = service(OagwConfig {
        allow_http_upstream: false,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    // Creation succeeds: the scheme is legal to declare, the dial is what is governed.
    let created = create_upstream(
        &router,
        &upstream_body("plain.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, headers, body) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/plain.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        headers[http::HeaderName::from_static("x-oagw-error-source")],
        "gateway"
    );
    assert!(
        body["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("link.unavailable.v1")),
        "the refusal names the plaintext policy: {body}"
    );
    assert!(
        body["detail"].as_str().is_some_and(|detail| detail.contains("http")),
        "the refusal names the scheme it would have dialled: {body}"
    );
    assert!(dialled.seen().is_empty(), "no cleartext connection was made");
}

/// The same upstream dials when the gear policy allows plaintext, so the refusal above is
/// about the policy and not about the scheme the endpoint declared.
#[tokio::test]
async fn a_plaintext_upstream_dials_when_the_policy_allows_it() {
    let (upstream_server, dialled) = listening_upstream("cleartext answer");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("clear.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let (status, _, body) = send(
        &router,
        json_request(Method::POST, "/oagw/v1/proxy/clear.local/v1/echo", &json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(dialled.seen(), vec!["POST /v1/echo"]);
}

/// FR-029: a problem document names the trace, so the caller can follow the same
/// identifier the log record and the relayed response carry.
#[tokio::test]
async fn a_problem_document_names_the_trace_id() {
    let (upstream_server, dialled) = listening_upstream("must not be reached");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("traced.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    // An endpoint the upstream does not configure is refused before a dial, which makes
    // the refusal a gateway-generated error document.
    let refused = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/traced.local/v1/echo")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::HeaderName::from_static("x-request-id"), "caller-trace-9")
        .header(http::HeaderName::from_static("x-oagw-target-host"), "nonesuch.example.com")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, body) = send(&router, refused).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["context"]["trace_id"], "caller-trace-9",
        "the caller's own trace reaches the problem document: {body}"
    );
    assert!(dialled.seen().is_empty(), "the refusal happened before dialling");

    // A request nothing settled an identifier for names no trace at all, rather than an
    // empty string pretending to be one.
    let anonymous = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/traced.local/v1/echo")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::HeaderName::from_static("x-oagw-target-host"), "nonesuch.example.com")
        .body(Body::from("{}"))
        .unwrap();
    let (_, _, body) = send(&router, anonymous).await;
    assert!(
        body["context"].get("trace_id").is_none(),
        "an unset trace is not named: {body}"
    );
}

/// A refusal the gateway takes before a route was matched is still observed: it counts as
/// a request, names its error type and carries the trace, where before it vanished
/// between the relay's early returns.
#[tokio::test]
async fn a_refusal_before_a_route_is_matched_is_still_recorded() {
    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let router = router(Arc::clone(&svc));

    let (status, _, body) = send(
        &router,
        bare_request(Method::GET, "/oagw/v1/proxy/nobody.local/v1/items"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["context"].get("trace_id").is_none(),
        "an unset trace is not named: {body}"
    );

    let recorded: Vec<(String, u64)> = svc
        .metrics()
        .counters()
        .into_iter()
        .filter(|(key, _)| key.contains("requests_total"))
        .collect();
    assert!(
        recorded.iter().any(|(key, value)| *value >= 1 && key.contains("nobody.local")),
        "the refused request is counted where the log record puts it: {recorded:?}"
    );
    let errors: Vec<(String, u64)> = svc
        .metrics()
        .counters()
        .into_iter()
        .filter(|(key, _)| key.contains("errors_total"))
        .collect();
    assert!(
        errors
            .iter()
            .any(|(key, _)| key.contains("route.not_found.v1") && key.contains("nobody.local")),
        "the refusal names its error type: {errors:?}"
    );
}

/// TR-07 end to end: what the upstream actually receives is the rewritten set — its own
/// host in the `host` header, and none of the gateway's routing headers.
#[tokio::test]
async fn the_upstream_receives_the_rewritten_headers() {
    let (upstream_server, dialled) = listening_upstream("relayed");

    let svc = service(OagwConfig {
        allow_http_upstream: true,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    });
    let router = router(svc);

    let created = create_upstream(
        &router,
        &upstream_body("rewritten.local", &upstream_server.host(), upstream_server.port()),
    )
    .await;
    create_route(
        &router,
        &route_body(created["id"].as_str().unwrap(), "/v1/echo", &["POST"]),
    )
    .await;

    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/rewritten.local/v1/echo")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::HeaderName::from_static("host"), "rewritten.local")
        .header(http::HeaderName::from_static("x-oagw-target-host"), &upstream_server.host())
        .header(http::HeaderName::from_static("connection"), "keep-alive")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, body) = send(&router, request).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let host = dialled.hosts().first().cloned().unwrap_or_default();
    assert_eq!(
        host,
        format!("{}:{}", upstream_server.host(), upstream_server.port()),
        "the upstream sees its own authority, not the caller's: {host}"
    );
    let leaked = dialled.routing().into_iter().any(|value| !value.is_empty());
    assert!(
        !leaked,
        "the gateway's routing header never reaches the upstream: {:?}",
        dialled.routing()
    );
}
