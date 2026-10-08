//! Integration-test harness: temp SQLite + migrations, real outbox, scripted fake provider,
//! recording plugins, switchable PDP mock, and REST/SSE helpers.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::let_underscore_must_use,
    clippy::unused_self,
    clippy::useless_let_if_seq
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::StreamExt;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::service::{Core, CoreDeps};
use mini_chat::infra::llm::{ProviderRequest, ProviderResponse, ProviderTransport, TransportError};
use mini_chat::infra::outbox_handlers::start_outbox;
use mini_chat::infra::plugin_gateway::{AuditGateway, PolicyGateway};
use mini_chat::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError,
    TurnAuditEvent, TurnMutationAuditEvent, UsageEvent, UserLimits,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------- PDP mock

pub const PDP_ALLOW: u8 = 0;
pub const PDP_DENY: u8 = 1;
pub const PDP_FAIL: u8 = 2;

pub struct MockAuthz {
    pub mode: AtomicU8,
    pub calls: AtomicUsize,
}

#[async_trait]
impl AuthZResolverApi for MockAuthz {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            PDP_DENY => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            PDP_FAIL => Err(CanonicalError::service_unavailable().create()),
            _ => {
                let tenant = req
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .unwrap();
                let constraints = if req.context.require_constraints {
                    vec![Constraint {
                        predicates: vec![Predicate::Eq(EqPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            tenant,
                        ))],
                    }]
                } else {
                    Vec::new()
                };
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext {
                        constraints,
                        ..Default::default()
                    },
                })
            }
        }
    }
}

// ---------------------------------------------------------------- plugins

pub struct RecordingPolicy {
    pub inner: Mutex<StaticModelPolicyService>,
    pub published: Mutex<Vec<UsageEvent>>,
    pub fail_snapshot: std::sync::atomic::AtomicBool,
}

impl RecordingPolicy {
    pub fn set_config(&self, cfg: &StaticModelPolicyConfig) {
        *self.inner.lock().unwrap() = StaticModelPolicyService::new(cfg);
    }

    pub fn published(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for RecordingPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        if self.fail_snapshot.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Unavailable("down".into()));
        }
        let s = self.inner.lock().unwrap().clone();
        s.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        v: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        let s = self.inner.lock().unwrap().clone();
        s.get_policy_snapshot(user_id, v).await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        v: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        let s = self.inner.lock().unwrap().clone();
        s.get_user_limits(user_id, v).await
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().unwrap().push(payload);
        Ok(())
    }
}

#[derive(Default)]
pub struct RecordingAudit {
    pub events: Mutex<Vec<AuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit_turn_audit(&self, event: TurnAuditEvent) -> Result<(), AuditPluginError> {
        self.events.lock().unwrap().push(AuditEvent::Turn(event));
        Ok(())
    }

    async fn emit_turn_mutation_audit(
        &self,
        event: TurnMutationAuditEvent,
    ) -> Result<(), AuditPluginError> {
        self.events
            .lock()
            .unwrap()
            .push(AuditEvent::Mutation(event));
        Ok(())
    }
}

// ---------------------------------------------------------------- fake provider

/// One scripted streaming answer.
#[derive(Clone)]
pub enum Script {
    /// Text deltas then `response.completed` with usage (and optional extra frames before completion).
    Ok {
        parts: Vec<String>,
        usage: (i64, i64),
        before_done: Vec<(String, Value)>,
        output: Option<Value>,
    },
    /// Deltas then `response.failed`.
    Failed {
        parts: Vec<String>,
        message: String,
        usage: Option<(i64, i64)>,
    },
    /// Non-success HTTP response.
    Http {
        status: u16,
        body: Value,
        retry_after: Option<u64>,
    },
    /// Emits `parts`, then waits on the gate before completing.
    Gated {
        parts: Vec<String>,
        gate: Arc<Notify>,
        usage: (i64, i64),
    },
    /// Emits `parts` then ends without a terminal event.
    Truncated { parts: Vec<String> },
}

impl Script {
    pub fn ok(text: &str) -> Self {
        Self::Ok {
            parts: vec![text.to_owned()],
            usage: (100, 20),
            before_done: vec![],
            output: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub json: Option<Value>,
    pub raw_len: usize,
    pub content_type: Option<String>,
}

pub struct FakeProvider {
    pub requests: Mutex<Vec<Recorded>>,
    pub scripts: Mutex<VecDeque<Script>>,
    pub indexing_status: Mutex<String>,
    pub status_after_polls: Mutex<Option<(usize, String)>>,
    pub polls: AtomicUsize,
    pub fail_file_upload: std::sync::atomic::AtomicBool,
    pub delete_status: Mutex<u16>,
    pub summary_text: Mutex<String>,
    pub fail_summary: std::sync::atomic::AtomicBool,
    counter: AtomicUsize,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(VecDeque::new()),
            indexing_status: Mutex::new("completed".to_owned()),
            status_after_polls: Mutex::new(None),
            polls: AtomicUsize::new(0),
            fail_file_upload: std::sync::atomic::AtomicBool::new(false),
            delete_status: Mutex::new(200),
            summary_text: Mutex::new(
                "<analysis>a</analysis><summary>The user said hello.</summary>".to_owned(),
            ),
            fail_summary: std::sync::atomic::AtomicBool::new(false),
            counter: AtomicUsize::new(0),
        }
    }
}

fn frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn json_response(status: u16, v: &Value) -> ProviderResponse {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    let b = Bytes::from(v.to_string());
    ProviderResponse {
        status,
        headers,
        gateway: false,
        body: futures::stream::iter(vec![Ok(b)]).boxed(),
    }
}

impl FakeProvider {
    pub fn push(&self, s: Script) {
        self.scripts.lock().unwrap().push_back(s);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn chat_requests(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| {
                r.path.contains("/responses")
                    && r.json.as_ref().is_some_and(|j| j["stream"] == true)
            })
            .filter_map(|r| r.json)
            .collect()
    }

    pub fn summary_requests(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| {
                r.path.contains("/responses")
                    && r.json.as_ref().is_some_and(|j| j["stream"] == false)
            })
            .filter_map(|r| r.json)
            .collect()
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}{n:024}")
    }

    fn stream_response(&self, script: Script) -> ProviderResponse {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
        tokio::spawn(async move {
            let send_parts =
                |parts: Vec<String>, tx: tokio::sync::mpsc::Sender<Result<Bytes, String>>| async move {
                    for p in parts {
                        let _ = tx
                            .send(Ok(frame(
                                "response.output_text.delta",
                                &json!({"type": "response.output_text.delta", "delta": p}),
                            )))
                            .await;
                    }
                };
            let _ = tx
                .send(Ok(frame(
                    "response.created",
                    &json!({"type": "response.created", "response": {"id": "resp_test"}}),
                )))
                .await;
            match script {
                Script::Ok {
                    parts,
                    usage,
                    before_done,
                    output,
                } => {
                    send_parts(parts, tx.clone()).await;
                    for (e, d) in before_done {
                        let _ = tx.send(Ok(frame(&e, &d))).await;
                    }
                    let mut resp = json!({"id": "resp_abcdef", "usage": {"input_tokens": usage.0, "output_tokens": usage.1}});
                    if let Some(o) = output {
                        resp["output"] = o;
                    }
                    let _ = tx
                        .send(Ok(frame(
                            "response.completed",
                            &json!({"type": "response.completed", "response": resp}),
                        )))
                        .await;
                }
                Script::Failed {
                    parts,
                    message,
                    usage,
                } => {
                    send_parts(parts, tx.clone()).await;
                    let mut resp = json!({"error": {"code": "server_error", "message": message}});
                    if let Some((i, o)) = usage {
                        resp["usage"] = json!({"input_tokens": i, "output_tokens": o});
                    }
                    let _ = tx
                        .send(Ok(frame(
                            "response.failed",
                            &json!({"type": "response.failed", "response": resp}),
                        )))
                        .await;
                }
                Script::Gated { parts, gate, usage } => {
                    send_parts(parts, tx.clone()).await;
                    tokio::select! {
                        () = gate.notified() => {}
                        () = tx.closed() => return,
                    }
                    let resp = json!({"id": "resp_g", "usage": {"input_tokens": usage.0, "output_tokens": usage.1}});
                    let _ = tx
                        .send(Ok(frame(
                            "response.completed",
                            &json!({"type": "response.completed", "response": resp}),
                        )))
                        .await;
                }
                Script::Truncated { parts } => send_parts(parts, tx.clone()).await,
                Script::Http { .. } => {}
            }
        });
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream".parse().unwrap(),
        );
        ProviderResponse {
            status: 200,
            headers,
            gateway: false,
            body: tokio_stream_from(rx),
        }
    }
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
) -> futures::stream::BoxStream<'static, Result<Bytes, String>> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
}

#[async_trait]
impl ProviderTransport for FakeProvider {
    async fn send(
        &self,
        _ctx: SecurityContext,
        req: ProviderRequest,
    ) -> Result<ProviderResponse, TransportError> {
        let json: Option<Value> = serde_json::from_slice(&req.body).ok();
        self.requests.lock().unwrap().push(Recorded {
            method: req.method.to_string(),
            path: req.path.clone(),
            json: json.clone(),
            raw_len: req.body.len(),
            content_type: req.content_type.clone(),
        });
        let path = req.path.split('?').next().unwrap_or("").to_owned();
        let m = req.method.as_str();
        if path.ends_with("/responses") {
            if json.as_ref().is_some_and(|j| j["stream"] == false) {
                if self.fail_summary.load(Ordering::SeqCst) {
                    return Ok(json_response(
                        500,
                        &json!({"error": {"message": "summary failed"}}),
                    ));
                }
                let text = self.summary_text.lock().unwrap().clone();
                return Ok(json_response(
                    200,
                    &json!({"id": "resp_s", "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
                            "usage": {"input_tokens": 50, "output_tokens": 30}}),
                ));
            }
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Script::ok("Hello world"));
            if let Script::Http {
                status,
                body,
                retry_after,
            } = &script
            {
                let mut r = json_response(*status, body);
                if let Some(s) = retry_after {
                    r.headers
                        .insert(http::header::RETRY_AFTER, s.to_string().parse().unwrap());
                }
                return Ok(r);
            }
            return Ok(self.stream_response(script));
        }
        if path.ends_with("/files") && m == "POST" && !path.contains("vector_stores") {
            if self.fail_file_upload.load(Ordering::SeqCst) {
                return Ok(json_response(
                    500,
                    &json!({"error": {"message": "upload failed"}}),
                ));
            }
            return Ok(json_response(200, &json!({"id": self.next_id("file-")})));
        }
        if path.contains("/vector_stores") && m == "POST" && path.ends_with("/files") {
            let s = self.indexing_status.lock().unwrap().clone();
            return Ok(json_response(200, &json!({"id": "vsf", "status": s})));
        }
        if path.ends_with("/vector_stores") && m == "POST" {
            return Ok(json_response(200, &json!({"id": self.next_id("vs_")})));
        }
        if path.contains("/vector_stores/") && m == "GET" {
            let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
            let mut s = self.indexing_status.lock().unwrap().clone();
            if let Some((after, status)) = self.status_after_polls.lock().unwrap().clone()
                && n >= after
            {
                s = status;
            }
            return Ok(json_response(200, &json!({"status": s})));
        }
        if m == "DELETE" {
            let st = *self.delete_status.lock().unwrap();
            return Ok(json_response(st, &json!({"deleted": st < 300})));
        }
        Ok(json_response(
            404,
            &json!({"error": {"message": "unknown path"}}),
        ))
    }
}

// ---------------------------------------------------------------- catalog

pub fn model(
    id: &str,
    tier: &str,
    enabled: bool,
    vision: bool,
    tools: (bool, bool, bool),
) -> Value {
    json!({
        "id": id, "provider_model_id": format!("prov-{id}"), "display_name": format!("Model {id}"),
        "description": if id == "std" { "" } else { "A test model" },
        "provider_id": "mock", "provider_display_name": "Mock", "tier": tier, "enabled": enabled,
        "multimodal_capabilities": if vision { json!(["VISION_INPUT"]) } else { json!([]) },
        "context_window": 128_000, "max_output_tokens": 1000, "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_000_000, "output_tokens_credit_multiplier_micro": 2_000_000,
        "multiplier_display": "1x", "system_prompt": format!("You are {id}."),
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 10, "safety_margin_pct": 10,
            "image_token_budget": 100, "tool_surcharge_tokens": 50, "web_search_surcharge_tokens": 50,
            "code_interpreter_surcharge_tokens": 50, "minimal_generation_floor": 5},
        "max_num_results": 4, "web_search_context_size": "low", "max_tool_calls": 2,
        "general_config": {"type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": {"temperature": 0.5, "stop": []}, "features": {"streaming": true},
            "tool_support": {"web_search": tools.0, "file_search": tools.1, "code_interpreter": tools.2},
            "supported_endpoints": {"responses": true}},
        "preference": {"is_default": id == "prem", "sort_order": 0}
    })
}

pub fn default_policy() -> Value {
    json!({
        "model_catalog": [
            model("prem", "premium", true, true, (true, true, true)),
            model("std", "standard", true, false, (false, true, false)),
            model("off", "standard", false, false, (false, false, false)),
        ],
        "default_standard_limits": {"limit_daily_credits_micro": 100_000_000, "limit_monthly_credits_micro": 1_000_000_000},
        "default_premium_limits": {"limit_daily_credits_micro": 50_000_000, "limit_monthly_credits_micro": 500_000_000}
    })
}

pub fn default_config() -> Value {
    json!({
        "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
        "providers": {"mock": {"kind": "openai_responses", "host": "mock.local", "storage_kind": "openai", "upstream_alias": "mock"}},
        "thread_summary_worker": {"summary_model_id": "std"},
        "orphan_watchdog": {"timeout_secs": 90}
    })
}

// ---------------------------------------------------------------- harness

pub struct Harness {
    pub core: Arc<Core>,
    pub router: Router,
    pub provider: Arc<FakeProvider>,
    pub policy: Arc<RecordingPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub authz: Arc<MockAuthz>,
    pub db: Db,
    pub tenant: Uuid,
    pub user: Uuid,
    _outbox: Option<OutboxHandle>,
    _dir: tempfile::TempDir,
}

pub struct Opts {
    pub config: Value,
    pub policy: Value,
    pub timings: Option<mini_chat::domain::service::attachments::UploadTimings>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            config: default_config(),
            policy: default_policy(),
            timings: None,
        }
    }
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(Opts::default()).await
    }

    pub async fn with(opts: Opts) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mini_chat.db");
        let dsn = format!(
            "sqlite://{}?mode=rwc&wal=true&busy_timeout=10000",
            path.display()
        );
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(8),
                create_sqlite_dirs: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        toolkit_db::migration_runner::run_migrations_for_testing(
            &db,
            mini_chat::gear::all_migrations(),
        )
        .await
        .unwrap();
        let cfg: MiniChatConfig = serde_json::from_value(opts.config).unwrap();
        cfg.validate().unwrap();
        let pcfg: StaticModelPolicyConfig = serde_json::from_value(opts.policy).unwrap();
        pcfg.validate().unwrap();
        let policy = Arc::new(RecordingPolicy {
            inner: Mutex::new(StaticModelPolicyService::new(&pcfg)),
            published: Mutex::new(Vec::new()),
            fail_snapshot: std::sync::atomic::AtomicBool::new(false),
        });
        let audit = Arc::new(RecordingAudit::default());
        let authz = Arc::new(MockAuthz {
            mode: AtomicU8::new(PDP_ALLOW),
            calls: AtomicUsize::new(0),
        });
        let provider = Arc::new(FakeProvider::default());
        let mut core = Core::new(CoreDeps {
            cfg,
            db: Arc::new(DBProvider::new(db.clone())),
            enforcer: PolicyEnforcer::new(authz.clone()),
            policy: Arc::new(PolicyGateway::fixed(policy.clone())),
            audit: Arc::new(AuditGateway::fixed(Some(audit.clone()))),
            transport: provider.clone(),
        });
        if let Some(t) = opts.timings {
            Arc::get_mut(&mut core).unwrap().upload_timings = t;
        } else {
            Arc::get_mut(&mut core).unwrap().upload_timings =
                mini_chat::domain::service::attachments::UploadTimings {
                    sync_deadline: Duration::from_millis(600),
                    sync_max_backoff: Duration::from_millis(50),
                    initial_backoff: Duration::from_millis(20),
                    background_total: Duration::from_secs(3),
                    background_round: Duration::from_millis(300),
                    background_max_backoff: Duration::from_millis(50),
                    loser_polls: 5,
                    stale_placeholder: Duration::from_secs(120),
                };
        }
        let outbox = start_outbox(db.clone(), &core).await.unwrap();
        let router = mini_chat::api::routes::register_routes(
            Router::new(),
            &OpenApiRegistryImpl::new(),
            core.clone(),
        );
        Self {
            core,
            router,
            provider,
            policy,
            audit,
            authz,
            db,
            tenant: Uuid::new_v4(),
            user: Uuid::new_v4(),
            _outbox: Some(outbox),
            _dir: dir,
        }
    }

    pub fn ctx(&self) -> SecurityContext {
        ctx_for(self.tenant, self.user)
    }

    pub async fn req(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, http::HeaderMap) {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let body = body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap()));
        let mut r = b.body(body).unwrap();
        r.extensions_mut().insert(ctx.clone());
        let resp = self.router.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    pub async fn raw(&self, ctx: &SecurityContext, r: Request<Body>) -> (StatusCode, Bytes) {
        let mut r = r;
        r.extensions_mut().insert(ctx.clone());
        let resp = self.router.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        (
            status,
            axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
                .await
                .unwrap(),
        )
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let (s, v, _) = self.req(&self.ctx(), "GET", uri, None).await;
        (s, v)
    }

    pub async fn create_chat(&self) -> Uuid {
        self.create_chat_as(&self.ctx(), json!({})).await
    }

    pub async fn create_chat_as(&self, ctx: &SecurityContext, body: Value) -> Uuid {
        let (s, v, _) = self
            .req(ctx, "POST", "/mini-chat/v1/chats", Some(body))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        Uuid::parse_str(v["id"].as_str().unwrap()).unwrap()
    }

    /// Sends an SSE request and collects all events (or the JSON error).
    pub async fn sse(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> SseResult {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let body = body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap()));
        let mut r = b.body(body).unwrap();
        r.extensions_mut().insert(ctx.clone());
        let resp = self.router.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = tokio::time::timeout(
            Duration::from_secs(20),
            axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024),
        )
        .await
        .expect("sse body timed out")
        .unwrap();
        if !ct.starts_with("text/event-stream") {
            return SseResult {
                status,
                events: Vec::new(),
                error: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            };
        }
        SseResult {
            status,
            events: parse_sse(&bytes),
            error: Value::Null,
        }
    }

    pub async fn send(&self, chat: Uuid, content: &str) -> SseResult {
        self.sse(
            &self.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            Some(json!({"content": content})),
        )
        .await
    }

    pub async fn send_body(&self, chat: Uuid, body: Value) -> SseResult {
        self.sse(
            &self.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            Some(body),
        )
        .await
    }

    pub async fn upload(
        &self,
        ctx: &SecurityContext,
        chat: Uuid,
        filename: &str,
        ct: &str,
        data: &[u8],
    ) -> (StatusCode, Value) {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {ct}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let r = Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let (s, b) = self.raw(ctx, r).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    pub async fn upload_ok(&self, chat: Uuid, filename: &str, ct: &str, data: &[u8]) -> Uuid {
        let (s, v) = self.upload(&self.ctx(), chat, filename, ct, data).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        Uuid::parse_str(v["id"].as_str().unwrap()).unwrap()
    }

    pub async fn messages(&self, chat: Uuid) -> Vec<Value> {
        let (s, v) = self
            .get(&format!("/mini-chat/v1/chats/{chat}/messages?limit=100"))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["items"].as_array().unwrap().clone()
    }

    /// Polls until `f` returns true (outbox delivery is asynchronous).
    pub async fn eventually<F: Fn() -> bool>(&self, f: F) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        f()
    }

    pub fn audit_events(&self) -> Vec<AuditEvent> {
        self.audit.events.lock().unwrap().clone()
    }
}

pub fn ctx_for(tenant: Uuid, user: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

#[derive(Debug, Clone)]
pub struct SseResult {
    pub status: StatusCode,
    pub events: Vec<(String, Value)>,
    pub error: Value,
}

impl SseResult {
    pub fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn first(&self, name: &str) -> Option<&Value> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn text(&self) -> String {
        self.events
            .iter()
            .filter(|(n, v)| n == "delta" && v["type"] == "text")
            .map(|(_, v)| v["content"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    pub fn request_id(&self) -> Uuid {
        Uuid::parse_str(
            self.first("stream_started").unwrap()["request_id"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    }
}

pub fn parse_sse(bytes: &[u8]) -> Vec<(String, Value)> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                event = Some(e.trim().to_owned());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            }
        }
        if let Some(e) = event {
            out.push((e, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    out
}

/// Asserts a canonical Problem with a field-violation reason.
pub fn assert_field_reason(v: &Value, field: &str, reason: &str) {
    let fv = v["context"]["field_violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no field_violations in {v}"));
    assert!(
        fv.iter()
            .any(|f| f["field"] == field && f["reason"] == reason),
        "expected {field}/{reason} in {v}"
    );
}

pub fn assert_reason(v: &Value, reason: &str) {
    assert_eq!(v["context"]["reason"], reason, "{v}");
}

pub fn assert_violation(v: &Value, subject: &str) {
    let vs = v["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no violations in {v}"));
    assert!(
        vs.iter().any(|x| x["subject"] == subject),
        "expected violation {subject} in {v}"
    );
}

/// Tiny valid PNG of the given size.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgb([200, 10, 10]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// Raw SQL helpers through the gear's secure ORM (allow-all scope).
pub mod db {
    use super::*;
    use mini_chat::infra::db::entities::{attachment, chat, message, quota_usage, turn};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use toolkit_db::secure::SecureEntityExt;
    use toolkit_security::AccessScope;

    pub async fn turns(h: &Harness, chat_id: Uuid) -> Vec<turn::Model> {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        turn::Entity::find()
            .filter(turn::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&c)
            .await
            .unwrap()
    }

    pub async fn messages(h: &Harness, chat_id: Uuid) -> Vec<message::Model> {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        message::Entity::find()
            .filter(message::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&c)
            .await
            .unwrap()
    }

    pub async fn quota_rows(h: &Harness) -> Vec<quota_usage::Model> {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        quota_usage::Entity::find()
            .filter(quota_usage::Column::UserId.eq(h.user))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&c)
            .await
            .unwrap()
    }

    pub async fn attachment(h: &Harness, id: Uuid) -> attachment::Model {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        attachment::Entity::find()
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&c)
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn chat(h: &Harness, id: Uuid) -> chat::Model {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        chat::Entity::find()
            .filter(chat::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&c)
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn set_turn_requester(h: &Harness, request_id: Uuid, user: Uuid) {
        use sea_orm::sea_query::Expr;
        use toolkit_db::secure::SecureUpdateExt;
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        turn::Entity::update_many()
            .col_expr(turn::Column::RequesterUserId, Expr::value(Some(user)))
            .filter(turn::Column::RequestId.eq(request_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&c)
            .await
            .unwrap();
    }

    pub async fn set_turn_started_back(h: &Harness, request_id: Uuid, secs: i64) {
        use sea_orm::sea_query::Expr;
        use toolkit_db::secure::SecureUpdateExt;
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
        turn::Entity::update_many()
            .col_expr(turn::Column::StartedAt, Expr::value(t))
            .col_expr(turn::Column::LastProgressAt, Expr::value(Some(t)))
            .filter(turn::Column::RequestId.eq(request_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&c)
            .await
            .unwrap();
    }

    pub async fn set_attachment_updated_back(h: &Harness, id: Uuid, secs: i64) {
        use sea_orm::sea_query::Expr;
        use toolkit_db::secure::SecureUpdateExt;
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
        attachment::Entity::update_many()
            .col_expr(attachment::Column::UpdatedAt, Expr::value(t))
            .filter(attachment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&c)
            .await
            .unwrap();
    }

    pub async fn chat_attachments(h: &Harness, chat_id: Uuid) -> Vec<attachment::Model> {
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        attachment::Entity::find()
            .filter(attachment::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&c)
            .await
            .unwrap()
    }

    pub async fn summary(
        h: &Harness,
        chat_id: Uuid,
    ) -> Option<mini_chat::infra::db::entities::thread_summary::Model> {
        use mini_chat::infra::db::entities::thread_summary;
        let p = DBProvider::<toolkit_db::DbError>::new(h.db.clone());
        let c = p.conn().unwrap();
        thread_summary::Entity::find()
            .filter(thread_summary::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&c)
            .await
            .unwrap()
    }

    pub fn total_daily(rows: &[quota_usage::Model]) -> &quota_usage::Model {
        rows.iter()
            .find(|r| r.bucket == "total" && r.period_type == "daily")
            .unwrap()
    }

    pub fn premium_daily(rows: &[quota_usage::Model]) -> Option<&quota_usage::Model> {
        rows.iter()
            .find(|r| r.bucket == "tier:premium" && r.period_type == "daily")
    }
}

impl Harness {
    pub fn deletes(&self) -> Vec<String> {
        self.provider
            .requests()
            .into_iter()
            .filter(|r| r.method == "DELETE")
            .map(|r| r.path)
            .collect()
    }

    /// Replaces the static policy (catalog, kill switches, limits).
    pub fn set_policy(&self, v: Value) {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(v).unwrap();
        self.policy.set_config(&cfg);
    }
}

/// An SSE response read incrementally.
pub struct OpenStream {
    pub status: StatusCode,
    pub body: axum::body::BodyDataStream,
    pub buf: Vec<u8>,
    pub events: Vec<(String, Value)>,
}

impl OpenStream {
    /// Reads frames until an event named `name` arrives (or the stream ends / times out).
    pub async fn until(&mut self, name: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if self.events.iter().any(|(n, _)| n == name) {
                return true;
            }
            let next = tokio::time::timeout_at(deadline, self.body.next()).await;
            match next {
                Ok(Some(Ok(chunk))) => {
                    self.buf.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&self.buf).replace("\r\n", "\n");
                    if let Some(idx) = text.rfind("\n\n") {
                        let (complete, rest) = text.split_at(idx + 2);
                        self.events.extend(parse_sse(complete.as_bytes()));
                        self.buf = rest.as_bytes().to_vec();
                    }
                }
                _ => return false,
            }
        }
    }

    pub fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).collect()
    }
}

impl Harness {
    pub async fn open(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> OpenStream {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let body = body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap()));
        let mut r = b.body(body).unwrap();
        r.extensions_mut().insert(ctx.clone());
        let resp = self.router.clone().oneshot(r).await.unwrap();
        OpenStream {
            status: resp.status(),
            body: resp.into_body().into_data_stream(),
            buf: Vec::new(),
            events: Vec::new(),
        }
    }

    pub async fn open_send(&self, chat: Uuid, body: Value) -> OpenStream {
        self.open(
            &self.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            Some(body),
        )
        .await
    }

    /// Waits until the turn reaches a terminal state in the DB.
    pub async fn wait_turn_terminal(
        &self,
        chat: Uuid,
    ) -> Vec<mini_chat::infra::db::entities::turn::Model> {
        for _ in 0..400 {
            let t = db::turns(self, chat).await;
            if !t.is_empty() && t.iter().all(|t| t.state != "running") {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        db::turns(self, chat).await
    }

    pub fn published_for(&self, request_id: Uuid) -> Vec<UsageEvent> {
        self.policy
            .published()
            .into_iter()
            .filter(|e| e.request_id == request_id)
            .collect()
    }
}
