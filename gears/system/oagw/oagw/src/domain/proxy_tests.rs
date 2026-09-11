//! Tests for the data-plane proxy: the body rules, the request and response
//! header transformation, the path and query transformation and the pass-through
//! of the upstream response — every assertion reads what the `httpmock` upstream
//! actually received.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, BodyDataStream, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use httpmock::{Mock, MockServer};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::control_plane::ControlPlane;
use crate::domain::model::{Endpoint, EndpointScheme, PathSuffixMode};
use crate::error::{
    AUTHENTICATION_FAILED_TYPE, CONNECTION_TIMEOUT_TYPE, CORS_METHOD_NOT_ALLOWED_TYPE,
    CORS_ORIGIN_NOT_ALLOWED_TYPE, DOWNSTREAM_ERROR_TYPE, ERROR_SOURCE_HEADER,
    LINK_UNAVAILABLE_TYPE, MISSING_TARGET_HOST_TYPE, OagwError, PAYLOAD_TOO_LARGE_TYPE,
    PLUGIN_NOT_FOUND_TYPE, PROTOCOL_ERROR_TYPE, RATE_LIMIT_EXCEEDED_TYPE, REQUEST_TIMEOUT_TYPE,
    RETRY_AFTER_HEADER, ROUTE_NOT_FOUND_TYPE, UNKNOWN_TARGET_HOST_TYPE, UPSTREAM_ERROR_SOURCE,
    VALIDATION_ERROR_TYPE, X_RATELIMIT_LIMIT_HEADER, X_RATELIMIT_REMAINING_HEADER,
};

use super::{
    endpoint_authority, ensure_supported_method, is_websocket_upgrade, outbound_path,
    outbound_query, percent_decode,
};
use crate::api::rest::register_routes;

// ── Harness ──────────────────────────────────────────────────────────────────

/// Tenant that owns every resource the tests create.
fn tenant() -> Uuid {
    Uuid::from_u128(0xA11A)
}

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xFEED))
        .subject_tenant_id(tenant())
        .build()
        .expect("test security context")
}

/// Data-plane configuration for a loopback upstream: plaintext is allowed, the
/// SSRF policy is off (the mock server is a loopback address).
fn data_plane_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy { enabled: false },
        body_limit_bytes: 1024 * 1024,
        ..OagwConfig::default()
    }
}

/// A data-plane test rig: two routers over one control plane plus the loopback
/// mock server the upstreams point at.
///
/// The control plane is served with the default configuration, so the setup
/// requests are never clipped by a limit the test is exercising on the data
/// plane; the proxy requests go through the data-plane router built from
/// `config`.
struct Rig {
    /// Serves the management API (upstream and route CRUD).
    control: Router,
    /// Serves `/oagw/v1/proxy/{alias}` with the configuration under test.
    data_plane: Router,
    ctx: SecurityContext,
}

/// The default alias of the rig's upstream.
const ALIAS: &str = "mock.local.test";

fn rig_with(config: OagwConfig) -> Rig {
    let openapi = OpenApiRegistryImpl::new();
    let plane = Arc::new(ControlPlane::new());
    let control = register_routes(
        Router::new(),
        &openapi,
        Arc::clone(&plane),
        OagwConfig::default(),
    );
    let data_plane = register_routes(Router::new(), &openapi, plane, config);
    Rig {
        control,
        data_plane,
        ctx: ctx(),
    }
}

fn rig() -> Rig {
    rig_with(data_plane_config())
}

impl Rig {
    /// `POST` a document to the control plane and return `(status, body)`.
    async fn post(&self, path: &str, document: Value) -> (StatusCode, Value) {
        let response = self.send("POST", path, &[], Some(document)).await;
        let status = response.status();
        (status, body_of(response).await)
    }

    /// Send a request with a JSON body through the rig router.
    async fn send(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        document: Option<Value>,
    ) -> Response {
        let document_body = document
            .as_ref()
            .map(|document| Body::from(serde_json::to_vec(document).unwrap()));
        let body = document_body.unwrap_or_else(Body::empty);
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if document.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(self.ctx.clone());
        self.control.clone().oneshot(request).await.unwrap()
    }

    /// Create an `http` upstream pointing at `server` and return its GTS id.
    async fn upstream_at(&self, alias: &str, server: &MockServer, extra: Value) -> String {
        let mut document = json!({
            "alias": alias,
            "server": {
                "endpoints": [{
                    "scheme": "http",
                    "host": server.host(),
                    "port": server.port(),
                }]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        if let Value::Object(extra) = extra {
            for (name, value) in extra {
                document[name.as_str()] = value;
            }
        }
        let (status, body) = self.post("/oagw/v1/upstreams", document).await;
        assert_eq!(status, StatusCode::CREATED, "the test upstream must exist");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Create the default upstream and a route for it, returning the upstream id.
    async fn default_upstream(&self, server: &MockServer, extra: Value) -> String {
        let upstream = self.upstream_at(ALIAS, server, extra).await;
        self.route(
            &upstream,
            json!({ "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"], "path": "/" }),
        )
        .await;
        upstream
    }

    /// Create an `http` upstream pointing at a raw responder at `address` — an
    /// upstream with **no** `headers.request` rule, so `passthrough` is unset —
    /// and a `GET` route for it, returning the upstream id.
    async fn raw_upstream(&self, address: SocketAddr) -> String {
        let (status, body) = self
            .post(
                "/oagw/v1/upstreams",
                json!({
                    "alias": ALIAS,
                    "server": {
                        "endpoints": [{
                            "scheme": "http",
                            "host": address.ip().to_string(),
                            "port": address.port(),
                        }]
                    },
                    "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "body {body}");
        let upstream = body["id"].as_str().unwrap().to_owned();
        self.route(&upstream, json!({ "methods": ["GET"], "path": "/" }))
            .await;
        upstream
    }

    /// Create an HTTP route for the upstream and return its GTS id.
    async fn route(&self, upstream_id: &str, http: Value) -> String {
        let (status, body) = self
            .post(
                "/oagw/v1/routes",
                json!({ "upstream_id": upstream_id, "match": { "http": http } }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "the test route must exist");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Send a proxy request for `alias` and return the gateway response.
    async fn proxy(
        &self,
        method: &str,
        alias: &str,
        suffix: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        query: Option<&str>,
    ) -> Response {
        let mut uri = format!("/oagw/v1/proxy/{alias}{suffix}");
        if let Some(query) = query {
            uri.push('?');
            uri.push_str(query);
        }
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = body.map_or_else(Body::empty, |bytes| Body::from(bytes.to_vec()));
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(self.ctx.clone());
        self.data_plane.clone().oneshot(request).await.unwrap()
    }

    /// Send a proxy request whose body is handed over verbatim, so a test can
    /// set its own framing headers (`transfer-encoding: chunked`).
    async fn proxy_body(
        &self,
        method: &str,
        alias: &str,
        suffix: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("/oagw/v1/proxy/{alias}{suffix}"));
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(self.ctx.clone());
        self.data_plane.clone().oneshot(request).await.unwrap()
    }

    /// Send a `GET` proxy request with no extra headers.
    async fn get(&self, alias: &str, suffix: &str, query: Option<&str>) -> Response {
        self.proxy("GET", alias, suffix, &[], None, query).await
    }
}

async fn body_of(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A mock answering `200` with `body` for `path`, asserted to be called once.
async fn echo<'a>(server: &'a MockServer, path: &str, body: &str) -> Mock<'a> {
    server
        .mock_async(|when, then| {
            when.method("GET").path(path);
            then.status(200).body(body);
        })
        .await
}

/// A mock answering `status` with `body` and `headers` for `path`.
async fn responds<'a>(
    server: &'a MockServer,
    path: &str,
    status: u16,
    body: &str,
    headers: &[(&str, &str)],
) -> Mock<'a> {
    server
        .mock_async(move |when, then| {
            when.path(path);
            let mut then = then.status(status).body(body);
            for (name, value) in headers {
                then = then.header(*name, *value);
            }
        })
        .await
}

// ── Pure helpers ─────────────────────────────────────────────────────────────

#[test]
fn the_outbound_path_follows_the_suffix_mode() {
    assert_eq!(outbound_path("/", PathSuffixMode::Append, ""), "/");
    assert_eq!(
        outbound_path("/", PathSuffixMode::Append, "/v1/echo"),
        "/v1/echo"
    );
    assert_eq!(
        outbound_path("/v1/chat", PathSuffixMode::Append, "v1/chat/x"),
        "/v1/chat/x"
    );
    assert_eq!(
        outbound_path("/v1/health", PathSuffixMode::Disabled, "/v1/health/live"),
        "/v1/health"
    );
}

#[test]
fn the_outbound_query_is_filtered_by_the_allowlist() {
    let allowlist = vec!["api-key".to_owned(), "trace".to_owned()];

    assert_eq!(outbound_query(None, &allowlist).unwrap(), None);
    // An empty allowlist allows none (route.v1.schema.json), so a request that
    // carries a query against it is a rejection and not a silent filter.
    assert!(outbound_query(Some("api-key=x"), &[]).is_err());
    // The original order and encoding survive; the listed names are kept.
    assert_eq!(
        outbound_query(Some("trace=abc&api-key=secret"), &allowlist)
            .unwrap()
            .as_deref(),
        Some("trace=abc&api-key=secret")
    );
    // A percent-encoded name matches its decoded allowlist entry.
    assert_eq!(
        outbound_query(Some("api%2Dkey=secret"), &["api-key".to_owned()])
            .unwrap()
            .as_deref(),
        Some("api%2Dkey=secret")
    );
}

#[test]
fn an_unknown_query_parameter_is_rejected_not_dropped() {
    let allowlist = vec!["api-key".to_owned()];

    let error = outbound_query(Some("api-key=x&debug=1"), &allowlist)
        .expect_err("an unknown parameter is a guard-rule rejection");
    assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
    assert!(error.to_string().contains("`debug`"), "{error}");
    // A request that declares no query never trips the rule.
    assert_eq!(outbound_query(None, &allowlist).unwrap(), None);
}

#[test]
fn percent_decode_decodes_escapes_only() {
    assert_eq!(percent_decode("api%2Dkey"), "api-key");
    assert_eq!(percent_decode("plain"), "plain");
    assert_eq!(percent_decode("a%2"), "a%2");
    assert_eq!(percent_decode("a%ZZ"), "a%ZZ");
}

#[test]
fn the_endpoint_authority_carries_a_non_default_port() {
    let authority = |scheme: EndpointScheme, host: &str, port: u16| {
        endpoint_authority(&Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        })
    };
    assert_eq!(
        authority(EndpointScheme::Https, "api.example.test", 443),
        "api.example.test"
    );
    assert_eq!(
        authority(EndpointScheme::Http, "api.example.test", 80),
        "api.example.test"
    );
    assert_eq!(
        authority(EndpointScheme::Http, "api.example.test", 8080),
        "api.example.test:8080"
    );
}

#[test]
fn the_data_plane_only_proxies_the_documented_methods() {
    for method in ["GET", "POST", "PUT", "DELETE", "PATCH", "patch"] {
        assert!(
            ensure_supported_method(&method.parse().unwrap()).is_ok(),
            "{method} must be proxied"
        );
    }
    for method in ["TRACE", "OPTIONS", "CONNECT", "PROPFIND"] {
        let error = ensure_supported_method(&method.parse().unwrap())
            .expect_err("an unlisted method must be rejected");
        assert_eq!(error.status_code(), 400, "{method}");
        assert_eq!(error.gts_type(), VALIDATION_ERROR_TYPE);
    }
}

// ── The pass-through ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_get_is_forwarded_verbatim_and_stamped_upstream() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/echo", "pong").await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[
                ("x-oagw-target-host", "127.0.0.1"),
                ("x-request-id", "req-1"),
                ("proxy-authorization", "Basic c2VjcmV0"),
                ("authorization", "Bearer client-token"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "pong");
    // The request reached the mock with the transformed headers and query.
    mock.assert_async().await;
}

#[tokio::test]
async fn hop_by_hop_headers_and_the_target_host_header_never_reach_the_upstream() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header_missing("proxy-authorization")
                .header_missing("proxy-authenticate")
                .header_missing("te")
                .header_missing("trailer")
                .header_missing("upgrade")
                .header_missing("keep-alive")
                .header_missing("x-oagw-target-host");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[
                ("x-oagw-target-host", "127.0.0.1"),
                ("proxy-authorization", "Basic c2VjcmV0"),
                ("proxy-authenticate", "Basic"),
                ("te", "trailers"),
                ("trailer", "x-checksum"),
                ("upgrade", "websocket"),
                ("keep-alive", "timeout=5"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

/// `connection` and `transfer-encoding` are hop-by-hop on both legs: the mock
/// never sees them on the request and the client never sees them on the
/// response, however insistently the endpoints set them.
#[tokio::test]
async fn connection_and_transfer_encoding_are_stripped_from_both_legs() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header_missing("connection")
                .header_missing("transfer-encoding");
            then.status(200)
                .body("ok")
                .header("connection", "keep-alive");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("connection", "close"), ("transfer-encoding", "chunked")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert!(
        headers.get("connection").is_none(),
        "the upstream's `connection` header must not reach the client"
    );
    mock.assert_async().await;
}

/// A chunked request (framed by `transfer-encoding`, with no content length)
/// reaches the upstream whole: the gateway buffers the inbound body and lets
/// the outbound client frame it again.
#[tokio::test]
async fn a_chunked_request_is_forwarded_whole() {
    use futures_util::stream;
    use std::convert::Infallible;

    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("POST")
                .path("/v1/echo")
                .header_missing("transfer-encoding")
                .body("hello upstream");
            then.status(200).body("ack");
        })
        .await;

    let chunks: Vec<Result<&[u8], Infallible>> =
        vec![Ok(b"hello ".as_slice()), Ok(b"upstream".as_slice())];
    let response = rig
        .proxy_body(
            "POST",
            ALIAS,
            "/v1/echo",
            &[("transfer-encoding", "chunked")],
            Body::from_stream(stream::iter(chunks)),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ack");
    mock.assert_async().await;
}

#[tokio::test]
async fn the_host_header_becomes_the_endpoint_authority() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let authority = format!("{}:{}", server.host(), server.port());
    let mock = server
        .mock_async(move |when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header("host", &authority);
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("host", "client.example.test")],
            None,
            None,
        )
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the mock must be reached"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn passthrough_none_forwards_only_host_content_length_and_content_type() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let body = br#"{"prompt":"hi"}"#;
    let length = body.len().to_string();
    let expected_length = length.clone();
    let mock = server
        .mock_async(move |when, then| {
            when.method("POST")
                .path("/v1/chat")
                .header("content-type", "application/json")
                .header("content-length", &expected_length)
                .header_missing("x-request-id")
                .header_missing("authorization");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[
                ("x-request-id", "req-2"),
                ("authorization", "Bearer client-token"),
                ("content-type", "application/json"),
                ("content-length", &length),
            ],
            Some(body),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls_async().await, 1);
}

#[tokio::test]
async fn passthrough_allowlist_forwards_only_the_listed_headers() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "headers": {
                "request": {
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["x-tenant", "authorization"]
                }
            }
        }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header("x-tenant", "acme")
                .header_missing("x-request-id")
                .header_missing("x-cookie");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[
                ("x-tenant", "acme"),
                ("x-request-id", "req-3"),
                ("x-cookie", "secret"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn passthrough_all_forwards_every_inbound_header() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "headers": { "request": { "passthrough": "all" } }
        }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header("x-tenant", "acme")
                .header("x-request-id", "req-4")
                .header_missing("x-oagw-target-host")
                .header_missing("proxy-authorization");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[
                ("x-tenant", "acme"),
                ("x-request-id", "req-4"),
                ("x-oagw-target-host", "127.0.0.1"),
                ("proxy-authorization", "Basic c2VjcmV0"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn set_add_and_remove_rules_are_applied_in_that_order() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "headers": {
                "request": {
                    "passthrough": "all",
                    "set": { "x-signature": "set-by-oagw" },
                    "add": { "x-added": "once" },
                    "remove": ["x-dropped"]
                }
            }
        }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header("x-signature", "set-by-oagw")
                .header("x-added", "once")
                .header_missing("x-dropped");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("x-dropped", "client-value")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn response_rules_are_applied_and_hop_by_hop_response_headers_are_stripped() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "headers": {
                "response": {
                    "set": { "x-gateway": "oagw" },
                    "remove": ["x-server"]
                }
            }
        }),
    )
    .await;
    let mock = responds(
        &server,
        "/v1/echo",
        200,
        "ok",
        &[
            ("x-server", "mock"),
            ("proxy-authenticate", "Basic"),
            ("x-kept", "yes"),
        ],
    )
    .await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers.get("x-gateway").unwrap(), "oagw");
    assert!(headers.get("x-server").is_none(), "the rule removed it");
    assert_eq!(
        headers.get("x-kept").unwrap(),
        "yes",
        "untouched headers stay"
    );
    assert!(headers.get("proxy-authenticate").is_none());
    assert_eq!(
        headers.get(ERROR_SOURCE_HEADER).unwrap(),
        UPSTREAM_ERROR_SOURCE
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn the_query_is_filtered_by_the_route_allowlist() {
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.upstream_at(ALIAS, &server, json!({})).await;
    rig.route(
        &upstream,
        json!({
            "methods": ["GET"],
            "path": "/v1/search",
            "query_allowlist": ["q", "api-key"]
        }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/search")
                .query_param("q", "gateway")
                .query_param("api-key", "secret")
                .query_param_missing("debug")
                .query_param_missing("trace");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .get(ALIAS, "/v1/search", Some("api-key=secret&q=gateway"))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn an_empty_allowlist_forwards_no_query_parameter() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET").path("/v1/echo").query_param_missing("q");
            then.status(200).body("ok");
        })
        .await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn a_query_parameter_the_allowlist_does_not_list_is_a_400() {
    // DESIGN "Guard Rules": "Query params | Validate against
    // `match.http.query_allowlist`; reject if unknown" — the rejection is a
    // guard rule, so the request never reaches the upstream.
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.default_upstream(&server, json!({})).await;
    rig.route(
        &upstream,
        json!({
            "methods": ["GET"],
            "path": "/v1/search",
            "query_allowlist": ["q"]
        }),
    )
    .await;

    let response = rig
        .get(ALIAS, "/v1/search", Some("q=gateway&debug=1"))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_of(response).await;
    assert_eq!(body["type"], VALIDATION_ERROR_TYPE);
    assert!(
        body["detail"].as_str().unwrap().contains("`debug`"),
        "{}",
        body["detail"]
    );
}

#[tokio::test]
async fn a_query_string_against_an_empty_allowlist_is_a_400() {
    // `route.v1.schema.json`: "White-listed query parameters. If empty, allow
    // none." A route without an allowlist forwards no query parameter, so a
    // request that carries one is rejected rather than silently filtered.
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;

    let response = rig.get(ALIAS, "/v1/echo", Some("q=gateway")).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_of(response).await;
    assert_eq!(body["type"], VALIDATION_ERROR_TYPE);
}

#[tokio::test]
async fn a_post_body_is_forwarded_verbatim() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let body = br#"{"prompt":"hello","n":2}"#;
    let mock = server
        .mock_async(move |when, then| {
            when.method("POST")
                .path("/v1/chat")
                .header("content-type", "application/json")
                .header("content-length", body.len().to_string())
                .body(String::from_utf8_lossy(body).into_owned());
            then.status(200).body("done");
        })
        .await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[("content-type", "application/json")],
            Some(body),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "done");
    mock.assert_async().await;
}

#[tokio::test]
async fn the_proxy_suffix_is_appended_to_the_route_path() {
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.upstream_at(ALIAS, &server, json!({})).await;
    rig.route(&upstream, json!({ "methods": ["GET"], "path": "/v1/chat" }))
        .await;
    let mock = echo(&server, "/v1/chat/completions", "ok").await;

    let response = rig.get(ALIAS, "/v1/chat/completions", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn a_proxy_url_without_a_suffix_serves_the_root_path() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/", "ok").await;

    let response = rig.get(ALIAS, "", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn a_disabled_suffix_mode_forwards_exactly_the_route_path() {
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.upstream_at(ALIAS, &server, json!({})).await;
    rig.route(
        &upstream,
        json!({ "methods": ["GET"], "path": "/v1/health", "path_suffix_mode": "disabled" }),
    )
    .await;
    let mock = echo(&server, "/v1/health", "healthy").await;

    let response = rig.get(ALIAS, "/v1/health", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;

    // A suffix beyond the prefix has no route at all.
    let rejected = rig.get(ALIAS, "/v1/health/live", None).await;
    assert_eq!(rejected.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(rejected.into_body(), 4096).await.unwrap()[..])
            .unwrap_or(Value::Null)["type"],
        json!(ROUTE_NOT_FOUND_TYPE)
    );
}

#[tokio::test]
async fn a_percent_encoded_suffix_reaches_the_upstream_untouched() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/files/a%20b%2Fc", "ok").await;

    let response = rig.get(ALIAS, "/v1/files/a%20b%2Fc", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

// ── The X-OAGW-Target-Host matrix, end to end ────────────────────────────────

#[tokio::test]
async fn a_single_endpoint_is_used_without_and_rejected_with_a_foreign_header() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;

    let rejected = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("x-oagw-target-host", "elsewhere.example.test")],
            None,
            None,
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let problem = body_of(rejected).await;
    assert_eq!(problem["type"], json!(UNKNOWN_TARGET_HOST_TYPE));
    assert_eq!(problem["valid_hosts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn an_explicit_alias_rotates_over_two_endpoints_without_the_header() {
    let (first, second) = loopback_pool().await;
    let rig = rig();
    let (status, body) = rig
        .post(
            "/oagw/v1/upstreams",
            json!({
                "alias": "pool.local.test",
                "server": {
                    "endpoints": [
                        { "scheme": "http", "host": first.host, "port": first.port },
                        { "scheme": "http", "host": second.host, "port": second.port }
                    ]
                },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");
    let upstream = body["id"].as_str().unwrap().to_owned();
    rig.route(
        &upstream,
        json!({ "methods": ["GET"], "path": "/v1/x", "path_suffix_mode": "append" }),
    )
    .await;

    // Both endpoints share the port and differ in host, so the responses say
    // which one served.
    let first_response = rig.get("pool.local.test", "/v1/x", None).await;
    assert_eq!(first_response.status(), StatusCode::OK);
    assert_eq!(body_text(first_response).await, "from-first\n");
    let second_response = rig.get("pool.local.test", "/v1/x", None).await;
    assert_eq!(second_response.status(), StatusCode::OK);
    assert_eq!(body_text(second_response).await, "from-second\n");
}

/// A pool whose alias is the endpoints' common suffix demands
/// `X-OAGW-Target-Host`, and the demand is enforced before any dial, so the
/// rejection is observable without a reachable upstream.
///
/// The endpoint selection itself — which host the header names, and that the
/// match is case-insensitive — is pinned by the resolution tests, which run the
/// same alias derivation without a socket.
#[tokio::test]
async fn a_common_suffix_alias_demands_the_target_host_header_before_any_dial() {
    let rig = rig();
    // Hostname endpoints whose common suffix is the alias. None of them is
    // dialed: the missing header stops the request before resolution picks one.
    let (status, body) = rig
        .post(
            "/oagw/v1/upstreams",
            json!({
                "alias": "vendor.test",
                "server": {
                    "endpoints": [
                        { "scheme": "http", "host": "us.vendor.test", "port": 80 },
                        { "scheme": "http", "host": "eu.vendor.test", "port": 80 }
                    ]
                },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");
    let upstream = body["id"].as_str().unwrap().to_owned();
    rig.route(&upstream, json!({ "methods": ["GET"], "path": "/" }))
        .await;

    let rejected = rig.get("vendor.test", "/v1/echo", None).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let problem = body_of(rejected).await;
    assert_eq!(problem["type"], json!(MISSING_TARGET_HOST_TYPE));
    assert_eq!(problem["alias"], json!("vendor.test"));
    let valid_hosts = problem["valid_hosts"].as_array().unwrap();
    assert_eq!(valid_hosts.len(), 2);
    assert!(valid_hosts.contains(&json!("us.vendor.test")));
    assert!(valid_hosts.contains(&json!("eu.vendor.test")));
}

// ── Streaming and upgrades (S3) ──────────────────────────────────────────────

/// A plain `GET` that asks for no upgrade is still transformed as before, even on
/// a route that carries upgrade traffic: the hop-by-hop strip keeps eating
/// `connection` and `upgrade`.
#[tokio::test]
async fn a_plain_get_on_the_same_route_is_unaffected_by_the_tunnel() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header_missing("upgrade")
                .header_missing("connection")
                .header_missing("sec-websocket-key");
            then.status(200).body("ok");
        })
        .await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
    mock.assert_async().await;
}

/// The upgrade detector reads both headers as comma-separated token lists and
/// compares them case-insensitively (RFC 7230 §6.7).
#[test]
fn an_upgrade_is_detected_from_both_header_tokens() {
    let headers = |values: &[(&str, &str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in values {
            map.insert(
                name.parse::<axum::http::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    };

    assert!(is_websocket_upgrade(&headers(&[
        ("connection", "keep-alive, Upgrade"),
        ("upgrade", "WebSocket"),
    ])));
    assert!(is_websocket_upgrade(&headers(&[
        ("connection", "Upgrade"),
        ("upgrade", "websocket"),
    ])));
    // Either token missing, and the request is an ordinary request.
    assert!(!is_websocket_upgrade(&headers(&[
        ("connection", "keep-alive"),
        ("upgrade", "websocket"),
    ])));
    assert!(!is_websocket_upgrade(&headers(&[(
        "connection",
        "Upgrade"
    )])));
    assert!(!is_websocket_upgrade(&headers(&[
        ("connection", "Upgrade"),
        ("upgrade", "h2c"),
    ])));
    assert!(!is_websocket_upgrade(&headers(&[])));
}

/// An SSE upstream that writes its first event, then waits, and only writes the
/// second one once the test releases it.
struct DelayedEvents {
    /// The dialable address of the responder.
    address: SocketAddr,
    /// Notified once the first event is written and flushed.
    first_flushed: Arc<Notify>,
    /// Releases the second event.
    release: Arc<Notify>,
    /// How many events the responder has written so far.
    written: Arc<AtomicUsize>,
}

/// Start the delayed two-event SSE responder.
async fn delayed_events() -> DelayedEvents {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_flushed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let written = Arc::new(AtomicUsize::new(0));
    let (first, released, counter) = (
        Arc::clone(&first_flushed),
        Arc::clone(&release),
        Arc::clone(&written),
    );
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let head = read_request_head(&mut socket).await;
        assert!(
            head.starts_with("GET "),
            "the upstream must see a GET: {head}"
        );
        drop(
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
x-request-id: upstream-sse\r\n\r\n",
                )
                .await,
        );
        drop(socket.write_all(b"data: one\n\n").await);
        drop(socket.flush().await);
        counter.fetch_add(1, Ordering::SeqCst);
        first.notify_one();
        released.notified().await;
        drop(socket.write_all(b"data: two\n\n").await);
        drop(socket.flush().await);
        counter.fetch_add(1, Ordering::SeqCst);
        drop(socket.shutdown().await);
    });
    DelayedEvents {
        address,
        first_flushed,
        release,
        written,
    }
}

/// An SSE response reaches the client event by event: the first event is read
/// while the upstream has written nothing else, and only the test releases the
/// second one. A gateway that buffered the response would never deliver the first
/// event, because the upstream does not close until the test lets it.
#[tokio::test]
async fn an_sse_response_streams_each_event_as_the_upstream_writes_it() {
    let upstream = delayed_events().await;
    let rig = rig();
    rig.raw_upstream(upstream.address).await;

    let response = rig.get(ALIAS, "/v1/events", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/event-stream",
        "the upstream content type is passed through"
    );
    assert_eq!(
        headers.get("x-request-id").unwrap(),
        "upstream-sse",
        "an upstream header that is not hop-by-hop survives"
    );
    assert_eq!(
        headers.get(ERROR_SOURCE_HEADER).unwrap(),
        UPSTREAM_ERROR_SOURCE
    );
    assert!(
        headers.get("content-length").is_none(),
        "an event stream is not length-delimited"
    );

    upstream.first_flushed.notified().await;
    let mut stream = response.into_body().into_data_stream();
    let mut received: Vec<u8> = Vec::new();
    read_event(&mut stream, &mut received, "data: one\n\n").await;
    assert_eq!(
        String::from_utf8_lossy(&received),
        "data: one\n\n",
        "nothing but the first event may be buffered"
    );
    assert_eq!(
        upstream.written.load(Ordering::SeqCst),
        1,
        "the second event has not been written yet"
    );

    upstream.release.notify_one();
    read_event(&mut stream, &mut received, "data: two\n\n").await;
    assert_eq!(
        String::from_utf8_lossy(&received),
        "data: one\n\ndata: two\n\n"
    );
    let end = tokio::time::timeout(STREAM_DEADLINE, stream.next())
        .await
        .unwrap();
    assert!(end.is_none(), "the stream must close cleanly");
}

/// A raw WebSocket echo upstream: it answers a well-formed handshake with a `101`
/// and then echoes every byte of the upgraded connection until EOF. A handshake
/// that is missing a header is refused with the same `400` an ordinary server
/// would send.
async fn websocket_echo() -> (SocketAddr, oneshot::Receiver<String>) {
    websocket_upstream(true).await
}

/// A raw upstream that receives a well-formed handshake and refuses it anyway:
/// a refused upgrade is an ordinary response, not a tunnel.
async fn websocket_refuser() -> (SocketAddr, oneshot::Receiver<String>) {
    websocket_upstream(false).await
}

/// A raw upstream that accepts a well-formed handshake with a `101`, then
/// reports the moment its upgraded socket reaches EOF — the observable end of the
/// upstream half of a tunnel.
async fn websocket_closer() -> (SocketAddr, oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, closed_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let head = read_request_head(&mut socket).await;
        if !is_ws_handshake(&head) {
            drop(
                socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\n\r\nnope")
                    .await,
            );
            drop(socket.shutdown().await);
            let _unused = closed.send(());
            return;
        }
        drop(socket.write_all(WS_SWITCHING_PROTOCOLS).await);
        drop(socket.flush().await);
        let mut buffer = [0u8; 512];
        while let Ok(read) = socket.read(&mut buffer).await {
            if read == 0 {
                break;
            }
        }
        drop(socket.shutdown().await);
        let _unused = closed.send(());
    });
    (address, closed_receiver)
}

/// The raw upstream used by both upgrade tests; `accepts` picks the reply.
async fn websocket_upstream(accepts: bool) -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (observed, observed_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let head = read_request_head(&mut socket).await;
        let complete = is_ws_handshake(&head);
        drop(observed.send(head));
        if !(accepts && complete) {
            drop(
                socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\n\r\nnope")
                    .await,
            );
            drop(socket.shutdown().await);
            return;
        }
        drop(socket.write_all(WS_SWITCHING_PROTOCOLS).await);
        drop(socket.flush().await);
        let mut buffer = [0u8; 512];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if socket.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            }
        }
        drop(socket.shutdown().await);
    });
    (address, observed_receiver)
}

/// The `101` the raw upstream answers a well-formed handshake with.
const WS_SWITCHING_PROTOCOLS: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\n\
upgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: \
s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";

/// Whether the raw upstream received every header the handshake needs.
fn is_ws_handshake(head: &str) -> bool {
    let lowered = head.to_ascii_lowercase();
    lowered.contains("upgrade: websocket")
        && lowered.contains("connection: upgrade")
        && lowered.contains("sec-websocket-key:")
        && lowered.contains("sec-websocket-version: 13")
}

/// Read the head of a raw HTTP request, up to the blank line that ends it.
///
/// A transport error fails the test outright: swallowing it would report a
/// truncated head as a complete one and send the test down the wrong branch.
async fn read_request_head(socket: &mut TcpStream) -> String {
    let mut head: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("the raw upstream socket must stay readable");
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if contains(&head, "\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Whether `haystack` contains `needle` as a byte subsequence.
fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// How long a streamed event may take to arrive before the test gives up.
const STREAM_DEADLINE: Duration = Duration::from_secs(5);

/// Read from `stream` into `received` until it carries `event`, bounded by a
/// deadline so a buffering gateway fails the test instead of hanging it.
async fn read_event(stream: &mut BodyDataStream, received: &mut Vec<u8>, event: &str) {
    let filled = async {
        while !contains(received, event) {
            let Some(chunk) = stream.next().await else {
                break;
            };
            received.extend_from_slice(&chunk.expect("the streamed body must be readable"));
        }
    };
    tokio::time::timeout(STREAM_DEADLINE, filled)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the event `{event}` did not arrive in time; received so far: {}",
                String::from_utf8_lossy(received)
            )
        });
}

/// An upgrade request is tunneled, not proxied: the upstream receives the
/// handshake (despite the unset `passthrough`), answers `101`, and the gateway
/// splices the two upgraded connections so the echo comes back over the tunnel.
///
/// Both legs of the test are real HTTP/1.1 connections — a hyper h1 server in
/// front of the router and a hyper client behind it — because an upgrade handle
/// only exists on a connection hyper owns: `hyper::upgrade::on` finds nothing in
/// a `Router::oneshot` request, so the downstream half of the tunnel could never
/// complete through it.
#[tokio::test]
async fn an_upgrade_request_is_tunneled_to_the_upstream_bidirectionally() {
    let (upstream, observed) = websocket_echo().await;
    let rig = rig();
    rig.raw_upstream(upstream).await;

    let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_address = gateway.local_addr().unwrap();
    let router = rig.data_plane.clone();
    let ctx = rig.ctx.clone();
    tokio::spawn(async move {
        let (socket, _) = gateway.accept().await.unwrap();
        // One hyper h1 connection in front of the router, with the security
        // context the gateway handlers read from the request extensions.
        let service = hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
            let router = router.clone();
            let ctx = ctx.clone();
            async move {
                let (mut parts, body) = request.into_parts();
                parts.extensions.insert(ctx);
                let request = Request::from_parts(parts, Body::new(body));
                router.oneshot(request).await
            }
        });
        drop(
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await,
        );
    });

    let client: Client<HttpConnector, Body> =
        Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{gateway_address}/oagw/v1/proxy/{ALIAS}/ws"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(Body::empty())
        .unwrap();
    let mut response = client.request(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "the upstream accepted the handshake"
    );
    let headers = response.headers().clone();
    assert_eq!(headers.get("upgrade").unwrap(), "websocket");
    assert_eq!(headers.get("connection").unwrap(), "Upgrade");
    assert!(
        headers.get("sec-websocket-accept").is_some(),
        "the upstream handshake headers are returned verbatim"
    );
    assert_eq!(
        headers.get(ERROR_SOURCE_HEADER).unwrap(),
        UPSTREAM_ERROR_SOURCE
    );

    let mut client_io = TokioIo::new(hyper::upgrade::on(&mut response).await.unwrap());
    drop(client_io.write_all(b"ping").await);
    drop(client_io.flush().await);
    let mut echoed = [0u8; 4];
    tokio::time::timeout(STREAM_DEADLINE, client_io.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"ping", "the tunnel is bidirectional");

    // Closing the client side has to close the tunnel cleanly in both directions.
    drop(client_io.shutdown().await);
    let mut rest = Vec::new();
    let read = tokio::time::timeout(STREAM_DEADLINE, client_io.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, 0, "the tunnel must be closed, got {rest:?}");

    let head = observed.await.unwrap();
    assert!(is_ws_handshake(&head), "the upstream received: {head}");
}

/// A `passthrough: all` upstream must not see a handshake header twice: the
/// passthrough loop forwards `sec-websocket-*` (they are not hop-by-hop) and
/// `apply_upgrade_headers` must not append them again, because RFC 6455 §4.1
/// requires exactly one `Sec-WebSocket-Key` per handshake.
#[tokio::test]
async fn a_passthrough_all_upstream_receives_each_handshake_header_once() {
    let (upstream, observed) = websocket_echo().await;
    let rig = rig();
    // The upstream declares `passthrough: all`, so the passthrough loop already
    // forwards every `sec-websocket-*` header the client sent.
    let (status, body) = rig
        .post(
            "/oagw/v1/upstreams",
            json!({
                "alias": ALIAS,
                "server": {
                    "endpoints": [{
                        "scheme": "http",
                        "host": upstream.ip().to_string(),
                        "port": upstream.port(),
                    }]
                },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "headers": { "request": { "passthrough": "all" } },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");
    let upstream_id = body["id"].as_str().unwrap().to_owned();
    rig.route(&upstream_id, json!({ "methods": ["GET"], "path": "/" }))
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/ws",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let head = observed.await.unwrap();
    let lowered = head.to_ascii_lowercase();
    for header in ["upgrade: websocket", "connection: upgrade"] {
        assert!(
            lowered.contains(header),
            "the handshake must carry `{header}`: {head}"
        );
    }
    for header in ["sec-websocket-key:", "sec-websocket-version:"] {
        let seen = lowered.matches(header).count();
        assert_eq!(seen, 1, "`{header}` must appear once, not {seen}: {head}");
    }
}

/// A reply to an upgrade request that is not a `101` is an ordinary response: the
/// client sees the upstream's status and body, the hop-by-hop strip still runs and
/// no tunnel is opened.
#[tokio::test]
async fn a_refused_upgrade_is_forwarded_as_an_ordinary_response() {
    let (upstream, observed) = websocket_refuser().await;
    let rig = rig();
    rig.raw_upstream(upstream).await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/ws",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let headers = response.headers().clone();
    assert_eq!(body_text(response).await, "nope");
    assert_eq!(
        headers.get(ERROR_SOURCE_HEADER).unwrap(),
        UPSTREAM_ERROR_SOURCE
    );
    assert_eq!(headers.get("content-length").unwrap(), "4");
    assert!(
        headers.get("upgrade").is_none() && headers.get("connection").is_none(),
        "the ordinary transformation strips the hop-by-hop headers"
    );
    let head = observed.await.unwrap();
    assert!(
        is_ws_handshake(&head),
        "the handshake reached the upstream despite the unset `passthrough`: {head}"
    );
}

/// An upgrade request on a method that cannot tunnel (`POST`) is an ordinary
/// request: the handshake headers reach the upstream through the normal
/// transformation and the upstream's ordinary response comes back, so no
/// connection is hijacked.
#[tokio::test]
async fn a_post_carrying_upgrade_tokens_is_proxied_not_tunneled() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            // A proxied request goes through the ordinary transformation, which
            // strips the hop-by-hop handshake headers it cannot carry.
            when.method("POST")
                .path("/v1/echo")
                .header_missing("upgrade")
                .header_missing("sec-websocket-key");
            then.status(200).body("posted");
        })
        .await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/echo",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
            Some(b"{}".as_slice()),
            None,
        )
        .await;

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(status, StatusCode::OK, "body {text}");
    assert_eq!(text, "posted");
    mock.assert_async().await;
}

/// A client that never completes its half of the upgrade closes the upstream side
/// too: the splice task sees the refused downstream half, drops the upstream
/// `Upgraded` it holds, and the upstream socket reaches EOF.
///
/// This is the cancellation branch of `splice` — PRD §8 ("Client disconnects:
/// System closes upstream connection") — and no successful tunnel reaches it. The
/// downstream half is refused here because a request served through the rig
/// carries no upgrade handle at all (`hyper::upgrade::on` finds none in a
/// hand-built request), which is exactly the "one end refused" case the branch
/// exists for.
#[tokio::test]
async fn a_client_that_never_completes_the_upgrade_closes_the_upstream() {
    let (upstream, closed) = websocket_closer().await;
    let rig = rig();
    rig.raw_upstream(upstream).await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/ws",
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ],
            None,
            None,
        )
        .await;

    assert_eq!(
        response.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "the upstream accepted the handshake"
    );
    let closed = tokio::time::timeout(STREAM_DEADLINE, closed).await.unwrap();
    assert!(
        closed.is_ok(),
        "the upstream side of the tunnel must close when the other end never upgrades"
    );
}

// ── Raw loopback endpoints ───────────────────────────────────────────────────

/// A dialable loopback endpoint of a raw HTTP responder.
struct PoolEndpoint {
    host: String,
    port: u16,
}

/// Serve one canned HTTP response per connection: `tag` as the body.
fn serve_loopback(listener: TcpListener, tag: &'static str) {
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                if socket.read(&mut buffer).await.unwrap_or(0) == 0 {
                    return;
                }
                let body = format!("{tag}\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length:                      {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                drop(socket.write_all(response.as_bytes()).await);
                drop(socket.shutdown().await);
            });
        }
    });
}

/// Two loopback HTTP responders that share one port, one on `127.0.0.1` and one
/// on `::1`.
///
/// A pool's endpoints must share the port (DESIGN "Multi-Endpoint Load
/// Balancing"), so two dialable endpoints need two hosts; the two loopback
/// address families give that without DNS. The bind is retried until both
/// addresses accept the same port.
async fn loopback_pool() -> (PoolEndpoint, PoolEndpoint) {
    for _ in 0..25 {
        let Ok(first) = TcpListener::bind(("127.0.0.1", 0)).await else {
            continue;
        };
        let port = first
            .local_addr()
            .expect("bound listener has an address")
            .port();
        let second = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))).await;
        let Ok(second) = second else {
            continue; // dropping `first` frees the port for the next attempt
        };
        serve_loopback(first, "from-first");
        serve_loopback(second, "from-second");
        return (
            PoolEndpoint {
                host: "127.0.0.1".to_owned(),
                port,
            },
            PoolEndpoint {
                host: "::1".to_owned(),
                port,
            },
        );
    }
    panic!("no free loopback port shared by 127.0.0.1 and ::1");
}

// ── Rejections ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem_document() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;

    let response = rig.get("no-such-alias.test", "/v1/echo", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        "gateway"
    );
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(ROUTE_NOT_FOUND_TYPE));
    assert_eq!(problem["alias"], json!("no-such-alias.test"));
}

#[tokio::test]
async fn a_request_without_a_matching_route_is_a_404() {
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.upstream_at(ALIAS, &server, json!({})).await;
    rig.route(&upstream, json!({ "methods": ["GET"], "path": "/v1/only" }))
        .await;

    let response = rig.get(ALIAS, "/v1/other", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(ROUTE_NOT_FOUND_TYPE));
    assert_eq!(problem["upstream_id"], json!(upstream));
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503() {
    let server = MockServer::start_async().await;
    let rig = rig();
    let upstream = rig.upstream_at(ALIAS, &server, json!({})).await;
    rig.route(&upstream, json!({ "methods": ["GET"], "path": "/" }))
        .await;
    let disabled = rig
        .send(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream}"),
            &[],
            Some(json!({
                "alias": ALIAS,
                "enabled": false,
                "server": {
                    "endpoints": [{
                        "scheme": "http",
                        "host": server.host(),
                        "port": server.port()
                    }]
                },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            })),
        )
        .await;
    assert_eq!(disabled.status(), StatusCode::OK);

    let response = rig.get(ALIAS, "/v1/echo", None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_of(response).await;
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
    );
    assert_eq!(problem["alias"], json!(ALIAS));
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_http_is_not_allowed() {
    let server = MockServer::start_async().await;
    let mut config = data_plane_config();
    config.allow_http_upstream = false;
    let rig = rig_with(config);
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_of(response).await;
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
    );
    assert_eq!(mock.calls_async().await, 0, "nothing was dialed");
}

#[tokio::test]
async fn an_ssrf_blocked_endpoint_is_a_400() {
    let server = MockServer::start_async().await;
    let mut config = data_plane_config();
    config.ssrf_policy = SsrfPolicy { enabled: true };
    let rig = rig_with(config);
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(problem["host"], json!(server.host()));
    assert_eq!(mock.calls_async().await, 0, "nothing was dialed");
}

#[tokio::test]
async fn an_oversized_body_is_a_413_raised_before_the_upstream_is_called() {
    let server = MockServer::start_async().await;
    let mut config = data_plane_config();
    config.body_limit_bytes = 16;
    let rig = rig_with(config);
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/chat", "ok").await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[("content-type", "application/json")],
            Some(&[b'x'; 64]),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(PAYLOAD_TOO_LARGE_TYPE));
    assert_eq!(mock.calls_async().await, 0, "nothing was forwarded");
}

#[tokio::test]
async fn a_mismatched_content_length_is_a_400() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/chat", "ok").await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[("content-length", "99")],
            Some(b"{}"),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(mock.calls_async().await, 0);
}

#[tokio::test]
async fn a_non_numeric_content_length_is_a_400() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/chat", "ok").await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[("content-length", "many")],
            Some(b"{}"),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_of(response).await["type"],
        json!(VALIDATION_ERROR_TYPE)
    );
    assert_eq!(mock.calls_async().await, 0);
}

#[tokio::test]
async fn a_transfer_encoding_that_is_not_chunked_is_a_400() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let mock = echo(&server, "/v1/chat", "ok").await;

    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/chat",
            &[("transfer-encoding", "gzip")],
            Some(b"{}"),
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert!(problem["detail"].as_str().unwrap().contains("chunked"));
    assert_eq!(mock.calls_async().await, 0);
}

// ── Transport failures ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_refused_connection_is_a_503_link_unavailable() {
    // A port that nothing listens on any more.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let rig = rig();
    let (status, body) = rig
        .post(
            "/oagw/v1/upstreams",
            json!({
                "alias": "closed.local.test",
                "server": {
                    "endpoints": [{
                        "scheme": "http",
                        "host": address.ip().to_string(),
                        "port": address.port()
                    }]
                },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");
    let upstream = body["id"].as_str().unwrap().to_owned();
    rig.route(&upstream, json!({ "methods": ["GET"], "path": "/" }))
        .await;

    let response = rig.get("closed.local.test", "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(LINK_UNAVAILABLE_TYPE));
}

#[tokio::test]
async fn a_slow_upstream_hits_the_deadline_with_504() {
    let server = MockServer::start_async().await;
    let mut config = data_plane_config();
    config.proxy_timeout_secs = 1;
    let rig = rig_with(config);
    rig.default_upstream(&server, json!({})).await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET").path("/v1/slow");
            then.status(200)
                .delay(Duration::from_secs(6))
                .body("too late");
        })
        .await;

    let response = rig.get(ALIAS, "/v1/slow", None).await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let problem = body_of(response).await;
    // The `proxy_timeout_secs` deadline brackets the whole upstream call, so its
    // overrun is a request timeout, not a connection timeout.
    assert_eq!(problem["type"], json!(REQUEST_TIMEOUT_TYPE));
    assert_eq!(
        problem["detail"],
        json!("the upstream did not respond within the configured deadline of 1 s")
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn an_upstream_error_response_is_passed_through_unchanged() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, json!({})).await;
    let body = r#"{"error":{"code":"quota_exceeded"}}"#;
    let mock = server
        .mock_async(move |when, then| {
            when.method("GET").path("/v1/forbidden");
            then.status(403)
                .body(body)
                .header("content-type", "application/json")
                .header("x-request-id", "upstream-1");
        })
        .await;

    let response = rig.get(ALIAS, "/v1/forbidden", None).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let headers = response.headers().clone();
    assert_eq!(body_text(response).await, body);
    // The pass-through is not rewritten into a gateway problem document.
    assert_eq!(
        headers.get(ERROR_SOURCE_HEADER).unwrap(),
        UPSTREAM_ERROR_SOURCE
    );
    assert_eq!(headers.get("x-request-id").unwrap(), "upstream-1");
    mock.assert_async().await;
}

/// The transport vocabulary of the data plane: the two 504 kinds are distinct
/// (a deadline overrun is a *request* timeout, a timeout inside the connector a
/// *connection* timeout) and every mapped kind carries its documented type.
#[test]
fn the_resolution_and_transport_error_types_are_the_problem_types() {
    let route = OagwError::route_not_found("no route");
    assert_eq!(route.gts_type(), ROUTE_NOT_FOUND_TYPE);
    assert_eq!(route.status_code(), 404);

    for (error, gts_type) in [
        (OagwError::request_timeout("deadline"), REQUEST_TIMEOUT_TYPE),
        (
            OagwError::connection_timeout("connect"),
            CONNECTION_TIMEOUT_TYPE,
        ),
        (
            OagwError::link_unavailable("refused"),
            LINK_UNAVAILABLE_TYPE,
        ),
        (OagwError::protocol_error("unparsable"), PROTOCOL_ERROR_TYPE),
        (OagwError::downstream_error("closed"), DOWNSTREAM_ERROR_TYPE),
    ] {
        assert_eq!(error.gts_type(), gts_type, "{gts_type}");
        assert!([502, 503, 504].contains(&error.status_code()), "{gts_type}");
    }
}

// ── S4: preflight, CORS, rate limiting and the plugin chain ──────────────────

/// The CORS document of an upstream admitting exactly `origin`.
fn cors_document(origin: &str) -> Value {
    json!({
        "cors": {
            "enabled": true,
            "allowed_origins": [origin],
            "allowed_methods": ["GET", "POST"],
            "expose_headers": ["X-Request-ID"],
            "allow_credentials": true
        }
    })
}

#[tokio::test]
async fn a_cors_preflight_is_answered_before_anything_is_resolved() {
    // No upstream is created at all: the preflight is answered by the data plane
    // itself, so an unknown alias cannot make a browser wait for a resolution.
    let rig = rig();
    let mut request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/no-such-alias.test/v1/echo");
    for (name, value) in [
        ("origin", "https://app.example.com"),
        ("access-control-request-method", "POST"),
        ("access-control-request-headers", "Content-Type"),
    ] {
        request = request.header(name, value);
    }
    let mut bound = request.body(Body::empty()).unwrap();
    bound.extensions_mut().insert(rig.ctx.clone());
    let response = rig.data_plane.clone().oneshot(bound).await.unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
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
        "Content-Type"
    );
    assert_eq!(
        response.headers().get("access-control-max-age").unwrap(),
        "86400"
    );
    assert_eq!(
        response.headers().get("vary").unwrap(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
}

#[tokio::test]
async fn a_cors_request_from_a_disallowed_origin_is_a_403_gateway_problem() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, cors_document("https://app.example.com"))
        .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("origin", "https://evil.com")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        "gateway"
    );
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(CORS_ORIGIN_NOT_ALLOWED_TYPE));
    assert_eq!(mock.calls(), 0, "the request never reached the upstream");
}

#[tokio::test]
async fn a_cors_request_from_an_allowed_origin_is_advertised_on_the_response() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, cors_document("https://app.example.com"))
        .await;
    echo(&server, "/v1/echo", "ok").await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("origin", "https://app.example.com")],
            None,
            None,
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
            .get("access-control-allow-credentials")
            .unwrap(),
        "true"
    );
    let vary = response
        .headers()
        .get_all("vary")
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        vary.contains("Origin"),
        "the response varies on Origin: {vary}"
    );
}

#[tokio::test]
async fn a_cors_disallowed_method_is_a_403() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(&server, cors_document("https://app.example.com"))
        .await;
    echo(&server, "/v1/echo", "ok").await;

    let response = rig
        .proxy(
            "DELETE",
            ALIAS,
            "/v1/echo",
            &[("origin", "https://app.example.com")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(CORS_METHOD_NOT_ALLOWED_TYPE));
}

#[tokio::test]
async fn an_exhausted_rate_limit_is_a_429_with_the_rate_limit_headers() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "rate_limit": {
                "sustained": { "rate": 1, "window": "minute" },
                "scope": "global"
            }
        }),
    )
    .await;
    echo(&server, "/v1/echo", "ok").await;

    let first = rig.get(ALIAS, "/v1/echo", None).await;
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "the first request is allowed"
    );
    let second = rig.get(ALIAS, "/v1/echo", None).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        second.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        "gateway"
    );
    assert!(
        second.headers().get(RETRY_AFTER_HEADER).is_some(),
        "the client is told when to retry"
    );
    assert_eq!(
        second.headers().get(X_RATELIMIT_LIMIT_HEADER).unwrap(),
        "1",
        "the limit the counter is drawn from"
    );
    assert_eq!(
        second.headers().get(X_RATELIMIT_REMAINING_HEADER).unwrap(),
        "0"
    );
    let problem = body_of(second).await;
    assert_eq!(problem["type"], json!(RATE_LIMIT_EXCEEDED_TYPE));
}

#[tokio::test]
async fn a_plugin_chain_rejects_a_request_through_the_data_plane() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "plugins": { "items": [
                { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                  "config": { "required_request_headers": "X-Tenant-Id" } }
            ] }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        "gateway"
    );
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(
        mock.calls(),
        0,
        "a rejected request never reaches the upstream"
    );

    let admitted = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("x-tenant-id", "acme")],
            None,
            None,
        )
        .await;
    assert_eq!(
        admitted.status(),
        StatusCode::OK,
        "a request that carries the header passes"
    );
}

#[tokio::test]
async fn a_transform_plugin_runs_after_the_guards() {
    // The guard requires the very header the transform would inject, so an
    // admitted request proves the guards ran first and the transform after them.
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "plugins": { "items": [
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
                { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                  "config": { "required_request_headers": "X-Request-ID" } }
            ] }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the transform runs too late to help"
    );
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(VALIDATION_ERROR_TYPE));
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn the_request_id_transform_reaches_the_upstream_and_the_client() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "headers": { "request": { "passthrough": "all" } },
            "plugins": { "items": [
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
            ] }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("x-request-id", "caller-chose-this")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "caller-chose-this"
    );
    let sent = mock.calls();
    assert_eq!(sent, 1, "the request reached the upstream once");
}

#[tokio::test]
async fn an_auth_plugin_without_a_credential_source_is_a_401_on_the_proxy_path() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "plugins": { "items": [
                { "plugin_ref": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                  "config": { "secret_ref": "cred://api-key" } }
            ] }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(ERROR_SOURCE_HEADER).unwrap(),
        "gateway"
    );
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(AUTHENTICATION_FAILED_TYPE));
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn a_configured_upstream_auth_binding_reaches_the_plugin_pipeline() {
    let server = MockServer::start_async().await;
    let rig = rig();
    // ADR-0008 binds the authentication plugin through `upstream.auth`, not
    // through `plugins`, so this upstream authenticates on every proxy request.
    rig.default_upstream(
        &server,
        json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "secret_ref": "cred://tenant/api-key" }
            }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    // This data plane has no credential source wired, so the plugin cannot
    // resolve the reference and refuses the request *before* the upstream is
    // called — which is the proof that the binding reached the pipeline.
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(AUTHENTICATION_FAILED_TYPE));
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn an_upstream_auth_binding_that_names_no_plugin_is_a_503() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({ "auth": { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1" } }),
    )
    .await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    // `basic.v1` is a catalog-only identifier, so the binding resolves to
    // nothing: the request is refused, never forwarded unauthenticated.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_of(response).await["type"],
        json!(PLUGIN_NOT_FOUND_TYPE)
    );
}

#[tokio::test]
async fn a_header_a_plugin_wrote_reaches_the_upstream_past_the_passthrough_mode() {
    let server = MockServer::start_async().await;
    let rig = rig();
    // `passthrough: none` forwards nothing of the client request; the
    // `request_id` transform has to inject its header anyway.
    rig.default_upstream(
        &server,
        json!({
            "headers": {
                "request": { "passthrough": "none" },
                "response": {}
            },
            "plugins": { "items": [
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
            ] }
        }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header_exists("x-request-id");
            then.status(200).body("ok");
        })
        .await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
    mock.assert_async().await;
}

#[tokio::test]
async fn a_header_a_plugin_wrote_reaches_the_upstream_when_the_upstream_has_no_headers_member() {
    let server = MockServer::start_async().await;
    let rig = rig();
    // No `headers` member at all: the schema default is `passthrough: "none"`,
    // which forwards nothing of the client request — but is still a mode, so the
    // `request_id` transform has to inject its header anyway. (An earlier
    // revision read the absent member as "no rules", which dropped the
    // plugin-written header with it.)
    rig.default_upstream(
        &server,
        json!({ "plugins": { "items": [
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        ] } }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/v1/echo")
                .header_exists("x-request-id")
                // The client's own headers stay behind the default mode.
                .header_missing("x-client-side");
            then.status(200).body("ok");
        })
        .await;

    let response = rig
        .proxy(
            "GET",
            ALIAS,
            "/v1/echo",
            &[("x-client-side", "present")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
    mock.assert_async().await;
}

#[tokio::test]
async fn a_plugin_cannot_smuggle_a_hop_by_hop_header_to_the_upstream() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({ "headers": { "request": { "passthrough": "all" } } }),
    )
    .await;
    let mock = server
        .mock_async(|when, then| {
            when.method("POST")
                .path("/v1/echo")
                .header_missing("connection")
                .header_missing("upgrade");
            then.status(200).body("ok");
        })
        .await;

    // The client asks for an upgrade on a `POST`: not a tunnel (RFC 6455 §4.1
    // defines the handshake for `GET` only), so the tokens are ordinary
    // headers — and ordinary hop-by-hop ones, stripped whatever the mode says.
    let response = rig
        .proxy(
            "POST",
            ALIAS,
            "/v1/echo",
            &[("connection", "Upgrade"), ("upgrade", "websocket")],
            None,
            None,
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    mock.assert_async().await;
}

#[tokio::test]
async fn an_unresolvable_plugin_reference_is_a_503() {
    let server = MockServer::start_async().await;
    let rig = rig();
    rig.default_upstream(
        &server,
        json!({
            "plugins": { "items": [
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
            ] }
        }),
    )
    .await;
    let mock = echo(&server, "/v1/echo", "ok").await;

    let response = rig.get(ALIAS, "/v1/echo", None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let problem = body_of(response).await;
    assert_eq!(problem["type"], json!(PLUGIN_NOT_FOUND_TYPE));
    assert_eq!(mock.calls(), 0);
}
