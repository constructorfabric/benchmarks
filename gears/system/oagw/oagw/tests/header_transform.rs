//! Header transformation happens where the gateway owns the message.
//!
//! A route's `request_headers` and `response_headers` rewrite what the two
//! sides see. These tests hold each action under its own light — `set`
//! overwrites, `replace` only touches what is already there, `remove` takes a
//! header out — and check that a value pulled from the credential store reaches
//! the upstream without ever reaching the caller.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;

const SECRET: &str = "tok_internal";
const SECRET_REF: &str = "cred://internal-token";

/// Wire `upstream` to a route carrying `request_headers` and `response_headers`.
async fn wired(
    app: &common::TestApp,
    upstream: &LocalUpstream,
    request_headers: serde_json::Value,
    response_headers: serde_json::Value,
) -> String {
    let spec = upstream.upstream_spec("transformed");
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
        "path": "/v1/echo",
        "methods": ["GET", "POST"],
        "target_alias": alias,
        "strip_prefix": false,
        "request_headers": request_headers,
        "response_headers": response_headers
    }))
    .await;
    alias
}

/// Proxy `path` with `headers` and return the response.
async fn proxy(
    app: &common::TestApp,
    alias: &str,
    headers: &[(&str, &str)],
) -> http::Response<axum::body::Body> {
    app.send(app.request(
        http::Method::GET,
        &format!("/oagw/v1/proxy/{alias}/v1/echo"),
        None,
        headers,
    ))
    .await
}

#[tokio::test]
async fn a_set_overwrites_what_the_caller_sent() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-trace", "action": "set", "value": "gateway"}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[("x-trace", "caller-value")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-trace"), Some("gateway"));
    assert_eq!(received.headers_all("x-trace").len(), 1);
}

/// `replace` leaves an absent header absent.
#[tokio::test]
async fn a_replace_does_not_invent_a_header() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-absent", "action": "replace", "value": "gateway"}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert!(
        received.header("x-absent").is_none(),
        "a replace with nothing to replace does nothing"
    );
}

/// `replace` rewrites a header the caller did bring.
#[tokio::test]
async fn a_replace_rewrites_a_present_header() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-tenant", "action": "replace", "value": "rewritten"}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[("x-tenant", "original")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-tenant"), Some("rewritten"));
}

#[tokio::test]
async fn a_remove_takes_the_header_out() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-internal-mark", "action": "remove"}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[("x-internal-mark", "drop-me")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-internal-mark"), None);
}

/// A `value_ref` resolves through the credential store at request time.
#[tokio::test]
async fn a_value_ref_is_resolved_for_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-auth-token", "action": "set", "value_ref": SECRET_REF}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-auth-token"), Some(SECRET));
}

/// The resolved value is an outbound concern: the caller sees none of it.
#[tokio::test]
async fn a_resolved_value_is_not_echoed_back() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-auth-token", "action": "set", "value_ref": SECRET_REF}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[]).await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert!(
        !String::from_utf8_lossy(&bytes).contains(SECRET),
        "the proxied answer carries nothing the gateway resolved"
    );
}

/// The response side is transformed too.
#[tokio::test]
async fn a_response_header_is_transformed() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([]),
        json!([{"name": "x-server-note", "action": "set", "value": "oagw"}]),
    )
    .await;

    let response = proxy(&app, &alias, &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-server-note")
            .and_then(|value| value.to_str().ok()),
        Some("oagw")
    );
}

/// A response header the caller must not see is dropped.
#[tokio::test]
async fn a_response_header_can_be_removed() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([]),
        json!([{"name": "x-upstream-build", "action": "remove"}]),
    )
    .await;

    let response = proxy(&app, &alias, &[]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(
        response.headers().get("x-upstream-build").is_none(),
        "the upstream's header does not reach the caller"
    );
}

/// The default stripping still holds when transforms are configured.
#[tokio::test]
async fn hop_by_hop_headers_are_still_stripped() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired(
        &app,
        &upstream,
        json!([{"name": "x-added", "action": "set", "value": "1"}]),
        json!([]),
    )
    .await;

    let response = proxy(&app, &alias, &[("connection", "close")]).await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("connection"), None);
    assert_eq!(received.header("x-added"), Some("1"));
}

/// A transform that names a header the gateway cannot parse is refused.
#[tokio::test]
async fn an_unparseable_header_name_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("invalid");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/echo",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "request_headers": [{"name": "bad header name", "action": "set", "value": "1"}]
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert!(
        document["detail"].is_string(),
        "the refusal explains itself: {document}"
    );
}

// --- passthrough -----------------------------------------------------------

/// Wire `upstream` to a route whose header rules are `request_headers`, served
/// under the passthrough posture `passthrough` with `allowlist`.
async fn wired_with_passthrough(
    app: &common::TestApp,
    upstream: &LocalUpstream,
    passthrough: serde_json::Value,
    allowlist: serde_json::Value,
) -> String {
    let spec = upstream.upstream_spec("passthrough");
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
        "path": "/v1/echo",
        "methods": ["GET", "POST"],
        "target_alias": alias,
        "strip_prefix": false,
        "passthrough": passthrough,
        "passthrough_allowlist": allowlist
    }))
    .await;
    alias
}

/// With the default `none` the caller's own headers are the caller's business:
/// the upstream sees the request, and none of what the caller brought with it.
#[tokio::test]
async fn the_default_passthrough_forwards_no_inbound_header() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_with_passthrough(&app, &upstream, json!("none"), json!([])).await;

    let response = proxy(
        &app,
        &alias,
        &[("x-caller-only", "secret"), ("accept", "text/plain")],
    )
    .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert!(
        received.header("x-caller-only").is_none(),
        "an unforwarded header does not reach the upstream: {received:?}"
    );
    assert!(
        received.header("accept").is_none(),
        "`none` forwards no inbound header: {received:?}"
    );
}

/// The route's create and read documents carry the setting, whose default is
/// the schema's `none`.
#[tokio::test]
async fn passthrough_round_trips_and_defaults_to_none() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("round.trip");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();

    let (status, defaulted) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "path": "/v1/echo",
                "methods": ["GET"],
                "target_alias": alias,
                "strip_prefix": false
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{defaulted}");
    assert_eq!(defaulted["passthrough"], "none", "the schema's default");
    assert_eq!(defaulted["passthrough_allowlist"], json!([]));
    let id = defaulted["id"].as_str().unwrap().to_owned();

    let (status, configured) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/routes/{id}"),
            Some(json!({
                "path": "/v1/echo",
                "methods": ["GET"],
                "target_alias": alias,
                "strip_prefix": false,
                "passthrough": "allowlist",
                "passthrough_allowlist": ["x-token"]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{configured}");
    assert_eq!(configured["passthrough"], "allowlist");
    assert_eq!(configured["passthrough_allowlist"], json!(["x-token"]));
}

/// An allowlist names exactly the inbound headers the upstream may see.
#[tokio::test]
async fn an_allowlist_forwards_only_the_headers_it_names() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_with_passthrough(
        &app,
        &upstream,
        json!("allowlist"),
        json!(["x-token", "accept"]),
    )
    .await;

    let response = proxy(
        &app,
        &alias,
        &[
            ("x-token", "let-through"),
            ("x-other", "held"),
            ("accept", "text/plain"),
        ],
    )
    .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-token"), Some("let-through"));
    assert_eq!(received.header("accept"), Some("text/plain"));
    assert!(
        received.header("x-other").is_none(),
        "a header off the allowlist stays behind: {received:?}"
    );
}

/// `all` forwards the caller's headers, still without the hop-by-hop set.
#[tokio::test]
async fn a_full_passthrough_forwards_everything_but_hop_by_hop() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_with_passthrough(&app, &upstream, json!("all"), json!([])).await;

    let response = proxy(
        &app,
        &alias,
        &[
            ("x-caller", "yes"),
            ("connection", "keep-alive"),
            ("te", "trailers"),
        ],
    )
    .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("x-caller"), Some("yes"));
    assert_eq!(received.header("connection"), None);
    assert_eq!(received.header("te"), None);
}

/// A body's own framing is the gateway's business, not something `none` may
/// withhold: the upstream can still read what the caller posted.
#[tokio::test]
async fn a_body_content_type_survives_a_closed_passthrough() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("body.passthrough");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/echo",
        "methods": ["POST"],
        "target_alias": alias,
        "strip_prefix": false,
        "passthrough": "none"
    }))
    .await;

    let response = app
        .send(common::raw_body_request(
            app.tenant,
            http::Method::POST,
            &format!("/oagw/v1/proxy/{alias}/v1/echo"),
            b"payload".to_vec(),
            None,
            &[("content-type", "application/json"), ("x-caller", "held")],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK, "{response:?}");
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("content-type"), Some("application/json"));
    assert_eq!(received.body, b"payload".to_vec());
    assert!(received.header("x-caller").is_none());
}
