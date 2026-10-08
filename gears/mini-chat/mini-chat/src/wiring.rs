//! Service construction. [`build_services`] is the single place where the service graph is
//! assembled; it is used by `gear.rs` (production clients) and by the test application
//! (in-memory fakes).

use std::sync::Arc;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use std::time::Duration;
use toolkit::client_hub::ClientHub;

use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::api::state::AppServices;
use crate::config::MiniChatConfig;
use crate::domain::attachment::{AttachmentDeps, AttachmentService, IndexingTimings};
use crate::domain::authz::Authz;
use crate::domain::chat_service::ChatService;
use crate::domain::message_service::MessageService;
use crate::domain::model_service::ModelService;
use crate::domain::quota::QuotaService;
use crate::domain::reaction_service::ReactionService;
use crate::domain::stream::{StreamDeps, StreamService};
use crate::domain::turn_service::TurnService;
use crate::infra::gateways::audit::AuditGateway;
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::llm::{LlmClient, ProviderKind, ProviderResolver, S2sContext};
use crate::infra::outbox::attachment_cleanup::{AttachmentCleanupHandler, CleanupDeps};
use crate::infra::outbox::audit::AuditHandler;
use crate::infra::outbox::chat_cleanup::ChatCleanupHandler;
use crate::infra::outbox::thread_summary::ThreadSummaryHandler;
use crate::infra::outbox::usage::UsageHandler;
use crate::infra::outbox::{OutboxEnqueuer, OutboxHandlers, QueueKind, start_outbox_tuned};
use crate::infra::storage::knowledge::{AzureKnowledgeRetriever, KnowledgeRetriever};
use crate::infra::storage::{AnthropicFiles, OpenAiStorage};
use crate::metrics::Metrics;

/// Inputs of [`build_services`]. Extended by later tasks.
pub struct ServiceDeps {
    /// Validated gear configuration.
    pub cfg: Arc<MiniChatConfig>,
    /// The gear's database (migrated).
    pub db: toolkit_db::Db,
    /// Client hub (S2S authn client and later lookups).
    pub hub: Arc<ClientHub>,
    /// PDP client behind the policy enforcement point.
    pub authz_client: Arc<dyn AuthZResolverApi>,
    /// OAGW client for provider calls.
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    pub policy: Arc<dyn PolicyGateway>,
    pub audit: Arc<dyn AuditGateway>,
    /// Waits and deadlines of document indexing (`IndexingTimings::default()` in production).
    pub indexing_timings: IndexingTimings,
    /// Instruments to record into; `None` registers them on the global meter (production).
    pub metrics: Option<Arc<Metrics>>,
}

/// Builds the service graph.
///
/// # Errors
/// Returns an error when a service cannot be constructed.
#[allow(clippy::unnecessary_wraps)] // later tasks add fallible construction steps
pub fn build_services(deps: ServiceDeps) -> anyhow::Result<Arc<AppServices>> {
    let ServiceDeps {
        cfg,
        db,
        hub: _,
        authz_client,
        gateway,
        policy,
        audit,
        indexing_timings,
        metrics,
    } = deps;
    let authz = Arc::new(Authz::new(PolicyEnforcer::new(authz_client)));
    let models = Arc::new(ModelService::new(Arc::clone(&policy), Arc::clone(&authz)));
    let metrics = metrics.unwrap_or_else(|| Arc::new(Metrics::new(&cfg.metrics.prefix)));
    let outbox = Arc::new(OutboxEnqueuer::new(&cfg.outbox));
    let s2s = S2sContext::new();
    let llm = Arc::new(LlmClient::new(Arc::clone(&gateway), s2s.clone()));
    let storage = Arc::new(OpenAiStorage::new(Arc::clone(&gateway), s2s.clone()));
    let providers = Arc::new(ProviderResolver::new(cfg.providers.clone()));
    let anthropic_files = cfg
        .providers
        .values()
        .any(|p| p.kind == ProviderKind::AnthropicMessages)
        .then(|| Arc::new(AnthropicFiles::new(Arc::clone(&gateway), s2s.clone())));
    let db = Arc::new(DBProvider::new(db));
    let knowledge: Option<Arc<dyn KnowledgeRetriever>> = cfg.knowledge_search.enabled.then(|| {
        Arc::new(AzureKnowledgeRetriever::new(
            Arc::clone(&gateway),
            s2s.clone(),
            cfg.knowledge_search.max_chunk_chars,
        )) as _
    });
    let chats = Arc::new(ChatService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&models),
        Arc::clone(&outbox),
    ));
    let quota = Arc::new(QuotaService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&policy),
        cfg.quota.clone(),
        Arc::clone(&metrics),
    ));
    let stream = Arc::new(StreamService::new(StreamDeps {
        cfg: Arc::clone(&cfg),
        db: Arc::clone(&db),
        authz: Arc::clone(&authz),
        models: Arc::clone(&models),
        policy: Arc::clone(&policy),
        quota: Arc::clone(&quota),
        providers: Arc::clone(&providers),
        llm: Arc::clone(&llm),
        outbox: Arc::clone(&outbox),
        metrics: Arc::clone(&metrics),
        knowledge: knowledge.clone(),
    }));
    let messages = Arc::new(MessageService::new(Arc::clone(&db), Arc::clone(&authz)));
    let reactions = Arc::new(ReactionService::new(Arc::clone(&db), Arc::clone(&authz)));
    let turns = Arc::new(TurnService::new(
        Arc::clone(&db),
        Arc::clone(&authz),
        Arc::clone(&stream),
        Arc::clone(&outbox),
        Arc::clone(&metrics),
    ));
    let attachments = Arc::new(
        AttachmentService::new(AttachmentDeps {
            cfg: Arc::clone(&cfg),
            db: Arc::clone(&db),
            authz: Arc::clone(&authz),
            models: Arc::clone(&models),
            providers: Arc::clone(&providers),
            files: Arc::clone(&storage) as _,
            vector_stores: Arc::clone(&storage) as _,
            outbox: Arc::clone(&outbox),
            metrics: Arc::clone(&metrics),
            anthropic_files: anthropic_files.clone(),
        })
        .with_timings(indexing_timings),
    );
    Ok(Arc::new(AppServices {
        cfg,
        db,
        authz,
        models,
        chats,
        metrics,
        policy,
        audit,
        gateway,
        s2s,
        providers,
        llm,
        files: Arc::clone(&storage) as _,
        vector_stores: storage,
        anthropic_files,
        outbox,
        quota,
        stream,
        messages,
        reactions,
        turns,
        attachments,
    }))
}

/// Handlers of the five outbox queues.
#[must_use]
pub fn default_outbox_handlers(services: &AppServices) -> OutboxHandlers {
    let cleanup = CleanupDeps::from_services(services);
    OutboxHandlers::logging()
        .with(
            QueueKind::Usage,
            Arc::new(UsageHandler::new(Arc::clone(&services.policy))),
        )
        .with(
            QueueKind::Audit,
            Arc::new(AuditHandler::new(
                Arc::clone(&services.audit),
                Arc::clone(&services.metrics),
            )),
        )
        .with(
            QueueKind::AttachmentCleanup,
            Arc::new(AttachmentCleanupHandler::new(cleanup.clone())),
        )
        .with(
            QueueKind::ChatCleanup,
            Arc::new(ChatCleanupHandler::new(cleanup)),
        )
        .with(
            QueueKind::ThreadSummary,
            Arc::new(ThreadSummaryHandler::from_services(services)),
        )
}

/// Starts the outbox pipeline and connects `services.outbox` to it. The caller owns the handle
/// and stops it on shutdown. `idle_interval` overrides the workers' idle polling (tests).
///
/// # Errors
/// Returns an error when the platform outbox cannot start.
pub async fn start_outbox_pipeline(
    services: &AppServices,
    db: toolkit_db::Db,
    handlers: OutboxHandlers,
    idle_interval: Option<Duration>,
) -> anyhow::Result<OutboxHandle> {
    let handle = start_outbox_tuned(db, &services.cfg, handlers, idle_interval)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start the outbox pipeline: {e}"))?;
    services.outbox.attach(Arc::clone(handle.outbox()));
    Ok(handle)
}
