//! Test harness: the gear's router over a temporary SQLite database, a mock
//! PDP, the static policy plugin, a recording audit plugin and a scripted
//! fake provider transport that records every outbound request.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::pep::PolicyEnforcer;
use authz_resolver_sdk::{AuthZResolverApi, Constraint, EqPredicate, Predicate};
use axum::Router;
use axum::body::Body as AxumBody;
use bytes::Bytes;
use http_body_util::BodyExt;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::app::App;
use mini_chat::domain::error::DomainError;
use mini_chat::infra::gateways::{AuditGateway, PolicyGateway};
use mini_chat::infra::llm::{ProviderTransport, TransportError};
use mini_chat::infra::plugins::static_model_policy::{StaticModelPolicyConfig, StaticModelPolicyService};
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1, PolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use oagw_sdk::Body;
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;
use toolkit_security::pep_properties;
use toolkit_security::{PlatformSecurityContext, SecurityContext};
use tower::ServiceExt;
use mini_chat::infra::db::entity::{attachments, chat_turns, chat_vector_stores, messages, quota_usage, thread_summaries};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);
pub const USER_1: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_2: Uuid = Uuid::from_u128(0x4444_4444_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const USER_3: Uuid = Uuid::from_u128(0x2222_2222_6a88_4768_9dfc_6bcd_5187_d9ed);

// ── PDP ─────────────────────────────────────────────────────────────────────

/// 0 = allow (tenant constraint like static-authz), 1 = deny, 2 = fail.
pub struct MockPdp {
    pub mode: AtomicU8,
}

#[async_trait]
impl AuthZResolverApi for MockPdp {
    async fn evaluate(&self, _ctx: PlatformSecurityContext, req: EvaluationRequest) -> Result<EvaluationResponse, CanonicalError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => Ok(EvaluationResponse { decision: false, context: EvaluationResponseContext::default() }),
            2 => Err(CanonicalError::service_unavailable().with_detail("pdp down").create()),
            _ => {
                let tenant = req
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .unwrap();
                let constraints = if req.context.supported_properties.iter().any(|p| p == pep_properties::OWNER_TENANT_ID) {
                    vec![Constraint {
                        predicates: vec![Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant))],
                    }]
                } else {
                    Vec::new()
                };
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext { constraints, ..Default::default() },
                })
            }
        }
    }
}

// ── Policy plugin (static) with publish recording ───────────────────────────

pub struct RecordingPolicy {
    pub inner: StaticModelPolicyService,
    pub published: Mutex<Vec<UsageEvent>>,
    /// Number of upcoming publish calls that fail transiently.
    pub fail_publish: std::sync::atomic::AtomicU32,
    pub publish_attempts: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for RecordingPolicy {
    async fn get_current_policy_version(&self, user_id: Uuid) -> Result<PolicyVersionInfo, PolicyPluginError> {
        self.inner.get_current_policy_version(user_id).await
    }
    async fn get_policy_snapshot(&self, user_id: Uuid, v: u64) -> Result<PolicySnapshot, PolicyPluginError> {
        self.inner.get_policy_snapshot(user_id, v).await
    }
    async fn get_user_limits(&self, user_id: Uuid, v: u64) -> Result<UserLimits, PolicyPluginError> {
        self.inner.get_user_limits(user_id, v).await
    }
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.publish_attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .fail_publish
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(PublishError::Transient("injected".into()));
        }
        self.published.lock().unwrap().push(payload);
        Ok(())
    }
}

#[derive(Default)]
pub struct RecordingAudit {
    pub events: Mutex<Vec<MiniChatAuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

// ── Fake provider ───────────────────────────────────────────────────────────

/// A recorded outbound request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub uri: String,
    pub json: Option<Value>,
    pub body_len: usize,
}

/// Scripted reply of a streaming chat request.
pub enum Script {
    /// SSE frames `(event, data)` sent at once.
    Sse(Vec<(String, Value)>),
    /// Frames are pushed by the test through the channel; `dropped` is set
    /// when the gear drops the response stream.
    Channel(tokio::sync::mpsc::UnboundedReceiver<Bytes>, Arc<AtomicBool>),
    /// Provider HTTP error.
    Http(u16, Value, Vec<(String, String)>),
    /// Gateway failure.
    Gateway(TransportError),
}

pub struct FakeProvider {
    pub requests: Mutex<Vec<Recorded>>,
    pub scripts: Mutex<VecDeque<Script>>,
    pub summary_replies: Mutex<VecDeque<Result<Value, u16>>>,
    pub upload_status: Mutex<u16>,
    pub index_statuses: Mutex<VecDeque<&'static str>>,
    pub delete_status: Mutex<u16>,
    counter: Mutex<u64>,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(VecDeque::new()),
            summary_replies: Mutex::new(VecDeque::new()),
            upload_status: Mutex::new(200),
            index_statuses: Mutex::new(VecDeque::new()),
            delete_status: Mutex::new(200),
            counter: Mutex::new(0),
        }
    }
}

pub fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Standard completed stream: `deltas`, then `response.completed` with usage.
pub fn completed(deltas: &[&str], input: i64, output: i64) -> Script {
    let mut f = vec![("response.created".to_owned(), json!({"type": "response.created", "response": {"id": "resp_abcdef123456"}}))];
    for d in deltas {
        f.push(("response.output_text.delta".to_owned(), json!({"type": "response.output_text.delta", "delta": d})));
    }
    f.push((
        "response.completed".to_owned(),
        json!({"type": "response.completed", "response": {"id": "resp_abcdef123456", "usage": {"input_tokens": input, "output_tokens": output}}}),
    ));
    Script::Sse(f)
}

impl FakeProvider {
    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().unwrap();
        *c += 1;
        format!("{prefix}{:024}", *c)
    }

    pub fn push(&self, s: Script) {
        self.scripts.lock().unwrap().push_back(s);
    }

    /// Pushes a channel script and returns the sender and the dropped flag.
    pub fn push_channel(&self) -> (tokio::sync::mpsc::UnboundedSender<Bytes>, Arc<AtomicBool>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicBool::new(false));
        self.push(Script::Channel(rx, Arc::clone(&dropped)));
        (tx, dropped)
    }

    pub fn chat_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.uri.ends_with("/responses") && r.json.as_ref().is_some_and(|j| j["stream"] == true))
            .filter_map(|r| r.json.clone())
            .collect()
    }

    pub fn summary_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.uri.ends_with("/responses") && r.json.as_ref().is_some_and(|j| j["stream"] == false))
            .filter_map(|r| r.json.clone())
            .collect()
    }

    pub fn calls(&self, method: &str, contains: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|r| r.method == method && r.uri.contains(contains)).count()
    }
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn json_response(status: u16, v: &Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

#[async_trait]
impl ProviderTransport for FakeProvider {
    async fn send(&self, req: http::Request<Body>) -> Result<http::Response<Body>, TransportError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap_or_default();
        let json: Option<Value> = serde_json::from_slice(&bytes).ok();
        let uri = parts.uri.to_string();
        let method = parts.method.to_string();
        self.requests.lock().unwrap().push(Recorded { method: method.clone(), uri: uri.clone(), json: json.clone(), body_len: bytes.len() });
        let path = uri.split('?').next().unwrap_or_default().to_owned();
        if path.ends_with("/responses") {
            if json.as_ref().is_some_and(|j| j["stream"] == false) {
                let reply = self.summary_replies.lock().unwrap().pop_front().unwrap_or_else(|| {
                    Ok(json!({"output_text": "<analysis>x</analysis><summary>Summary text</summary>", "usage": {"input_tokens": 40, "output_tokens": 12}}))
                });
                return Ok(match reply {
                    Ok(v) => json_response(200, &v),
                    Err(code) => json_response(code, &json!({"error": {"message": "summary failed"}})),
                });
            }
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| completed(&["Hello", " world"], 10, 5));
            return match script {
                Script::Sse(frames) => {
                    let text: String = frames.iter().map(|(e, d)| frame(e, d)).collect();
                    Ok(http::Response::builder().status(200).header("content-type", "text/event-stream").body(Body::from(text)).unwrap())
                }
                Script::Channel(rx, dropped) => {
                    let flag = DropFlag(dropped);
                    let stream = futures::stream::unfold((rx, flag), |(mut rx, flag)| async move {
                        rx.recv().await.map(|b| (Ok::<Bytes, oagw_sdk::body::BoxError>(b), (rx, flag)))
                    });
                    let body: oagw_sdk::body::BodyStream = Box::pin(stream);
                    Ok(http::Response::builder().status(200).header("content-type", "text/event-stream").body(Body::from(body)).unwrap())
                }
                Script::Http(code, v, headers) => {
                    let mut b = http::Response::builder().status(code).header("content-type", "application/json");
                    for (k, val) in headers {
                        b = b.header(k, val);
                    }
                    Ok(b.body(Body::from(v.to_string())).unwrap())
                }
                Script::Gateway(e) => Err(e),
            };
        }
        if method == "POST" && path.ends_with("/files") && !path.contains("vector_stores") {
            let status = *self.upload_status.lock().unwrap();
            if status != 200 {
                return Ok(json_response(status, &json!({"error": {"message": "upload failed"}})));
            }
            return Ok(json_response(200, &json!({"id": self.next_id("file-")})));
        }
        if method == "POST" && path.ends_with("/vector_stores") {
            return Ok(json_response(200, &json!({"id": self.next_id("vs_")})));
        }
        if path.contains("/vector_stores/") && path.ends_with("/files") && method == "POST" {
            let s = self.index_statuses.lock().unwrap().pop_front().unwrap_or("completed");
            return Ok(json_response(200, &json!({"status": s})));
        }
        if path.contains("/vector_stores/") && method == "GET" {
            let s = self.index_statuses.lock().unwrap().pop_front().unwrap_or("completed");
            return Ok(json_response(200, &json!({"status": s})));
        }
        if method == "DELETE" {
            let status = *self.delete_status.lock().unwrap();
            return Ok(json_response(status, &json!({"deleted": status == 200})));
        }
        Ok(json_response(404, &json!({"error": {"message": "unknown"}})))
    }
}

// ── Catalog ─────────────────────────────────────────────────────────────────

pub fn model_entry(id: &str, tier: &str, mult_in: u64, mult_out: u64, extra: Value) -> Value {
    let mut v = json!({
        "id": id, "provider_model_id": format!("{id}-provider"), "display_name": id.to_uppercase(),
        "description": format!("{id} model"), "provider_id": "openai", "provider_display_name": "OpenAI",
        "tier": tier, "enabled": true, "system_prompt": format!("You are {id}."),
        "multimodal_capabilities": ["VISION_INPUT"], "context_window": 128_000, "max_output_tokens": 1000,
        "max_input_tokens": 100_000, "input_tokens_credit_multiplier_micro": mult_in,
        "output_tokens_credit_multiplier_micro": mult_out, "multiplier_display": "1x",
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 100, "safety_margin_pct": 10,
            "image_token_budget": 1000, "tool_surcharge_tokens": 500, "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000, "minimal_generation_floor": 50},
        "max_num_results": 5, "web_search_context_size": "low", "max_tool_calls": 3,
        "general_config": {"max_file_size_mb": 25, "api_params": {"temperature": 0.5},
            "tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": false, "sort_order": 0}
    });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in e {
            o.insert(k.clone(), val.clone());
        }
    }
    v
}

pub fn default_catalog() -> Value {
    json!([
        model_entry("premium-1", "premium", 3_000_000, 15_000_000, json!({"preference": {"is_default": true, "sort_order": 0}})),
        model_entry("standard-1", "standard", 1_000_000, 3_000_000, json!({})),
        model_entry("novision", "standard", 1_000_000, 1_000_000, json!({"multimodal_capabilities": [],
            "general_config": {"max_file_size_mb": 25, "tool_support": {"web_search": false, "file_search": false, "code_interpreter": false}}})),
        model_entry("tiny", "standard", 1_000_000, 1_000_000, json!({"context_window": 4096, "max_output_tokens": 1024, "max_input_tokens": 3072})),
        model_entry("disabled-1", "premium", 1_000_000, 1_000_000, json!({"enabled": false})),
    ])
}

// ── Harness ─────────────────────────────────────────────────────────────────

pub struct Options {
    pub catalog: Value,
    pub kill_switches: Value,
    pub standard_limits: (i64, i64),
    pub premium_limits: (i64, i64),
    pub config: Value,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            catalog: default_catalog(),
            kill_switches: json!({}),
            standard_limits: (100_000_000, 1_000_000_000),
            premium_limits: (50_000_000, 500_000_000),
            config: json!({}),
        }
    }
}

pub struct Harness {
    pub app: Arc<App>,
    pub router: Router,
    pub provider: Arc<FakeProvider>,
    pub policy: Arc<RecordingPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub pdp: Arc<MockPdp>,
    _outbox: Option<OutboxHandle>,
    pub db_path: String,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    pub async fn with(opts: Options) -> Self {
        let mut cfg_json = json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
            "providers": {"openai": {"kind": "openai_responses", "host": "127.0.0.1", "port": 1, "use_http": true,
                "upstream_alias": "mock", "storage_kind": "openai"}},
            "streaming": {"sse_ping_interval_seconds": 5},
            "thread_summary_worker": {"summary_model_id": "standard-1"},
            "orphan_watchdog": {"enabled": false},
            "upload_reaper": {"enabled": false}
        });
        merge(&mut cfg_json, &opts.config);
        let cfg: MiniChatConfig = serde_json::from_value(cfg_json).unwrap();
        cfg.validate().unwrap();

        let db_path = format!("/tmp/mini-chat-test-{}.db", Uuid::new_v4());
        let db = toolkit_db::connect_db(&format!("sqlite:{db_path}?mode=rwc"), toolkit_db::ConnectOpts::default())
            .await
            .unwrap();
        let mut migrations = <mini_chat::infra::db::migrations::Migrator as sea_orm_migration::MigratorTrait>::migrations();
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations).await.unwrap();

        let policy_cfg: StaticModelPolicyConfig = serde_json::from_value(json!({
            "model_catalog": opts.catalog,
            "kill_switches": opts.kill_switches,
            "default_standard_limits": {"limit_daily_credits_micro": opts.standard_limits.0, "limit_monthly_credits_micro": opts.standard_limits.1},
            "default_premium_limits": {"limit_daily_credits_micro": opts.premium_limits.0, "limit_monthly_credits_micro": opts.premium_limits.1}
        }))
        .unwrap();
        let policy = Arc::new(RecordingPolicy {
            inner: StaticModelPolicyService::from_config(&policy_cfg).unwrap(),
            published: Mutex::new(Vec::new()),
            fail_publish: std::sync::atomic::AtomicU32::new(0),
            publish_attempts: std::sync::atomic::AtomicU32::new(0),
        });
        let audit = Arc::new(RecordingAudit::default());
        let pdp = Arc::new(MockPdp { mode: AtomicU8::new(0) });
        let provider = Arc::new(FakeProvider::default());
        let mut app = App::new(
            cfg,
            DBProvider::<DomainError>::new(db),
            PolicyEnforcer::new(Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>),
            PolicyGateway::fixed(Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>),
            AuditGateway::fixed(Arc::clone(&audit) as Arc<dyn MiniChatAuditPluginClientV1>),
            Arc::clone(&provider) as Arc<dyn ProviderTransport>,
        );
        app.indexing_deadline = Duration::from_millis(1500);
        app.background_indexing_limit = Duration::from_secs(5);
        let app = Arc::new(app);
        let outbox = mini_chat::infra::outbox::handlers::start_outbox(&app).await.unwrap();
        let openapi = toolkit::api::OpenApiRegistryImpl::new();
        let router = mini_chat::api::rest::register_routes(Router::new(), &openapi, Arc::clone(&app), "/mini-chat")
            .layer(axum::middleware::from_fn(inject_ctx));
        Self { app, router, provider, policy, audit, pdp, _outbox: Some(outbox), db_path }
    }

    /// Sends a request as `(tenant, user)`.
    pub async fn call(&self, who: (Uuid, Uuid), method: &str, path: &str, body: Option<Value>) -> Resp {
        let mut b = http::Request::builder().method(method).uri(format!("/mini-chat/v1{path}"))
            .header("x-test-tenant", who.0.to_string())
            .header("x-test-user", who.1.to_string());
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                AxumBody::from(v.to_string())
            }
            None => AxumBody::empty(),
        };
        self.send(b.body(body).unwrap()).await
    }

    pub async fn send(&self, req: http::Request<AxumBody>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let bytes = tokio::time::timeout(Duration::from_secs(30), resp.into_body().collect())
            .await
            .expect("response body timed out")
            .unwrap()
            .to_bytes();
        Resp { status, headers, body: bytes }
    }

    /// Opens a streaming request and returns the live body.
    pub async fn open(&self, who: (Uuid, Uuid), method: &str, path: &str, body: Option<Value>) -> (u16, AxumBody) {
        let mut b = http::Request::builder().method(method).uri(format!("/mini-chat/v1{path}"))
            .header("x-test-tenant", who.0.to_string())
            .header("x-test-user", who.1.to_string());
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                AxumBody::from(v.to_string())
            }
            None => AxumBody::empty(),
        };
        let resp = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        (resp.status().as_u16(), resp.into_body())
    }

    pub async fn upload(&self, who: (Uuid, Uuid), chat: Uuid, filename: &str, content_type: &str, data: &[u8]) -> Resp {
        let boundary = "XBOUNDARYX";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
            .header("x-test-tenant", who.0.to_string())
            .header("x-test-user", who.1.to_string())
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(AxumBody::from(body))
            .unwrap();
        self.send(req).await
    }

    pub async fn create_chat(&self, who: (Uuid, Uuid), model: Option<&str>) -> Uuid {
        let body = match model {
            Some(m) => json!({"model": m}),
            None => json!({}),
        };
        let r = self.call(who, "POST", "/chats", Some(body)).await;
        assert_eq!(r.status, 201, "{}", r.text());
        Uuid::parse_str(r.json()["id"].as_str().unwrap()).unwrap()
    }

    /// Sends a message and returns the parsed SSE events.
    pub async fn send_message(&self, who: (Uuid, Uuid), chat: Uuid, body: Value) -> Resp {
        self.call(who, "POST", &format!("/chats/{chat}/messages:stream"), Some(body)).await
    }

    pub async fn turns(&self, chat: Uuid) -> Vec<chat_turns::Model> {
        let conn = self.app.db.conn().unwrap();
        chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat)))
            .order_by(chat_turns::Column::StartedAt, Order::Asc)
            .all(&conn)
            .await
            .unwrap()
    }

    pub async fn turn(&self, chat: Uuid, request_id: Uuid) -> chat_turns::Model {
        self.turns(chat).await.into_iter().find(|t| t.request_id == request_id).expect("turn")
    }

    pub async fn messages(&self, chat: Uuid) -> Vec<messages::Model> {
        let conn = self.app.db.conn().unwrap();
        messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(messages::Column::ChatId.eq(chat)))
            .order_by(messages::Column::CreatedAt, Order::Asc)
            .all(&conn)
            .await
            .unwrap()
    }

    pub async fn quota_rows(&self, user: Uuid) -> Vec<quota_usage::Model> {
        let conn = self.app.db.conn().unwrap();
        quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(quota_usage::Column::UserId.eq(user)))
            .all(&conn)
            .await
            .unwrap()
    }

    pub async fn quota(&self, user: Uuid, bucket: &str, period: &str) -> Option<quota_usage::Model> {
        self.quota_rows(user).await.into_iter().find(|r| r.bucket == bucket && r.period_type == period)
    }

    /// Sets spent credits of a bucket row for the current periods.
    pub async fn seed_spent(&self, tenant: Uuid, user: Uuid, bucket: &str, period: &str, spent: i64) {
        use sea_orm::ActiveValue::Set;
        let now = time::OffsetDateTime::now_utc();
        let p = mini_chat::domain::quota::PeriodStarts::at(now);
        let start = if period == "daily" { p.daily } else { p.monthly };
        let conn = self.app.db.conn().unwrap();
        let existing = self.quota(user, bucket, period).await;
        if let Some(r) = existing {
            quota_usage::Entity::update_many()
                .col_expr(quota_usage::Column::SpentCreditsMicro, sea_orm::sea_query::Expr::value(spent))
                .filter(Condition::all().add(quota_usage::Column::Id.eq(r.id)))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .exec(&conn)
                .await
                .unwrap();
            return;
        }
        let am = quota_usage::ActiveModel {
            id: Set(Uuid::new_v4()), tenant_id: Set(tenant), user_id: Set(user), period_type: Set(period.into()),
            period_start: Set(start), bucket: Set(bucket.into()), spent_credits_micro: Set(spent), reserved_credits_micro: Set(0),
            calls: Set(0), input_tokens: Set(0), output_tokens: Set(0), file_search_calls: Set(0), web_search_calls: Set(0),
            code_interpreter_calls: Set(0), rag_retrieval_calls: Set(0), image_inputs: Set(0), image_upload_bytes: Set(0),
            updated_at: Set(now),
        };
        quota_usage::Entity::insert(am).secure().scope_unchecked(&AccessScope::allow_all()).unwrap().exec(&conn).await.unwrap();
    }

    pub async fn attachment(&self, id: Uuid) -> attachments::Model {
        let conn = self.app.db.conn().unwrap();
        attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .one(&conn)
            .await
            .unwrap()
            .expect("attachment")
    }

    pub async fn vector_stores(&self, chat: Uuid) -> Vec<chat_vector_stores::Model> {
        let conn = self.app.db.conn().unwrap();
        chat_vector_stores::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat)))
            .all(&conn)
            .await
            .unwrap()
    }

    pub async fn summary(&self, chat: Uuid) -> Option<thread_summaries::Model> {
        let conn = self.app.db.conn().unwrap();
        thread_summaries::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat)))
            .one(&conn)
            .await
            .unwrap()
    }

    /// Updates a turn column (test-only manipulation, e.g. staleness).
    pub async fn age_turn(&self, turn_id: Uuid, secs: i64) {
        let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
        let conn = self.app.db.conn().unwrap();
        chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, sea_orm::sea_query::Expr::value(Some(t)))
            .col_expr(chat_turns::Column::StartedAt, sea_orm::sea_query::Expr::value(t))
            .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    pub async fn age_attachment(&self, id: Uuid, secs: i64) {
        let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs);
        let conn = self.app.db.conn().unwrap();
        attachments::Entity::update_many()
            .col_expr(attachments::Column::UpdatedAt, sea_orm::sea_query::Expr::value(t))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    /// Forces an attachment status (test-only, e.g. an interrupted upload).
    pub async fn set_attachment_status(&self, id: Uuid, status: &str) {
        let conn = self.app.db.conn().unwrap();
        attachments::Entity::update_many()
            .col_expr(attachments::Column::Status, sea_orm::sea_query::Expr::value(status))
            .filter(Condition::all().add(attachments::Column::Id.eq(id)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .unwrap();
    }

    /// Waits until the outbox has delivered `n` usage events.
    pub async fn wait_published(&self, n: usize) -> Vec<UsageEvent> {
        for _ in 0..200 {
            let p = self.policy.published.lock().unwrap().clone();
            if p.len() >= n {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.policy.published.lock().unwrap().clone()
    }

    pub async fn wait_audit(&self, n: usize) -> Vec<MiniChatAuditEvent> {
        for _ in 0..200 {
            let p = self.audit.events.lock().unwrap().clone();
            if p.len() >= n {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.audit.events.lock().unwrap().clone()
    }

    pub async fn wait_until<F: Fn() -> bool>(&self, f: F) -> bool {
        for _ in 0..300 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(format!("{}-wal", self.db_path));
        let _ = std::fs::remove_file(format!("{}-shm", self.db_path));
    }
}

fn merge(base: &mut Value, extra: &Value) {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            if v.is_object() && b.get(k).is_some_and(Value::is_object) {
                merge(b.get_mut(k).unwrap(), v);
            } else {
                b.insert(k.clone(), v.clone());
            }
        }
    }
}

fn header_uuid(headers: &http::HeaderMap, name: &str, default: Uuid) -> Uuid {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or(default)
}

async fn inject_ctx(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let tenant = header_uuid(req.headers(), "x-test-tenant", TENANT_A);
    let user = header_uuid(req.headers(), "x-test-user", USER_1);
    let ctx = SecurityContext::builder().subject_id(user).subject_tenant_id(tenant).token_scopes(vec!["*".into()]).build().unwrap();
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// Response of a test call.
pub struct Resp {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    /// Parsed SSE events `(name, data)`.
    pub fn events(&self) -> Vec<(String, Value)> {
        parse_sse(&self.text())
    }
    pub fn event_names(&self) -> Vec<String> {
        self.events().into_iter().map(|(n, _)| n).collect()
    }
    pub fn event(&self, name: &str) -> Option<Value> {
        self.events().into_iter().find(|(n, _)| n == name).map(|(_, d)| d)
    }
    /// Machine reason of a Problem (context.reason / first field violation / first violation subject).
    pub fn reason(&self) -> String {
        let c = &self.json()["context"];
        if let Some(r) = c["reason"].as_str() {
            return r.to_owned();
        }
        if let Some(r) = c["field_violations"][0]["reason"].as_str() {
            return r.to_owned();
        }
        if let Some(r) = c["violations"][0]["type"].as_str() {
            return format!("{}:{}", c["violations"][0]["subject"].as_str().unwrap_or_default(), r);
        }
        if let Some(r) = c["violations"][0]["subject"].as_str() {
            return r.to_owned();
        }
        if let Some(r) = c["resource_name"].as_str() {
            return r.to_owned();
        }
        String::new()
    }
}

pub fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                name = Some(v.trim().to_owned());
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim_start());
            }
        }
        if let Some(n) = name {
            out.push((n, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    out
}

/// Reads frames of a live SSE body until `pred` matches an event or timeout.
pub async fn read_until(body: &mut AxumBody, buf: &mut String, pred: impl Fn(&str, &Value) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if parse_sse(buf).iter().any(|(n, d)| pred(n, d)) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(f))) => {
                if let Some(d) = f.data_ref() {
                    buf.push_str(&String::from_utf8_lossy(d));
                }
            }
            _ => return parse_sse(buf).iter().any(|(n, d)| pred(n, d)),
        }
    }
}

pub fn a(t: Uuid, u: Uuid) -> (Uuid, Uuid) {
    (t, u)
}

pub const U1: (Uuid, Uuid) = (TENANT_A, USER_1);
pub const U2: (Uuid, Uuid) = (TENANT_A, USER_2);
pub const U3: (Uuid, Uuid) = (TENANT_B, USER_3);
