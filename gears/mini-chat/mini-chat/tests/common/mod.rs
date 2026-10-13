//! Shared in-process test harness: temp-file SQLite with the real migrations
//! and outbox, a permissive (tenant-scoped) PDP, a recording policy plugin, a
//! recording audit plugin and a scripted fake provider behind OAGW's port.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use bytes::Bytes;
use futures::StreamExt;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::service::policy::FixedPolicySource;
use mini_chat::domain::service::stream::{StreamEvent, StreamStart};
use mini_chat::domain::service::{AppServices, Authz};
use mini_chat::infra::llm::transport::ProviderTransport;
use mini_chat::infra::outbox::OutboxEnqueuer;
use mini_chat::infra::outbox::handlers::{AuditResolution, AuditSource, start_pipeline};
use mini_chat::infra::plugins::static_model_policy::{StaticModelPolicy, StaticModelPolicyConfig};
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits, TurnAuditEvent,
    TurnMutationAuditEvent, UsageEvent, UserLimits,
};
use oagw_sdk::Body;
use parking_lot::Mutex;
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, Db, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

// ── PDP ──────────────────────────────────────────────────────────────────

/// Mirrors static-authz-plugin: `In(owner_tenant_id, [subject tenant])`.
pub struct TenantPdp {
    pub mode: Mutex<PdpMode>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PdpMode {
    Allow,
    Deny,
    Fail,
}

#[async_trait]
impl AuthZResolverApi for TenantPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        match *self.mode.lock() {
            PdpMode::Deny => {
                return Ok(EvaluationResponse {
                    decision: false,
                    context: EvaluationResponseContext::default(),
                });
            }
            PdpMode::Fail => {
                return Err(CanonicalError::service_unavailable().with_detail("pdp down").create());
            }
            PdpMode::Allow => {}
        }
        let tid = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap();
        let declared = request
            .context
            .supported_properties
            .iter()
            .any(|p| p == pep_properties::OWNER_TENANT_ID);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: if declared {
                    vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(pep_properties::OWNER_TENANT_ID, [tid]))],
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            },
        })
    }
}

// ── policy plugin ────────────────────────────────────────────────────────

pub struct RecordingPolicy {
    pub inner: Mutex<Arc<StaticModelPolicy>>,
    pub published: Mutex<Vec<UsageEvent>>,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for RecordingPolicy {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<u64, MiniChatModelPolicyPluginError> {
        Ok(1)
    }

    async fn get_policy_snapshot(&self, user_id: Uuid, v: u64) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        let inner = self.inner.lock().clone();
        inner.get_policy_snapshot(user_id, v).await
    }

    async fn get_user_limits(&self, user_id: Uuid, v: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        let inner = self.inner.lock().clone();
        inner.get_user_limits(user_id, v).await
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().push(payload);
        Ok(())
    }
}

// ── audit plugin ─────────────────────────────────────────────────────────

#[derive(Default)]
pub struct RecordingAudit {
    pub turns: Mutex<Vec<TurnAuditEvent>>,
    pub mutations: Mutex<Vec<TurnMutationAuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.turns.lock().push(event);
        Ok(())
    }

    async fn emit_turn_mutation_audit(&self, event: TurnMutationAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.mutations.lock().push(event);
        Ok(())
    }
}

pub struct FixedAudit(pub Arc<RecordingAudit>);

#[async_trait]
impl AuditSource for FixedAudit {
    async fn resolve(&self) -> AuditResolution {
        AuditResolution::Found(self.0.clone())
    }
}

// ── fake provider ────────────────────────────────────────────────────────

/// Scripted reply of a chat call.
#[derive(Clone, Debug)]
pub enum Reply {
    /// SSE frames `(event, data)` then end of body.
    Events(Vec<(String, Value)>),
    /// SSE frames then a body that never ends (until dropped).
    Hang(Vec<(String, Value)>),
    /// Non-2xx status with a JSON body and optional Retry-After.
    Status(u16, Value, Option<u64>),
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub uri: String,
    pub body: Value,
}

pub struct FakeProvider {
    pub requests: Mutex<Vec<Recorded>>,
    pub chat_replies: Mutex<VecDeque<Reply>>,
    pub summary_text: Mutex<String>,
    pub vs_status: Mutex<String>,
    pub delete_status: Mutex<u16>,
    pub upload_status: Mutex<u16>,
    pub delay_ms: Mutex<u64>,
    counter: AtomicUsize,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            chat_replies: Mutex::new(VecDeque::new()),
            summary_text: Mutex::new("<analysis>a</analysis><summary>The summary</summary>".to_owned()),
            vs_status: Mutex::new("completed".to_owned()),
            delete_status: Mutex::new(200),
            upload_status: Mutex::new(200),
            delay_ms: Mutex::new(0),
            counter: AtomicUsize::new(0),
        }
    }
}

pub fn ev(name: &str, data: Value) -> (String, Value) {
    (name.to_owned(), data)
}

pub fn delta(t: &str) -> (String, Value) {
    ev("response.output_text.delta", json!({"type": "response.output_text.delta", "delta": t}))
}

pub fn completed(input: i64, output: i64) -> (String, Value) {
    ev(
        "response.completed",
        json!({"type": "response.completed", "response": {"id": "resp_abc123", "usage": {"input_tokens": input, "output_tokens": output}}}),
    )
}

/// Default successful reply: "Hello" + " world", usage 100/50.
pub fn ok_reply() -> Reply {
    Reply::Events(vec![delta("Hello"), delta(" world"), completed(100, 50)])
}

fn sse_body(frames: Vec<(String, Value)>, hang: bool, delay: u64) -> Body {
    let s = async_stream::stream! {
        for (name, data) in frames {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            yield Ok::<Bytes, oagw_sdk::body::BoxError>(Bytes::from(format!("event: {name}\ndata: {data}\n\n")));
        }
        if hang {
            futures::future::pending::<()>().await;
        }
    };
    Body::Stream(Box::pin(s))
}

fn json_resp(status: u16, v: &Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::Bytes(Bytes::from(v.to_string())))
        .unwrap()
}

impl FakeProvider {
    pub fn push(&self, r: Reply) {
        self.chat_replies.lock().push_back(r);
    }

    pub fn chat_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .iter()
            .filter(|r| r.uri.contains("/responses"))
            .map(|r| r.body.clone())
            .collect()
    }

    pub fn count(&self, method: &str, needle: &str) -> usize {
        self.requests
            .lock()
            .iter()
            .filter(|r| r.method == method && r.uri.contains(needle))
            .count()
    }
}

#[async_trait]
impl ProviderTransport for FakeProvider {
    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap_or_default();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let uri = parts.uri.to_string();
        let method = parts.method.to_string();
        self.requests.lock().push(Recorded {
            method: method.clone(),
            uri: uri.clone(),
            body: parsed.clone(),
        });
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        if uri.contains("/responses") {
            if parsed.get("stream") == Some(&Value::Bool(false)) {
                let text = self.summary_text.lock().clone();
                return Ok(json_resp(
                    200,
                    &json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}], "usage": {"input_tokens": 40, "output_tokens": 12}}),
                ));
            }
            let reply = self.chat_replies.lock().pop_front().unwrap_or_else(ok_reply);
            let delay = *self.delay_ms.lock();
            return Ok(match reply {
                Reply::Events(f) => http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(sse_body(f, false, delay))
                    .unwrap(),
                Reply::Hang(f) => http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(sse_body(f, true, delay))
                    .unwrap(),
                Reply::Status(code, v, retry) => {
                    let mut b = http::Response::builder().status(code).header("content-type", "application/json");
                    if let Some(r) = retry {
                        b = b.header("retry-after", r.to_string());
                    }
                    b.body(Body::Bytes(Bytes::from(v.to_string()))).unwrap()
                }
            });
        }
        if method == "DELETE" {
            let st = *self.delete_status.lock();
            return Ok(json_resp(st, &json!({"deleted": st < 300})));
        }
        if uri.contains("/vector_stores") {
            let status = self.vs_status.lock().clone();
            if method == "POST" && !uri.contains("/files") {
                return Ok(json_resp(200, &json!({"id": format!("vs_testvectorstore{n:06}")})));
            }
            return Ok(json_resp(200, &json!({"status": status})));
        }
        if uri.contains("/files") {
            let st = *self.upload_status.lock();
            if st >= 300 {
                return Ok(json_resp(st, &json!({"error": {"message": "upload failed"}})));
            }
            return Ok(json_resp(200, &json!({"id": format!("file-testfileid{n:06}")})));
        }
        Ok(json_resp(404, &json!({"error": {"message": "unknown"}})))
    }
}

// ── catalog ──────────────────────────────────────────────────────────────

pub fn model(id: &str, tier: &str, default: bool, vision: bool) -> Value {
    json!({
        "id": id, "provider_model_id": format!("{id}-provider"), "display_name": id.to_uppercase(),
        "description": format!("{id} model"), "provider_id": "p", "tier": tier, "enabled": true,
        "system_prompt": "You are a test assistant.",
        "multimodal_capabilities": if vision { json!(["VISION_INPUT"]) } else { json!([]) },
        "context_window": 128000, "max_output_tokens": 1000, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000, "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "general_config": {"max_file_size_mb": 25, "tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": default, "sort_order": 0}
    })
}

pub fn default_catalog() -> Vec<Value> {
    let mut tiny = model("tiny", "standard", false, false);
    tiny["context_window"] = json!(1000);
    tiny["max_output_tokens"] = json!(200);
    tiny["max_input_tokens"] = json!(800);
    let mut disabled = model("off", "premium", false, true);
    disabled["enabled"] = json!(false);
    vec![
        model("prem", "premium", true, true),
        model("std", "standard", false, true),
        model("novision", "standard", false, false),
        tiny,
        disabled,
    ]
}

pub fn policy_cfg(catalog: Vec<Value>) -> StaticModelPolicyConfig {
    StaticModelPolicyConfig {
        model_catalog: catalog
            .into_iter()
            .map(|v| serde_json::from_value::<ModelCatalogEntry>(v).unwrap())
            .collect(),
        default_standard_limits: TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000,
        },
        default_premium_limits: TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 500_000_000,
        },
        ..StaticModelPolicyConfig::default()
    }
}

pub fn base_config() -> MiniChatConfig {
    let mut cfg: MiniChatConfig = serde_json::from_value(json!({
        "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
        "providers": {"p": {"kind": "openai_responses", "host": "mock.local", "storage_kind": "openai"}},
        "streaming": {"sse_ping_interval_seconds": 5},
        "orphan_watchdog": {"timeout_secs": 90},
        "thread_summary_worker": {"summary_model_id": "std"}
    }))
    .unwrap();
    cfg.expand_and_normalize().unwrap();
    cfg.validate().unwrap();
    cfg
}

// ── harness ──────────────────────────────────────────────────────────────

pub struct Harness {
    pub svc: Arc<AppServices>,
    pub provider: Arc<FakeProvider>,
    pub policy: Arc<RecordingPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub pdp: Arc<TenantPdp>,
    pub db: Db,
    pub handle: Option<OutboxHandle>,
    pub path: std::path::PathBuf,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(base_config(), policy_cfg(default_catalog())).await
    }

    pub async fn with(cfg: MiniChatConfig, pcfg: StaticModelPolicyConfig) -> Self {
        let path = std::env::temp_dir().join(format!("mini_chat_test_{}.db", Uuid::new_v4().simple()));
        let dsn = format!("sqlite://{}?mode=rwc&wal=true&busy_timeout=10000", path.display());
        let db = connect_db(&dsn, ConnectOpts::default()).await.unwrap();
        let mut migrations = {
            use sea_orm_migration::MigratorTrait;
            mini_chat::infra::db::migrations::Migrator::migrations()
        };
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations)
            .await
            .unwrap();
        let provider = Arc::new(FakeProvider::default());
        let policy = Arc::new(RecordingPolicy {
            inner: Mutex::new(Arc::new(StaticModelPolicy::new(&pcfg))),
            published: Mutex::new(Vec::new()),
        });
        let audit = Arc::new(RecordingAudit::default());
        let pdp = Arc::new(TenantPdp {
            mode: Mutex::new(PdpMode::Allow),
        });
        let cfg = Arc::new(cfg);
        let svc = AppServices::new(
            Arc::clone(&cfg),
            db.clone(),
            Authz::new(PolicyEnforcer::new(pdp.clone())),
            Arc::new(FixedPolicySource(policy.clone())),
            provider.clone(),
            Arc::new(OutboxEnqueuer::new(cfg.outbox.clone())),
        );
        let handle = start_pipeline(db.clone(), &svc, Arc::new(FixedAudit(audit.clone())))
            .await
            .unwrap();
        Self {
            svc,
            provider,
            policy,
            audit,
            pdp,
            db,
            handle: Some(handle),
            path,
        }
    }

    pub fn set_policy(&self, pcfg: &StaticModelPolicyConfig) {
        *self.policy.inner.lock() = Arc::new(StaticModelPolicy::new(pcfg));
    }

    pub async fn shutdown(mut self) {
        self.svc.shutdown.cancel();
        if let Some(h) = self.handle.take() {
            h.stop().await;
        }
        let _ = std::fs::remove_file(&self.path);
    }

    /// Raw SQL query helper (assertions only).
    pub async fn query(&self, sql: &str) -> Vec<sea_orm::QueryResult> {
        use sea_orm::{ConnectionTrait, Database};
        let conn = Database::connect(format!("sqlite://{}", self.path.display())).await.unwrap();
        conn.query_all_raw(sea_orm::Statement::from_string(sea_orm::DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
    }

    pub async fn exec(&self, sql: &str) {
        use sea_orm::{ConnectionTrait, Database};
        let conn = Database::connect(format!("sqlite://{}", self.path.display())).await.unwrap();
        conn.execute_unprepared(sql).await.unwrap();
    }

    pub async fn scalar_i64(&self, sql: &str) -> i64 {
        let rows = self.query(sql).await;
        rows.first().map(|r| r.try_get_by_index::<i64>(0).unwrap()).unwrap_or(0)
    }

    pub async fn scalar_str(&self, sql: &str) -> Option<String> {
        let rows = self.query(sql).await;
        rows.first().and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
    }

    /// Waits until a condition on the DB holds (outbox handlers are async).
    pub async fn eventually<F, Fut>(&self, mut f: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..200 {
            if f().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not met in time");
    }
}

pub fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

pub fn user() -> SecurityContext {
    ctx(Uuid::new_v4(), Uuid::new_v4())
}

/// Collects every event of a stream start (drives the stream to completion).
pub async fn collect(start: StreamStart) -> Vec<StreamEvent> {
    match start {
        StreamStart::Replay(v) => v,
        StreamStart::Live { mut events, cancel } => {
            let _g = cancel.drop_guard();
            let mut out = Vec::new();
            while let Some(e) = events.recv().await {
                let t = e.is_terminal();
                out.push(e);
                if t {
                    break;
                }
            }
            out
        }
    }
}

pub fn names(evs: &[StreamEvent]) -> Vec<&'static str> {
    evs.iter()
        .map(|e| match e {
            StreamEvent::Started { .. } => "stream_started",
            StreamEvent::Ping => "ping",
            StreamEvent::Delta { .. } => "delta",
            StreamEvent::Tool { .. } => "tool",
            StreamEvent::Citations(_) => "citations",
            StreamEvent::Done(_) => "done",
            StreamEvent::Error { .. } => "error",
        })
        .collect()
}

pub fn request_id_of(evs: &[StreamEvent]) -> Uuid {
    match &evs[0] {
        StreamEvent::Started { request_id, .. } => *request_id,
        other => panic!("first event is not stream_started: {other:?}"),
    }
}

/// Waits for the turn to leave `running`.
pub async fn wait_terminal(h: &Harness, request_id: Uuid) {
    let rid = hex_uuid(request_id);
    h.eventually(|| async {
        h.scalar_str(&format!(
            "SELECT state FROM chat_turns WHERE hex(request_id) = '{rid}'"
        ))
        .await
        .is_some_and(|s| s != "running")
    })
    .await;
}

/// Upper-case hex of a UUID (SQLite stores UUIDs as 16-byte blobs).
pub fn hex_uuid(u: Uuid) -> String {
    u.as_simple().to_string().to_uppercase()
}

pub fn drain_stream(_s: impl futures::Stream + Unpin) {}

pub async fn next_n(rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>, n: usize) -> Vec<StreamEvent> {
    let mut v = Vec::new();
    while v.len() < n {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(e)) => v.push(e),
            _ => break,
        }
    }
    v
}

pub async fn body_text(b: axum::body::Body) -> String {
    let bytes = http_body_util::BodyExt::collect(b).await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn _unused() -> impl futures::Stream<Item = u8> {
    futures::stream::iter(vec![1u8]).map(|x| x)
}

// ── HTTP helpers ─────────────────────────────────────────────────────────

pub struct HttpResp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub text: String,
}

impl HttpResp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }

    /// Parses an SSE body into `(event, data)` pairs.
    pub fn sse(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for block in self.text.split("\n\n") {
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    name = v.trim().to_owned();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push_str(v.trim_start());
                }
            }
            if !name.is_empty() {
                out.push((name, serde_json::from_str(&data).unwrap_or(Value::Null)));
            }
        }
        out
    }
}

impl Harness {
    pub fn router(&self) -> axum::Router {
        let openapi = toolkit::api::OpenApiRegistryImpl::new();
        mini_chat::api::rest::routes::register_routes(axum::Router::new(), &openapi, self.svc.clone(), "/mini-chat")
    }

    pub async fn call(&self, c: &SecurityContext, method: &str, uri: &str, body: Option<Value>) -> HttpResp {
        let mut b = http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                axum::body::Body::from(v.to_string())
            }
            None => axum::body::Body::empty(),
        };
        self.raw(c, b.body(body).unwrap()).await
    }

    pub async fn raw(&self, c: &SecurityContext, mut req: http::Request<axum::body::Body>) -> HttpResp {
        use tower::ServiceExt;
        req.extensions_mut().insert(c.clone());
        let resp = self.router().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let text = body_text(resp.into_body()).await;
        HttpResp { status, headers, text }
    }

    /// Multipart upload helper.
    pub async fn upload(&self, c: &SecurityContext, chat: Uuid, filename: &str, content_type: &str, data: &[u8]) -> HttpResp {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        self.raw(c, req).await
    }

    pub async fn create_chat(&self, c: &SecurityContext, body: Value) -> Uuid {
        let r = self.call(c, "POST", "/mini-chat/v1/chats", Some(body)).await;
        assert_eq!(r.status, 201, "{}", r.text);
        Uuid::parse_str(r.json()["id"].as_str().unwrap()).unwrap()
    }

    pub async fn send(&self, c: &SecurityContext, chat: Uuid, body: Value) -> HttpResp {
        self.call(c, "POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), Some(body)).await
    }
}

pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 10, 10, 255]));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

pub fn reason(v: &Value) -> String {
    let c = &v["context"];
    c["reason"]
        .as_str()
        .or_else(|| c["field_violations"][0]["reason"].as_str())
        .or_else(|| c["violations"][0]["type"].as_str())
        .unwrap_or_default()
        .to_owned()
}
