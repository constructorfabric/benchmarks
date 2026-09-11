//! The proxy exchange against a live local upstream.
//!
//! Covers the forward and classify rows of `cpt-cf-oagw-dod-proxy-forward` and
//! `cpt-cf-oagw-dod-error-source`: the answer that passes through with the
//! upstream's status, body, headers, and `X-OAGW-Error-Source: upstream`, the
//! routing header, the hop-by-hop set, and the caller credential that never
//! reach the upstream, the `Host` replacement, the outbound path with the
//! admitted query, the upstream's own response rules and plugin chain, and the
//! 503 a refusing endpoint is answered with. The upstream is a minimal HTTP/1.1
//! listener that echoes each request back as the answer body, so every
//! assertion reads what the gateway actually sent.

// @cpt-dod:cpt-cf-oagw-dod-proxy-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::error::AuthZResolverError;

use oagw::OagwConfig;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::OagwState;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const TENANT: u128 = 0x41;
const HOST: &str = "127.0.0.1";
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

/// One request the listener received, as the echo serializes it.
#[derive(Debug, Clone, Serialize)]
struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "String::is_empty")]
    body: String,
}

/// The JSON writer the echo answers with.
use serde::Serialize;

/// A live HTTP/1.1 upstream, which answers on the port it bound.
#[derive(Clone)]
struct Upstream {
    port: u16,
}

/// Starts one echo upstream on an ephemeral port.
async fn upstream() -> Upstream {
    let listener = TcpListener::bind((HOST, 0))
        .await
        .expect("the listener binds");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let request = match read_request(&mut socket).await {
                    Some(request) => request,
                    None => return,
                };
                let body = serde_json::to_vec(&request).expect("the echo serializes");
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     x-upstream-marker: probe\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Upstream { port }
}

/// Reads one HTTP/1.1 request off the socket, with its framed body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Captured> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if buffer.len() > 128 * 1024 {
            return None;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_head_end(&buffer) {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((
                name.trim().to_ascii_lowercase(),
                value.trim().to_owned(),
            ));
        }
    }
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.contains("chunked"));
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    if chunked {
        while read_chunk(socket, &mut body).await? {}
    } else {
        while body.len() < length {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
    }
    Some(Captured {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Reads one chunked-transfer chunk into `body`, answering whether more follow.
async fn read_chunk(socket: &mut tokio::net::TcpStream, body: &mut Vec<u8>) -> Option<bool> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        socket.read_exact(&mut byte).await.ok()?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
    }
    let size = usize::from_str_radix(
        std::str::from_utf8(&line[..line.len() - 2]).ok()?.trim(),
        16,
    )
    .ok()?;
    if size == 0 {
        return Some(false);
    }
    let start = body.len();
    while body.len() - start < size {
        let mut chunk = [0_u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(start + size);
    Some(true)
}

/// The index of the blank line that ends a request head.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
}

/// One mounted proxy surface over the echo upstream.
struct Surface {
    router: Router,
}

/// Builds the surface, the upstream, and the route the caller states.
async fn wired(headers: Option<Value>, plugins: Vec<Value>) -> (Surface, Upstream) {
    let upstream = upstream().await;
    let store = Arc::new(oagw::store::OagwStore::new());
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
        "server": { "endpoints": [{ "scheme": "http", "host": HOST, "port": upstream.port }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": plugins }
    });
    if let Some(headers) = headers {
        body["headers"] = headers;
    }
    let upstream_instance = created(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        &body,
    )
    .await;
    created(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        &json!({
            "upstream_id": key_of(&upstream_instance),
            "match": { "http": { "methods": ["GET", "POST"], "path": "/api", "query_allowlist": ["model"] } },
            "priority": 10
        }),
    )
    .await;
    (Surface { router }, upstream)
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

/// The authenticated subject a request carries.
fn subject() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(TENANT))
        .subject_tenant_id(Uuid::from_u128(TENANT))
        .build()
        .expect("the subject is complete")
}

/// Issues one proxy request and returns its status, headers, body, and source.
async fn exchange(
    app: Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, Vec<(String, String)>, Value, Option<String>) {
    let mut builder = Request::builder().method(method).uri(uri).extension(subject());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Body::from(body.to_vec()))
        .expect("the request builds");
    let response = app.oneshot(request).await.expect("oneshot resolves");
    let status = response.status();
    let source = response
        .headers()
        .get(ERROR_SOURCE)
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let answer_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document = serde_json::from_slice(&bytes).expect("the answer body is JSON");
    (status, answer_headers, document, source)
}

/// Whether the answer carries one header name.
fn carries(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_upstream_answer_passes_through_with_its_status_body_and_headers() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (status, answer_headers, document, source) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[],
        b"",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source.as_deref(), Some("upstream"));
    assert!(
        carries(&answer_headers, "x-upstream-marker"),
        "the upstream's own header travels: {answer_headers:?}"
    );
    assert_eq!(document["method"], "GET", "{document}");
    assert_eq!(document["path"], "/api", "{document}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_header_names_the_selected_endpoint() {
    let (surface, upstream) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[],
        b"",
    )
    .await;
    assert_eq!(document["headers"][0][0], "host", "{document}");
    assert_eq!(document["headers"][0][1], format!("{HOST}:{}", upstream.port));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_routing_header_never_reaches_the_upstream() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[("x-oagw-target-host", HOST)],
        b"",
    )
    .await;
    let headers = document["headers"].as_array().expect("the header list");
    assert!(
        !headers
            .iter()
            .any(|pair| pair[0] == "x-oagw-target-host"),
        "the routing header was forwarded: {headers:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_hop_by_hop_header_reaches_the_upstream() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[("connection", "keep-alive"), ("te", "trailers")],
        b"",
    )
    .await;
    let headers = document["headers"].as_array().expect("the header list");
    for hop_by_hop in ["connection", "te"] {
        assert!(
            !headers.iter().any(|pair| pair[0] == hop_by_hop),
            "{hop_by_hop} was forwarded: {headers:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_callers_authorization_never_reaches_the_upstream() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[("authorization", "Bearer caller-token")],
        b"",
    )
    .await;
    let headers = document["headers"].as_array().expect("the header list");
    assert!(
        !headers.iter().any(|pair| pair[0] == "authorization"),
        "the caller's credential was forwarded: {headers:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_outbound_path_carries_the_suffix_and_the_admitted_query() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api/deeper?model=gpt-4"),
        &[],
        b"",
    )
    .await;
    assert_eq!(document["path"], "/api/deeper?model=gpt-4", "{document}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_body_travels_to_the_upstream() {
    let (surface, _) = wired(None, Vec::new()).await;
    let (_, _, document, _) = exchange(
        surface.router,
        Method::POST,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[("content-type", "application/json")],
        br#"{"prompt":"hello"}"#,
    )
    .await;
    assert_eq!(document["method"], "POST", "{document}");
    assert_eq!(document["body"], r#"{"prompt":"hello"}"#, "{document}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_response_rules_of_the_upstream_apply_on_the_way_back() {
    let headers = json!({
        "response": { "set": { "x-gateway-added": "gateway" }, "remove": ["x-upstream-marker"] }
    });
    let (surface, _) = wired(Some(headers), Vec::new()).await;
    let (_, answer_headers, _, _) = exchange(
        surface.router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[],
        b"",
    )
    .await;
    let added = answer_headers
        .iter()
        .find(|(name, _)| name == "x-gateway-added")
        .map(|(_, value)| value.as_str());
    assert_eq!(added, Some("gateway"));
    assert!(
        !carries(&answer_headers, "x-upstream-marker"),
        "the removed header was forwarded: {answer_headers:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_chain_of_the_upstream_runs_before_the_forward() {
    // The built-in request-id transform is resolvable by name, so the binding
    // rides on the upstream write rather than on a stored row.
    let (router, _) = wired_with_binding().await;
    let (_, _, document, _) = exchange(
        router,
        Method::GET,
        &format!("/oagw/v1/proxy/{HOST}/api"),
        &[],
        b"",
    )
    .await;
    let headers = document["headers"].as_array().expect("the header list");
    assert!(
        headers.iter().any(|pair| pair[0] == "x-request-id"),
        "the transform's mutation reached the upstream: {headers:?}"
    );
}

/// The surface, the upstream, and the upstream whose chain carries the
/// built-in request-id transform.
async fn wired_with_binding() -> (Router, Upstream) {
    let upstream = upstream().await;
    let store = Arc::new(oagw::store::OagwStore::new());
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
    let reference = oagw::gts::plugin_catalog::TRANSFORM_REQUEST_ID;
    let upstream_instance = created(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        &json!({
            "alias": HOST,
            "server": { "endpoints": [{ "scheme": "http", "host": HOST, "port": upstream.port }] },
            "protocol": HTTP_PROTOCOL,
            "plugins": { "items": [{ "position": 0, "plugin_ref": reference, "config": {} }] }
        }),
    )
    .await;
    created(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        &json!({
            "upstream_id": key_of(&upstream_instance),
            "match": { "http": { "methods": ["GET"], "path": "/api" } },
            "priority": 10
        }),
    )
    .await;
    (router, upstream)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_that_refuses_the_connection_is_answered_503() {
    // A port nothing listens on: the listener is bound to learn the port and
    // dropped to close it.
    let closed = TcpListener::bind((HOST, 0))
        .await
        .expect("the listener binds");
    let port = closed.local_addr().expect("the address").port();
    drop(closed);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let store = Arc::new(oagw::store::OagwStore::new());
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
    let upstream_instance = created(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        &json!({
            "alias": HOST,
            "server": { "endpoints": [{ "scheme": "http", "host": HOST, "port": port }] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    created(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        &json!({
            "upstream_id": key_of(&upstream_instance),
            "match": { "http": { "methods": ["GET"], "path": "/api" } },
            "priority": 10
        }),
    )
    .await;

    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/proxy/{HOST}/api"))
        .extension(subject());
    builder = builder.header("x-oagw-error-source", "expect-gateway");
    let request = builder.body(Body::empty()).expect("the request builds");
    let response = router.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE)
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
}
