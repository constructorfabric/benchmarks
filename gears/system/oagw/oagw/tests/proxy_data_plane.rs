//! End-to-end data-plane coverage (PRD `cpt-cf-oagw-fr-streaming`, DESIGN
//! "Proxy Endpoint"): a real axum router built through
//! [`oagw::api::rest::register_routes`] proxies real HTTP traffic to real
//! upstreams ([httpmock](httpmock) for canned answers, a local echo server for
//! assertions about what actually arrived).
//!
//! Everything asserted here is wire behaviour: the status the client sees, the
//! bytes that reach the upstream, and the headers that do (or do not) survive
//! the hop.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::{Method, StatusCode};
use httpmock::MockServer;
use serde_json::{Value, json};

use crate::common::{catch_all_route, create_upstream, harness};

/// Register an enabled catch-all upstream + route for `server` under `alias`.
async fn route_to(h: &common::Harness, host: &str, port: u16, alias: &str) -> String {
    let (status, body) = create_upstream(h, common::http_upstream(host, port, Some(alias))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let upstream_id = body["id"].as_str().expect("upstream id").to_owned();
    let (status, body) = common::create_route(h, catch_all_route(&upstream_id, "/")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    alias.to_owned()
}

/// `POST /oagw/v1/proxy/{alias}{path}` returning `(status, body)`.
async fn post(h: &common::Harness, alias: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let (status, value, _) = h
        .json(
            Method::POST,
            &format!("/oagw/v1/proxy/{alias}{path}"),
            &[],
            Some(body),
        )
        .await;
    (status, value)
}

#[tokio::test]
async fn a_get_is_proxied_to_the_upstream_with_its_path() {
    let server = MockServer::start();
    let answer = json!({"object": "list", "data": ["one"]});
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/models")
            .header("accept", "application/json");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(answer.clone());
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "list").await;
    let response = common::proxy(
        &h,
        Method::GET,
        &alias,
        "/v1/models",
        &[("accept", "application/json")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = common::body_bytes(response.into_body()).await;
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), answer);
}

#[tokio::test]
async fn a_successful_response_also_carries_the_error_source_header() {
    // ADR 0007 "Confirmation": "success responses include the header", not
    // only the error paths. A relayed body is upstream-owned, so the value is
    // `upstream`; a gateway-answered exchange (CORS preflight) tags `gateway`.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/ok");
        then.status(200).body("fine");
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "okroute").await;
    let response = common::proxy(&h, Method::GET, &alias, "/ok", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "a relayed success response is labelled `upstream`"
    );
}

#[tokio::test]
async fn a_post_body_and_its_content_type_reach_the_upstream() {
    let (host, port) = common::echo_server().await;
    let h = harness();
    let alias = route_to(&h, &host, port, "echo").await;

    let sent = json!({"model": "gpt-test", "stream": true});
    let (status, echoed) = post(&h, &alias, "/v1/chat/completions", sent.clone()).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    assert_eq!(echoed["method"], "POST");
    assert_eq!(echoed["path"], "/v1/chat/completions");
    assert_eq!(echoed["json"], sent);
    assert_eq!(echoed["headers"]["content-type"], "application/json");
}

#[tokio::test]
async fn the_upstream_status_code_and_headers_are_relayed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::POST).path("/things");
        then.status(201).header("x-created", "yes").body("made");
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "maker").await;
    let response = common::proxy(&h, Method::POST, &alias, "/things", &[], None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("x-created")
            .and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    assert_eq!(common::body_string(response.into_body()).await, "made");
}

#[tokio::test]
async fn gateway_headers_never_reach_the_upstream() {
    let (host, port) = common::echo_server().await;
    let h = harness();
    let (status, upstream) = create_upstream(
        &h,
        json!({
            "enabled": true,
            "alias": "hygiene",
            "server": {"endpoints": [{"scheme": "http", "host": &host, "port": port}]},
            "headers": {"request": {"passthrough": "all"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    common::create_route(&h, catch_all_route(&upstream_id, "/")).await;
    let _alias = "hygiene";

    let (status, echoed, _) = h
        .json(
            Method::POST,
            "/oagw/v1/proxy/hygiene/relayed",
            &[("x-tenant", "acme"), ("x-oagw-trace-id", "forged")],
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{echoed}");

    let headers = echoed["headers"].as_object().expect("echoed headers");
    // The gateway's own routing headers are never forwarded, so a client cannot
    // forge a trace id or a target host (DESIGN "Hop-by-hop headers").
    for name in [
        "x-oagw-trace-id",
        "x-oagw-target-host",
        "x-oagw-error-source",
    ] {
        assert!(
            !headers.contains_key(name),
            "{name} must not be forwarded: {headers:?}"
        );
    }
    // An ordinary operator header is.
    assert_eq!(headers["x-tenant"], "acme", "{headers:?}");
}

#[tokio::test]
async fn the_client_is_not_able_to_spoof_the_upstream_host() {
    let (host, port) = common::echo_server().await;
    let h = harness();
    let alias = route_to(&h, &host, port, "hosted").await;

    let (status, echoed) = post(&h, &alias, "/who-am-i", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    let host_header = echoed["headers"]["host"].as_str().expect("host header");
    assert_eq!(
        host_header,
        format!("{host}:{port}"),
        "the Host header always comes from the resolved endpoint"
    );
}

#[tokio::test]
async fn the_path_suffix_is_appended_to_the_route_path() {
    let (host, port) = common::echo_server().await;
    let h = harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("nested"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, route) = common::create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET", "POST"], "path": "/api/v1",
                               "path_suffix_mode": "append"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    let (status, echoed) = post(&h, "nested", "/api/v1/things/42", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    assert_eq!(echoed["path"], "/api/v1/things/42");
}

#[tokio::test]
async fn a_disabled_suffix_mode_forwards_the_route_path_verbatim() {
    let (host, port) = common::echo_server().await;
    let h = harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream(&host, port, Some("literal"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (status, route) = common::create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["POST"], "path": "/v1/thing",
                               "path_suffix_mode": "disabled"}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");

    let (status, echoed) = post(&h, "literal", "/v1/thing", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{echoed}");
    assert_eq!(echoed["path"], "/v1/thing");
}

#[tokio::test]
async fn an_unknown_alias_is_a_404_problem() {
    let h = harness();
    let (status, problem, headers) = h
        .json(Method::GET, "/oagw/v1/proxy/does-not-exist", &[], None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(problem["status"], json!(404));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway"),
        "the gateway owns an unresolved alias"
    );
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_path_no_route_serves_is_a_404_problem() {
    let h = harness();
    let (status, upstream) =
        create_upstream(&h, common::http_upstream("127.0.0.1", 9, Some("narrow"))).await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let (_, _) = common::create_route(
        &h,
        json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": {"http": {"methods": ["GET"], "path": "/only/this",
                               "path_suffix_mode": "disabled"}}
        }),
    )
    .await;

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/narrow/other", &[], None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_problem() {
    // Loopback port 1 is never listening, so the dial fails. DESIGN error
    // table: that is a `502 DownstreamError`, and ADR 0007 still attributes it
    // to the gateway because the upstream produced no response to relay.
    let h = harness();
    let alias = route_to(&h, "127.0.0.1", 1, "dark").await;

    let (status, problem, headers) = h
        .json(
            Method::GET,
            &format!("/oagw/v1/proxy/{alias}/anything"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
    assert_eq!(problem["status"], json!(502));
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway"),
        "a failed dial is a gateway-side error"
    );
}

#[tokio::test]
async fn an_upstream_5xx_is_relayed_as_received() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/broken");
        then.status(500).body("upstream blew up");
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "broken").await;
    let response = common::proxy(&h, Method::GET, &alias, "/broken", &[], None).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // ADR 0007 "Confirmation": the header is present on every response, and an
    // error the upstream produced is labelled `upstream`, not `gateway`.
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "a relayed upstream error is labelled `upstream`, not `gateway`"
    );
    // The bytes are relayed verbatim, not rewritten into a gateway problem doc.
    assert_eq!(
        common::body_string(response.into_body()).await,
        "upstream blew up"
    );
}

#[tokio::test]
async fn a_disabled_upstream_is_a_503_problem() {
    let server = MockServer::start();
    let h = harness();
    let mut upstream = common::http_upstream("127.0.0.1", server.address().port(), Some("paused"));
    upstream["enabled"] = json!(false);
    let (status, body) = create_upstream(&h, upstream).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let upstream_id = body["id"].as_str().unwrap().to_owned();
    common::create_route(&h, catch_all_route(&upstream_id, "/")).await;

    let (status, problem, _) = h
        .json(Method::GET, "/oagw/v1/proxy/paused/anything", &[], None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.disabled.v1"
    );
    assert_eq!(problem["status"], json!(503));
    assert_eq!(problem["upstream_id"], json!(upstream_id));
}

#[tokio::test]
async fn a_head_request_is_proxied_without_a_body() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::HEAD).path("/head");
        then.status(200).body("payload");
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "headed").await;
    let response = common::proxy(&h, Method::HEAD, &alias, "/head", &[], None).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        response.headers()
    );
    assert_eq!(
        common::body_bytes(response.into_body()).await.len(),
        0,
        "a HEAD response carries no body"
    );
}

#[tokio::test]
async fn the_alias_resolves_case_insensitively() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/ping");
        then.status(200).body("pong");
    });

    let h = harness();
    route_to(&h, "127.0.0.1", server.address().port(), "Ping.Pong").await;
    let response = common::proxy(&h, Method::GET, "ping.pong", "/ping", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(common::body_string(response.into_body()).await, "pong");
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn repeated_requests_all_dial_the_upstream() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/ping");
        then.status(200).body("pong");
    });

    let h = harness();
    let alias = route_to(&h, "127.0.0.1", server.address().port(), "twice").await;
    for _ in 0..2 {
        let response = common::proxy(&h, Method::GET, &alias, "/ping", &[], None).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(mock.calls(), 2, "every request is dialled");
}
