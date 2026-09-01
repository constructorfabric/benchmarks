// Created: 2026-08-29 by Constructor Tech
//! Plugin chain execution order (ADR-0002) and guard rejection statuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use async_trait::async_trait;
use common::{Harness, json_body, post, tenant};
use httpmock::MockServer;
use oagw::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext, TransformPlugin,
};
use oagw::infra::plugin::guard::RequiredHeadersGuard;
use oagw::infra::plugin::registry::plugin_not_found;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use uuid::Uuid;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Marker transform that appends `name: value` to the outbound request and to
/// the returned response, so the execution order becomes observable.
struct Marker {
    id: &'static str,
    name: &'static str,
    journal: Arc<Mutex<Vec<&'static str>>>,
}

impl Marker {
    fn header_name(&self) -> axum::http::HeaderName {
        axum::http::HeaderName::from_bytes(self.name.as_bytes()).unwrap()
    }
}

#[async_trait]
impl TransformPlugin for Marker {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_type(&self) -> &str {
        "transform_plugin"
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        self.journal.lock().unwrap().push(self.id);
        if let Ok(value) = axum::http::HeaderValue::from_str(self.name) {
            ctx.headers.insert(self.header_name(), value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        self.journal.lock().unwrap().push(self.id);
        if let Ok(value) = axum::http::HeaderValue::from_str(self.name) {
            ctx.headers.insert(self.header_name(), value);
        }
        Ok(())
    }

    async fn transform_error(
        &self,
        ctx: &mut oagw::domain::plugin::ErrorContext,
    ) -> Result<(), PluginError> {
        let _ = &ctx.headers;
        Ok(())
    }
}

/// Guard that always rejects with the given status.
struct Rejecting {
    id: &'static str,
    status: axum::http::StatusCode,
}

#[async_trait]
impl oagw::domain::plugin::GuardPlugin for Rejecting {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_type(&self) -> &str {
        "guard_plugin"
    }

    async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        Ok(GuardDecision::Reject {
            status: self.status,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            message: "rejected by the test guard".to_owned(),
        })
    }

    async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        Ok(GuardDecision::Reject {
            status: self.status,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            message: "rejected by the test guard".to_owned(),
        })
    }
}

async fn route(harness: &Harness, upstream_id: &str, plugins: Value) {
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "plugins": plugins,
        }),
        tenant(),
    )
    .await;
}

#[tokio::test]
async fn auth_and_request_transforms_run_before_the_upstream() {
    let server = MockServer::start();
    // The upstream only answers when both the credential and the propagated
    // request id arrived, which proves the request phases ran first.
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .header("authorization", "injected")
            .header("x-request-id", tenant().simple().to_string());
        then.status(200).body("ok");
    });

    let credstore = Arc::new(common::FakeCredStore::new(&[("key", "injected")]));
    let harness = Harness::new(common::test_config(), Some(credstore));
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "ordered.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "auth": {
                    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    "config": { "api_key_ref": "cred://key", "header_name": "authorization" },
                },
                "plugins": {
                    "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"],
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/ordered.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        target.calls(),
        1,
        "auth and transforms must run before the upstream"
    );
}

#[tokio::test]
async fn an_apikey_configured_for_the_query_travels_in_the_query_string() {
    let server = MockServer::start();
    // The caller's `page` survives only because the route allowlists it; the
    // credential is appended after the filter and is never subject to it.
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .query_param("api-key", "injected")
            .query_param("page", "2");
        then.status(200).body("ok");
    });

    let credstore = Arc::new(common::FakeCredStore::new(&[("key", "injected")]));
    let harness = Harness::new(common::test_config(), Some(credstore));
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "querykey.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "auth": {
                    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    "config": { "api_key_ref": "cred://key", "query_param_name": "api-key" },
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": {
                "http": { "methods": ["GET"], "path": "/", "query_allowlist": ["page"] }
            },
        }),
        tenant(),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/querykey.example.com/api?page=2",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200, "upstream must see both parameters");
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn the_response_transform_echoes_the_request_id() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "echo.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": {
                    "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"],
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send("GET", "/oagw/v1/proxy/echo.example.com/api", None, tenant())
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        common::header(&response, "x-request-id").as_deref(),
        Some(tenant().simple().to_string().as_str()),
        "the response phase must run after the upstream"
    );
}

#[tokio::test]
async fn upstream_plugins_execute_before_route_plugins() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let journal: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let harness = Harness::with_plugins(common::test_config(), None, |registry| {
        registry.register_transform(Arc::new(Marker {
            id: "test.upstream.v1",
            name: "x-oagw-test-upstream",
            journal: Arc::clone(&journal),
        }));
        registry.register_transform(Arc::new(Marker {
            id: "test.route.v1",
            name: "x-oagw-test-route",
            journal: Arc::clone(&journal),
        }));
    });

    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "chain-order.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": { "items": ["gts.cf.core.oagw.transform_plugin.v1~test.upstream.v1"] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(
        &harness,
        &upstream_id,
        json!({ "items": ["gts.cf.core.oagw.transform_plugin.v1~test.route.v1"] }),
    )
    .await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/chain-order.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200);
    // Both markers ran twice: once on the request, once on the response. Each
    // phase keeps the upstream before the route (ADR-0002).
    assert_eq!(
        *journal.lock().unwrap(),
        vec![
            "test.upstream.v1",
            "test.route.v1",
            "test.upstream.v1",
            "test.route.v1"
        ]
    );
}

#[tokio::test]
async fn a_request_guard_rejection_is_a_400_without_reaching_the_upstream() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = Harness::with_plugins(common::test_config(), None, |registry| {
        registry.register_guard(Arc::new(Rejecting {
            id: "test.reject400.v1",
            status: axum::http::StatusCode::BAD_REQUEST,
        }));
    });
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "guarded.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~test.reject400.v1"] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/guarded.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("REQUIRED_HEADER_MISSING"),
        "the guard error code must surface: {}",
        body["detail"]
    );
    assert_eq!(
        target.calls(),
        0,
        "a rejected request must not be forwarded"
    );
}

#[tokio::test]
async fn a_response_guard_rejection_is_a_502() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = Harness::with_plugins(common::test_config(), None, |registry| {
        registry.register_guard(Arc::new(Rejecting {
            id: "test.reject502.v1",
            status: axum::http::StatusCode::BAD_GATEWAY,
        }));
    });
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "response-guarded.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~test.reject502.v1"] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/response-guarded.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 502);
    let source = common::header(&response, "x-oagw-error-source");
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
    );
    assert_eq!(source.as_deref(), Some("gateway"));
}

#[tokio::test]
async fn a_configured_required_headers_guard_is_enforced_on_the_wire() {
    let server = MockServer::start();
    let target = server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = Harness::new(common::test_config(), None);
    // ADR-0009's binding shape: the guard carries its configuration next to the
    // reference, so the headers it enforces travel with the binding itself.
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "guarded.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": {
                    "items": [ {
                        "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                        "config": { "required_request_headers": "x-correlation-id" },
                    } ],
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/guarded.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 400, "a missing header must be rejected");
    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("x-correlation-id"),
        "the problem must name the missing header: {}",
        body["detail"]
    );
    assert_eq!(target.calls(), 0, "a rejected request is not forwarded");

    // The same binding passes once the caller supplies the header.
    let supplied = axum::http::Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/guarded.example.com/api")
        .header("x-correlation-id", "abc")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = harness.send_request(supplied, tenant()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(target.calls(), 1);
}

#[tokio::test]
async fn an_unconfigured_required_headers_guard_fails_open() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET);
        then.status(200).body("ok");
    });

    let harness = Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "failopen.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
                "plugins": {
                    "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
                },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();
    route(&harness, &upstream_id, json!({ "items": [] })).await;

    let response = harness
        .send(
            "GET",
            "/oagw/v1/proxy/failopen.example.com/api",
            None,
            tenant(),
        )
        .await;
    assert_eq!(response.status(), 200, "absent configuration must allow");
}

/// A `RequestContext` for direct guard evaluation.
fn request_context(required: Value) -> RequestContext {
    RequestContext {
        tenant_id: tenant(),
        upstream_id: Uuid::new_v4(),
        alias: "guard.example.com".to_owned(),
        method: "GET".to_owned(),
        path: "/api".to_owned(),
        query: None,
        headers: {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::HeaderName::from_static("accept"),
                axum::http::HeaderValue::from_static("application/json"),
            );
            headers
        },
        body: bytes::Bytes::new(),
        uri: "/oagw/v1/proxy/guard.example.com/api".parse().unwrap(),
        config: required.as_object().cloned().unwrap_or_default(),
        security: common::security_for(tenant()),
    }
}

#[tokio::test]
async fn required_request_headers_guard_rejects_with_400() {
    let guard = RequiredHeadersGuard;
    let context =
        request_context(json!({ "required_request_headers": "X-Correlation-Id, accept" }));
    let decision = guard.guard_request(&context).await.unwrap();
    match decision {
        GuardDecision::Reject {
            status, message, ..
        } => {
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert!(message.contains("x-correlation-id"), "{message}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }

    let satisfied = request_context(json!({ "required_request_headers": "accept" }));
    assert_eq!(
        guard.guard_request(&satisfied).await.unwrap(),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn required_response_headers_guard_rejects_with_502() {
    let guard = RequiredHeadersGuard;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::HeaderName::from_static("content-type"),
        axum::http::HeaderValue::from_static("application/json"),
    );
    let context = ResponseContext {
        request_headers: axum::http::HeaderMap::new(),
        headers: headers.clone(),
        status: axum::http::StatusCode::OK,
        body: bytes::Bytes::new(),
        config: json!({ "required_response_headers": "x-signature" })
            .as_object()
            .cloned()
            .unwrap(),
    };
    let decision = guard.guard_response(&context).await.unwrap();
    assert_eq!(
        decision,
        GuardDecision::Reject {
            status: axum::http::StatusCode::BAD_GATEWAY,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            message: "required response header 'x-signature' is absent".to_owned(),
        }
    );

    let satisfied = ResponseContext {
        request_headers: axum::http::HeaderMap::new(),
        headers,
        status: axum::http::StatusCode::OK,
        body: bytes::Bytes::new(),
        config: json!({ "required_response_headers": "content-type" })
            .as_object()
            .cloned()
            .unwrap(),
    };
    assert_eq!(
        guard.guard_response(&satisfied).await.unwrap(),
        GuardDecision::Allow
    );
}

#[tokio::test]
async fn blank_guard_configuration_is_a_no_op() {
    let guard = RequiredHeadersGuard;
    let blank = request_context(json!({ "required_request_headers": " , ," }));
    assert_eq!(
        guard.guard_request(&blank).await.unwrap(),
        GuardDecision::Allow
    );
}

#[test]
fn a_plugin_without_an_implementation_reports_the_reference() {
    let error = plugin_not_found("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.logging.v1");
    assert!(format!("{error:?}").contains("logging"));
}

#[tokio::test]
async fn a_dangling_plugin_reference_is_refused_at_config_time() {
    let server = MockServer::start();
    let harness = Harness::new(common::test_config(), None);
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "dangling.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": server.host(), "port": server.port() } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let upstream_id = created["id"].as_str().unwrap().to_owned();

    // `required_header.v1` (no trailing `s`) is not a plugin anyone implements.
    let response = post(
        harness.router(),
        "/oagw/v1/routes",
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/" } },
            "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_header.v1"] },
        }),
        tenant(),
    )
    .await;
    assert_eq!(response.status(), 400);

    let body = json_body(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}
