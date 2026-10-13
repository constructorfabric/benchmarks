//! Test support: an in-memory service environment with fakes (tests only).
//!
//! `TestEnv::new(...)` builds `Deps`/`Services` over a migrated in-memory SQLite DB with:
//! - a PDP fake that behaves like `static-authz-plugin` (tenant constraint only),
//! - the static model policy (fixed catalog / kill switches / limits),
//! - a scripted [`FakeLlm`] and a recording [`FakeStorage`],
//! - a started outbox whose handlers only record delivered payloads.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc, dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use futures::stream::BoxStream;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits, UsageTokens};
use parking_lot::Mutex;
use sea_orm_migration::MigratorTrait;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxHandle, OutboxMessage};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::service::{Deps, Services};
use crate::infra::llm::{
    FileStorage, KnowledgeChunk, KnowledgeRetriever, LlmClient, LlmCompletion, LlmEvent,
    LlmFailure, LlmRequest, LlmTextResult, ProviderResolver, StorageError, VectorFileStatus,
};
use crate::infra::outbox::{self, MiniChatOutbox, OutboxHandlers};
use crate::infra::plugin_gateways::{AuditGateway, PolicyGateway};
use crate::infra::plugins::static_model_policy::{StaticModelPolicy, StaticModelPolicyConfig};

pub const TENANT_A: Uuid = Uuid::from_u128(0x00000000_df51_5b42_9538_d2b56b7ee953);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbbbbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb);
pub const USER_A1: Uuid = Uuid::from_u128(0x11111111_6a88_4768_9dfc_6bcd5187d9ed);
pub const USER_A2: Uuid = Uuid::from_u128(0x44444444_6a88_4768_9dfc_6bcd5187d9ed);
pub const USER_B: Uuid = Uuid::from_u128(0x22222222_6a88_4768_9dfc_6bcd5187d9ed);

#[must_use]
pub fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .build()
        .expect("ctx")
}

#[must_use]
pub fn ctx_a1() -> SecurityContext {
    ctx(USER_A1, TENANT_A)
}

// ── PDP fakes ──────────────────────────────────────────────────────────────

/// Behaves like `static-authz-plugin`: tenant constraint only (no owner predicate).
pub struct TenantPdp;

#[async_trait]
impl AuthZResolverApi for TenantPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("tenant");
        let supports_tenant = request
            .context
            .supported_properties
            .iter()
            .any(|p| p == pep_properties::OWNER_TENANT_ID);
        let constraints = if request.context.require_constraints && supports_tenant {
            vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, tenant))],
            }]
        } else {
            vec![]
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

/// Always denies.
pub struct DenyPdp;

#[async_trait]
impl AuthZResolverApi for DenyPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// Always fails evaluation (PDP outage).
pub struct FailingPdp;

#[async_trait]
impl AuthZResolverApi for FailingPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable().with_detail("pdp down").create())
    }
}

// ── Fake LLM ───────────────────────────────────────────────────────────────

/// One scripted provider response.
#[derive(Clone)]
pub enum Script {
    /// Events streamed in order (each after `delay`).
    Events { events: Vec<LlmEvent>, delay: Duration },
    /// Pre-stream failure.
    Fail(LlmFailure),
    /// Streams the events then never ends (until cancelled).
    Hang(Vec<LlmEvent>),
    /// The stream call itself takes `open_delay` (provider headers not received yet), then
    /// streams the events.
    SlowOpen {
        open_delay: Duration,
        events: Vec<LlmEvent>,
    },
}

/// Scripted LLM client recording every request.
#[derive(Default)]
pub struct FakeLlm {
    pub requests: Mutex<Vec<LlmRequest>>,
    pub scripts: Mutex<VecDeque<Script>>,
    pub summary_text: Mutex<String>,
    /// When set, `complete` reports no usage object.
    pub summary_no_usage: Mutex<bool>,
    pub cancelled: Mutex<u32>,
}

impl FakeLlm {
    pub fn push(&self, s: Script) {
        self.scripts.lock().push_back(s);
    }

    /// Default script: "Hello world" + completed with usage 100/50.
    #[must_use]
    pub fn default_events() -> Vec<LlmEvent> {
        vec![
            LlmEvent::TextDelta("Hello".into()),
            LlmEvent::TextDelta(" world".into()),
            LlmEvent::Completed(LlmCompletion {
                response_id: Some("resp_test123".into()),
                usage: Some(UsageTokens {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..UsageTokens::default()
                }),
                citations: vec![],
                output_text: "Hello world".into(),
                incomplete_reason: None,
            }),
        ]
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn stream(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, LlmFailure> {
        self.requests.lock().push(req);
        let script = self.scripts.lock().pop_front().unwrap_or(Script::Events {
            events: Self::default_events(),
            delay: Duration::ZERO,
        });
        match script {
            Script::Fail(f) => Err(f),
            Script::SlowOpen { open_delay, events } => {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(Box::pin(futures::stream::empty())),
                    () = tokio::time::sleep(open_delay) => {}
                }
                Ok(Box::pin(futures::stream::iter(events)))
            }
            Script::Events { events, delay } => {
                let s = async_stream::stream! {
                    for ev in events {
                        if !delay.is_zero() {
                            tokio::select! {
                                () = cancel.cancelled() => return,
                                () = tokio::time::sleep(delay) => {}
                            }
                        }
                        if cancel.is_cancelled() { return; }
                        yield ev;
                    }
                };
                Ok(Box::pin(s))
            }
            Script::Hang(events) => {
                let s = async_stream::stream! {
                    for ev in events { yield ev; }
                    cancel.cancelled().await;
                };
                Ok(Box::pin(s))
            }
        }
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmTextResult, LlmFailure> {
        self.requests.lock().push(req);
        let text = self.summary_text.lock().clone();
        Ok(LlmTextResult {
            text: if text.is_empty() {
                "<analysis>a</analysis><summary>Summary of the conversation.</summary>".into()
            } else {
                text
            },
            usage: (!*self.summary_no_usage.lock()).then(|| UsageTokens {
                input_tokens: 500,
                output_tokens: 40,
                ..UsageTokens::default()
            }),
            response_id: Some("resp_summary".into()),
        })
    }
}

// ── Fake storage ───────────────────────────────────────────────────────────

/// Recording Files / Vector Stores fake.
pub struct FakeStorage {
    pub calls: Mutex<Vec<String>>,
    pub upload_error: Mutex<Option<StorageError>>,
    pub delete_error: Mutex<Option<StorageError>>,
    pub vs_delete_error: Mutex<Option<StorageError>>,
    pub file_status: Mutex<VectorFileStatus>,
    counter: Mutex<u64>,
}

impl Default for FakeStorage {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            upload_error: Mutex::new(None),
            delete_error: Mutex::new(None),
            vs_delete_error: Mutex::new(None),
            file_status: Mutex::new(VectorFileStatus::Completed),
            counter: Mutex::new(0),
        }
    }
}

impl FakeStorage {
    fn next(&self) -> u64 {
        let mut c = self.counter.lock();
        *c += 1;
        *c
    }
}

#[async_trait]
impl FileStorage for FakeStorage {
    async fn upload_file(
        &self,
        provider_id: &str,
        _tenant_id: Uuid,
        filename: &str,
        content_type: &str,
        data: bytes::Bytes,
    ) -> Result<String, StorageError> {
        self.calls
            .lock()
            .push(format!("upload {provider_id} {filename} {content_type} {}", data.len()));
        if let Some(e) = self.upload_error.lock().clone() {
            return Err(e);
        }
        Ok(format!("file-test{:016}", self.next()))
    }

    async fn delete_file(&self, _p: &str, _t: Uuid, file_id: &str) -> Result<(), StorageError> {
        self.calls.lock().push(format!("delete_file {file_id}"));
        self.delete_error.lock().clone().map_or(Ok(()), Err)
    }

    async fn create_vector_store(&self, _p: &str, _t: Uuid, _name: &str) -> Result<String, StorageError> {
        let id = format!("vs_test{:016}", self.next());
        self.calls.lock().push(format!("create_vs {id}"));
        Ok(id)
    }

    async fn add_file_to_vector_store(
        &self,
        _p: &str,
        _t: Uuid,
        vs: &str,
        file_id: &str,
        _attributes: BTreeMap<String, String>,
    ) -> Result<VectorFileStatus, StorageError> {
        self.calls.lock().push(format!("add_vs_file {vs} {file_id}"));
        Ok(self.file_status.lock().clone())
    }

    async fn get_vector_store_file_status(
        &self,
        _p: &str,
        _t: Uuid,
        vs: &str,
        file_id: &str,
    ) -> Result<VectorFileStatus, StorageError> {
        self.calls.lock().push(format!("get_vs_file {vs} {file_id}"));
        Ok(self.file_status.lock().clone())
    }

    async fn delete_vector_store(&self, _p: &str, _t: Uuid, vs: &str) -> Result<(), StorageError> {
        self.calls.lock().push(format!("delete_vs {vs}"));
        self.vs_delete_error.lock().clone().map_or(Ok(()), Err)
    }
}

// ── Fake knowledge retriever ───────────────────────────────────────────────

/// Recording knowledge retriever: `(provider_id, vector_store_id, query, max_num_results)` per
/// call; returns `chunks` (or `error`).
#[derive(Default)]
pub struct FakeKnowledge {
    pub calls: Mutex<Vec<(String, String, String, usize)>>,
    pub chunks: Mutex<Vec<KnowledgeChunk>>,
    pub error: Mutex<Option<StorageError>>,
}

#[async_trait]
impl KnowledgeRetriever for FakeKnowledge {
    async fn search(
        &self,
        provider_id: &str,
        _tenant_id: Uuid,
        vector_store_id: &str,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, StorageError> {
        self.calls.lock().push((
            provider_id.to_owned(),
            vector_store_id.to_owned(),
            query.to_owned(),
            max_num_results,
        ));
        if let Some(e) = self.error.lock().clone() {
            return Err(e);
        }
        Ok(self.chunks.lock().clone())
    }
}

// ── Recording outbox handler ───────────────────────────────────────────────

/// Delivered outbox payloads per queue.
pub type Delivered = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

pub struct RecordingHandler {
    queue: String,
    sink: Delivered,
}

#[async_trait]
impl LeasedMessageHandler for RecordingHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let v = serde_json::from_slice(&msg.payload).unwrap_or(serde_json::Value::Null);
        self.sink.lock().push((self.queue.clone(), v));
        MessageResult::Ok
    }
}

// ── Catalog helpers ────────────────────────────────────────────────────────

/// Catalog entry with sensible defaults (tool support on, vision on).
#[must_use]
pub fn model(id: &str, tier: &str) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id.to_uppercase(),
        "description": format!("{id} description"),
        "provider_id": "openai",
        "tier": tier,
        "enabled": true,
        "system_prompt": "You are a helpful assistant.",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "max_num_results": 5,
        "general_config": {
            "max_file_size_mb": 25,
            "tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}
        }
    }))
    .unwrap()
}

/// Default catalog: `gpt-premium` (premium, default), `gpt-standard`, `gpt-disabled`.
#[must_use]
pub fn default_catalog() -> Vec<ModelCatalogEntry> {
    let mut p = model("gpt-premium", "premium");
    p.preference = Some(mini_chat_sdk::ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let s = model("gpt-standard", "standard");
    let mut d = model("gpt-disabled", "standard");
    d.enabled = false;
    vec![p, s, d]
}

/// Options of a [`TestEnv`].
pub struct TestOptions {
    pub catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
    pub standard_limits: TierLimits,
    pub premium_limits: TierLimits,
    pub pdp: Arc<dyn AuthZResolverApi>,
    pub cfg: MiniChatConfig,
}

impl Default for TestOptions {
    fn default() -> Self {
        let mut cfg: MiniChatConfig = serde_json::from_value(serde_json::json!({
            "client_credentials": {"client_id": "mini-chat", "client_secret": "s"},
            "providers": {"openai": {"kind": "openai_responses", "storage_kind": "openai",
                                      "host": "127.0.0.1", "port": 18090, "use_http": true}}
        }))
        .unwrap();
        cfg.fill_aliases();
        Self {
            catalog: default_catalog(),
            kill_switches: KillSwitches::default(),
            standard_limits: TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            },
            premium_limits: TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            },
            pdp: Arc::new(TenantPdp),
            cfg,
        }
    }
}

/// A ready-to-use service environment.
pub struct TestEnv {
    pub deps: Arc<Deps>,
    pub services: Arc<Services>,
    pub llm: Arc<FakeLlm>,
    pub storage: Arc<FakeStorage>,
    pub knowledge: Arc<FakeKnowledge>,
    pub delivered: Delivered,
    handle: Option<OutboxHandle>,
}

impl TestEnv {
    pub async fn new(opts: TestOptions) -> Self {
        let dsn = format!("sqlite:file:mc_{}?mode=memory&cache=shared", Uuid::new_v4().simple());
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(4),
                min_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("connect sqlite");
        let mut migrations = crate::infra::db::migrations::Migrator::migrations();
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations)
            .await
            .expect("migrations");
        let provider = Arc::new(DBProvider::<DomainError>::new(db.clone()));

        let policy_cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": opts.catalog,
            "kill_switches": {
                "disable_premium_tier": opts.kill_switches.disable_premium_tier,
                "force_standard_tier": opts.kill_switches.force_standard_tier,
                "disable_web_search": opts.kill_switches.disable_web_search,
                "disable_file_search": opts.kill_switches.disable_file_search,
                "disable_images": opts.kill_switches.disable_images,
                "disable_code_interpreter": opts.kill_switches.disable_code_interpreter,
            },
            "default_standard_limits": opts.standard_limits,
            "default_premium_limits": opts.premium_limits,
        }))
        .unwrap();
        let policy = Arc::new(PolicyGateway::with_client(Arc::new(StaticModelPolicy::from_config(
            &policy_cfg,
        ))));
        let audit = Arc::new(AuditGateway::new(
            Arc::new(toolkit::client_hub::ClientHub::new()),
            "test".into(),
        ));
        let cfg = Arc::new(opts.cfg);
        let llm = Arc::new(FakeLlm::default());
        let storage = Arc::new(FakeStorage::default());
        let knowledge = Arc::new(FakeKnowledge::default());
        let outbox_facade = Arc::new(MiniChatOutbox::new(cfg.outbox.clone()));
        let deps = Arc::new(Deps {
            cfg: Arc::clone(&cfg),
            db: provider,
            enforcer: PolicyEnforcer::new(opts.pdp),
            policy,
            audit,
            outbox: Arc::clone(&outbox_facade),
            llm: llm.clone(),
            storage: storage.clone(),
            knowledge: cfg
                .knowledge_search
                .enabled
                .then(|| Arc::clone(&knowledge) as Arc<dyn KnowledgeRetriever>),
            providers: Arc::new(ProviderResolver::new(&cfg)),
            upload_slots: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        });
        let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
        let rec = |q: &str| RecordingHandler {
            queue: q.to_owned(),
            sink: Arc::clone(&delivered),
        };
        let o = &cfg.outbox;
        let handle = outbox::start(
            db,
            o,
            60,
            OutboxHandlers {
                usage: rec(&o.queue_name),
                audit: rec(&o.audit_queue_name),
                attachment_cleanup: rec(&o.cleanup_queue_name),
                chat_cleanup: rec(&o.chat_cleanup_queue_name),
                thread_summary: rec(&o.thread_summary_queue_name),
            },
        )
        .await
        .expect("outbox");
        outbox_facade.bind(Arc::clone(handle.outbox()));
        let services = Arc::new(Services::new(Arc::clone(&deps)));
        Self {
            deps,
            services,
            llm,
            storage,
            knowledge,
            delivered,
            handle: Some(handle),
        }
    }

    pub async fn default_env() -> Self {
        Self::new(TestOptions::default()).await
    }

    /// Payloads delivered so far to `queue` (waits up to 5 s for at least `min`).
    pub async fn delivered_to(&self, queue: &str, min: usize) -> Vec<serde_json::Value> {
        for _ in 0..100 {
            let got: Vec<serde_json::Value> = self
                .delivered
                .lock()
                .iter()
                .filter(|(q, _)| q == queue)
                .map(|(_, v)| v.clone())
                .collect();
            if got.len() >= min {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.delivered
            .lock()
            .iter()
            .filter(|(q, _)| q == queue)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub async fn shutdown(mut self) {
        self.deps.shutdown.cancel();
        if let Some(h) = self.handle.take() {
            h.stop().await;
        }
    }
}
