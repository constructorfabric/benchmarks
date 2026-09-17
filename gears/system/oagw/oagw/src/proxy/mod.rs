//! Proxy data plane: the `/oagw/v1/proxy/{alias}/…` endpoint (R1-R18).
//!
//! [`register_proxy_routes`] adds the data plane to the shared router the gear
//! is handed, next to the management REST API of [`crate::api`]. The route is
//! registered method-agnostically, so no method is answered with `405` (R1), and
//! at its gear-relative path with no `/api` prefix.
//!
//! [`ProxyService`] is the orchestrator of DESIGN.md §3.2. One request runs
//! through, short-circuiting on the first failure (R3):
//!
//! ```text
//! preflight (204, no resolution)
//!   -> resolve upstream by alias (tenant walk, closest match)
//!   -> match a route (method allowlist + longest path prefix)
//!   -> merge configuration (CORS, rate limit, plugin chain)
//!   -> CORS check on the actual request
//!   -> select the target endpoint (X-OAGW-Target-Host matrix)
//!   -> validate the body (Content-Length, 100 MB, transfer-encoding)
//!   -> upstream call (no retry, no cache)
//!   -> response passthrough with X-OAGW-Error-Source: upstream
//! ```
//!
//! Every gateway-generated failure is rendered by [`crate::error`] as an RFC
//! 9457 problem document; an upstream status is passed through unchanged (R14).

// The shared `GatewayError` exceeds clippy's `result_large_err` threshold and is
// handed to the client whole, so every resolver/matcher/forward entry point that
// rejects input trips it. The error type is shared with the whole crate (see
// `crate::error`), so the data plane accepts the size rather than boxing a
// problem it renders as a problem+json body anyway.
#![allow(clippy::result_large_err)]

mod forward;
mod headers;
mod matcher;
mod resolver;
mod stream;
mod websocket;

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use toolkit_http::HttpResponse;
use toolkit_security::SecurityContext;

use crate::error::{ERROR_SOURCE_HEADER_NAME, ErrorSource, GatewayError, GatewayErrorKind};
use crate::proxy::headers as gateway_headers;

pub use forward::{MAX_REQUEST_BODY_BYTES, ProxyService, UpstreamCall};
pub use gateway_headers::{RoundRobin, TARGET_HOST_HEADER, TARGET_HOST_HEADER_NAME};
pub use matcher::{RouteMatch, http_method};
pub use resolver::{ResolvedUpstream, tenant_chain};

/// Registers the proxy data plane on the shared router (R1).
///
/// Both path shapes are registered so `/oagw/v1/proxy/{alias}` — a proxy URL
/// with no path suffix — is served by the same handler as
/// `/oagw/v1/proxy/{alias}/{*path}`.
pub fn register_proxy_routes(router: Router, service: Arc<ProxyService>) -> Router {
    let data_plane = Router::new()
        .route("/oagw/v1/proxy/{alias}", axum::routing::any(proxy_alias))
        .route(
            "/oagw/v1/proxy/{alias}/{*path}",
            axum::routing::any(proxy_path),
        )
        .layer(axum::middleware::from_fn(stamp_error_source))
        .with_state(service);

    router.merge(data_plane)
}

/// One proxied request as the handlers received it, at either proxy path shape.
struct Proxied<'a> {
    /// The `{alias}` of the proxy URL.
    alias: String,
    /// The path suffix after the alias, `""` when the URL carries none.
    path: &'a str,
    /// The request method, as received.
    method: Method,
    /// The request headers, as received.
    headers: HeaderMap,
    /// The raw query string, `""` when the URL carries none.
    query: String,
    /// The caller's security context, when the platform attached one.
    context: Option<&'a Extension<SecurityContext>>,
    /// The request itself, so its body can be read.
    request: Request,
}

/// Serves `{method} /oagw/v1/proxy/{alias}` (R1).
async fn proxy_alias(
    State(service): State<Arc<ProxyService>>,
    Path(alias): Path<String>,
    method: Method,
    headers: HeaderMap,
    query: RawQuery,
    context: Option<Extension<SecurityContext>>,
    request: Request,
) -> Response {
    serve(
        &service,
        Proxied {
            alias,
            path: "",
            method,
            headers,
            query: query.0.unwrap_or_default(),
            context: context.as_ref(),
            request,
        },
    )
    .await
}

/// Serves `{method} /oagw/v1/proxy/{alias}/{*path}` (R1).
async fn proxy_path(
    State(service): State<Arc<ProxyService>>,
    Path((alias, path)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    query: RawQuery,
    context: Option<Extension<SecurityContext>>,
    request: Request,
) -> Response {
    serve(
        &service,
        Proxied {
            alias,
            path: &path,
            method,
            headers,
            query: query.0.unwrap_or_default(),
            context: context.as_ref(),
            request,
        },
    )
    .await
}

/// The proxy orchestration of DESIGN.md §3.2 (R3), short-circuiting on the
/// first failure.
async fn serve(service: &ProxyService, proxied: Proxied<'_>) -> Response {
    // A preflight is answered before any resolution: it carries no credentials,
    // so there is no tenant context to resolve an upstream with (ADR 0004
    // "Preflight Request Handling").
    if gateway_headers::is_preflight(&proxied.method, &proxied.headers) {
        return preflight_response(&proxied.headers);
    }

    let Proxied {
        alias,
        path,
        method,
        headers,
        query,
        context,
        request,
    } = proxied;

    let chain = resolver::tenant_chain(crate::api::rest::handlers::caller_tenant(context));

    let resolved = match resolver::resolve_upstream(&service.config, &chain, &alias) {
        Ok(resolved) => resolved,
        Err(error) => return error.into_response(),
    };

    let route = match resolver::resolve_route(
        &service.config,
        &chain,
        &resolved.upstream,
        &method,
        path,
        &query,
    ) {
        Ok(matched) => matched,
        Err(error) => return error.into_response(),
    };

    let cors = resolver::effective_cors(&resolved.cors, route.route.config.cors.as_ref());
    let limit = resolver::effective_limit(resolved.rate_limit, route.route.config.rate_limit);
    let plugins = resolver::effective_plugins(&resolved.upstream, &route.route);

    // Plugin execution (auth/guard/transform) is not implemented in this gear;
    // the chain a request would run is resolved here so a later phase only has
    // to execute it (ADR 0002 internals are out of scope, R2).
    tracing::debug!(
        upstream = %resolved.upstream.alias(),
        plugins = plugins.len(),
        rate_limited = limit.is_some(),
        "resolved the plugin chain"
    );

    let Some(domain_method) = http_method(&method) else {
        // A method no route can name never matches a route, so this is only
        // reachable for a request that survived route matching by accident.
        return GatewayError::new(
            GatewayErrorKind::RouteNotFound,
            format!("no route of this upstream matches `{method} {path}`"),
        )
        .into_response();
    };

    // An origin outside the allowlist is refused before anything is forwarded
    // (ADR 0004 "Simple Request Handling"); an allowed one adds the CORS
    // response headers to whatever this request ends up answering.
    let cors_headers =
        match gateway_headers::check_cors(&cors, headers.get(header::ORIGIN), domain_method) {
            Ok(cors_headers) => cors_headers,
            Err(error) => return error.into_response(),
        };

    let response = self::forward(service, &resolved, route, method, path, headers, request).await;

    apply_cors(&cors_headers, response)
}

/// Selects the target endpoint, validates the body and dials the upstream.
async fn forward(
    service: &ProxyService,
    resolved: &ResolvedUpstream,
    route: RouteMatch,
    method: Method,
    path: &str,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let endpoint = match gateway_headers::select_target(
        &resolved.upstream,
        headers.get(TARGET_HOST_HEADER_NAME.as_str()),
        &service.round_robin,
    ) {
        Ok(endpoint) => endpoint,
        Err(error) => return error.into_response(),
    };

    if let Err(error) = forward::check_scheme_policy(service.config.config(), &endpoint) {
        return error.with_host(endpoint.host.as_str()).into_response();
    }

    // An upgrade is tunnelled rather than proxied: the handshake is performed
    // against the upstream and the tunnel relayed in both directions (R4).
    if websocket::is_upgrade_request(&headers) {
        return websocket::relay(
            service,
            &resolved.upstream,
            &endpoint,
            &route,
            headers,
            request,
        )
        .await;
    }

    // A plaintext upstream is dialled with its body still streaming, and its
    // response is relayed frame by frame — which is what an SSE upstream needs
    // (R1-R3). A TLS upstream keeps the leg below: its response body already
    // streams, and the shared client only forwards a whole request body.
    if endpoint.is_plaintext() {
        return stream::forward_streaming(
            service, resolved, route, method, headers, request, &endpoint,
        )
        .await;
    }

    let body = match forward::read_body(&headers, request.into_body()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };

    let request_headers =
        gateway_headers::build_request_headers(&headers, &resolved.upstream, &endpoint, body.len());

    let Some(domain_method) = http_method(&method) else {
        return GatewayError::new(
            GatewayErrorKind::RouteNotFound,
            format!("no route of this upstream matches `{method} {path}`"),
        )
        .into_response();
    };

    let call = UpstreamCall {
        method: domain_method,
        scheme: endpoint.scheme,
        authority: gateway_headers::authority(&endpoint),
        path: route.upstream_path,
        query: route.query,
        headers: request_headers,
        body,
    };

    match forward::dial(&service.client, &call).await {
        Ok(upstream) => passthrough(&resolved.upstream, upstream),
        Err(mut error) => {
            error = error.with_host(endpoint.host.as_str());
            error.into_response()
        }
    }
}

/// Adds the CORS response headers of the effective configuration to a response.
fn apply_cors(
    allowed: &[(axum::http::HeaderName, axum::http::HeaderValue)],
    mut response: Response,
) -> Response {
    for (name, value) in allowed {
        response.headers_mut().insert(name, value.clone());
    }

    response
}

/// Passes an upstream response through to the caller (R8, R9, R14, R17).
///
/// The status, the headers (minus hop-by-hop, plus the upstream
/// `headers.response` rules) and the streamed body are returned as they came;
/// only `X-OAGW-Error-Source: upstream` is added.
fn passthrough(
    upstream: &crate::domain::model::Upstream,
    upstream_response: HttpResponse,
) -> Response {
    let (mut parts, body) = upstream_response.into_inner().into_parts();

    let mut upstream_headers = parts.headers;
    gateway_headers::build_response_headers(upstream, &mut upstream_headers);
    parts.headers = upstream_headers;

    let mut response = Response::from_parts(parts, axum::body::Body::new(body));

    ErrorSource::Upstream.set_on(&mut response);

    response
}

/// The permissive 204 of an actual preflight (ADR 0004).
fn preflight_response(headers: &HeaderMap) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let requested_method = headers
        .get("access-control-request-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let requested_headers = headers
        .get("access-control-request-headers")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    let mut builder = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, requested_method)
        .header(header::ACCESS_CONTROL_MAX_AGE, 86_400)
        .header(
            header::VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        );

    if !requested_headers.is_empty() {
        builder = builder.header(header::ACCESS_CONTROL_ALLOW_HEADERS, requested_headers);
    }

    let mut response = match builder.body(axum::body::Body::empty()) {
        Ok(response) => response,
        Err(error) => {
            // The status is static and every header above is a literal, so this
            // only happens with a value the HTTP types refuse; fall back to the
            // bare permissive status rather than panicking in the request path.
            tracing::warn!(error = %error, "the preflight response could not be built");
            StatusCode::NO_CONTENT.into_response()
        }
    };

    ErrorSource::Gateway.set_on(&mut response);

    response
}

/// Adds `X-OAGW-Error-Source: gateway` to a response the data plane produces
/// that does not already carry it (R17).
async fn stamp_error_source(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let response = next.run(request).await;

    if response
        .headers()
        .contains_key(ERROR_SOURCE_HEADER_NAME.as_str())
    {
        response
    } else {
        ErrorSource::Gateway.on(response)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request};
    use httpmock::{Method as MockMethod, MockServer};
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::OagwConfig;
    use crate::domain::model::{PROTOCOL_HTTP, RouteSpec, UpstreamSpec};
    use crate::domain::store::ConfigService;

    /// A gateway with plaintext upstreams allowed, so `httpmock` can play the
    /// upstream.
    fn config() -> OagwConfig {
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        }
    }

    /// The tenant of the tests' callers. The proxy router runs without a
    /// security context, so resolution happens under the anonymous tenant.
    fn tenant() -> Uuid {
        Uuid::nil()
    }

    /// Builds a data plane over an empty store; the store is returned so a test
    /// can seed it.
    fn data_plane(config: OagwConfig) -> (Router, Arc<ConfigService>) {
        let config_service = Arc::new(ConfigService::new(config));
        let service = Arc::new(ProxyService::new(config_service.clone()).expect("client builds"));

        (
            register_proxy_routes(Router::new(), service),
            config_service,
        )
    }

    fn endpoint(host: &str, port: u16) -> Value {
        json!({ "host": host, "port": port, "scheme": "http" })
    }

    fn route(methods: &[&str], path: &str) -> Value {
        json!({ "match": { "http": { "methods": methods, "path": path } } })
    }

    /// Creates an upstream and one route of it, returning the upstream id.
    fn seed(config_service: &ConfigService, upstream: Value, route: Value) -> Uuid {
        let spec: UpstreamSpec = serde_json::from_value(upstream).unwrap();
        let created = config_service.create_upstream(tenant(), &spec).unwrap();

        let mut route_spec: RouteSpec = serde_json::from_value(route).unwrap();
        route_spec.upstream_id = created.id;
        config_service.create_route(tenant(), &route_spec).unwrap();

        created.id
    }

    /// A single-endpoint plaintext upstream pointing at the mock, with a route.
    fn seed_mock(config_service: &ConfigService, server: &MockServer, route: Value) -> Uuid {
        seed(
            config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", server.port())] }
            }),
            route,
        )
    }

    async fn send(router: Router, request: Request<Body>) -> axum::http::Response<Body> {
        router.oneshot(request).await.unwrap()
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    fn method_request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        builder.body(Body::empty()).unwrap()
    }

    async fn body(response: axum::http::Response<Body>) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();

        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        }
    }

    async fn problem_type(response: axum::http::Response<Body>) -> String {
        body(response).await["type"].as_str().unwrap().to_owned()
    }

    fn error_source(response: &axum::http::Response<Body>) -> String {
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME.as_str())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    // -- happy path -------------------------------------------------------

    // -- happy path -------------------------------------------------------

    #[tokio::test]
    async fn test_a_plain_http_request_is_proxied_end_to_end() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::POST)
                .path("/v1/chat/completions")
                .body(r#"{"model":"gpt-4"}"#)
                .header("content-type", "application/json");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-oagw-upstream", "mock")
                .body(r#"{"answer":"ok"}"#);
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["POST"], "/v1/chat"));

        let response = send(
            router,
            post(
                "/oagw/v1/proxy/api.openai.com/v1/chat/completions",
                r#"{"model":"gpt-4"}"#,
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(error_source(&response), "upstream");
        assert_eq!(response.headers().get("x-oagw-upstream").unwrap(), "mock");
        assert_eq!(body(response).await, json!({ "answer": "ok" }));
        assert_eq!(mock.calls(), 1);
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(axum::http::Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    // -- resolution failures ---------------------------------------------

    #[tokio::test]
    async fn test_an_unknown_alias_is_a_404_route_problem() {
        let (router, _config_service) = data_plane(config());

        let response = send(router, get("/oagw/v1/proxy/no-such-alias/v1/status")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn test_no_matching_route_is_a_404_route_problem() {
        let (router, config_service) = data_plane(config());
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["GET"], "/v1/status"),
        );

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/other")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn test_a_method_outside_the_allowlist_is_not_a_405() {
        let (router, config_service) = data_plane(config());
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["GET"], "/v1/status"),
        );

        let response = send(
            router,
            method_request("DELETE", "/oagw/v1/proxy/api.openai.com/v1/status", &[]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn test_a_disabled_upstream_is_a_503_link_problem() {
        let (router, config_service) = data_plane(config());
        let upstream = seed(
            &config_service,
            json!({
                "alias": "vendor.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["GET"], "/v1"),
        );

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "vendor.com",
                    "protocol": PROTOCOL_HTTP,
                    "enabled": false,
                    "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
                }))
                .unwrap(),
            )
            .unwrap();

        let response = send(router, get("/oagw/v1/proxy/vendor.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn test_an_upstream_status_is_passed_through_unchanged() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/missing");
            then.status(404)
                .header("content-type", "application/json")
                .body(r#"{"error":"not found"}"#);
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/missing"));

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/missing")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_source(&response), "upstream");
        assert_eq!(body(response).await, json!({ "error": "not found" }));
    }

    #[tokio::test]
    async fn test_an_unreachable_upstream_is_a_502_downstream_problem() {
        let (router, config_service) = data_plane(config());
        // Nothing is listening on port 1 in the test environment.
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["GET"], "/v1"),
        );

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
        );
    }

    // -- registration ------------------------------------------------------

    #[tokio::test]
    async fn test_every_routable_method_is_forwarded() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.is_true(|request| request.uri().path().starts_with("/v1/things"));
            then.status(204);
        });

        let (router, config_service) = data_plane(config());
        seed_mock(
            &config_service,
            &server,
            route(&["GET", "POST", "PUT", "PATCH", "DELETE"], "/v1/things"),
        );

        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            let request = method_request(method, "/oagw/v1/proxy/api.openai.com/v1/things/42", &[]);
            let response = send(router.clone(), request).await;

            assert_eq!(
                response.status(),
                StatusCode::NO_CONTENT,
                "{method} must be forwarded, not answered with 405"
            );
        }

        assert_eq!(mock.calls(), 5);
    }

    #[tokio::test]
    async fn test_the_proxy_route_is_not_served_under_the_api_prefix() {
        let (router, _config_service) = data_plane(config());

        let response = send(router, get("/api/oagw/v1/proxy/alias/v1/status")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_the_proxy_alias_without_a_path_is_served() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/");
            then.status(200).body("root");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/"));

        let response = send(router, get("/oagw/v1/proxy/api.openai.com")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1);
    }

    // -- target selection --------------------------------------------------

    #[tokio::test]
    async fn test_a_single_endpoint_ignores_the_target_host_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        let request = Request::builder()
            .uri("/oagw/v1/proxy/api.openai.com/v1/status")
            .header(TARGET_HOST_HEADER, "nowhere.example.com")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK, "the header is ignored");
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn test_a_common_suffix_alias_requires_the_target_host_header() {
        let (router, config_service) = data_plane(config());
        seed_multi_endpoint(&config_service);

        let response = send(router, get("/oagw/v1/proxy/vendor.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[tokio::test]
    async fn test_a_malformed_target_host_is_rejected() {
        let (router, config_service) = data_plane(config());
        seed_multi_endpoint(&config_service);

        let request = Request::builder()
            .uri("/oagw/v1/proxy/vendor.com/v1/status")
            .header(TARGET_HOST_HEADER, "us.vendor.com:443/path")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
        );
    }

    #[tokio::test]
    async fn test_an_unknown_target_host_is_rejected() {
        let (router, config_service) = data_plane(config());
        seed_multi_endpoint(&config_service);

        let request = Request::builder()
            .uri("/oagw/v1/proxy/vendor.com/v1/status")
            .header(TARGET_HOST_HEADER, "ap.vendor.com")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
        );
    }

    #[tokio::test]
    async fn test_the_target_host_header_selects_the_endpoint() {
        let (router, config_service) = data_plane(config());
        seed_multi_endpoint(&config_service);

        let request = Request::builder()
            .uri("/oagw/v1/proxy/vendor.com/v1/status")
            .header(TARGET_HOST_HEADER, "eu.vendor.com")
            .body(Body::empty())
            .unwrap();
        let response = send(router, request).await;

        // Neither hostname resolves in the test environment, so the dial fails;
        // the `host` extension records which endpoint was selected.
        assert_eq!(body(response).await["host"], "eu.vendor.com");
    }

    fn seed_multi_endpoint(config_service: &ConfigService) {
        seed(
            config_service,
            json!({
                "protocol": PROTOCOL_HTTP,
                "server": {
                    "endpoints": [
                        { "host": "us.vendor.com", "port": 443 },
                        { "host": "eu.vendor.com", "port": 443 }
                    ]
                }
            }),
            route(&["GET"], "/v1"),
        );
    }

    #[tokio::test]
    async fn test_a_multi_endpoint_pool_without_a_target_host_round_robins() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        // IP literals are not derivable, so an explicit alias is required: this
        // is the "multi-endpoint explicit alias" row of the R6 matrix.
        seed(
            &config_service,
            json!({
                "alias": "my-service",
                "protocol": PROTOCOL_HTTP,
                "server": {
                    "endpoints": [
                        endpoint("127.0.0.1", server.port()),
                        endpoint("127.0.0.1", server.port())
                    ]
                }
            }),
            route(&["GET"], "/v1"),
        );

        for _ in 0..2 {
            let response = send(router.clone(), get("/oagw/v1/proxy/my-service/v1/status")).await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        assert_eq!(mock.calls(), 2, "both round-robin turns reach the pool");
    }

    #[test]
    fn test_the_round_robin_cursor_advances_per_upstream() {
        let round_robin = RoundRobin::default();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        assert_eq!(round_robin.next_index(first, 2), 0);
        assert_eq!(round_robin.next_index(first, 2), 1);
        assert_eq!(round_robin.next_index(first, 2), 0);
        assert_eq!(
            round_robin.next_index(second, 2),
            0,
            "cursors are per upstream"
        );
        assert_eq!(
            round_robin.next_index(second, 1),
            0,
            "a single endpoint is not rotated"
        );
    }

    // -- headers -----------------------------------------------------------

    #[tokio::test]
    async fn test_the_host_header_is_rewritten_to_the_upstream() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET)
                .path("/v1/status")
                .header("host", format!("127.0.0.1:{}", server.port()));
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[("host", "oagw.example.com")],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK, "the host is rewritten");
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn test_hop_by_hop_and_routing_headers_are_stripped_from_the_forwarded_request() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.is_true(|request| {
                let headers = request.headers();
                !headers.contains_key("connection")
                    && !headers.contains_key("keep-alive")
                    && !headers.contains_key("te")
                    && !headers.contains_key("trailer")
                    && !headers.contains_key("transfer-encoding")
                    && !headers.contains_key("proxy-authenticate")
                    && !headers.contains_key("proxy-authorization")
                    && !headers.contains_key("x-oagw-target-host")
            });
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[
                ("connection", "keep-alive, x-oagw-target-host"),
                ("keep-alive", "timeout=5"),
                ("te", "trailers"),
                ("trailer", "x-oagw-checksum"),
                ("transfer-encoding", "chunked"),
                ("proxy-authenticate", "Basic realm=upstream"),
                ("proxy-authorization", "Basic Zm9vOmJhcg=="),
                ("x-oagw-target-host", "127.0.0.1"),
            ],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1, "every stripped header was absent upstream");
    }

    #[tokio::test]
    async fn test_hop_by_hop_headers_are_stripped_from_the_response() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200)
                .header("connection", "keep-alive")
                .header("x-oagw-keep", "yes")
                .body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("connection").is_none());
        assert_eq!(response.headers().get("x-oagw-keep").unwrap(), "yes");
    }

    #[tokio::test]
    async fn test_request_header_rules_are_applied() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.is_true(|request| {
                let headers = request.headers();
                headers.get("x-oagw-added") == Some(&HeaderValue::from_static("by-gateway"))
                    && !headers.contains_key("x-oagw-dropped")
            });
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        let upstream = seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "api.openai.com",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [endpoint("127.0.0.1", server.port())] },
                    "headers": {
                        "request": {
                            "set": { "x-oagw-added": "by-gateway" },
                            "remove": ["x-oagw-dropped"]
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[("x-oagw-dropped", "inbound")],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn test_response_header_rules_are_applied() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        let upstream = seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "api.openai.com",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [endpoint("127.0.0.1", server.port())] },
                    "headers": { "response": { "set": { "x-oagw-response": "stamped" } } }
                }))
                .unwrap(),
            )
            .unwrap();

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-oagw-response").unwrap(),
            "stamped"
        );
    }

    #[tokio::test]
    async fn test_an_allowlisted_request_header_is_forwarded() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.is_true(|request| {
                request.headers().get("x-oagw-allow") == Some(&HeaderValue::from_static("yes"))
            });
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        let upstream = seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "api.openai.com",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [endpoint("127.0.0.1", server.port())] },
                    "headers": { "request": { "passthrough": "allowlist", "passthrough_allowlist": ["x-oagw-allow"] } }
                }))
                .unwrap(),
            )
            .unwrap();

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[("x-oagw-allow", "yes"), ("x-oagw-blocked", "no")],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1);
    }

    // -- guards ------------------------------------------------------------

    #[tokio::test]
    async fn test_an_unknown_query_parameter_is_rejected() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route_with_allowlist(&["model"]));

        let response = send(
            router,
            get("/oagw/v1/proxy/api.openai.com/v1/status?model=gpt-4&trace=1"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn test_an_allowlisted_query_parameter_is_forwarded() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET)
                .path("/v1/status")
                .query_param("model", "gpt-4");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route_with_allowlist(&["model"]));

        let response = send(
            router,
            get("/oagw/v1/proxy/api.openai.com/v1/status?model=gpt-4"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1);
    }

    fn route_with_allowlist(allowlist: &[&str]) -> Value {
        json!({
            "match": {
                "http": {
                    "methods": ["GET"],
                    "path": "/v1/status",
                    "query_allowlist": allowlist
                }
            }
        })
    }

    #[tokio::test]
    async fn test_a_path_suffix_is_rejected_when_the_mode_is_disabled() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", server.port())] }
            }),
            json!({
                "match": {
                    "http": {
                        "methods": ["GET"],
                        "path": "/v1/status",
                        "path_suffix_mode": "disabled"
                    }
                }
            }),
        );

        let response = send(
            router,
            get("/oagw/v1/proxy/api.openai.com/v1/status/history"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_source(&response), "gateway");
    }

    #[tokio::test]
    async fn test_a_non_integer_content_length_is_rejected() {
        let (router, config_service) = data_plane(config());
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["POST"], "/v1/chat"),
        );

        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/oagw/v1/proxy/api.openai.com/v1/chat")
            .header("content-length", "not-a-number")
            .body(Body::from("{}"))
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn test_a_transfer_encoding_other_than_chunked_is_rejected() {
        let (router, config_service) = data_plane(config());
        seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["POST"], "/v1/chat"),
        );

        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/oagw/v1/proxy/api.openai.com/v1/chat")
            .header("transfer-encoding", "gzip")
            .body(Body::from("{}"))
            .unwrap();
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -- CORS --------------------------------------------------------------

    #[tokio::test]
    async fn test_a_preflight_is_answered_locally_with_a_permissive_204() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::OPTIONS);
            then.status(500);
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["POST"], "/v1/chat"));

        let request = method_request(
            "OPTIONS",
            "/oagw/v1/proxy/api.openai.com/v1/chat",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "content-type, authorization",
                ),
            ],
        );
        let response = send(router, request).await;

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
            "content-type, authorization"
        );
        assert_eq!(
            response.headers().get("access-control-max-age").unwrap(),
            "86400"
        );
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(mock.calls(), 0, "a preflight is never forwarded");
    }

    #[tokio::test]
    async fn test_a_disallowed_cors_origin_is_rejected_with_403() {
        let (router, config_service) = data_plane(config());
        let upstream = seed(
            &config_service,
            json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [endpoint("127.0.0.1", 1)] }
            }),
            route(&["GET"], "/v1/status"),
        );

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "api.openai.com",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [endpoint("127.0.0.1", 1)] },
                    "cors": {
                        "enabled": true,
                        "allowed_origins": ["https://allowed.example.com"]
                    }
                }))
                .unwrap(),
            )
            .unwrap();

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[("origin", "https://evil.example.com")],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(error_source(&response), "gateway");
    }

    #[tokio::test]
    async fn test_an_allowed_cors_origin_is_answered_with_cors_headers() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("ok");
        });

        let (router, config_service) = data_plane(config());
        let upstream = seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        config_service
            .replace_upstream(
                tenant(),
                upstream,
                &serde_json::from_value(json!({
                    "alias": "api.openai.com",
                    "protocol": PROTOCOL_HTTP,
                    "server": { "endpoints": [endpoint("127.0.0.1", server.port())] },
                    "cors": {
                        "enabled": true,
                        "allowed_origins": ["https://allowed.example.com"],
                        "allow_credentials": true
                    }
                }))
                .unwrap(),
            )
            .unwrap();

        let request = method_request(
            "GET",
            "/oagw/v1/proxy/api.openai.com/v1/status",
            &[("origin", "https://allowed.example.com")],
        );
        let response = send(router, request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://allowed.example.com"
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-credentials")
                .unwrap(),
            "true"
        );
        assert_eq!(response.headers().get("vary").unwrap(), "Origin");
    }

    // -- plaintext policy --------------------------------------------------

    #[tokio::test]
    async fn test_a_plaintext_upstream_is_not_dialled_while_the_policy_forbids_it() {
        let (router, config_service) = data_plane(OagwConfig::default());

        // The management API rejects a plaintext pool while the policy forbids
        // it, so the store is seeded directly to reach the proxy-side check.
        // The management API would refuse to store a plaintext pool under this
        // policy, so the pool is validated as TLS and then flipped to plaintext
        // to reach the proxy-side check directly.
        let mut config = crate::domain::validation::validate_upstream(
            &serde_json::from_value::<UpstreamSpec>(json!({
                "alias": "api.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [{ "host": "127.0.0.1", "port": 1 }] }
            }))
            .unwrap(),
        )
        .expect("a TLS pool validates");

        config.server.endpoints[0].scheme = crate::domain::model::Scheme::Http;

        let upstream = crate::domain::model::Upstream::new(Uuid::new_v4(), tenant(), config);
        config_service
            .store()
            .insert_upstream(upstream.clone())
            .unwrap();

        // A route of the plaintext pool, stored the same direct way.
        let route_spec = serde_json::from_value::<RouteSpec>(json!({
            "upstream_id": upstream.id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }))
        .unwrap();
        let mut route_config =
            crate::domain::validation::validate_route(&route_spec).expect("a route validates");
        route_config.upstream_id = upstream.id;
        config_service
            .store()
            .insert_route(crate::domain::model::Route::new(
                Uuid::new_v4(),
                tenant(),
                route_config,
            ))
            .unwrap();
        seed(
            &config_service,
            json!({
                "alias": "second.openai.com",
                "protocol": PROTOCOL_HTTP,
                "server": { "endpoints": [{ "host": "10.0.1.5", "port": 443 }] }
            }),
            route(&["GET"], "/v1"),
        );

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body(response).await["detail"]
                .as_str()
                .unwrap()
                .contains("allow_http_upstream")
        );
    }

    #[tokio::test]
    async fn test_a_plaintext_upstream_is_dialled_while_the_policy_allows_it() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::GET).path("/v1/status");
            then.status(200).body("plaintext");
        });

        let (router, config_service) = data_plane(config());
        seed_mock(&config_service, &server, route(&["GET"], "/v1/status"));

        let response = send(router, get("/oagw/v1/proxy/api.openai.com/v1/status")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.calls(), 1);
    }
}
