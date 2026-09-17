#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests of the plugin execution engine
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md) "Execution Order"):
//! the order, the short-circuit and the error path, on recording plugins and a
//! transport that counts the calls it received.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, StatusCode};
use parking_lot::Mutex;
use uuid::Uuid;

use super::{
    ErrorView, ExecutionOutcome, PluginExecution, RequestContext, UpstreamCall,
    UpstreamResponseView,
};
use crate::domain::error::OagwError;
use crate::domain::model::{AuthConfig, AuthType, PluginBinding, SharingMode};
use crate::infra::plugins::{
    AuthPlugin, GuardDecision, GuardPlugin, PluginCatalog, PluginInput, PluginRegistry, PluginType,
    TokenCacheConfig, UnavailableSecretResolver,
};

// ── The order the plugins of a chain ran in ──────────────────────────────

/// Scratch-space key the request-side order is recorded under.
const REQUEST_ORDER: &str = "request_order";
/// Scratch-space key the response-side order is recorded under.
const RESPONSE_ORDER: &str = "response_order";
/// Scratch-space key the error-phase order is recorded under.
const ERROR_ORDER: &str = "error_order";
/// Scratch-space key the configuration of the auth binding is recorded under.
const AUTH_CONFIG: &str = "auth_config";

const TEST_AUTH_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.test_auth.v1";
const TEST_GUARD_UPSTREAM: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_upstream.v1";
const TEST_GUARD_ROUTE: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_route.v1";
const TEST_GUARD_REJECTING: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test_guard_rejecting.v1";
const TEST_TRANSFORM_UPSTREAM: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.test_transform_upstream.v1";
const TEST_TRANSFORM_ROUTE: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.test_transform_route.v1";

/// Appends `label` to the `key` scratch-space entry, so a chain's order is
/// readable as `auth,guard:upstream,...`.
fn record(ctx: &mut RequestContext, key: &str, label: &str) {
    let next = match ctx.attribute(key) {
        Some(previous) => format!("{previous},{label}"),
        None => label.to_owned(),
    };
    ctx.set_attribute(key, next);
}

/// A no-op plugin of every kind, recording each phase it runs in.
#[derive(Debug)]
struct Recorder {
    id: &'static str,
    label: &'static str,
}

impl Recorder {
    fn kind(&self) -> PluginType {
        if self.id.contains("auth_plugin") {
            PluginType::Auth
        } else if self.id.contains("guard_plugin") {
            PluginType::Guard
        } else {
            PluginType::Transform
        }
    }
}

#[async_trait]
impl AuthPlugin for Recorder {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_type(&self) -> PluginType {
        self.kind()
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        record(ctx, REQUEST_ORDER, self.label);
        if let Some(config) = ctx.config.as_ref() {
            ctx.set_attribute(AUTH_CONFIG, config.to_string());
        }
        Ok(())
    }
}

#[async_trait]
impl GuardPlugin for Recorder {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_type(&self) -> PluginType {
        self.kind()
    }

    async fn guard_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        record(ctx, REQUEST_ORDER, self.label);
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(
        &self,
        ctx: &mut RequestContext,
        _response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        record(ctx, RESPONSE_ORDER, self.label);
        Ok(GuardDecision::Allow)
    }
}

#[async_trait]
impl crate::infra::plugins::TransformPlugin for Recorder {
    fn id(&self) -> &str {
        self.id
    }

    fn plugin_type(&self) -> PluginType {
        self.kind()
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        record(ctx, REQUEST_ORDER, self.label);
        Ok(())
    }

    async fn transform_response(
        &self,
        ctx: &mut RequestContext,
        _response: &mut UpstreamResponseView,
    ) -> Result<(), OagwError> {
        record(ctx, RESPONSE_ORDER, self.label);
        Ok(())
    }

    async fn transform_error(
        &self,
        ctx: &mut RequestContext,
        _error: &mut ErrorView,
    ) -> Result<(), OagwError> {
        record(ctx, ERROR_ORDER, self.label);
        Ok(())
    }
}

/// A guard that rejects the request with the ADR-0009 rejection.
#[derive(Debug)]
struct RejectingGuard;

#[async_trait]
impl GuardPlugin for RejectingGuard {
    fn id(&self) -> &str {
        TEST_GUARD_REJECTING
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Guard
    }

    async fn guard_request(&self, _ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        Ok(GuardDecision::Reject {
            status: StatusCode::BAD_REQUEST,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            message: "required request header 'x-correlation-id' is missing".to_owned(),
        })
    }

    async fn guard_response(
        &self,
        _ctx: &mut RequestContext,
        _response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        Ok(GuardDecision::Allow)
    }
}

// ── The transport the engine is driven against ───────────────────────────

/// An upstream that counts the calls it received and answers with what the test
/// configured: a response, or the error view of a failed call.
#[derive(Debug, Default)]
struct SpyUpstream {
    calls: Mutex<usize>,
    response: Option<UpstreamResponseView>,
    error: Option<ErrorView>,
}

impl SpyUpstream {
    fn answering(response: UpstreamResponseView) -> Self {
        Self {
            calls: Mutex::new(0),
            response: Some(response),
            error: None,
        }
    }

    fn failing(error: ErrorView) -> Self {
        Self {
            calls: Mutex::new(0),
            response: None,
            error: Some(error),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock()
    }
}

#[async_trait]
impl UpstreamCall for SpyUpstream {
    async fn send(&self, _ctx: &mut RequestContext) -> Result<UpstreamResponseView, ErrorView> {
        *self.calls.lock() += 1;
        match &self.error {
            Some(error) => Err(error.clone()),
            None => Ok(self
                .response
                .clone()
                .unwrap_or_else(|| UpstreamResponseView::new(StatusCode::OK))),
        }
    }
}

// ── Harness ──────────────────────────────────────────────────────────────

const TOKEN_CACHE: TokenCacheConfig =
    TokenCacheConfig::new(std::time::Duration::from_secs(300), 10_000);

fn security() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("valid security context")
}

fn context() -> RequestContext {
    RequestContext::new(security(), Uuid::new_v4(), "/v1/chat")
}

fn empty_registry() -> PluginRegistry {
    PluginRegistry::with_builtins(Arc::new(UnavailableSecretResolver), TOKEN_CACHE)
}

/// A registry with one recorder of every kind, so a chain's order is observable.
fn recording_registry() -> PluginRegistry {
    let mut registry = empty_registry();
    registry.register_auth(Arc::new(Recorder {
        id: TEST_AUTH_ID,
        label: "auth",
    }));
    registry.register_guard(Arc::new(Recorder {
        id: TEST_GUARD_UPSTREAM,
        label: "guard:upstream",
    }));
    registry.register_guard(Arc::new(Recorder {
        id: TEST_GUARD_ROUTE,
        label: "guard:route",
    }));
    registry.register_guard(Arc::new(RejectingGuard));
    registry.register_transform(Arc::new(Recorder {
        id: TEST_TRANSFORM_UPSTREAM,
        label: "transform:upstream",
    }));
    registry.register_transform(Arc::new(Recorder {
        id: TEST_TRANSFORM_ROUTE,
        label: "transform:route",
    }));
    registry
}

fn auth_of(id: &str) -> Option<AuthConfig> {
    Some(AuthConfig {
        auth_type: Some(AuthType::try_new(id).expect("valid auth plugin id")),
        sharing: SharingMode::default(),
        config: None,
    })
}

fn engine(
    registry: &PluginRegistry,
    upstream: &[PluginBinding],
    route: &[PluginBinding],
) -> PluginExecution {
    PluginExecution::resolve(
        registry,
        None,
        security().subject_tenant_id(),
        None,
        upstream,
        route,
    )
    .expect("a resolvable chain")
}

fn rejection_of(outcome: ExecutionOutcome) -> (StatusCode, String) {
    let ExecutionOutcome::Error(error) = outcome else {
        panic!("expected an error outcome, got {outcome:?}");
    };
    (error.status, error.error_code)
}

/// The response an outcome let through, or a panic naming the error instead.
fn response_of(outcome: ExecutionOutcome) -> UpstreamResponseView {
    let ExecutionOutcome::Response(response) = outcome else {
        panic!("expected a response outcome, got {outcome:?}");
    };
    response
}

// ── Order ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_request_side_runs_auth_then_guards_then_transforms() {
    let registry = recording_registry();
    let engine = engine(
        &registry,
        &[
            PluginBinding::builtin(TEST_GUARD_UPSTREAM),
            PluginBinding::builtin(TEST_TRANSFORM_UPSTREAM),
        ],
        &[
            PluginBinding::builtin(TEST_GUARD_ROUTE),
            PluginBinding::builtin(TEST_TRANSFORM_ROUTE),
        ],
    );

    let mut ctx = context();
    let decision = engine.run_request(&mut ctx).await.expect("request side");

    assert_eq!(decision, GuardDecision::Allow);
    assert_eq!(
        ctx.attribute(REQUEST_ORDER),
        Some("guard:upstream,guard:route,transform:upstream,transform:route"),
        "every guard runs before every transform, upstream-bound before route-bound"
    );
}

#[tokio::test]
async fn the_auth_plugin_runs_first() {
    let registry = recording_registry();
    let tenant = security().subject_tenant_id();
    let engine = PluginExecution::resolve(
        &registry,
        None,
        tenant,
        auth_of(TEST_AUTH_ID).as_ref(),
        &[],
        &[PluginBinding::builtin(TEST_GUARD_ROUTE)],
    )
    .expect("a resolvable chain");

    let mut ctx = context();
    engine.run_request(&mut ctx).await.expect("request side");
    assert_eq!(
        ctx.attribute(REQUEST_ORDER),
        Some("auth,guard:route"),
        "auth runs before the guards"
    );
}

#[tokio::test]
async fn the_response_side_runs_guards_before_transforms() {
    let registry = recording_registry();
    let engine = engine(
        &registry,
        &[PluginBinding::builtin(TEST_GUARD_UPSTREAM)],
        &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
    );

    let mut ctx = context();
    let mut response = UpstreamResponseView::new(StatusCode::OK);
    let decision = engine
        .run_response(&mut ctx, &mut response)
        .await
        .expect("response side");

    assert_eq!(decision, GuardDecision::Allow);
    assert_eq!(
        ctx.attribute(RESPONSE_ORDER),
        Some("guard:upstream,transform:route"),
        "a guard sees the response before a transform mutates it"
    );
}

// ── Short-circuit and error path ─────────────────────────────────────────

#[tokio::test]
async fn a_guard_rejection_short_circuits_the_request_side() {
    let registry = recording_registry();
    let engine = engine(
        &registry,
        &[
            PluginBinding::builtin(TEST_GUARD_REJECTING),
            PluginBinding::builtin(TEST_TRANSFORM_UPSTREAM),
        ],
        &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
    );
    let upstream = SpyUpstream::default();

    let mut ctx = context();
    let outcome = engine
        .execute(&mut ctx, &upstream)
        .await
        .expect("the engine ran");

    let (status, error_code) = rejection_of(outcome);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
    assert_eq!(
        upstream.calls(),
        0,
        "a rejected request never reaches the upstream"
    );
    assert_eq!(
        ctx.attribute(REQUEST_ORDER),
        None,
        "no request transform ran after the rejection"
    );
}

#[tokio::test]
async fn a_response_guard_rejection_becomes_the_error_outcome() {
    let registry = recording_registry();
    // The built-in guard runs on both phases; the rejecting guard only ever
    // rejects the request, so the response side needs its own rejection.
    let mut registry = registry;
    registry.register_guard(Arc::new(ResponseRejectingGuard));
    let engine = engine(
        &registry,
        &[PluginBinding::builtin(TEST_GUARD_REJECTING)],
        &[],
    );
    let upstream = SpyUpstream::answering(UpstreamResponseView::new(StatusCode::OK));

    let mut ctx = context();
    let outcome = engine
        .execute(&mut ctx, &upstream)
        .await
        .expect("the engine ran");

    let (status, error_code) = rejection_of(outcome);
    assert_eq!(status, StatusCode::BAD_GATEWAY, "the guard's own status");
    assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
    assert_eq!(
        upstream.calls(),
        1,
        "the upstream was called before the guard rejected"
    );
}

/// A guard that rejects the response phase with the ADR-0009 rejection.
#[derive(Debug)]
struct ResponseRejectingGuard;

#[async_trait]
impl GuardPlugin for ResponseRejectingGuard {
    fn id(&self) -> &str {
        TEST_GUARD_REJECTING
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Guard
    }

    async fn guard_request(&self, _ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        Ok(GuardDecision::Allow)
    }

    async fn guard_response(
        &self,
        _ctx: &mut RequestContext,
        _response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        Ok(GuardDecision::Reject {
            status: StatusCode::BAD_GATEWAY,
            error_code: "REQUIRED_HEADER_MISSING".to_owned(),
            message: "required response header 'content-type' is missing".to_owned(),
        })
    }
}

#[tokio::test]
async fn an_allowed_chain_returns_the_upstream_response() {
    let registry = recording_registry();
    let engine = engine(
        &registry,
        &[PluginBinding::builtin(TEST_TRANSFORM_UPSTREAM)],
        &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
    );
    let mut response = UpstreamResponseView::new(StatusCode::OK);
    response.body = b"{}".to_vec();
    let upstream = SpyUpstream::answering(response);

    let mut ctx = context();
    let outcome = engine
        .execute(&mut ctx, &upstream)
        .await
        .expect("the engine ran");

    let response = response_of(outcome);
    assert_eq!(upstream.calls(), 1, "the upstream was called once");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body, b"{}",
        "the upstream's response is what comes back"
    );
}

#[tokio::test]
async fn the_error_transforms_run_on_a_failed_upstream_call() {
    let registry = recording_registry();
    let engine = engine(
        &registry,
        &[],
        &[PluginBinding::builtin(TEST_TRANSFORM_ROUTE)],
    );
    let upstream = SpyUpstream::failing(ErrorView {
        status: StatusCode::BAD_GATEWAY,
        error_code: "PROTOCOL_ERROR".to_owned(),
        message: "the upstream answered out of protocol".to_owned(),
        headers: HeaderMap::new(),
    });

    let mut ctx = context();
    let outcome = engine
        .execute(&mut ctx, &upstream)
        .await
        .expect("the engine ran");

    let (status, _) = rejection_of(outcome);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(upstream.calls(), 1);
    assert_eq!(
        ctx.attribute(ERROR_ORDER),
        Some("transform:route"),
        "the error phase ran the bound transforms"
    );
}

#[tokio::test]
async fn an_empty_engine_is_a_noop_that_still_calls_the_upstream() {
    let registry = empty_registry();
    let engine = engine(&registry, &[], &[]);
    assert!(engine.is_empty());
    let upstream = SpyUpstream::answering(UpstreamResponseView::new(StatusCode::OK));

    let mut ctx = context();
    let outcome = engine
        .execute(&mut ctx, &upstream)
        .await
        .expect("the engine ran");

    assert!(
        matches!(outcome, ExecutionOutcome::Response(_)),
        "{outcome:?}"
    );
    assert_eq!(upstream.calls(), 1);
}

// ── Resolution ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_binding_configuration_travels_on_the_request_context() {
    let registry = recording_registry();
    let tenant = security().subject_tenant_id();
    let auth = AuthConfig {
        auth_type: Some(AuthType::try_new(TEST_AUTH_ID).expect("valid auth plugin id")),
        sharing: SharingMode::default(),
        config: Some(serde_json::json!({ "secret_ref": "cred://client_secret" })),
    };
    let engine = PluginExecution::resolve(&registry, None, tenant, Some(&auth), &[], &[])
        .expect("a resolvable chain");

    let mut ctx = context();
    engine.run_request(&mut ctx).await.expect("request side");
    assert_eq!(
        ctx.attribute(AUTH_CONFIG),
        Some("{\"secret_ref\":\"cred://client_secret\"}"),
        "the binding's config is published for the plugin currently running"
    );
}

#[tokio::test]
async fn a_second_auth_plugin_in_plugins_items_is_a_400() {
    let registry = recording_registry();
    let error = PluginExecution::resolve(
        &registry,
        None,
        security().subject_tenant_id(),
        None,
        &[PluginBinding::builtin(TEST_AUTH_ID)],
        &[],
    )
    .expect_err("auth plugins bind through auth.type only");
    assert!(matches!(error, OagwError::Validation { .. }), "{error}");
}

#[tokio::test]
async fn a_catalog_only_binding_is_a_400() {
    let registry = empty_registry();
    for reference in ["timeout", "cors", "logging", "metrics"] {
        let error = PluginExecution::resolve(
            &registry,
            None,
            security().subject_tenant_id(),
            None,
            &[PluginBinding::builtin(reference)],
            &[],
        )
        .expect_err("catalog only");
        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
    }
}

#[tokio::test]
async fn an_unknown_binding_is_a_400() {
    let registry = empty_registry();
    let error = PluginExecution::resolve(
        &registry,
        None,
        security().subject_tenant_id(),
        None,
        &[PluginBinding::builtin(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1",
        )],
        &[],
    )
    .expect_err("unknown plugin");
    assert!(matches!(error, OagwError::Validation { .. }), "{error}");
}

#[tokio::test]
async fn a_custom_plugin_without_an_implementation_is_a_503() {
    let registry = empty_registry();
    let catalog = PluginCatalog::new();
    let tenant = security().subject_tenant_id();
    let definition = catalog.create(
        tenant,
        PluginInput {
            plugin_type: PluginType::Transform,
            name: "redact_pii".to_owned(),
            config_schema: serde_json::json!({}),
            source_code: "def transform(ctx): pass".to_owned(),
        },
    );

    let error = PluginExecution::resolve(
        &registry,
        Some(&catalog),
        tenant,
        None,
        &[PluginBinding::custom(definition.id)],
        &[],
    )
    .expect_err("no in-process implementation yet");
    assert!(matches!(error, OagwError::PluginNotFound { .. }), "{error}");

    // A UUID no definition belongs to is a 400, not a 503.
    let error = PluginExecution::resolve(
        &registry,
        Some(&catalog),
        tenant,
        None,
        &[PluginBinding::custom(Uuid::new_v4())],
        &[],
    )
    .expect_err("unknown custom plugin");
    assert!(matches!(error, OagwError::Validation { .. }), "{error}");
}

#[tokio::test]
async fn the_engine_resolves_the_same_bindings_twice() {
    let registry = recording_registry();
    let bindings = [
        PluginBinding::builtin(TEST_GUARD_UPSTREAM),
        PluginBinding::builtin(TEST_TRANSFORM_ROUTE),
    ];
    let first = engine(&registry, &bindings, &[]);
    let second = engine(&registry, &bindings, &[]);

    assert!(!first.is_empty());
    assert!(!second.is_empty());

    let mut left = context();
    let mut right = context();
    first.run_request(&mut left).await.expect("request side");
    second.run_request(&mut right).await.expect("request side");
    assert_eq!(
        left.attribute(REQUEST_ORDER),
        right.attribute(REQUEST_ORDER)
    );
}
