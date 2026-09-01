//! Tests for [`crate::domain::plugin`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use uuid::Uuid;

use super::{
    AUTH_PLUGIN_TYPE_ID, AuthPlugin, BodyPayload, CorsOutcome, ErrorContext, GUARD_PLUGIN_TYPE_ID,
    GuardDecision, GuardPlugin, PluginChain, PluginTier, RequestContext, ResponseContext,
    TRANSFORM_PLUGIN_TYPE_ID, TransformPlugin, builtin, cors_error,
};
use crate::domain::cors::CORS_ORIGIN_NOT_ALLOWED_TYPE;
use crate::domain::error::{OagwError, ProblemBody, ProblemContext};
use crate::domain::model::RateLimitStrategy;
use crate::domain::rate_limit::RateLimitDecision;

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(Uuid::from_u128(0x11))
        .build()
}

fn reject_bad_request() -> GuardDecision {
    GuardDecision::reject(StatusCode::BAD_REQUEST, "TEST_REJECTED", "test rejection")
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingAuth {
    calls: AtomicUsize,
    fail: bool,
}

impl std::fmt::Debug for RecordingAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecordingAuth")
    }
}

#[async_trait]
impl AuthPlugin for RecordingAuth {
    fn id(&self) -> &str {
        "test.auth"
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_TYPE_ID
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(OagwError::authentication_failed("nope"))
        } else {
            Ok(())
        }
    }
}

struct RecordingGuard {
    calls: AtomicUsize,
    verdict: Mutex<GuardDecision>,
}

impl std::fmt::Debug for RecordingGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecordingGuard")
    }
}

impl Default for RecordingGuard {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            verdict: Mutex::new(GuardDecision::Allow),
        }
    }
}

impl RecordingGuard {
    fn rejecting() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            verdict: Mutex::new(reject_bad_request()),
        }
    }
}

#[async_trait]
impl GuardPlugin for RecordingGuard {
    fn id(&self) -> &str {
        "test.guard"
    }

    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE_ID
    }

    async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .verdict
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(GuardDecision::Allow))
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        let _ = ctx;
        Ok(GuardDecision::Allow)
    }
}

#[derive(Default)]
struct RecordingTransform {
    calls: AtomicUsize,
    fail: bool,
}

impl std::fmt::Debug for RecordingTransform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecordingTransform")
    }
}

#[async_trait]
impl TransformPlugin for RecordingTransform {
    fn id(&self) -> &str {
        "test.transform"
    }

    fn plugin_type(&self) -> &str {
        TRANSFORM_PLUGIN_TYPE_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ctx.request_id = Some("from-transform".to_owned());
        if self.fail {
            Err(OagwError::downstream_error("transform failed"))
        } else {
            Ok(())
        }
    }

    async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
        Ok(())
    }

    async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// builtin ids
// ---------------------------------------------------------------------------

#[test]
fn builtin_ids_carry_their_base_type() {
    assert_eq!(
        builtin::NOOP_AUTH,
        format!("{AUTH_PLUGIN_TYPE_ID}~cf.core.oagw.noop.v1")
    );
    assert_eq!(
        builtin::APIKEY_AUTH,
        format!("{AUTH_PLUGIN_TYPE_ID}~cf.core.oagw.apikey.v1")
    );
    assert_eq!(
        builtin::OAUTH2_CLIENT_CRED,
        format!("{AUTH_PLUGIN_TYPE_ID}~cf.core.oagw.oauth2_client_cred.v1")
    );
    assert_eq!(
        builtin::OAUTH2_CLIENT_CRED_BASIC,
        format!("{AUTH_PLUGIN_TYPE_ID}~cf.core.oagw.oauth2_client_cred_basic.v1")
    );
    assert_eq!(
        builtin::REQUIRED_HEADERS_GUARD,
        format!("{GUARD_PLUGIN_TYPE_ID}~cf.core.oagw.required_headers.v1")
    );
    assert_eq!(
        builtin::REQUEST_ID_TRANSFORM,
        format!("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1")
    );
    assert_eq!(
        builtin::base_type(builtin::APIKEY_AUTH),
        Some(AUTH_PLUGIN_TYPE_ID)
    );
    assert_eq!(
        builtin::base_type("cf.core.oagw.apikey.v1"),
        Some("cf.core.oagw.apikey.v1")
    );
    assert_eq!(builtin::base_type(""), None);
}

#[test]
fn catalog_only_ids_are_declared_but_distinct_from_builtins() {
    for catalog_only in [
        builtin::BASIC_AUTH,
        builtin::BEARER_AUTH,
        builtin::TIMEOUT_GUARD,
        builtin::CORS_GUARD,
        builtin::LOGGING_TRANSFORM,
        builtin::METRICS_TRANSFORM,
    ] {
        assert_ne!(catalog_only, builtin::NOOP_AUTH);
        assert_ne!(catalog_only, builtin::APIKEY_AUTH);
        assert_ne!(catalog_only, builtin::REQUIRED_HEADERS_GUARD);
        assert_ne!(catalog_only, builtin::REQUEST_ID_TRANSFORM);
    }
}

// ---------------------------------------------------------------------------
// BodyPayload
// ---------------------------------------------------------------------------

#[test]
fn body_payload_reports_its_shape() {
    assert_eq!(BodyPayload::default(), BodyPayload::Empty);
    assert_eq!(BodyPayload::Empty.buffered_len(), None);
    assert_eq!(
        BodyPayload::Buffered(Bytes::from_static(b"abc")).buffered_len(),
        Some(3)
    );
    assert_eq!(
        BodyPayload::Buffered(Bytes::from_static(b"abc"))
            .as_buffered()
            .map(Bytes::as_ref),
        Some(&b"abc"[..])
    );
    assert!(BodyPayload::Streaming.is_streaming());
    assert!(!BodyPayload::Empty.is_streaming());
}

// ---------------------------------------------------------------------------
// Request / response / error contexts
// ---------------------------------------------------------------------------

#[test]
fn request_context_builder_defaults() {
    let ctx = request();
    assert_eq!(ctx.method, "GET");
    assert_eq!(ctx.alias, "payments");
    assert!(ctx.headers.is_empty());
    assert_eq!(ctx.body, BodyPayload::Empty);
    assert!(matches!(ctx.cors, CorsOutcome::Disabled));
    assert!(ctx.rate_limit.is_none());
    assert!(ctx.injected_headers.is_empty());
    assert!(ctx.injected_query.is_empty());
    assert!(ctx.security.is_none());
    assert!(ctx.upstream_id.is_none());
}

#[test]
fn request_context_header_lookup_is_case_insensitive() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    let ctx = RequestContext::builder()
        .method("POST")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .headers(headers)
        .build();
    assert_eq!(
        ctx.header("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("secret")
    );
    assert_eq!(
        ctx.header("X-Api-Key")
            .and_then(|value| value.to_str().ok()),
        Some("secret")
    );
    assert!(ctx.has_header("x-api-key"));
    assert!(!ctx.has_header("x-other"));
}

#[test]
fn request_context_carries_rate_limit_and_cors() {
    let decision = RateLimitDecision {
        allowed: true,
        limit: 10,
        remaining: 9,
        reset_epoch_secs: 1,
        retry_after_seconds: None,
        queue_wait: None,
        degraded: false,
        strategy: RateLimitStrategy::Reject,
        emit_headers: true,
    };
    let ctx = RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1")
        .tenant_id(Uuid::from_u128(1))
        .rate_limit(decision)
        .build();
    let recorded = ctx.rate_limit.as_ref().expect("decision");
    assert!(recorded.is_allowed());
    assert_eq!(recorded.limit, 10);
}

#[test]
fn response_context_reports_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("x-upstream", HeaderValue::from_static("1"));
    let ctx = ResponseContext::builder()
        .status(StatusCode::OK)
        .headers(headers)
        .request_id("abc")
        .build();
    assert_eq!(ctx.status, StatusCode::OK);
    assert_eq!(
        ctx.header("x-upstream").map(HeaderValue::as_bytes),
        Some(b"1".as_slice())
    );
    assert!(ctx.has_header("x-upstream"));
    assert!(!ctx.has_header("x-request-id"));
    assert_eq!(ctx.request_id.as_deref(), Some("abc"));
    assert_eq!(ctx.body, BodyPayload::Empty);
}

#[test]
fn error_context_renders_the_problem_body() {
    let error = cors_error(
        CORS_ORIGIN_NOT_ALLOWED_TYPE,
        "CORS Origin Not Allowed",
        "Origin 'https://evil.example.com' not in allowed origins list".to_owned(),
        "https://evil.example.com".to_owned(),
    );
    let ctx = ErrorContext::from_error(error);
    assert_eq!(ctx.status, StatusCode::FORBIDDEN);
    assert_eq!(ctx.problem_body().r#type, CORS_ORIGIN_NOT_ALLOWED_TYPE);
    assert_eq!(
        ctx.problem_body(),
        &ProblemBody {
            context: ProblemContext {
                invalid_value: Some("https://evil.example.com".to_owned()),
                ..ProblemContext::default()
            },
            ..ProblemBody::bare(
                CORS_ORIGIN_NOT_ALLOWED_TYPE.to_owned(),
                "CORS Origin Not Allowed".to_owned(),
                403,
                "Origin 'https://evil.example.com' not in allowed origins list".to_owned(),
            )
        }
    );
}

// ---------------------------------------------------------------------------
// Guard decisions
// ---------------------------------------------------------------------------

#[test]
fn guard_decision_allow_has_no_status() {
    let decision = GuardDecision::allow();
    assert!(decision.is_allow());
    assert_eq!(decision.status(), None);
    assert_eq!(decision.error_code(), None);
}

#[test]
fn guard_decision_reject_carries_status_and_code() {
    let decision = GuardDecision::reject(StatusCode::BAD_REQUEST, "REQUIRED_HEADER_MISSING", "x");
    assert!(!decision.is_allow());
    assert_eq!(decision.status(), Some(StatusCode::BAD_REQUEST));
    assert_eq!(decision.error_code(), Some("REQUIRED_HEADER_MISSING"));
}

#[test]
fn guard_rejection_maps_onto_the_error_taxonomy() {
    let validation = GuardDecision::reject(StatusCode::BAD_REQUEST, "A", "a").into_error();
    assert_eq!(validation.status(), StatusCode::BAD_REQUEST);
    let unauthorized = GuardDecision::reject(StatusCode::UNAUTHORIZED, "A", "a").into_error();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let forbidden = GuardDecision::reject(StatusCode::FORBIDDEN, "A", "a").into_error();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let throttled = GuardDecision::reject(StatusCode::TOO_MANY_REQUESTS, "A", "a").into_error();
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    let gateway = GuardDecision::reject(StatusCode::BAD_GATEWAY, "A", "a").into_error();
    assert_eq!(gateway.status(), StatusCode::BAD_GATEWAY);
    let unavailable = GuardDecision::reject(StatusCode::SERVICE_UNAVAILABLE, "A", "a").into_error();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    // A status outside the taxonomy folds into the downstream-error family
    // rather than being reported as a client error.
    let custom = GuardDecision::reject(StatusCode::IM_A_TEAPOT, "A", "a").into_error();
    assert_eq!(custom.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn cors_error_is_a_403_with_the_gts_type() {
    let error = cors_error(
        CORS_ORIGIN_NOT_ALLOWED_TYPE,
        "CORS Origin Not Allowed",
        "nope".to_owned(),
        "https://a.example.com".to_owned(),
    );
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(error.problem_body().status, 403);
    assert_eq!(
        error.problem_body().context.invalid_value.as_deref(),
        Some("https://a.example.com")
    );
}

// ---------------------------------------------------------------------------
// Plugin chain
// ---------------------------------------------------------------------------

#[test]
fn empty_chain_is_a_no_op() {
    let chain = PluginChain::new();
    assert!(chain.is_empty());
    assert_eq!(chain.len(), 0);
    assert!(chain.plugin_refs().is_empty());
    assert!(chain.auth_plugins().is_empty());
    assert!(chain.guard_plugins().is_empty());
    assert!(chain.transform_plugins().is_empty());
    assert!(PluginChain::default().is_empty());
}

#[test]
fn plugin_refs_preserve_insertion_order() {
    let mut chain = PluginChain::new();
    chain.push_auth(
        PluginTier::Upstream,
        0,
        "auth",
        Arc::new(RecordingAuth::default()),
    );
    chain.push_guard(
        PluginTier::Upstream,
        1,
        "guard-1",
        Arc::new(RecordingGuard::default()),
    );
    chain.push_guard(
        PluginTier::Route,
        0,
        "guard-2",
        Arc::new(RecordingGuard::default()),
    );
    chain.push_transform(
        PluginTier::Route,
        1,
        "transform",
        Arc::new(RecordingTransform::default()),
    );
    assert_eq!(chain.len(), 4);
    assert_eq!(
        chain.plugin_refs(),
        vec!["auth", "guard-1", "guard-2", "transform"]
    );
    assert_eq!(chain.auth_plugins().len(), 1);
    assert_eq!(chain.guard_plugins().len(), 2);
    assert_eq!(chain.transform_plugins().len(), 1);
    assert!(format!("{chain:?}").contains("guard-1"));
}

#[tokio::test]
async fn chain_runs_auth_before_guards_before_transforms() {
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    struct Tracer {
        phase: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl std::fmt::Debug for Tracer {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.phase)
        }
    }

    #[async_trait]
    impl AuthPlugin for Tracer {
        fn id(&self) -> &str {
            self.phase
        }

        fn plugin_type(&self) -> &str {
            AUTH_PLUGIN_TYPE_ID
        }

        async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            self.order.lock().expect("order").push(self.phase);
            Ok(())
        }
    }

    #[async_trait]
    impl GuardPlugin for Tracer {
        fn id(&self) -> &str {
            self.phase
        }

        fn plugin_type(&self) -> &str {
            GUARD_PLUGIN_TYPE_ID
        }

        async fn guard_request(&self, _ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
            self.order.lock().expect("order").push(self.phase);
            Ok(GuardDecision::Allow)
        }

        async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
            Ok(GuardDecision::Allow)
        }
    }

    #[async_trait]
    impl TransformPlugin for Tracer {
        fn id(&self) -> &str {
            self.phase
        }

        fn plugin_type(&self) -> &str {
            TRANSFORM_PLUGIN_TYPE_ID
        }

        async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            self.order.lock().expect("order").push(self.phase);
            Ok(())
        }

        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
            Ok(())
        }

        async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
            Ok(())
        }
    }

    let mut chain = PluginChain::new();
    chain.push_transform(
        PluginTier::Route,
        0,
        "transform",
        Arc::new(Tracer {
            phase: "transform",
            order: Arc::clone(&order),
        }),
    );
    chain.push_guard(
        PluginTier::Upstream,
        0,
        "guard",
        Arc::new(Tracer {
            phase: "guard",
            order: Arc::clone(&order),
        }),
    );
    chain.push_auth(
        PluginTier::Upstream,
        0,
        "auth",
        Arc::new(Tracer {
            phase: "auth",
            order: Arc::clone(&order),
        }),
    );

    let mut ctx = request();
    chain.authenticate(&mut ctx).await.expect("auth");
    chain.guard_request(&ctx).await.expect("guard");
    chain.transform_request(&mut ctx).await.expect("transform");

    assert_eq!(
        *order.lock().expect("order"),
        vec!["auth", "guard", "transform"]
    );
}

#[tokio::test]
async fn guard_rejection_aborts_the_chain_with_the_mapped_error() {
    let mut chain = PluginChain::new();
    chain.push_guard(
        PluginTier::Upstream,
        0,
        "rejecting",
        Arc::new(RecordingGuard::rejecting()),
    );
    chain.push_guard(
        PluginTier::Upstream,
        1,
        "after",
        Arc::new(RecordingGuard::default()),
    );

    let ctx = request();
    let error = chain.guard_request(&ctx).await.expect_err("rejection");
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("TEST_REJECTED"));
}

#[tokio::test]
async fn transform_failure_aborts_the_chain() {
    let mut chain = PluginChain::new();
    chain.push_transform(
        PluginTier::Upstream,
        0,
        "failing",
        Arc::new(RecordingTransform {
            calls: AtomicUsize::new(0),
            fail: true,
        }),
    );
    chain.push_transform(
        PluginTier::Upstream,
        1,
        "after",
        Arc::new(RecordingTransform::default()),
    );
    let mut ctx = request();
    assert!(chain.transform_request(&mut ctx).await.is_err());
    assert_eq!(ctx.request_id.as_deref(), Some("from-transform"));
}

#[tokio::test]
async fn auth_failure_is_surfaced_verbatim() {
    let mut chain = PluginChain::new();
    chain.push_auth(
        PluginTier::Upstream,
        0,
        "auth",
        Arc::new(RecordingAuth {
            calls: AtomicUsize::new(0),
            fail: true,
        }),
    );
    let mut ctx = request();
    let error = chain.authenticate(&mut ctx).await.expect_err("401");
    assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chain_response_and_error_phases_run_to_completion() {
    let mut chain = PluginChain::new();
    chain.push_guard(
        PluginTier::Upstream,
        0,
        "guard",
        Arc::new(RecordingGuard::default()),
    );
    chain.push_transform(
        PluginTier::Upstream,
        1,
        "transform",
        Arc::new(RecordingTransform::default()),
    );
    let response = ResponseContext::builder().status(StatusCode::OK).build();
    chain
        .guard_response(&response)
        .await
        .expect("guard response");
    let mut response = response;
    chain
        .transform_response(&mut response)
        .await
        .expect("transform response");
    let mut error = ErrorContext::from_error(OagwError::route_not_found("no route"));
    chain
        .transform_error(&mut error)
        .await
        .expect("transform error");
}
