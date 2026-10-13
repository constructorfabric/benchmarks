//! Composition of the gear's services. [`Services::build`] is the single construction path used by
//! gear `init` (and therefore by the test harness): repositories, gateways, the PEP and the domain
//! services are wired here and nowhere else.

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use toolkit::client_hub::ClientHub;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::attachment::{
    AttachmentCleanup, AttachmentRepository, AttachmentService, AttachmentServiceParts,
    ChatCleanup, CleanupRepository, IndexingTimings, StorageGateway, UploadReaper,
    UploadReaperRepository,
};
use crate::domain::authz::Pep;
use crate::domain::chat::ChatService;
use crate::domain::error::DomainError;
use crate::domain::message::MessageService;
use crate::domain::metrics::Metrics;
use crate::domain::models::ModelsService;
use crate::domain::mutation::{TurnMutationService, TurnMutationServiceParts};
use crate::domain::orphan::{OrphanWatchdog, OrphanWatchdogParts};
use crate::domain::ports::ModelPolicyGateway;
use crate::domain::quota::{QuotaService, QuotaStatusService};
use crate::domain::stream::{
    Finalizer, RunDeps, RunSettings, StreamConfig, StreamService, StreamServiceParts,
};
use crate::domain::summary::{SummaryService, SummaryServiceParts};
use crate::domain::turn::TurnService;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::llm::ConfiguredProviders;
use crate::infra::metrics::OtelMetrics;
use crate::infra::model_policy_gateway::GtsModelPolicyGateway;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::provider_storage::OagwStorage;
use crate::infra::storage::{
    AttachmentRepositoryImpl, ChatRepositoryImpl, ConversationRepositoryImpl,
    MessageRepositoryImpl, MutationRepositoryImpl, QuotaRepositoryImpl, SummaryRepositoryImpl,
    TurnRepositoryImpl,
};
use crate::infra::trace::OtelTraceIds;

/// The shortest interval between `last_progress_at` refreshes of a streaming turn.
const PROGRESS_REFRESH: Duration = Duration::from_secs(30);

/// The services of the running gear.
pub struct Services {
    /// The validated gear configuration.
    pub config: MiniChatConfig,
    /// The gear's database entrypoint.
    pub db: Arc<DBProvider<DomainError>>,
    /// Chat CRUD.
    pub chats: Arc<ChatService>,
    /// Models API.
    pub models: Arc<ModelsService>,
    /// Messages API and reactions.
    pub messages: Arc<MessageService>,
    /// Turn status API.
    pub turns: Arc<TurnService>,
    /// The send-message pipeline.
    pub stream: Arc<StreamService>,
    /// Retry, edit and delete of the latest turn.
    pub mutations: Arc<TurnMutationService>,
    /// The thread-summary handler (outbox-driven).
    pub summary: Arc<SummaryService>,
    /// The quota status API.
    pub quota_status: Arc<QuotaStatusService>,
    /// The model policy (catalog, limits, usage sink).
    pub policy: Arc<dyn ModelPolicyGateway>,
    /// Outbox enqueuer, bound to the pipeline by gear `start`.
    pub outbox: Arc<OutboxEnqueuer>,
    /// The orphan turn watchdog (spawned by gear `start`).
    pub watchdog: Arc<OrphanWatchdog>,
    /// Attachment upload, get and delete.
    pub attachments: Arc<AttachmentService>,
    /// The attachment cleanup handler (outbox-driven).
    pub attachment_cleanup: Arc<AttachmentCleanup>,
    /// The chat cleanup handler (outbox-driven).
    pub chat_cleanup: Arc<ChatCleanup>,
    /// The upload reaper (spawned by gear `start`).
    pub upload_reaper: Arc<UploadReaper>,
    /// Audit plugin gateway (delivery runs from the audit outbox handler).
    pub audit: Arc<AuditGateway>,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
    /// The in-process OAGW client.
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    /// The client hub (S2S authentication at gear start).
    pub hub: Arc<ClientHub>,
}

impl Services {
    /// Builds the services from a validated configuration.
    ///
    /// The model-policy and audit plugins are resolved lazily through the types registry; the
    /// outbox pipeline is not started here.
    ///
    /// `timings` are the indexing wait timings of an upload (the production values unless a test
    /// shortens them).
    ///
    /// # Errors
    /// An error when the OAGW client is not registered in the `ClientHub`.
    pub fn build(
        config: MiniChatConfig,
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<dyn AuthZResolverApi>,
        hub: &Arc<ClientHub>,
        timings: IndexingTimings,
    ) -> anyhow::Result<Self> {
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|err| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {err}"))?;
        let pep = Pep::new(PolicyEnforcer::new(authz));
        let policy: Arc<dyn ModelPolicyGateway> = Arc::new(GtsModelPolicyGateway::new(
            Arc::clone(hub),
            config.vendor.clone(),
        ));
        let metrics: Arc<dyn Metrics> = Arc::new(OtelMetrics::new(
            &opentelemetry::global::meter("mini-chat"),
            &config.metrics.prefix,
        ));
        let outbox = Arc::new(OutboxEnqueuer::new(&config.outbox));
        let chat_repo = Arc::new(ChatRepositoryImpl::new(
            Arc::clone(&db),
            Arc::clone(&outbox),
        ));
        let turn_repo = Arc::new(TurnRepositoryImpl::new(
            Arc::clone(&db),
            Arc::clone(&outbox),
            Arc::clone(&metrics),
        ));
        let mutation_repo = Arc::new(MutationRepositoryImpl::new(
            Arc::clone(&db),
            Arc::clone(&outbox),
        ));
        let watchdog = Arc::new(OrphanWatchdog::new(OrphanWatchdogParts {
            candidates: turn_repo.clone(),
            turns: turn_repo.clone(),
            policy: Arc::clone(&policy),
            metrics: Arc::clone(&metrics),
            timeout: Duration::from_secs(config.orphan_watchdog.timeout_secs),
            scan_interval: Duration::from_secs(config.orphan_watchdog.scan_interval_secs),
        }));
        let conversation_repo = Arc::new(ConversationRepositoryImpl::new(Arc::clone(&db)));
        let message_repo = Arc::new(MessageRepositoryImpl::new(Arc::clone(&db)));
        let quota = Arc::new(QuotaService::new(
            Arc::new(QuotaRepositoryImpl::new(Arc::clone(&db))),
            config.quota.clone(),
        ));
        let finalizer = Arc::new(Finalizer::new(
            turn_repo.clone(),
            Arc::clone(&metrics),
            config.quota.overshoot_tolerance_factor,
        ));
        let deps = RunDeps {
            turns: turn_repo.clone(),
            quota: Arc::clone(&quota),
            finalizer,
            metrics: Arc::clone(&metrics),
            trace: Arc::new(OtelTraceIds),
            settings: RunSettings {
                web_search_max_calls: config.quota.web_search_max_calls_per_message,
                code_interpreter_max_calls: config.quota.code_interpreter_max_calls_per_message,
                progress_refresh: PROGRESS_REFRESH,
            },
        };
        let providers = Arc::new(ConfiguredProviders::new(
            Arc::clone(&gateway),
            config.providers.clone(),
        ));
        let stream = Arc::new(StreamService::new(StreamServiceParts {
            pep: pep.clone(),
            chats: chat_repo.clone(),
            turns: turn_repo.clone(),
            mutations: mutation_repo.clone(),
            conversation: conversation_repo,
            policy: Arc::clone(&policy),
            quota: Arc::clone(&quota),
            providers: providers.clone(),
            deps,
            config: StreamConfig::from_config(&config),
        }));
        let mutations = Arc::new(TurnMutationService::new(TurnMutationServiceParts {
            pep: pep.clone(),
            chats: chat_repo.clone(),
            turns: turn_repo.clone(),
            mutations: mutation_repo,
            stream: Arc::clone(&stream),
            metrics: Arc::clone(&metrics),
        }));
        let summary = Arc::new(SummaryService::new(SummaryServiceParts {
            repo: Arc::new(SummaryRepositoryImpl::new(
                Arc::clone(&db),
                Arc::clone(&outbox),
            )),
            policy: Arc::clone(&policy),
            providers,
            config: config.thread_summary_worker.clone(),
            metrics: Arc::clone(&metrics),
        }));
        let quota_status = Arc::new(QuotaStatusService::new(
            pep.clone(),
            Arc::clone(&policy),
            Arc::clone(&quota),
        ));
        let chats = Arc::new(ChatService::new(
            chat_repo.clone(),
            pep.clone(),
            Arc::clone(&policy),
        ));
        let models = Arc::new(ModelsService::new(pep.clone(), Arc::clone(&policy)));
        let messages = Arc::new(MessageService::new(
            pep.clone(),
            chat_repo.clone(),
            message_repo,
        ));
        let attachment_repo = Arc::new(AttachmentRepositoryImpl::new(
            Arc::clone(&db),
            Arc::clone(&outbox),
        ));
        let storage: Arc<dyn StorageGateway> = Arc::new(OagwStorage::new(
            Arc::clone(&gateway),
            config.providers.clone(),
        ));
        let attachments = Arc::new(AttachmentService::new(AttachmentServiceParts {
            pep: pep.clone(),
            chats: chat_repo.clone(),
            repo: attachment_repo.clone() as Arc<dyn AttachmentRepository>,
            policy: Arc::clone(&policy),
            storage: Arc::clone(&storage),
            metrics: Arc::clone(&metrics),
            rag: config.rag.clone(),
            thumbnail: config.thumbnail.clone(),
            timings,
        }));
        let cleanup_repo: Arc<dyn CleanupRepository> = attachment_repo.clone();
        let attachment_cleanup = Arc::new(AttachmentCleanup::new(
            Arc::clone(&cleanup_repo),
            Arc::clone(&storage),
            Arc::clone(&metrics),
            config.cleanup_worker.max_attempts,
        ));
        let chat_cleanup = Arc::new(ChatCleanup::new(
            cleanup_repo,
            Arc::clone(&storage),
            Arc::clone(&policy),
            Arc::clone(&metrics),
            config.cleanup_worker.max_attempts,
        ));
        let reaper_repo: Arc<dyn UploadReaperRepository> = attachment_repo;
        let upload_reaper = Arc::new(UploadReaper::new(
            reaper_repo,
            Arc::clone(&metrics),
            Duration::from_secs(config.upload_reaper.stale_after_secs),
            Duration::from_secs(config.upload_reaper.scan_interval_secs),
        ));
        let turns = Arc::new(TurnService::new(pep, chat_repo, turn_repo));
        let audit = Arc::new(AuditGateway::new(Arc::clone(hub), config.vendor.clone()));
        Ok(Self {
            config,
            db,
            chats,
            models,
            messages,
            turns,
            stream,
            mutations,
            summary,
            quota_status,
            policy,
            outbox,
            watchdog,
            attachments,
            attachment_cleanup,
            chat_cleanup,
            upload_reaper,
            audit,
            metrics,
            gateway,
            hub: Arc::clone(hub),
        })
    }
}
