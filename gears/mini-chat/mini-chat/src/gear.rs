//! The `mini-chat` gear: wiring, lifecycle and capabilities.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use time::OffsetDateTime;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;

use crate::config::MiniChatConfig;
use crate::domain::authz::PolicyEnforcerAuthz;
use crate::domain::background::Background;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuditSink, AuthzPort, LlmClient, PolicyProvider, RagStorage};
use crate::domain::services::attachment_service::{AttachmentDeps, AttachmentService};
use crate::domain::services::chat_service::ChatService;
use crate::domain::services::finalization_service::FinalizationService;
use crate::domain::services::message_service::MessageService;
use crate::domain::services::model_service::ModelService;
use crate::domain::services::quota_service::QuotaService;
use crate::domain::services::reaction_service::ReactionService;
use crate::domain::services::stream::{StreamDeps, StreamService};
use crate::domain::services::thread_summary_service::{ThreadSummaryDeps, ThreadSummaryService};
use crate::domain::services::turn_service::TurnService;
use crate::infra::llm::knowledge::{AzureKnowledgeRetriever, KnowledgeRetriever};
use crate::infra::llm::providers::OagwLlmClient;
use crate::infra::llm::storage::OagwRagStorage;
use crate::infra::llm::{ProviderResolver, ServiceIdentity};
use crate::infra::oagw_provisioning::{OagwProvisioner, ProvisioningHealth, spawn_reconcile};
use crate::infra::outbox::{
    AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, CleanupDeps, DeferredHandler,
    MiniChatOutbox, OutboxHandlers, ThreadSummaryHandler, UsageHandler,
};
use crate::infra::plugins::audit_gateway::AuditGateway;
use crate::infra::plugins::policy_gateway::PolicyGateway;
use crate::infra::workers::{
    LeaderElector, OrphanWatchdog, ROLE_ORPHAN_WATCHDOG, ROLE_UPLOAD_REAPER, UploadReaper,
    build_elector, shutdown_workers, spawn_worker,
};

/// Total time `serve` waits, after cancellation, for the workers and the
/// request-spawned background tasks (concurrently), leaving room for
/// `outbox.stop()` inside the 30 s lifecycle `stop_timeout`.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(25);

/// Shared state handed to every REST handler (`Extension(Arc<AppState>)`).
pub struct AppState {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<dyn AuthzPort>,
    pub policy: Arc<dyn PolicyProvider>,
    pub audit: Arc<dyn AuditSink>,
    pub outbox: Arc<MiniChatOutbox>,
    pub models: Arc<ModelService>,
    pub chats: Arc<ChatService>,
    pub messages: Arc<MessageService>,
    pub reactions: Arc<ReactionService>,
    pub turns: Arc<TurnService>,
    pub quota: Arc<QuotaService>,
    /// Turn finalization (CAS, settlement, usage/audit/summary outbox).
    pub finalization: Arc<FinalizationService>,
    /// Send pipeline of `messages:stream` (provider task, SSE events).
    pub stream: Arc<StreamService>,
    /// Attachment upload / get / delete and background indexing.
    pub attachments: Arc<AttachmentService>,
    /// Provider calls through OAGW with the gear's S2S identity.
    pub llm: Arc<dyn LlmClient>,
    /// Files and vector stores through OAGW with the gear's S2S identity.
    pub storage: Arc<dyn RagStorage>,
    /// Request-spawned background tasks (attachment indexing); cancelled and
    /// awaited on stop.
    pub background: Background,
    pub authn: Arc<dyn AuthNResolverClient>,
    pub oagw: Arc<dyn ServiceGatewayClientV1>,
    /// The gear's S2S context for every OAGW call (set at start).
    pub service_identity: Arc<ServiceIdentity>,
    /// `provider_id` + tenant -> provider entry and OAGW alias.
    pub resolver: Arc<ProviderResolver>,
    pub provisioner: Arc<OagwProvisioner>,
    /// Readiness of the start-phase provisioning (gear healthcheck).
    pub provisioning_health: Arc<ProvisioningHealth>,
}

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
#[derive(Default)]
pub struct MiniChatGear {
    state: OnceLock<Arc<AppState>>,
}

impl MiniChatGear {
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(state) = self.state.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        // Background workers (OAGW reconcile, leader election, orphan
        // watchdog, upload reaper) observe a child token.
        let mut workers: JoinSet<()> = JoinSet::new();
        if let Err(e) = Self::start_phase(&state, &mut workers, cancel.child_token()).await {
            state.outbox.stop().await;
            return Err(e);
        }
        ready.notify();
        cancel.cancelled().await;

        shutdown_workers(&mut workers, &state.background, SHUTDOWN_BUDGET).await;
        state.outbox.stop().await;
        tracing::info!("mini-chat stopped");
        Ok(())
    }

    /// Start phase (not init: plugin resolution through types-registry only
    /// works after the system gears' `post_init`): leader elector, S2S
    /// exchange, provisioning, reconcile task for deferred providers, then
    /// the leader-only workers (DESIGN section 3.2 "Gear lifecycle"; the
    /// outbox pipeline already runs since init). A failure leaves the gear's
    /// healthcheck permanently unhealthy; the caller ends the gear task.
    async fn start_phase(
        state: &AppState,
        workers: &mut JoinSet<()>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let elector = match build_elector(&leader_roles(&state.cfg), workers, &cancel).await {
            Ok(elector) => elector,
            Err(e) => {
                tracing::error!(error = %e, "mini-chat: startup failed");
                state.provisioning_health.mark_failed();
                return Err(e);
            }
        };
        let pending = match provision_providers(
            &state.cfg,
            state.authn.as_ref(),
            &state.provisioner,
            &state.service_identity,
        )
        .await
        {
            Ok(pending) => pending,
            Err(e) => {
                tracing::error!(error = %e, "mini-chat: startup failed");
                state.provisioning_health.mark_failed();
                return Err(e);
            }
        };
        state.provisioning_health.mark_ready();
        check_summary_model(state).await;
        if let Ok(ctx) = state.service_identity.get().await {
            spawn_reconcile(
                workers,
                Arc::clone(&state.provisioner),
                ctx,
                pending,
                cancel.clone(),
            );
        }
        spawn_leader_workers(state, &elector, workers, &cancel);
        Ok(())
    }
}

/// Roles of the enabled leader-only workers.
fn leader_roles(cfg: &MiniChatConfig) -> Vec<&'static str> {
    let mut roles = Vec::new();
    if cfg.orphan_watchdog.enabled {
        roles.push(ROLE_ORPHAN_WATCHDOG);
    }
    if cfg.upload_reaper.enabled {
        roles.push(ROLE_UPLOAD_REAPER);
    }
    roles
}

/// Spawns the enabled orphan watchdog and upload reaper scan loops.
fn spawn_leader_workers(
    state: &AppState,
    elector: &Arc<dyn LeaderElector>,
    workers: &mut JoinSet<()>,
    cancel: &CancellationToken,
) {
    let w = &state.cfg.orphan_watchdog;
    if w.enabled {
        let watchdog = Arc::new(OrphanWatchdog::new(
            Arc::clone(&state.db),
            Arc::clone(&state.finalization),
            Arc::clone(elector),
            Duration::from_secs(w.timeout_secs),
        ));
        spawn_worker(
            workers,
            "orphan_watchdog",
            Duration::from_secs(w.scan_interval_secs),
            cancel.clone(),
            move || {
                let watchdog = Arc::clone(&watchdog);
                async move { watchdog.scan_once(OffsetDateTime::now_utc()).await }
            },
        );
    }
    let r = &state.cfg.upload_reaper;
    if r.enabled {
        let reaper = Arc::new(UploadReaper::new(
            Arc::clone(&state.db),
            Arc::clone(&state.outbox),
            Arc::clone(elector),
            Duration::from_secs(r.stale_after_secs),
        ));
        spawn_worker(
            workers,
            "upload_reaper",
            Duration::from_secs(r.scan_interval_secs),
            cancel.clone(),
            move || {
                let reaper = Arc::clone(&reaper);
                async move { reaper.scan_once(OffsetDateTime::now_utc()).await }
            },
        );
    }
}

/// DESIGN section 3.6: with summaries enabled, a missing or disabled summary
/// model is logged as an error at start; startup continues (a dynamic policy
/// plugin can add it later, and each job rejects meanwhile).
async fn check_summary_model(state: &AppState) {
    let cfg = &state.cfg.thread_summary_worker;
    if !cfg.enabled {
        return;
    }
    let model_id = cfg.effective_summary_model_id();
    match state
        .models
        .resolve_for_chat(DEFAULT_SUBJECT_ID, model_id, true)
        .await
    {
        Ok(_) => {}
        Err(DomainError::InvalidModel) => tracing::error!(
            model = model_id,
            "mini-chat: thread summary model is missing from the catalog or disabled"
        ),
        Err(e) => tracing::error!(
            model = model_id,
            error = %e,
            "mini-chat: thread summary model could not be checked"
        ),
    }
}

/// S2S client-credentials exchange, then OAGW provisioning (DESIGN §3.2).
/// Returns the deferred targets.
async fn provision_providers(
    cfg: &MiniChatConfig,
    authn: &dyn AuthNResolverClient,
    provisioner: &OagwProvisioner,
    identity: &ServiceIdentity,
) -> anyhow::Result<Vec<String>> {
    let creds = &cfg.client_credentials;
    let request = ClientCredentialsRequest {
        client_id: creds.client_id.clone(),
        client_secret: creds.client_secret.clone(),
        scopes: vec![],
    };
    let ctx = authn
        .exchange_client_credentials(&request)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "mini-chat: S2S client-credentials exchange for client_id '{}' failed: {e}",
                creds.client_id
            )
        })?
        .security_context;
    identity.set(ctx.clone());

    let report = provisioner
        .provision_all(&ctx)
        .await
        .map_err(|e| anyhow::anyhow!("mini-chat: {e}"))?;
    if report.pending.is_empty() {
        tracing::info!("mini-chat: OAGW provisioning complete");
    } else {
        tracing::info!(
            pending = ?report.pending,
            "mini-chat: OAGW provisioning deferred for some providers; reconciling in background"
        );
    }
    Ok(report.pending)
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(gear = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.fill_upstream_aliases();
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for warning in cfg.deprecated_warnings() {
            tracing::warn!("mini-chat config: {warning}");
        }
        let cfg = Arc::new(cfg);

        let raw_db = ctx.db_required()?;
        let db = Arc::new(DBProvider::<DomainError>::new(raw_db.db()));

        let hub = ctx.client_hub();
        let authz_api = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;

        let resolver = Arc::new(ProviderResolver::new(&cfg));
        let provisioner = Arc::new(OagwProvisioner::new(Arc::clone(&oagw), &cfg));
        let service_identity = Arc::new(ServiceIdentity::default());
        let provisioning_health = Arc::new(ProvisioningHealth::default());

        let authz: Arc<dyn AuthzPort> =
            Arc::new(PolicyEnforcerAuthz::new(PolicyEnforcer::new(authz_api)));
        let policy: Arc<dyn PolicyProvider> =
            Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit: Arc<dyn AuditSink> =
            Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));

        let storage: Arc<dyn RagStorage> = Arc::new(OagwRagStorage::new(
            Arc::clone(&oagw),
            Arc::clone(&service_identity),
        ));

        // Ruling R1: the outbox pipeline starts in init so nothing can be
        // enqueued before the queues exist. The thread-summary worker
        // enqueues through the outbox, so its handler is installed once the
        // outbox has started.
        let cleanup = CleanupDeps {
            db: Arc::clone(&db),
            resolver: Arc::clone(&resolver),
            storage: Arc::clone(&storage),
            max_attempts: cfg.cleanup_worker.max_attempts,
        };
        let thread_summary_slot = Arc::new(DeferredHandler::default());
        let handlers = OutboxHandlers {
            usage: Arc::new(UsageHandler::new(Arc::clone(&policy))),
            audit: Arc::new(AuditHandler::new(Arc::clone(&audit))),
            attachment_cleanup: Arc::new(AttachmentCleanupHandler::new(cleanup.clone())),
            chat_cleanup: Arc::new(ChatCleanupHandler::new(cleanup)),
            thread_summary: thread_summary_slot.clone(),
        };
        let outbox = Arc::new(
            MiniChatOutbox::start(
                db.db(),
                &cfg.outbox,
                Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs),
                handlers,
            )
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat outbox failed to start: {e}"))?,
        );

        let models = Arc::new(ModelService::new(Arc::clone(&authz), Arc::clone(&policy)));
        let chats = Arc::new(ChatService::new(
            Arc::clone(&db),
            Arc::clone(&authz),
            Arc::clone(&models),
            Arc::clone(&outbox),
        ));
        let messages = Arc::new(MessageService::new(Arc::clone(&db), Arc::clone(&authz)));
        let reactions = Arc::new(ReactionService::new(Arc::clone(&db), Arc::clone(&authz)));
        let turns = Arc::new(TurnService::new(
            Arc::clone(&db),
            Arc::clone(&authz),
            Arc::clone(&outbox),
        ));
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&db),
            Arc::clone(&authz),
            Arc::clone(&policy),
            Arc::clone(&cfg),
        ));
        let finalization = Arc::new(FinalizationService::new(
            Arc::clone(&db),
            Arc::clone(&policy),
            Arc::clone(&quota),
            Arc::clone(&outbox),
        ));

        let llm: Arc<dyn LlmClient> = Arc::new(OagwLlmClient::new(
            Arc::clone(&oagw),
            Arc::clone(&service_identity),
        ));
        let summaries = Arc::new(ThreadSummaryService::new(ThreadSummaryDeps {
            db: Arc::clone(&db),
            cfg: Arc::clone(&cfg),
            models: Arc::clone(&models),
            llm: Arc::clone(&llm),
            resolver: Arc::clone(&resolver),
            outbox: Arc::clone(&outbox),
        }));
        thread_summary_slot.install(Arc::new(ThreadSummaryHandler::new(summaries)));
        // Knowledge retriever: created only when the feature is enabled.
        let knowledge: Option<Arc<dyn KnowledgeRetriever>> =
            cfg.knowledge_search.enabled.then(|| {
                Arc::new(AzureKnowledgeRetriever::new(
                    Arc::clone(&oagw),
                    Arc::clone(&service_identity),
                )) as Arc<dyn KnowledgeRetriever>
            });
        let stream = Arc::new(StreamService::new(StreamDeps {
            db: Arc::clone(&db),
            cfg: Arc::clone(&cfg),
            authz: Arc::clone(&authz),
            models: Arc::clone(&models),
            quota: Arc::clone(&quota),
            finalization: Arc::clone(&finalization),
            llm: Arc::clone(&llm),
            resolver: Arc::clone(&resolver),
            turns: Arc::clone(&turns),
            knowledge,
        }));

        let background = Background::default();
        let attachments = Arc::new(AttachmentService::new(AttachmentDeps {
            db: Arc::clone(&db),
            cfg: Arc::clone(&cfg),
            authz: Arc::clone(&authz),
            models: Arc::clone(&models),
            resolver: Arc::clone(&resolver),
            storage: Arc::clone(&storage),
            outbox: Arc::clone(&outbox),
            background: background.clone(),
        }));

        let state = Arc::new(AppState {
            cfg,
            db,
            authz,
            policy,
            audit,
            outbox,
            models,
            chats,
            messages,
            reactions,
            turns,
            quota,
            finalization,
            stream,
            attachments,
            llm,
            storage,
            background,
            authn,
            oagw,
            service_identity,
            resolver,
            provisioner,
            provisioning_health,
        });
        if let Err(state) = self.state.set(state) {
            state.outbox.stop().await;
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }
        tracing::info!("mini-chat initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        crate::infra::db::all_migrations()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let local = crate::api::rest::routes::register_routes(openapi, state);
        Ok(router.merge(local))
    }

    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn toolkit::Healthcheck>> {
        let state = self.state.get()?;
        let health: Arc<dyn toolkit::Healthcheck> = state.provisioning_health.clone();
        Some(health)
    }
}
