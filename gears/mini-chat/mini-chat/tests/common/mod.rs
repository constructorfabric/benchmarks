//! Integration test harness: the full mini-chat service graph on a file-backed SQLite database,
//! the real OAGW data plane (`build_test_gateway`) and an in-process mock provider. Requests go
//! through the gear's axum router (`tower::ServiceExt::oneshot`); the caller identity is chosen
//! with the `x-test-user` header (`a1`, `a2` = same tenant, `b` = other tenant).

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "shared integration-test harness: each test binary uses a subset, and setup failures must panic"
)]

pub mod mock_provider;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverApi, Constraint, EvaluationRequest, EvaluationResponse, EvaluationResponseContext, InPredicate,
    PolicyEnforcer, Predicate,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::audit::AuditGateway;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::policy::PolicyGateway;
use mini_chat::domain::service::Svc;
use mini_chat::gear::{build_services, start_outbox};
use mini_chat::infra::oagw_provisioning::Provisioner;
use mini_chat_sdk::{
    KillSwitches, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo, PublishError,
    TierLimits, TurnAuditEvent, TurnDeleteAuditEvent, TurnEditAuditEvent, TurnRetryAuditEvent, UsageEvent,
    UserLimits,
};
use oagw::test_support::{TestCpBuilder, TestDpBuilder, build_test_gateway};
use serde_json::{Value, json};
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use toolkit_security::{SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::{Uuid, uuid};

#[allow(unused_imports)]
pub use mock_provider::{MockConfig, MockProvider, Recorded};

pub const TENANT_A: Uuid = uuid!("0000000a-0000-4000-8000-00000000000a");
pub const USER_A1: Uuid = uuid!("a1a1a1a1-0000-4000-8000-000000000001");
pub const USER_A2: Uuid = uuid!("a2a2a2a2-0000-4000-8000-000000000002");
pub const TENANT_B: Uuid = uuid!("0000000b-0000-4000-8000-00000000000b");
pub const USER_B: Uuid = uuid!("b1b1b1b1-0000-4000-8000-000000000003");
pub const SYS_TENANT: Uuid = uuid!("00000000-df51-5b42-9538-d2b56b7ee953");
pub const SYS_USER: Uuid = uuid!("11111111-6a88-4768-9dfc-6bcd5187d9ed");

/// Identity of a test caller.
pub fn identity(who: &str) -> (Uuid, Uuid) {
    match who {
        "a2" => (TENANT_A, USER_A2),
        "b" => (TENANT_B, USER_B),
        _ => (TENANT_A, USER_A1),
    }
}

// ---------------------------------------------------------------------------------------------
// PDP stub
// ---------------------------------------------------------------------------------------------

/// PDP behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpMode {
    /// Allow with a tenant `In` constraint (like the static PDP).
    Allow = 0,
    /// Deny.
    Deny = 1,
    /// Infrastructure failure.
    Fail = 2,
}

/// Configurable PDP.
#[derive(Default)]
pub struct PdpStub {
    mode: AtomicU8,
}

impl PdpStub {
    pub fn set(&self, m: PdpMode) {
        self.mode.store(m as u8, Ordering::SeqCst);
    }
}

#[async_trait]
impl AuthZResolverApi for PdpStub {
    async fn evaluate(
        &self,
        _ctx: toolkit_security::PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => Ok(EvaluationResponse { decision: false, context: EvaluationResponseContext::default() }),
            2 => Err(CanonicalError::service_unavailable().with_detail("pdp down").create()),
            _ => {
                let tid = req
                    .context
                    .tenant_context
                    .as_ref()
                    .and_then(|t| t.root_id)
                    .or_else(|| {
                        req.subject
                            .properties
                            .get("tenant_id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| Uuid::parse_str(s).ok())
                    })
                    .unwrap_or_default();
                // Like the static PDP: emit the tenant clamp only for PEPs declaring the property.
                let constraints = if req.context.supported_properties.iter().any(|p| p == pep_properties::OWNER_TENANT_ID) {
                    vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(pep_properties::OWNER_TENANT_ID, [tid]))],
                    }]
                } else {
                    Vec::new()
                };
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext { constraints, ..EvaluationResponseContext::default() },
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Model policy plugin stub
// ---------------------------------------------------------------------------------------------

/// A catalog entry with test defaults.
pub fn model_entry(id: &str, tier: &str, over: Value) -> ModelCatalogEntry {
    let mut v = json!({
        "id": id,
        "provider_model_id": format!("{id}-provider"),
        "display_name": id.to_uppercase(),
        "description": "test model",
        "provider_id": "mock",
        "provider_display_name": "Mock",
        "tier": tier,
        "enabled": true,
        "system_prompt": "You are a test assistant.",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0, "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true, "image_generation": false, "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true, "embeddings": false, "image_generation": false,
                "audio_speech_generation": false, "audio_transcription": false, "audio_translation": false}
        },
        "preference": {"is_default": tier == "Premium", "sort_order": 0}
    });
    merge(&mut v, over);
    serde_json::from_value(v).expect("catalog entry")
}

fn merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                merge(d.entry(k).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s,
    }
}

/// Default catalog.
pub fn default_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        model_entry("gpt-4.1", "Premium", json!({})),
        model_entry("gpt-4.1-mini", "Standard", json!({"preference": {"is_default": false, "sort_order": 1}})),
        model_entry("text-only", "Standard", json!({"multimodal_capabilities": [], "preference": {"is_default": false, "sort_order": 2}})),
        model_entry("disabled-model", "Standard", json!({"enabled": false, "preference": {"is_default": false, "sort_order": 3}})),
    ]
}

/// Scriptable policy plugin (catalog, kill switches, limits) that records usage events.
pub struct TestPolicy {
    pub snapshot: RwLock<PolicySnapshot>,
    pub standard: RwLock<TierLimits>,
    pub premium: RwLock<TierLimits>,
    pub usage: Mutex<Vec<UsageEvent>>,
    pub publish_error: Mutex<Option<PublishError>>,
    pub fail: std::sync::atomic::AtomicBool,
}

impl TestPolicy {
    pub fn new(catalog: Vec<ModelCatalogEntry>) -> Self {
        Self {
            snapshot: RwLock::new(PolicySnapshot {
                policy_version: 1,
                model_catalog: catalog,
                kill_switches: KillSwitches {
                    disable_premium_tier: false,
                    force_standard_tier: false,
                    disable_web_search: false,
                    disable_file_search: false,
                    disable_images: false,
                    disable_code_interpreter: false,
                },
            }),
            standard: RwLock::new(TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 }),
            premium: RwLock::new(TierLimits { limit_daily_credits_micro: 50_000_000, limit_monthly_credits_micro: 500_000_000 }),
            usage: Mutex::new(Vec::new()),
            publish_error: Mutex::new(None),
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn kill_switches(&self, f: impl FnOnce(&mut KillSwitches)) {
        f(&mut self.snapshot.write().unwrap().kill_switches);
    }

    pub fn catalog(&self, f: impl FnOnce(&mut Vec<ModelCatalogEntry>)) {
        f(&mut self.snapshot.write().unwrap().model_catalog);
    }

    pub fn usage_events(&self) -> Vec<UsageEvent> {
        self.usage.lock().unwrap().clone()
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for TestPolicy {
    async fn get_current_policy_version(&self, _u: Uuid) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Unavailable("down".into()));
        }
        Ok(PolicyVersionInfo { policy_version: 1, generated_at: time::OffsetDateTime::UNIX_EPOCH })
    }

    async fn get_policy_snapshot(&self, _u: Uuid, _v: u64) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Unavailable("down".into()));
        }
        Ok(self.snapshot.read().unwrap().clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, v: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version: v,
            standard: *self.standard.read().unwrap(),
            premium: *self.premium.read().unwrap(),
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        if let Some(e) = self.publish_error.lock().unwrap().clone() {
            return Err(e);
        }
        self.usage.lock().unwrap().push(payload);
        Ok(())
    }
}

/// Recording audit plugin.
#[derive(Default)]
pub struct TestAudit {
    pub turns: Mutex<Vec<TurnAuditEvent>>,
    pub retries: Mutex<Vec<TurnRetryAuditEvent>>,
    pub edits: Mutex<Vec<TurnEditAuditEvent>>,
    pub deletes: Mutex<Vec<TurnDeleteAuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for TestAudit {
    async fn emit_turn_audit(&self, e: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.turns.lock().unwrap().push(e);
        Ok(())
    }
    async fn emit_turn_retry_audit(&self, e: TurnRetryAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.retries.lock().unwrap().push(e);
        Ok(())
    }
    async fn emit_turn_edit_audit(&self, e: TurnEditAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.edits.lock().unwrap().push(e);
        Ok(())
    }
    async fn emit_turn_delete_audit(&self, e: TurnDeleteAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.deletes.lock().unwrap().push(e);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

/// Options of a test environment.
pub struct EnvOptions {
    /// Extra gear config (merged over the defaults).
    pub config: Value,
    /// Catalog.
    pub catalog: Vec<ModelCatalogEntry>,
    /// Start the outbox pipeline.
    pub outbox: bool,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self { config: json!({}), catalog: default_catalog(), outbox: true }
    }
}

/// A running test environment.
pub struct TestEnv {
    pub svc: Arc<Svc>,
    pub router: Router,
    pub mock: MockProvider,
    pub db: Db,
    pub pdp: Arc<PdpStub>,
    pub policy: Arc<TestPolicy>,
    pub audit: Arc<TestAudit>,
    pub outbox: Mutex<Option<OutboxHandle>>,
    /// Separate read connection for assertions.
    pub raw_db: sea_orm::DatabaseConnection,
    _dir: tempfile::TempDir,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.svc.shutdown.cancel();
    }
}

impl TestEnv {
    pub async fn start() -> Self {
        Self::with(EnvOptions::default()).await
    }

    pub async fn with(opts: EnvOptions) -> Self {
        let mock = MockProvider::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let dsn = format!("sqlite://{}?mode=rwc", dir.path().join("mini_chat.db").display());
        let db = connect_db(&dsn, ConnectOpts { max_conns: Some(4), ..ConnectOpts::default() }).await.expect("db");
        toolkit_db::migration_runner::run_migrations_for_testing(&db, mini_chat::infra::db::migrations::all_migrations())
            .await
            .expect("migrations");

        let mut cfg = json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
            "providers": {
                "mock": {
                    "kind": "openai_responses",
                    "host": "127.0.0.1",
                    "port": mock.addr.port(),
                    "use_http": true,
                    "upstream_alias": "mock-openai",
                    "api_path": "/v1/responses",
                    "storage_kind": "openai",
                    "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                    "auth_config": {"header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key"}
                }
            }
        });
        merge(&mut cfg, opts.config);
        let cfg: MiniChatConfig = serde_json::from_value(cfg).expect("gear config");
        cfg.validate().expect("valid config");

        let hub = Arc::new(ClientHub::new());
        let oagw = build_test_gateway(
            &hub,
            TestCpBuilder::new().with_credentials(vec![("openai-key".into(), "sk-test-secret".into())]),
            TestDpBuilder::new(),
        );
        let pdp = Arc::new(PdpStub::default());
        let policy = Arc::new(TestPolicy::new(opts.catalog));
        let audit = Arc::new(TestAudit::default());
        let provider = Arc::new(DBProvider::<DomainError>::new(db.clone()));
        let svc = build_services(
            cfg,
            provider,
            PolicyEnforcer::new(pdp.clone()),
            Arc::new(PolicyGateway::fixed(hub.clone(), policy.clone())),
            Arc::new(AuditGateway::fixed(hub.clone(), audit.clone())),
            oagw,
        );
        svc.llm.gateway.set_context(
            SecurityContext::builder().subject_tenant_id(SYS_TENANT).subject_id(SYS_USER).build().unwrap(),
        );
        let pending = Provisioner::new(svc.llm.gateway.clone(), svc.llm.resolver.clone()).provision_all().await;
        assert!(pending.is_empty(), "provider provisioning failed");
        let handle = if opts.outbox { Some(start_outbox(&svc, db.clone()).await.expect("outbox")) } else { None };

        let registry = toolkit::OpenApiRegistryImpl::new();
        let router = mini_chat::api::rest::register_routes(Router::new(), &registry, svc.clone())
            .layer(axum::middleware::from_fn(inject_identity));
        let raw_db = sea_orm::Database::connect(dsn.as_str()).await.expect("raw db");
        Self { svc, router, mock, db, pdp, policy, audit, outbox: Mutex::new(handle), raw_db, _dir: dir }
    }

    /// Stops the outbox pipeline (for tests that drive handlers directly).
    pub async fn stop_outbox(&self) {
        let h = self.outbox.lock().unwrap().take();
        if let Some(h) = h {
            h.stop().await;
        }
    }

    /// Sends a request as `who`.
    pub async fn call(&self, who: &str, method: Method, path: &str, body: Option<Value>) -> Response {
        let mut b = Request::builder().method(method).uri(format!("/mini-chat/v1{path}")).header("x-test-user", who);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap()
    }

    /// Sends a raw request as `who`.
    pub async fn raw(&self, who: &str, req: axum::http::request::Builder, body: Body) -> Response {
        let req = req.header("x-test-user", who).body(body).unwrap();
        self.router.clone().oneshot(req).await.unwrap()
    }

    /// JSON request returning `(status, body)`.
    pub async fn json(&self, who: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let r = self.call(who, method, path, body).await;
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
        (status, v)
    }

    /// Creates a chat as `who` and returns its id.
    pub async fn create_chat(&self, who: &str, body: Value) -> Uuid {
        let (s, v) = self.json(who, Method::POST, "/chats", Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().parse().unwrap()
    }

    /// Streams a message and collects the SSE events (JSON error when not 200).
    pub async fn stream(&self, who: &str, chat: Uuid, body: Value) -> StreamResult {
        self.stream_path(who, &format!("/chats/{chat}/messages:stream"), Method::POST, body).await
    }

    /// Streams on any SSE route.
    pub async fn stream_path(&self, who: &str, path: &str, method: Method, body: Value) -> StreamResult {
        let r = self.call(who, method, path, Some(body)).await;
        let status = r.status();
        let headers = r.headers().clone();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        if status != StatusCode::OK {
            return StreamResult { status, events: Vec::new(), error: serde_json::from_str(&text).unwrap_or(Value::Null), headers };
        }
        StreamResult { status, events: parse_sse(&text), error: Value::Null, headers }
    }

    /// Streams and drops the response after `n` events (client disconnect).
    pub async fn stream_and_disconnect(&self, who: &str, chat: Uuid, body: Value, n: usize) -> Vec<(String, Value)> {
        let r = self.call(who, Method::POST, &format!("/chats/{chat}/messages:stream"), Some(body)).await;
        assert_eq!(r.status(), StatusCode::OK);
        let mut s = r.into_body().into_data_stream();
        let mut buf = String::new();
        let mut events = Vec::new();
        while events.len() < n {
            let Some(Ok(chunk)) = tokio::time::timeout(Duration::from_secs(10), s.next()).await.ok().flatten() else { break };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            events = parse_sse(&buf);
        }
        drop(s);
        events
    }

    /// Runs a closure until it returns `Some` or the timeout elapses.
    pub async fn eventually<T, F, Fut>(&self, timeout: Duration, mut f: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(v) = f().await {
                return Some(v);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Raw SQL query against the gear database (tests only).
    pub async fn query(&self, sql: &str) -> Vec<sea_orm::QueryResult> {
        use sea_orm::{ConnectionTrait, Statement};
        let conn = &self.raw_db;
        conn.query_all_raw(Statement::from_string(conn.get_database_backend(), sql.to_owned())).await.expect("query")
    }

    /// Executes a raw SQL statement (test setup only, e.g. aging rows).
    pub async fn exec(&self, sql: &str) {
        use sea_orm::ConnectionTrait;
        self.raw_db.execute_unprepared(sql).await.expect("exec");
    }

    /// Single integer from a SQL query.
    pub async fn count(&self, sql: &str) -> i64 {
        let rows = self.query(sql).await;
        rows.first().and_then(|r| r.try_get_by_index::<i64>(0).ok()).unwrap_or(0)
    }
}

/// Result of a streaming request.
#[derive(Debug)]
pub struct StreamResult {
    pub status: StatusCode,
    pub events: Vec<(String, Value)>,
    pub error: Value,
    pub headers: axum::http::HeaderMap,
}

impl StreamResult {
    pub fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).filter(|n| *n != "ping").collect()
    }

    pub fn event(&self, name: &str) -> Option<&Value> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn text(&self) -> String {
        self.events
            .iter()
            .filter(|(n, _)| n == "delta")
            .filter_map(|(_, v)| v["content"].as_str())
            .collect()
    }

    pub fn last(&self) -> &(String, Value) {
        self.events.last().expect("no events")
    }
}

/// Parses SSE frames (`event:` / `data:`), ignoring comments.
pub fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = String::from("message");
        let mut data = String::new();
        let mut any = false;
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                v.trim().clone_into(&mut name);
                any = true;
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim_start());
                any = true;
            }
        }
        if any {
            out.push((name, serde_json::from_str(&data).unwrap_or(Value::String(data))));
        }
    }
    out
}

async fn inject_identity(mut req: Request<Body>, next: axum::middleware::Next) -> Response {
    let who = req.headers().get("x-test-user").and_then(|v| v.to_str().ok()).unwrap_or("a1").to_owned();
    let (tenant, user) = identity(&who);
    let ctx = SecurityContext::builder().subject_tenant_id(tenant).subject_id(user).build().unwrap();
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// Builds a multipart body with one `file` part.
pub fn multipart(filename: &str, content_type: Option<&str>, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = "XTESTBOUNDARYx";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n").as_bytes());
    if let Some(ct) = content_type {
        body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A PNG image of `w x h` pixels.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 10, 10, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Asserts a canonical problem response.
pub fn assert_problem(status: StatusCode, body: &Value, expected_status: u16, category: &str) {
    assert_eq!(status.as_u16(), expected_status, "unexpected status, body: {body}");
    let ty = body["type"].as_str().unwrap_or_default();
    assert!(ty.contains(&format!("cf.core.err.{category}.v1~")), "expected category {category}, got {ty} ({body})");
}

/// First field violation reason.
pub fn violation_reason(body: &Value) -> Option<String> {
    body["context"]["field_violations"][0]["reason"].as_str().map(str::to_owned)
}

/// UUID as SQLite BLOB literal.
pub fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}
