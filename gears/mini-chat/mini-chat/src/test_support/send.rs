//! Test doubles and harnesses of the message-streaming feature, slice S1-setup-path: a recording
//! provider-task port, the send rig (the real services over in-memory `SQLite`, the quota
//! machinery of the quota rig, the real router), catalog builders, seeding of messages, turns,
//! attachments, thread summaries and reactions that do not go through the code under test, raw
//! inspection helpers and an SSE frame parser.
//!
//! If the author stage changes the constructors of `MessageStreamService`, `MessagesService`,
//! `StreamSettings` or `Services`, [`send_rig_with`] is the only place to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use axum::middleware::from_fn;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Set};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use toolkit::api::{OpenApiRegistryImpl, canonical_error_middleware};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use super::chats::{RecordingOpenApi, Reply, a1, request};
use super::provider::MockProvider;
use super::quota::{
    FixedClock, PREMIUM, QuotaRig, QuotaRigOptions, STANDARD, UsageQueueKind, default_now,
    plain_budgets, premium_model, standard_model,
};
use super::s2::{MetricsHarness, metrics_harness};
use super::{Pdp, TENANT_A, TestDb, USER_A1, raw_exec};
use crate::api::rest::register_routes;
use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::attachments::AttachmentMetrics;
use crate::domain::attachments::contributions::{AttachmentToolSettings, AttachmentToolSource};
use crate::domain::attachments::knowledge::{
    KnowledgeLoopDeps, KnowledgeLoopSettings, KnowledgeRetriever,
};
use crate::domain::chats::{ChatCleanupQueue, ChatDetail, ChatDraft, ChatStore, ChatsService};
use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::messages::MessagesService;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::mutations::service::MutationDeps;
use crate::domain::mutations::{
    MutationMetrics, MutationStore, ReactionService, ReactionStore, TurnMutationService,
};
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;
use crate::domain::quota::QuotaService;
use crate::domain::services::Services;
use crate::domain::streaming::{
    ContextReads, MessageStreamService, PreflightReads, ProviderCallPlan, ProviderTaskPort,
    RawSendBody, RelayEvent, ReserveCommand, ReservedTurn, SnapshotBoundary, StartedTurn,
    StreamSettings, StreamStore, ToolContribution, TurnFinalizer, TurnOutbox, TurnProgress,
};
use crate::domain::turns::TurnRow;
use crate::infra::leader::{LeaderElector, NoopLeaderElector};
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::orphan_watchdog::{OrphanWatchdog, WatchdogSettings};
use crate::infra::outbox::{OutboxEnqueuer, queue_specs};
use crate::infra::provider_task::{
    ProviderTaskDeps, ProviderTaskSettings, S2sContext, StreamingProviderTask,
};
use crate::infra::storage::entity::{
    attachment, chat, chat_turn, message, message_attachment, thread_summary,
};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::infra::turn_finalizer::{SqlTurnFinalizer, StreamFinalizationDeps, SummarySettings};

/// The system prompt of the catalog entries of the send rig.
pub const SYSTEM_PROMPT: &str = "You are the Mini Chat assistant.";
/// `context.web_search_guard` of the settings of the send rig.
pub const WEB_GUARD: &str = "WEB-GUARD: use web_search sparingly.";
/// `context.file_search_guard` of the settings of the send rig.
pub const FILE_GUARD: &str = "FILE-GUARD: use file_search for documents.";
/// `context.resource_type` of a turn not-found.
pub const TURN_RESOURCE: &str = "gts.cf.core.mini_chat.turn.v1~";

/// `/mini-chat/v1/chats/{id}/messages`.
pub fn messages_uri(chat_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/messages")
}

/// `/mini-chat/v1/chats/{id}/messages:stream`.
pub fn stream_uri(chat_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/messages:stream")
}

/// `/mini-chat/v1/chats/{id}/turns/{request_id}`.
pub fn turn_uri(chat_id: Uuid, request_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/turns/{request_id}")
}

// ── the provider task double ─────────────────────────────────────────────────

/// What the database held at the moment the provider task was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// `chat_turns` rows in state `running`.
    pub running_turns: i64,
    /// `messages` rows with role `user`.
    pub user_messages: i64,
    /// The sum of `reserved_credits_micro` over every `quota_usage` row.
    pub reserved_credits: i64,
}

/// A provider-task port that records every plan it receives (the seam of slice S2) and, when a
/// probe connection is attached, what the database held at that moment (the reserve transaction
/// must have committed before the first outbound byte).
#[derive(Default)]
pub struct RecordingProviderTask {
    plans: Mutex<Vec<ProviderCallPlan>>,
    observed: Mutex<Vec<Observation>>,
    probe: Mutex<Option<sea_orm::DatabaseConnection>>,
    /// `start` records the plan and then fails.
    fails: bool,
}

impl RecordingProviderTask {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A port whose `start` records the plan and then answers an error.
    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            fails: true,
            ..Self::default()
        })
    }

    /// The plans received so far.
    pub fn plans(&self) -> Vec<ProviderCallPlan> {
        self.plans.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.plans.lock().unwrap().len()
    }

    /// The only plan received.
    pub fn only_plan(&self) -> ProviderCallPlan {
        let plans = self.plans();
        assert_eq!(plans.len(), 1, "expected exactly one provider task start");
        plans.into_iter().next().unwrap()
    }

    /// The database state seen at each start, in order.
    pub fn observations(&self) -> Vec<Observation> {
        self.observed.lock().unwrap().clone()
    }

    /// Observe the database through `connection` at every start.
    pub fn attach_probe(&self, connection: sea_orm::DatabaseConnection) {
        *self.probe.lock().unwrap() = Some(connection);
    }
}

#[async_trait]
impl ProviderTaskPort for RecordingProviderTask {
    async fn start(
        &self,
        plan: ProviderCallPlan,
        _cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<RelayEvent>, DomainError> {
        let probe = self.probe.lock().unwrap().clone();
        if let Some(raw) = probe {
            let observation = Observation {
                running_turns: super::raw_i64(
                    &raw,
                    "SELECT COUNT(*) FROM chat_turns WHERE state = 'running'",
                )
                .await,
                user_messages: super::raw_i64(
                    &raw,
                    "SELECT COUNT(*) FROM messages WHERE role = 'user'",
                )
                .await,
                reserved_credits: super::raw_i64(
                    &raw,
                    "SELECT COALESCE(SUM(reserved_credits_micro), 0) FROM quota_usage",
                )
                .await,
            };
            self.observed.lock().unwrap().push(observation);
        }
        self.plans.lock().unwrap().push(plan);
        if self.fails {
            return Err(DomainError::internal("the provider task could not start"));
        }
        // no events: the sender is dropped at once, so the receiver is a closed channel
        let (_sender, receiver) = mpsc::channel(1);
        Ok(receiver)
    }
}

// ── a store that parks the setup after the reserve commit ────────────────────

/// Parks the send setup right after the reserve transaction committed: `committed` is signalled
/// and the setup resumes when `release` is. A test drops the handler future in between.
#[derive(Default)]
pub struct ReserveGate {
    pub committed: Notify,
    pub release: Notify,
}

/// A [`StreamStore`] over `inner` whose `reserve` waits on a [`ReserveGate`] after the commit.
struct GatedStore {
    inner: Arc<dyn StreamStore>,
    gate: Arc<ReserveGate>,
}

#[async_trait]
impl ChatStore for GatedStore {
    async fn insert_chat(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        draft: ChatDraft,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.insert_chat(scope, ctx, draft).await
    }

    async fn load_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.load_chat(scope, chat_id).await
    }

    async fn list_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        self.inner.list_chats(scope, query).await
    }

    async fn rename_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.rename_chat(scope, chat_id, title).await
    }

    async fn delete_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        cleanup: Arc<dyn ChatCleanupQueue>,
    ) -> Result<(), DomainError> {
        self.inner.delete_chat(scope, chat_id, cleanup).await
    }
}

#[async_trait]
impl StreamStore for GatedStore {
    async fn find_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        self.inner.find_turn(tenant_id, chat_id, request_id).await
    }

    async fn has_running_turn(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        self.inner.has_running_turn(tenant_id, chat_id).await
    }

    async fn load_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        self.inner
            .load_message(tenant_id, chat_id, message_id)
            .await
    }

    async fn read_preflight(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<PreflightReads, DomainError> {
        self.inner
            .read_preflight(tenant_id, chat_id, attachment_ids)
            .await
    }

    async fn read_context(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: Option<&SnapshotBoundary>,
        limit: u32,
    ) -> Result<ContextReads, DomainError> {
        self.inner
            .read_context(tenant_id, chat_id, boundary, limit)
            .await
    }

    async fn reserve(
        &self,
        quota: &QuotaService,
        command: &ReserveCommand,
    ) -> Result<ReservedTurn, DomainError> {
        let reserved = self.inner.reserve(quota, command).await?;
        self.gate.committed.notify_one();
        self.gate.release.notified().await;
        Ok(reserved)
    }
}

// ── catalog builders ─────────────────────────────────────────────────────────

/// `entry` with the top-level keys of `patch` replaced.
pub fn patched(entry: &ModelCatalogEntry, patch: &Value) -> ModelCatalogEntry {
    let mut value = serde_json::to_value(entry).expect("entry serializes");
    for (key, v) in patch.as_object().expect("patch is an object") {
        value[key] = v.clone();
    }
    serde_json::from_value(value).expect("patched entry")
}

/// Estimation budgets where a text of `n` bytes estimates exactly `n` tokens: one byte per
/// token, no overhead, no margin; the surcharges are those of the quota example (image 1000,
/// file search 500, web search 500, code interpreter 1000).
pub fn byte_budgets() -> Value {
    let mut budgets = plain_budgets();
    budgets["bytes_per_token_conservative"] = json!(1);
    budgets
}

/// The standard model of the rig: no vision, the rig's system prompt, `tool_support.web_search`
/// on (the entry default), the patch applied on top.
pub fn std_model(patch: &Value) -> ModelCatalogEntry {
    let base = patched(
        &standard_model(),
        &json!({ "system_prompt": SYSTEM_PROMPT }),
    );
    patched(&base, patch)
}

/// The premium model of the rig: vision input, the rig's system prompt, the patch applied on top.
pub fn prem_model(patch: &Value) -> ModelCatalogEntry {
    let base = patched(&premium_model(), &json!({ "system_prompt": SYSTEM_PROMPT }));
    patched(&base, patch)
}

/// `tool_support` with the given web search flag (the other tools stay off).
pub fn tool_support(web_search: bool) -> Value {
    json!({
        "web_search": web_search, "file_search": false, "image_generation": false,
        "code_interpreter": false, "mcp": false
    })
}

/// A catalog entry whose `general_config.tool_support.web_search` is `web_search`.
pub fn with_web_search(entry: &ModelCatalogEntry, web_search: bool) -> ModelCatalogEntry {
    let mut value = serde_json::to_value(entry).expect("entry serializes");
    value["general_config"]["tool_support"] = tool_support(web_search);
    serde_json::from_value(value).expect("entry with tool support")
}

/// A snapshot of version 1 with the given entries and no kill switch.
pub fn catalog_of(entries: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    super::quota::catalog_v(1, entries)
}

/// The default catalog of the rig: the premium model, then the standard model.
pub fn default_catalog() -> PolicySnapshot {
    catalog_of(vec![prem_model(&json!({})), std_model(&json!({}))])
}

/// The settings of the rig: 10 recent messages, 4 images per message, 50 documents per chat and
/// recognizable guard texts.
pub fn rig_settings() -> StreamSettings {
    StreamSettings {
        recent_messages_limit: 10,
        web_search_guard: WEB_GUARD.to_owned(),
        file_search_guard: FILE_GUARD.to_owned(),
        max_images_per_message: 4,
        max_documents_per_chat: 50,
        sse_ping_interval: Duration::from_secs(15),
    }
}

/// The provider registry of the rig: the catalog `provider_id` of the entries
/// (`azure_openai`) resolves to `azure.example.com` (OAGW alias `azure.example.com`).
pub fn rig_providers() -> Arc<ProviderRegistry> {
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": {
                "kind": "openai_responses",
                "host": "azure.example.com",
                "storage_kind": "openai"
            }
        }
    }))
    .expect("valid config");
    Arc::new(ProviderRegistry::build(&config.providers))
}

/// The models of the multi-adapter catalog: one per adapter kind besides the default
/// `openai_responses` standard model.
pub const CC_MODEL: &str = "cc-1";
pub const VLLM_MODEL: &str = "vllm-1";
pub const ANTHROPIC_MODEL: &str = "anthropic-1";

/// A provider registry with one provider per adapter kind: `azure_openai` (the default
/// `openai_responses` of [`rig_providers`]), `cc` (`openai_chat_completions`), `vllm`
/// (`vllm_responses`) and `anthropic` (`anthropic_messages`).
pub fn multi_providers() -> Arc<ProviderRegistry> {
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": { "kind": "openai_responses", "host": "azure.example.com",
                               "storage_kind": "openai" },
            "cc": { "kind": "openai_chat_completions", "host": "cc.example.com",
                     "storage_kind": "openai", "api_path": "/v1/chat/completions" },
            "vllm": { "kind": "vllm_responses", "host": "vllm.example.com",
                       "storage_kind": "openai" },
            "anthropic": { "kind": "anthropic_messages", "host": "anthropic.example.com",
                            "storage_kind": "openai", "api_path": "/v1/messages" }
        }
    }))
    .expect("valid config");
    Arc::new(ProviderRegistry::build(&config.providers))
}

/// The default catalog plus one standard-tier model per extra adapter kind.
pub fn multi_catalog() -> PolicySnapshot {
    let on = |id: &str, provider: &str| {
        std_model(&json!({
            "id": id,
            "provider_id": provider,
            "provider_model_id": format!("provider-{id}"),
        }))
    };
    catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({})),
        on(CC_MODEL, "cc"),
        on(VLLM_MODEL, "vllm"),
        on(ANTHROPIC_MODEL, "anthropic"),
    ])
}

// ── the rig ──────────────────────────────────────────────────────────────────

/// The provider-task port a [`SendRig`] wires into the send service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    /// [`RecordingProviderTask`]: records the plan and leaves the turn `running`.
    Recording,
    /// [`RecordingProviderTask::failing`]: records the plan and answers an error.
    Failing,
}

/// How to build a [`SendRig`].
pub struct SendRigOptions {
    pub pdp: Pdp,
    pub snapshot: PolicySnapshot,
    pub settings: StreamSettings,
    pub providers: Arc<ProviderRegistry>,
    pub contributions: Vec<Arc<dyn ToolContribution>>,
    pub clock: Arc<dyn Clock>,
    pub provider_mode: ProviderMode,
    /// Park the setup after the reserve commit on this gate.
    pub gate: Option<Arc<ReserveGate>>,
    /// Slice S2: the mock OAGW; when set, the REAL provider task runs against it (and
    /// `provider_mode` is ignored).
    pub upstream: Option<Arc<MockProvider>>,
    /// Slice S2: the settings of the real provider task.
    pub task_settings: ProviderTaskSettings,
    /// Slice S2: the OTLP metrics wired into the quota rules, the finalizer, the provider task
    /// and the watchdog.
    pub metrics: Option<MetricsHarness>,
    /// Slice S2: a replacement of the audit and thread-summary queues of the finalizer and the
    /// watchdog (the real enqueuer by default).
    pub turn_outbox: Option<Arc<dyn TurnOutbox>>,
    /// Slice S2: `thread_summary_worker.enabled` and `compression_threshold_pct`.
    pub summary: SummarySettings,
    /// Slice S2: the S2S context of the real provider task (bound to [`provider_s2s`] by
    /// default).
    pub s2s: Arc<S2sContext>,
    /// Slice S2: the leader elector of the watchdog.
    pub elector: Arc<dyn LeaderElector>,
    /// FEATURE 0005 slice S2: the attachment tool contributions (`file_search`,
    /// `code_interpreter`, `search_knowledge`) over the rig's database.
    pub attachment_tools: Option<AttachmentToolSettings>,
    /// FEATURE 0005 slice S2: the knowledge search loop of the real provider task.
    pub knowledge: Option<KnowledgeLoopSpec>,
    /// FEATURE 0006: runs after the mutation transaction committed (see `mutations::CommitHook`).
    pub commit_hook: Option<Arc<dyn super::mutations::CommitHook>>,
}

/// The knowledge search loop of a rig: the retriever, the loop values and the metrics sink; the
/// turn progress port is the rig's database.
#[derive(Clone)]
pub struct KnowledgeLoopSpec {
    pub retriever: Arc<dyn KnowledgeRetriever>,
    pub settings: KnowledgeLoopSettings,
    pub metrics: Arc<dyn AttachmentMetrics>,
}

/// The S2S security context the real provider task runs under in the rig: a subject and a
/// tenant that are neither the user nor its tenant.
pub fn provider_s2s() -> SecurityContext {
    super::ctx(
        Uuid::from_u128(0x5252_0000_0000_0000_0000_0000_0000_0099),
        Uuid::from_u128(0x5252_0000_0000_0000_0000_0000_0000_0098),
    )
}

/// A turn outbox that delegates the audit enqueue to the real enqueuer and fails every
/// thread-summary enqueue (a database failure in the last step of the finalization
/// transaction). Bind the real enqueuer once the rig exists.
#[derive(Default)]
pub struct FailingSummaryOutbox {
    inner: std::sync::OnceLock<Arc<dyn TurnOutbox>>,
}

impl FailingSummaryOutbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn bind(&self, inner: Arc<dyn TurnOutbox>) {
        drop(self.inner.set(inner));
    }
}

#[async_trait]
impl TurnOutbox for FailingSummaryOutbox {
    async fn enqueue_turn_audit(
        &self,
        tx: &toolkit_db::secure::DbTx<'_>,
        event: &mini_chat_sdk::AuditEvent,
    ) -> Result<toolkit_db::outbox::Wake, DomainError> {
        self.inner
            .get()
            .expect("the real enqueuer is bound")
            .enqueue_turn_audit(tx, event)
            .await
    }

    async fn enqueue_mutation_audit(
        &self,
        tx: &toolkit_db::secure::DbTx<'_>,
        tenant_id: Uuid,
        event: &mini_chat_sdk::AuditEvent,
    ) -> Result<toolkit_db::outbox::Wake, DomainError> {
        self.inner
            .get()
            .expect("the real enqueuer is bound")
            .enqueue_mutation_audit(tx, tenant_id, event)
            .await
    }

    async fn enqueue_thread_summary(
        &self,
        _tx: &toolkit_db::secure::DbTx<'_>,
        _work: &crate::domain::streaming::summary::ThreadSummaryWork,
    ) -> Result<toolkit_db::outbox::Wake, DomainError> {
        Err(DomainError::internal(
            "forced thread-summary enqueue failure",
        ))
    }
}

/// The settings of the real provider task in the rig: the default limits of the configuration,
/// a small channel and a long idle timeout (a test that exercises the timeout shortens it).
pub fn rig_task_settings() -> ProviderTaskSettings {
    ProviderTaskSettings {
        channel_capacity: 16,
        web_search_max_calls_per_message: 2,
        code_interpreter_max_calls_per_message: 10,
        idle_timeout: Duration::from_secs(30),
        progress_interval: Duration::from_secs(30),
    }
}

impl SendRigOptions {
    pub fn new() -> Self {
        Self {
            pdp: Pdp::TenantOnly,
            snapshot: default_catalog(),
            settings: rig_settings(),
            providers: rig_providers(),
            contributions: Vec::new(),
            clock: FixedClock::at(default_now()),
            provider_mode: ProviderMode::Recording,
            gate: None,
            upstream: None,
            task_settings: rig_task_settings(),
            metrics: None,
            turn_outbox: None,
            summary: SummarySettings {
                enabled: true,
                compression_threshold_pct: 80,
            },
            s2s: S2sContext::bound(provider_s2s()),
            elector: Arc::new(NoopLeaderElector),
            attachment_tools: None,
            knowledge: None,
            commit_hook: None,
        }
    }

    /// FEATURE 0006: run `hook` right after every mutation commit.
    pub fn with_commit_hook(mut self, hook: Arc<dyn super::mutations::CommitHook>) -> Self {
        self.commit_hook = Some(hook);
        self
    }

    pub fn with_pdp(mut self, pdp: Pdp) -> Self {
        self.pdp = pdp;
        self
    }

    pub fn with_snapshot(mut self, snapshot: PolicySnapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    pub fn with_provider(mut self, mode: ProviderMode) -> Self {
        self.provider_mode = mode;
        self
    }

    pub fn with_gate(mut self, gate: Arc<ReserveGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Run the real provider task against `upstream`.
    pub fn with_upstream(mut self, upstream: Arc<MockProvider>) -> Self {
        self.upstream = Some(upstream);
        self
    }

    pub fn with_task_settings(mut self, settings: ProviderTaskSettings) -> Self {
        self.task_settings = settings;
        self
    }

    pub fn with_settings(mut self, settings: StreamSettings) -> Self {
        self.settings = settings;
        self
    }

    pub fn with_providers(mut self, providers: Arc<ProviderRegistry>) -> Self {
        self.providers = providers;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Record the OTLP series on an in-memory pipeline under the prefix `mini_chat`.
    pub fn with_metrics(mut self) -> Self {
        self.metrics = Some(metrics_harness("mini_chat"));
        self
    }

    pub fn with_turn_outbox(mut self, outbox: Arc<dyn TurnOutbox>) -> Self {
        self.turn_outbox = Some(outbox);
        self
    }

    pub fn with_summary(mut self, summary: SummarySettings) -> Self {
        self.summary = summary;
        self
    }

    pub fn with_s2s(mut self, s2s: Arc<S2sContext>) -> Self {
        self.s2s = s2s;
        self
    }

    pub fn with_elector(mut self, elector: Arc<dyn LeaderElector>) -> Self {
        self.elector = elector;
        self
    }

    /// Wire the attachment tool contributions of FEATURE 0005 into the send service.
    pub fn with_attachment_tools(mut self, settings: AttachmentToolSettings) -> Self {
        self.attachment_tools = Some(settings);
        self
    }

    /// Wire the knowledge search loop of FEATURE 0005 into the real provider task.
    pub fn with_knowledge_loop(mut self, spec: KnowledgeLoopSpec) -> Self {
        self.knowledge = Some(spec);
        self
    }
}

impl Default for SendRigOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The in-process harness of the send setup, the messages list and the turn status: the quota
/// rig (database, quota service, gateway, clock, PDP, outbox pipeline) plus the services of this
/// slice and the real router.
pub struct SendRig {
    pub quota: QuotaRig,
    pub router: Router,
    pub openapi: Arc<RecordingOpenApi>,
    pub service: Arc<MessageStreamService>,
    pub provider: Arc<RecordingProviderTask>,
    /// Slice S2: the finalization of the rig (with the stream dependencies).
    pub finalizer: Arc<dyn TurnFinalizer>,
    /// Slice S2: the audit and thread-summary queues the finalizer and the watchdog write to.
    pub turn_outbox: Arc<dyn TurnOutbox>,
    /// Slice S2: the OTLP metrics pipeline, when the rig was built with one.
    pub metrics: Option<MetricsHarness>,
    pub elector: Arc<dyn LeaderElector>,
    /// FEATURE 0006: the retry, edit and delete service of the rig (the one behind the routes
    /// has the same collaborators).
    pub mutations: Arc<TurnMutationService>,
    /// FEATURE 0006: the reactions service of the rig.
    pub reactions: Arc<ReactionService>,
}

impl Deref for SendRig {
    type Target = QuotaRig;

    fn deref(&self) -> &QuotaRig {
        &self.quota
    }
}

/// A rig with the default catalog and the tenant-only PDP.
pub async fn send_rig() -> SendRig {
    send_rig_with(SendRigOptions::new()).await
}

/// A rig that runs the real provider task against `upstream`, with the default catalog.
pub async fn stream_rig(upstream: Arc<MockProvider>) -> SendRig {
    send_rig_with(SendRigOptions::new().with_upstream(upstream)).await
}

/// A rig built from `options`.
pub async fn send_rig_with(options: SendRigOptions) -> SendRig {
    let gateway = super::quota::QuotaGateway::serving(
        options.snapshot,
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    );
    let quota = super::quota::quota_rig_with(QuotaRigOptions {
        pdp: options.pdp,
        gateway,
        queue: UsageQueueKind::Real,
        clock: options.clock,
        metrics: options
            .metrics
            .as_ref()
            .map(|harness| harness.metrics.clone() as Arc<dyn crate::domain::quota::QuotaMetrics>),
        ..QuotaRigOptions::new()
    })
    .await;
    let provider = if options.provider_mode == ProviderMode::Failing {
        RecordingProviderTask::failing()
    } else {
        RecordingProviderTask::new()
    };
    provider.attach_probe(quota.db.raw.clone());
    let policy: Arc<dyn ModelPolicyGateway> = quota.gateway.clone();
    let turn_outbox: Arc<dyn TurnOutbox> = options
        .turn_outbox
        .clone()
        .unwrap_or_else(|| quota.enqueuer.clone());
    let metrics_sink = options.metrics.as_ref().map_or_else(
        || {
            Arc::new(MiniChatMetrics::new(
                &opentelemetry::global::meter("mini-chat-send-rig"),
                "mini_chat",
            ))
        },
        |harness| Arc::clone(&harness.metrics),
    );
    let finalizer: Arc<dyn TurnFinalizer> = Arc::new(
        SqlTurnFinalizer::new(Arc::clone(&quota.service)).with_stream_deps(
            StreamFinalizationDeps {
                outbox: Arc::clone(&turn_outbox),
                summary: options.summary,
                metrics: Arc::clone(&metrics_sink),
            },
        ),
    );
    let upstream = options.upstream.clone().unwrap_or_else(MockProvider::new);
    let progress: Arc<dyn TurnProgress> = quota.db.provider.clone();
    let provider_task = || -> Arc<dyn ProviderTaskPort> {
        if options.upstream.is_some() {
            Arc::new(StreamingProviderTask::new(ProviderTaskDeps {
                gateway: upstream.clone(),
                s2s: Arc::clone(&options.s2s),
                finalizer: Arc::clone(&finalizer),
                progress: Arc::clone(&progress),
                quota: Arc::clone(&quota.service),
                metrics: Arc::clone(&metrics_sink),
                settings: options.task_settings.clone(),
                knowledge: options.knowledge.as_ref().map(|spec| KnowledgeLoopDeps {
                    retriever: Arc::clone(&spec.retriever),
                    progress: Arc::clone(&progress),
                    metrics: Arc::clone(&spec.metrics),
                    settings: spec.settings,
                }),
            }))
        } else {
            provider.clone()
        }
    };
    let build_streaming = || {
        let store: Arc<dyn StreamStore> = match &options.gate {
            Some(gate) => Arc::new(GatedStore {
                inner: quota.db.provider.clone(),
                gate: Arc::clone(gate),
            }),
            None => quota.db.provider.clone(),
        };
        let service = MessageStreamService::new(
            Arc::clone(&quota.pep),
            Arc::clone(&policy),
            store,
            Arc::clone(&quota.service),
            Arc::clone(&options.providers),
            provider_task(),
            Arc::clone(&finalizer),
            options.settings.clone(),
        )
        .with_contributions(options.contributions.clone());
        match &options.attachment_tools {
            Some(settings) => service.with_attachment_tools(Arc::new(AttachmentToolSource::new(
                quota.db.provider.clone(),
                settings.clone(),
            ))),
            None => service,
        }
    };
    let build_messages = || MessagesService::new(Arc::clone(&quota.pep), quota.db.provider.clone());
    // the chat endpoints (for `message_count`); the cleanup queue is never bound because no test
    // of this slice deletes a chat
    let cleanup: Arc<dyn ChatCleanupQueue> = Arc::new(OutboxEnqueuer::new(queue_specs(
        &OutboxConfig::default(),
        300,
    )));
    let chats = ChatsService::new(
        Arc::clone(&quota.pep),
        Arc::clone(&policy),
        quota.db.provider.clone(),
        cleanup,
    );
    // FEATURE 0006: the mutation and reaction services share the stores, the audit queue and the
    // metrics of the rig; `turn_outbox` carries the mutation audit enqueue
    let mutation_metrics: Arc<dyn MutationMetrics> = Arc::clone(&metrics_sink) as _;
    let build_mutations = || {
        // the gate of the rig parks the setup after the `Existing` reserve commit, the hook runs
        // after the mutation commit
        let store: Arc<dyn MutationStore> =
            if options.gate.is_some() || options.commit_hook.is_some() {
                Arc::new(super::mutations::HookedMutationStore {
                    inner: quota.db.provider.clone(),
                    raw: quota.db.raw.clone(),
                    hook: options.commit_hook.clone(),
                    gate: options.gate.clone(),
                })
            } else {
                quota.db.provider.clone()
            };
        TurnMutationService::new(MutationDeps {
            streaming: Arc::new(build_streaming()),
            store,
            outbox: Arc::clone(&turn_outbox),
            metrics: Arc::clone(&mutation_metrics),
        })
    };
    let build_reactions = || {
        let store: Arc<dyn ReactionStore> = quota.db.provider.clone();
        ReactionService::new(Arc::clone(&quota.pep), store, Arc::clone(&quota.clock))
    };
    let services = Arc::new(
        Services::new(Arc::clone(&quota.pep), Arc::clone(&policy))
            .with_chats(chats)
            .with_quota(quota.make_service())
            .with_messages(build_messages())
            .with_streaming(build_streaming())
            .with_mutations(build_mutations())
            .with_reactions(build_reactions()),
    );
    let openapi = Arc::new(RecordingOpenApi {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = register_routes(Router::new(), openapi.as_ref(), "/mini-chat", services)
        .layer(from_fn(canonical_error_middleware));
    let service = Arc::new(build_streaming());
    let mutations = Arc::new(build_mutations());
    let reactions = Arc::new(build_reactions());
    SendRig {
        quota,
        router,
        openapi,
        service,
        provider,
        finalizer,
        turn_outbox,
        metrics: options.metrics,
        elector: options.elector,
        mutations,
        reactions,
    }
}

impl SendRig {
    /// The orphan watchdog over the quota service, the turn outbox, the elector and the metrics
    /// of the rig, with the given stale timeout and the default scan interval.
    pub fn watchdog(&self, timeout: Duration) -> OrphanWatchdog {
        let metrics = self.metrics.as_ref().map_or_else(
            || {
                Arc::new(MiniChatMetrics::new(
                    &opentelemetry::global::meter("mini-chat-send-rig"),
                    "mini_chat",
                ))
            },
            |harness| Arc::clone(&harness.metrics),
        );
        OrphanWatchdog::new(
            Arc::clone(&self.quota.service),
            Arc::clone(&self.turn_outbox),
            Arc::clone(&self.elector),
            metrics,
            WatchdogSettings {
                timeout,
                scan_interval: Duration::from_millis(20),
            },
        )
    }

    /// Run the send setup of user 1 of tenant A directly (no HTTP) and return the running turn:
    /// its plan, the receiver of the relay channel and the turn's cancellation token. Panics
    /// unless the real provider task answered with an event stream.
    pub async fn start_turn(&self, chat: &ChatRecord, body: &Value) -> RunningTurn {
        let cancel = CancellationToken::new();
        let send =
            self.service
                .validate(RawSendBody {
                    content: body["content"].as_str().expect("content").to_owned(),
                    request_id: body
                        .get("request_id")
                        .and_then(Value::as_str)
                        .map(|id| Uuid::parse_str(id).expect("request id")),
                    attachment_ids: body.get("attachment_ids").and_then(Value::as_array).map(
                        |ids| {
                            ids.iter()
                                .map(|id| Uuid::parse_str(id.as_str().expect("id")).expect("uuid"))
                                .collect()
                        },
                    ),
                    web_search_enabled: body["web_search"]["enabled"].as_bool(),
                })
                .expect("a valid send");
        let outcome = self
            .service
            .send(&a1(), chat.id, send, &cancel)
            .await
            .expect("the send setup");
        let crate::domain::streaming::SendOutcome::Streaming(started) = outcome else {
            panic!("expected a running provider task, got {outcome:?}");
        };
        let StartedTurn { plan, events } = *started;
        RunningTurn {
            plan,
            events,
            cancel,
        }
    }

    /// A real provider task over `upstream` that shares the finalizer, the progress store, the
    /// quota service and the metrics of this rig, for tests that start it with a plan they hold
    /// (the rig itself may run the recording double).
    pub fn task_over(
        &self,
        upstream: Arc<MockProvider>,
        s2s: Arc<S2sContext>,
        settings: ProviderTaskSettings,
    ) -> StreamingProviderTask {
        let metrics = self.metrics.as_ref().map_or_else(
            || {
                Arc::new(MiniChatMetrics::new(
                    &opentelemetry::global::meter("mini-chat-send-rig"),
                    "mini_chat",
                ))
            },
            |harness| Arc::clone(&harness.metrics),
        );
        StreamingProviderTask::new(ProviderTaskDeps {
            gateway: upstream,
            s2s,
            finalizer: Arc::clone(&self.finalizer),
            progress: self.quota.db.provider.clone(),
            quota: Arc::clone(&self.quota.service),
            metrics,
            settings,
            knowledge: None,
        })
    }

    /// Run the send setup of user 1 of tenant A directly (no HTTP) with a provider task that
    /// produces no events (the recording double: a closed channel) and return the committed
    /// turn's plan.
    pub async fn plan_for(&self, chat: &ChatRecord, content: &str) -> ProviderCallPlan {
        let cancel = CancellationToken::new();
        let send = self
            .service
            .validate(RawSendBody {
                content: content.to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search_enabled: None,
            })
            .expect("a valid send");
        let outcome = self
            .service
            .send(&a1(), chat.id, send, &cancel)
            .await
            .expect("the send setup");
        let crate::domain::streaming::SendOutcome::Streaming(started) = outcome else {
            panic!("expected a started turn, got {outcome:?}");
        };
        started.plan
    }

    /// `POST /chats/{id}/messages:stream` as user 1 of tenant A, returned as soon as the response
    /// head is available so the body can be read frame by frame; dropping the result drops the
    /// body (the client disconnects).
    pub async fn open_stream(&self, chat_id: Uuid, body: &Value) -> OpenStream {
        let request =
            super::chats::json_request("POST", &stream_uri(chat_id), &a1(), &body.to_string());
        self.open_request(request).await
    }

    /// Send any request and return as soon as the response head is available (an SSE body is
    /// read frame by frame; dropping the result drops the body, the client disconnects).
    pub async fn open_request(&self, request: Request<Body>) -> OpenStream {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let headers = response.headers().clone();
        OpenStream {
            status,
            content_type,
            headers,
            body: Some(response.into_body().into_data_stream()),
            buffer: String::new(),
        }
    }

    /// Wait (5 s at most) until the turn of the chat left `running`; returns it.
    pub async fn wait_terminal(&self, chat_id: Uuid) -> chat_turn::Model {
        for _ in 0..500 {
            let turns = turn_rows(&self.quota.db, chat_id).await;
            if let Some(turn) = turns.into_iter().find(|turn| turn.state != "running") {
                return turn;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the turn is still running");
    }

    /// Wait (5 s at most) until the chat has at least one turn and none of them is `running`.
    pub async fn wait_idle(&self, chat_id: Uuid) {
        for _ in 0..500 {
            let turns = turn_rows(&self.quota.db, chat_id).await;
            if !turns.is_empty() && turns.iter().all(|turn| turn.state != "running") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("a turn is still running");
    }

    /// Every audit event of the audit queue, typed.
    pub async fn audit_events(&self) -> Vec<mini_chat_sdk::AuditEvent> {
        self.audit_payloads()
            .await
            .into_iter()
            .map(|payload| serde_json::from_value(payload).expect("an audit event payload"))
            .collect()
    }

    /// The raw JSON payloads of the audit queue.
    pub async fn audit_payloads(&self) -> Vec<Value> {
        let queue = self.quota.outbox.audit_queue_name.clone();
        super::chats::outbox_rows(&self.quota.db)
            .await
            .into_iter()
            .filter(|row| row.queue == queue)
            .map(|row| row.payload)
            .collect()
    }

    /// The thread-summary work messages with their partition and payload.
    pub async fn summary_rows(&self) -> Vec<super::chats::OutboxRow> {
        let queue = self.quota.outbox.thread_summary_queue_name.clone();
        super::chats::outbox_rows(&self.quota.db)
            .await
            .into_iter()
            .filter(|row| row.queue == queue)
            .collect()
    }

    /// Send one request through the router.
    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        let content_type = header("content-type").unwrap_or_default();
        let location = header("location");
        let retry_after = header("retry-after");
        let bytes = to_bytes(response.into_body(), 1 << 22).await.unwrap();
        Reply {
            status,
            content_type,
            location,
            retry_after,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    /// `GET uri` as `who`.
    pub async fn get(&self, uri: &str, who: &SecurityContext) -> Reply {
        self.send(request("GET", uri, who)).await
    }

    /// `POST uri` with a JSON body as `who`.
    pub async fn post_json(&self, uri: &str, who: &SecurityContext, body: &Value) -> Reply {
        self.send(super::chats::json_request(
            "POST",
            uri,
            who,
            &body.to_string(),
        ))
        .await
    }

    /// `POST /chats/{id}/messages:stream` as user 1 of tenant A.
    pub async fn stream(&self, chat_id: Uuid, body: &Value) -> Reply {
        self.post_json(&stream_uri(chat_id), &a1(), body).await
    }

    /// A live chat of user 1 of tenant A on the standard model.
    pub async fn std_chat(&self) -> ChatRecord {
        self.quota.chat_a1(STANDARD).await
    }

    /// A live chat of user 1 of tenant A on the premium model.
    pub async fn prem_chat(&self) -> ChatRecord {
        self.quota.chat_a1(PREMIUM).await
    }
}

/// The rows a send writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    pub messages: i64,
    pub turns: i64,
    pub links: i64,
    pub outbox: i64,
    /// The sum of `reserved_credits_micro` over every `quota_usage` row.
    pub reserved_credits: i64,
}

/// The [`Footprint`] of a chat.
pub async fn footprint(db: &TestDb, chat: &ChatRecord) -> Footprint {
    let id = super::chats::blob(chat.id);
    let count = |sql: String| async move { super::raw_i64(&db.raw, &sql).await };
    Footprint {
        messages: count(format!(
            "SELECT COUNT(*) FROM messages WHERE chat_id = {id}"
        ))
        .await,
        turns: count(format!(
            "SELECT COUNT(*) FROM chat_turns WHERE chat_id = {id}"
        ))
        .await,
        links: count(format!(
            "SELECT COUNT(*) FROM message_attachments WHERE chat_id = {id}"
        ))
        .await,
        outbox: super::chats::outbox_body_count(db).await,
        reserved_credits: count(
            "SELECT COALESCE(SUM(reserved_credits_micro), 0) FROM quota_usage".to_owned(),
        )
        .await,
    }
}

// ── time ─────────────────────────────────────────────────────────────────────

/// `default_now()` plus `micros` microseconds: the instants of the messages of the fixtures.
pub fn t(micros: i64) -> OffsetDateTime {
    super::chats::at(0) + time::Duration::microseconds(micros)
}

// ── seeding ──────────────────────────────────────────────────────────────────

/// A `messages` row to insert.
#[derive(Debug, Clone)]
pub struct MessageSeed {
    pub id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: &'static str,
    pub content: String,
    pub created_at: OffsetDateTime,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub is_compressed: bool,
    pub deleted_at: Option<OffsetDateTime>,
}

impl MessageSeed {
    /// A live message with a fresh `request_id`, no model, no tokens.
    pub fn new(role: &'static str, content: &str, created_at: OffsetDateTime) -> Self {
        Self {
            id: Uuid::new_v4(),
            request_id: Some(Uuid::new_v4()),
            role,
            content: content.to_owned(),
            created_at,
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            is_compressed: false,
            deleted_at: None,
        }
    }

    pub fn id(mut self, id: Uuid) -> Self {
        self.id = id;
        self
    }

    pub fn request(mut self, request_id: Option<Uuid>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    pub fn tokens(mut self, input: i64, output: i64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    pub fn compressed(mut self) -> Self {
        self.is_compressed = true;
        self
    }

    pub fn deleted(mut self, at: OffsetDateTime) -> Self {
        self.deleted_at = Some(at);
        self
    }
}

/// Insert a message through the Secure ORM (timestamps are stored exactly as production writes
/// them); returns its id.
pub async fn seed_msg(db: &TestDb, chat: &ChatRecord, seed: MessageSeed) -> Uuid {
    let conn = db.provider.conn().unwrap();
    let row = message::ActiveModel {
        id: Set(seed.id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(seed.request_id),
        role: Set(seed.role.to_owned()),
        content: Set(seed.content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(seed.input_tokens),
        output_tokens: Set(seed.output_tokens),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(seed.model),
        is_compressed: Set(seed.is_compressed),
        created_at: Set(seed.created_at),
        deleted_at: Set(seed.deleted_at),
    };
    secure_insert::<message::Entity>(row, &AccessScope::for_tenant(chat.tenant_id), &conn)
        .await
        .expect("seed message");
    seed.id
}

/// A `chat_turns` row to insert.
#[derive(Debug, Clone)]
pub struct TurnSeedFull {
    pub id: Uuid,
    pub request_id: Uuid,
    pub state: &'static str,
    pub requester_user_id: Option<Uuid>,
    pub assistant_message_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub effective_model: Option<String>,
    pub deleted_at: Option<OffsetDateTime>,
    pub started_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// `chat_turns.web_search_enabled`.
    pub web_search_enabled: bool,
    /// `(web_search, code_interpreter, file_search)` completed counts.
    pub counts: (i32, i32, i32),
    /// `chat_turns.replaced_by_request_id`.
    pub replaced_by: Option<Uuid>,
}

impl TurnSeedFull {
    /// A turn of `state` of user 1 with a fresh `request_id`, started and updated at `t(0)`.
    pub fn new(state: &'static str) -> Self {
        Self {
            id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            state,
            requester_user_id: Some(USER_A1),
            assistant_message_id: None,
            error_code: None,
            effective_model: None,
            deleted_at: None,
            started_at: t(0),
            updated_at: t(0),
            web_search_enabled: false,
            counts: (0, 0, 0),
            replaced_by: None,
        }
    }

    /// The `started_at` of the turn (the latest-turn order key).
    pub fn started(mut self, at: OffsetDateTime) -> Self {
        self.started_at = at;
        self
    }

    /// The requester of the turn (`None`: a NULL requester).
    pub fn requester(mut self, user_id: Option<Uuid>) -> Self {
        self.requester_user_id = user_id;
        self
    }

    pub fn web_search(mut self, enabled: bool) -> Self {
        self.web_search_enabled = enabled;
        self
    }

    /// The completed tool counts `(web_search, code_interpreter, file_search)`.
    pub fn counts(mut self, web: i32, code: i32, file: i32) -> Self {
        self.counts = (web, code, file);
        self
    }

    pub fn replaced_by(mut self, request_id: Uuid) -> Self {
        self.replaced_by = Some(request_id);
        self
    }

    pub fn id(mut self, id: Uuid) -> Self {
        self.id = id;
        self
    }

    pub fn request(mut self, request_id: Uuid) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn assistant(mut self, message_id: Uuid) -> Self {
        self.assistant_message_id = Some(message_id);
        self
    }

    pub fn error(mut self, code: &str) -> Self {
        self.error_code = Some(code.to_owned());
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        self.effective_model = Some(model.to_owned());
        self
    }

    pub fn deleted(mut self, at: OffsetDateTime) -> Self {
        self.deleted_at = Some(at);
        self
    }

    pub fn updated(mut self, at: OffsetDateTime) -> Self {
        self.updated_at = at;
        self
    }
}

/// Insert a turn of the chat through the Secure ORM; returns its id. A terminal state gets
/// `completed_at = updated_at`; a running turn gets `last_progress_at = started_at`.
pub async fn seed_turn_full(db: &TestDb, chat: &ChatRecord, seed: TurnSeedFull) -> Uuid {
    let conn = db.provider.conn().unwrap();
    let terminal = seed.state != "running";
    let row = chat_turn::ActiveModel {
        id: Set(seed.id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(seed.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(seed.requester_user_id),
        state: Set(seed.state.to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(seed.assistant_message_id),
        error_code: Set(seed.error_code),
        reserve_tokens: Set(None),
        max_output_tokens_applied: Set(None),
        reserved_credits_micro: Set(None),
        policy_version_applied: Set(None),
        effective_model: Set(seed.effective_model),
        minimal_generation_floor_applied: Set(None),
        error_detail: Set(None),
        deleted_at: Set(seed.deleted_at),
        replaced_by_request_id: Set(seed.replaced_by),
        started_at: Set(seed.started_at),
        last_progress_at: Set((!terminal).then_some(seed.started_at)),
        web_search_enabled: Set(seed.web_search_enabled),
        web_search_completed_count: Set(seed.counts.0),
        code_interpreter_completed_count: Set(seed.counts.1),
        file_search_completed_count: Set(seed.counts.2),
        completed_at: Set(terminal.then_some(seed.updated_at)),
        updated_at: Set(seed.updated_at),
    };
    secure_insert::<chat_turn::Entity>(row, &AccessScope::for_tenant(chat.tenant_id), &conn)
        .await
        .expect("seed turn");
    seed.id
}

/// An `attachments` row to insert.
#[derive(Debug, Clone)]
pub struct AttachmentSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub kind: &'static str,
    pub status: &'static str,
    pub filename: String,
    pub provider_file_id: Option<String>,
    pub for_code_interpreter: bool,
    pub deleted_at: Option<OffsetDateTime>,
    pub thumbnail: Option<(i32, i32, Vec<u8>)>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: &'static str,
    /// `None`: `for_file_search` follows the kind (a document is routed to file search).
    pub for_file_search: Option<bool>,
    /// `None`: `t(0)`.
    pub created_at: Option<OffsetDateTime>,
}

impl AttachmentSeed {
    /// A ready attachment of `kind` uploaded by user 1 of tenant A, with a provider file id.
    pub fn ready(kind: &'static str) -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            tenant_id: TENANT_A,
            uploaded_by_user_id: USER_A1,
            kind,
            status: "ready",
            filename: format!("{kind}-{}.bin", id.simple()),
            provider_file_id: Some(format!("file-{}", id.simple())),
            for_code_interpreter: false,
            deleted_at: None,
            thumbnail: None,
            secondary_file_id: None,
            secondary_status: "not_attempted",
            for_file_search: None,
            created_at: None,
        }
    }

    /// A document that is not in the chat vector store (an XLSX file): `for_file_search` false.
    pub fn no_file_search(mut self) -> Self {
        self.for_file_search = Some(false);
        self
    }

    /// The row has no provider file id.
    pub fn no_provider_file(mut self) -> Self {
        self.provider_file_id = None;
        self
    }

    /// The provider file id of the row.
    pub fn file_id(mut self, provider_file_id: &str) -> Self {
        self.provider_file_id = Some(provider_file_id.to_owned());
        self
    }

    pub fn created(mut self, at: OffsetDateTime) -> Self {
        self.created_at = Some(at);
        self
    }

    /// An Anthropic secondary copy of the attachment with the given file id and status.
    pub fn secondary(mut self, file_id: &str, status: &'static str) -> Self {
        self.secondary_file_id = Some(file_id.to_owned());
        self.secondary_status = status;
        self
    }

    pub fn status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    pub fn filename(mut self, filename: &str) -> Self {
        self.filename = filename.to_owned();
        self
    }

    pub fn uploaded_by(mut self, user_id: Uuid) -> Self {
        self.uploaded_by_user_id = user_id;
        self
    }

    pub fn tenant(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = tenant_id;
        self
    }

    pub fn deleted(mut self, at: OffsetDateTime) -> Self {
        self.deleted_at = Some(at);
        self
    }

    pub fn code_interpreter(mut self) -> Self {
        self.for_code_interpreter = true;
        self
    }

    pub fn thumbnail(mut self, width: i32, height: i32, data: &[u8]) -> Self {
        self.thumbnail = Some((width, height, data.to_vec()));
        self
    }
}

/// Insert an attachment into the chat (the chat id of the row is the chat given, the tenant is
/// the seed's); returns its id.
pub async fn seed_att(db: &TestDb, chat: &ChatRecord, seed: AttachmentSeed) -> Uuid {
    let conn = db.provider.conn().unwrap();
    let (width, height, data) = match seed.thumbnail {
        Some((w, h, d)) => (Some(w), Some(h), Some(d)),
        None => (None, None, None),
    };
    let when = seed.created_at.unwrap_or_else(|| t(0));
    let secondary_kind = seed
        .secondary_file_id
        .as_ref()
        .map(|_| "anthropic".to_owned());
    let row = attachment::ActiveModel {
        id: Set(seed.id),
        tenant_id: Set(seed.tenant_id),
        chat_id: Set(chat.id),
        uploaded_by_user_id: Set(seed.uploaded_by_user_id),
        filename: Set(seed.filename),
        content_type: Set(None),
        size_bytes: Set(None),
        storage_backend: Set("azure_openai".to_owned()),
        provider_file_id: Set(seed.provider_file_id),
        status: Set(seed.status.to_owned()),
        error_code: Set(None),
        attachment_kind: Set(seed.kind.to_owned()),
        for_file_search: Set(seed.for_file_search.unwrap_or(seed.kind == "document")),
        for_code_interpreter: Set(seed.for_code_interpreter),
        doc_summary: Set(None),
        img_thumbnail: Set(data),
        img_thumbnail_width: Set(width),
        img_thumbnail_height: Set(height),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(when),
        updated_at: Set(when),
        deleted_at: Set(seed.deleted_at),
        secondary_file_id: Set(seed.secondary_file_id),
        secondary_status: Set(seed.secondary_status.to_owned()),
        secondary_provider_kind: Set(secondary_kind),
    };
    secure_insert::<attachment::Entity>(row, &AccessScope::for_tenant(seed.tenant_id), &conn)
        .await
        .expect("seed attachment");
    seed.id
}

/// Link an attachment to a message (`message_attachments`).
pub async fn link_att(db: &TestDb, chat: &ChatRecord, message_id: Uuid, attachment_id: Uuid) {
    let conn = db.provider.conn().unwrap();
    let row = message_attachment::ActiveModel {
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(t(0)),
    };
    secure_insert::<message_attachment::Entity>(
        row,
        &AccessScope::for_tenant(chat.tenant_id),
        &conn,
    )
    .await
    .expect("link attachment");
}

/// Insert the thread summary of the chat.
pub async fn seed_summary(
    db: &TestDb,
    chat: &ChatRecord,
    text: &str,
    token_estimate: i32,
    frontier: (OffsetDateTime, Uuid),
) {
    let conn = db.provider.conn().unwrap();
    let row = thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        summary_text: Set(text.to_owned()),
        summarized_up_to_created_at: Set(frontier.0),
        summarized_up_to_message_id: Set(frontier.1),
        token_estimate: Set(token_estimate),
        created_at: Set(t(0)),
        updated_at: Set(t(0)),
    };
    secure_insert::<thread_summary::Entity>(row, &AccessScope::for_tenant(chat.tenant_id), &conn)
        .await
        .expect("seed summary");
}

/// Insert a reaction of `user_id` on a message.
pub async fn seed_reaction_of(
    db: &TestDb,
    tenant_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    reaction: &str,
) {
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, \
             created_at) VALUES ({}, {}, {}, {}, '{reaction}', '2026-01-01 00:00:00+00:00')",
            super::chats::blob(Uuid::new_v4()),
            super::chats::blob(message_id),
            super::chats::blob(user_id),
            super::chats::blob(tenant_id),
        ),
    )
    .await
    .expect("seed reaction");
}

/// Seed a completed turn of `chat`: a user message at `at_user`, an assistant message one
/// microsecond later and the `completed` turn linking them; returns `(request_id,
/// assistant_message_id)`.
pub async fn seed_completed_turn(
    db: &TestDb,
    chat: &ChatRecord,
    user_text: &str,
    assistant_text: &str,
    at_user: OffsetDateTime,
) -> (Uuid, Uuid) {
    let request_id = Uuid::new_v4();
    let assistant_id = Uuid::new_v4();
    seed_msg(
        db,
        chat,
        MessageSeed::new("user", user_text, at_user).request(Some(request_id)),
    )
    .await;
    seed_msg(
        db,
        chat,
        MessageSeed::new(
            "assistant",
            assistant_text,
            at_user + time::Duration::microseconds(1),
        )
        .id(assistant_id)
        .request(Some(request_id))
        .model(&chat.model)
        .tokens(120, 30),
    )
    .await;
    seed_turn_full(
        db,
        chat,
        TurnSeedFull::new("completed")
            .request(request_id)
            .assistant(assistant_id)
            .model(&chat.model),
    )
    .await;
    (request_id, assistant_id)
}

// ── direct inspection (no code under test) ───────────────────────────────────

/// Every turn row of the chat (soft-deleted ones included), ordered by `started_at` then id.
pub async fn turn_rows(db: &TestDb, chat_id: Uuid) -> Vec<chat_turn::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = chat_turn::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat_turn::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| (row.started_at, row.id));
    rows
}

/// Every `message_attachments` row of the chat.
pub async fn link_rows(db: &TestDb, chat_id: Uuid) -> Vec<message_attachment::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = message_attachment::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(message_attachment::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| row.attachment_id);
    rows
}

/// The chat row, whatever its state.
pub async fn chat_model(db: &TestDb, chat_id: Uuid) -> chat::Model {
    super::chats::chat_row(db, chat_id).await.expect("chat row")
}

// ── SSE ──────────────────────────────────────────────────────────────────────

/// One frame of an SSE body.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub event: String,
    pub data: Value,
}

/// The `event:` and `data:` frames of a finished SSE body (frames without an `event:` line, such
/// as comment keep-alives, are skipped).
pub fn parse_sse(text: &str) -> Vec<SseFrame> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push_str(value.trim_start());
                }
            }
            event.map(|event| SseFrame {
                event,
                data: serde_json::from_str(&data).unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// A turn started through the send setup with the real provider task: the plan, the receiver of
/// the relay channel and the turn's cancellation token.
pub struct RunningTurn {
    pub plan: ProviderCallPlan,
    pub events: mpsc::Receiver<RelayEvent>,
    pub cancel: CancellationToken,
}

impl RunningTurn {
    /// Every event until the channel closes (10 s at most).
    pub async fn drain(&mut self) -> Vec<RelayEvent> {
        let mut events = Vec::new();
        let read = async {
            while let Some(event) = self.events.recv().await {
                events.push(event);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .expect("the relay channel never closed");
        events
    }

    /// The next event (5 s at most); `None` when the channel closed.
    pub async fn next(&mut self) -> Option<RelayEvent> {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("no relay event arrived")
    }
}

/// An open SSE response whose body is read frame by frame; dropping it disconnects the client.
pub struct OpenStream {
    pub status: axum::http::StatusCode,
    pub content_type: String,
    pub headers: axum::http::HeaderMap,
    body: Option<axum::body::BodyDataStream>,
    buffer: String,
}

impl OpenStream {
    /// The next complete frame; `None` at the end of the body. Panics when nothing arrives
    /// within 5 s.
    pub async fn next_frame(&mut self) -> Option<SseFrame> {
        self.next_frame_within(Duration::from_secs(5))
            .await
            .expect("no SSE frame arrived in time")
    }

    /// The next complete frame; `Err(())` when nothing arrives within `limit`, `Ok(None)` at the
    /// end of the body.
    pub async fn next_frame_within(&mut self, limit: Duration) -> Result<Option<SseFrame>, ()> {
        use futures::StreamExt;
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(frame) = parse_sse(&block).into_iter().next() {
                    return Ok(Some(frame));
                }
                continue;
            }
            let Some(body) = self.body.as_mut() else {
                return Ok(None);
            };
            match tokio::time::timeout_at(deadline, body.next()).await {
                Err(_elapsed) => return Err(()),
                Ok(None) => {
                    self.body = None;
                    return Ok(None);
                }
                Ok(Some(chunk)) => {
                    let bytes = chunk.expect("a body chunk");
                    self.buffer
                        .push_str(std::str::from_utf8(&bytes).expect("utf-8 body"));
                }
            }
        }
    }

    /// Every remaining frame until the end of the body.
    pub async fn rest(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame().await {
            frames.push(frame);
        }
        frames
    }

    /// Frames until (and including) the first one named `event`.
    pub async fn until(&mut self, event: &str) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame().await {
            let done = frame.event == event;
            frames.push(frame);
            if done {
                return frames;
            }
        }
        panic!("the stream ended before `{event}`: {frames:?}");
    }

    /// Whether the body reached its end (no more bytes will come).
    pub fn disconnect(self) {
        drop(self);
    }
}

/// The names of the frames.
pub fn names(frames: &[SseFrame]) -> Vec<&str> {
    frames.iter().map(|frame| frame.event.as_str()).collect()
}

/// The text of every `delta` frame joined.
pub fn delta_text(frames: &[SseFrame]) -> String {
    frames
        .iter()
        .filter(|frame| frame.event == "delta")
        .map(|frame| frame.data["content"].as_str().unwrap_or_default())
        .collect()
}
