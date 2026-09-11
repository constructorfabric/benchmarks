//! Plugin chain ordering (T056, PRD "Auth -> Guards -> Transform(request) ->
//! Upstream call -> Transform(response/error)").
//!
//! Recording plugins of each kind are installed into the gear's registry, so
//! the order the chain ran in is read off a log rather than inferred from
//! side effects. Upstream bindings run before route bindings, a guard
//! rejection stops the chain, and a gateway failure runs the error half of
//! the transform chain, in reverse.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use common::*;
use httpmock::prelude::*;
use oagw::domain::error::DomainError;
use oagw::domain::gts_helpers::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};
use oagw::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginError, RequestContext,
    ResponseContext, TransformPlugin,
};
use oagw::infra::plugin::PluginRegistry;
use serde_json::{Value, json};

/// The log every recording plugin appends to.
type Log = Arc<Mutex<Vec<String>>>;

/// A registry-resolvable plugin id: `…~test.<name>`, the shape
/// `parse_plugin_ref` treats as a named plugin rather than a persisted one.
fn plugin_id(kind: &str, name: &str) -> String {
    format!("gts.cf.core.oagw.{kind}_plugin.v1~test.{name}")
}

/// The recording auth plugin: it notes that it ran and stamps a header.
struct RecordingAuth {
    plugin: String,
    log: Log,
}

#[async_trait]
impl AuthPlugin for RecordingAuth {
    fn id(&self) -> &str {
        &self.plugin
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        self.log.lock().expect("log").push("auth".to_string());
        ctx.set_header("x-plugin-auth", "on");
        Ok(())
    }
}

/// The recording guard plugin: it notes that it ran and may reject.
struct RecordingGuard {
    plugin: String,
    tag: &'static str,
    reject: Option<DomainError>,
    log: Log,
}

#[async_trait]
impl GuardPlugin for RecordingGuard {
    fn id(&self) -> &str {
        &self.plugin
    }

    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        self.log
            .lock()
            .expect("log")
            .push(format!("guard:{}", self.tag));
        if let Some(error) = self.reject.clone() {
            return Ok(GuardDecision::Reject(error));
        }
        Ok(GuardDecision::Next)
    }

    async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        Ok(GuardDecision::Next)
    }
}

/// The recording transform plugin: it notes every hook it was handed.
struct RecordingTransform {
    plugin: String,
    tag: &'static str,
    log: Log,
}

impl RecordingTransform {
    fn note(&self, phase: &str) {
        self.log
            .lock()
            .expect("log")
            .push(format!("{phase}:{}", self.tag));
    }
}

#[async_trait]
impl TransformPlugin for RecordingTransform {
    fn id(&self) -> &str {
        &self.plugin
    }

    fn plugin_type(&self) -> &str {
        TRANSFORM_PLUGIN_TYPE
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        self.note("request");
        ctx.set_header(format!("x-plugin-{}", self.tag), "request");
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError> {
        self.note("response");
        ctx.set_header(format!("x-plugin-{}", self.tag), "response");
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
        self.note("error");
        ctx.set_header(format!("x-plugin-{}", self.tag), "error");
        Ok(())
    }
}

/// The transforms the registry carries, named for their binding.
const TRANSFORMS: &[&str] = &["u1", "u2", "r1", "r2", "err"];

/// A registry holding only the recording plugins.
fn recording_registry(log: &Log, guard: RecordingGuard) -> PluginRegistry {
    let mut registry = PluginRegistry::empty();
    registry.register_auth(Arc::new(RecordingAuth {
        plugin: plugin_id("auth", "u1"),
        log: log.clone(),
    }));
    registry.register_guard(Arc::new(guard));
    for tag in TRANSFORMS {
        registry.register_transform(Arc::new(RecordingTransform {
            plugin: plugin_id("transform", tag),
            tag,
            log: log.clone(),
        }));
    }
    registry
}

/// A guard that lets everything through.
fn passing_guard(log: &Log) -> RecordingGuard {
    RecordingGuard {
        plugin: plugin_id("guard", "u2"),
        tag: "guard",
        reject: None,
        log: log.clone(),
    }
}

/// A gear whose registry is the recording one, backed by a stub upstream.
async fn gear(stub: &MockServer, registry: PluginRegistry) -> (Harness, String) {
    let harness = Harness::with_plugins(Harness::plaintext_config(), None, Some(Arc::new(registry)));
    let upstream_id = create_upstream(&harness, "vendor.com", "127.0.0.1", stub.port(), "http").await;
    create_route(&harness, &upstream_id, "/v1/models", &["GET"]).await;
    (harness, upstream_id)
}

/// Rewrites an upstream, re-stating the record the PUT requires and adding
/// the plugin bindings.
async fn bind_upstream_plugins(
    harness: &Harness,
    stub: &MockServer,
    upstream_id: &str,
    items: &[String],
) {
    let mut body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    body["plugins"] = json!({ "items": items });
    let response = harness
        .send(harness.request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(body),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "plugins bound to the upstream");
}

/// Binds plugins to the route serving `/v1/models`, returning its id.
async fn bind_route_plugins(
    harness: &Harness,
    upstream_id: &str,
    items: &[String],
) -> String {
    let listed: Value =
        read_json(harness.send(harness.request("GET", "/oagw/v1/routes", None)).await).await;
    let route_id = listed["value"].as_array().cloned().unwrap_or_default()[0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut body = route_body(upstream_id, "/v1/models", &["GET"]);
    body["plugins"] = json!({ "items": items });
    let response = harness
        .send(harness.request(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(body),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "plugins bound to the route");
    route_id
}

/// A stub answering `GET /v1/models` with `ok`.
fn models(stub: &MockServer) -> httpmock::Mock<'_> {
    stub.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).body("ok");
    })
}

async fn proxy(harness: &Harness, url: &str) -> axum::http::Response<axum::body::Body> {
    harness
        .send(harness.proxy_request("GET", url, &[], None))
        .await
}

/// The recorded order.
fn order(log: &Log) -> Vec<String> {
    log.lock().expect("log").clone()
}

// ---------------------------------------------------------------------------
// The documented order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_runs_before_guards_and_guards_before_transforms() {
    let stub = MockServer::start();
    let _models = models(&stub);
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let registry = recording_registry(&log, passing_guard(&log));
    let (harness, upstream_id) = gear(&stub, registry).await;
    bind_upstream_plugins(
        &harness,
        &stub,
        &upstream_id,
        &[
            plugin_id("auth", "u1"),
            plugin_id("guard", "u2"),
            plugin_id("transform", "u1"),
        ],
    )
    .await;

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK, "the chain let it through");
    assert_eq!(
        order(&log),
        vec![
            "auth".to_string(),
            "guard:guard".to_string(),
            "request:u1".to_string(),
            "response:u1".to_string(),
        ],
        "auth, then guards, then request transforms"
    );
}

#[tokio::test]
async fn upstream_plugins_run_before_route_plugins() {
    let stub = MockServer::start();
    let _models = models(&stub);
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let registry = recording_registry(&log, passing_guard(&log));
    let (harness, upstream_id) = gear(&stub, registry).await;
    bind_upstream_plugins(
        &harness,
        &stub,
        &upstream_id,
        &[plugin_id("transform", "u1"), plugin_id("transform", "u2")],
    )
    .await;
    bind_route_plugins(
        &harness,
        &upstream_id,
        &[plugin_id("transform", "r1"), plugin_id("transform", "r2")],
    )
    .await;

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        order(&log),
        vec![
            "request:u1".to_string(),
            "request:u2".to_string(),
            "request:r1".to_string(),
            "request:r2".to_string(),
            "response:r2".to_string(),
            "response:r1".to_string(),
            "response:u2".to_string(),
            "response:u1".to_string(),
        ],
        "upstream bindings first, response transforms in reverse"
    );
}

// ---------------------------------------------------------------------------
// Rejection and the error half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_guard_rejection_stops_the_chain_and_returns_its_status() {
    let stub = MockServer::start();
    let _models = models(&stub);
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let registry = recording_registry(
        &log,
        RecordingGuard {
            plugin: plugin_id("guard", "reject"),
            tag: "reject",
            reject: Some(DomainError::ValidationError {
                detail: "the guard said no".to_string(),
            }),
            log: log.clone(),
        },
    );
    let (harness, upstream_id) = gear(&stub, registry).await;
    // The transform is bound *after* the guard, so a stopped chain leaves it
    // unrun and the upstream undialled.
    bind_upstream_plugins(
        &harness,
        &stub,
        &upstream_id,
        &[plugin_id("guard", "reject"), plugin_id("transform", "u1")],
    )
    .await;

    let response = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "the guard's status");
    let problem: Value = read_json(response).await;
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
    );
    assert_eq!(
        order(&log),
        vec!["guard:reject".to_string()],
        "nothing ran after the guard"
    );
}

#[tokio::test]
async fn a_gateway_failure_runs_the_error_transforms() {
    let stub = MockServer::start();
    let _models = models(&stub);
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let registry = recording_registry(&log, passing_guard(&log));
    let (harness, upstream_id) = gear(&stub, registry).await;
    // A one-request ceiling turns the second proxy call into a 429, which is a
    // gateway failure the transform chain is handed.
    let mut body = upstream_body("vendor.com", "127.0.0.1", stub.port(), "http");
    body["rate_limit"] = json!({
        "sharing": "enforce",
        "algorithm": "token_bucket",
        "sustained": { "rate": 1, "window": "second" },
        "scope": "global",
        "strategy": "reject"
    });
    body["plugins"] = json!({ "items": [plugin_id("transform", "err"), plugin_id("transform", "u1")] });
    let response = harness
        .send(harness.request(
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(body),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "rate limit configured");

    let first = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(first.status(), StatusCode::OK, "the first call is inside the ceiling");

    let second = proxy(&harness, "/oagw/v1/proxy/vendor.com/v1/models").await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the ceiling stops the second"
    );
    // The error transforms' headers are stamped onto the rendered problem.
    let stamped = |name: &str| -> Vec<String> {
        second
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        stamped("x-plugin-u1"),
        vec!["error".to_string()],
        "the error transform's header landed"
    );
    assert_eq!(
        stamped("x-plugin-err"),
        vec!["error".to_string()],
        "the inner error transform's header landed"
    );

    let problem: Value = read_json(second).await;
    assert_eq!(
        problem["type"],
        json!("gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
    );

    assert_eq!(
        order(&log),
        vec![
            "request:err".to_string(),
            "request:u1".to_string(),
            "response:u1".to_string(),
            "response:err".to_string(),
            "request:err".to_string(),
            "request:u1".to_string(),
            "error:u1".to_string(),
            "error:err".to_string(),
        ],
        "the error half ran, in reverse, after the gateway failed"
    );
}
