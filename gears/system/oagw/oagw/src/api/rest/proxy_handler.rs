//! The proxy handlers behind the registered shell
//! (`cpt-cf-oagw-feature-proxy-pipeline`).
//!
//! The module is the API handler of the proxy path: it reads the request the
//! shell routed to `/oagw/v1/proxy/{alias}[/{*path}]` into the classified
//! [`ProxyRequest`] the pipeline takes, performs the outbound call through
//! [`ProxyPipeline::handle`], and renders what comes back — the preflight 204,
//! the upstream passthrough, or the problem+json rejection the closed table
//! maps. The pipeline owns every rule; this module owns only the wire.

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, Path, RawQuery, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use toolkit_security::SecurityContext;

use crate::api::rest::error::{
    ERROR_SOURCE_GATEWAY, X_OAGW_ERROR_SOURCE, error_response, upstream_passthrough,
};
use crate::domain::error::OagwError;
use crate::domain::proxy::{ProxyRequest, parse_query};
use crate::infra::proxy::pipeline::{ProxyOutcome, ProxyPipeline, ProxyRejection, UpgradeOutcome};
use hyper::upgrade::OnUpgrade;

// @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-01
// Receive the proxied request from the API handler on the registered shell
// `{METHOD} /oagw/v1/proxy/{alias}/{path}` — the method, the normalized alias,
// the path suffix, the query string, the headers and the body stream, together
// with the security context the platform `toolkit-auth` middleware established
// for the `gts.cf.core.oagw.proxy.v1~:invoke` permission — and open the
// `ProxyContext` of §4 in its `Classified` state, recording the arrival instant
// that both the request timeout and the `<10ms` latency budget are measured
// from; the alias arrives already normalized by
// `cpt-cf-oagw-algo-alias-normalization` of
// `cpt-cf-oagw-feature-alias-resolution` and no case handling of its own is
// performed. The pipeline opens the context and records that instant: the
// handler only classifies the request the shell routed and hands it over.
// @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pf-01

/// The handler of `{METHOD} /oagw/v1/proxy/{alias}`: the alias-only proxy path.
#[allow(clippy::too_many_arguments)] // the extractor list is the wire shape
pub async fn proxy_alias(
    State(pipeline): State<Arc<ProxyPipeline>>,
    method: axum::http::Method,
    Path(alias): Path<String>,
    headers: axum::http::HeaderMap,
    query: RawQuery,
    security: Option<Extension<SecurityContext>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    upgrade: Option<Extension<OnUpgrade>>,
    body: axum::body::Bytes,
) -> Response {
    proxy(
        pipeline,
        method,
        alias,
        String::new(),
        query,
        headers,
        security,
        peer,
        upgrade,
        body,
    )
    .await
}

/// The handler of `{METHOD} /oagw/v1/proxy/{alias}/{*path}`: the proxy path
/// carrying a path suffix.
#[allow(clippy::too_many_arguments)] // the extractor list is the wire shape
pub async fn proxy_path(
    State(pipeline): State<Arc<ProxyPipeline>>,
    method: axum::http::Method,
    Path((alias, suffix)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    query: RawQuery,
    security: Option<Extension<SecurityContext>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    upgrade: Option<Extension<OnUpgrade>>,
    body: axum::body::Bytes,
) -> Response {
    proxy(
        pipeline, method, alias, suffix, query, headers, security, peer, upgrade, body,
    )
    .await
}

/// Classifies one proxied request and drives the pipeline with it.
///
/// The path suffix is normalized to the leading-slash form the route matcher
/// compares a `match.http.path` prefix against, whether the shell captured a
/// suffix at all.
#[allow(clippy::too_many_arguments)]
async fn proxy(
    pipeline: Arc<ProxyPipeline>,
    method: axum::http::Method,
    alias: String,
    suffix: String,
    query: RawQuery,
    headers: axum::http::HeaderMap,
    security: Option<Extension<SecurityContext>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    upgrade: Option<Extension<OnUpgrade>>,
    body: Bytes,
) -> Response {
    let Some(Extension(security)) = security else {
        // A request the platform auth middleware did not authenticate reaches
        // no stage of the pipeline and is answered by the existing
        // `AuthenticationFailed` row.
        return error_response(&OagwError::authentication_failed(
            "oagw.proxy: the proxy surface answers only an authenticated caller carrying a \
             security context",
        ));
    };
    let body_len = u64::try_from(body.len()).unwrap_or(u64::MAX);
    let request = ProxyRequest {
        method: method.as_str().to_owned(),
        alias,
        path_suffix: format!("/{suffix}"),
        query: parse_query(query.0.as_deref().unwrap_or_default()),
        headers,
        body_len,
    };
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-05
    // Receive the upstream-origin failure returned on the proxy path by the
    // upstream service: either a connection-level failure the gateway itself
    // classified as an `OagwError`, or an error response the upstream service
    // itself produced.
    match pipeline
        .handle(&request, &security, peer_of(peer), body)
        .await
    {
        Ok(outcome) => match outcome {
            ProxyOutcome::Preflight(headers) => preflight(headers),
            // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-08
            // ELSE the response was produced by the upstream service.
            ProxyOutcome::Upstream(reply) => upstream(reply),
            // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-08
            ProxyOutcome::Upgrade(outcome) => upgraded(
                Arc::clone(&pipeline),
                outcome,
                upgrade.map(|Extension(it)| it),
            ),
        },
        Err(rejection) => rejected(rejection),
    }
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-05
}

/// The peer address the connection carried, unspecified when the router the
/// gear was handed reports none.
fn peer_of(peer: Option<Extension<ConnectInfo<SocketAddr>>>) -> IpAddr {
    peer.map_or_else(|| IpAddr::from([0, 0, 0, 0]), |info| info.0.ip())
}

/// Renders the preflight 204 the CORS check answered before anything resolved.
///
/// The status and the headers of that response are OAGW-generated, so it carries
/// `X-OAGW-Error-Source: gateway` (`cpt-cf-oagw-dod-error-source-header`).
fn preflight(headers: crate::domain::proxy::PreflightHeaders) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let pairs = [
        ("access-control-allow-origin", headers.allow_origin),
        ("access-control-allow-methods", headers.allow_methods),
        ("access-control-allow-headers", headers.allow_headers),
        ("access-control-max-age", headers.max_age.to_string()),
        ("vary", headers.vary.to_owned()),
    ];
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(&value)) {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        HeaderName::from_static(crate::api::rest::error::X_OAGW_ERROR_SOURCE),
        HeaderValue::from_static(crate::api::rest::error::ERROR_SOURCE_GATEWAY),
    );
    response
}

/// Renders the upstream passthrough: status, headers and body as received.
fn upstream(reply: crate::infra::proxy::pipeline::UpstreamReply) -> Response {
    let body = Body::new(reply.body);
    let mut response = Response::new(body);
    *response.status_mut() = reply.status;
    *response.headers_mut() = reply.headers;
    upstream_passthrough(response)
}

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-13
// Relay the upstream `101 Switching Protocols` to the client with the handshake
// headers the RFC 6455 exchange produced, the status and headers of that
// response being OAGW-generated on the client side and therefore carrying
// `X-OAGW-Error-Source: gateway` per §1.5. The relay is the response head only:
// the session's bytes are relayed by the pump this hands the connection over
// to, which runs for as long as the session lives and never re-serialises a
// failure onto it.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-13
///
/// The client half of the upgrade is the one the connection itself hands over:
/// the response this renders carries no body, the server half of the RFC 6455
/// session being fulfilled once the head is written. When the connection
/// carries no such half the 101 cannot be switched and the closed
/// `ProtocolError` row answers, nothing having been relayed yet.
fn upgraded(
    pipeline: Arc<ProxyPipeline>,
    outcome: UpgradeOutcome,
    upgrade: Option<OnUpgrade>,
) -> Response {
    let UpgradeOutcome {
        status,
        headers,
        tunnel,
    } = outcome;
    let Some(upgrade) = upgrade else {
        return error_response(&OagwError::protocol_error(
            "oagw.proxy: the client connection carries no upgrade half to switch",
        ));
    };
    // The pump outlives this handler: the session is pumped from its own task,
    // which ends when either side closes and never touches this response.
    tokio::spawn(async move {
        pipeline.relay_tunnel(upgrade, tunnel).await;
    });
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        HeaderName::from_static(X_OAGW_ERROR_SOURCE),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// Renders a gateway rejection through the closed table, carrying the headers
/// the refusing stage produced — the `Retry-After` and the three
/// `X-RateLimit-*` headers of a 429 — unchanged.
fn rejected(rejection: ProxyRejection) -> Response {
    let mut response = error_response(&rejection.error);
    for (name, value) in rejection.headers {
        if let Some(name) = name {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use super::*;
    use crate::api::control_plane::ControlPlaneService;
    use crate::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM, PROBLEM_JSON};
    use crate::api::rest::route_shell::{self, MountLedger};
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchConfig, PROTOCOL_HTTP, Route, ServerConfig, Upstream,
    };
    use crate::domain::repo::{RouteRepository, UpstreamRepository};
    use crate::infra::proxy::pipeline::stub::{self, StubConnector, StubReply};
    use crate::infra::proxy::pipeline::{OutboundBody, UpstreamConnector};
    use crate::infra::storage::InMemoryStores;
    use axum::Router;
    use axum::body::to_bytes as read_body;
    use axum::extract::RawQuery;
    use axum::http::Method;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    const SUBJECT: uuid::Uuid = uuid::Uuid::from_u128(0xbe11);
    const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0xbe11_0001);
    const UPSTREAM_ID: uuid::Uuid = uuid::Uuid::from_u128(0xbe11_0002);
    const ALIAS: &str = "payments";
    const HOST: &str = "api.vendor.com";
    const PEER: &str = "127.0.0.1:41000";

    /// A security context the platform auth middleware would have established.
    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    /// The middleware the platform `toolkit-auth` layer stands for: it
    /// establishes the security context and the connection peer every proxy
    /// request the shell routes carries.
    async fn inject_security(
        mut request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> Response {
        request.extensions_mut().insert(ConnectInfo(
            PEER.parse::<SocketAddr>().expect("the peer address parses"),
        ));
        request.extensions_mut().insert(security());
        next.run(request).await
    }

    /// The proxy surface as the shell mounts it, over a control plane whose
    /// stores hold the fixture upstream and route, and over `connector`.
    fn surface(connector: Arc<StubConnector>) -> Router {
        let control_plane = Arc::new(ControlPlaneService::default());
        let stores: &InMemoryStores = control_plane.stores();
        seed(stores);
        let pipeline = stub::pipeline(stores, connector);
        route_shell::mount(Router::new(), &MountLedger::new(), control_plane, pipeline)
            .expect("the shell mounts on a fresh ledger")
            .layer(axum::middleware::from_fn(inject_security))
    }

    /// Stores the enabled HTTPS upstream under [`ALIAS`] and the GET+POST route
    /// matching `/api`, the fixture every test of this module proxies through.
    fn seed(stores: &InMemoryStores) {
        stores
            .upstreams()
            .insert(&Upstream {
                id: Some(UPSTREAM_ID),
                tenant_id: Some(TENANT),
                enabled: true,
                alias: Some(ALIAS.to_owned()),
                protocol: Some(PROTOCOL_HTTP.to_owned()),
                server: Some(ServerConfig {
                    endpoints: vec![Endpoint::new("https", HOST, 443)],
                }),
                ..Upstream::default()
            })
            .expect("the fixture upstream is well formed");
        stores
            .routes()
            .insert(&Route {
                id: Some(uuid::Uuid::new_v4()),
                tenant_id: Some(TENANT),
                enabled: true,
                priority: 10,
                upstream_id: Some(UPSTREAM_ID),
                match_config: Some(MatchConfig {
                    http: Some(HttpMatch {
                        methods: vec!["GET".to_owned(), "POST".to_owned()],
                        path: Some("/api".to_owned()),
                        query_allowlist: vec!["a".to_owned()],
                        path_suffix_mode: "append".to_owned(),
                    }),
                    grpc: None,
                }),
                ..Route::default()
            })
            .expect("the fixture route is well formed");
    }

    /// Sends one request to the surface.
    async fn send(
        router: Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> axum::response::Response {
        let mut builder = HttpRequest::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(bytes) => builder.body(axum::body::Body::from(bytes.to_vec())),
            None => builder.body(axum::body::Body::empty()),
        }
        .expect("the request is well formed");
        router.oneshot(request).await.expect("the service answers")
    }

    /// One response header as text.
    fn header_of(response: &axum::response::Response, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// A fresh stub connector the surface drives.
    fn connector() -> Arc<StubConnector> {
        Arc::new(StubConnector::new())
    }

    /// The header set a browser preflight carries.
    fn preflight_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            axum::http::header::ACCESS_CONTROL_REQUEST_METHOD,
            axum::http::HeaderValue::from_static("GET"),
        );
        headers
    }

    /// The body of a response the test reads in full.
    async fn body(response: axum::response::Response) -> bytes::Bytes {
        read_body(response.into_body(), usize::MAX).await.unwrap()
    }

    /// A canned upstream answer of `status` and `content_type`.
    fn reply(status: StatusCode, content_type: &str, body: &str) -> StubReply {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_static("content-type"),
            http::HeaderValue::from_str(content_type).expect("the content type is a header value"),
        );
        StubReply {
            status,
            headers,
            body: bytes::Bytes::from(body.to_owned()),
            version: http::Version::HTTP_11,
        }
    }

    /// Acceptance criterion 1 over the wire: a proxied request reaches the
    /// endpoint once and comes back unchanged apart from the gateway's header
    /// work, carrying the upstream error-source header and never a problem
    /// document.
    #[tokio::test]
    async fn a_proxied_request_passes_through_the_mounted_shell() {
        let stub = connector();
        stub.push(reply(StatusCode::OK, "application/json", "{\"ok\":true}"));
        let router = surface(stub);

        let response = send(
            router,
            "GET",
            "/oagw/v1/proxy/payments/api/v1/users?a=1",
            &[],
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, "content-type").as_deref(),
            Some("application/json"),
            "the upstream Content-Type is forwarded untouched"
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_UPSTREAM),
            "a proxied response carries the upstream source header"
        );
        assert_ne!(
            header_of(&response, "content-type").as_deref(),
            Some(PROBLEM_JSON),
            "the upstream body is never re-serialized into problem+json"
        );
        assert_eq!(
            body(response).await.as_ref(),
            b"{\"ok\":true}",
            "the upstream body is forwarded byte for byte"
        );
    }

    /// The path suffix the shell captured reaches the pipeline in the
    /// leading-slash form the route matcher compares against, and the query the
    /// client sent reaches the outbound request untouched.
    #[tokio::test]
    async fn the_captured_suffix_reaches_the_pipeline_with_a_leading_slash() {
        let stub = connector();
        let router = surface(Arc::clone(&stub));

        let response = send(
            router,
            "POST",
            "/oagw/v1/proxy/payments/api/v1/users?a=1",
            &[],
            Some(b"payload"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let received = stub.received();
        assert_eq!(received.len(), 1, "the endpoint is called once");
        let outbound = &received[0].request;
        assert_eq!(
            outbound.uri().path(),
            "/api/v1/users",
            "the `{{*path}}` capture is the remainder without its leading slash, so the \
             handler re-attaches it"
        );
        assert_eq!(outbound.uri().query(), Some("a=1"));
        assert_eq!(
            outbound.method(),
            http::Method::POST,
            "the method the client used is the method the pipeline sends"
        );
    }

    /// Acceptance criterion 3: a preflight that reaches the handler is answered
    /// 204 with the gateway source header and the echoed CORS headers, before
    /// the alias is resolved at all. The handler is driven directly, because the
    /// closed method set of the shell the gear registers carries no `OPTIONS`
    /// route — the preflight branch is a decision of the pipeline this handler
    /// fronts, not of the routing layer.
    #[tokio::test]
    async fn a_preflight_reaching_the_handler_is_answered_204_with_the_gateway_source() {
        let stub = connector();
        let control_plane = Arc::new(ControlPlaneService::default());
        let pipeline = stub::pipeline(
            control_plane.stores(),
            Arc::clone(&stub) as Arc<dyn UpstreamConnector>,
        );

        let response = proxy_path(
            State(Arc::clone(&pipeline)),
            Method::OPTIONS,
            Path(("no-such-alias".to_owned(), "api".to_owned())),
            preflight_headers(),
            RawQuery(None),
            Some(Extension(security())),
            Some(Extension(ConnectInfo(
                PEER.parse::<SocketAddr>().expect("the peer address parses"),
            ))),
            None,
            Bytes::new(),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "the preflight is answered before the alias is resolved at all"
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_GATEWAY),
            "the 204 the gateway produced carries the gateway source header"
        );
        assert_eq!(
            header_of(&response, "access-control-allow-origin").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(
            header_of(&response, "access-control-allow-methods").as_deref(),
            Some("GET")
        );
        assert!(
            header_of(&response, "access-control-max-age").is_some(),
            "the preflight carries Access-Control-Max-Age"
        );
        assert_eq!(
            header_of(&response, "vary").as_deref(),
            Some(crate::domain::proxy::VARY_PREFLIGHT),
            "the preflight varies on every request header the echo depends on"
        );
        assert!(
            stub.received().is_empty(),
            "no outbound call is made for a preflight"
        );
    }

    /// The closed proxy shell accepts `OPTIONS` among the methods
    /// `cpt-cf-oagw-feature-gear-wiring` registers, so a browser preflight
    /// reaches the pipeline's preflight branch over the wire and is answered
    /// with the permissive 204 and the gateway source header, before the alias
    /// is resolved at all.
    #[tokio::test]
    async fn the_closed_shell_answers_a_browser_preflight_with_the_permissive_204() {
        let router = surface(connector());

        let response = send(
            router,
            "OPTIONS",
            "/oagw/v1/proxy/payments/api",
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "GET"),
                ("access-control-request-headers", "x-trace-id"),
            ],
            None,
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "the preflight is answered at handler level, not by the routing layer"
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_GATEWAY),
            "the 204 is OAGW-generated, so it carries the gateway source"
        );
        assert_eq!(
            header_of(&response, "access-control-allow-origin").as_deref(),
            Some("https://app.example.com"),
            "the requested origin is echoed"
        );
        assert_eq!(
            header_of(&response, "access-control-allow-methods").as_deref(),
            Some("GET"),
            "the requested method is echoed"
        );
        assert_eq!(
            header_of(&response, "access-control-allow-headers").as_deref(),
            Some("x-trace-id"),
            "the requested headers are echoed"
        );
        assert!(
            header_of(&response, "access-control-max-age").is_some(),
            "the preflight carries the max age"
        );
        assert_ne!(
            header_of(&response, "content-type").as_deref(),
            Some(PROBLEM_JSON),
            "no problem body is invented for a preflight"
        );
    }

    /// Acceptance criterion 24: every gateway-produced body is
    /// `application/problem+json` carrying the five members, with the gateway
    /// source header and the request path as the `instance`.
    #[tokio::test]
    async fn a_gateway_rejection_is_problem_json_with_the_gateway_source() {
        let router = surface(connector());

        let response = send(router, "GET", "/oagw/v1/proxy/no-such-alias/api", &[], None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_of(&response, "content-type").as_deref(),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_GATEWAY)
        );
        let document: serde_json::Value = serde_json::from_slice(&body(response).await).unwrap();
        assert_eq!(
            document["type"], "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "the closed table row is carried, and no row is added to it"
        );
        assert_eq!(document["status"], 404);
        assert_eq!(
            document["instance"], "/oagw/v1/proxy/no-such-alias/api",
            "the instance is the path the client called"
        );
        for member in ["type", "title", "status", "detail", "instance"] {
            assert!(
                document.get(member).is_some(),
                "the problem document carries {member}"
            );
        }
    }

    /// An upstream error status passes through the shell as-is, its body never
    /// re-serialized, with the upstream source header.
    #[tokio::test]
    async fn an_upstream_error_status_is_not_re_rendered_by_the_shell() {
        let stub = connector();
        stub.push(reply(
            StatusCode::BAD_GATEWAY,
            "text/plain",
            "upstream text",
        ));
        let router = surface(stub);

        let response = send(router, "GET", "/oagw/v1/proxy/payments/api", &[], None).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            header_of(&response, "content-type").as_deref(),
            Some("text/plain"),
            "the upstream sent this Content-Type, and the gateway does not replace it"
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_UPSTREAM)
        );
        assert_eq!(body(response).await.as_ref(), b"upstream text");
    }

    /// The alias-only proxy path of the shell serves the same pipeline.
    #[tokio::test]
    async fn the_alias_only_path_answers_through_the_same_pipeline() {
        let router = surface(connector());

        let response = send(router, "GET", "/oagw/v1/proxy/no-such-alias", &[], None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_of(&response, "content-type").as_deref(),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_GATEWAY)
        );
    }

    /// A request without a security context reaches no stage of the pipeline:
    /// the handler refuses it with the `AuthenticationFailed` row before the
    /// alias is resolved.
    #[tokio::test]
    async fn an_unauthenticated_proxy_request_is_refused_by_the_handler() {
        let control_plane = Arc::new(ControlPlaneService::default());
        let pipeline = stub::pipeline(control_plane.stores(), connector());
        let router =
            route_shell::mount(Router::new(), &MountLedger::new(), control_plane, pipeline)
                .expect("the shell mounts on a fresh ledger");

        let response = send(router, "GET", "/oagw/v1/proxy/payments/api", &[], None).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            header_of(&response, "content-type").as_deref(),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            header_of(&response, "x-oagw-error-source").as_deref(),
            Some(ERROR_SOURCE_GATEWAY)
        );
        let document: serde_json::Value = serde_json::from_slice(&body(response).await).unwrap();
        assert_eq!(
            document["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
        );
    }

    /// A body the handler buffered is forwarded once and whole: the client
    /// bytes are the bytes the endpoint received. Under the upstream's default
    /// `passthrough: none` posture no inbound header is forwarded, so the
    /// body is the only thing the hop carries of the client request.
    #[tokio::test]
    async fn the_buffered_body_is_forwarded_once_and_whole() {
        let stub = connector();
        stub.push(reply(StatusCode::OK, "application/json", "{}"));
        let router = surface(Arc::clone(&stub));

        let response = send(
            router,
            "POST",
            "/oagw/v1/proxy/payments/api/v1/users",
            &[("content-type", "application/json")],
            Some(b"{\"amount\":42}"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let received = stub.received();
        assert_eq!(
            received.len(),
            1,
            "the buffered body is read once and sent once"
        );
        let outbound = &received[0].request;
        match outbound.body() {
            OutboundBody::Full(bytes) => assert_eq!(
                bytes.as_ref(),
                b"{\"amount\":42}",
                "the bytes the client sent are the bytes the endpoint received"
            ),
            OutboundBody::Streaming(_) => panic!("a buffered client body is forwarded in full"),
        }
        assert!(
            outbound.headers().get("content-type").is_none(),
            "the default passthrough posture forwards no inbound header"
        );
    }
}
