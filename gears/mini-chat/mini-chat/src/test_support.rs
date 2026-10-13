//! Test harness: real router + SQLite + outbox, fake provider proxy,
//! static policy plugin and authz fakes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc, dead_code)]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::Request;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::Body;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::Authorizer;
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::domain::workers::cleanup::{AttachmentCleanupHandler, ChatCleanupHandler};
use crate::domain::workers::thread_summary::ThreadSummaryHandler;
use crate::domain::workers::{AuditHandler, ServiceSlot, UsageHandler};
use crate::infra::audit_gateway::{AuditResolution, AuditResolver};
use crate::infra::llm::client::{LlmClient, ProxyClient};
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::llm::storage::StorageClient;
use crate::infra::outbox::{OutboxEnqueuer, OutboxHandlers, start_pipeline};
use crate::infra::plugins::static_model_policy::{StaticModelPolicy, StaticModelPolicyConfig};
use crate::infra::policy_gateway::StaticPolicyProvider;

pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
pub const USER_A: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_A2: Uuid = Uuid::from_u128(0x4444_4444_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_B: Uuid = Uuid::from_u128(0x2222_2222_6a88_4768_9dfc_6bcd_5187_d9ed);

#[must_use]
pub fn ctx(tenant: Uuid, user: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

#[must_use]
pub fn user_a() -> SecurityContext {
    ctx(TENANT_A, USER_A)
}

/// Authorization behavior of the fake PDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzMode {
    Allow,
    Deny,
    Fail,
}

pub struct FakeAuthz {
    pub mode: Mutex<AuthzMode>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeAuthz {
    fn check(&self, action: &str) -> Result<(), DomainError> {
        self.calls.lock().unwrap().push(action.to_owned());
        match *self.mode.lock().unwrap() {
            AuthzMode::Allow => Ok(()),
            AuthzMode::Deny => Err(DomainError::AuthzDenied),
            AuthzMode::Fail => Err(DomainError::AuthzUnavailable),
        }
    }
}

#[async_trait]
impl Authorizer for FakeAuthz {
    async fn chat_scope(&self, ctx: &SecurityContext, action: &str, _chat: Option<Uuid>) -> Result<AccessScope, DomainError> {
        self.check(action)?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()))
    }
    async fn model_access(&self, _ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        self.check(action)
    }
    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        self.check("read")?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()))
    }
}

/// Audit plugin capturing every delivered event.
#[derive(Default)]
pub struct CapturedAudit {
    pub events: Mutex<Vec<Value>>,
}

#[async_trait]
impl mini_chat_sdk::MiniChatAuditPluginClientV1 for CapturedAudit {
    async fn emit_audit_event(
        &self,
        event: mini_chat_sdk::MiniChatAuditEvent,
    ) -> Result<(), mini_chat_sdk::MiniChatAuditPluginError> {
        self.events.lock().unwrap().push(serde_json::to_value(&event).unwrap());
        Ok(())
    }
}

struct CapturingAudit(Arc<CapturedAudit>);

#[async_trait]
impl AuditResolver for CapturingAudit {
    async fn resolve(&self) -> AuditResolution {
        AuditResolution::Client(Arc::clone(&self.0) as Arc<dyn mini_chat_sdk::MiniChatAuditPluginClientV1>)
    }
}

/// Model-policy plugin wrapper recording published usage events.
pub struct CapturingPolicy {
    inner: StaticModelPolicy,
    pub usage: Mutex<Vec<mini_chat_sdk::UsageEvent>>,
}

#[async_trait]
impl mini_chat_sdk::MiniChatModelPolicyPluginClientV1 for CapturingPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<mini_chat_sdk::PolicyVersionInfo, mini_chat_sdk::MiniChatModelPolicyPluginError> {
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<mini_chat_sdk::PolicySnapshot, mini_chat_sdk::MiniChatModelPolicyPluginError> {
        self.inner.get_policy_snapshot(user_id, policy_version).await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<mini_chat_sdk::UserLimits, mini_chat_sdk::MiniChatModelPolicyPluginError> {
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, payload: mini_chat_sdk::UsageEvent) -> Result<(), mini_chat_sdk::PublishError> {
        self.usage.lock().unwrap().push(payload);
        Ok(())
    }
}

/// One request seen by the fake provider.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub uri: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Recorded {
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Scripted provider response.
pub enum FakeResponse {
    /// Status + raw body chunks (delivered at once).
    Raw(u16, Vec<(String, String)>, Vec<Bytes>),
    /// Status + streamed chunks pushed by the test.
    Live(u16, tokio::sync::mpsc::Receiver<Bytes>),
    /// Gateway-side error.
    Gateway(CanonicalError),
}

type Responder = Box<dyn Fn(&Recorded) -> Option<FakeResponse> + Send + Sync>;

/// Fake OAGW proxy recording requests.
pub struct FakeProxy {
    pub requests: Mutex<Vec<Recorded>>,
    responders: Mutex<Vec<Responder>>,
    pub file_counter: Mutex<u64>,
}

impl FakeProxy {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responders: Mutex::new(Vec::new()),
            file_counter: Mutex::new(0),
        }
    }

    /// Install a responder tried before the defaults (latest wins).
    pub fn respond(&self, f: impl Fn(&Recorded) -> Option<FakeResponse> + Send + Sync + 'static) {
        self.responders.lock().unwrap().insert(0, Box::new(f));
    }

    /// Requests whose URI contains `needle`.
    #[must_use]
    pub fn requests_to(&self, needle: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.uri.contains(needle))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn chat_requests(&self) -> Vec<Recorded> {
        self.requests_to("/v1/responses")
    }

    fn default_response(&self, r: &Recorded) -> FakeResponse {
        let uri = r.uri.as_str();
        if uri.contains("/responses") {
            let body = r.json();
            if body["stream"] == json!(false) {
                return json_resp(200, &json!({
                    "id": "resp_summaryabc123",
                    "output": [{"type":"message","content":[{"type":"output_text","text":"<analysis>a</analysis><summary>Summary of chat</summary>"}]}],
                    "usage": {"input_tokens": 50, "output_tokens": 20}
                }));
            }
            return sse_resp(&completion_events("Hello world", 12, 5));
        }
        if r.method == "POST" && uri.contains("/files") && !uri.contains("vector_stores") {
            let mut c = self.file_counter.lock().unwrap();
            *c += 1;
            return json_resp(200, &json!({"id": format!("file-{:016}", *c)}));
        }
        if r.method == "POST" && uri.contains("/vector_stores") && uri.contains("/files") {
            return json_resp(200, &json!({"status": "completed"}));
        }
        if r.method == "GET" && uri.contains("/vector_stores") {
            return json_resp(200, &json!({"status": "completed"}));
        }
        if r.method == "POST" && uri.contains("/vector_stores") {
            return json_resp(200, &json!({"id": "vs_abcdefghijklmnop"}));
        }
        json_resp(200, &json!({"deleted": true}))
    }
}

#[must_use]
pub fn json_resp(status: u16, v: &Value) -> FakeResponse {
    FakeResponse::Raw(
        status,
        vec![("content-type".into(), "application/json".into())],
        vec![Bytes::from(v.to_string())],
    )
}

/// SSE body of named events.
#[must_use]
pub fn sse_resp(events: &[(String, Value)]) -> FakeResponse {
    let chunks = events
        .iter()
        .map(|(n, d)| Bytes::from(format!("event: {n}\ndata: {d}\n\n")))
        .collect();
    FakeResponse::Raw(200, vec![("content-type".into(), "text/event-stream".into())], chunks)
}

/// Responses API events of a completed answer split in two deltas.
#[must_use]
pub fn completion_events(text: &str, input: i64, output: i64) -> Vec<(String, Value)> {
    #[allow(clippy::integer_division)] // reason: split point for a test fixture, truncation intended
    let mid = text.len() / 2;
    vec![
        ("response.created".into(), json!({"type": "response.created"})),
        ("response.output_text.delta".into(), json!({"item_id": "m1", "content_index": 0, "delta": &text[..mid]})),
        ("response.output_text.delta".into(), json!({"item_id": "m1", "content_index": 0, "delta": &text[mid..]})),
        (
            "response.completed".into(),
            json!({"response": {"id": "resp_test123", "usage": {"input_tokens": input, "output_tokens": output}}}),
        ),
    ]
}

#[async_trait]
impl ProxyClient for FakeProxy {
    async fn proxy(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap_or_default();
        let rec = Recorded {
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            content_type: parts
                .headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            body: bytes.to_vec(),
        };
        self.requests.lock().unwrap().push(rec.clone());
        let scripted = {
            let rs = self.responders.lock().unwrap();
            rs.iter().find_map(|f| f(&rec))
        };
        let resp = scripted.unwrap_or_else(|| self.default_response(&rec));
        match resp {
            FakeResponse::Gateway(e) => Err(e),
            FakeResponse::Raw(status, headers, chunks) => {
                let mut b = http::Response::builder().status(status);
                for (k, v) in headers {
                    b = b.header(k, v);
                }
                let stream = futures::stream::iter(chunks.into_iter().map(Ok));
                Ok(b.body(Body::Stream(Box::pin(stream))).unwrap())
            }
            FakeResponse::Live(status, rx) => {
                let stream = tokio_stream_from(rx);
                Ok(http::Response::builder()
                    .status(status)
                    .header("content-type", "text/event-stream")
                    .body(Body::Stream(Box::pin(stream)))
                    .unwrap())
            }
        }
    }
}

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> impl futures::Stream<Item = Result<Bytes, oagw_sdk::body::BoxError>> + Send {
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok(b), rx)) })
}

/// Default test catalog: premium `gpt-4.1` (vision, all tools), standard
/// `gpt-4.1-mini`, standard `no-vision`, disabled `old-model`, tiny-context model.
#[must_use]
pub fn default_catalog() -> Value {
    let model = |id: &str, tier: &str, enabled: bool, default: bool, vision: bool, ts: Value, cw: u32, mo: u32, mi: u32, mult: i64| {
        json!({
            "id": id, "provider_model_id": id, "display_name": id.to_uppercase(), "description": format!("{id} model"),
            "provider_id": "mock", "provider_display_name": "Mock", "tier": tier, "enabled": enabled,
            "system_prompt": "You are a helpful assistant.", "multimodal_capabilities": if vision { json!(["VISION_INPUT"]) } else { json!([]) },
            "context_window": cw, "max_output_tokens": mo, "max_input_tokens": mi,
            "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": mult * 3,
            "multiplier_display": "1x", "max_num_results": 5, "web_search_context_size": "low", "max_tool_calls": 2,
            "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 100, "safety_margin_pct": 10,
                "image_token_budget": 1000, "tool_surcharge_tokens": 500, "web_search_surcharge_tokens": 500,
                "code_interpreter_surcharge_tokens": 1000, "minimal_generation_floor": 50},
            "general_config": {"type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
                "api_params": {"temperature": 0.7, "stop": []}, "features": {"streaming": true},
                "tool_support": ts, "supported_endpoints": {"responses": true}},
            "preference": {"is_default": default, "sort_order": 0}
        })
    };
    let all = json!({"web_search": true, "file_search": true, "code_interpreter": true});
    json!([
        model("gpt-4.1", "Premium", true, true, true, all.clone(), 1_047_576, 32768, 1_047_576, 3_000_000),
        model("gpt-4.1-mini", "Standard", true, false, true, all, 1_047_576, 32768, 1_047_576, 1_000_000),
        model("no-vision", "Standard", true, false, false, json!({}), 100_000, 4096, 0, 1_000_000),
        model("old-model", "Premium", false, false, true, json!({}), 100_000, 4096, 0, 1_000_000),
        model("tiny-ctx", "Standard", true, false, true, json!({}), 4096, 1024, 3072, 1_000_000),
    ])
}

/// Test environment options.
pub struct EnvOptions {
    pub config: Value,
    pub policy: Value,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self {
            config: json!({}),
            policy: json!({"model_catalog": default_catalog()}),
        }
    }
}

/// A running in-process mini-chat.
pub struct TestEnv {
    pub svc: Arc<MiniChat>,
    pub router: Router,
    pub proxy: Arc<FakeProxy>,
    pub authz: Arc<FakeAuthz>,
    pub db: toolkit_db::Db,
    pub db_path: String,
    pub policy: Arc<CapturingPolicy>,
    pub audit: Arc<CapturedAudit>,
    _outbox: OutboxHandle,
    _dir: tempfile::TempDir,
}

fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

impl TestEnv {
    pub async fn new() -> Self {
        Self::with(EnvOptions::default()).await
    }

    pub async fn with(opts: EnvOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mini_chat.db");
        let dsn = format!("sqlite://{}?mode=rwc&wal=true&busy_timeout=10000", path.display());
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(8),
                min_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        toolkit_db::migration_runner::run_migrations_for_testing(&db, crate::infra::db::migrations::all_migrations())
            .await
            .unwrap();
        let mut cfgv = json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
            "providers": {"mock": {"kind": "openai_responses", "host": "127.0.0.1", "port": 9, "use_http": true, "storage_kind": "openai"}},
            "orphan_watchdog": {"enabled": false},
            "upload_reaper": {"enabled": false}
        });
        merge(&mut cfgv, &opts.config);
        let cfg = Arc::new(MiniChatConfig::from_value(Some(&cfgv)).unwrap());
        let policy_cfg = StaticModelPolicyConfig::from_value(&opts.policy).unwrap();
        let capture = Arc::new(CapturingPolicy {
            inner: StaticModelPolicy::new(&policy_cfg),
            usage: Mutex::new(Vec::new()),
        });
        let policy = Arc::new(StaticPolicyProvider(
            Arc::clone(&capture) as Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>
        ));
        let audit = Arc::new(CapturedAudit::default());
        let proxy = Arc::new(FakeProxy::new());
        let authz = Arc::new(FakeAuthz {
            mode: Mutex::new(AuthzMode::Allow),
            calls: Mutex::new(Vec::new()),
        });
        let slot: ServiceSlot = Arc::new(OnceLock::new());
        let handlers = OutboxHandlers {
            usage: Box::new(UsageHandler { slot: Arc::clone(&slot) }),
            attachment_cleanup: Box::new(AttachmentCleanupHandler { slot: Arc::clone(&slot) }),
            chat_cleanup: Box::new(ChatCleanupHandler { slot: Arc::clone(&slot) }),
            thread_summary: Box::new(ThreadSummaryHandler { slot: Arc::clone(&slot) }),
            audit: Box::new(AuditHandler {
                resolver: Arc::new(CapturingAudit(Arc::clone(&audit))),
            }),
        };
        let handle = start_pipeline(db.clone(), &cfg.outbox, 60, handlers).await.unwrap();
        let svc = Arc::new(MiniChat {
            cfg: Arc::clone(&cfg),
            db: Arc::new(DBProvider::new(db.clone())),
            authz: Arc::clone(&authz) as Arc<dyn Authorizer>,
            policy,
            llm: LlmClient::new(Arc::clone(&proxy) as Arc<dyn ProxyClient>),
            storage: StorageClient::new(Arc::clone(&proxy) as Arc<dyn ProxyClient>),
            resolver: ProviderResolver::new(cfg.providers.clone()),
            outbox: OutboxEnqueuer::new(Arc::clone(handle.outbox()), cfg.outbox.clone()),
            upload_slots: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
        });
        slot.set(Arc::clone(&svc)).ok();
        let openapi = OpenApiRegistryImpl::new();
        let router = crate::api::rest::routes::register_routes(Router::new(), &openapi, Arc::clone(&svc), "/mini-chat");
        Self {
            svc,
            router,
            proxy,
            authz,
            db,
            db_path: path.display().to_string(),
            policy: capture,
            audit,
            _outbox: handle,
            _dir: dir,
        }
    }

    pub fn set_authz(&self, m: AuthzMode) {
        *self.authz.mode.lock().unwrap() = m;
    }

    /// Send a request as `ctx`; returns status, headers and body bytes.
    pub async fn call(&self, ctx: &SecurityContext, method: &str, uri: &str, body: Option<Value>) -> TestResponse {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let body = body.map_or_else(AxumBody::empty, |v| AxumBody::from(v.to_string()));
        let mut req = b.body(body).unwrap();
        req.extensions_mut().insert(ctx.clone());
        self.send(req).await
    }

    pub async fn send(&self, req: Request<AxumBody>) -> TestResponse {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let mut stream = resp.into_body().into_data_stream();
        let mut buf = Vec::new();
        while let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        TestResponse { status, headers, body: buf }
    }

    /// Start a request and return the status plus the live body stream.
    pub async fn open(
        &self,
        ctx: &SecurityContext,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (u16, axum::body::BodyDataStream) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(AxumBody::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ctx.clone());
        let resp = self.router.clone().oneshot(req).await.unwrap();
        (resp.status().as_u16(), resp.into_body().into_data_stream())
    }

    pub async fn get(&self, uri: &str) -> TestResponse {
        self.call(&user_a(), "GET", uri, None).await
    }

    pub async fn create_chat(&self, ctx: &SecurityContext, body: Value) -> Value {
        let r = self.call(ctx, "POST", "/mini-chat/v1/chats", Some(body)).await;
        assert_eq!(r.status, 201, "{}", r.text());
        r.json()
    }

    /// Create a chat for user A and return its id.
    pub async fn chat(&self, model: Option<&str>) -> String {
        let body = model.map_or_else(|| json!({}), |m| json!({"model": m}));
        self.create_chat(&user_a(), body).await["id"].as_str().unwrap().to_owned()
    }

    pub async fn stream(&self, ctx: &SecurityContext, chat: &str, body: Value) -> TestResponse {
        self.call(ctx, "POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), Some(body)).await
    }

    pub async fn send_msg(&self, chat: &str, content: &str) -> TestResponse {
        self.stream(&user_a(), chat, json!({"content": content})).await
    }

    /// Wait until the outbox processed everything (best effort sleep loop).
    pub async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// Poll until `f` holds (outbox delivery is asynchronous); 10 s cap.
    pub async fn eventually(&self, what: &str, f: impl Fn(&Self) -> bool) {
        for _ in 0..200 {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Usage events published so far.
    pub fn usage_events(&self) -> Vec<mini_chat_sdk::UsageEvent> {
        self.policy.usage.lock().unwrap().clone()
    }

    /// Audit events delivered so far (JSON).
    pub fn audit_events(&self) -> Vec<Value> {
        self.audit.events.lock().unwrap().clone()
    }

    pub async fn sql_rows(&self, sql: &str) -> Vec<sea_orm::QueryResult> {
        use sea_orm::{ConnectionTrait, Statement};
        let conn = test_conn(&self.db_path).await;
        conn.query_all_raw(Statement::from_string(sea_orm::DatabaseBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap()
    }

    pub async fn sql_exec(&self, sql: &str) {
        use sea_orm::ConnectionTrait;
        let conn = test_conn(&self.db_path).await;
        conn.execute_unprepared(sql).await.unwrap();
    }

    pub async fn count(&self, sql: &str) -> i64 {
        let rows = self.sql_rows(sql).await;
        rows.first().map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap_or(0))
    }
}

/// A plain `SeaORM` connection to the same database file (raw SQL in tests only).
async fn test_conn(path: &str) -> sea_orm::DatabaseConnection {
    sea_orm::Database::connect(format!("sqlite://{path}?mode=rw")).await.unwrap()
}

/// Read the next SSE event from a live body stream (5 s timeout).
pub async fn next_event(
    stream: &mut axum::body::BodyDataStream,
    parser: &mut crate::infra::llm::sse_parser::SseParser,
    pending: &mut std::collections::VecDeque<(String, Value)>,
) -> Option<(String, Value)> {
    loop {
        if let Some(e) = pending.pop_front() {
            return Some(e);
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.next()).await.ok()??.ok()?;
        for e in parser.feed(&chunk) {
            pending.push_back((e.event.unwrap_or_default(), serde_json::from_str(&e.data).unwrap_or(Value::Null)));
        }
    }
}

/// Live provider stream controlled by the test: returns the sender and installs
/// a responder for the next chat request.
pub fn live_provider(proxy: &FakeProxy) -> tokio::sync::mpsc::Sender<Bytes> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let slot = Mutex::new(Some(rx));
    proxy.respond(move |r| {
        if r.uri.contains("/responses") {
            slot.lock().unwrap().take().map(|rx| FakeResponse::Live(200, rx))
        } else {
            None
        }
    });
    tx
}

/// One SSE chunk of a Responses event.
#[must_use]
pub fn sse_chunk(name: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// Captured HTTP response.
pub struct TestResponse {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl TestResponse {
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// Parsed SSE events `(name, data)`.
    #[must_use]
    pub fn events(&self) -> Vec<(String, Value)> {
        let mut p = crate::infra::llm::sse_parser::SseParser::new();
        let mut out: Vec<(String, Value)> = p
            .feed(&self.body)
            .into_iter()
            .map(|e| (e.event.unwrap_or_default(), serde_json::from_str(&e.data).unwrap_or(Value::Null)))
            .collect();
        if let Some(e) = p.finish() {
            out.push((e.event.unwrap_or_default(), serde_json::from_str(&e.data).unwrap_or(Value::Null)));
        }
        out
    }

    #[must_use]
    pub fn event_names(&self) -> Vec<String> {
        self.events().into_iter().map(|(n, _)| n).collect()
    }

    #[must_use]
    pub fn event(&self, name: &str) -> Option<Value> {
        self.events().into_iter().find(|(n, _)| n == name).map(|(_, d)| d)
    }

    /// Assert a canonical problem with status and a reason somewhere in context.
    pub fn assert_problem(&self, status: u16, reason: &str) {
        assert_eq!(self.status, status, "body: {}", self.text());
        let ctx = self.json()["context"].to_string();
        assert!(ctx.contains(reason), "reason {reason} not in {ctx}");
    }
}
