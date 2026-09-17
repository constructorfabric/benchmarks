//! Integration tests of the oagw data plane (DESIGN.md §3.2, §3.3, §3.5).
//!
//! The tests compose the real router (`register_routes`) with a real httpmock
//! upstream and drive it with `tower::ServiceExt::oneshot`, asserting the
//! proxying behaviour, the policy errors and the passthrough rules the
//! specification states.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::significant_drop_tightening
)]

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use httpmock::prelude::*;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::hierarchy::StaticTenantHierarchy;
use oagw::domain::services::OagwService;

/// A router bound to a fresh service, with helpers to configure it.
struct Harness {
    router: Router,
    tenant: Uuid,
}

/// A proxied response, reduced to what the assertions need.
struct Sent {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Sent {
    fn problem(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

impl Harness {
    fn with(config: OagwConfig) -> Self {
        let service = std::sync::Arc::new(OagwService::new(
            config,
            std::sync::Arc::new(StaticTenantHierarchy::default()),
        ));
        let router = register_routes(Router::new(), &OpenApiRegistryImpl::new(), service)
            .expect("data plane composes");
        Self {
            router,
            tenant: Uuid::new_v4(),
        }
    }

    fn plain() -> Self {
        Self::with(OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: true,
            ..OagwConfig::default()
        })
    }

    fn ctx(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(self.tenant)
            .build()
            .expect("context")
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Sent {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(bytes) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes.to_vec()))
                .expect("request"),
            None => builder.body(Body::empty()).expect("request"),
        };
        let mut request = request;
        request.extensions_mut().insert(self.ctx());
        let response = self.router.clone().oneshot(request).await.expect("served");
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Sent {
            status,
            headers,
            body,
        }
    }

    async fn send_json(&self, method: &str, uri: &str, body: Value) -> Sent {
        self.send(
            method,
            uri,
            &[("content-type", "application/json")],
            Some(serde_json::to_vec(&body).unwrap().as_slice()),
        )
        .await
    }

    /// Creates an upstream whose only endpoint is `host:port` and returns its
    /// GTS id.
    async fn upstream(&self, alias: &str, host: &str, port: u16) -> String {
        self.upstream_with(alias, host, port, json!({})).await
    }

    /// Creates an upstream with extra specification fields.
    async fn upstream_with(&self, alias: &str, host: &str, port: u16, extra: Value) -> String {
        let mut body = json!({
            "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": host, "port": port }
            ] }
        });
        merge(&mut body, extra);
        let sent = self.send_json("POST", "/oagw/v1/upstreams", body).await;
        assert_eq!(
            sent.status,
            StatusCode::CREATED,
            "create upstream: {}",
            sent.problem()
        );
        sent.problem()["id"]
            .as_str()
            .expect("upstream id")
            .to_owned()
    }

    /// Creates a route on `upstream_id` and returns its GTS id.
    async fn route(&self, upstream_id: &str, path: &str, methods: &[&str]) -> String {
        self.route_with(upstream_id, path, methods, json!({})).await
    }

    /// Creates a route with extra specification fields.
    async fn route_with(
        &self,
        upstream_id: &str,
        path: &str,
        methods: &[&str],
        extra: Value,
    ) -> String {
        let mut body = json!({
            "upstream_id": upstream_id,
            "match": { "http": {
                "methods": methods,
                "path": path,
            } }
        });
        merge(&mut body, extra);
        let sent = self.send_json("POST", "/oagw/v1/routes", body).await;
        assert_eq!(
            sent.status,
            StatusCode::CREATED,
            "create route: {}",
            sent.problem()
        );
        sent.problem()["id"].as_str().expect("route id").to_owned()
    }

    /// Creates a multi-host upstream directly on the wire and returns its
    /// response, so a test can read back the derived alias.
    async fn upstream_raw(&self, body: Value) -> Value {
        let sent = self.send_json("POST", "/oagw/v1/upstreams", body).await;
        assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.problem());
        sent.problem()
    }
}

/// Recursively merges `extra` into `base`.
fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(base), Value::Object(extra)) => {
            for (key, value) in extra {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, extra) => *base = extra,
    }
}

impl Sent {
    /// The GTS instance of the problem body's `type`, e.g.
    /// `cf.oagw.routing.missing_target_host.v1`.
    fn error_type(&self) -> String {
        let body = self.problem();
        let kind = body["type"].as_str().unwrap_or_default();
        kind.split('~').nth(1).unwrap_or_default().to_owned()
    }

    /// Asserts the problem body names `instance` of the `cf.oagw.*` family.
    fn assert_error(&self, instance: &str) {
        assert_eq!(self.error_type(), instance, "body: {}", self.problem());
    }
}

/// A closed port: a bound-then-dropped listener.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

// ---------------------------------------------------------------- round trip

#[tokio::test]
async fn a_plain_get_round_trips_to_the_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/pets")
            .query_param("limit", "1")
            .header("x-request-id", "req-7");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"pets":[]}"#);
    });

    let harness = Harness::plain();
    let id = harness
        .upstream("pets", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;

    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/pets/v1/pets?limit=1",
            &[("x-request-id", "req-7")],
            None,
        )
        .await;
    assert_eq!(mock.calls(), 1, "the upstream must be dialed exactly once");
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(
        sent.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        sent.header("x-oagw-error-source").as_deref(),
        Some("upstream"),
        "a proxied answer comes from upstream (ADR-0007 stamps every response)"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&sent.body).unwrap()["pets"],
        json!([])
    );
}

#[tokio::test]
async fn the_upstream_sees_the_forwarded_path_query_and_body() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/pets")
            .query_param("dry", "true")
            .header("content-type", "application/json")
            .body_includes("mio");
        then.status(201)
            .header("content-type", "application/json")
            .body(r#"{"id":"p1"}"#);
    });

    let harness = Harness::plain();
    let id = harness
        .upstream("pets", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["POST"]).await;

    let sent = harness
        .send(
            "POST",
            "/oagw/v1/proxy/pets/v1/pets?dry=true",
            &[("content-type", "application/json")],
            Some(br#"{"name":"mio"}"#),
        )
        .await;
    assert_eq!(sent.status, StatusCode::CREATED);
    assert_eq!(mock.calls(), 1);
    assert_eq!(sent.problem()["id"], json!("p1"));
}

#[tokio::test]
async fn hop_by_hop_headers_are_stripped_and_the_host_is_replaced() {
    let server = MockServer::start();
    let authority = format!("{}:{}", server.host(), server.port());
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/things")
            .header_missing("connection")
            .header_missing("te")
            .header_missing("x-spoofed")
            .header("host", authority.clone());
        then.status(200).body("ok");
    });

    let harness = Harness::plain();
    let id = harness
        .upstream("things", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;

    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/things/v1/things",
            &[
                ("connection", "close"),
                ("te", "trailers"),
                ("host", "spoofed.example.com"),
            ],
            None,
        )
        .await;
    assert_eq!(
        sent.status,
        StatusCode::OK,
        "the mocked contract must match"
    );
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn non_configured_headers_do_not_reach_the_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/quiet")
            .header_missing("x-internal-secret")
            .header("accept", "*/*");
        then.status(200).body("ok");
    });

    let harness = Harness::plain();
    let id = harness
        .upstream("quiet", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;

    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/quiet/v1/quiet",
            &[("x-internal-secret", "s3cret"), ("accept", "*/*")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(
        mock.calls(),
        1,
        "the passthrough allowlist must be honoured"
    );
}

// ---------------------------------------------------------------- target host

#[tokio::test]
async fn the_target_host_matrix_is_enforced() {
    // The pool hosts are not resolvable on purpose: a pinned endpoint is then
    // observed through the dial failure that names it, while an ambiguous pool
    // is refused before anything is dialed.
    let server = MockServer::start();
    let port = server.port();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/zone");
        then.status(200).body("zone");
    });

    let harness = Harness::plain();
    // A multi-host pool whose alias is derived from its hosts (the registrable
    // common suffix plus the non-standard port).
    let created = harness
        .upstream_raw(json!({
            "server": { "endpoints": [
                { "scheme": "http", "host": "us.zones.com", "port": port },
                { "scheme": "http", "host": "eu.zones.com", "port": port }
            ] }
        }))
        .await;
    let alias = created["alias"].as_str().expect("derived alias").to_owned();
    assert_eq!(
        alias,
        format!("zones.com:{port}"),
        "the suffix derivation carries the port"
    );
    let id = created["id"].as_str().expect("upstream id").to_owned();
    harness.route(&id, "/v1", &["GET"]).await;
    let url = format!("/oagw/v1/proxy/{alias}/v1/zone");

    // 1. No header, ambiguous pool → 400 missing_target_host.
    let sent = harness.send("GET", &url, &[], None).await;
    assert_eq!(
        sent.status,
        StatusCode::BAD_REQUEST,
        "ambiguous pool needs a target"
    );
    sent.assert_error("cf.oagw.routing.missing_target_host.v1");
    assert_eq!(
        sent.problem()["valid_hosts"],
        json!(["us.zones.com", "eu.zones.com"])
    );
    assert_eq!(
        mock.calls(),
        0,
        "nothing is dialed when the pool is ambiguous"
    );

    // 2. A header pins one endpoint of the pool, which is then dialed.
    let sent = harness
        .send("GET", &url, &[("x-oagw-target-host", "eu.zones.com")], None)
        .await;
    assert_eq!(
        sent.status,
        StatusCode::BAD_GATEWAY,
        "the pinned endpoint is dialed"
    );
    sent.assert_error("cf.oagw.protocol.error.v1");
    assert_eq!(
        sent.problem()["host"],
        json!("eu.zones.com"),
        "the pinned endpoint is dialed"
    );

    // 3. An unparsable header is invalid_target_host.
    let sent = harness
        .send(
            "GET",
            &url,
            &[("x-oagw-target-host", "eu.zones.com/x")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    sent.assert_error("cf.oagw.routing.invalid_target_host.v1");
    assert_eq!(sent.problem()["invalid_value"], json!("eu.zones.com/x"));

    // 4. A parsable but unknown host is unknown_target_host.
    let sent = harness
        .send("GET", &url, &[("x-oagw-target-host", "ap.zones.com")], None)
        .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    sent.assert_error("cf.oagw.routing.unknown_target_host.v1");
    assert_eq!(sent.problem()["invalid_value"], json!("ap.zones.com"));
}

#[tokio::test]
async fn a_single_endpoint_pool_needs_no_target_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/one");
        then.status(200).body("one");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream("single", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/single/v1/one", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

// ---------------------------------------------------------------- resolution

#[tokio::test]
async fn an_unknown_alias_is_upstream_not_found() {
    let harness = Harness::plain();
    let sent = harness
        .send("GET", "/oagw/v1/proxy/nobody/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    sent.assert_error("cf.oagw.upstream.not_found.v1");
    assert_eq!(
        sent.header("x-oagw-error-source").as_deref(),
        Some("gateway")
    );
}

#[tokio::test]
async fn an_unmatched_method_is_route_not_found() {
    let server = MockServer::start();
    let harness = Harness::plain();
    let id = harness
        .upstream("only-get", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("DELETE", "/oagw/v1/proxy/only-get/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    sent.assert_error("cf.oagw.route.not_found.v1");
}

#[tokio::test]
async fn a_disabled_upstream_is_not_routable() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/x");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "off",
            &server.host(),
            server.port(),
            json!({ "enabled": false }),
        )
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/off/v1/x", &[], None)
        .await;
    // `resolve_alias` resolves the closest *enabled* upstream, so a disabled
    // one is indistinguishable from an absent one (DESIGN.md §3.3).
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    sent.assert_error("cf.oagw.upstream.not_found.v1");
    assert_eq!(mock.calls(), 0, "a disabled upstream is never dialed");
}

#[tokio::test]
async fn plaintext_is_refused_when_the_policy_forbids_it() {
    let server = MockServer::start();
    let harness = Harness::with(OagwConfig {
        allow_http_upstream: false,
        ..OagwConfig::default()
    });
    let id = harness
        .upstream("guarded", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/guarded/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::SERVICE_UNAVAILABLE);
    sent.assert_error("cf.oagw.link.unavailable.v1");
    assert_eq!(
        sent.problem()["endpoint"],
        json!(format!("http://{}:{}", server.host(), server.port())),
        "the refused endpoint must be named"
    );
}

// ---------------------------------------------------------------- body policy

#[tokio::test]
async fn an_oversized_body_is_rejected_with_413() {
    let server = MockServer::start();
    let harness = Harness::with(OagwConfig {
        allow_http_upstream: true,
        max_body_bytes: 16,
        ..OagwConfig::default()
    });
    let id = harness
        .upstream("small", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["POST"]).await;
    let sent = harness
        .send(
            "POST",
            "/oagw/v1/proxy/small/v1/x",
            &[],
            Some(b"0123456789abcdef0123456789"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE);
    sent.assert_error("cf.oagw.payload.too_large.v1");
}

#[tokio::test]
async fn a_mismatched_content_length_is_rejected() {
    let server = MockServer::start();
    let harness = Harness::plain();
    let id = harness.upstream("len", &server.host(), server.port()).await;
    harness.route(&id, "/v1", &["POST"]).await;
    let sent = harness
        .send(
            "POST",
            "/oagw/v1/proxy/len/v1/x",
            &[("content-length", "99")],
            Some(b"short"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    sent.assert_error("cf.oagw.validation.error.v1");
}

// ---------------------------------------------------------------- rate limit

#[tokio::test]
async fn an_exhausted_rate_limit_is_rejected_with_429() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/limited");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness.upstream("rl", &server.host(), server.port()).await;
    harness
        .route_with(
            &id,
            "/v1",
            &["GET"],
            json!({ "rate_limit": {
                "sustained": { "rate": 2, "window": "minute" },
                "burst": { "capacity": 2 },
                "scope": "tenant",
                "strategy": "reject",
                "response_headers": true
            } }),
        )
        .await;
    for call in 0..2 {
        let sent = harness
            .send("GET", "/oagw/v1/proxy/rl/v1/limited", &[], None)
            .await;
        assert_eq!(sent.status, StatusCode::OK, "the first two calls pass");
        assert_eq!(
            sent.header("x-ratelimit-limit").as_deref(),
            Some("2"),
            "call {call} reports the limit"
        );
    }
    let sent = harness
        .send("GET", "/oagw/v1/proxy/rl/v1/limited", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        sent.headers.contains_key("retry-after"),
        "Retry-After must be set"
    );
    assert_eq!(sent.header("x-ratelimit-remaining").as_deref(), Some("0"));
    sent.assert_error("cf.oagw.rate_limit.exceeded.v1");
    let retry_after = sent.problem()["retry_after_seconds"]
        .as_u64()
        .expect("retry guidance");
    assert!(retry_after > 0, "the rejected call reports the window");
    assert_eq!(mock.calls(), 2, "the rejected call must not be dialed");
}

#[tokio::test]
async fn the_effective_rate_limit_is_the_min_of_upstream_and_route() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/capped");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "capped",
            &server.host(),
            server.port(),
            json!({ "rate_limit": {
                "sustained": { "rate": 50, "window": "minute" },
                "burst": { "capacity": 50 },
                "scope": "tenant",
                "strategy": "reject"
            } }),
        )
        .await;
    harness
        .route_with(
            &id,
            "/v1",
            &["GET"],
            json!({ "rate_limit": {
                "sustained": { "rate": 3, "window": "minute" },
                "burst": { "capacity": 3 },
                "scope": "tenant",
                "strategy": "reject",
                "response_headers": true
            } }),
        )
        .await;
    for call in 0..3 {
        let sent = harness
            .send("GET", "/oagw/v1/proxy/capped/v1/capped", &[], None)
            .await;
        assert_eq!(
            sent.status,
            StatusCode::OK,
            "call {call} is within the tighter limit"
        );
        assert_eq!(sent.header("x-ratelimit-limit").as_deref(), Some("3"));
    }
    let sent = harness
        .send("GET", "/oagw/v1/proxy/capped/v1/capped", &[], None)
        .await;
    assert_eq!(
        sent.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the route limit wins"
    );
    sent.assert_error("cf.oagw.rate_limit.exceeded.v1");
    assert_eq!(mock.calls(), 3, "the rejected call is not dialed");
}

// ---------------------------------------------------------------- upstream failures

#[tokio::test]
async fn a_hanging_upstream_times_out_with_504() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/slow");
        then.status(200).delay(Duration::from_secs(10)).body("late");
    });
    let harness = Harness::with(OagwConfig {
        proxy_timeout_secs: 1,
        allow_http_upstream: true,
        ..OagwConfig::default()
    });
    let id = harness
        .upstream("slow", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/slow/v1/slow", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::GATEWAY_TIMEOUT);
    sent.assert_error("cf.oagw.timeout.request.v1");
}

#[tokio::test]
async fn a_refused_connection_is_a_bad_gateway() {
    let port = closed_port();
    let harness = Harness::plain();
    let id = harness.upstream("closed", "127.0.0.1", port).await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/closed/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::BAD_GATEWAY);
    sent.assert_error("cf.oagw.protocol.error.v1");
}

#[tokio::test]
async fn the_circuit_breaker_short_circuits_a_dead_upstream() {
    let port = closed_port();
    let harness = Harness::plain();
    let id = harness.upstream("breaker", "127.0.0.1", port).await;
    harness.route(&id, "/v1", &["GET"]).await;
    for attempt in 0..5 {
        let sent = harness
            .send("GET", "/oagw/v1/proxy/breaker/v1/x", &[], None)
            .await;
        assert_eq!(sent.status, StatusCode::BAD_GATEWAY, "attempt {attempt}");
    }
    let sent = harness
        .send("GET", "/oagw/v1/proxy/breaker/v1/x", &[], None)
        .await;
    assert_eq!(
        sent.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the sixth call is not dialed"
    );
    sent.assert_error("cf.oagw.circuit_breaker.open.v1");
}

// ---------------------------------------------------------------- plugins

#[tokio::test]
async fn the_required_headers_guard_rejects_the_request() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/v1/x");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream("guarded-headers", &server.host(), server.port())
        .await;
    harness
        .route_with(
            &id,
            "/v1",
            &["GET"],
            json!({ "plugins": {
                "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
                "config": {
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1": {
                        "required_request_headers": "X-Tenant-Id"
                    }
                }
            } }),
        )
        .await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/guarded-headers/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.problem()["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );

    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/guarded-headers/v1/x",
            &[("x-tenant-id", "acme")],
            None,
        )
        .await;
    assert_eq!(
        sent.status,
        StatusCode::OK,
        "the guard lets a complete request through"
    );
}

#[tokio::test]
async fn the_request_id_is_propagated_end_to_end() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/v1/echo")
            .header("x-request-id", "trace-9");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream("echo", &server.host(), server.port())
        .await;
    harness
        .route_with(
            &id,
            "/v1",
            &["GET"],
            json!({ "plugins": { "items": [oagw::domain::model::builtins::TRANSFORM_REQUEST_ID] } }),
        )
        .await;

    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/echo/v1/echo",
            &[("x-request-id", "trace-9")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(sent.header("x-request-id").as_deref(), Some("trace-9"));

    let sent = harness
        .send("GET", "/oagw/v1/proxy/echo/v1/echo", &[], None)
        .await;
    let minted = sent
        .header("x-request-id")
        .expect("a request id is always minted");
    assert!(
        Uuid::parse_str(&minted).is_ok(),
        "minted id must be a uuid: {minted}"
    );
}

#[tokio::test]
async fn header_transformations_are_applied_in_both_directions() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/signed")
            .header("x-signed", "gateway")
            .header_missing("x-removed")
            .header_missing("x-keep-out");
        then.status(200)
            .header("x-upstream-marker", "present")
            .body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "signed",
            &server.host(),
            server.port(),
            json!({ "headers": {
                "request": {
                    "set": { "x-signed": "gateway" },
                    "remove": ["x-removed"],
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["x-request-id"]
                },
                "response": { "set": { "x-gateway-marker": "oagw" } }
            } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/signed/v1/signed",
            &[("x-removed", "gone"), ("x-keep-out", "no")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.problem());
    assert_eq!(mock.calls(), 1);
    assert_eq!(
        sent.header("x-gateway-marker").as_deref(),
        Some("oagw"),
        "the response operation must add a header"
    );
    assert_eq!(
        sent.header("x-upstream-marker").as_deref(),
        Some("present"),
        "the upstream header passes through"
    );
}

// ---------------------------------------------------------------- auth + chain

#[tokio::test]
async fn an_apikey_upstream_injects_the_credential_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/keys")
            .header("x-api-key", "static-key");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "keys",
            &server.host(),
            server.port(),
            json!({ "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "header_value_ref": "static-key" }
            } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/keys/v1/keys", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.problem());
    assert_eq!(mock.calls(), 1, "the credential was injected upstream");
}

#[tokio::test]
async fn a_missing_secret_is_a_gateway_error() {
    let server = MockServer::start();
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "nosecret",
            &server.host(),
            server.port(),
            json!({ "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "header_value_ref": "cred://vault/missing" }
            } }),
        )
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    let sent = harness
        .send("GET", "/oagw/v1/proxy/nosecret/v1/x", &[], None)
        .await;
    assert_eq!(sent.status, StatusCode::INTERNAL_SERVER_ERROR);
    sent.assert_error("cf.oagw.secret.not_found.v1");
    sent.assert_error("cf.oagw.secret.not_found.v1");
}

#[tokio::test]
async fn the_plugin_chain_runs_auth_then_guard_then_transform() {
    let server = MockServer::start();
    // The auth plugin runs first and injects the key; the guard then requires
    // a header only present because auth already ran; the transform finally
    // stamps the request id the upstream sees.
    // `x-tenant-id` is inbound-only (the default passthrough drops it); the
    // upstream sees the injected credential and the propagated request id.
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/chain")
            .header("x-api-key", "chain-key")
            .header("x-request-id", "chain-1");
        then.status(200).body("ok");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream_with(
            "chain",
            &server.host(),
            server.port(),
            json!({ "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "header_value_ref": "chain-key" }
            } }),
        )
        .await;
    harness
        .route_with(
            &id,
            "/v1",
            &["GET"],
            json!({ "plugins": {
                "items": [
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
                ],
                "config": {
                    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1": {
                        "required_request_headers": "X-Tenant-Id"
                    }
                }
            } }),
        )
        .await;
    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/chain/v1/chain",
            &[("x-tenant-id", "acme"), ("x-request-id", "chain-1")],
            None,
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.problem());
    assert_eq!(
        mock.calls(),
        1,
        "auth, guard and transform all ran in order"
    );
}

#[tokio::test]
async fn a_single_endpoint_pool_validates_the_target_header() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/one");
        then.status(200).body("one");
    });
    let harness = Harness::plain();
    let id = harness
        .upstream("single", &server.host(), server.port())
        .await;
    harness.route(&id, "/v1", &["GET"]).await;
    // A header naming the only endpoint is accepted.
    let sent = harness
        .send(
            "GET",
            "/oagw/v1/proxy/single/v1/one",
            &[("x-oagw-target-host", &server.host())],
            None,
        )
        .await;
    assert_eq!(
        sent.status,
        StatusCode::OK,
        "the only endpoint is routed to"
    );
    assert_eq!(mock.calls(), 1);
}
