//! Test harness shared by all mini-chat tests: in-memory SQLite with migrations and a started
//! outbox, fake authz / policy / audit ports, a scriptable OpenAI-compatible provider, and
//! HTTP + SSE helpers for router-level tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use bytes::Bytes;
use futures::StreamExt;
use http::{Request, StatusCode};
use mini_chat_sdk::{
    MiniChatAuditEvent, ModelCatalogEntry, PolicySnapshot, TierLimits, UsageEvent, UserLimits,
};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuditDelivery, AuditFailure, AuditPort, AuthzPort, PolicyPort, PublishFailure};
use crate::domain::services::{AppServices, Db};
use crate::infra::llm::transport::{
    HttpResponse, OutgoingBody, ProviderTransport, RawSseEvent, StreamOutcome, TransportError,
};

// ───────────────────────────── identities ─────────────────────────────

pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
pub const USER_A1: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_A2: Uuid = Uuid::from_u128(0x4444_4444_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_B1: Uuid = Uuid::from_u128(0x2222_2222_6a88_4768_9dfc_6bcd_5187_d9ed);

#[must_use]
pub fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder().subject_id(user).subject_tenant_id(tenant).build().unwrap()
}

#[must_use]
pub fn ctx_a1() -> SecurityContext {
    ctx(USER_A1, TENANT_A)
}

// ───────────────────────────── catalog ─────────────────────────────

pub const PREMIUM: &str = "gpt-4.1";
pub const STANDARD: &str = "gpt-4.1-mini";
pub const NO_VISION: &str = "text-only";
pub const TINY: &str = "tiny-ctx";

fn entry(id: &str, tier: &str, enabled: bool, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": format!("{id}-provider"),
        "display_name": id.to_uppercase(),
        "description": format!("{id} model"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": format!("You are {id}."),
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120000,
        "input_tokens_credit_multiplier_micro": if tier == "premium" { 3_000_000 } else { 1_000_000 },
        "output_tokens_credit_multiplier_micro": if tier == "premium" { 15_000_000 } else { 3_000_000 },
        "multiplier_display": if tier == "premium" { "3x" } else { "1x" },
        "max_num_results": 5,
        "max_tool_calls": 2,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "temperature": 0.7 },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": { "web_search": true, "file_search": true, "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": { "responses": true }
        },
        "preference": { "is_default": default, "sort_order": 0 }
    }))
    .unwrap()
}

/// Catalog: premium default `gpt-4.1`, standard `gpt-4.1-mini`, standard `text-only` (no
/// vision, no tools), standard `tiny-ctx` (context 4096, output 1024, input 3072), disabled `old-model`.
#[must_use]
pub fn test_catalog() -> Vec<ModelCatalogEntry> {
    let mut no_vision = entry(NO_VISION, "standard", true, false);
    no_vision.multimodal_capabilities.clear();
    no_vision.general_config.tool_support = mini_chat_sdk::ModelToolSupport::default();
    let mut tiny = entry(TINY, "standard", true, false);
    tiny.context_window = 4096;
    tiny.max_output_tokens = 1024;
    tiny.max_input_tokens = 3072;
    vec![
        entry(PREMIUM, "premium", true, true),
        entry(STANDARD, "standard", true, false),
        no_vision,
        tiny,
        entry("old-model", "standard", false, false),
    ]
}

// ───────────────────────────── fake ports ─────────────────────────────

/// Authz fake: tenant + owner scope; switchable denial / PDP outage.
#[derive(Default)]
pub struct FakeAuthz {
    pub deny: AtomicBool,
    pub unavailable: AtomicBool,
    pub calls: Mutex<Vec<(String, Option<Uuid>)>>,
}

impl FakeAuthz {
    fn check(&self) -> Result<(), DomainError> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(DomainError::unavailable(5, "pdp down"));
        }
        if self.deny.load(Ordering::SeqCst) {
            return Err(DomainError::permission_denied());
        }
        Ok(())
    }
}

#[async_trait]
impl AuthzPort for FakeAuthz {
    async fn chat_scope(&self, ctx: &SecurityContext, action: &str, chat_id: Option<Uuid>) -> Result<AccessScope, DomainError> {
        self.calls.lock().unwrap().push((action.to_owned(), chat_id));
        self.check()?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id()))
    }

    async fn model_access(&self, _ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        self.calls.lock().unwrap().push((format!("model:{action}"), None));
        self.check()
    }

    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        self.calls.lock().unwrap().push(("quota:read".to_owned(), None));
        self.check()?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id()))
    }
}

/// Policy fake: mutable snapshot and limits, records published usage.
pub struct FakePolicy {
    pub snapshot: Mutex<PolicySnapshot>,
    pub standard: Mutex<TierLimits>,
    pub premium: Mutex<TierLimits>,
    pub published: Mutex<Vec<UsageEvent>>,
    pub fail: AtomicBool,
}

impl Default for FakePolicy {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(PolicySnapshot {
                policy_version: 1,
                model_catalog: test_catalog(),
                kill_switches: mini_chat_sdk::KillSwitches::default(),
            }),
            standard: Mutex::new(TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 }),
            premium: Mutex::new(TierLimits { limit_daily_credits_micro: 50_000_000, limit_monthly_credits_micro: 500_000_000 }),
            published: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        }
    }
}

impl FakePolicy {
    pub fn with_snapshot(&self, f: impl FnOnce(&mut PolicySnapshot)) {
        f(&mut self.snapshot.lock().unwrap());
    }

    pub fn set_limits(&self, standard: TierLimits, premium: TierLimits) {
        *self.standard.lock().unwrap() = standard;
        *self.premium.lock().unwrap() = premium;
    }
}

#[async_trait]
impl PolicyPort for FakePolicy {
    async fn current_snapshot(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(DomainError::internal("policy plugin down"));
        }
        Ok(Arc::new(self.snapshot.lock().unwrap().clone()))
    }

    async fn snapshot_by_version(&self, user_id: Uuid, _version: i64) -> Result<Arc<PolicySnapshot>, DomainError> {
        self.current_snapshot(user_id).await
    }

    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError> {
        Ok(UserLimits {
            user_id,
            policy_version: version,
            standard: *self.standard.lock().unwrap(),
            premium: *self.premium.lock().unwrap(),
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishFailure> {
        self.published.lock().unwrap().push(event);
        Ok(())
    }
}

/// Audit fake.
#[derive(Default)]
pub struct FakeAudit {
    pub events: Mutex<Vec<MiniChatAuditEvent>>,
}

#[async_trait]
impl AuditPort for FakeAudit {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<AuditDelivery, AuditFailure> {
        self.events.lock().unwrap().push(event);
        Ok(AuditDelivery::Delivered)
    }
}

// ───────────────────────────── mock provider ─────────────────────────────

/// A recorded provider request.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub uri: String,
    pub json: Option<serde_json::Value>,
    pub multipart: Vec<(String, Option<String>, Option<String>, usize)>,
    pub streaming: bool,
}

/// Scripted answer to a streaming chat request.
#[derive(Debug, Clone)]
pub enum StreamScript {
    /// SSE events, each sent after `delay`.
    Events { events: Vec<RawSseEvent>, delay: Duration },
    /// Events then the stream stays open forever (until the consumer drops it).
    EventsThenHang { events: Vec<RawSseEvent>, delay: Duration },
    /// Non-2xx HTTP response.
    Http { status: u16, body: serde_json::Value, retry_after: Option<u64> },
    /// Transport failure (gateway timeout etc.).
    Transport(TransportError),
}

/// Scriptable OpenAI-compatible provider (Responses API, Files API, Vector Stores API).
pub struct MockProvider {
    pub requests: Mutex<Vec<RecordedRequest>>,
    pub streams: Mutex<VecDeque<StreamScript>>,
    /// Non-streaming chat (thread summary) answers; default `<summary>Summary text</summary>`.
    pub completions: Mutex<VecDeque<HttpResponse>>,
    /// Status reported for vector store files (POST and GET); default `completed`.
    pub vector_file_status: Mutex<VecDeque<String>>,
    /// Status codes for DELETE requests (popped per call); default 200.
    pub delete_statuses: Mutex<VecDeque<u16>>,
    /// Status codes for file uploads; default 200.
    pub upload_statuses: Mutex<VecDeque<u16>>,
    /// Number of streaming requests whose stream was dropped by the consumer.
    pub dropped_streams: AtomicU64,
    seq: AtomicU64,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            streams: Mutex::new(VecDeque::new()),
            completions: Mutex::new(VecDeque::new()),
            vector_file_status: Mutex::new(VecDeque::new()),
            delete_statuses: Mutex::new(VecDeque::new()),
            upload_statuses: Mutex::new(VecDeque::new()),
            dropped_streams: AtomicU64::new(0),
            seq: AtomicU64::new(1),
        }
    }
}

/// Builds a Responses API SSE event.
#[must_use]
pub fn ev(event_type: &str, data: serde_json::Value) -> RawSseEvent {
    let mut data = data;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("type".into(), serde_json::Value::String(event_type.to_owned()));
    }
    RawSseEvent { event: Some(event_type.to_owned()), data: data.to_string() }
}

/// Standard successful text stream: `response.created`, one delta per chunk, `response.completed`.
#[must_use]
pub fn text_stream(chunks: &[&str], input_tokens: i64, output_tokens: i64) -> Vec<RawSseEvent> {
    let mut out = vec![ev("response.created", serde_json::json!({"response": {"id": "resp_abc123def456", "status": "in_progress"}}))];
    for c in chunks {
        out.push(ev("response.output_text.delta", serde_json::json!({"delta": c, "output_index": 0, "content_index": 0})));
    }
    out.push(completed_event(input_tokens, output_tokens));
    out
}

#[must_use]
pub fn completed_event(input_tokens: i64, output_tokens: i64) -> RawSseEvent {
    ev(
        "response.completed",
        serde_json::json!({"response": {"id": "resp_abc123def456", "status": "completed",
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens, "total_tokens": input_tokens + output_tokens,
                      "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}}),
    )
}

impl MockProvider {
    pub fn push_stream(&self, script: StreamScript) {
        self.streams.lock().unwrap().push_back(script);
    }

    pub fn push_events(&self, events: Vec<RawSseEvent>) {
        self.push_stream(StreamScript::Events { events, delay: Duration::ZERO });
    }

    #[must_use]
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Recorded streaming chat request bodies.
    #[must_use]
    pub fn chat_bodies(&self) -> Vec<serde_json::Value> {
        self.recorded().into_iter().filter(|r| r.streaming).filter_map(|r| r.json).collect()
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}{n:024}")
    }

    fn record(&self, method: &str, uri: &str, body: &OutgoingBody, streaming: bool) {
        let (json, multipart) = match body {
            OutgoingBody::Empty => (None, Vec::new()),
            OutgoingBody::Json(v) => (Some(v.clone()), Vec::new()),
            OutgoingBody::Multipart(f) => (
                None,
                f.iter().map(|p| (p.name.clone(), p.filename.clone(), p.content_type.clone(), p.data.len())).collect(),
            ),
        };
        self.requests.lock().unwrap().push(RecordedRequest {
            method: method.to_owned(),
            uri: uri.to_owned(),
            json,
            multipart,
            streaming,
        });
    }
}

fn ok_json(v: &serde_json::Value) -> HttpResponse {
    HttpResponse { status: 200, retry_after_secs: None, body: Bytes::from(v.to_string()) }
}

struct DropGuard(Arc<MockProvider>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.dropped_streams.fetch_add(1, Ordering::SeqCst);
    }
}

/// Transport wrapper so the mock can be shared as `Arc<MockProvider>`.
pub struct MockTransport(pub Arc<MockProvider>);

#[async_trait]
impl ProviderTransport for MockTransport {
    async fn request(
        &self,
        _ctx: &SecurityContext,
        method: http::Method,
        uri: &str,
        body: OutgoingBody,
    ) -> Result<HttpResponse, TransportError> {
        let p = &self.0;
        p.record(method.as_str(), uri, &body, false);
        let path = uri.split('?').next().unwrap_or(uri);
        let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        // segs[0] = alias; then v1|openai ...
        let rest: Vec<&str> = segs.iter().skip(2).copied().collect();
        match (method.as_str(), rest.as_slice()) {
            ("POST", ["files"]) => {
                let st = p.upload_statuses.lock().unwrap().pop_front().unwrap_or(200);
                if st >= 300 {
                    return Ok(HttpResponse { status: st, retry_after_secs: None, body: Bytes::from_static(b"{\"error\":{\"message\":\"upload failed\"}}") });
                }
                Ok(ok_json(&serde_json::json!({"id": p.next_id("file-"), "object": "file"})))
            }
            ("DELETE", ["files", _]) | ("DELETE", ["vector_stores", _]) => {
                let st = p.delete_statuses.lock().unwrap().pop_front().unwrap_or(200);
                Ok(HttpResponse { status: st, retry_after_secs: None, body: Bytes::from_static(b"{\"deleted\":true}") })
            }
            ("POST", ["vector_stores"]) => Ok(ok_json(&serde_json::json!({"id": p.next_id("vs_"), "object": "vector_store"}))),
            ("POST", ["vector_stores", _, "files"]) | ("GET", ["vector_stores", _, "files", _]) => {
                let st = p.vector_file_status.lock().unwrap().pop_front().unwrap_or_else(|| "completed".to_owned());
                Ok(ok_json(&serde_json::json!({"id": "vsf", "object": "vector_store.file", "status": st})))
            }
            ("POST", _) => {
                // Non-streaming chat (thread summary).
                if let Some(r) = p.completions.lock().unwrap().pop_front() {
                    return Ok(r);
                }
                Ok(ok_json(&serde_json::json!({
                    "id": "resp_summary0000000001", "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": [
                        {"type": "output_text", "text": "<analysis>thinking</analysis>\n<summary>Summary text</summary>"}]}],
                    "usage": {"input_tokens": 100, "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 0}}
                })))
            }
            _ => Ok(HttpResponse { status: 404, retry_after_secs: None, body: Bytes::from_static(b"{}") }),
        }
    }

    async fn stream(&self, _ctx: &SecurityContext, uri: &str, body: serde_json::Value) -> Result<StreamOutcome, TransportError> {
        let p = Arc::clone(&self.0);
        p.record("POST", uri, &OutgoingBody::Json(body), true);
        let script = p
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| StreamScript::Events { events: text_stream(&["Hello", " world"], 10, 5), delay: Duration::ZERO });
        let (events, delay, hang) = match script {
            StreamScript::Http { status, body, retry_after } => {
                return Ok(StreamOutcome::Http(HttpResponse { status, retry_after_secs: retry_after, body: Bytes::from(body.to_string()) }));
            }
            StreamScript::Transport(e) => return Err(e),
            StreamScript::Events { events, delay } => (events, delay, false),
            StreamScript::EventsThenHang { events, delay } => (events, delay, true),
        };
        let guard = DropGuard(Arc::clone(&p));
        let s = async_stream::stream! {
            let _guard = guard;
            for e in events {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                yield Ok(e);
            }
            if hang {
                futures::future::pending::<()>().await;
            }
        };
        Ok(StreamOutcome::Events(s.boxed()))
    }
}

// ───────────────────────────── app builder ─────────────────────────────

/// Test configuration (valid defaults + client credentials).
#[must_use]
pub fn test_config() -> MiniChatConfig {
    let mut cfg: MiniChatConfig =
        serde_json::from_value(serde_json::json!({"client_credentials": {"client_id": "mini-chat", "client_secret": "s"}}))
            .unwrap();
    cfg.orphan_watchdog.timeout_secs = 90;
    cfg.validate().unwrap();
    cfg
}

/// A fully wired application over an in-memory database.
pub struct TestApp {
    pub app: Arc<AppServices>,
    pub provider: Arc<MockProvider>,
    pub policy: Arc<FakePolicy>,
    pub authz: Arc<FakeAuthz>,
    pub audit: Arc<FakeAudit>,
    pub outbox: Option<OutboxHandle>,
}

/// Fresh in-memory database with gear + outbox migrations.
pub async fn test_db() -> Arc<Db> {
    let dsn = format!("sqlite:file:minichat_{}?mode=memory&cache=shared", Uuid::new_v4().simple());
    let db = connect_db(&dsn, ConnectOpts { max_conns: Some(4), min_conns: Some(1), ..Default::default() })
        .await
        .unwrap();
    toolkit_db::migration_runner::run_migrations_for_testing(&db, crate::infra::db::migrations::all_migrations())
        .await
        .unwrap();
    Arc::new(DBProvider::new(db))
}

impl TestApp {
    /// Builds the app with `cfg_mut` applied to the default test config and starts the outbox.
    pub async fn with_config(cfg_mut: impl FnOnce(&mut MiniChatConfig)) -> Self {
        let mut cfg = test_config();
        cfg_mut(&mut cfg);
        cfg.validate().unwrap();
        let provider = Arc::new(MockProvider::default());
        let policy = Arc::new(FakePolicy::default());
        let authz = Arc::new(FakeAuthz::default());
        let audit = Arc::new(FakeAudit::default());
        let app = Arc::new(AppServices::new(
            Arc::new(cfg),
            test_db().await,
            authz.clone(),
            policy.clone(),
            audit.clone(),
            Arc::new(MockTransport(Arc::clone(&provider))),
        ));
        let outbox = crate::infra::outbox::handlers::start_pipeline(&app).await.unwrap();
        Self { app, provider, policy, authz, audit, outbox: Some(outbox) }
    }

    pub async fn new() -> Self {
        Self::with_config(|_| {}).await
    }

    /// Router with every mini-chat route.
    #[must_use]
    pub fn router(&self) -> axum::Router {
        let openapi = OpenApiRegistryImpl::new();
        crate::api::routes::register(axum::Router::new(), &openapi, Arc::clone(&self.app))
    }

    /// JSON request through the router. Returns `(status, headers, json body)`.
    pub async fn call(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, http::HeaderMap, serde_json::Value) {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let mut req = b.body(body.map_or_else(Body::empty, |v| Body::from(v.to_string()))).unwrap();
        req.extensions_mut().insert(ctx.clone());
        send(self.router(), req).await
    }

    /// Raw request through the router (multipart uploads etc.).
    pub async fn call_raw(&self, ctx: &SecurityContext, mut req: Request<Body>) -> (StatusCode, http::HeaderMap, serde_json::Value) {
        req.extensions_mut().insert(ctx.clone());
        send(self.router(), req).await
    }

    /// Streaming request; reads the whole SSE body. Returns `(status, events)`; for non-SSE
    /// responses the events list is empty and the JSON body is returned as the third element.
    pub async fn call_sse(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, Vec<SseEvent>, serde_json::Value) {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let mut req = b.body(body.map_or_else(Body::empty, |v| Body::from(v.to_string()))).unwrap();
        req.extensions_mut().insert(ctx.clone());
        let resp = self.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let is_sse = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let bytes = tokio::time::timeout(Duration::from_secs(30), axum::body::to_bytes(resp.into_body(), usize::MAX))
            .await
            .expect("sse body timed out")
            .unwrap();
        if is_sse {
            (status, parse_sse(&String::from_utf8_lossy(&bytes)), serde_json::Value::Null)
        } else {
            (status, Vec::new(), serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
        }
    }

    /// Sends a message (`messages:stream`) and returns `(status, events, error json)`.
    pub async fn send(&self, ctx: &SecurityContext, chat_id: Uuid, body: serde_json::Value) -> (StatusCode, Vec<SseEvent>, serde_json::Value) {
        self.call_sse(ctx, "POST", &format!("/mini-chat/v1/chats/{chat_id}/messages:stream"), Some(body)).await
    }

    /// Creates a chat (optional model) and returns its id.
    pub async fn create_chat(&self, ctx: &SecurityContext, model: Option<&str>) -> Uuid {
        let body = model.map_or_else(|| serde_json::json!({}), |m| serde_json::json!({"model": m}));
        let (st, _, json) = self.call(ctx, "POST", "/mini-chat/v1/chats", Some(body)).await;
        assert_eq!(st, StatusCode::CREATED, "{json}");
        json["id"].as_str().unwrap().parse().unwrap()
    }

    /// Waits (up to 5 s) until `check` returns true.
    pub async fn eventually<F, Fut>(&self, what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..100 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not reached: {what}");
    }
}

async fn send(router: axum::Router, req: Request<Body>) -> (StatusCode, http::HeaderMap, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

/// One parsed SSE event.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// Parses an SSE body (`event:` / `data:` lines, blank-line separated; comments ignored).
#[must_use]
pub fn parse_sse(text: &str) -> Vec<SseEvent> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if let Some(e) = event {
            out.push(SseEvent { event: e, data: serde_json::from_str(&data).unwrap_or(serde_json::Value::String(data)) });
        }
    }
    out
}

/// Names of the events in order.
#[must_use]
pub fn event_names(events: &[SseEvent]) -> Vec<String> {
    events.iter().map(|e| e.event.clone()).collect()
}
