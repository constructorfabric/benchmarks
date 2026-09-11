//! Proxy behaviour over plain HTTP and server-sent events, against a mock
//! upstream whose bytes the test controls.

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use common::{Fixture, HTTP_PROTOCOL, MockBehavior, MockUpstream, body_text, empty_request};
use futures_util::StreamExt;
use http::{Request, StatusCode};
use oagw::OagwConfig;
use oagw::test_utils::HarnessBuilder;
use serde_json::json;
use uuid::Uuid;

fn echo(body: &str) -> MockBehavior {
    MockBehavior::Fixed {
        status: 200,
        content_type: "application/json",
        body: body.to_owned(),
    }
}

fn permissive_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        proxy_timeout_secs: 2,
        connect_timeout_secs: 2,
        ssrf_policy: oagw::config::SsrfPolicy {
            enabled: false,
            ..oagw::config::SsrfPolicy::default()
        },
        ..OagwConfig::default()
    }
}

// -- The happy path ---------------------------------------------------------

#[tokio::test]
async fn a_get_is_forwarded_and_the_response_relayed() {
    let upstream = MockUpstream::start(echo(r#"{"ok":true}"#)).await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture
        .send(empty_request("GET", "/oagw/v1/proxy/mock/v1/models"))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    // The relayed answer states that it came from the upstream.
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "upstream"
    );
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(body_text(response).await, r#"{"ok":true}"#);

    let seen = upstream.last_request().await;
    assert_eq!(seen.request_line(), "GET /v1/models HTTP/1.1");
    // Host is replaced by the upstream authority.
    assert_eq!(
        seen.header("host").as_deref(),
        Some(format!("127.0.0.1:{}", upstream.port()).as_str())
    );
}

#[tokio::test]
async fn a_post_body_and_its_content_type_reach_the_upstream() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    fixture
        .wire_upstream("mock", upstream.port(), json!(["GET", "POST"]))
        .await;

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/mock/v1/chat")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gpt-4"}"#))
        .unwrap();
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let seen = upstream.last_request().await;
    assert_eq!(seen.body, r#"{"model":"gpt-4"}"#);
    assert_eq!(seen.header("content-type").as_deref(), Some("application/json"));
    assert_eq!(seen.header("content-length").as_deref(), Some("17"));
}

#[tokio::test]
async fn the_root_proxy_path_works_without_a_suffix() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.last_request().await.target(), "/");
}

// -- Header hygiene ---------------------------------------------------------

#[tokio::test]
async fn hop_by_hop_and_credential_headers_never_reach_the_upstream() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let id = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            // Even the most permissive passthrough must not leak the caller's
            // platform credentials to a third party.
            "headers": {"request": {"passthrough": "all"}},
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": id["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("authorization", "Bearer platform-token")
        .header("cookie", "session=secret")
        .header("te", "trailers")
        .header("x-oagw-target-host", "127.0.0.1")
        .header("x-keep-me", "yes")
        .body(Body::empty())
        .unwrap();
    fixture.send(request).await;

    let seen = upstream.last_request().await;
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("cookie"), None);
    assert_eq!(seen.header("te"), None);
    assert_eq!(seen.header("x-oagw-target-host"), None);
    assert_eq!(seen.header("x-keep-me").as_deref(), Some("yes"));
}

#[tokio::test]
async fn passthrough_none_forwards_nothing_beyond_the_entity_headers() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("x-caller", "acme")
        .body(Body::empty())
        .unwrap();
    fixture.send(request).await;

    assert_eq!(upstream.last_request().await.header("x-caller"), None);
}

#[tokio::test]
async fn the_configured_header_rules_are_applied_in_order() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "headers": {
                "request": {
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["x-keep", "x-drop"],
                    "remove": ["x-drop"],
                    "set": {"x-tenant": "acme"},
                    "add": {"x-trace": "on"}
                },
                "response": {"set": {"x-served-by": "oagw"}, "remove": ["content-type"]}
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("x-keep", "kept")
        .header("x-drop", "dropped")
        .body(Body::empty())
        .unwrap();
    let response = fixture.send(request).await;

    assert_eq!(response.headers().get("x-served-by").unwrap(), "oagw");
    assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());

    let seen = upstream.last_request().await;
    assert_eq!(seen.header("x-keep").as_deref(), Some("kept"));
    assert_eq!(seen.header("x-drop"), None);
    assert_eq!(seen.header("x-tenant").as_deref(), Some("acme"));
    assert_eq!(seen.header("x-trace").as_deref(), Some("on"));
}

// -- Routing and validation -------------------------------------------------

#[tokio::test]
async fn an_unknown_alias_is_a_404_route_not_found() {
    let fixture = Fixture::new();
    let (status, body) = fixture.get("/oagw/v1/proxy/nope/x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(body["alias"], "nope");
}

#[tokio::test]
async fn a_method_no_route_matches_is_a_404() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture
        .send(empty_request("DELETE", "/oagw/v1/proxy/mock/x"))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_disabled_upstream_is_503_not_404() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "enabled": false,
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn an_unlisted_query_parameter_is_rejected_and_a_listed_one_is_forwarded() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/", "query_allowlist": ["model"]}},
        }))
        .await;

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/x?model=gpt-4").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.last_request().await.target(), "/x?model=gpt-4");

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x?secret=1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn a_route_with_the_suffix_disabled_refuses_a_deeper_path() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {
                "http": {"methods": ["GET"], "path": "/v1/models", "path_suffix_mode": "disabled"}
            },
        }))
        .await;

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/v1/models").await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/v1/models/gpt-4").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_longest_matching_route_decides_the_upstream_path() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    for path in ["/", "/v1", "/v1/chat"] {
        fixture
            .create_route(json!({
                "upstream_id": created["id"],
                "match": {"http": {"methods": ["GET"], "path": path}},
            }))
            .await;
    }

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/v1/chat/completions").await;
    assert_eq!(status, StatusCode::OK);
    // The suffix is appended to the matched route path, which reproduces the
    // caller's path exactly.
    assert_eq!(upstream.last_request().await.target(), "/v1/chat/completions");
}

// -- Error semantics --------------------------------------------------------

#[tokio::test]
async fn an_upstream_error_passes_through_unchanged() {
    let upstream = MockUpstream::start(MockBehavior::Fixed {
        status: 500,
        content_type: "application/json",
        body: r#"{"error":"upstream exploded"}"#.to_owned(),
    })
    .await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock/x")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "upstream"
    );
    // Not wrapped in problem details — the upstream's body verbatim.
    assert_eq!(body_text(response).await, r#"{"error":"upstream exploded"}"#);
}

#[tokio::test]
async fn a_slow_upstream_times_out_with_504_and_retry_guidance() {
    let upstream = MockUpstream::start(MockBehavior::Slow {
        delay: Duration::from_secs(5),
    })
    .await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock/x")).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
    assert!(response.headers().get(http::header::RETRY_AFTER).is_some());

    let (_, body) = common::split(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
}

#[tokio::test]
async fn an_upstream_that_hangs_up_is_a_gateway_error() {
    let upstream = MockUpstream::start(MockBehavior::Hangup).await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock/x")).await;
    assert!(
        response.status().is_server_error(),
        "expected a 5xx, got {}",
        response.status()
    );
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
}

#[tokio::test]
async fn a_plaintext_upstream_is_refused_when_the_flag_is_off() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let mut config = permissive_config();
    config.allow_http_upstream = false;
    let fixture = Fixture::with_builder(HarnessBuilder::new().with_config(config));
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["detail"].as_str().unwrap().contains("allow_http_upstream"),
        "{body}"
    );
}

#[tokio::test]
async fn a_body_over_the_configured_limit_is_413() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let mut config = permissive_config();
    config.max_request_body_bytes = 32;
    let fixture = Fixture::with_builder(HarnessBuilder::new().with_config(config));
    fixture
        .wire_upstream("mock", upstream.port(), json!(["GET", "POST"]))
        .await;

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/mock/x")
        .header("content-type", "application/json")
        .body(Body::from("x".repeat(1024)))
        .unwrap();
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let (_, body) = common::split(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
    );
}

// -- Streaming --------------------------------------------------------------

#[tokio::test]
async fn server_sent_events_arrive_incrementally_rather_than_buffered() {
    let gap = Duration::from_millis(120);
    let upstream = MockUpstream::start(MockBehavior::Sse {
        events: vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
        gap,
    })
    .await;
    let fixture = Fixture::new();
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture
        .send(empty_request("GET", "/oagw/v1/proxy/mock/sse"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );

    let started = Instant::now();
    let mut stream = response.into_body().into_data_stream();
    let mut arrivals = Vec::new();
    let mut collected = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("chunk");
        collected.push_str(&String::from_utf8_lossy(&chunk));
        arrivals.push(started.elapsed());
        if collected.contains("three") {
            break;
        }
    }

    assert!(collected.contains("data: one"), "{collected}");
    assert!(collected.contains("data: three"), "{collected}");
    assert!(
        arrivals.len() >= 2,
        "the stream should surface more than one frame: {arrivals:?}"
    );
    // Buffering would deliver everything at once, well after the last event.
    assert!(
        arrivals[0] < gap,
        "the first event should arrive before the upstream sends the second: {arrivals:?}"
    );
}

#[tokio::test]
async fn a_stream_is_not_cut_short_by_the_request_timeout() {
    // The timeout bounds connect + response head, not the body: an event
    // stream that outlives it must keep flowing.
    let mut config = permissive_config();
    config.proxy_timeout_secs = 1;
    let upstream = MockUpstream::start(MockBehavior::Sse {
        events: vec!["one".to_owned(), "two".to_owned()],
        gap: Duration::from_millis(700),
    })
    .await;
    let fixture = Fixture::with_builder(HarnessBuilder::new().with_config(config));
    fixture.wire_upstream("mock", upstream.port(), json!(["GET"])).await;

    let response = fixture
        .send(empty_request("GET", "/oagw/v1/proxy/mock/sse"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = response.into_body().into_data_stream();
    let mut collected = String::new();
    while let Some(Ok(chunk)) = stream.next().await {
        collected.push_str(&String::from_utf8_lossy(&chunk));
        if collected.contains("two") {
            break;
        }
    }
    assert!(collected.contains("data: two"), "{collected}");
}

// -- Endpoint selection -----------------------------------------------------

#[tokio::test]
async fn a_common_suffix_pool_requires_the_target_host_header() {
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "server": {"endpoints": [
                {"scheme": "https", "host": "us.vendor.com", "port": 443},
                {"scheme": "https", "host": "eu.vendor.com", "port": 443}
            ]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    assert_eq!(created["alias"], "vendor.com");
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/proxy/vendor.com/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(body["valid_hosts"], json!(["us.vendor.com", "eu.vendor.com"]));
}

#[tokio::test]
async fn a_malformed_or_unknown_target_host_is_reported_distinctly() {
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "server": {"endpoints": [
                {"scheme": "https", "host": "us.vendor.com", "port": 443},
                {"scheme": "https", "host": "eu.vendor.com", "port": 443}
            ]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let malformed = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/vendor.com/x")
        .header("x-oagw-target-host", "us.vendor.com:8443")
        .body(Body::empty())
        .unwrap();
    let (status, body) = common::split(fixture.send(malformed).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
    assert_eq!(body["invalid_value"], "us.vendor.com:8443");

    let unknown = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/vendor.com/x")
        .header("x-oagw-target-host", "apac.vendor.com")
        .body(Body::empty())
        .unwrap();
    let (status, body) = common::split(fixture.send(unknown).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

#[tokio::test]
async fn an_explicit_target_host_selects_a_member_of_the_pool() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    // Two spellings of the same loopback address: an explicit alias pool, so
    // the header is optional but honoured.
    let created = fixture
        .create_upstream(json!({
            "alias": "pool",
            "server": {"endpoints": [
                {"scheme": "http", "host": "127.0.0.1", "port": upstream.port()},
                {"scheme": "http", "host": "127.0.0.2", "port": upstream.port()}
            ]},
            "protocol": HTTP_PROTOCOL,
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/pool/x")
        .header("x-oagw-target-host", "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        upstream.last_request().await.header("host").as_deref(),
        Some(format!("127.0.0.1:{}", upstream.port()).as_str())
    );
}

// -- Rate limiting ----------------------------------------------------------

#[tokio::test]
async fn a_rate_limit_rejects_with_429_and_the_standard_headers() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "rate_limit": {
                "sustained": {"rate": 2, "window": "minute"},
                "burst": {"capacity": 2},
                "strategy": "reject"
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    for attempt in 0..2 {
        let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock/x")).await;
        assert_eq!(response.status(), StatusCode::OK, "attempt {attempt}");
        assert!(response.headers().get("x-ratelimit-limit").is_some());
    }

    let response = fixture.send(empty_request("GET", "/oagw/v1/proxy/mock/x")).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get(http::header::RETRY_AFTER).is_some());
    assert_eq!(response.headers().get("x-ratelimit-limit").unwrap(), "2");
    assert_eq!(response.headers().get("x-ratelimit-remaining").unwrap(), "0");
    assert!(response.headers().get("x-ratelimit-reset").is_some());

    let (_, body) = common::split(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
    );
}

#[tokio::test]
async fn the_degrade_strategy_serves_through_the_limit() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "rate_limit": {
                "sustained": {"rate": 1, "window": "hour"},
                "burst": {"capacity": 1},
                "strategy": "degrade"
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    for _ in 0..3 {
        let (status, _) = fixture.get("/oagw/v1/proxy/mock/x").await;
        assert_eq!(status, StatusCode::OK);
    }
}

// -- Credential injection ---------------------------------------------------

#[tokio::test]
async fn the_api_key_plugin_injects_the_resolved_credential() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::with_builder(
        HarnessBuilder::new().with_secret("cred://openai-key", "sk-test-value"),
    );
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": {"secret_ref": "cred://openai-key", "name": "x-api-key"}
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upstream.last_request().await.header("x-api-key").as_deref(),
        Some("sk-test-value")
    );
}

#[tokio::test]
async fn an_unresolvable_credential_answers_500_secret_not_found() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": {"secret_ref": "cred://absent"}
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}

#[tokio::test]
async fn a_catalog_only_auth_identifier_fails_at_proxy_time_with_503() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "auth": {"type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1"},
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("unknown auth plugin"),
        "{body}"
    );
}

// -- Plugin chain -----------------------------------------------------------

#[tokio::test]
async fn a_guard_rejects_before_the_upstream_is_contacted() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "headers": {"request": {
                "passthrough": "allowlist",
                "passthrough_allowlist": ["x-correlation-id"]
            }},
            "plugins": {"items": [{
                "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                "config": {"required_request_headers": "x-correlation-id"}
            }]},
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, body) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "REQUIRED_HEADER_MISSING");
    assert!(
        upstream.requests().await.is_empty(),
        "a rejected request must never reach the upstream"
    );

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("x-correlation-id", "abc")
        .body(Body::empty())
        .unwrap();
    assert_eq!(fixture.send(request).await.status(), StatusCode::OK);
    assert_eq!(
        upstream.last_request().await.header("x-correlation-id").as_deref(),
        Some("abc")
    );
}

#[tokio::test]
async fn the_request_id_transform_mints_a_correlation_id() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "plugins": {"items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"]},
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET"], "path": "/"}},
        }))
        .await;

    let (status, _) = fixture.get("/oagw/v1/proxy/mock/x").await;
    assert_eq!(status, StatusCode::OK);
    let minted = upstream.last_request().await.header("x-request-id");
    assert!(minted.is_some(), "the transform should mint an id");
    assert!(Uuid::parse_str(&minted.unwrap()).is_ok());
}

// -- CORS -------------------------------------------------------------------

#[tokio::test]
async fn a_preflight_is_answered_without_resolving_an_upstream() {
    let fixture = Fixture::new();
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/oagw/v1/proxy/never-configured/users")
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .body(Body::empty())
        .unwrap();

    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        response.headers().get(http::header::ACCESS_CONTROL_MAX_AGE).unwrap(),
        "86400"
    );
}

#[tokio::test]
async fn an_actual_cross_origin_request_is_validated_and_annotated() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let fixture = Fixture::new();
    let created = fixture
        .create_upstream(json!({
            "alias": "mock",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}]},
            "protocol": HTTP_PROTOCOL,
            "cors": {
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET"],
                "expose_headers": ["X-Request-ID"],
                "allow_credentials": true
            },
        }))
        .await;
    fixture
        .create_route(json!({
            "upstream_id": created["id"],
            "match": {"http": {"methods": ["GET", "POST"], "path": "/"}},
        }))
        .await;

    let allowed = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("origin", "https://app.example.com")
        .body(Body::empty())
        .unwrap();
    let response = fixture.send(allowed).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        headers.get(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS).unwrap(),
        "true"
    );
    assert_eq!(
        headers.get("access-control-expose-headers").unwrap(),
        "X-Request-ID"
    );
    assert!(headers.get(http::header::VARY).is_some());

    let disallowed_origin = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/mock/x")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let (status, body) = common::split(fixture.send(disallowed_origin).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );

    let disallowed_method = Request::builder()
        .method("POST")
        .uri("/oagw/v1/proxy/mock/x")
        .header("origin", "https://app.example.com")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = common::split(fixture.send(disallowed_method).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

// -- Hierarchy at proxy time ------------------------------------------------

#[tokio::test]
async fn a_descendant_proxies_through_an_ancestors_upstream() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let fixture = Fixture::with_builder(
        HarnessBuilder::new()
            .with_config(permissive_config())
            .with_tenant_parent(child, parent),
    );

    let (status, created) = common::split(
        fixture
            .send_as(
                parent,
                common::json_request(
                    "POST",
                    "/oagw/v1/upstreams",
                    &json!({
                        "alias": "shared",
                        "server": {"endpoints": [
                            {"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}
                        ]},
                        "protocol": HTTP_PROTOCOL,
                    }),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = common::split(
        fixture
            .send_as(
                parent,
                common::json_request(
                    "POST",
                    "/oagw/v1/routes",
                    &json!({
                        "upstream_id": created["id"],
                        "match": {"http": {"methods": ["GET"], "path": "/"}},
                    }),
                ),
            )
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let response = fixture
        .send_as(child, empty_request("GET", "/oagw/v1/proxy/shared/inherited"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.last_request().await.target(), "/inherited");
}

#[tokio::test]
async fn an_ancestor_disabling_the_alias_stops_the_descendant() {
    let upstream = MockUpstream::start(echo("{}")).await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let fixture = Fixture::with_builder(
        HarnessBuilder::new()
            .with_config(permissive_config())
            .with_tenant_parent(child, parent),
    );

    for (tenant, enabled) in [(parent, false), (child, true)] {
        let (_, created) = common::split(
            fixture
                .send_as(
                    tenant,
                    common::json_request(
                        "POST",
                        "/oagw/v1/upstreams",
                        &json!({
                            "alias": "shared",
                            "enabled": enabled,
                            "server": {"endpoints": [
                                {"scheme": "http", "host": "127.0.0.1", "port": upstream.port()}
                            ]},
                            "protocol": HTTP_PROTOCOL,
                        }),
                    ),
                )
                .await,
        )
        .await;
        fixture
            .send_as(
                tenant,
                common::json_request(
                    "POST",
                    "/oagw/v1/routes",
                    &json!({
                        "upstream_id": created["id"],
                        "match": {"http": {"methods": ["GET"], "path": "/"}},
                    }),
                ),
            )
            .await;
    }

    let response = fixture
        .send_as(child, empty_request("GET", "/oagw/v1/proxy/shared/x"))
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
