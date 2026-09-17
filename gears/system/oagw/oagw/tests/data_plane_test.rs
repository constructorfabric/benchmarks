//! Data-plane tests over a real socket.
//!
//! `management_api_test.rs` exercises the control plane through
//! `Router::oneshot`, which cannot carry a protocol upgrade, cannot observe a
//! streamed response mid-flight and cannot show what the gateway actually put
//! on the wire. These tests therefore serve the gear's router with
//! `axum::serve` on `127.0.0.1:0`, dial it with a raw `TcpStream` and point it
//! at hand-written fake upstreams, so every assertion below is an observation
//! of the gateway's real behaviour:
//!
//! * SSE relayed incrementally (never buffered);
//! * WebSocket upgrades tunneled in both directions;
//! * CORS on actual requests, plus the create-time wildcard rule;
//! * rate limiting on both the 429 and the 200 path;
//! * `cred://`-resolved credential injection;
//! * the ADR 0009 required-headers guard;
//! * the `X-OAGW-Target-Host` behaviour matrix (ADR 0001);
//! * request fidelity (verbatim body, forwarded query, intact response);
//! * the gRPC refusal and the plaintext-transport gate.
//!
//! Management writes go through the same `Router` (shared state, so the L1
//! cache the data plane reads through is invalidated in the same tick).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod support;

use axum::http::StatusCode;
use credstore_sdk::test_util::MockCredStoreClient;
use serde_json::{Value, json};

use support::{
    PROTOCOL_GRPC, PROTOCOL_HTTP, READ_BUDGET, Behaviour, FakeResponse, FakeUpstream,
    FakeWsUpstream, Fixture, RawResponse, config, create_upstream, http_endpoint, raw_get,
    raw_request, raw_send, read_some, read_until, read_until_seeded, route_body, send_raw,
    state_with, upstream_body, wire_up,
};

const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
const MISSING_TARGET_HOST: &str = "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
const INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
const UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
const RATE_LIMIT_EXCEEDED: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
const CORS_ORIGIN: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
const CORS_METHOD: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
const VALIDATION_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";

const AUTH_API_KEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
const PROTOCOL_ERROR_502: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";

/// A gateway request that asks for the SSE content type, so the streaming
/// branch is taken even if the upstream mislabels its response.
fn sse_get(alias: &str, suffix: &str) -> String {
    format!(
        "GET /oagw/v1/proxy/{alias}/{suffix} HTTP/1.1\r\n\
         host: gateway\r\n\
         accept: text/event-stream\r\n\r\n"
    )
}

/// A `GET` over a raw socket carrying one extra inbound header.
fn get_with_header(alias: &str, suffix: &str, name: &str, value: &str) -> String {
    format!(
        "GET /oagw/v1/proxy/{alias}/{suffix} HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         {name}: {value}\r\n\r\n"
    )
}

/// A request carrying an `Origin` header.
fn cors_get(alias: &str, suffix: &str, origin: &str, method: &str) -> String {
    format!(
        "{method} /oagw/v1/proxy/{alias}/{suffix} HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         origin: {origin}\r\n\r\n"
    )
}

// ── Streaming (SSE) ─────────────────────────────────────────────────────

/// A `text/event-stream` response must reach the caller as it is produced:
/// chunk 1 arrives while the upstream is still holding the connection open,
/// which is only possible if the gateway is not buffering the body.
#[tokio::test(flavor = "multi_thread")]
async fn sse_response_is_relayed_incrementally() {
    let upstream = FakeUpstream::start(Behaviour::Gated {
        head: vec![
            ("content-type".to_owned(), "text/event-stream".to_owned()),
            ("cache-control".to_owned(), "no-cache".to_owned()),
        ],
        first: "data: one\n\n".to_owned(),
        second: "data: two\n\n".to_owned(),
    })
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "sse.test", &upstream.addr(), "/v1").await;

    let RawResponse {
        status,
        headers,
        prefix,
        stream: mut client,
    } = raw_request(fixture.gateway(), &sse_get("sse.test", "v1/events")).await;
    assert_eq!(status, 200, "the SSE head is relayed");
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.as_str()),
        Some("text/event-stream"),
        "the streaming content type is passed through"
    );
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "x-oagw-error-source")
            .map(|(_, value)| value.as_str()),
        Some("upstream"),
        "a relayed upstream response is not a gateway failure"
    );

    // The first event must already be readable while the upstream is still
    // holding the connection open: bytes that the upstream has not sent yet
    // cannot be in the buffer, so this is a direct observation that the gateway
    // relayed the stream instead of buffering it.
    let (seen, complete) = read_until_seeded(prefix, &mut client, "data: one", READ_BUDGET).await;
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(complete, "the first SSE event arrived: {seen}");
    assert!(
        !seen.contains("data: two"),
        "the gateway must not buffer the stream: {seen}"
    );

    // Only now does the upstream emit the second event.
    upstream.release();
    let (rest, complete) = read_until(&mut client, "data: two", READ_BUDGET).await;
    assert!(complete, "the second SSE event arrived after release: {}", String::from_utf8_lossy(&rest));
    assert_eq!(upstream.count(), 1, "one upstream exchange");

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── WebSocket upgrade tunneling ─────────────────────────────────────────

/// An inbound `Upgrade: websocket` request must be answered with the upstream's
/// `101` and then spliced, so bytes flow in both directions afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_upgrade_is_tunneled_in_both_directions() {
    let upstream = FakeWsUpstream::start().await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "ws.test", &upstream.addr(), "/v1").await;

    let request: &str = "GET /oagw/v1/proxy/ws.test/v1/ws HTTP/1.1\r\n\
         host: gateway\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         sec-websocket-version: 13\r\n\r\n";
    let RawResponse {
        status,
        headers,
        prefix,
        stream: mut client,
    } = raw_request(fixture.gateway(), request).await;
    assert_eq!(status, 101, "the upgrade is answered with the upstream's 101");
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "upgrade")
            .map(|(_, value)| value.as_str()),
        Some("websocket"),
        "headers: {headers:?}"
    );
    assert!(upstream.saw_upgrade(), "the handshake reached the upstream");

    // Upstream → client: the greeting the fake upstream pushed right after the
    // handshake, without any client byte preceding it.
    let greeting = read_some(&mut client, READ_BUDGET, 4096).await;
    let seen = [prefix.as_slice(), greeting.as_slice()].concat();
    assert!(
        seen.windows("hello-from-upstream".len())
            .any(|window| window == "hello-from-upstream".as_bytes()),
        "the upstream push must cross the tunnel: {}",
        String::from_utf8_lossy(&seen)
    );

    // Client → upstream → client.
    send_raw(&mut client, "ping-from-client").await;
    let (echo, complete) = read_until(&mut client, "ping-from-client", READ_BUDGET).await;
    assert!(complete, "the echo must come back: {}", String::from_utf8_lossy(&echo));

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A proxied request that never asks to upgrade is not answered with a `101`.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_request_is_not_answered_with_an_upgrade() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "plain.test", &upstream.addr(), "/v1").await;

    let response = raw_get(
        fixture.gateway(),
        "/oagw/v1/proxy/plain.test/v1/models",
        &[],
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(
        response.header("upgrade").is_none(),
        "no upgrade header on a plain request: {:?}",
        response.headers
    );
    assert_eq!(response.json()["ok"], Value::Bool(true));

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── CORS on actual requests ─────────────────────────────────────────────

/// Upstream with an origin-scoped CORS policy.
async fn cors_fixture(
    allowed_origins: &[&str],
    allowed_methods: &[&str],
    allow_credentials: bool,
) -> (Fixture, FakeUpstream) {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"served":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let mut body = upstream_body("cors.test", &upstream.addr());
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": allowed_origins,
        "allowed_methods": allowed_methods,
        "expose_headers": ["x-request-id"],
        "allow_credentials": allow_credentials,
    });
    let router = fixture.router();
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "upstream: {created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let mut route = route_body(&id, "/v1");
    route["match"]["http"]["methods"] = json!(["GET", "DELETE"]);
    let (status, route) = support::create_route(&router, route).await;
    assert_eq!(status, StatusCode::CREATED, "route: {route}");
    (fixture, upstream)
}

/// An origin the policy does not list is refused before the upstream is
/// consulted, with the ADR 0004 problem type.
#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_origin_is_refused_with_a_403_problem_document() {
    let (fixture, upstream) = cors_fixture(&["https://app.example"], &["GET"], false).await;
    let response = raw_send(
        fixture.gateway(),
        cors_get("cors.test", "v1/models", "https://evil.example", "GET"),
    )
    .await;
    response.assert_problem(403, CORS_ORIGIN);
    assert_eq!(upstream.count(), 0, "a CORS refusal never reaches the upstream");

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A method the policy does not list is refused with its own problem type.
#[tokio::test(flavor = "multi_thread")]
async fn a_disallowed_method_is_refused_with_a_403_problem_document() {
    let (fixture, upstream) = cors_fixture(&["https://app.example"], &["GET"], false).await;
    let response = raw_send(
        fixture.gateway(),
        cors_get("cors.test", "v1/models", "https://app.example", "DELETE"),
    )
    .await;
    response.assert_problem(403, CORS_METHOD);
    assert!(
        response.text().contains("DELETE"),
        "the refusal names the method: {}",
        response.text()
    );
    assert_eq!(upstream.count(), 0);

    upstream.shutdown().await;
    fixture.stop().await;
}

/// An allowed origin gets a proxied response decorated with the CORS headers,
/// including `Vary: Origin` so the origin-specific decision is not cached for
/// another origin.
#[tokio::test(flavor = "multi_thread")]
async fn an_allowed_origin_gets_cors_headers_on_the_proxied_response() {
    let (fixture, upstream) = cors_fixture(&["https://app.example"], &["GET"], true).await;
    let response = raw_send(
        fixture.gateway(),
        cors_get("cors.test", "v1/models", "https://app.example", "GET"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("access-control-allow-origin"), Some("https://app.example"));
    assert_eq!(response.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(response.header("access-control-expose-headers"), Some("x-request-id"));
    assert!(
        response.header_values("vary").contains(&"Origin"),
        "an origin-specific decision must vary: {:?}",
        response.headers
    );
    assert_eq!(
        response.header("x-oagw-error-source"),
        Some("upstream"),
        "a proxied response names the upstream as its source"
    );
    assert_eq!(response.json()["served"], true);

    upstream.shutdown().await;
    fixture.stop().await;
}

/// `allow_credentials` with a wildcard origin is refused by the model, not by
/// the data plane.
#[tokio::test]
async fn cors_rejects_credentials_with_a_wildcard_origin_at_create_time() {
    let fixture = Fixture::start().await;
    let mut body = upstream_body("wildcard.test", &"127.0.0.1:1".parse().expect("addr"));
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allow_credentials": true,
    });
    let (status, rejected) = create_upstream(&fixture.router(), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");
    assert_eq!(rejected["status"], 400);
    assert!(
        rejected.to_string().contains("credential"),
        "the refusal explains the rule: {rejected}"
    );
    fixture.stop().await;
}

// ── Rate limiting ───────────────────────────────────────────────────────

/// An upstream whose token bucket cannot refill inside the test window.
///
/// `sustained: { rate: 1, window: day }` gives a bucket of one token that
/// refills at one token per day, so the first request is allowed and every
/// later one within the test is refused — with no sleep and no boundary race.
fn rate_limited_body(alias: &str, addr: &std::net::SocketAddr) -> Value {
    let mut body = upstream_body(alias, addr);
    body["rate_limit"] = json!({
        "sustained": { "rate": 1, "window": "day" },
        "strategy": "reject",
    });
    body
}

/// The second request to an exhausted bucket is refused with a 429 problem
/// document carrying `Retry-After` *and* the `X-RateLimit-*` headers, and never
/// reaches the upstream.
///
/// ADR 0003 ("429 responses include `X-RateLimit-*` and `Retry-After` headers")
/// asks for the same limiter headers a proxied response carries: the limit is
/// the effective limit, the remaining count and the reset horizon are the
/// limiter's own numbers.
#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limited_upstream_answers_429_with_retry_after() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let router = fixture.router();
    let (status, created) = create_upstream(&router, rate_limited_body("throttled.test", &upstream.addr())).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let first = raw_get(fixture.gateway(), "/oagw/v1/proxy/throttled.test/v1/models", &[]).await;
    assert_eq!(first.status, 200, "{}", first.text());

    let second = raw_get(fixture.gateway(), "/oagw/v1/proxy/throttled.test/v1/models", &[]).await;
    second.assert_problem(429, RATE_LIMIT_EXCEEDED);
    let retry_after = second
        .header("retry-after")
        .unwrap_or_else(|| panic!("429 must carry Retry-After: {:?}", second.headers))
        .parse::<u64>()
        .expect("numeric Retry-After");
    assert!(retry_after >= 1, "a retry horizon is always given: {retry_after}");
    // ADR 0003: the limiter headers travel with the refusal too, so the caller
    // can budget its calls exactly as on the 200 path.
    assert_eq!(
        second.header("x-ratelimit-limit"),
        Some("1"),
        "the effective limit: {:?}",
        second.headers
    );
    let remaining = second
        .header("x-ratelimit-remaining")
        .unwrap_or_else(|| panic!("429 must carry X-RateLimit-Remaining: {:?}", second.headers));
    assert!(remaining.parse::<u64>().is_ok(), "numeric remaining: {remaining}");
    let reset = second
        .header("x-ratelimit-reset")
        .unwrap_or_else(|| panic!("429 must carry X-RateLimit-Reset: {:?}", second.headers));
    assert!(reset.parse::<u64>().is_ok(), "numeric reset: {reset}");
    assert_eq!(upstream.count(), 1, "the refused request never reaches the upstream");

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A request that passes the throttle gets the `X-RateLimit-*` headers
/// computed from the resolved policy on the proxied response.
#[tokio::test(flavor = "multi_thread")]
async fn a_proxied_response_carries_the_rate_limit_headers() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let router = fixture.router();
    let (status, created) = create_upstream(&router, rate_limited_body("metered.test", &upstream.addr())).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/metered.test/v1/models", &[]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("x-ratelimit-limit"), Some("1"));
    assert_eq!(response.header("x-ratelimit-remaining"), Some("0"));
    let reset = response
        .header("x-ratelimit-reset")
        .unwrap_or_else(|| panic!("X-RateLimit-Reset is set: {:?}", response.headers));
    assert!(reset.parse::<u64>().is_ok(), "numeric reset: {reset}");
    assert_eq!(response.header("x-oagw-error-source"), Some("upstream"));

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Credential injection ────────────────────────────────────────────────

/// Upstream with the `apikey` auth plugin bound to a `cred://` reference.
fn api_key_upstream(alias: &str, addr: &std::net::SocketAddr, reference: &str) -> Value {
    let mut body = upstream_body(alias, addr);
    body["auth"] = json!({
        "type": AUTH_API_KEY,
        "config": { "api_key_ref": reference, "header": "x-api-key" },
    });
    body
}

/// The `apikey` auth plugin injects the secret resolved from credstore into the
/// configured header, and the raw secret never leaks into the response.
#[tokio::test(flavor = "multi_thread")]
async fn api_key_plugin_injects_the_resolved_secret_into_the_upstream_request() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let credstore = MockCredStoreClient::with_secrets(vec![("api-key".to_owned(), "sk-live-42".to_owned())]);
    let fixture =
        Fixture::start_with(state_with(config(), Some(std::sync::Arc::new(credstore)))).await;
    let router = fixture.router();
    let (status, created) =
        create_upstream(&router, api_key_upstream("keyed.test", &upstream.addr(), "cred://api-key"))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/keyed.test/v1/models", &[]).await;
    assert_eq!(response.status, 200, "{}", response.text());

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    assert_eq!(
        requests[0].header("x-api-key"),
        Some("sk-live-42"),
        "the resolved secret is injected: {:?}",
        requests[0].headers
    );
    assert!(
        !response.text().contains("sk-live-42"),
        "the secret never echoes back to the caller: {}",
        response.text()
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A `cred://` reference the store cannot resolve fails the request with a 500
/// problem document — the caller never gets an unauthenticated forward.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_secret_is_never_forwarded_unauthenticated() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    // An empty store: every reference resolves to "not found".
    let fixture = Fixture::start_with(state_with(
        config(),
        Some(std::sync::Arc::new(MockCredStoreClient::empty())),
    ))
    .await;
    let router = fixture.router();
    let (status, created) =
        create_upstream(&router, api_key_upstream("secretless.test", &upstream.addr(), "cred://absent"))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/secretless.test/v1/models", &[]).await;
    response.assert_problem(500, SECRET_NOT_FOUND);
    let requests = upstream.requests();
    assert!(requests.is_empty(), "nothing is forwarded without credentials");
    assert!(
        !requests.iter().any(|request| request.header("x-api-key").is_some()),
        "no credential-free forward ever happened"
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A malformed auth config is a 400 validation error, again before any upstream
/// traffic.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_auth_plugin_config_fails_closed() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(
        config(),
        Some(std::sync::Arc::new(MockCredStoreClient::with_secrets(vec![(
            "api-key".to_owned(),
            "sk-live-42".to_owned(),
        )]))),
    ))
    .await;
    let router = fixture.router();
    let mut body = api_key_upstream("misconfigured.test", &upstream.addr(), "cred://api-key");
    body["auth"]["config"] = json!({ "header": "x-api-key" });
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/misconfigured.test/v1/models", &[]).await;
    response.assert_problem(400, VALIDATION_ERROR);
    assert_eq!(upstream.count(), 0, "a misconfigured plugin never forwards");

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Required-headers guard (ADR 0009) ───────────────────────────────────

/// Upstream + route with a `plugins` block and an optional header policy bound,
/// served by `behaviour`.
async fn plugin_fixture(
    alias: &str,
    behaviour: Behaviour,
    plugins: Value,
    headers: Option<Value>,
) -> (Fixture, FakeUpstream) {
    let upstream = FakeUpstream::start(behaviour).await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let mut body = upstream_body(alias, &upstream.addr());
    if let Some(headers) = headers {
        body["headers"] = headers;
    }
    body["plugins"] = plugins;
    let router = fixture.router();
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);
    (fixture, upstream)
}

/// Upstream + route with the required-headers guard bound to `required`.
async fn guard_fixture(required: &str) -> (Fixture, FakeUpstream) {
    let plugins = json!({
        "items": [GUARD_REQUIRED_HEADERS],
        "configs": {
            GUARD_REQUIRED_HEADERS: { "required_request_headers": required }
        },
    });
    plugin_fixture(
        "guarded.test",
        Behaviour::Canned(FakeResponse::json(200, br#"{"ok":true}"#.to_vec())),
        plugins,
        None,
    )
    .await
}

/// A request missing a required header is rejected with a 400 problem document
/// and never reaches the upstream.
#[tokio::test(flavor = "multi_thread")]
async fn required_headers_guard_rejects_a_request_before_it_reaches_the_upstream() {
    let (fixture, upstream) = guard_fixture("x-correlation-id").await;
    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/guarded.test/v1/models", &[]).await;
    response.assert_problem(400, VALIDATION_ERROR);
    assert!(
        response.text().contains("x-correlation-id"),
        "the refusal names the missing header: {}",
        response.text()
    );
    assert!(
        response.text().contains("REQUIRED_HEADER_MISSING"),
        "the refusal carries the ADR 0009 error code: {}",
        response.text()
    );
    assert_eq!(upstream.count(), 0, "a rejected request is not forwarded");
    upstream.shutdown().await;
    fixture.stop().await;
}

/// Inbound headers reach the upstream only through the upstream's header
/// policy: the allowlist forwards the correlation header, drops everything
/// else, and `Host` is always re-derived from the endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn inbound_headers_are_forwarded_per_the_passthrough_policy() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let mut body = upstream_body("allowlist.test", &upstream.addr());
    body["headers"] = json!({
        "request": {
            "passthrough": "allowlist",
            "passthrough_allowlist": ["x-correlation-id", "content-type", "accept"],
        }
    });
    let router = fixture.router();
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let gateway = fixture.gateway();
    let raw: &str = "GET /oagw/v1/proxy/allowlist.test/v1/models HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         x-correlation-id: abc-123\r\n\
         x-secret: never-forward-me\r\n\r\n";
    let response = raw_send(gateway, raw.to_owned()).await;
    assert_eq!(response.status, 200, "{}", response.text());

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("x-correlation-id"),
        Some("abc-123"),
        "an allowlisted header is forwarded: {:?}",
        requests[0].headers
    );
    assert_eq!(
        requests[0].header("x-secret"),
        None,
        "a header outside the allowlist is not forwarded"
    );
    assert_eq!(
        requests[0].header("host"),
        Some(upstream.addr().to_string().as_str()),
        "the Host is the endpoint's, never the inbound one"
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Required-headers guard: accept path and response phase (ADR 0009) ───

/// The guard's *accept* path: a request that carries the required header is
/// forwarded, and the upstream sees it.
///
/// The guard inspects the set the gateway forwards, so the upstream's header
/// policy has to allow the correlation header through — a `none` policy
/// forwards nothing, and the guard would refuse the same request.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_carrying_the_required_header_is_forwarded() {
    let plugins = json!({
        "items": [GUARD_REQUIRED_HEADERS],
        "configs": {
            GUARD_REQUIRED_HEADERS: { "required_request_headers": "x-correlation-id" }
        },
    });
    let headers = json!({
        "request": { "passthrough": "allowlist", "passthrough_allowlist": ["x-correlation-id"] }
    });
    let (fixture, upstream) = plugin_fixture(
        "accepted.test",
        Behaviour::Canned(FakeResponse::json(200, br#"{"ok":true}"#.to_vec())),
        plugins,
        Some(headers),
    )
    .await;

    let response = raw_send(
        fixture.gateway(),
        get_with_header(
            "accepted.test",
            "v1/models",
            "x-correlation-id",
            "abc-123",
        ),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("x-oagw-error-source"), Some("upstream"));

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "the request was forwarded");
    assert_eq!(
        requests[0].header("x-correlation-id"),
        Some("abc-123"),
        "the accepted header reaches the upstream: {:?}",
        requests[0].headers
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

/// The guard's *response* phase: an upstream that omits a required response
/// header is turned into a 502 problem document.
#[tokio::test(flavor = "multi_thread")]
async fn a_response_missing_a_required_header_is_refused_with_502() {
    let plugins = json!({
        "items": [GUARD_REQUIRED_HEADERS],
        "configs": {
            GUARD_REQUIRED_HEADERS: { "required_response_headers": "content-type" }
        },
    });
    // An upstream that answers without a `content-type` at all.
    let (fixture, upstream) = plugin_fixture(
        "guarded.response.test",
        Behaviour::Canned(FakeResponse {
            status: 200,
            headers: Vec::new(),
            body: b"no content type".to_vec(),
        }),
        plugins,
        None,
    )
    .await;

    let response =
        raw_get(fixture.gateway(), "/oagw/v1/proxy/guarded.response.test/v1/models", &[]).await;
    response.assert_problem(502, PROTOCOL_ERROR_502);
    assert!(
        response.text().contains("content-type"),
        "the refusal names the missing header: {}",
        response.text()
    );
    assert!(
        response.text().contains("REQUIRED_HEADER_MISSING"),
        "the refusal carries the ADR 0009 error code: {}",
        response.text()
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A response that does carry the required header is proxied untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_response_carrying_the_required_header_is_proxied() {
    let plugins = json!({
        "items": [GUARD_REQUIRED_HEADERS],
        "configs": {
            GUARD_REQUIRED_HEADERS: { "required_response_headers": "content-type" }
        },
    });
    let (fixture, upstream) = plugin_fixture(
        "guarded.ok.test",
        Behaviour::Canned(FakeResponse::json(200, br#"{"ok":true}"#.to_vec())),
        plugins,
        None,
    )
    .await;

    let response =
        raw_get(fixture.gateway(), "/oagw/v1/proxy/guarded.ok.test/v1/models", &[]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json()["ok"], Value::Bool(true));
    assert_eq!(upstream.count(), 1);

    upstream.shutdown().await;
    fixture.stop().await;
}

/// A guard and a transform plugin bound *together* on one upstream each run in
/// their own phase: the shared `plugins.items` chain must not make either phase
/// report the other's ref as `plugin.not_found` (503).
#[tokio::test(flavor = "multi_thread")]
async fn a_guard_and_a_transform_bound_together_do_not_fail_the_request() {
    let plugins = json!({
        "items": [GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID],
        "configs": {
            GUARD_REQUIRED_HEADERS: { "required_request_headers": "x-correlation-id" }
        },
    });
    let headers = json!({
        "request": { "passthrough": "allowlist", "passthrough_allowlist": ["x-correlation-id"] }
    });
    let (fixture, upstream) = plugin_fixture(
        "chained.test",
        Behaviour::Canned(FakeResponse::json(200, br#"{"ok":true}"#.to_vec())),
        plugins,
        Some(headers),
    )
    .await;

    let response = raw_send(
        fixture.gateway(),
        get_with_header("chained.test", "v1/models", "x-correlation-id", "abc-123"),
    )
    .await;
    assert_eq!(
        response.status, 200,
        "a mixed chain must not 503: {}",
        response.text()
    );

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("x-correlation-id"),
        Some("abc-123"),
        "the guard accepted and the header was forwarded: {:?}",
        requests[0].headers
    );
    let request_id = requests[0].header("x-request-id");
    assert!(
        request_id.is_some_and(|value| !value.is_empty()),
        "the transform phase added its own header: {:?}",
        requests[0].headers
    );

    // The guard in the same chain still enforces its requirement.
    let refused = raw_get(fixture.gateway(), "/oagw/v1/proxy/chained.test/v1/models", &[]).await;
    refused.assert_problem(400, VALIDATION_ERROR);
    assert_eq!(upstream.count(), 1, "the refused request is not forwarded");

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── X-OAGW-Target-Host matrix (ADR 0001) ────────────────────────────────

/// A two-endpoint pool with a dot-containing alias, served by two fake
/// upstreams that share a port on two distinct loopback addresses.
///
/// The model requires every endpoint of a pool to share scheme *and* port, so
/// the only way to tell two endpoints apart is to give each its own loopback
/// address (`127.0.0.2` is loopback on Linux just like `127.0.0.1`).
async fn multi_endpoint_fixture() -> (Fixture, FakeUpstream, FakeUpstream) {
    let first = FakeUpstream::start(Behaviour::Canned(FakeResponse::plain(
        200,
        b"first-endpoint".to_vec(),
    )))
    .await;
    // `127.0.0.2` is loopback on Linux just like `127.0.0.1`, so both endpoints
    // can share the port the model requires them to share.
    let second = FakeUpstream::start_on(
        std::net::IpAddr::from([127, 0, 0, 2]),
        first.addr().port(),
        Behaviour::Canned(FakeResponse::plain(200, b"second-endpoint".to_vec())),
    )
    .await;
    assert_eq!(first.addr().port(), second.addr().port(), "one pool, one port");

    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let router = fixture.router();
    let body = json!({
        "alias": "multi.test",
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [http_endpoint(&first.addr()), http_endpoint(&second.addr())] },
        "tags": ["test"],
    });
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);
    (fixture, first, second)
}

fn target_host(value: &str) -> String {
    format!(
        "GET /oagw/v1/proxy/multi.test/v1/models HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         x-oagw-target-host: {value}\r\n\r\n"
    )
}

/// A multi-endpoint pool with a dotted alias requires the header.
#[tokio::test(flavor = "multi_thread")]
async fn a_multi_endpoint_pool_without_target_host_is_refused() {
    let (fixture, first, second) = multi_endpoint_fixture().await;
    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/multi.test/v1/models", &[]).await;
    response.assert_problem(400, MISSING_TARGET_HOST);
    assert_eq!(first.count() + second.count(), 0);
    first.shutdown().await;
    second.shutdown().await;
    fixture.stop().await;
}

/// A header that names no configured endpoint is refused.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_target_host_is_refused() {
    let (fixture, first, second) = multi_endpoint_fixture().await;
    let response = raw_send(fixture.gateway(), target_host("nope.example")).await;
    response.assert_problem(400, UNKNOWN_TARGET_HOST);
    assert_eq!(first.count() + second.count(), 0);
    first.shutdown().await;
    second.shutdown().await;
    fixture.stop().await;
}

/// A header that is not a bare host is refused before the endpoint list is
/// consulted.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_target_host_is_refused() {
    let (fixture, first, second) = multi_endpoint_fixture().await;
    let response = raw_send(fixture.gateway(), target_host("127.0.0.1:8080")).await;
    response.assert_problem(400, INVALID_TARGET_HOST);
    assert_eq!(first.count() + second.count(), 0);
    first.shutdown().await;
    second.shutdown().await;
    fixture.stop().await;
}

/// A valid header pins the endpoint the request is forwarded to.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_target_host_selects_that_endpoint() {
    let (fixture, first, second) = multi_endpoint_fixture().await;
    let to_first = raw_send(fixture.gateway(), target_host(&first.ip().to_string())).await;
    assert_eq!(to_first.status, 200, "{}", to_first.text());
    assert_eq!(to_first.text(), "first-endpoint");
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 0);

    let to_second = raw_send(fixture.gateway(), target_host(&second.ip().to_string())).await;
    assert_eq!(to_second.status, 200, "{}", to_second.text());
    assert_eq!(to_second.text(), "second-endpoint");
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 1);
    assert_eq!(to_second.header("x-oagw-error-source"), Some("upstream"));

    first.shutdown().await;
    second.shutdown().await;
    fixture.stop().await;
}

/// A single-endpoint pool is served without the header, and the header is
/// accepted when it names that endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_single_endpoint_pool_is_served_with_and_without_the_header() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"single":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "single.test", &upstream.addr(), "/v1").await;

    let without = raw_get(fixture.gateway(), "/oagw/v1/proxy/single.test/v1/models", &[]).await;
    assert_eq!(without.status, 200, "{}", without.text());

    let gateway = fixture.gateway();
    let with = format!(
        "GET /oagw/v1/proxy/single.test/v1/models HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         x-oagw-target-host: {}\r\n\r\n",
        upstream.ip()
    );
    let with = raw_send(gateway, with).await;
    assert_eq!(with.status, 200, "{}", with.text());
    assert_eq!(upstream.count(), 2, "both requests were served");

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Request fidelity ────────────────────────────────────────────────────

/// The request the caller sent is what the upstream receives: verbatim body,
/// forwarded query string, unchanged suffix — and the buffered response body
/// comes back intact.
///
/// The upstream's header policy is `all`, so the structural headers
/// (`content-type`, `accept`) travel with the body; with the default `none`
/// policy the gateway forwards no inbound header at all.
#[tokio::test(flavor = "multi_thread")]
async fn request_fidelity_body_query_and_response_body() {
    const REQUEST_BODY: &str = r#"{"prompt":"say hi","n":3,"nested":{"a":[1,2,3]}}"#;
    const RESPONSE_BODY: &str = r#"{"echo":{"prompt":"say hi","n":3},"tokens":["a","b"]}"#;
    let upstream = FakeUpstream::start(Behaviour::Canned(
        FakeResponse::json(200, RESPONSE_BODY.as_bytes().to_vec())
            .with_header("x-upstream-fingerprint", "fp-1"),
    ))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let mut body = upstream_body("fidelity.test", &upstream.addr());
    body["headers"] = json!({ "request": { "passthrough": "all" } });
    let router = fixture.router();
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let raw = format!(
        "POST /oagw/v1/proxy/fidelity.test/v1/chat/completions?api-version=2024-02&dry=run HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         accept: application/json\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\r\n\
         {REQUEST_BODY}",
        REQUEST_BODY.len()
    );
    let response = raw_send(fixture.gateway(), raw.to_owned()).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.text(), RESPONSE_BODY, "the body is buffered intact");
    assert_eq!(response.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(response.header("x-upstream-fingerprint"), Some("fp-1"));

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/chat/completions");
    assert_eq!(sent.query.as_deref(), Some("api-version=2024-02&dry=run"));
    assert_eq!(sent.body_str(), REQUEST_BODY, "the body is forwarded byte for byte");
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.header("accept"), Some("application/json"));
    assert_eq!(
        sent.header("host"),
        Some(upstream.addr().to_string().as_str()),
        "the Host is the endpoint's authority, never the inbound one"
    );
    assert!(
        !sent.headers.iter().any(|(name, _)| name.starts_with("x-oagw-")),
        "the OAGW control headers are stripped: {:?}",
        sent.headers
    );

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── gRPC refusal ────────────────────────────────────────────────────────

/// A gRPC upstream has no proxy path in this phase: an http-matched request to
/// it is refused with a 502 protocol error and nothing is dialed.
///
/// The match has to be an `http` match: route resolution only consults
/// `match.http`, so a route carrying only a `match.grpc` block never matches
/// anything and cannot be reached at all (reported separately).
#[tokio::test(flavor = "multi_thread")]
async fn a_grpc_upstream_is_refused_with_a_502_protocol_error() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::plain(
        200,
        b"not-a-grpc-response".to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    let router = fixture.router();
    let body = json!({
        "alias": "grpc.test",
        "protocol": PROTOCOL_GRPC,
        "server": { "endpoints": [http_endpoint(&upstream.addr())] },
        "tags": ["test"],
    });
    let (status, created) = create_upstream(&router, body).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("upstream id").to_owned();
    let (status, _) = support::create_route(&router, route_body(&id, "/v1")).await;
    assert_eq!(status, StatusCode::CREATED);

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/grpc.test/v1/models", &[]).await;
    response.assert_problem(502, PROTOCOL_ERROR);
    assert!(
        response.text().contains("grpc"),
        "the refusal names the unsupported protocol: {}",
        response.text()
    );
    assert_eq!(upstream.count(), 0, "nothing is dialed for a gRPC upstream");

    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Plaintext gate ──────────────────────────────────────────────────────

/// `allow_http_upstream: true` dials a plaintext endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_plaintext_upstream_is_allowed_when_the_config_permits_it() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"plain":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "open.test", &upstream.addr(), "/v1").await;

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/open.test/v1/models", &[]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(upstream.count(), 1);
    upstream.shutdown().await;
    fixture.stop().await;
}

/// `allow_http_upstream: false` refuses the same endpoint with the
/// `link.unavailable.v1` problem type, and never dials it.
#[tokio::test(flavor = "multi_thread")]
async fn a_plaintext_upstream_is_refused_when_the_config_forbids_it() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"plain":true}"#.to_vec(),
    )))
    .await;
    let mut config = config();
    config.allow_http_upstream = false;
    let fixture = Fixture::start_with(state_with(config, None)).await;
    wire_up(&fixture.router(), "closed.test", &upstream.addr(), "/v1").await;

    let response = raw_get(fixture.gateway(), "/oagw/v1/proxy/closed.test/v1/models", &[]).await;
    response.assert_problem(503, LINK_UNAVAILABLE);
    assert!(
        response.text().contains("plaintext"),
        "the refusal explains the policy: {}",
        response.text()
    );
    assert_eq!(upstream.count(), 0, "a refused endpoint is never dialed");
    upstream.shutdown().await;
    fixture.stop().await;
}

// ── Routing basics over the real socket ─────────────────────────────────

/// An unknown alias and an unmatched path produce the DESIGN problem documents
/// over the wire, with the gateway named as the source.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_alias_and_unmatched_path_are_gateway_problems() {
    let fixture = Fixture::start().await;

    let alias = raw_get(fixture.gateway(), "/oagw/v1/proxy/nobody.test/v1/models", &[]).await;
    alias.assert_problem(404, ROUTE_NOT_FOUND);

    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    wire_up(&fixture.router(), "narrow.test", &upstream.addr(), "/v1").await;
    let miss = raw_get(fixture.gateway(), "/oagw/v1/proxy/narrow.test/v2/nope", &[]).await;
    miss.assert_problem(404, ROUTE_NOT_FOUND);
    assert_eq!(upstream.count(), 0, "an unmatched path never reaches the upstream");

    upstream.shutdown().await;
    fixture.stop().await;
}

/// `OPTIONS` preflights are answered by the gateway itself, permissively,
/// without consulting the upstream.
#[tokio::test(flavor = "multi_thread")]
async fn a_preflight_is_answered_permissively_by_the_gateway() {
    let upstream = FakeUpstream::start(Behaviour::Canned(FakeResponse::json(
        200,
        br#"{"ok":true}"#.to_vec(),
    )))
    .await;
    let fixture = Fixture::start_with(state_with(config(), None)).await;
    wire_up(&fixture.router(), "preflight.test", &upstream.addr(), "/v1").await;

    let raw: &str = "OPTIONS /oagw/v1/proxy/preflight.test/v1/models HTTP/1.1\r\n\
         host: gateway\r\n\
         connection: close\r\n\
         origin: https://any.example\r\n\
         access-control-request-method: DELETE\r\n\r\n";
    let response = raw_send(fixture.gateway(), raw.to_owned()).await;
    assert_eq!(response.status, 204, "{}", response.text());
    assert_eq!(response.header("access-control-allow-origin"), Some("*"));
    assert!(
        response
            .header("access-control-allow-methods")
            .is_some_and(|value| value.contains("DELETE")),
        "permissive methods: {:?}",
        response.headers
    );
    assert_eq!(upstream.count(), 0, "a preflight never reaches the upstream");

    upstream.shutdown().await;
    fixture.stop().await;
}
