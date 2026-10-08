//! Domain services and the shared service container used by the REST layer.

pub mod attachment_service;
pub mod chat_service;
pub mod cleanup;
pub mod finalization;
pub mod message_service;
pub mod model_resolver;
pub mod quota_service;
pub mod reaction_service;
pub mod replay;
pub mod stream_service;
pub mod thread_summary;
pub mod turn_runner;
pub mod turn_service;

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use oagw_sdk::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use tracing::warn;
use uuid::Uuid;

use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::authz::Pep;
use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    KnowledgeRetriever, LlmPort, OutboxPort, PolicyPort, SecondaryFilesPort, StoragePort,
};
use crate::infra::llm::gateway::LlmGateway;
use crate::infra::llm::provider_resolver::ProviderResolver;
use crate::infra::llm::storage::RagStorage;
use crate::infra::llm::storage::anthropic_files::AnthropicFiles;
use crate::infra::llm::storage::knowledge::AzureKnowledgeRetriever;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::s2s::S2sContextProvider;

pub use attachment_service::{AttachmentService, IndexingTimings};
pub use chat_service::ChatService;
pub use cleanup::CleanupService;
pub use finalization::FinalizationService;
pub use message_service::MessageService;
pub use model_resolver::ModelResolver;
pub use quota_service::QuotaService;
pub use reaction_service::ReactionService;
pub use stream_service::StreamService;
pub use thread_summary::ThreadSummaryService;
pub use turn_runner::TurnRunner;
pub use turn_service::TurnService;

/// Dependencies the services are built from (gear `init` and the test harness).
pub struct AppDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub policy: Arc<dyn PolicyPort>,
    pub enforcer: PolicyEnforcer,
    pub outbox: Arc<dyn OutboxPort>,
    /// OAGW client used for every provider call.
    pub oagw: Arc<dyn ServiceGatewayClientV1>,
    /// S2S security context of the OAGW calls.
    pub s2s: Arc<S2sContextProvider>,
    /// Indexing waits of uploads ([`IndexingTimings::default`] in production).
    pub indexing: IndexingTimings,
    /// Gear shutdown token; background tasks started by requests run on
    /// child tokens of it.
    pub shutdown: CancellationToken,
    /// OpenTelemetry instruments ([`MiniChatMetrics::noop`] in tests).
    pub metrics: Arc<MiniChatMetrics>,
}

/// Every service the REST handlers use (injected as `Extension<Arc<AppServices>>`).
pub struct AppServices {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub pep: Arc<Pep>,
    pub models: Arc<ModelResolver>,
    pub outbox: Arc<dyn OutboxPort>,
    pub chats: Arc<ChatService>,
    /// Quota preflight / reserve / settlement / status.
    pub quota: Arc<QuotaService>,
    /// Provider entry / OAGW alias resolution (from `config.providers`).
    pub providers: Arc<ProviderResolver>,
    /// LLM calls through OAGW.
    pub llm: Arc<dyn LlmPort>,
    /// Provider file / vector-store calls through OAGW.
    pub storage: Arc<dyn StoragePort>,
    /// Stream finalization (CAS, settlement, outbox).
    pub finalization: Arc<FinalizationService>,
    /// Send pipeline (`messages:stream`).
    pub streams: Arc<StreamService>,
    /// Messages list.
    pub messages: Arc<MessageService>,
    /// Turn status and mutations (retry / edit / delete).
    pub turns: Arc<TurnService>,
    /// Idle interval of the SSE `ping` event before the first content event
    /// (`streaming.sse_ping_interval_seconds`; tests may shorten it).
    pub sse_ping_interval: Duration,
    /// Gear shutdown token (parent of the upload background tasks).
    pub shutdown: CancellationToken,
    /// Attachments (upload / get / delete).
    pub attachments: Arc<AttachmentService>,
    /// Message reactions (set / remove).
    pub reactions: Arc<ReactionService>,
    /// Provider cleanup of deleted attachments / chats (outbox handlers).
    pub cleanup: Arc<CleanupService>,
    /// Thread summary trigger (finalization hook) and outbox runner.
    pub summaries: Arc<ThreadSummaryService>,
    /// Model policy (snapshots of a turn's policy version: orphan watchdog).
    pub policy: Arc<dyn PolicyPort>,
    /// OpenTelemetry instruments.
    pub metrics: Arc<MiniChatMetrics>,
}

impl AppServices {
    #[must_use]
    pub fn new(deps: AppDeps) -> Self {
        let pep = Arc::new(Pep::new(deps.enforcer));
        let models = Arc::new(ModelResolver::new(Arc::clone(&deps.policy)));
        let chats = Arc::new(ChatService::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.clock),
            Arc::clone(&pep),
            Arc::clone(&models),
            Arc::clone(&deps.outbox),
        ));
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.clock),
            Arc::clone(&pep),
            Arc::clone(&deps.policy),
            Arc::clone(&deps.config),
            Arc::clone(&deps.metrics),
        ));
        let providers = Arc::new(ProviderResolver::new(&deps.config.providers));
        let llm: Arc<dyn LlmPort> = Arc::new(LlmGateway::new(
            Arc::clone(&deps.oagw),
            Arc::clone(&deps.s2s),
        ));
        let storage: Arc<dyn StoragePort> = Arc::new(RagStorage::new(
            Arc::clone(&deps.oagw),
            Arc::clone(&deps.s2s),
        ));
        let knowledge = knowledge_retriever(&deps.config, &deps.oagw, &deps.s2s, &providers);
        let secondary: Option<Arc<dyn SecondaryFilesPort>> = deps
            .config
            .providers
            .values()
            .any(|p| p.kind == ProviderKind::AnthropicMessages)
            .then(|| {
                Arc::new(AnthropicFiles::new(
                    Arc::clone(&deps.oagw),
                    Arc::clone(&deps.s2s),
                )) as Arc<dyn SecondaryFilesPort>
            });
        let summaries = Arc::new(ThreadSummaryService::new(
            thread_summary::ThreadSummaryDeps {
                config: Arc::clone(&deps.config),
                db: Arc::clone(&deps.db),
                clock: Arc::clone(&deps.clock),
                policy: Arc::clone(&deps.policy),
                providers: Arc::clone(&providers),
                llm: Arc::clone(&llm),
                outbox: Arc::clone(&deps.outbox),
                metrics: Arc::clone(&deps.metrics),
            },
        ));
        let finalization = Arc::new(FinalizationService::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.clock),
            Arc::clone(&quota),
            Arc::clone(&deps.outbox),
            summaries.clone(),
            Arc::clone(&deps.config),
            Arc::clone(&deps.metrics),
        ));
        let runner = TurnRunner::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.clock),
            Arc::clone(&llm),
            Arc::clone(&finalization),
            Arc::clone(&deps.config),
            Arc::clone(&deps.metrics),
        )
        .with_knowledge(knowledge.clone());
        let streams = Arc::new(StreamService::new(stream_service::StreamDeps {
            config: Arc::clone(&deps.config),
            db: Arc::clone(&deps.db),
            clock: Arc::clone(&deps.clock),
            chats: Arc::clone(&chats),
            models: Arc::clone(&models),
            policy: Arc::clone(&deps.policy),
            quota: Arc::clone(&quota),
            providers: Arc::clone(&providers),
            runner,
            knowledge,
        }));
        let messages = Arc::new(MessageService::new(
            Arc::clone(&deps.db),
            Arc::clone(&chats),
        ));
        let turns = Arc::new(TurnService::new(turn_service::TurnDeps {
            db: Arc::clone(&deps.db),
            clock: Arc::clone(&deps.clock),
            chats: Arc::clone(&chats),
            streams: Arc::clone(&streams),
            outbox: Arc::clone(&deps.outbox),
            metrics: Arc::clone(&deps.metrics),
        }));
        let attachments = Arc::new(AttachmentService::new(attachment_service::AttachmentDeps {
            config: Arc::clone(&deps.config),
            db: Arc::clone(&deps.db),
            clock: Arc::clone(&deps.clock),
            chats: Arc::clone(&chats),
            models: Arc::clone(&models),
            providers: Arc::clone(&providers),
            storage: Arc::clone(&storage),
            outbox: Arc::clone(&deps.outbox),
            indexing: deps.indexing,
            shutdown: deps.shutdown.clone(),
            metrics: Arc::clone(&deps.metrics),
            secondary: secondary.clone(),
        }));
        let cleanup = Arc::new(
            CleanupService::new(
                Arc::clone(&deps.db),
                Arc::clone(&deps.clock),
                Arc::clone(&storage),
                Arc::clone(&providers),
                deps.config.cleanup_worker.max_attempts,
                Arc::clone(&deps.metrics),
            )
            .with_secondary(secondary, Arc::clone(&models)),
        );
        let reactions = Arc::new(ReactionService::new(
            Arc::clone(&deps.db),
            Arc::clone(&deps.clock),
            Arc::clone(&chats),
        ));
        let sse_ping_interval =
            Duration::from_secs(u64::from(deps.config.streaming.sse_ping_interval_seconds));
        Self {
            config: deps.config,
            db: deps.db,
            clock: deps.clock,
            pep,
            models,
            outbox: deps.outbox,
            chats,
            quota,
            providers,
            llm,
            storage,
            finalization,
            streams,
            messages,
            turns,
            sse_ping_interval,
            shutdown: deps.shutdown,
            attachments,
            reactions,
            cleanup,
            summaries,
            policy: deps.policy,
            metrics: deps.metrics,
        }
    }
}

/// The knowledge retriever of an enabled knowledge search: the Azure vector
/// store `knowledge_search.vector_store_id` behind the
/// `knowledge_search.provider_id` entry (its default alias). `None` when
/// disabled or when the entry is unknown (logged).
fn knowledge_retriever(
    config: &MiniChatConfig,
    oagw: &Arc<dyn ServiceGatewayClientV1>,
    s2s: &Arc<S2sContextProvider>,
    providers: &ProviderResolver,
) -> Option<Arc<dyn KnowledgeRetriever>> {
    let k = &config.knowledge_search;
    if !k.enabled {
        return None;
    }
    let (provider_id, vector_store_id) = (k.provider_id.as_deref()?, k.vector_store_id.clone()?);
    match providers.knowledge_target(provider_id, Uuid::nil()) {
        Ok((_, target)) => Some(Arc::new(AzureKnowledgeRetriever::new(
            Arc::clone(oagw),
            Arc::clone(s2s),
            target,
            vector_store_id,
        ))),
        Err(e) => {
            warn!(error = %e, "knowledge retriever not created");
            None
        }
    }
}
