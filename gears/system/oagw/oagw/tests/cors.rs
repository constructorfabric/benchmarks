//! AT-8: CORS (`contracts/proxy-api.md` § 3, `DESIGN.md` § 3.2).
//!
//! A preflight is answered permissively at the boundary without resolving the
//! alias; an actual request carrying a disallowed origin or method is rejected
//! with `403` before the upstream is dialled.

mod common;

use axum::http::StatusCode;
use common::net::{bind_upstream, read_request, write_response};
use common::{Caller, app, create_route, create_upstream, route_body, send};
use tower::ServiceExt;

const PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// A route on `path` admitting `GET` and `DELETE`.
fn wide_route(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET", "DELETE"], "path": path,
            "query_allowlist": [], "path_suffix_mode": "append"
        }}
    })
    .to_string()
}

/// An upstream with CORS enabled for `https://app.example`.
fn cors_upstream(alias: &str, port: u16) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": port }
        ]},
        "protocol": common::PROTOCOL_HTTP,
        "cors": {
            "enabled": true,
            "allowed_origins": ["https://app.example"],
            "allowed_methods": ["GET", "POST"],
            "allow_credentials": true,
            "expose_headers": ["x-upstream"]
        }
    })
    .to_string()
}

/// Sends a request with caller-supplied headers.
async fn send_with(
    app: &axum::Router,
    caller: &Caller,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", "Bearer test");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(axum::body::Body::empty()).expect("request");
    request.extensions_mut().insert(caller.context());
    let response = app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (parts.status, parts.headers, json)
}

#[tokio::test]
async fn a_preflight_is_answered_permissively_without_an_upstream() {
    let app = app();
    let caller = Caller::default();
    // No upstream exists at all: the preflight never touches the control plane.
    let path = "/oagw/v1/proxy/no-such-alias/v1/chat";

    let (status, headers, body) = send_with(
        &app,
        &caller,
        "OPTIONS",
        path,
        &[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-tenant,content-type"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|v| v.to_str().ok()),
        Some("x-tenant,content-type")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|v| v.to_str().ok()),
        Some("600")
    );
}

#[tokio::test]
async fn an_allowed_origin_and_method_reach_the_upstream_with_cors_headers() {
    let (addr, listener) = bind_upstream().await;
    let alias = format!("cors-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &cors_upstream(&alias, addr.port())).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        write_response(
            &mut stream,
            "HTTP/1.1 200 OK",
            &[("x-upstream", "yes")],
            b"{}",
        )
        .await;
    });

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, headers, body) = send_with(
        &app,
        &caller,
        "GET",
        &path,
        &[("origin", "https://app.example")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        headers
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    // The header the upstream configuration exposes is named for the browser.
    assert_eq!(
        headers
            .get("access-control-expose-headers")
            .and_then(|v| v.to_str().ok()),
        Some("x-upstream")
    );
    server.await.expect("upstream task");
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_with_403() {
    let addr = common::free_port().await;
    let alias = format!("strict-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &cors_upstream(&alias, addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, headers, body) = send_with(
        &app,
        &caller,
        "GET",
        &path,
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.cors.origin_not_allowed.v1")
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn a_method_outside_the_cors_allowlist_is_rejected_with_403() {
    let addr = common::free_port().await;
    let alias = format!("methods-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &cors_upstream(&alias, addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &wide_route(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, _, body) = send_with(
        &app,
        &caller,
        "DELETE",
        &path,
        &[("origin", "https://app.example")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        body["type"],
        format!("{PREFIX}cf.oagw.cors.method_not_allowed.v1")
    );
}

#[tokio::test]
async fn a_request_without_an_origin_is_not_subject_to_cors() {
    let addr = common::free_port().await;
    let alias = format!("plain-{addr}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &cors_upstream(&alias, addr)).await;
    let id = upstream["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &wide_route(&id, "/v1")).await;

    let path = format!("/oagw/v1/proxy/{alias}/v1/chat");
    // No `Origin`: the CORS policy does not apply, so the request proceeds to
    // the dial and fails there instead.
    let (status, headers, _) = send_with(&app, &caller, "DELETE", &path, &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(headers.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn a_wildcard_origin_with_credentials_is_refused_on_submission() {
    let app = app();
    let caller = Caller::default();

    let (status, _, problem) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/upstreams",
        Some(
            &serde_json::json!({
                "enabled": true,
                "alias": "credentialed.local",
                "server": { "endpoints": [
                    { "scheme": "http", "host": "127.0.0.1", "port": 8443 }
                ]},
                "protocol": common::PROTOCOL_HTTP,
                "cors": {
                    "enabled": true,
                    "allowed_origins": ["*"],
                    "allowed_methods": ["GET"],
                    "allow_credentials": true
                }
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        format!("{PREFIX}cf.oagw.validation.error.v1")
    );
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("wildcard origin"),
        "{problem}"
    );
}
