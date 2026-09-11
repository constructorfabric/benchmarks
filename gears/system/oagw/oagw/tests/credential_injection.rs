//! Credentials travel to the upstream and nowhere else.
//!
//! An upstream that needs a key declares a `secret_ref`; the gateway resolves
//! it against the credential store per request and injects it into the outbound
//! request. These tests hold the answer under the caller's hands: the key must
//! arrive on the upstream's own connection, and it must not appear in any
//! management document, error document or log the gateway produces.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;

/// The value the harness's mock credential store holds for `stripe-key`.
const SECRET: &str = "sk_test_123";
/// A key the credential store does not hold.
const MISSING_REF: &str = "cred://no-such-key";
const SECRET_REF: &str = "cred://stripe-key";

/// An upstream holding one `api_key` auth method, wired to a route.
async fn wired_api_key(app: &common::TestApp, upstream: &LocalUpstream, header: &str) -> String {
    let spec = upstream.upstream_spec("keyed");
    let spec = json!({
        "alias": spec["alias"],
        "name": spec["name"],
        "endpoints": spec["endpoints"],
        "sharing": "inherit",
        "auth_methods": [{
            "type": "api_key",
            "secret_ref": SECRET_REF,
            "header_name": header
        }]
    });
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/charges",
        "methods": ["GET", "POST"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;
    alias
}

#[tokio::test]
async fn the_api_key_is_injected_on_every_proxied_call() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_api_key(&app, &upstream, "x-api-key").await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);

    let received = upstream.last().expect("the upstream was called");
    assert_eq!(
        received.header("x-api-key"),
        Some(SECRET),
        "the resolved key is injected under the configured name"
    );
}

/// The default header is `Authorization`, the way the data model states it.
#[tokio::test]
async fn the_key_lands_in_authorization_by_default() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("defaulted");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "endpoints": spec["endpoints"],
            "auth_methods": [{
                "type": "api_key",
                "secret_ref": SECRET_REF
            }]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/charges",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("authorization"), Some(SECRET));
}

/// A second call injects the key again: the resolution is per request.
#[tokio::test]
async fn the_key_is_injected_on_the_second_call_too() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_api_key(&app, &upstream, "x-api-key").await;
    let path = format!("/oagw/v1/proxy/{alias}/v1/charges");

    for _ in 0..2 {
        let response = app
            .send(app.request(http::Method::GET, &path, None, &[]))
            .await;
        assert_eq!(response.status(), http::StatusCode::OK);
    }
    assert_eq!(upstream.count(), 2);
    for received in upstream.received() {
        assert_eq!(received.header("x-api-key"), Some(SECRET));
    }
}

/// A key the caller supplied is replaced, not appended.
#[tokio::test]
async fn an_injected_key_replaces_what_the_caller_sent() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_api_key(&app, &upstream, "x-api-key").await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[("x-api-key", "attacker-supplied")],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(
        received.header("x-api-key"),
        Some(SECRET),
        "the caller's own key does not survive"
    );
    assert_eq!(received.headers_all("x-api-key").len(), 1);
}

/// A missing secret fails the call at the gateway, and no upstream is touched.
#[tokio::test]
async fn a_missing_secret_refuses_the_call_without_reaching_the_upstream() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("missing");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "endpoints": spec["endpoints"],
            "auth_methods": [{
                "type": "api_key",
                "secret_ref": MISSING_REF,
                "header_name": "x-api-key"
            }]
        }))
        .await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/charges",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response
            .headers()
            .get(common::error_source_header())
            .and_then(|value| value.to_str().ok()),
        Some(common::error_source_gateway()),
        "a missing secret is the gateway's own failure"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("secret.not_found.v1")),
        "{document}"
    );
    assert!(
        upstream.count() == 0,
        "the request never leaves the gateway"
    );
}

/// An upstream without auth methods injects nothing.
#[tokio::test]
async fn an_upstream_without_auth_methods_sends_no_credentials() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let upstream_doc = app.create_upstream(upstream.upstream_spec("plain")).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/open",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false
    }))
    .await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/open"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
    let received = upstream.last().expect("the upstream was called");
    assert_eq!(received.header("authorization"), None);
    assert_eq!(received.header("x-api-key"), None);
}

/// The reference the store sees has no scheme on it.
#[tokio::test]
async fn the_cred_scheme_is_stripped_before_the_lookup() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_api_key(&app, &upstream, "x-api-key").await;
    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK, "the key resolved");
}

#[tokio::test]
async fn no_management_document_carries_the_secret() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("documented");
    let upstream_doc = app
        .create_upstream(json!({
            "alias": spec["alias"],
            "endpoints": spec["endpoints"],
            "auth_methods": [{
                "type": "api_key",
                "secret_ref": SECRET_REF,
                "header_name": "x-api-key"
            }]
        }))
        .await;

    let rendered = serde_json::to_string(&upstream_doc).expect("upstream renders");
    assert!(
        !rendered.contains(SECRET),
        "an upstream document never carries the key: {rendered}"
    );
    assert!(
        rendered.contains("stripe-key"),
        "the document carries the reference the operator wrote"
    );

    let (status, listing) = app
        .send_json(http::Method::GET, "/oagw/v1/upstreams", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK);
    let rendered = serde_json::to_string(&listing).expect("list renders");
    assert!(!rendered.contains(SECRET), "no listing carries the key");
}

/// The secret does not reach the caller through a proxy response either.
#[tokio::test]
async fn the_secret_is_stripped_from_what_the_caller_sees() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let alias = wired_api_key(&app, &upstream, "x-api-key").await;

    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        ))
        .await;
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert!(
        !String::from_utf8_lossy(&bytes).contains(SECRET),
        "the response echoes nothing it injected"
    );
}

/// A binding that names no reference is refused at configuration time.
#[tokio::test]
async fn an_api_key_without_a_secret_ref_is_rejected() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("incomplete");
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": spec["alias"],
                "endpoints": spec["endpoints"],
                "auth_methods": [{"type": "api_key", "header_name": "x-api-key"}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
    assert!(
        document["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("secret_ref")),
        "the refusal names the missing field: {document}"
    );
}

/// A route-bound `apikey` plugin injects exactly as an upstream auth method
/// does: the plugin declares what it needs, the data plane resolves it.
#[tokio::test]
async fn a_route_bound_apikey_plugin_injects_the_resolved_key() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("plugged");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/charges",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "plugins": [{
            "plugin_id": "apikey",
            "config": {"secret_ref": SECRET_REF, "header_name": "x-partner-key"}
        }]
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{body}");
    let received = upstream.last().expect("the upstream saw the call");
    assert_eq!(
        received.header("x-partner-key"),
        Some(SECRET),
        "the resolved key reached the upstream"
    );
}

/// A route-bound plugin whose reference names nothing aborts the exchange with
/// the documented 500, and the upstream is never woken for it.
#[tokio::test]
async fn a_route_bound_plugin_with_a_missing_secret_refuses_the_call() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("plugged");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/charges",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "plugins": [{
            "plugin_id": "apikey",
            "config": {"secret_ref": MISSING_REF, "header_name": "x-partner-key"}
        }]
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/charges"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
    assert_eq!(upstream.count(), 0, "no call without its credential");
}

/// The same refusal on a WebSocket dial: the head is the only message the
/// upstream sees, so a missing credential stops the upgrade there.
#[tokio::test]
async fn a_ws_dial_with_a_missing_secret_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("plugged.ws");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/ws",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "plugins": [{
            "plugin_id": "apikey",
            "config": {"secret_ref": MISSING_REF, "header_name": "x-partner-key"}
        }]
    }))
    .await;

    let (status, body) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/ws"),
            None,
            &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ],
        )
        .await;
    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
}
