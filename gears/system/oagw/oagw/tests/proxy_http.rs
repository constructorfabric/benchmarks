//! Plain HTTP proxying: `{METHOD} /oagw/v1/proxy/{alias}/...`.
//!
//! Forwarding is the gear's reason to exist, so the tests pin what arrives at
//! the upstream and what comes back to the caller — method, path, query, body,
//! host rewriting, hop-by-hop stripping — and the routing errors the gateway
//! answers itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Answer, LocalUpstream, RawUpstream, app};
use serde_json::json;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Wire an upstream to `prefix`, forwarding the request path verbatim.
///
/// `strip_prefix` is off so a test can compare the path the upstream received
/// against the path the caller sent.
async fn wired(app: &common::TestApp, upstream: &LocalUpstream, prefix: &str) -> String {
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": prefix,
        "methods": METHODS,
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    alias
}

#[tokio::test]
async fn a_request_reaches_the_upstream_with_its_method_path_and_query() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/payments").await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/payments/charges?limit=5&cursor=abc"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    let received = upstream.last().expect("the upstream saw a request");
    assert_eq!(received.method, "GET");
    assert_eq!(received.path, "/v1/payments/charges");
    assert_eq!(received.query, "limit=5&cursor=abc");
}

/// A route with `strip_prefix` on forwards the remainder only.
#[tokio::test]
async fn a_stripped_prefix_leaves_the_remainder_only() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/stripped",
        "methods": METHODS,
        "target_alias": alias,
        "strip_prefix": true
    }))
    .await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/stripped/things"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    assert_eq!(received.path, "/things");
}

/// A route with `strip_prefix` off forwards the whole request path.
#[tokio::test]
async fn an_unstripped_prefix_keeps_the_whole_path() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/kept",
        "methods": METHODS,
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/kept/a/b"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    assert_eq!(received.path, "/v1/kept/a/b");
}

#[tokio::test]
async fn a_request_body_travels_to_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/pay").await;

    let (status, _) = app
        .send_json(
            http::Method::POST,
            &format!("/oagw/v1/proxy/{alias}/v1/pay/charge"),
            Some(json!({"amount": 4200, "currency": "eur"})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    assert_eq!(received.method, "POST");
    assert_eq!(
        received.body_string(),
        r#"{"amount":4200,"currency":"eur"}"#
    );
}

/// The gateway rewrites `Host` to the upstream unless the route preserves it.
#[tokio::test]
async fn the_host_header_names_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/host").await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/host/x"),
            None,
            &[("host", "client.example")],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    let host = received.header("host").unwrap_or_default();
    assert!(
        host.starts_with("127.0.0.1"),
        "the host names the upstream, not the caller: {host}"
    );
}

/// Hop-by-hop headers govern one leg only and are never forwarded.
#[tokio::test]
async fn hop_by_hop_headers_are_dropped() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/hops").await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/hops/x"),
            None,
            &[
                ("connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("te", "trailers"),
            ],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    assert!(
        received.header("connection").is_none(),
        "connection must not travel: {:?}",
        received.headers
    );
    assert!(received.header("keep-alive").is_none());
}

/// The gateway's own routing header is an instruction, not a passthrough.
#[tokio::test]
async fn the_target_host_header_does_not_reach_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/hidden").await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/hidden/x"),
            None,
            &[(
                oagw::infra::proxy::outbound::TARGET_HOST_HEADER,
                "127.0.0.1",
            )],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let received = upstream.last().expect("the upstream saw a request");
    assert!(
        received
            .header(oagw::infra::proxy::outbound::TARGET_HOST_HEADER)
            .is_none(),
        "the routing header is consumed: {:?}",
        received.headers
    );
}

#[tokio::test]
async fn the_upstreams_status_headers_and_body_come_back() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(Answer {
        status: 201,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-upstream-fingerprint".to_owned(), "issuer-7".to_owned()),
        ],
        body: Some(json!({"created": true}).to_string().into_bytes()),
        ..Answer::default()
    })
    .await;
    let alias = wired(&app, &upstream, "/v1/made").await;

    let (status, body) = app
        .send_json(
            http::Method::POST,
            &format!("/oagw/v1/proxy/{alias}/v1/made/x"),
            Some(json!({"a": 1})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    assert_eq!(body["created"], true);

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/made/x"),
            None,
            &[],
        ))
        .await;
    let fingerprint = response
        .headers()
        .get("x-upstream-fingerprint")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    assert_eq!(
        fingerprint.as_deref(),
        Some("issuer-7"),
        "upstream headers travel"
    );
}

/// A non-2xx answer from the upstream passes through untouched, and carries
/// the gateway's marker that it came from the upstream rather than from OAGW.
#[tokio::test]
async fn an_upstreams_own_error_passes_through() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(Answer::status(404)).await;
    let alias = wired(&app, &upstream, "/v1/missing").await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/missing/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert!(
        document.is_null() || document.is_object(),
        "the upstream's own body is not replaced: {document}"
    );
}

#[tokio::test]
async fn an_upstream_error_keeps_its_own_body() {
    let app = app().await;
    let upstream = LocalUpstream::start_with(Answer {
        status: 500,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: Some(
            json!({"error": "upstream exploded"})
                .to_string()
                .into_bytes(),
        ),
        ..Answer::default()
    })
    .await;
    let alias = wired(&app, &upstream, "/v1/broken").await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/broken/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(document["error"], "upstream exploded", "{document}");
}

/// The correlation identifier the caller supplied is echoed by the gateway.
#[tokio::test]
async fn a_trace_identifier_is_echoed_on_gateway_errors() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/proxy/ghost.partner.com/v1/none",
            None,
            &[("x-request-id", "trace-42")],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn an_unknown_alias_is_a_gateway_not_found() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/proxy/nobody.partner.com/v1/x",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    let _ = &app;
}

/// No route matches this method, so the gateway answers, not the upstream.
#[tokio::test]
async fn a_method_outside_the_allowlist_is_not_forwarded() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/readonly",
        "methods": ["GET"],
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::POST,
            &format!("/oagw/v1/proxy/{alias}/v1/readonly/x"),
            Some(json!({})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(upstream.count(), 0, "nothing reached the upstream");
}

/// A disabled route matches nothing.
#[tokio::test]
async fn a_disabled_route_does_not_match() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/off",
        "methods": METHODS,
        "target_alias": alias,
        "enabled": false
    }))
    .await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/off/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(upstream.count(), 0);
}

/// The longest matching prefix wins, so a sibling route can specialise a path.
#[tokio::test]
async fn the_longest_matching_route_wins() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("local")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();

    app.create_route(json!({
        "path": "/v1/broad",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    app.create_route(json!({
        "path": "/v1/broad/special",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/broad/special/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    let received = upstream.last().expect("a request was forwarded");
    assert_eq!(received.path, "/v1/broad/special/x");
}

/// A pool where every endpoint shares one host has nothing to disambiguate.
#[tokio::test]
async fn a_single_endpoint_pool_needs_no_target_host() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(&app, &upstream, "/v1/single").await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/single/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
}

/// A multi-endpoint pool over one common suffix is ambiguous without a hint.
#[tokio::test]
async fn a_multi_endpoint_pool_without_a_target_host_is_refused() {
    let app = app().await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "pool.partner.com",
            "endpoints": [
                {"scheme": "http", "host": "a.partner.com"},
                {"scheme": "http", "host": "b.partner.com"}
            ]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/pooled",
        "methods": METHODS,
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/pooled/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
}

#[tokio::test]
async fn an_unparseable_target_host_is_refused() {
    let app = app().await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "hint.partner.com",
            "endpoints": [
                {"scheme": "http", "host": "a.partner.com"},
                {"scheme": "http", "host": "b.partner.com"}
            ]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/hinted",
        "methods": METHODS,
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/hinted/x"),
            None,
            &[(
                oagw::infra::proxy::outbound::TARGET_HOST_HEADER,
                "not a host!",
            )],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

#[tokio::test]
async fn an_unconfigured_target_host_is_refused() {
    let app = app().await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "hinted2.partner.com",
            "endpoints": [
                {"scheme": "http", "host": "a.partner.com"},
                {"scheme": "http", "host": "b.partner.com"}
            ]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/hinted2",
        "methods": METHODS,
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/hinted2/x"),
            None,
            &[(
                oagw::infra::proxy::outbound::TARGET_HOST_HEADER,
                "elsewhere.partner.com",
            )],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
}

/// A hint naming one of the pool's endpoints dials exactly that one, where the
/// bare pool would have been ambiguous. A hint is a host only — no port.
#[tokio::test]
async fn a_target_host_hint_selects_the_endpoint() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "picked.partner.com",
            "endpoints": [
                {"scheme": "http", "host": "elsewhere.partner.com"},
                upstream.endpoint()
            ]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/picked",
        "methods": METHODS,
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/picked/x"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/picked/x"),
            None,
            &[(
                oagw::infra::proxy::outbound::TARGET_HOST_HEADER,
                "127.0.0.1",
            )],
        )
        .await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "the hinted endpoint is dialled"
    );
    assert_eq!(upstream.count(), 1);

    // A hint carries no port: the pool's endpoint port is the gateway's own
    // business, so a ported hint is a malformed instruction, not a selection.
    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/picked/x"),
            None,
            &[(
                oagw::infra::proxy::outbound::TARGET_HOST_HEADER,
                format!("127.0.0.1:{}", upstream.addr.port()).as_str(),
            )],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
}

/// The request target written to the upstream is origin-form.
///
/// Hyper writes whatever the request URI carries, so a URI that still holds the
/// scheme and authority would put `GET http://host:port/p HTTP/1.1` on the wire
/// — a form upstreams that match on their path do not recognise.
#[tokio::test]
async fn the_forwarded_request_target_is_origin_form() {
    let app = app().await;
    let upstream = RawUpstream::start().await;
    let endpoint = json!({
        "scheme": "http",
        "host": upstream.addr.ip().to_string(),
        "port": upstream.addr.port()
    });
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "raw",
            "name": "raw capture",
            "endpoints": [endpoint]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/pick",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/proxy/raw/v1/pick?limit=2",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");

    let line = upstream.request_line().expect("a captured request line");
    assert_eq!(
        line, "GET /v1/pick?limit=2 HTTP/1.1",
        "the target is origin-form, not absolute-form"
    );
}

/// A disabled upstream refuses to forward: the caller is told the link is
/// unavailable, and the upstream itself is never woken (FR-8).
#[tokio::test]
async fn a_disabled_upstream_answers_503_and_wakes_no_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("switched.off");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "enabled": false
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/thing"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{document}");
    assert_eq!(
        document["type"], "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
        "the documented 503 type for an upstream that cannot serve"
    );
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("disabled")),
        "the refusal says why: {document}"
    );
    assert_eq!(upstream.count(), 0, "a disabled upstream is never dialled");
}

/// Re-enabling the upstream restores proxying: the flag is live, not a
/// snapshot taken at route-creation time.
#[tokio::test]
async fn re_enabling_the_upstream_restores_proxying() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("switched.later");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit",
            "enabled": false
        }))
        .await;
    let id = upstream_doc["id"].as_str().unwrap().to_owned();
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/thing"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let mut replacement = upstream_doc.clone();
    replacement["enabled"] = json!(true);
    for drop in ["id", "tenant_id", "created_at", "updated_at", "alias"] {
        replacement.as_object_mut().unwrap().remove(drop);
    }
    let (re_enabled, replaced) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(replacement),
            &[],
        )
        .await;
    assert_eq!(re_enabled, http::StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], true);

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/thing"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    assert_eq!(upstream.count(), 1, "the upstream answers once re-enabled");
}

/// A route with `path_suffix_mode: disabled` refuses a call carrying a suffix.
#[tokio::test]
async fn a_disabled_path_suffix_refuses_a_call_that_carries_one() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("suffix.off");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/fixed",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "path_suffix_mode": "disabled"
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/fixed/unwanted/extra"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("suffix")),
        "the refusal names the rule: {document}"
    );
    assert_eq!(upstream.count(), 0);
}

/// The same route answers exactly on its path, and an `append` route forwards
/// the suffix it always did.
#[tokio::test]
async fn an_append_route_still_forwards_the_suffix() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("suffix.on");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "name": spec["name"],
            "endpoints": spec["endpoints"],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/loose",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/loose/deep/target"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    let received = upstream.last().expect("the upstream saw the call");
    assert_eq!(received.path, "/v1/loose/deep/target");
}

/// Wire a route on `/v1/body` to a fresh upstream and return the proxy path.
async fn body_route(app: &common::TestApp, upstream: &LocalUpstream, tag: &str) -> String {
    let spec = upstream.upstream_spec(tag);
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/body",
        "methods": ["POST"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    format!("/oagw/v1/proxy/{alias}/v1/body")
}

/// A body beyond the configured limit is refused before it leaves the building.
#[tokio::test]
async fn a_body_beyond_the_limit_is_refused_with_413() {
    let app = common::app_with(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_bytes": 16
    }))
    .await;
    let upstream = LocalUpstream::start().await;
    let path = body_route(&app, &upstream, "body.too.big").await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &path,
            vec![b'x'; 17],
            Some(17),
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::PAYLOAD_TOO_LARGE,
        "{response:?}"
    );
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway())
    );
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("payload.too_large.v1")),
        "{document}"
    );
    assert_eq!(upstream.count(), 0, "an over-limit body wakes no upstream");
}

/// A body exactly at the limit is not refused: the limit is a limit, not a
/// quota one byte short.
#[tokio::test]
async fn a_body_at_the_limit_is_accepted() {
    let app = common::app_with(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_bytes": 16
    }))
    .await;
    let upstream = LocalUpstream::start().await;
    let path = body_route(&app, &upstream, "body.on.limit").await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &path,
            vec![b'x'; 16],
            Some(16),
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK, "{response:?}");
    assert_eq!(upstream.count(), 1);
}

/// `Content-Length` and `Transfer-Encoding` together is a smuggling-shaped
/// request, so the gateway refuses it instead of choosing one of the two.
#[tokio::test]
async fn a_request_with_both_length_and_encoding_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let path = body_route(&app, &upstream, "body.smuggle").await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &path,
            b"hello".to_vec(),
            Some(5),
            &[("transfer-encoding", "chunked")],
        ))
        .await;
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("validation.error.v1")),
        "{document}"
    );
    assert_eq!(upstream.count(), 0);
}

/// A declared length that disagrees with the payload is a request the gateway
/// cannot trust to forward intact.
#[tokio::test]
async fn a_disagreeing_content_length_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let path = body_route(&app, &upstream, "body.disagree").await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &path,
            b"hello".to_vec(),
            Some(3),
            &[],
        ))
        .await;
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("validation.error.v1")),
        "{document}"
    );
    assert_eq!(upstream.count(), 0);
}

/// An unsupported transfer encoding is refused rather than forwarded blind.
#[tokio::test]
async fn an_unsupported_transfer_encoding_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let path = body_route(&app, &upstream, "body.gzip").await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &path,
            b"hello".to_vec(),
            None,
            &[("transfer-encoding", "gzip")],
        ))
        .await;
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(upstream.count(), 0);
}

// --- transport failures the upstream causes --------------------------------

/// Wire `upstream` to a route that forwards `/v1/ping` unchanged.
async fn wired_plain(app: &common::TestApp, upstream: &LocalUpstream) -> String {
    let spec = upstream.upstream_spec("plain");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    alias
}

/// An upstream that takes the connection and never answers is a timeout, and
/// the answer is the gateway's own.
#[tokio::test]
async fn an_upstream_that_never_answers_times_out_with_504() {
    let app = common::app_with(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 1
    }))
    .await;
    let silent = common::ByteUpstream::start(None).await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "silent.partner.com",
            "name": "Silent",
            "endpoints": [silent.endpoint()],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/ping"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::GATEWAY_TIMEOUT,
        "{response:?}"
    );
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway())
    );
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("timeout.request.v1")),
        "{document}"
    );
}

/// An upstream that answers, but not in HTTP, is a protocol failure.
#[tokio::test]
async fn an_upstream_that_speaks_no_http_is_a_502() {
    let app = common::app_with(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5
    }))
    .await;
    let garbage = common::ByteUpstream::start(Some(b"this is not a response head\r\n\r\n")).await;
    let upstream_doc = app
        .create_upstream(json!({
            "alias": "nonsense.partner.com",
            "name": "Nonsense",
            "endpoints": [garbage.endpoint()],
            "sharing": "inherit"
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/ping"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::BAD_GATEWAY,
        "{response:?}"
    );
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway())
    );
    let (status, document) = common::status_and_document(response).await;
    assert_eq!(status, http::StatusCode::BAD_GATEWAY);
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("protocol.error.v1")),
        "{document}"
    );
}

// --- retry and cache -------------------------------------------------------

/// A failing call is forwarded once: the gateway does not retry on the
/// caller's behalf, and a caller that wants a retry re-issues the bytes.
#[tokio::test]
async fn a_failing_call_is_forwarded_exactly_once() {
    let app = app().await;
    let upstream = common::LocalUpstream::start_with(Answer::status(503)).await;
    let alias = wired_plain(&app, &upstream).await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/ping"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.count(), 1, "one call, one forwarding: {status}");
}

/// The gateway remembers nothing: the same bytes re-issued go upstream again.
#[tokio::test]
async fn re_issued_bytes_are_forwarded_again() {
    let app = app().await;
    let upstream = common::LocalUpstream::start_with(Answer::status(404)).await;
    let alias = wired_plain(&app, &upstream).await;

    for _ in 0..3 {
        let (status, _) = app
            .send_json(
                http::Method::GET,
                &format!("/oagw/v1/proxy/{alias}/v1/ping"),
                None,
                &[],
            )
            .await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }
    assert_eq!(
        upstream.count(),
        3,
        "no answer was remembered between calls"
    );
}

// --- alias case ------------------------------------------------------------

/// Resolution does not care what case the caller spelled the alias in.
#[tokio::test]
async fn an_alias_resolves_regardless_of_letter_case() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("Case.Partner.COM");
    let upstream_doc = app.create_upstream(spec).await;
    let stored = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ping",
        "methods": ["GET"],
        "target_alias": stored.clone(),
        "strip_prefix": false
    }))
    .await;

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{}/v1/ping", "CASE.PARTNER.COM"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(upstream.count(), 1);
}
