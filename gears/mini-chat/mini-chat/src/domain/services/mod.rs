//! Domain services and their single construction point, [`build_services`],
//! shared by the gear (`gear.rs`) and the test harness (`mini_chat::testing`).

pub mod attachment;
pub mod chat;
pub mod cleanup;
pub mod context;
pub mod finalization;
pub mod message;
pub mod model_catalog;
pub mod quota;
pub mod quota_status;
pub mod reaction;
pub mod stream;
pub mod thread_summary;
pub mod turn;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::config::ProviderKind;
use crate::domain::authz::ChatAuthz;
use crate::domain::error::DomainError;
use crate::infra::gateways::model_policy::ModelPolicyGateway;
use crate::infra::llm::{
    AnthropicFilesClient, KnowledgeSearch, LlmClient, ProviderResolver, RagClient,
};
use crate::infra::oagw::s2s::S2sContext;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::workers::leader::LeaderElector;
use crate::infra::workers::orphan_watchdog::{OrphanWatchdog, OrphanWatchdogDeps};
use crate::infra::workers::upload_reaper::{UploadReaper, UploadReaperDeps};

pub use attachment::{
    AttachmentDeps, AttachmentService, PartMeta, UploadContext, UploadPart, UploadTimings,
};
pub use chat::{ChatService, ChatView};
pub use cleanup::{CleanupDeps, CleanupOutcome, CleanupService};
pub use context::{
    ContextInput, ContextPlan, HistoryMessage, SUMMARY_PREAMBLE, ToolSet, assemble, guards_for,
};
pub use finalization::{
    FinalizationDeps, FinalizationService, FinalizeInput, FinalizeResult, TerminalKind,
    derive_billing,
};
pub use message::{MessageService, MessageView};
pub use model_catalog::{ModelCatalogService, ResolvedModel};
pub use quota::{PreflightDecision, PreflightInput, QuotaService, Settlement};
pub use quota_status::{
    PeriodStatus, QuotaStatus, QuotaStatusService, QuotaTier, TierStatus, compute_tier_status,
};
pub use reaction::ReactionService;
pub use stream::{
    LiveStream, PreparedTurn, SendRequest, StreamDeps, StreamService, StreamStart, TurnMode,
    TurnPrep,
};
pub use thread_summary::{ThreadSummaryDeps, ThreadSummaryService};
pub use turn::{TurnDeps, TurnService};

/// Infrastructure the services are built from.
pub struct ServiceDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<ChatAuthz>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub outbox: Arc<OutboxEnqueuer>,
    /// Provider client over OAGW.
    pub llm: Arc<LlmClient>,
    /// The gear's S2S security context used for every OAGW call.
    pub s2s: Arc<S2sContext>,
    /// Files / vector stores client over OAGW.
    pub rag: Arc<RagClient>,
    /// Provider resolution, shared with OAGW provisioning (which routes it by
    /// the aliases OAGW registered).
    pub providers: Arc<ProviderResolver>,
    /// Upload and indexing waits (documented defaults; shortened in tests).
    pub upload_timings: UploadTimings,
    /// Root "gear stop" token: background tasks end when it is cancelled.
    pub stop: CancellationToken,
    /// Leader election of the orphan watchdog and the upload reaper.
    pub elector: Arc<dyn LeaderElector>,
}

/// Every domain service of the gear.
#[derive(Clone)]
pub struct Services {
    pub chats: Arc<ChatService>,
    pub messages: Arc<MessageService>,
    pub reactions: Arc<ReactionService>,
    pub models: Arc<ModelCatalogService>,
    pub quota_status: Arc<QuotaStatusService>,
    pub quota: Arc<QuotaService>,
    pub finalization: Arc<FinalizationService>,
    pub stream: Arc<StreamService>,
    pub turns: Arc<TurnService>,
    pub attachments: Arc<AttachmentService>,
    /// Thread-summary scheduling and the outbox task execution.
    pub thread_summary: Arc<ThreadSummaryService>,
    /// Provider file / vector store cleanup behind the two cleanup queues.
    pub cleanup: Arc<CleanupService>,
    /// Fails stale `running` turns (spawned by the gear under the leader elector).
    pub orphan_watchdog: Arc<OrphanWatchdog>,
    /// Fails abandoned `pending` / `uploaded` attachments (same).
    pub upload_reaper: Arc<UploadReaper>,
    pub llm: Arc<LlmClient>,
    pub rag: Arc<RagClient>,
    pub providers: Arc<ProviderResolver>,
    pub s2s: Arc<S2sContext>,
}

/// Build all services from `deps`.
#[must_use]
pub fn build_services(deps: ServiceDeps) -> Services {
    let ServiceDeps {
        config,
        db,
        authz,
        policy,
        outbox,
        llm,
        s2s,
        rag,
        providers,
        upload_timings,
        stop,
        elector,
    } = deps;
    // Optional clients (DESIGN §3.2 "Gear lifecycle" `init`).
    let anthropic_files = providers
        .has_kind(ProviderKind::AnthropicMessages)
        .then(|| Arc::new(AnthropicFilesClient::new(Arc::clone(&rag))));
    let knowledge = config.knowledge_search.enabled.then(|| {
        Arc::new(KnowledgeSearch::new(
            &config.knowledge_search,
            Arc::clone(&providers),
            Arc::clone(&rag),
        ))
    });
    let models = Arc::new(ModelCatalogService::new(
        Arc::clone(&policy),
        Arc::clone(&authz),
    ));
    let chats = Arc::new(ChatService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&models),
        Arc::clone(&outbox),
    ));
    let messages = Arc::new(MessageService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&chats),
    ));
    let reactions = Arc::new(ReactionService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&chats),
    ));
    let quota = Arc::new(QuotaService::new(Arc::clone(&config), Arc::clone(&db)));
    let finalization = Arc::new(FinalizationService::new(FinalizationDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        policy: Arc::clone(&policy),
        outbox: Arc::clone(&outbox),
        quota: Arc::clone(&quota),
    }));
    let stream = Arc::new(StreamService::new(StreamDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        authz: Arc::clone(&authz),
        chats: Arc::clone(&chats),
        models: Arc::clone(&models),
        policy: Arc::clone(&policy),
        quota: Arc::clone(&quota),
        finalization: Arc::clone(&finalization),
        llm: Arc::clone(&llm),
        providers: Arc::clone(&providers),
        knowledge,
    }));
    let turns = Arc::new(TurnService::new(TurnDeps {
        db: Arc::clone(&db),
        authz: Arc::clone(&authz),
        chats: Arc::clone(&chats),
        models: Arc::clone(&models),
        stream: Arc::clone(&stream),
        outbox: Arc::clone(&outbox),
    }));
    let thread_summary = Arc::new(ThreadSummaryService::new(ThreadSummaryDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        policy: Arc::clone(&policy),
        llm: Arc::clone(&llm),
        providers: Arc::clone(&providers),
        outbox: Arc::clone(&outbox),
    }));
    let cleanup = Arc::new(CleanupService::new(CleanupDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        providers: Arc::clone(&providers),
        rag: Arc::clone(&rag),
        anthropic_files: anthropic_files.clone(),
        models: Arc::clone(&models),
    }));
    let orphan_watchdog = Arc::new(OrphanWatchdog::new(OrphanWatchdogDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        policy: Arc::clone(&policy),
        outbox: Arc::clone(&outbox),
        quota: Arc::clone(&quota),
        elector: Arc::clone(&elector),
    }));
    let upload_reaper = Arc::new(UploadReaper::new(UploadReaperDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        outbox: Arc::clone(&outbox),
        elector,
    }));
    let attachments = Arc::new(AttachmentService::new(AttachmentDeps {
        config: Arc::clone(&config),
        db: Arc::clone(&db),
        authz: Arc::clone(&authz),
        chats: Arc::clone(&chats),
        models: Arc::clone(&models),
        providers: Arc::clone(&providers),
        rag: Arc::clone(&rag),
        anthropic_files,
        outbox,
        timings: upload_timings,
        stop,
    }));
    let quota_status = Arc::new(QuotaStatusService::new(config, db, authz, policy));
    Services {
        chats,
        messages,
        reactions,
        models,
        quota_status,
        quota,
        finalization,
        stream,
        turns,
        attachments,
        thread_summary,
        cleanup,
        orphan_watchdog,
        upload_reaper,
        llm,
        rag,
        providers,
        s2s,
    }
}

/// Failure of a list endpoint: `OData` errors keep their own canonical mapping
/// (resource type `gts.cf.core.odata.query.v1~`).
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error(transparent)]
    OData(#[from] toolkit_odata::Error),
    #[error(transparent)]
    Domain(#[from] DomainError),
}
