// Data Plane regression tests for the review findings.
//
// Each test pins one finding of the Data Plane review through the real
// transport: the SSRF policy, the local CORS preflight, the hop-by-hop filter
// in both directions, the `X-RateLimit-*` headers, the per-resource rate-limit
// identity, the ignored client forwarding headers and the correlation
// identifier shared by the audit line and the `X-Request-ID` response header.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    Harness, ProxyOptions, TestUpstream, context_for, echo_upstream, gateway_request, respond, text,
};
use hyper_util::rt::TokioIo;
use oagw::domain::model::{
    BurstCapacity, CorsConfig, Endpoint, EndpointScheme, HeadersConfig, PassthroughMode,
    RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, SharingMode, SustainedRate,
};
use oagw::domain::services::management::ControlPlaneService;
use oagw::infra::metrics::OagwMetrics;
use oagw::infra::plugin::BuiltinPlugins;
use tenant_resolver_sdk::TenantId;
use toolkit::api::operation_builder::OperationSpec;
use tracing::field::{Field, Visit};
use tracing::{Event, Id, Metadata};

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

fn protected_options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: true,
    }
}

/// A private token bucket of `capacity` tokens per second.
fn rate_limit(capacity: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: capacity,
            window: oagw::domain::model::RateWindow::Second,
        },
        burst: Some(BurstCapacity { capacity }),
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

/// A token bucket per client address.
fn ip_rate_limit(capacity: u32) -> RateLimitConfig {
    RateLimitConfig {
        scope: RateScope::Ip,
        ..rate_limit(capacity)
    }
}

/// A CORS policy allowing one explicit origin, GET and POST.
fn cors_config() -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: false,
    }
}

/// Forward every inbound header upstream.
fn passthrough_all() -> HeadersConfig {
    HeadersConfig {
        request: Some(oagw::domain::model::RequestHeaderRules {
            set: Default::default(),
            add: Default::default(),
            remove: Vec::new(),
            passthrough: PassthroughMode::All,
            passthrough_allowlist: Vec::new(),
        }),
        response: None,
    }
}

/// An upstream with `alias` on `host:port`, created through the Control Plane
/// so the alias, the header rules and the endpoint are the real ones.
async fn create_upstream(
    harness: &Harness,
    ctx: &toolkit_security::SecurityContext,
    alias: &str,
    host: &str,
    port: u16,
) -> oagw::domain::model::Upstream {
    let mut shell = common::upstream_shell(port);
    shell.alias = alias.to_owned();
    shell.server.endpoints[0] = Endpoint {
        scheme: EndpointScheme::Http,
        host: host.to_owned(),
        port,
    };
    harness
        .control_plane()
        .create_upstream(ctx, shell)
        .await
        .expect("upstream")
}

/// Binds the built-in `X-Request-ID` transform plugin on `upstream`, so the
/// response carries the correlation identifier of the request.
async fn bind_request_id_transform(
    harness: &Harness,
    ctx: &toolkit_security::SecurityContext,
    upstream: &oagw::domain::model::Upstream,
) {
    let mut replaced = upstream.clone();
    replaced.plugins = Some(oagw::domain::model::PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![oagw::domain::model::PluginBinding::Bare(
            oagw::domain::gts::TRANSFORM_PLUGIN_REQUEST_ID.to_owned(),
        )],
    });
    harness
        .control_plane()
        .replace_upstream(ctx, &upstream.id.to_string(), replaced)
        .await
        .expect("upstream with the request-id transform");
}

/// An upstream that echoes method, path and every value of `header`.
async fn header_echo_upstream(header: &'static str) -> TestUpstream {
    TestUpstream::start(move |request| async move {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();
        let host = request
            .headers()
            .get("host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let seen = request
            .headers()
            .get_all(header)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join("|");
        respond(
            StatusCode::OK,
            format!("{method} {path} {header}={seen} host={host}"),
        )
    })
    .await
}

/// An echo upstream bound to the IPv6 loopback address.
async fn echo_upstream_v6() -> TestUpstream {
    let listener = tokio::net::TcpListener::bind("[::1]:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|request| async move {
                    let method = request.method().to_string();
                    let path = request.uri().path().to_string();
                    let host = request
                        .headers()
                        .get("host")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    Ok::<_, std::convert::Infallible>(respond(
                        StatusCode::OK,
                        format!("{method} {path} host={host}"),
                    ))
                });
                let io = TokioIo::new(socket);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    TestUpstream { address }
}

// -------------------------------------------------------- the audit line ----

/// A captured `proxy.request` audit line.
type AuditLine = HashMap<String, String>;

/// Every audit line captured so far.
type CapturedLines = Arc<Mutex<Vec<AuditLine>>>;

/// A minimal `tracing` subscriber that records the fields of every event.
#[derive(Clone, Default)]
struct AuditCapture {
    lines: CapturedLines,
}

struct Recorder<'a> {
    fields: &'a mut HashMap<String, String>,
}

impl Visit for Recorder<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for AuditCapture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = HashMap::new();
        event.record(&mut Recorder {
            fields: &mut fields,
        });
        self.lines.lock().expect("capture").push(fields);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}

    fn clone_span(&self, span: &Id) -> Id {
        span.clone()
    }

    fn drop_span(&self, span: Id) {
        self.try_close(span);
    }
}

impl AuditCapture {
    /// Every recorded line that carries the audit event marker.
    fn audit_lines(&self) -> Vec<AuditLine> {
        self.lines
            .lock()
            .expect("capture")
            .iter()
            .filter(|line| line.get("event").map(String::as_str) == Some("proxy.request"))
            .cloned()
            .collect()
    }
}

/// The process-wide audit capture.
///
/// `tracing`'s thread-local default subscriber only sees events emitted by the
/// thread that installed it, so the capture is installed once as the global
/// default instead: every test then reads the lines of its own request out of
/// the shared capture, filtering on the request identifier it minted.
fn audit_capture() -> &'static AuditCapture {
    static CAPTURE: std::sync::OnceLock<AuditCapture> = std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = AuditCapture::default();
        // Another test of this binary may have won the race; the capture it
        // installed is this very one, so a rejected install is harmless.
        let _ = tracing::subscriber::set_global_default(capture.clone());
        capture
    })
}

/// The value of a response header.
fn header(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

// ---------------------------------------------------------------- F-001 ----

/// An endpoint whose host is not a globally routable public unicast address is
/// refused, in both the IPv4 and the IPv6 form — including the IPv4-mapped,
/// unique-local and link-local IPv6 spellings a numeric-only check lets
/// through.
#[tokio::test]
async fn ssrf_blocks_non_routable_endpoint_hosts() {
    for host in [
        "::ffff:169.254.169.254",
        "fc00::1",
        "fe80::1",
        "0.1.2.3",
        "100.64.0.1",
    ] {
        let harness = Harness::new(protected_options());
        let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
        let created = create_upstream(&harness, &ctx, "ssrf", host, 1).await;
        harness
            .control_plane()
            .create_route(&ctx, common::http_route(created.id, "/", &["GET"]))
            .await
            .expect("route");

        let mut response = harness
            .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/ssrf/v1/items"))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{host}");
        assert_eq!(
            header(&response, "x-oagw-error-source").as_deref(),
            Some("gateway"),
            "{host}"
        );
        let body = text(&mut response).await;
        assert!(
            body.contains("cf.oagw.routing.invalid_target_host.v1"),
            "{host}: {body}"
        );
    }
}

/// A public address is still relayed, so the policy is not a blanket IP ban.
#[tokio::test]
async fn ssrf_still_relays_a_public_endpoint() {
    let upstream = echo_upstream().await;
    // The loopback upstream itself needs the guard off.
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = create_upstream(&harness, &ctx, "local", "127.0.0.1", upstream.port()).await;
    harness
        .control_plane()
        .create_route(&ctx, common::http_route(created.id, "/", &["GET"]))
        .await
        .expect("route");
    let response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// The gateway addresses an upstream *origin server*, so the request line must
/// carry origin form (`GET /v1/items`), not the absolute form a client may send
/// to a proxy. `hyper` normalises the request target on the receiving side, so
/// this is pinned against a raw socket that echoes the head it was handed.
#[tokio::test]
async fn the_request_target_is_sent_in_origin_form() {
    let upstream = common::RawUpstream::start("{\"ok\": true}").await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = create_upstream(
        &harness,
        &ctx,
        "origin",
        "127.0.0.1",
        upstream.address.port(),
    )
    .await;
    harness
        .control_plane()
        .create_route(&ctx, common::http_route(created.id, "/", &["GET"]))
        .await
        .expect("route");

    let response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/origin/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let request_line = upstream.request_line().await;
    assert_eq!(
        request_line, "GET /v1/items HTTP/1.1",
        "the upstream must receive origin form, got: {request_line:?}"
    );
}

// ---------------------------------------------------------------- F-002 ----

/// A preflight is answered locally with the CORS headers, and the upstream is
/// never contacted (nothing listens on the endpoint port).
#[tokio::test]
async fn a_preflight_is_answered_locally() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = create_upstream(&harness, &ctx, "local", "127.0.0.1", 1).await;
    let mut route = common::http_route(created.id, "/", &["GET", "OPTIONS"]);
    route.cors = Some(cors_config());
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/local/v1/items")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "x-trace")
        .body(Body::empty())
        .expect("request");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header(&response, "access-control-allow-methods").as_deref(),
        Some("GET, POST")
    );
    assert_eq!(
        header(&response, "access-control-allow-headers").as_deref(),
        Some("x-trace")
    );
    assert_eq!(
        header(&response, "access-control-max-age").as_deref(),
        Some("86400")
    );
    let vary = header(&response, "vary").unwrap_or_default();
    assert!(vary.contains("Origin"), "{vary}");
    assert!(
        vary.contains("Access-Control-Request-Method")
            && vary.contains("Access-Control-Request-Headers"),
        "{vary}"
    );
    assert!(text(&mut response).await.is_empty(), "no body");
}

/// An origin outside `allowed_origins` is refused before the upstream.
#[tokio::test]
async fn a_foreign_origin_preflight_is_refused() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = create_upstream(&harness, &ctx, "local", "127.0.0.1", 1).await;
    let mut route = common::http_route(created.id, "/", &["GET", "OPTIONS"]);
    route.cors = Some(cors_config());
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/local/v1/items")
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .expect("request");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(header(&response, "access-control-allow-origin").is_none());
    let body = text(&mut response).await;
    assert!(
        body.contains("cf.oagw.cors.origin_not_allowed.v1"),
        "{body}"
    );
}

/// A relayed cross-origin response carries the allow-origin header.
#[tokio::test]
async fn a_relayed_cross_origin_response_carries_the_cors_headers() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, common::upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = common::http_route(created.id, "/", &["GET"]);
    route.cors = Some(cors_config());
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/local/v1/items")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .expect("request");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header(&response, "access-control-expose-headers").as_deref(),
        Some("x-request-id")
    );
    // Same-origin and non-browser requests are untouched.
    let plain = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert!(header(&plain, "access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------- F-029 ----

/// An IPv6 endpoint host reaches the upstream as a bracketed authority.
#[tokio::test]
async fn an_ipv6_endpoint_host_produces_a_bracketed_uri() {
    let upstream = echo_upstream_v6().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = create_upstream(&harness, &ctx, "local", "::1", upstream.port()).await;
    harness
        .control_plane()
        .create_route(&ctx, common::http_route(created.id, "/", &["GET"]))
        .await
        .expect("route");

    let mut response = harness
        .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/local/v1/feed"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.starts_with("GET /v1/feed"), "{body}");
    assert!(
        body.contains(&format!("host=[::1]:{}", upstream.port())),
        "the authority must be bracketed: {body}"
    );
}

// ---------------------------------------------------------------- F-031 ----

/// Repeated request headers survive the passthrough, and a token named by
/// `Connection` is dropped on the way out.
#[tokio::test]
async fn repeated_headers_survive_and_connection_named_ones_are_stripped() {
    let upstream = header_echo_upstream("x-trace").await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let mut shell = common::upstream_shell(upstream.port());
    shell.headers = Some(passthrough_all());
    let created = harness
        .control_plane()
        .create_upstream(&ctx, shell)
        .await
        .expect("upstream");
    harness
        .control_plane()
        .create_route(&ctx, common::http_route(created.id, "/", &["GET"]))
        .await
        .expect("route");

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/local/v1/echo")
        .header("x-trace", "one")
        .header("x-trace", "two")
        .header("connection", "x-secret, keep-alive")
        .header("x-secret", "leaked")
        .body(Body::empty())
        .expect("request");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(&mut response).await;
    assert!(body.contains("x-trace=one|two"), "{body}");
    assert!(!body.contains("leaked"), "{body}");
}

/// The response side strips the hop-by-hop headers too.
#[tokio::test]
async fn the_response_side_strips_hop_by_hop_headers() {
    let upstream = TestUpstream::start(|_| async {
        let mut response = respond(StatusCode::OK, "body");
        response.headers_mut().insert(
            "connection",
            axum::http::HeaderValue::from_static("x-internal"),
        );
        response
            .headers_mut()
            .append("x-internal", axum::http::HeaderValue::from_static("secret"));
        response
            .headers_mut()
            .append("x-oagw-kept", axum::http::HeaderValue::from_static("kept"));
        response
    })
    .await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("route");

    let response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "x-internal").is_none());
    assert!(header(&response, "connection").is_none());
    assert_eq!(header(&response, "x-oagw-kept").as_deref(), Some("kept"));
}

// ---------------------------------------------------------------- F-023 ----

/// A rate-limited upstream's response carries the `X-RateLimit-*` headers.
#[tokio::test]
async fn a_rate_limited_response_carries_the_rate_limit_headers() {
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, common::upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = common::http_route(created.id, "/", &["GET"]);
    route.rate_limit = Some(rate_limit(3));
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "x-ratelimit-limit").as_deref(), Some("3"));
    assert_eq!(
        header(&response, "x-ratelimit-remaining").as_deref(),
        Some("2")
    );
    assert!(header(&response, "x-ratelimit-reset").is_some());

    let mut second = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(
        header(&second, "x-ratelimit-remaining").as_deref(),
        Some("1")
    );
    let _ = text(&mut second).await;
}

// ---------------------------------------------------------------- F-011 ----

/// Two upstreams with different limits never share a bucket.
#[tokio::test]
async fn two_upstreams_do_not_share_a_rate_limit_bucket() {
    let first = echo_upstream().await;
    let second = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let tight = create_upstream(&harness, &ctx, "tight", "127.0.0.1", first.port()).await;
    let loose = create_upstream(&harness, &ctx, "loose", "127.0.0.1", second.port()).await;
    let mut tight_route = common::http_route(tight.id, "/", &["GET"]);
    tight_route.rate_limit = Some(rate_limit(1));
    harness
        .control_plane()
        .create_route(&ctx, tight_route)
        .await
        .expect("route");
    let mut loose_route = common::http_route(loose.id, "/", &["GET"]);
    loose_route.rate_limit = Some(rate_limit(5));
    harness
        .control_plane()
        .create_route(&ctx, loose_route)
        .await
        .expect("route");

    let first_response = harness
        .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/tight/v1/a"))
        .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let exhausted = harness
        .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/tight/v1/a"))
        .await;
    assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);

    // The sibling upstream has its own bucket: it still serves requests.
    let sibling = harness
        .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/loose/v1/a"))
        .await;
    assert_eq!(sibling.status(), StatusCode::OK);
}

// ---------------------------------------------------------------- F-012 ----

/// A client-supplied `X-Forwarded-For` cannot mint a fresh bucket when no
/// trusted proxy is configured.
#[tokio::test]
async fn a_client_forwarded_for_header_does_not_split_the_ip_bucket() {
    let upstream = header_echo_upstream("x-trace").await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, common::upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = common::http_route(created.id, "/", &["GET"]);
    route.rate_limit = Some(ip_rate_limit(1));
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let first = harness
        .send(&ctx, gateway_request("GET", "/oagw/v1/proxy/local/v1/a"))
        .await;
    assert_eq!(first.status(), StatusCode::OK);

    // A different claimed address, and another header name: neither may open a
    // second bucket while no trusted proxy is configured.
    for (uri, name, value) in [
        (
            "/oagw/v1/proxy/local/v1/b",
            "x-forwarded-for",
            "203.0.113.7",
        ),
        (
            "/oagw/v1/proxy/local/v1/c",
            "x-forwarded-for",
            "203.0.113.8",
        ),
        ("/oagw/v1/proxy/local/v1/d", "forwarded", "for=203.0.113.9"),
    ] {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .header(name, value)
            .body(Body::empty())
            .expect("request");
        let response = harness.send(&ctx, request).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
    }
}

/// With a trusted proxy configured the forwarded address is the client
/// address, so two different addresses get two different buckets.
#[tokio::test]
async fn a_trusted_proxy_lets_the_forwarded_address_be_believed() {
    let upstream = header_echo_upstream("x-trace").await;
    let harness = TrustedHarness::new(options(), vec!["10.0.0.0/8".to_owned()]);
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .control_plane()
        .create_upstream(&ctx, common::upstream_shell(upstream.port()))
        .await
        .expect("upstream");
    let mut route = common::http_route(created.id, "/", &["GET"]);
    route.rate_limit = Some(ip_rate_limit(1));
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    for (claimed, expected) in [
        ("203.0.113.7, 10.0.0.9", StatusCode::OK),
        ("203.0.113.7, 10.0.0.9", StatusCode::TOO_MANY_REQUESTS),
        ("203.0.113.8, 10.0.0.9", StatusCode::OK),
    ] {
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/proxy/local/v1/a")
            .header("x-forwarded-for", claimed)
            .body(Body::empty())
            .expect("request");
        let response = harness.send(&ctx, request).await;
        assert_eq!(response.status(), expected, "{claimed}");
    }
}

/// A local harness that builds the router itself, so the Data Plane can be
/// configured beyond what `ProxyOptions` carries.
struct TrustedHarness {
    sub: axum::Router,
    control_plane: Arc<oagw::infra::controlplane::ControlPlaneServiceImpl>,
}

impl TrustedHarness {
    fn new(options: ProxyOptions, trusted_proxies: Vec<String>) -> Self {
        let store = oagw::infra::storage::InMemoryStore::new();
        let control_plane = Arc::new(
            oagw::infra::controlplane::ControlPlaneServiceImpl::new(store, None)
                .allowing_http_upstream(true),
        );
        let data_plane = Arc::new(
            oagw::infra::proxy::service::DataPlaneServiceImpl::new(
                control_plane.clone(),
                BuiltinPlugins::with_builtins_optional(None),
                OagwMetrics::new(),
                options,
            )
            .with_trusted_proxies(trusted_proxies),
        );
        let registry = NoopOpenApiRegistry;
        let sub = oagw::api::rest::routes::build_router(
            Arc::clone(&control_plane),
            data_plane,
            &registry,
        );
        Self { sub, control_plane }
    }

    /// Sends a request through the sub-router as `ctx`.
    async fn send(
        &self,
        ctx: &toolkit_security::SecurityContext,
        request: Request<Body>,
    ) -> axum::response::Response {
        let app = self.sub.clone().layer(axum::Extension(ctx.clone()));
        tower::ServiceExt::oneshot(app, request)
            .await
            .expect("infallible service")
    }
}

/// A handle over the Control Plane, for direct configuration of a test.
impl TrustedHarness {
    fn control_plane(&self) -> &Arc<oagw::infra::controlplane::ControlPlaneServiceImpl> {
        &self.control_plane
    }
}

/// Noop OpenAPI registry: the tests exercise proxying, not the OpenAPI document.
struct NoopOpenApiRegistry;

impl toolkit::api::OpenApiRegistry for NoopOpenApiRegistry {
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

// --------------------------------------------- F-022, F-027, F-013, F-028 ----

/// The correlation identifier of a request is stable across the audit line and
/// the `X-Request-ID` response header.
#[tokio::test]
async fn the_request_id_is_stable_across_the_audit_line_and_the_response() {
    // Installs the audit capture before the request is sent.
    audit_capture();
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("route");
    bind_request_id_transform(&harness, &ctx, &created).await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/local/v1/items")
        .header("x-request-id", "audit-corr-42")
        .body(Body::empty())
        .expect("request");
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-request-id").as_deref(),
        Some("audit-corr-42")
    );

    let lines = audit_capture().audit_lines();
    assert!(
        lines
            .iter()
            .any(
                |line| line.get("request_id").map(String::as_str) == Some("audit-corr-42")
                    && line.get("path").map(String::as_str) == Some("/local/v1/items")
                    && line.get("route").is_some()
            ),
        "no audit line carries the request identifier: {lines:?}"
    );
}

/// A generated identifier is echoed too, so a caller can always correlate.
#[tokio::test]
async fn a_generated_request_id_reaches_the_response_and_the_audit_line() {
    // Installs the audit capture before the request is sent.
    audit_capture();
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let created = harness
        .local_upstream(&ctx, upstream.port(), "/", &["GET"])
        .await
        .expect("route");
    bind_request_id_transform(&harness, &ctx, &created).await;

    let response = harness
        .send(
            &ctx,
            gateway_request("GET", "/oagw/v1/proxy/local/v1/items"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let echoed = header(&response, "x-request-id").expect("a generated identifier");
    assert!(!echoed.is_empty());
    let lines = audit_capture().audit_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.get("request_id").map(String::as_str) == Some(echoed.as_str())),
        "the generated identifier must reach the audit line: {lines:?}"
    );
}

/// A body that grows past the payload limit while it streams is a 413, even
/// though it never declared a `Content-Length`.
#[tokio::test]
async fn a_chunked_body_past_the_payload_budget_is_rejected() {
    let chunk = "x".repeat(10 * 1024 * 1024);
    let stream = futures_util::stream::iter(
        (0..11).map(move |_| Ok::<_, std::convert::Infallible>(bytes::Bytes::from(chunk.clone()))),
    );
    let upstream = echo_upstream().await;
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    harness
        .local_upstream(&ctx, upstream.port(), "/", &["POST"])
        .await
        .expect("route");

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/local/v1/upload")
        .header("transfer-encoding", "chunked")
        .body(Body::from_stream(stream))
        .expect("request");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.payload.too_large.v1"), "{body}");
}

/// An IPv6 endpoint really is an IPv6 socket, so the bracketed authority is
/// not silently rewritten to an IPv4 one.
#[tokio::test]
async fn the_v6_upstream_address_is_an_ipv6_socket() {
    let upstream = echo_upstream_v6().await;
    assert!(upstream.address.is_ipv6());

    let mut socket = tokio::net::TcpStream::connect(upstream.address)
        .await
        .expect("an IPv6 loopback connection");
    let (mut reader, mut writer) = socket.split();
    use tokio::io::AsyncWriteExt;
    writer
        .write_all(b"GET /probe HTTP/1.1\r\nhost: probe\r\nconnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut answer = String::new();
    use tokio::io::AsyncReadExt;
    reader.read_to_string(&mut answer).await.expect("read");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("GET /probe"), "{answer}");
}
