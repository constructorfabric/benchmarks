//! Shared integration-test harness: SQLite DB with the gear migrations, a
//! fake OAGW gateway standing in for the OpenAI-compatible provider, a
//! switchable PDP, test policy/audit plugins and the gear's REST router.

#![allow(dead_code, clippy::missing_panics_doc, clippy::must_use_candidate, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::Body as AxumBody;
use bytes::Bytes;
use futures::StreamExt;
use http::{Request, StatusCode};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::authz::Authz;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::services::{MiniChatService, ServiceDeps, Timings};
use mini_chat::infra::audit::AuditGateway;
use mini_chat::infra::llm::storage::StorageClient;
use mini_chat::infra::llm::{LlmClient, ProviderResolver, S2sContext};
use mini_chat::infra::metrics::Metrics;
use mini_chat::infra::outbox::OutboxEnqueuer;
use mini_chat::infra::outbox::handlers::start_pipeline;
use mini_chat::infra::policy::PolicyGateway;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, KillSwitches, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits,
    UsageEvent, UserLimits,
};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::Body as GwBody;
use oagw_sdk::{CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest, UpdateUpstreamRequest, Upstream};
use parking_lot::Mutex;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------

pub const TENANT_A: Uuid = uuid::uuid!("00000000-df51-5b42-9538-d2b56b7ee953");
pub const TENANT_B: Uuid = uuid::uuid!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
pub const USER_A: Uuid = uuid::uuid!("11111111-6a88-4768-9dfc-6bcd5187d9ed");
pub const USER_A2: Uuid = uuid::uuid!("44444444-6a88-4768-9dfc-6bcd5187d9ed");
pub const USER_B: Uuid = uuid::uuid!("22222222-6a88-4768-9dfc-6bcd5187d9ed");

pub fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("ctx")
}

pub fn user_a() -> SecurityContext {
    ctx(USER_A, TENANT_A)
}
pub fn user_a2() -> SecurityContext {
    ctx(USER_A2, TENANT_A)
}
pub fn user_b() -> SecurityContext {
    ctx(USER_B, TENANT_B)
}

// ---------------------------------------------------------------------------
// PDP mock
// ---------------------------------------------------------------------------

pub const PDP_ALLOW: u8 = 0;
pub const PDP_DENY: u8 = 1;
pub const PDP_FAIL: u8 = 2;

/// Static-authz-like PDP: allows with `owner_tenant_id IN (subject tenant)`;
/// can be switched to deny or to fail.
pub struct TestPdp {
    pub mode: AtomicU8,
    pub calls: AtomicUsize,
    pub requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait]
impl AuthZResolverApi for TestPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().push(request.clone());
        match self.mode.load(Ordering::SeqCst) {
            PDP_DENY => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            PDP_FAIL => Err(CanonicalError::service_unavailable().with_detail("pdp down").create()),
            _ => {
                let tid = request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .unwrap_or_default();
                if tid.is_nil() {
                    return Ok(EvaluationResponse {
                        decision: false,
                        context: EvaluationResponseContext::default(),
                    });
                }
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext {
                        constraints: vec![Constraint {
                            predicates: vec![Predicate::In(InPredicate::new(pep_properties::OWNER_TENANT_ID, [tid]))],
                        }],
                        ..Default::default()
                    },
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Model policy plugin (mutable)
// ---------------------------------------------------------------------------

pub struct TestPolicy {
    pub version: Mutex<u64>,
    pub snapshots: Mutex<HashMap<u64, PolicySnapshot>>,
    pub standard: Mutex<TierLimits>,
    pub premium: Mutex<TierLimits>,
    pub published: Mutex<Vec<UsageEvent>>,
    pub publish_fail: AtomicBool,
    pub fail: AtomicBool,
}

impl TestPolicy {
    pub fn new(catalog: Vec<ModelCatalogEntry>) -> Self {
        let mut snaps = HashMap::new();
        snaps.insert(
            1,
            PolicySnapshot {
                policy_version: 1,
                model_catalog: catalog,
                kill_switches: KillSwitches::default(),
            },
        );
        Self {
            version: Mutex::new(1),
            snapshots: Mutex::new(snaps),
            standard: Mutex::new(TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            }),
            premium: Mutex::new(TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            }),
            published: Mutex::new(Vec::new()),
            publish_fail: AtomicBool::new(false),
            fail: AtomicBool::new(false),
        }
    }

    /// Publishes a new policy version produced by `f` from the current one.
    pub fn update(&self, f: impl FnOnce(&mut PolicySnapshot)) {
        let mut v = self.version.lock();
        let mut snap = self.snapshots.lock().get(&*v).cloned().expect("snapshot");
        *v += 1;
        snap.policy_version = *v;
        f(&mut snap);
        self.snapshots.lock().insert(*v, snap);
    }

    pub fn current(&self) -> PolicySnapshot {
        let v = *self.version.lock();
        self.snapshots.lock().get(&v).cloned().expect("snapshot")
    }

    pub fn set_limits(&self, standard: (i64, i64), premium: (i64, i64)) {
        *self.standard.lock() = TierLimits {
            limit_daily_credits_micro: standard.0,
            limit_monthly_credits_micro: standard.1,
        };
        *self.premium.lock() = TierLimits {
            limit_daily_credits_micro: premium.0,
            limit_monthly_credits_micro: premium.1,
        };
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for TestPolicy {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Internal("policy down".to_owned()));
        }
        Ok(PolicyVersionInfo {
            policy_version: *self.version.lock(),
            generated_at: time::OffsetDateTime::now_utc(),
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::Internal("policy down".to_owned()));
        }
        self.snapshots
            .lock()
            .get(&policy_version)
            .cloned()
            .ok_or_else(|| MiniChatModelPolicyPluginError::Internal("unknown version".to_owned()))
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: *self.standard.lock(),
            premium: *self.premium.lock(),
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        if self.publish_fail.load(Ordering::SeqCst) {
            return Err(PublishError::Transient("plugin busy".to_owned()));
        }
        self.published.lock().push(payload);
        Ok(())
    }
}

/// Records audit events.
#[derive(Default)]
pub struct TestAudit {
    pub events: Mutex<Vec<AuditEvent>>,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for TestAudit {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        self.events.lock().push(event);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fake OAGW gateway / provider
// ---------------------------------------------------------------------------

/// One recorded outbound request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub uri: String,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
    pub subject: Uuid,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Scripted reply of the `/responses` endpoint.
pub enum Reply {
    /// Complete SSE body (frames).
    Sse(Vec<String>),
    /// Frames pushed by the test; the stream ends when the sender is dropped.
    Channel(mpsc::UnboundedReceiver<String>),
    /// Upstream HTTP error with a JSON body.
    Status(u16, Value),
    /// Gateway-originated HTTP error.
    Gateway(u16),
    /// Gateway call error (e.g. header timeout).
    Error(CanonicalError),
    /// Non-streaming JSON body.
    Json(Value),
}

/// Fake `ServiceGatewayClientV1`: records every proxied request and answers
/// like an OpenAI-compatible provider.
pub struct FakeGateway {
    pub requests: Mutex<Vec<Recorded>>,
    pub replies: Mutex<VecDeque<Reply>>,
    pub summary_replies: Mutex<VecDeque<Reply>>,
    pub files_fail: AtomicBool,
    pub vs_status: Mutex<String>,
    pub vs_add_fail: AtomicBool,
    pub delete_fail: AtomicBool,
    pub counter: AtomicUsize,
}

impl Default for FakeGateway {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
            summary_replies: Mutex::new(VecDeque::new()),
            files_fail: AtomicBool::new(false),
            vs_status: Mutex::new("completed".to_owned()),
            vs_add_fail: AtomicBool::new(false),
            delete_fail: AtomicBool::new(false),
            counter: AtomicUsize::new(0),
        }
    }
}

/// One SSE frame of the Responses API.
pub fn frame(kind: &str, mut data: Value) -> String {
    if let Some(o) = data.as_object_mut() {
        o.insert("type".to_owned(), Value::String(kind.to_owned()));
    }
    format!("event: {kind}\ndata: {data}\n\n")
}

pub fn created_frame() -> String {
    frame("response.created", json!({"response": {"id": "resp_0123456789abcdef"}}))
}

pub fn delta_frame(t: &str) -> String {
    frame("response.output_text.delta", json!({"delta": t, "output_index": 0, "content_index": 0}))
}

pub fn completed_frame(input: i64, output: i64) -> String {
    frame(
        "response.completed",
        json!({"response": {"id": "resp_0123456789abcdef", "usage": {"input_tokens": input, "output_tokens": output,
            "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}}),
    )
}

/// A complete successful text response.
pub fn text_reply(parts: &[&str], input: i64, output: i64) -> Reply {
    let mut v = vec![created_frame()];
    v.extend(parts.iter().map(|p| delta_frame(p)));
    v.push(completed_frame(input, output));
    Reply::Sse(v)
}

impl FakeGateway {
    pub fn push(&self, r: Reply) {
        self.replies.lock().push_back(r);
    }

    pub fn push_summary(&self, r: Reply) {
        self.summary_replies.lock().push_back(r);
    }

    /// Opens a channel-backed reply; returns the frame sender.
    pub fn push_channel(&self) -> mpsc::UnboundedSender<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.push(Reply::Channel(rx));
        tx
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().clone()
    }

    /// Recorded requests whose path contains `needle`.
    pub fn calls(&self, needle: &str) -> Vec<Recorded> {
        self.recorded().into_iter().filter(|r| r.uri.contains(needle)).collect()
    }

    /// Streaming chat requests (`/responses` with `stream: true`).
    pub fn chat_calls(&self) -> Vec<Recorded> {
        self.calls("/responses")
            .into_iter()
            .filter(|r| r.json().get("stream").and_then(Value::as_bool) == Some(true))
            .collect()
    }

    /// Non-streaming (summary) requests.
    pub fn summary_calls(&self) -> Vec<Recorded> {
        self.calls("/responses")
            .into_iter()
            .filter(|r| r.json().get("stream").and_then(Value::as_bool) != Some(true))
            .collect()
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}{n:024}")
    }

    fn json_resp(status: u16, v: &Value) -> http::Response<GwBody> {
        http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(GwBody::from(serde_json::to_vec(v).unwrap_or_default()))
            .expect("response")
    }

    fn render(reply: Reply) -> Result<http::Response<GwBody>, CanonicalError> {
        match reply {
            Reply::Sse(frames) => {
                let chunks: Vec<Result<Bytes, Box<dyn std::error::Error + Send + Sync>>> =
                    frames.into_iter().map(|f| Ok(Bytes::from(f))).collect();
                Ok(http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(GwBody::Stream(Box::pin(futures::stream::iter(chunks))))
                    .expect("response"))
            }
            Reply::Channel(rx) => {
                let s = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
                    .map(|f| Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Bytes::from(f)));
                Ok(http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(GwBody::Stream(Box::pin(s)))
                    .expect("response"))
            }
            Reply::Status(code, v) => Ok(Self::json_resp(code, &v)),
            Reply::Gateway(code) => {
                let mut r = Self::json_resp(code, &json!({"error": "gateway"}));
                r.extensions_mut().insert(ErrorSource::Gateway);
                Ok(r)
            }
            Reply::Error(e) => Err(e),
            Reply::Json(v) => Ok(Self::json_resp(200, &v)),
        }
    }
}

fn unimplemented_err() -> CanonicalError {
    CanonicalError::internal("not supported by the fake gateway").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(&self, _ctx: SecurityContext, _req: CreateUpstreamRequest) -> Result<Upstream, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn get_upstream(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Upstream, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn list_upstreams(&self, _ctx: SecurityContext, _query: &ListQuery) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn delete_upstream(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn create_route(&self, _ctx: SecurityContext, _req: CreateRouteRequest) -> Result<Route, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Ok(Vec::new())
    }
    async fn update_route(&self, _ctx: SecurityContext, _id: Uuid, _req: UpdateRouteRequest) -> Result<Route, CanonicalError> {
        Err(unimplemented_err())
    }
    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }
    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unimplemented_err())
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<GwBody>,
    ) -> Result<http::Response<GwBody>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body.into_bytes().await.unwrap_or_default();
        let uri = parts.uri.to_string();
        let method = parts.method.to_string();
        let headers = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
            .collect();
        let rec = Recorded {
            method: method.clone(),
            uri,
            headers,
            body,
            subject: ctx.subject_id(),
        };
        self.requests.lock().push(rec.clone());
        let path = parts.uri.path().to_owned();

        if path.ends_with("/responses") {
            let streaming = rec.json().get("stream").and_then(Value::as_bool) == Some(true);
            if !streaming {
                let r = self.summary_replies.lock().pop_front().unwrap_or_else(|| {
                    Reply::Json(json!({
                        "id": "resp_summary",
                        "output": [{"type": "message", "content": [{"type": "output_text",
                            "text": "<analysis>a</analysis><summary>Earlier: the user greeted the assistant.</summary>"}]}],
                        "usage": {"input_tokens": 100, "output_tokens": 20}
                    }))
                });
                return Self::render(r);
            }
            let r = self
                .replies
                .lock()
                .pop_front()
                .unwrap_or_else(|| text_reply(&["Hello", " world"], 10, 5));
            return Self::render(r);
        }
        if path.ends_with("/files") && method == "POST" && !path.contains("/vector_stores/") {
            if self.files_fail.load(Ordering::SeqCst) {
                return Ok(Self::json_resp(500, &json!({"error": {"message": "files api down"}})));
            }
            return Ok(Self::json_resp(200, &json!({"id": self.next_id("file-"), "object": "file"})));
        }
        if path.contains("/files/") && method == "DELETE" && !path.contains("/vector_stores/") {
            if self.delete_fail.load(Ordering::SeqCst) {
                return Ok(Self::json_resp(500, &json!({"error": {"message": "delete failed"}})));
            }
            return Ok(Self::json_resp(200, &json!({"deleted": true})));
        }
        if path.ends_with("/vector_stores") && method == "POST" {
            return Ok(Self::json_resp(200, &json!({"id": self.next_id("vs_")})));
        }
        if path.contains("/vector_stores/") && path.ends_with("/files") && method == "POST" {
            if self.vs_add_fail.load(Ordering::SeqCst) {
                return Ok(Self::json_resp(500, &json!({"error": {"message": "vs add failed"}})));
            }
            let st = self.vs_status.lock().clone();
            return Ok(Self::json_resp(200, &json!({"id": "vsf", "status": st})));
        }
        if path.contains("/vector_stores/") && path.contains("/files/") && method == "GET" {
            let st = self.vs_status.lock().clone();
            return Ok(Self::json_resp(200, &json!({"id": "vsf", "status": st})));
        }
        if path.contains("/vector_stores/") && method == "DELETE" {
            if self.delete_fail.load(Ordering::SeqCst) {
                return Ok(Self::json_resp(500, &json!({"error": {"message": "delete failed"}})));
            }
            return Ok(Self::json_resp(200, &json!({"deleted": true})));
        }
        Ok(Self::json_resp(404, &json!({"error": {"message": "unknown path"}})))
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// Builds a catalog entry; `extra` is merged over the defaults.
pub fn model(id: &str, tier: &str, extra: Value) -> ModelCatalogEntry {
    let mut base = json!({
        "id": id,
        "provider_model_id": format!("{id}-provider"),
        "display_name": id.to_uppercase(),
        "description": format!("{id} model"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "max_num_results": 5,
        "enabled": true,
        "system_prompt": "You are a helpful assistant.",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128_000,
        "max_output_tokens": 1000,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "general_config": {
            "type": "model.general.v1",
            "max_file_size_mb": 25,
            "tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}
        }
    });
    merge(&mut base, extra);
    serde_json::from_value(base).expect("catalog entry")
}

fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

/// Default catalog: a premium default model and a standard model.
pub fn default_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        model("gpt-premium", "premium", json!({"preference": {"is_default": true}})),
        model(
            "gpt-standard",
            "standard",
            json!({"multimodal_capabilities": [], "input_tokens_credit_multiplier_micro": 500_000,
                   "output_tokens_credit_multiplier_micro": 1_000_000}),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct Options {
    pub catalog: Vec<ModelCatalogEntry>,
    pub config: Value,
    pub timings: Option<Timings>,
    pub start_outbox: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            catalog: default_catalog(),
            config: json!({}),
            timings: None,
            start_outbox: true,
        }
    }
}

pub struct Harness {
    pub svc: Arc<MiniChatService>,
    pub router: Router,
    pub gw: Arc<FakeGateway>,
    pub pdp: Arc<TestPdp>,
    pub policy: Arc<TestPolicy>,
    pub audit: Arc<TestAudit>,
    pub raw: DatabaseConnection,
    pub outbox: Option<OutboxHandle>,
    pub dir: tempfile::TempDir,
}

/// Parsed JSON response.
#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Value,
    pub text: String,
}

/// Parsed SSE response.
#[derive(Debug)]
pub struct SseResp {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub events: Vec<(String, Value)>,
    pub raw: String,
    pub problem: Value,
}

impl SseResp {
    pub fn names(&self) -> Vec<&str> {
        self.events.iter().map(|(n, _)| n.as_str()).filter(|n| *n != "ping").collect()
    }
    pub fn first(&self, name: &str) -> Option<&Value> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    pub fn text(&self) -> String {
        self.events
            .iter()
            .filter(|(n, v)| n == "delta" && v["type"] == "text")
            .map(|(_, v)| v["content"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
    pub fn request_id(&self) -> Uuid {
        let v = self.first("stream_started").expect("stream_started");
        Uuid::parse_str(v["request_id"].as_str().expect("request_id")).expect("uuid")
    }
    pub fn message_id(&self) -> Uuid {
        let v = self.first("stream_started").expect("stream_started");
        Uuid::parse_str(v["message_id"].as_str().expect("message_id")).expect("uuid")
    }
}

/// Parses an SSE body into `(event, data)` pairs.
pub fn parse_sse(raw: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in raw.split("\n\n") {
        let mut name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(n) = line.strip_prefix("event:") {
                name = Some(n.trim().to_owned());
            } else if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim_start());
            }
        }
        if let Some(n) = name {
            out.push((n, serde_json::from_str(&data).unwrap_or(Value::String(data))));
        }
    }
    out
}

pub fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    pub async fn with(opts: Options) -> Self {
        if std::env::var("MC_TEST_LOG").is_ok() {
            tracing_subscriber::fmt()
                .with_env_filter(std::env::var("MC_TEST_LOG").unwrap_or_default())
                .try_init()
                .ok();
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mini_chat.db");
        let dsn = format!("sqlite://{}?mode=rwc&journal_mode=wal&busy_timeout=10000", path.display());
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(8),
                ..Default::default()
            },
        )
        .await
        .expect("db");
        toolkit_db::migration_runner::run_migrations_for_testing(&db, mini_chat::infra::db::migrations::all_migrations())
            .await
            .expect("migrations");
        let raw = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("raw db");

        let mut cfg_json = json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
            "providers": {"openai": {"kind": "openai_responses", "host": "api.test.local", "storage_kind": "openai"}}
        });
        merge(&mut cfg_json, opts.config);
        let cfg: MiniChatConfig = serde_json::from_value(cfg_json).expect("config");
        cfg.validate().expect("valid config");
        let cfg = Arc::new(cfg);

        let pdp = Arc::new(TestPdp {
            mode: AtomicU8::new(PDP_ALLOW),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let gw = Arc::new(FakeGateway::default());
        let policy = Arc::new(TestPolicy::new(opts.catalog));
        let audit = Arc::new(TestAudit::default());
        let s2s = Arc::new(S2sContext::default());
        let resolver = Arc::new(ProviderResolver::new(cfg.providers.clone()));
        let gateway: Arc<dyn ServiceGatewayClientV1> = gw.clone();
        let llm = Arc::new(LlmClient::new(Arc::clone(&gateway), resolver, Arc::clone(&s2s)));
        let storage = Arc::new(StorageClient::new(gateway, s2s));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let policy_client: Arc<dyn MiniChatModelPolicyPluginClientV1> = policy.clone();
        let audit_client: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        let mut svc = MiniChatService::new(ServiceDeps {
            db: Arc::new(DBProvider::<DomainError>::new(db)),
            cfg: Arc::clone(&cfg),
            authz: Authz::new(PolicyEnforcer::new(pdp.clone())),
            policy: Arc::new(PolicyGateway::direct(policy_client)),
            audit: Arc::new(AuditGateway::direct(Some(audit_client))),
            llm,
            storage,
            outbox: Arc::clone(&outbox),
            metrics: Arc::new(Metrics::new("mini_chat")),
        });
        if let Some(t) = opts.timings {
            svc = svc.with_timings(t);
        }
        let svc = Arc::new(svc);
        let handle = if opts.start_outbox {
            let h = start_pipeline(&svc, true).await.expect("outbox");
            outbox.bind(Arc::clone(h.outbox()));
            Some(h)
        } else {
            None
        };
        let openapi = OpenApiRegistryImpl::new();
        let router = mini_chat::api::rest::register_routes(Router::new(), &openapi, Arc::clone(&svc));
        Self {
            svc,
            router,
            gw,
            pdp,
            policy,
            audit,
            raw,
            outbox: handle,
            dir,
        }
    }

    pub fn set_pdp(&self, mode: u8) {
        self.pdp.mode.store(mode, Ordering::SeqCst);
    }

    // --- HTTP ---

    pub async fn call(&self, req: Request<AxumBody>) -> http::Response<AxumBody> {
        self.router.clone().oneshot(req).await.expect("router")
    }

    fn build(method: &str, uri: &str, ctx: &SecurityContext, body: Option<&Value>) -> Request<AxumBody> {
        let mut b = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                AxumBody::from(serde_json::to_vec(v).expect("json"))
            }
            None => AxumBody::empty(),
        };
        let mut req = b.body(body).expect("request");
        req.extensions_mut().insert(ctx.clone());
        req
    }

    pub async fn into_resp(resp: http::Response<AxumBody>) -> Resp {
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024).await.expect("body");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            body,
            text,
        }
    }

    pub async fn req(&self, method: &str, uri: &str, ctx: &SecurityContext, body: Option<Value>) -> Resp {
        Self::into_resp(self.call(Self::build(method, uri, ctx, body.as_ref())).await).await
    }

    pub async fn get(&self, uri: &str, ctx: &SecurityContext) -> Resp {
        self.req("GET", uri, ctx, None).await
    }

    /// Sends a raw body with the given content type.
    pub async fn raw_body(&self, method: &str, uri: &str, ctx: &SecurityContext, content_type: Option<&str>, body: &str) -> Resp {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            b = b.header("content-type", ct);
        }
        let mut req = b.body(AxumBody::from(body.to_owned())).expect("request");
        req.extensions_mut().insert(ctx.clone());
        Self::into_resp(self.call(req).await).await
    }

    pub async fn sse(&self, method: &str, uri: &str, ctx: &SecurityContext, body: Option<Value>) -> SseResp {
        let resp = self.call(Self::build(method, uri, ctx, body.as_ref())).await;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = tokio::time::timeout(
            Duration::from_secs(30),
            axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024),
        )
        .await
        .expect("sse body timeout")
        .expect("sse body");
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let is_sse = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let events = if is_sse { parse_sse(&raw) } else { Vec::new() };
        let problem = if is_sse { Value::Null } else { serde_json::from_str(&raw).unwrap_or(Value::Null) };
        SseResp {
            status,
            headers,
            events,
            raw,
            problem,
        }
    }

    /// Opens a stream and returns the live response (for incremental reads).
    pub async fn open_stream(&self, uri: &str, ctx: &SecurityContext, body: Value) -> http::Response<AxumBody> {
        self.call(Self::build("POST", uri, ctx, Some(&body))).await
    }

    pub async fn multipart(
        &self,
        uri: &str,
        ctx: &SecurityContext,
        filename: &str,
        content_type: Option<&str>,
        data: &[u8],
    ) -> Resp {
        let boundary = "XtestBoundary1234";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n").as_bytes());
        if let Some(ct) = content_type {
            body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(AxumBody::from(body))
            .expect("request");
        req.extensions_mut().insert(ctx.clone());
        Self::into_resp(self.call(req).await).await
    }

    // --- shortcuts ---

    pub async fn create_chat(&self, ctx: &SecurityContext) -> Uuid {
        let r = self.req("POST", "/mini-chat/v1/chats", ctx, Some(json!({}))).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        Uuid::parse_str(r.body["id"].as_str().expect("id")).expect("uuid")
    }

    pub async fn create_chat_with(&self, ctx: &SecurityContext, body: Value) -> Resp {
        self.req("POST", "/mini-chat/v1/chats", ctx, Some(body)).await
    }

    pub async fn send(&self, ctx: &SecurityContext, chat: Uuid, content: &str) -> SseResp {
        self.sse(
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            ctx,
            Some(json!({"content": content})),
        )
        .await
    }

    pub async fn send_body(&self, ctx: &SecurityContext, chat: Uuid, body: Value) -> SseResp {
        self.sse("POST", &format!("/mini-chat/v1/chats/{chat}/messages:stream"), ctx, Some(body))
            .await
    }

    pub async fn messages(&self, ctx: &SecurityContext, chat: Uuid) -> Vec<Value> {
        let r = self.get(&format!("/mini-chat/v1/chats/{chat}/messages?limit=100"), ctx).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        r.body["items"].as_array().cloned().unwrap_or_default()
    }

    pub async fn upload(&self, ctx: &SecurityContext, chat: Uuid, filename: &str, ct: &str, data: &[u8]) -> Resp {
        self.multipart(&format!("/mini-chat/v1/chats/{chat}/attachments"), ctx, filename, Some(ct), data)
            .await
    }

    // --- DB ---

    /// Runs a query whose columns are all text (cast in SQL); NULL → None.
    pub async fn rows(&self, sql: &str) -> Vec<Vec<Option<String>>> {
        let res = self
            .raw
            .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"));
        res.iter()
            .map(|row| {
                let mut out = Vec::new();
                let mut i = 0;
                while let Ok(v) = row.try_get_by_index::<Option<String>>(i) {
                    out.push(v);
                    i += 1;
                }
                out
            })
            .collect()
    }

    pub async fn scalar(&self, sql: &str) -> i64 {
        let res = self
            .raw
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"))
            .expect("row");
        res.try_get_by_index::<i64>(0).expect("i64")
    }

    pub async fn exec(&self, sql: &str) {
        self.raw
            .execute_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("exec failed: {sql}: {e}"));
    }

    /// Polls `f` until it returns `Some` (outbox deliveries, background tasks).
    pub async fn eventually<T, F, Fut>(&self, what: &str, mut f: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(v) = f().await {
                return v;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the outbox has delivered all queued messages.
    pub async fn drain_outbox(&self) {
        self.eventually("outbox drained", || async {
            let pending = self
                .scalar(
                    "SELECT (SELECT COUNT(*) FROM toolkit_outbox_incoming) + \
                     (SELECT COUNT(*) FROM toolkit_outbox_outgoing o JOIN toolkit_outbox_processor p \
                      ON p.partition_id = o.partition_id WHERE o.seq > p.processed_seq)",
                )
                .await;
            (pending == 0).then_some(())
        })
        .await;
    }

    /// Quota row values `(spent, reserved, calls)` for a bucket/period.
    pub async fn quota(&self, user: Uuid, bucket: &str, period: &str) -> (i64, i64, i64) {
        let rows = self
            .rows(&format!(
                "SELECT CAST(spent_credits_micro AS TEXT), CAST(reserved_credits_micro AS TEXT), CAST(calls AS TEXT) \
                 FROM quota_usage WHERE user_id = {} AND bucket = '{bucket}' AND period_type = '{period}'",
                blob(user)
            ))
            .await;
        rows.first().map_or((0, 0, 0), |r| {
            let p = |i: usize| r[i].as_deref().unwrap_or("0").parse::<i64>().unwrap_or(0);
            (p(0), p(1), p(2))
        })
    }

    pub async fn turn_row(&self, request_id: Uuid) -> Vec<Option<String>> {
        self.rows(&format!(
            "SELECT state, error_code, CAST(reserve_tokens AS TEXT), CAST(reserved_credits_micro AS TEXT), \
             effective_model, CAST(deleted_at IS NOT NULL AS TEXT), hex(assistant_message_id) \
             FROM chat_turns WHERE request_id = {} ORDER BY started_at DESC",
            blob(request_id)
        ))
        .await
        .into_iter()
        .next()
        .unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.svc.shutdown_token().cancel();
    }
}

#[toolkit_canonical_errors::resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct GatewayResource;

/// The gateway's header-timeout error.
pub fn gateway_timeout() -> CanonicalError {
    GatewayResource::deadline_exceeded("upstream did not respond in time").create()
}
