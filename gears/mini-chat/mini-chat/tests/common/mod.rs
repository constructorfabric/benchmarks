//! In-process integration harness: SQLite + migrations, mock PDP, fake
//! OpenAI-compatible provider behind `ServiceGatewayClientV1`, test policy
//! and audit ports, the real router and outbox pipeline.
#![allow(
    dead_code,
    clippy::disallowed_methods,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::Body as AxumBody;
use bytes::Bytes;
use http_body_util::BodyExt;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::ports::{AuditDelivery, AuditPort, PolicyPort, UsagePublishError};
use mini_chat::domain::service::MiniChatService;
use mini_chat::infra::llm::{LlmGateway, ProviderResolver};
use mini_chat::infra::outbox::OutboxEnqueuer;
use mini_chat::infra::plugins::static_model_policy::{StaticModelPolicyConfig, StaticModelPolicyService};
use mini_chat_sdk::{AuditEvent, MiniChatModelPolicyPluginClientV1, PolicySnapshot, UsageEvent, UserLimits};
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, ServiceGatewayClientV1, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

pub use mini_chat::infra::db::entities as ent;

pub const TENANT_A: Uuid = Uuid::from_u128(0xaaaa_aaaa_aaaa_aaaa_aaaa_aaaa_aaaa_aaaa);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
pub const USER_1: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
pub const USER_2: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
pub const USER_3: Uuid = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);

/// A caller identity.
#[derive(Debug, Clone, Copy)]
pub struct Who {
    pub user: Uuid,
    pub tenant: Uuid,
}

pub const ALICE: Who = Who { user: USER_1, tenant: TENANT_A };
/// Same tenant, another user.
pub const BOB: Who = Who { user: USER_2, tenant: TENANT_A };
/// Another tenant.
pub const CAROL: Who = Who { user: USER_3, tenant: TENANT_B };

// ------------------------------------------------------------------- PDP

pub const PDP_ALLOW: u8 = 0;
pub const PDP_DENY: u8 = 1;
pub const PDP_FAIL: u8 = 2;

/// Mirrors static-authz: `In(owner_tenant_id, [tenant])`; switchable to deny / failure.
pub struct MockPdp {
    pub mode: AtomicU8,
}

#[async_trait]
impl AuthZResolverApi for MockPdp {
    async fn evaluate(&self, _ctx: PlatformSecurityContext, request: EvaluationRequest) -> Result<EvaluationResponse, CanonicalError> {
        match self.mode.load(Ordering::SeqCst) {
            PDP_DENY => Ok(EvaluationResponse { decision: false, context: EvaluationResponseContext::default() }),
            PDP_FAIL => Err(CanonicalError::service_unavailable().with_detail("pdp down".to_owned()).create()),
            _ => {
                let tenant = request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .expect("tenant");
                let constraints = if request.context.supported_properties.iter().any(|p| p == pep_properties::OWNER_TENANT_ID) {
                    vec![Constraint { predicates: vec![Predicate::In(InPredicate::new(pep_properties::OWNER_TENANT_ID, [tenant]))] }]
                } else {
                    Vec::new()
                };
                Ok(EvaluationResponse { decision: true, context: EvaluationResponseContext { constraints, ..Default::default() } })
            }
        }
    }
}

// --------------------------------------------------------------- policy

/// Policy port backed by the static plugin; swappable per test.
pub struct TestPolicy {
    pub inner: Mutex<Arc<StaticModelPolicyService>>,
    pub published: Mutex<Vec<UsageEvent>>,
    pub publish_failures: AtomicUsize,
}

impl TestPolicy {
    pub fn set(&self, cfg: &StaticModelPolicyConfig) {
        *self.inner.lock().unwrap() = Arc::new(StaticModelPolicyService::from_config(cfg));
    }
    fn svc(&self) -> Arc<StaticModelPolicyService> {
        Arc::clone(&self.inner.lock().unwrap())
    }
}

#[async_trait]
impl PolicyPort for TestPolicy {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        let s = self.svc();
        let v = s.get_current_policy_version(user_id).await.unwrap();
        Ok(Arc::new(s.get_policy_snapshot(user_id, v).await.unwrap()))
    }
    async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<Arc<PolicySnapshot>, DomainError> {
        Ok(Arc::new(self.svc().get_policy_snapshot(user_id, version).await.unwrap()))
    }
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        Ok(self.svc().get_user_limits(user_id, version).await.unwrap())
    }
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError> {
        if self.publish_failures.load(Ordering::SeqCst) > 0 {
            self.publish_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(UsagePublishError::Transient("injected".into()));
        }
        self.published.lock().unwrap().push(event);
        Ok(())
    }
}

pub const AUDIT_OK: u8 = 0;
pub const AUDIT_RETRY: u8 = 1;
pub const AUDIT_REJECT: u8 = 2;
pub const AUDIT_DROP: u8 = 3;

/// Recording audit port with a switchable delivery outcome.
#[derive(Default)]
pub struct TestAudit {
    pub events: Mutex<Vec<AuditEvent>>,
    pub mode: AtomicU8,
}

#[async_trait]
impl AuditPort for TestAudit {
    async fn deliver(&self, event: AuditEvent) -> AuditDelivery {
        match self.mode.load(Ordering::SeqCst) {
            AUDIT_RETRY => AuditDelivery::Retry("injected".into()),
            AUDIT_REJECT => AuditDelivery::Reject("injected".into()),
            AUDIT_DROP => AuditDelivery::Dropped,
            _ => {
                self.events.lock().unwrap().push(event);
                AuditDelivery::Ok
            }
        }
    }
}

// ------------------------------------------------------------- provider

/// Scripted provider reply for a chat call.
#[derive(Clone)]
pub enum Reply {
    /// SSE events `(event, data)` with a delay before each one.
    Sse { events: Vec<(String, Value)>, delay: Duration },
    /// Non-2xx JSON response.
    Status { status: u16, body: Value, retry_after: Option<u64> },
    /// Gateway error returned by `proxy_request`.
    Gateway(CanonicalError),
}

impl Reply {
    pub fn text(text: &str, input: i64, output: i64) -> Self {
        Self::with_events(text, vec![], json!({"input_tokens": input, "output_tokens": output}), vec![])
    }

    /// Text deltas preceded by `pre` events and followed by `response.completed`.
    pub fn with_events(text: &str, pre: Vec<(&str, Value)>, usage: Value, annotations: Vec<Value>) -> Self {
        let mut events: Vec<(String, Value)> = vec![("response.created".into(), json!({"type": "response.created"}))];
        events.extend(pre.into_iter().map(|(e, v)| (e.to_owned(), v)));
        for w in text.split_inclusive(' ') {
            events.push(("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "delta": w})));
        }
        events.push((
            "response.completed".into(),
            json!({"type": "response.completed", "response": {"id": "resp_abc123", "usage": usage,
                "output": [{"type": "message", "content": [{"type": "output_text", "text": text, "annotations": annotations}]}]}}),
        ));
        Self::Sse { events, delay: Duration::ZERO }
    }

    pub fn slow(text: &str, delay: Duration) -> Self {
        match Self::text(text, 5, 5) {
            Self::Sse { events, .. } => Self::Sse { events, delay },
            other => other,
        }
    }
}

/// One recorded outbound request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub json: Option<Value>,
    pub body_len: usize,
    pub headers: http::HeaderMap,
}

/// Fake OAGW + OpenAI-compatible provider.
pub struct FakeProvider {
    pub requests: Mutex<Vec<Recorded>>,
    pub chat_replies: Mutex<VecDeque<Reply>>,
    pub summary_reply: Mutex<Option<Reply>>,
    pub file_upload_status: Mutex<u16>,
    pub vs_add_status: Mutex<String>,
    pub vs_poll_statuses: Mutex<VecDeque<String>>,
    pub delete_status: Mutex<u16>,
    pub vs_delete_status: Mutex<u16>,
    counter: AtomicUsize,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            chat_replies: Mutex::new(VecDeque::new()),
            summary_reply: Mutex::new(None),
            file_upload_status: Mutex::new(200),
            vs_add_status: Mutex::new("completed".into()),
            vs_poll_statuses: Mutex::new(VecDeque::new()),
            delete_status: Mutex::new(200),
            vs_delete_status: Mutex::new(200),
            counter: AtomicUsize::new(0),
        }
    }
}

fn json_response(status: u16, v: &Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(v).unwrap()))
        .unwrap()
}

impl FakeProvider {
    pub fn push(&self, r: Reply) {
        self.chat_replies.lock().unwrap().push_back(r);
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// Streaming chat requests (`stream: true`).
    pub fn chat_requests(&self) -> Vec<Value> {
        self.recorded()
            .into_iter()
            .filter(|r| r.path.contains("/responses"))
            .filter_map(|r| r.json)
            .filter(|j| j["stream"] == json!(true))
            .collect()
    }

    pub fn summary_requests(&self) -> Vec<Value> {
        self.recorded()
            .into_iter()
            .filter(|r| r.path.contains("/responses"))
            .filter_map(|r| r.json)
            .filter(|j| j["stream"] == json!(false))
            .collect()
    }

    pub fn count(&self, method: &str, contains: &str) -> usize {
        self.recorded().iter().filter(|r| r.method == method && r.path.contains(contains)).count()
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}{n:024x}")
    }

    fn sse(reply: Reply) -> Result<http::Response<Body>, CanonicalError> {
        match reply {
            Reply::Gateway(e) => Err(e),
            Reply::Status { status, body, retry_after } => {
                let mut r = json_response(status, &body);
                if let Some(s) = retry_after {
                    r.headers_mut().insert(http::header::RETRY_AFTER, http::HeaderValue::from(s));
                }
                Ok(r)
            }
            Reply::Sse { events, delay } => {
                let stream = futures::stream::unfold(events.into_iter(), move |mut it| async move {
                    let (e, d) = it.next()?;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let chunk = format!("event: {e}\ndata: {d}\n\n");
                    Some((Ok::<Bytes, Box<dyn std::error::Error + Send + Sync>>(Bytes::from(chunk)), it))
                });
                Ok(http::Response::builder()
                    .status(200)
                    .header(http::header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::Stream(Box::pin(stream)))
                    .unwrap())
            }
        }
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeProvider {
    async fn create_upstream(&self, _: SecurityContext, _: CreateUpstreamRequest) -> Result<Upstream, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn list_upstreams(&self, _: SecurityContext, _: &ListQuery) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_upstream(&self, _: SecurityContext, _: Uuid, _: UpdateUpstreamRequest) -> Result<Upstream, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn create_route(&self, _: SecurityContext, _: CreateRouteRequest) -> Result<Route, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn list_routes(&self, _: SecurityContext, _: Option<Uuid>, _: &ListQuery) -> Result<Vec<Route>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_route(&self, _: SecurityContext, _: Uuid, _: UpdateRouteRequest) -> Result<Route, CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }
    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn resolve_proxy_target(&self, _: SecurityContext, _: &str, _: &str, _: &str) -> Result<(Upstream, Route), CanonicalError> {
        Err(CanonicalError::internal("not supported").create())
    }

    async fn proxy_request(&self, _ctx: SecurityContext, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap_or_default();
        let json: Option<Value> = serde_json::from_slice(&bytes).ok();
        let path = parts.uri.path().to_owned();
        let full = parts.uri.to_string();
        self.requests.lock().unwrap().push(Recorded {
            method: parts.method.to_string(),
            path: full,
            json: json.clone(),
            body_len: bytes.len(),
            headers: parts.headers.clone(),
        });
        let method = parts.method.as_str();
        if path.ends_with("/responses") {
            let streaming = json.as_ref().is_some_and(|j| j["stream"] == json!(true));
            if !streaming {
                let reply = self.summary_reply.lock().unwrap().clone();
                if let Some(Reply::Status { status, body, .. }) = reply {
                    return Ok(json_response(status, &body));
                }
                if let Some(Reply::Gateway(e)) = reply {
                    return Err(e);
                }
                return Ok(json_response(200, &json!({
                    "id": "resp_summary",
                    "output": [{"type": "message", "content": [{"type": "output_text",
                        "text": "<analysis>thinking</analysis>\n<summary>Summary of the chat.</summary>"}]}],
                    "usage": {"input_tokens": 100, "output_tokens": 20}
                })));
            }
            let reply = self.chat_replies.lock().unwrap().pop_front().unwrap_or_else(|| Reply::text("Hello from the fake provider", 42, 7));
            return Self::sse(reply);
        }
        if method == "POST" && path.ends_with("/files") && !path.contains("vector_stores") {
            let st = *self.file_upload_status.lock().unwrap();
            if st != 200 {
                return Ok(json_response(st, &json!({"error": {"message": "upload failed"}})));
            }
            return Ok(json_response(200, &json!({"id": self.next_id("file-")})));
        }
        if method == "POST" && path.ends_with("/vector_stores") {
            return Ok(json_response(200, &json!({"id": self.next_id("vs_")})));
        }
        if method == "POST" && path.contains("/vector_stores/") && path.ends_with("/files") {
            let st = self.vs_add_status.lock().unwrap().clone();
            return Ok(json_response(200, &json!({"id": "vsf", "status": st})));
        }
        if method == "GET" && path.contains("/vector_stores/") {
            let st = self.vs_poll_statuses.lock().unwrap().pop_front().unwrap_or_else(|| "completed".into());
            return Ok(json_response(200, &json!({"status": st})));
        }
        if method == "DELETE" && path.contains("/vector_stores/") {
            let st = *self.vs_delete_status.lock().unwrap();
            return Ok(json_response(st, &json!({"deleted": st == 200})));
        }
        if method == "DELETE" && path.contains("/files/") {
            let st = *self.delete_status.lock().unwrap();
            return Ok(json_response(st, &json!({"deleted": st == 200})));
        }
        Ok(json_response(404, &json!({"error": {"message": "unknown"}})))
    }
}

// --------------------------------------------------------------- catalog

pub fn model(id: &str, tier: &str, enabled: bool, default: bool, extra: Value) -> Value {
    let mut m = json!({
        "id": id, "provider_model_id": format!("prov-{id}"), "display_name": id.to_uppercase(),
        "description": format!("{id} model"), "provider_id": "openai", "provider_display_name": "OpenAI",
        "tier": tier, "enabled": enabled, "system_prompt": format!("System prompt of {id}."),
        "multimodal_capabilities": ["VISION_INPUT"], "context_window": 128_000, "max_output_tokens": 1000,
        "max_input_tokens": 0, "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 2_000_000, "multiplier_display": "1x",
        "max_num_results": 5, "web_search_context_size": "low", "max_tool_calls": 2,
        "general_config": {"max_file_size_mb": 25, "api_params": {"temperature": 0.5},
            "tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": default, "sort_order": 0}
    });
    if let (Some(base), Some(extra)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    m
}

/// Default test catalog.
pub fn catalog() -> Value {
    json!([
        model("premium-m", "Premium", true, true, json!({"input_tokens_credit_multiplier_micro": 3_000_000, "output_tokens_credit_multiplier_micro": 15_000_000, "multiplier_display": "3x"})),
        model("standard-m", "Standard", true, false, json!({})),
        model("novision-m", "Standard", true, false, json!({"multimodal_capabilities": [],
            "general_config": {"tool_support": {"web_search": false, "file_search": false, "code_interpreter": false}}})),
        model("tiny-m", "Standard", true, false, json!({"context_window": 900, "max_output_tokens": 100, "max_input_tokens": 600,
            "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 10, "safety_margin_pct": 0,
                "image_token_budget": 100, "tool_surcharge_tokens": 10, "web_search_surcharge_tokens": 10, "code_interpreter_surcharge_tokens": 10}})),
        model("disabled-m", "Premium", false, false, json!({}))
    ])
}

pub fn policy_cfg(catalog: Value, extra: Value) -> StaticModelPolicyConfig {
    let mut v = json!({"model_catalog": catalog});
    if let (Some(base), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in extra {
            base.insert(k.clone(), val.clone());
        }
    }
    serde_json::from_value(v).expect("policy cfg")
}

// --------------------------------------------------------------- harness

pub struct Harness {
    pub app: Router,
    pub svc: Arc<MiniChatService>,
    pub db: Arc<DBProvider<DomainError>>,
    pub provider: Arc<FakeProvider>,
    pub policy: Arc<TestPolicy>,
    pub audit: Arc<TestAudit>,
    pub pdp: Arc<MockPdp>,
    pub cfg: Arc<MiniChatConfig>,
    _outbox: Option<OutboxHandle>,
    db_path: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.db_path.clone().into_os_string();
            p.push(suffix);
            drop(std::fs::remove_file(p));
        }
    }
}

pub struct Opts {
    pub cfg: MiniChatConfig,
    pub policy: StaticModelPolicyConfig,
    pub start_outbox: bool,
}

impl Default for Opts {
    fn default() -> Self {
        let mut cfg: MiniChatConfig = serde_json::from_value(json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
            "providers": {"openai": {"kind": "openai_responses", "host": "127.0.0.1", "port": 9, "use_http": true,
                "storage_kind": "openai"}},
            "thread_summary_worker": {"summary_model_id": "standard-m"}
        }))
        .unwrap();
        cfg.fill_aliases();
        Self { cfg, policy: policy_cfg(catalog(), json!({})), start_outbox: true }
    }
}

async fn ctx_layer(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let header = req.headers().get("x-test-who").and_then(|v| v.to_str().ok()).map(str::to_owned);
    if let Some(h) = header {
        let (u, t) = h.split_once(':').unwrap();
        let ctx = SecurityContext::builder()
            .subject_id(Uuid::parse_str(u).unwrap())
            .subject_tenant_id(Uuid::parse_str(t).unwrap())
            .token_scopes(vec!["*".into()])
            .build()
            .unwrap();
        req.extensions_mut().insert(ctx);
    }
    next.run(req).await
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(Opts::default()).await
    }

    pub async fn with(opts: Opts) -> Self {
        drop(
            tracing_subscriber::fmt()
                .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,sqlx=error".to_owned()))
                .with_test_writer()
                .try_init(),
        );
        // File-backed WAL database: concurrent outbox workers wait on locks (busy_timeout)
        // instead of failing with SQLITE_LOCKED as a shared-cache in-memory DB would.
        let path = std::env::temp_dir().join(format!("mini_chat_test_{}.db", Uuid::new_v4().simple()));
        let dsn = format!("sqlite://{}?mode=rwc&journal_mode=WAL&busy_timeout=10000", path.display());
        let raw = connect_db(&dsn, ConnectOpts { max_conns: Some(8), min_conns: Some(1), ..Default::default() }).await.unwrap();
        let mut migrations = <mini_chat::infra::db::migrations::Migrator as sea_orm_migration::MigratorTrait>::migrations();
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        toolkit_db::migration_runner::run_migrations_for_testing(&raw, migrations).await.unwrap();
        let db = Arc::new(DBProvider::<DomainError>::new(raw.clone()));
        let cfg = Arc::new(opts.cfg);
        let pdp = Arc::new(MockPdp { mode: AtomicU8::new(PDP_ALLOW) });
        let provider = Arc::new(FakeProvider::default());
        let policy = Arc::new(TestPolicy {
            inner: Mutex::new(Arc::new(StaticModelPolicyService::from_config(&opts.policy))),
            published: Mutex::new(Vec::new()),
            publish_failures: AtomicUsize::new(0),
        });
        let audit = Arc::new(TestAudit::default());
        let resolver = Arc::new(ProviderResolver::new(&cfg));
        let llm = Arc::new(LlmGateway::new(provider.clone(), resolver));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let svc = MiniChatService::new(
            Arc::clone(&db),
            PolicyEnforcer::new(pdp.clone()),
            policy.clone(),
            audit.clone(),
            llm,
            outbox,
            Arc::clone(&cfg),
        );
        let handle = if opts.start_outbox {
            Some(mini_chat::infra::outbox_handlers::start(raw, &svc).await.unwrap())
        } else {
            None
        };
        let registry = toolkit::api::openapi_registry::OpenApiRegistryImpl::new();
        let app = mini_chat::api::rest::routes::register_routes(Router::new(), &registry, Arc::clone(&svc), "/mini-chat")
            .layer(axum::middleware::from_fn(ctx_layer));
        Self { app, svc, db, provider, policy, audit, pdp, cfg, _outbox: handle, db_path: path }
    }

    pub fn set_pdp(&self, mode: u8) {
        self.pdp.mode.store(mode, Ordering::SeqCst);
    }

    pub async fn send(&self, who: Who, method: &str, path: &str, body: Option<Value>) -> Resp {
        let mut b = http::Request::builder().method(method).uri(format!("/mini-chat/v1{path}"));
        b = b.header("x-test-who", format!("{}:{}", who.user, who.tenant));
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(AxumBody::from(v.to_string())).unwrap(),
            None => b.body(AxumBody::empty()).unwrap(),
        };
        self.call(req).await
    }

    pub async fn raw(&self, who: Who, method: &str, path: &str, content_type: Option<&str>, body: Vec<u8>) -> Resp {
        let mut b = http::Request::builder().method(method).uri(format!("/mini-chat/v1{path}"));
        b = b.header("x-test-who", format!("{}:{}", who.user, who.tenant));
        if let Some(ct) = content_type {
            b = b.header("content-type", ct);
        }
        self.call(b.body(AxumBody::from(body)).unwrap()).await
    }

    pub async fn call(&self, req: http::Request<AxumBody>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Resp { status, headers, body }
    }

    /// Start a request and return the live response (for disconnect tests).
    pub async fn open(&self, who: Who, method: &str, path: &str, body: Value) -> axum::response::Response {
        let req = http::Request::builder()
            .method(method)
            .uri(format!("/mini-chat/v1{path}"))
            .header("x-test-who", format!("{}:{}", who.user, who.tenant))
            .header("content-type", "application/json")
            .body(AxumBody::from(body.to_string()))
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap()
    }

    pub async fn upload(&self, who: Who, chat: Uuid, filename: &str, content_type: &str, data: &[u8]) -> Resp {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        self.raw(who, "POST", &format!("/chats/{chat}/attachments"), Some(&format!("multipart/form-data; boundary={boundary}")), body).await
    }

    /// Create a chat and return its id.
    pub async fn chat(&self, who: Who, model: Option<&str>) -> Uuid {
        let body = match model {
            Some(m) => json!({"model": m}),
            None => json!({}),
        };
        let r = self.send(who, "POST", "/chats", Some(body)).await;
        assert_eq!(r.status, 201, "{}", r.text());
        Uuid::parse_str(r.json()["id"].as_str().unwrap()).unwrap()
    }

    /// Send a message and return the parsed SSE events.
    pub async fn stream(&self, who: Who, chat: Uuid, body: Value) -> (Resp, Vec<(String, Value)>) {
        let r = self.send(who, "POST", &format!("/chats/{chat}/messages:stream"), Some(body)).await;
        let ev = r.sse();
        (r, ev)
    }

    pub async fn say(&self, who: Who, chat: Uuid, text: &str) -> Vec<(String, Value)> {
        let (r, ev) = self.stream(who, chat, json!({"content": text})).await;
        assert_eq!(r.status, 200, "{}", r.text());
        ev
    }

    // ---------- DB helpers

    pub async fn turns(&self, chat: Uuid) -> Vec<ent::chat_turns::Model> {
        let conn = self.db.conn().unwrap();
        let mut v = ent::chat_turns::Entity::find()
            .filter(ent::chat_turns::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap();
        v.sort_by_key(|t| (t.started_at, t.id));
        v
    }

    pub async fn messages(&self, chat: Uuid) -> Vec<ent::messages::Model> {
        let conn = self.db.conn().unwrap();
        let mut v = ent::messages::Entity::find()
            .filter(ent::messages::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap();
        v.sort_by_key(|m| (m.created_at, m.id));
        v
    }

    pub async fn quota_rows(&self, who: Who) -> Vec<ent::quota_usage::Model> {
        let conn = self.db.conn().unwrap();
        ent::quota_usage::Entity::find()
            .filter(ent::quota_usage::Column::UserId.eq(who.user))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
    }

    pub async fn quota_row(&self, who: Who, bucket: &str, period: &str) -> Option<ent::quota_usage::Model> {
        self.quota_rows(who).await.into_iter().find(|r| r.bucket == bucket && r.period_type == period)
    }

    pub async fn attachment(&self, id: Uuid) -> ent::attachments::Model {
        let conn = self.db.conn().unwrap();
        ent::attachments::Entity::find()
            .filter(ent::attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
            .unwrap()
    }

    /// Seed spent credits on a bucket row (both periods).
    pub async fn seed_spent(&self, who: Who, bucket: &str, spent: i64) {
        let now = time::OffsetDateTime::now_utc();
        for (p, start) in mini_chat::domain::quota::period_starts(now) {
            mini_chat::domain::quota::apply_delta(
                &self.db.conn().unwrap(),
                who.tenant,
                who.user,
                p,
                start,
                bucket,
                mini_chat::domain::quota::BucketDelta { spent, ..Default::default() },
            )
            .await
            .unwrap();
        }
    }

    pub async fn seed_daily_tool_calls(&self, who: Who, web: i64, ci: i64) {
        let now = time::OffsetDateTime::now_utc();
        let start = mini_chat::domain::credits::Period::Daily.start(now);
        mini_chat::domain::quota::apply_delta(
            &self.db.conn().unwrap(),
            who.tenant,
            who.user,
            mini_chat::domain::credits::Period::Daily,
            start,
            "total",
            mini_chat::domain::quota::BucketDelta { web_search_calls: web, code_interpreter_calls: ci, ..Default::default() },
        )
        .await
        .unwrap();
    }

    /// Force a running turn to look stale.
    pub async fn age_turn(&self, turn: Uuid, secs: i64) {
        let conn = self.db.conn().unwrap();
        let old = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
        ent::chat_turns::Entity::update_many()
            .col_expr(ent::chat_turns::Column::LastProgressAt, sea_orm::sea_query::Expr::value(old))
            .col_expr(ent::chat_turns::Column::StartedAt, sea_orm::sea_query::Expr::value(old))
            .filter(ent::chat_turns::Column::Id.eq(turn))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    pub fn usage_events(&self) -> Vec<UsageEvent> {
        self.policy.published.lock().unwrap().clone()
    }

    pub fn audit_events(&self) -> Vec<AuditEvent> {
        self.audit.events.lock().unwrap().clone()
    }

    /// Poll until `f` returns true (outbox delivery is asynchronous).
    pub async fn eventually<F: Fn() -> bool>(&self, what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}; usage={:?} audit={:?}", self.usage_events(), self.audit_events().iter().map(|e| e.event_type().to_owned()).collect::<Vec<_>>());
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not json: {}", self.text()))
    }
    /// Parse an SSE body into `(event, data)` pairs (comments skipped).
    pub fn sse(&self) -> Vec<(String, Value)> {
        parse_sse(&self.text())
    }
    /// Assert a canonical problem and return it.
    pub fn problem(&self, status: u16) -> Value {
        assert_eq!(self.status, status, "unexpected status, body: {}", self.text());
        let p = self.json();
        assert_eq!(p["status"], json!(status));
        assert!(p.get("code").is_none(), "no top-level code field");
        p
    }
}

pub fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                e.trim().clone_into(&mut event);
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            }
        }
        if !event.is_empty() {
            out.push((event, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    out
}

pub fn names(ev: &[(String, Value)]) -> Vec<&str> {
    ev.iter().map(|(n, _)| n.as_str()).collect()
}

pub fn find<'a>(ev: &'a [(String, Value)], name: &str) -> &'a Value {
    &ev.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no {name} event in {:?}", names(ev))).1
}

/// Field violation reason of a problem.
pub fn fv_reason(p: &Value) -> String {
    p["context"]["field_violations"][0]["reason"].as_str().unwrap_or_default().to_owned()
}

pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 10, 10, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}
