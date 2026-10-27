//! Test harness: in-memory SQLite, stub policy/audit ports, stub AuthZ, and
//! a stub OAGW gateway serving canned provider responses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use authz_resolver_sdk::constraints::{Constraint, Predicate};
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelFeatures, ModelGeneralConfig,
    ModelPreference, ModelTier, ModelToolSupport, PolicySnapshot, SupportedEndpoints, TierLimits,
    UsageEvent, UserLimits, WebSearchContextSize,
};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::Pep;
use crate::domain::services::{Repos, ServiceContext};
use crate::infra::gateways::{AuditPort, AuditTarget, GatewayError, PolicyPort};
use crate::infra::metrics::Metrics;
use crate::infra::outbox::SharedEnqueuer;

/// A stub policy port with a configurable snapshot/limits.
pub struct StubPolicy {
    pub snapshot: PolicySnapshot,
    pub standard: TierLimits,
    pub premium: TierLimits,
    /// Events published via `publish_usage`.
    pub usage_events: Mutex<Vec<UsageEvent>>,
    /// Optional rendezvous engaged by the next N `policy_snapshot` calls
    /// (concurrency tests).
    pub snapshot_rendezvous: Mutex<Option<Arc<SnapshotRendezvous>>>,
}

/// A barrier that disarms after `parties` arrivals, so later
/// `policy_snapshot` calls pass through.
pub struct SnapshotRendezvous {
    barrier: tokio::sync::Barrier,
    remaining: std::sync::atomic::AtomicUsize,
}

impl SnapshotRendezvous {
    /// Take one rendezvous slot; false when the rendezvous is exhausted.
    fn take_slot(&self) -> bool {
        let mut current = self.remaining.load(std::sync::atomic::Ordering::Acquire);
        loop {
            if current == 0 {
                return false;
            }
            match self.remaining.compare_exchange_weak(
                current,
                current - 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Default for StubPolicy {
    fn default() -> Self {
        Self::new(Self::default_catalog(), KillSwitches::default())
    }
}

impl StubPolicy {
    /// A stub with the given catalog and kill switches, default limits.
    pub fn new(catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: 1,
                model_catalog: catalog,
                kill_switches,
            },
            standard: TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            },
            premium: TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            },
            usage_events: Mutex::new(Vec::new()),
            snapshot_rendezvous: Mutex::new(None),
        }
    }

    /// Arm a rendezvous for the next `parties` `policy_snapshot` calls.
    pub fn arm_snapshot_rendezvous(&self, parties: usize) {
        *self.snapshot_rendezvous.lock().expect("lock") = Some(Arc::new(SnapshotRendezvous {
            barrier: tokio::sync::Barrier::new(parties),
            remaining: std::sync::atomic::AtomicUsize::new(parties),
        }));
    }

    /// The default two-model catalog (premium `gpt-premium`, standard
    /// `gpt-standard`).
    pub fn default_catalog() -> Vec<ModelCatalogEntry> {
        vec![
            catalog_entry("gpt-premium", ModelTier::Premium, true, true),
            catalog_entry("gpt-standard", ModelTier::Standard, true, false),
        ]
    }
}

#[async_trait::async_trait]
impl PolicyPort for StubPolicy {
    async fn current_policy_version(&self, _user_id: Uuid) -> Result<i64, GatewayError> {
        Ok(1)
    }

    async fn policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: i64,
    ) -> Result<PolicySnapshot, GatewayError> {
        let rendezvous = self.snapshot_rendezvous.lock().expect("lock").clone();
        if let Some(rendezvous) = rendezvous
            && rendezvous.take_slot()
        {
            rendezvous.barrier.wait().await;
        }
        Ok(self.snapshot.clone())
    }

    async fn user_limits(&self, user_id: Uuid, policy_version: i64) -> Result<UserLimits, GatewayError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), GatewayError> {
        self.usage_events.lock().expect("lock").push(event);
        Ok(())
    }
}

/// A catalog entry builder for tests.
pub fn catalog_entry(id: &str, tier: ModelTier, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    ModelCatalogEntry {
        id: id.to_owned(),
        provider_model_id: id.to_owned(),
        display_name: id.to_owned(),
        provider_id: "mock".to_owned(),
        provider_display_name: "Mock".to_owned(),
        tier,
        context_window: 128_000,
        max_output_tokens: 4096,
        max_input_tokens: 100_000,
        input_tokens_credit_multiplier_micro: 1_000_000,
        output_tokens_credit_multiplier_micro: 1_000_000,
        max_num_results: 5,
        general_config: ModelGeneralConfig {
            config_type: String::new(),
            available_from: "1970-01-01T00:00:00Z".to_owned(),
            max_file_size_mb: 25,
            api_params: mini_chat_sdk::ApiParams::default(),
            features: ModelFeatures {
                streaming: true,
                structured_output: false,
            },
            tool_support: ModelToolSupport {
                web_search: true,
                file_search: true,
                image_generation: false,
                code_interpreter: true,
                mcp: false,
            },
            supported_endpoints: SupportedEndpoints {
                chat_completions: true,
                responses: true,
                embeddings: false,
                image_generation: false,
                audio_speech_generation: false,
                audio_transcription: false,
                audio_translation: false,
            },
        },
        description: String::new(),
        icon: String::new(),
        multiplier_display: "1x".to_owned(),
        system_prompt: "You are a test assistant.".to_owned(),
        thread_summary_prompt: String::new(),
        enabled,
        multimodal_capabilities: vec!["VISION_INPUT".to_owned()],
        estimation_budgets: EstimationBudgets::default(),
        web_search_context_size: WebSearchContextSize::Low,
        max_tool_calls: 2,
        preference: Some(ModelPreference {
            is_default,
            sort_order: 0,
        }),
    }
}

/// A stub audit port recording nothing (no plugin registered).
pub struct StubAudit;

#[async_trait::async_trait]
impl AuditPort for StubAudit {
    async fn target(&self) -> AuditTarget {
        AuditTarget::NoPlugin
    }
}

/// A stub AuthZ resolver: allows everything with owner constraints.
pub struct StubAuthZ;

#[async_trait::async_trait]
impl authz_resolver_sdk::AuthZResolverApi for StubAuthZ {
    async fn evaluate(
        &self,
        _ctx: toolkit_security::internal_auth::PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, toolkit_canonical_errors::CanonicalError> {
        let tenant = req
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let mut predicates = Vec::new();
        if let Some(tenant) = tenant {
            predicates.push(Predicate::Eq(authz_resolver_sdk::constraints::EqPredicate::new(
                "owner_tenant_id",
                tenant,
            )));
        }
        // Include the owner predicate for the subject id.
        predicates.push(Predicate::Eq(authz_resolver_sdk::constraints::EqPredicate::new(
            "owner_id",
            req.subject.id,
        )));
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint { predicates }],
                deny_reason: None,
            },
        })
    }
}

/// A test bed: in-memory DB + service context with stubs.
pub struct TestBed {
    /// The service context.
    pub ctx: Arc<ServiceContext>,
    /// The request security context.
    pub security: SecurityContext,
    /// The stub policy (inspect usage events).
    pub policy: Arc<StubPolicy>,
}

/// Start the real outbox pipeline over the test DB (usage/audit/cleanup/
/// summary handlers).
pub async fn start_outbox(
    ctx: &Arc<ServiceContext>,
    gateway: Arc<crate::test_gateway::StubGateway>,
) -> toolkit_db::outbox::OutboxHandle {
    let mut cfg = (*ctx.config).clone();
    cfg.providers = std::collections::HashMap::from([(
        "mock".to_owned(),
        crate::config::ProviderEntry {
            kind: Some(crate::config::ProviderKind::OpenaiResponses),
            host: "mock".to_owned(),
            upstream_alias: Some("mock".to_owned()),
            storage_kind: Some(crate::config::StorageKind::Openai),
            storage_backend: Some("mock".to_owned()),
            ..crate::config::ProviderEntry::default()
        },
    )]);
    let registry = Arc::new(crate::infra::llm::resolver::ProviderRegistry::new(&cfg));
    let deps = crate::infra::outbox::pipeline::PipelineDeps {
        ctx: Arc::clone(ctx),
        storage: Arc::new(crate::infra::llm::storage::StorageClient::new(gateway.clone())),
        registry: registry.clone(),
        adapter: Arc::new(crate::infra::llm::providers::ProviderAdapter::new(gateway)),
        s2s: SecurityContext::anonymous(),
        enqueuer: Arc::clone(&ctx.outbox),
        processors: Some(1),
    };
    crate::infra::outbox::pipeline::start(&deps).await.expect("outbox pipeline")
}

/// Build the application services over the test context (with a stub
/// gateway for provider calls).
pub fn app_services(
    ctx: &Arc<ServiceContext>,
    gateway: Arc<crate::test_gateway::StubGateway>,
) -> crate::domain::services::AppServices {
    use crate::config::ProviderEntry;
    let mut cfg = (*ctx.config).clone();
    cfg.providers = std::collections::HashMap::from([(
        "mock".to_owned(),
        ProviderEntry {
            kind: Some(crate::config::ProviderKind::OpenaiResponses),
            host: "mock".to_owned(),
            upstream_alias: Some("mock".to_owned()),
            storage_kind: Some(crate::config::StorageKind::Openai),
            storage_backend: Some("mock".to_owned()),
            ..ProviderEntry::default()
        },
    )]);
    let registry = Arc::new(crate::infra::llm::resolver::ProviderRegistry::new(&cfg));
    let adapter = Arc::new(crate::infra::llm::providers::ProviderAdapter::new(gateway.clone()));
    let storage = Arc::new(crate::infra::llm::storage::StorageClient::new(gateway));
    let chats = Arc::new(crate::domain::chat_service::ChatService::new(Arc::clone(ctx)));
    let models = Arc::new(crate::domain::model_service::ModelService::new(Arc::clone(ctx)));
    let stream = Arc::new(crate::domain::stream_service::StreamService::new(
        Arc::clone(ctx),
        adapter,
        registry.clone(),
        ctx.metrics.clone(),
    ));
    let turns = Arc::new(crate::domain::turn_service::TurnService::new(
        Arc::clone(ctx),
        stream.clone(),
    ));
    let attachments = Arc::new(crate::domain::attachment_service::AttachmentService::new(
        Arc::clone(ctx),
        registry,
        storage,
    ));
    crate::domain::services::AppServices {
        chats,
        models,
        stream,
        turns,
        attachments,
        config: Arc::clone(&ctx.config),
    }
}

/// Create a chat in the test bed.
pub async fn create_chat(
    bed: &TestBed,
    title: Option<&str>,
    model: Option<&str>,
) -> crate::api::rest::dto::ChatDetailDto {
    bed.ctx
        .repos
        .chats
        .insert(
            &bed.ctx.db.conn().expect("conn"),
            &AccessScope::allow_all(),
            crate::infra::storage::repo::chats::NewChat {
                id: Uuid::new_v4(),
                tenant_id: bed.security.subject_tenant_id(),
                user_id: bed.security.subject_id(),
                model: model.unwrap_or("gpt-premium").to_owned(),
                title: title.map(str::to_owned),
            },
        )
        .await
        .map(|row| crate::domain::chat_service::chat_dto(&row, 0))
        .expect("create chat")
}

/// Drain the SSE channel into events.
pub async fn drain(
    mut rx: crate::domain::stream_service::SseReceiver,
) -> Vec<crate::api::rest::dto::MiniChatSseEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

/// Build a test bed with the given config and policy.
pub async fn test_bed(mut config: MiniChatConfig, policy: Arc<StubPolicy>) -> TestBed {
    // Tests use the in-process mock provider for every model.
    config.providers = std::collections::HashMap::from([(
        "mock".to_owned(),
        crate::config::ProviderEntry {
            kind: Some(crate::config::ProviderKind::OpenaiResponses),
            host: "mock".to_owned(),
            upstream_alias: Some("mock".to_owned()),
            storage_kind: Some(crate::config::StorageKind::Openai),
            storage_backend: Some("mock".to_owned()),
            ..crate::config::ProviderEntry::default()
        },
    )]);
    let db = toolkit_db::connect_db("sqlite::memory:", toolkit_db::ConnectOpts::default())
        .await
        .expect("connect");
    use sea_orm_migration::MigratorTrait;
    let mut migrations = crate::infra::storage::Migrator::migrations();
    let mut outbox = toolkit_db::outbox::outbox_migrations();
    migrations.append(&mut outbox);
    toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations)
        .await
        .expect("migrations");
    let db: crate::infra::storage::repo::Db = toolkit_db::DBProvider::new(db);

    let pep = Arc::new(Pep::new(Arc::new(StubAuthZ)));
    let service_ctx = Arc::new(ServiceContext {
        config: Arc::new(config),
        db: db.clone(),
        repos: Repos::new(&db),
        pep,
        policy: policy.clone(),
        audit: Arc::new(StubAudit),
        knowledge: None,
        metrics: Metrics::new("mini_chat_test"),
        outbox: Arc::new(SharedEnqueuer::new()),
    });
    let security = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("security context");
    TestBed {
        ctx: service_ctx,
        security,
        policy,
    }
}

/// A canned SSE response script for the stub gateway.
#[derive(Clone, Default)]
pub struct CannedStream {
    /// Ordered SSE events (name, JSON data).
    pub events: Vec<(String, String)>,
}

/// A stub `ServiceGatewayClientV1` serving canned provider responses.
pub struct StubGateway {
    /// Script for the next streaming responses call.
    pub stream_script: Mutex<Option<CannedStream>>,
    /// Files uploaded (id -> (filename, bytes)).
    pub files: Mutex<HashMap<String, (String, bytes::Bytes)>>,
    /// Vector stores (id -> files).
    pub vector_stores: Mutex<HashMap<String, Vec<String>>>,
    /// Vector store file statuses.
    pub vs_file_status: Mutex<HashMap<(String, String), String>>,
    /// Recorded request bodies of chat calls.
    pub requests: Mutex<Vec<serde_json::Value>>,
    /// Non-streaming completion response.
    pub complete_response: Mutex<Option<String>>,
    /// When set, the next responses call fails with HTTP 429 + Retry-After.
    pub fail_429_flag: Mutex<bool>,
    /// Deleted provider file ids.
    pub deleted_files: Mutex<Vec<String>>,
    /// Deleted vector store ids.
    pub deleted_vector_stores: Mutex<Vec<String>>,
    /// When set, the responses SSE stream hangs after the script events.
    pub hang: Mutex<bool>,
    /// Forced vector-store file status for the next add.
    pub forced_vs_status: Mutex<Option<String>>,
}

impl StubGateway {
    /// A new stub with default behavior.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stream_script: Mutex::new(None),
            files: Mutex::new(HashMap::new()),
            vector_stores: Mutex::new(HashMap::new()),
            vs_file_status: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            complete_response: Mutex::new(None),
            fail_429_flag: Mutex::new(false),
            deleted_files: Mutex::new(Vec::new()),
            deleted_vector_stores: Mutex::new(Vec::new()),
            hang: Mutex::new(false),
            forced_vs_status: Mutex::new(None),
        })
    }

    /// Force vector store file adds to report this status.
    pub fn set_vs_file_status(self: &Arc<Self>, status: &str) {
        *self.forced_vs_status.lock().expect("lock") = Some(status.to_owned());
    }

    /// The next streaming response hangs open after its events.
    pub fn hang_next(self: &Arc<Self>) {
        *self.hang.lock().expect("lock") = true;
    }

    /// Fail the next responses call with HTTP 429 and `Retry-After: 7`.
    pub fn fail_next_with_429(self: &Arc<Self>) {
        *self.fail_429_flag.lock().expect("lock") = true;
    }

    /// Take the 429-failure flag.
    pub fn fail_429(&self) -> bool {
        let mut guard = self.fail_429_flag.lock().expect("lock");
        std::mem::take(&mut *guard)
    }

    /// The default successful stream script (two deltas + completed).
    pub fn default_stream() -> CannedStream {
        let resp_id = format!("resp-{}", Uuid::new_v4().as_simple());
        CannedStream {
            events: vec![
                (
                    "response.output_text.delta".to_owned(),
                    r#"{"delta": "Hello"}"#.to_owned(),
                ),
                (
                    "response.output_text.delta".to_owned(),
                    r#"{"delta": " world"}"#.to_owned(),
                ),
                (
                    "response.completed".to_owned(),
                    format!(
                        r#"{{"response": {{"id": "{resp_id}", "usage": {{"input_tokens": 10, "output_tokens": 5, "input_tokens_details": {{"cached_tokens": 1}}, "output_tokens_details": {{"reasoning_tokens": 2}}}}}}}}"#
                    ),
                ),
            ],
        }
    }
}
