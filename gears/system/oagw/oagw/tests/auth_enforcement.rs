//! Authentication and authorization hold on every route the gear serves.
//!
//! The platform installs its bearer-token middleware above the gear, so what
//! the gear can be reached without is a subject in the request extensions. These
//! tests leave it out and assert the refusal is the documented one; they then
//! check that a subject which *is* authenticated still sees nothing of another
//! tenant's configuration — a 404, never a 403, so the answer leaks no
//! existence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{app, subject};
use http_body_util::BodyExt;
use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};
use serde_json::json;
use toolkit_security::SecurityContext;

const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";
const PROXY_PERMISSION: &str = "gts.cf.core.oagw.proxy.v1~:invoke";
const UPSTREAM_READ_PERMISSION: &str = "gts.cf.core.oagw.upstream.v1~:read";

/// A subject restricted to `scopes`, the way a real token would be.
fn scoped(tenant: uuid::Uuid, scopes: &[&str]) -> SecurityContext {
    let mut builder = SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(tenant);
    builder = builder.token_scopes(scopes.iter().map(ToString::to_string).collect());
    builder.build().expect("valid security context")
}

/// Whether the document carries the gateway's problem type.
fn is_problem(document: &serde_json::Value, suffix: &str) -> bool {
    document["type"]
        .as_str()
        .is_some_and(|kind| kind.ends_with(suffix))
}

#[tokio::test]
async fn a_management_call_without_a_subject_is_refused() {
    let app = app().await;
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri("/oagw/v1/upstreams")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(json!({"endpoints": []}).to_string()))
        .unwrap();

    let response = app.send(request).await;
    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY),
        "the refusal is the gateway's own"
    );
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(APPLICATION_PROBLEM_JSON)
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert!(is_problem(&document, "auth.failed.v1"), "{document}");
    assert_eq!(document["status"], 401);
}

/// A read, a write and a delete are all refused without a subject.
#[tokio::test]
async fn every_management_verb_is_refused_without_a_subject() {
    let app = app().await;
    for (method, path) in [
        (http::Method::GET, "/oagw/v1/upstreams"),
        (http::Method::GET, "/oagw/v1/routes"),
        (
            http::Method::PUT,
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000",
        ),
        (
            http::Method::DELETE,
            "/oagw/v1/routes/00000000-0000-0000-0000-000000000000",
        ),
        (http::Method::GET, "/oagw/v1/plugins"),
    ] {
        let request = http::Request::builder()
            .method(method.clone())
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.send(request).await;
        assert_eq!(
            response.status(),
            http::StatusCode::UNAUTHORIZED,
            "{method} {path} is refused"
        );
    }
}

#[tokio::test]
async fn a_proxy_call_without_a_subject_is_refused() {
    let app = app().await;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri("/oagw/v1/proxy/nobody/x")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.send(request).await;
    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
}

#[tokio::test]
async fn a_subject_without_a_tenant_is_refused() {
    let app = app().await;
    let (status, document) = app
        .send_json_as(
            uuid::Uuid::nil(),
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    assert!(is_problem(&document, "auth.failed.v1"), "{document}");
}

/// An absent extension and a present-but-empty one are the same refusal.
#[tokio::test]
async fn a_nil_tenant_is_refused_on_the_proxy_path_too() {
    let app = app().await;
    let (status, document) = app
        .send_json_as(
            uuid::Uuid::nil(),
            http::Method::GET,
            "/oagw/v1/proxy/nobody/x",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::UNAUTHORIZED);
    assert!(is_problem(&document, "auth.failed.v1"), "{document}");
}

#[tokio::test]
async fn a_tenant_cannot_read_another_tenants_upstream_by_id() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "name": "secret",
            "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
        }))
        .await;
    let id = upstream["id"].as_str().unwrap().to_owned();

    let (status, document) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(
        status,
        http::StatusCode::NOT_FOUND,
        "never a 403: {document}"
    );
    assert!(is_problem(&document, "route.not_found.v1"), "{document}");
}

#[tokio::test]
async fn a_tenant_cannot_update_or_delete_another_tenants_upstream() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "name": "secret",
            "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
        }))
        .await;
    let id = upstream["id"].as_str().unwrap().to_owned();

    let (status, _) = app
        .send_json_as(
            app.foreign,
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "name": "renamed",
                "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "a write is a 404 too");

    let (status, _) = app
        .send_json_as(
            app.foreign,
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "a delete is a 404 too");
}

/// Proxying through a foreign alias is a plain 404: the alias does not exist
/// for the caller, and saying so leaks nothing.
#[tokio::test]
async fn a_tenant_cannot_proxy_through_another_tenants_alias() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "name": "secret",
            "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
        }))
        .await;
    let alias = upstream["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/anything",
        "methods": ["GET"],
        "target_alias": alias
    }))
    .await;

    let (status, document) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/anything"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{document}");
    assert!(is_problem(&document, "route.not_found.v1"), "{document}");
    assert_eq!(document["status"], 404, "the answer is a 404, never a 403");
}

#[tokio::test]
async fn another_tenants_configuration_is_invisible_in_the_list() {
    let app = app().await;
    app.create_upstream(json!({
        "name": "mine",
        "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
    }))
    .await;

    let (status, document) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(document["total_count"], 0, "{document}");
    assert_eq!(
        document["items"].as_array().map(Vec::len),
        Some(0),
        "the list carries nothing of the other tenant"
    );
}

/// A token that carries scopes is governed by them: the proxy path needs the
/// proxy permission, not merely a tenant.
#[tokio::test]
async fn a_scoped_token_without_the_proxy_permission_cannot_proxy() {
    let app = app().await;
    let subject = scoped(app.tenant, &[UPSTREAM_READ_PERMISSION]);
    let response = app
        .send(common::request_with_subject(
            subject,
            http::Method::GET,
            "/oagw/v1/proxy/nobody/x",
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    assert!(is_problem(&document, "auth.failed.v1"), "{document}");
    let detail = document["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(PROXY_PERMISSION),
        "the refusal names the permission it wants: {detail}"
    );
}

#[tokio::test]
async fn a_token_with_the_proxy_permission_may_proxy() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "name": "permitted",
            "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
        }))
        .await;
    let alias = upstream["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/ok",
        "methods": ["GET"],
        "target_alias": alias
    }))
    .await;

    // The alias is known but the upstream is not reachable from the test, so
    // the interesting assertion is that the permission check passed: the call
    // gets past authorization and fails later, at the transport, with a
    // gateway error that is not the permission refusal.
    let subject = scoped(app.tenant, &[PROXY_PERMISSION]);
    let response = app
        .send(common::request_with_subject(
            subject,
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/ok"),
            None,
            &[],
        ))
        .await;
    assert_ne!(
        response.status(),
        http::StatusCode::UNAUTHORIZED,
        "the permission held"
    );
}

/// Scopes the token does carry do not spill over into other resources.
#[tokio::test]
async fn a_proxy_scoped_token_cannot_manage_upstreams() {
    let app = app().await;
    let subject = scoped(app.tenant, &[PROXY_PERMISSION]);
    let response = app
        .send(common::request_with_subject(
            subject,
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "name": "nope",
                "endpoints": [{"scheme": "https", "host": "api.partner.com"}]
            })),
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("problem document");
    let detail = document["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(UPSTREAM_READ_PERMISSION) || detail.contains("create"),
        "the refusal names the management permission: {detail}"
    );
}

#[tokio::test]
async fn a_token_carrying_the_management_permission_may_read_upstreams() {
    let app = app().await;
    let subject = scoped(app.tenant, &[UPSTREAM_READ_PERMISSION]);
    let response = app
        .send(common::request_with_subject(
            subject,
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "the read permission held"
    );
}

/// The platform's first-party tokens carry the wildcard scope.
#[tokio::test]
async fn a_wildcard_scope_grants_everything() {
    let app = app().await;
    let subject = scoped(app.tenant, &["*"]);
    let read = app
        .send(common::request_with_subject(
            subject.clone(),
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        ))
        .await;
    assert_eq!(read.status(), http::StatusCode::OK);

    let proxied = app
        .send(common::request_with_subject(
            subject,
            http::Method::GET,
            "/oagw/v1/proxy/nobody/x",
            None,
            &[],
        ))
        .await;
    assert_ne!(
        proxied.status(),
        http::StatusCode::UNAUTHORIZED,
        "the wildcard covers the proxy path too"
    );
}

/// A subject built without any scope is unrestricted: the platform
/// authenticated it and expressed no restriction.
#[tokio::test]
async fn an_unrestricted_subject_keeps_its_access() {
    let app = app().await;
    let response = app
        .send(common::request_with_subject(
            subject(app.tenant),
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
}

/// A restricted token is denied even for an alias that does not exist, so the
/// answer cannot be used to probe the configuration.
#[tokio::test]
async fn the_permission_check_precedes_route_resolution() {
    let app = app().await;
    let subject = scoped(app.tenant, &[UPSTREAM_READ_PERMISSION]);
    let response = app
        .send(common::request_with_subject(
            subject,
            http::Method::GET,
            "/oagw/v1/proxy/never-configured/x",
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::UNAUTHORIZED,
        "an unauthorized caller learns nothing about the route table"
    );
}
