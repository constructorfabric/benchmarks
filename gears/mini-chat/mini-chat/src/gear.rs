//! `ToolKit` gear declaration, lifecycle and wiring.
//!
//! Lifecycle (state machine `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`):
//! `CREATED -> INITIALIZED` (init loaded and validated the config, resolved the clients and
//! built the services), `CREATED -> FAILED` (invalid config or missing client),
//! `INITIALIZED -> STARTED` (start finished and the outbox pipeline is running),
//! `INITIALIZED -> FAILED`, `STARTED -> STOPPING -> STOPPED`.
//!
//! Migrations run before `init`; REST registration runs before `start`, so the services are
//! kept from `init` and the outbox is bound late.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use oagw_sdk::api::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::var_expand::ExpandVars;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::outbox::{LeasedMessageHandler, OutboxHandle};
use toolkit_db::{DBProvider, Db};
use uuid::Uuid;

use crate::api::rest::register_routes;
use crate::config::MiniChatConfig;
use crate::domain::attachments::cleanup::{AttachmentCleanupWorker, CleanupDeps};
use crate::domain::attachments::completion::UploadPermits;
use crate::domain::attachments::contributions::{AttachmentToolSettings, AttachmentToolSource};
use crate::domain::attachments::facts::AttachmentFactsStore;
use crate::domain::attachments::knowledge::{KnowledgeLoopDeps, KnowledgeLoopSettings};
use crate::domain::attachments::provider::StorageProvider;
use crate::domain::attachments::store::{AttachmentCleanupQueue, AttachmentStore};
use crate::domain::attachments::timing::TokioTimer;
use crate::domain::attachments::vector_store::ChatVectorStoreRepo;
use crate::domain::attachments::{
    AttachmentMetrics, AttachmentSettings, AttachmentsDeps, AttachmentsService,
};
use crate::domain::authz::PepService;
use crate::domain::chats::{ChatCleanupQueue, ChatStore, ChatsService};
use crate::domain::maintenance::chat_cleanup::{ChatCleanupDeps, ChatCleanupWorker};
use crate::domain::maintenance::metrics::{CleanupMetrics, SummaryMetrics};
use crate::domain::maintenance::secondary::has_anthropic_provider;
use crate::domain::maintenance::settings::{CleanupSettings, SummaryWorkerSettings};
use crate::domain::maintenance::summary_worker::{SummaryDeps, ThreadSummaryWorker};
use crate::domain::messages::MessagesService;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::mutations::service::MutationDeps;
use crate::domain::mutations::{
    MutationMetrics, MutationStore, ReactionService, ReactionStore, TurnMutationService,
};
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::{
    QuotaDeps, QuotaMetrics, QuotaService, QuotaSettings, UsagePublisher, UsageQueue,
};
use crate::domain::services::Services;
use crate::domain::streaming::{
    MessageStreamService, StreamSettings, StreamStore, TurnFinalizer, TurnOutbox, TurnProgress,
};
use crate::domain::summary_model::{SummaryModelStatus, summary_model_status};
use crate::infra::attachment_cleanup::AttachmentCleanupHandler;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::chat_cleanup_handler::ChatCleanupHandler;
use crate::infra::clock::SystemClock;
use crate::infra::leader::{LeaderElector, NoopLeaderElector, prepare_elector};
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::oagw::knowledge_client::OagwKnowledgeRetriever;
use crate::infra::oagw::provisioning::first_pass;
use crate::infra::oagw::reconcile::run_reconcile;
use crate::infra::oagw::storage_client::OagwStorageClient;
use crate::infra::oagw::summary_client::OagwSummaryClient;
use crate::infra::orphan_watchdog::{OrphanWatchdog, WatchdogSettings};
use crate::infra::outbox::{
    OutboxEnqueuer, QueueHandlers, queue_specs, start_pipeline_with_handlers,
};
use crate::infra::policy_gateway::GtsModelPolicyGateway;
use crate::infra::provider_task::{
    ProviderTaskDeps, ProviderTaskSettings, S2sContext, StreamingProviderTask,
};
use crate::infra::storage::migrations::all_migrations;
use crate::infra::storage::quota_ledger::SqlQuotaLedger;
use crate::infra::tasks::BackgroundTasks;
use crate::infra::thread_summary_handler::ThreadSummaryHandler;
use crate::infra::turn_finalizer::{SqlTurnFinalizer, StreamFinalizationDeps, SummarySettings};
use crate::infra::upload_reaper::{ReaperDeps, ReaperSettings, UploadReaper};
use crate::infra::usage_handler::UsageOutboxHandler;

/// How long `stop` waits for the background tasks before it reports and aborts them.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The summary model the thread-summary worker uses when `summary_model_id` is empty.
const DEFAULT_SUMMARY_MODEL: &str = "gpt-4.1-mini";

/// State of the gear lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Created,
    Initialized,
    Started,
    Stopping,
    Stopped,
    Failed,
}

/// The platform clients resolved at init. `authn_resolver` and `oagw` serve the provider
/// provisioning of `start`.
#[derive(Clone)]
pub struct PlatformClients {
    pub authz: Arc<dyn AuthZResolverApi>,
    pub authn: Arc<dyn AuthNResolverClient>,
    pub oagw: Arc<dyn ServiceGatewayClientV1>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    state: Mutex<LifecycleState>,
    config: OnceLock<Arc<MiniChatConfig>>,
    db: OnceLock<Db>,
    clients: OnceLock<PlatformClients>,
    services: OnceLock<Arc<Services>>,
    enqueuer: OnceLock<Arc<OutboxEnqueuer>>,
    outbox: tokio::sync::Mutex<Option<OutboxHandle>>,
    policy: OnceLock<Arc<dyn ModelPolicyGateway>>,
    usage_publisher: OnceLock<Arc<dyn UsagePublisher>>,
    // slice S2: set by `init` (registry, metrics, audit gateway) and `start` (tasks, elector)
    providers: OnceLock<Arc<ProviderRegistry>>,
    metrics: OnceLock<Arc<MiniChatMetrics>>,
    audit_gateway: OnceLock<Arc<AuditGateway>>,
    tasks: OnceLock<Arc<BackgroundTasks>>,
    elector: OnceLock<Arc<dyn LeaderElector>>,
    // slice S2: the S2S context the provider task runs under (bound at start) and the quota
    // service the orphan watchdog settles through
    s2s: OnceLock<Arc<S2sContext>>,
    quota: OnceLock<Arc<QuotaService>>,
    // attachments and file search: the token that stops the background indexing tasks (a child of
    // the gear cancellation token, cancelled first by `stop`) and the service that owns the tasks
    uploads_shutdown: OnceLock<CancellationToken>,
    attachments: OnceLock<Arc<AttachmentsService>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: Mutex::new(LifecycleState::Created),
            config: OnceLock::new(),
            db: OnceLock::new(),
            clients: OnceLock::new(),
            services: OnceLock::new(),
            enqueuer: OnceLock::new(),
            outbox: tokio::sync::Mutex::new(None),
            policy: OnceLock::new(),
            usage_publisher: OnceLock::new(),
            providers: OnceLock::new(),
            metrics: OnceLock::new(),
            audit_gateway: OnceLock::new(),
            tasks: OnceLock::new(),
            elector: OnceLock::new(),
            s2s: OnceLock::new(),
            quota: OnceLock::new(),
            uploads_shutdown: OnceLock::new(),
            attachments: OnceLock::new(),
        }
    }
}

impl MiniChatGear {
    /// The current lifecycle state.
    ///
    /// # Panics
    ///
    /// Panics if the state lock is poisoned.
    #[must_use]
    #[allow(clippy::expect_used, reason = "a poisoned state lock is unrecoverable")]
    pub fn lifecycle_state(&self) -> LifecycleState {
        *self.state.lock().expect("lifecycle state lock")
    }

    fn set_state(&self, state: LifecycleState) {
        match self.state.lock() {
            Ok(mut current) => *current = state,
            Err(poisoned) => *poisoned.into_inner() = state,
        }
    }

    /// The attachment cleanup handler of `mini-chat.attachment_cleanup` (FEATURE 0005): the
    /// database store, the OAGW storage client under the S2S context, the registry and
    /// `cleanup_worker.max_attempts`.
    fn cleanup_handler(
        &self,
        config: &MiniChatConfig,
        db: &Db,
    ) -> Option<Arc<dyn LeasedMessageHandler>> {
        let (Some(clients), Some(s2s), Some(providers), Some(metrics)) = (
            self.clients.get(),
            self.s2s.get(),
            self.providers.get(),
            self.metrics.get(),
        ) else {
            tracing::error!(
                "the attachment cleanup handler cannot start: the gear is not initialized"
            );
            return None;
        };
        let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
            Arc::clone(&clients.oagw),
            Arc::clone(s2s),
        ));
        // the retry budget is `cleanup_worker.max_attempts`; the queue is
        // `outbox.cleanup_queue_name`; the other `cleanup_worker` keys have no effect
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-workers
        let worker = AttachmentCleanupWorker::new(CleanupDeps {
            store: Arc::new(DBProvider::new(db.clone())),
            storage: Arc::clone(&client),
            secondary_storage: Some(client),
            providers: Arc::clone(providers),
            clock: Arc::new(SystemClock),
            metrics: Arc::clone(metrics) as Arc<dyn AttachmentMetrics>,
            max_attempts: config.cleanup_worker.max_attempts,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-workers
        Some(Arc::new(AttachmentCleanupHandler::new(Arc::new(worker))))
    }

    /// The thread-summary handler of `mini-chat.thread_summary` (FEATURE 0007): the database
    /// store, the OAGW summary client under the S2S context, the registry, the policy gateway,
    /// the usage enqueuer and `thread_summary_worker`.
    fn summary_handler(
        &self,
        config: &MiniChatConfig,
        db: &Db,
    ) -> Option<Arc<dyn LeasedMessageHandler>> {
        let (
            Some(clients),
            Some(s2s),
            Some(providers),
            Some(metrics),
            Some(policy),
            Some(enqueuer),
        ) = (
            self.clients.get(),
            self.s2s.get(),
            self.providers.get(),
            self.metrics.get(),
            self.policy.get(),
            self.enqueuer.get(),
        )
        else {
            tracing::error!("the thread summary handler cannot start: the gear is not initialized");
            return None;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-build-summary
        // the handler is built whether or not `thread_summary_worker.enabled` is set: the flag
        // gates only the trigger of the message streaming feature, so work enqueued earlier still
        // runs; the deprecated keys change nothing (ADR-0010)
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-enabled-flag
        let worker = ThreadSummaryWorker::new(SummaryDeps {
            store: Arc::new(DBProvider::new(db.clone())),
            policy: Arc::clone(policy),
            providers: Arc::clone(providers),
            transport: Arc::new(OagwSummaryClient::new(
                Arc::clone(&clients.oagw),
                Arc::clone(s2s),
            )),
            usage: Arc::clone(enqueuer) as Arc<dyn UsageQueue>,
            clock: Arc::new(SystemClock),
            timer: Arc::new(TokioTimer),
            metrics: Arc::clone(metrics) as Arc<dyn SummaryMetrics>,
            settings: SummaryWorkerSettings::from_config(config),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-enabled-flag
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-build-summary
        Some(Arc::new(ThreadSummaryHandler::new(Arc::new(worker))))
    }

    /// The chat-cleanup handler of `mini-chat.chat_cleanup` (FEATURE 0007).
    fn chat_cleanup_handler(
        &self,
        config: &MiniChatConfig,
        db: &Db,
    ) -> Option<Arc<dyn LeasedMessageHandler>> {
        let (Some(clients), Some(s2s), Some(providers), Some(metrics), Some(policy)) = (
            self.clients.get(),
            self.s2s.get(),
            self.providers.get(),
            self.metrics.get(),
            self.policy.get(),
        ) else {
            tracing::error!("the chat cleanup handler cannot start: the gear is not initialized");
            return None;
        };
        let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
            Arc::clone(&clients.oagw),
            Arc::clone(s2s),
        ));
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-build-cleanup
        // the Anthropic Files client exists only when an `anthropic_messages` provider does; the
        // delivery timer is the injectable monotonic timer
        let secondary_storage = has_anthropic_provider(providers).then(|| Arc::clone(&client));
        let worker = ChatCleanupWorker::new(ChatCleanupDeps {
            store: Arc::new(DBProvider::new(db.clone())),
            storage: client,
            secondary_storage,
            providers: Arc::clone(providers),
            policy: Arc::clone(policy),
            clock: Arc::new(SystemClock),
            timer: Arc::new(TokioTimer),
            metrics: Arc::clone(metrics) as Arc<dyn CleanupMetrics>,
            settings: CleanupSettings::from_config(config),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-build-cleanup
        Some(Arc::new(ChatCleanupHandler::new(Arc::new(worker))))
    }

    /// The upload reaper (FEATURE 0005), spawned on the leader gate when enabled.
    fn spawn_upload_reaper(&self, config: &MiniChatConfig, tasks: &BackgroundTasks) {
        if !config.upload_reaper.enabled {
            return;
        }
        let (Some(db), Some(enqueuer), Some(metrics)) =
            (self.db.get(), self.enqueuer.get(), self.metrics.get())
        else {
            tracing::error!("the upload reaper cannot start: the gear is not initialized");
            return;
        };
        let elector = self
            .elector
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(NoopLeaderElector) as Arc<dyn LeaderElector>);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-workers
        // `upload_reaper.enabled`, `scan_interval_secs` and `stale_after_secs`
        let reaper = Arc::new(UploadReaper::new(ReaperDeps {
            store: Arc::new(DBProvider::new(db.clone())),
            cleanup: Arc::clone(enqueuer) as Arc<dyn AttachmentCleanupQueue>,
            elector,
            clock: Arc::new(SystemClock),
            timer: Arc::new(TokioTimer),
            metrics: Arc::clone(metrics) as Arc<dyn AttachmentMetrics>,
            settings: ReaperSettings::from_config(config),
        }));
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-workers
        tasks.spawn("upload-reaper", move |token| reaper.run(token));
    }

    /// The outbox part of `start`: register the five leased queues and start the pipeline,
    /// then bind the enqueuer. Callable after `init`, independently of provisioning.
    ///
    /// # Errors
    ///
    /// Any failure to start the pipeline, or a call before `init`.
    pub async fn start_outbox(&self) -> anyhow::Result<()> {
        let (Some(config), Some(db), Some(enqueuer)) =
            (self.config.get(), self.db.get(), self.enqueuer.get())
        else {
            anyhow::bail!("the outbox cannot start before the gear is initialized");
        };
        let mut handle = self.outbox.lock().await;
        if handle.is_some() {
            anyhow::bail!("the outbox pipeline is already started");
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-outbox
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-outbox
        // the usage queue name `outbox.queue_name` and `outbox.num_partitions` come from the
        // outbox section
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-outbox
        // `outbox.thread_summary_queue_name`, `outbox.chat_cleanup_queue_name`,
        // `outbox.queue_name` and `outbox.num_partitions` reach the handlers through these queue
        // registrations
        let specs = queue_specs(
            &config.outbox,
            config.thread_summary_worker.claim_timeout_secs,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-outbox
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-outbox
        // the audit queue is served by the audit gateway and the usage queue by the usage
        // handler of quota and billing, which publishes through the model-policy gateway
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-registration
        let handlers = QueueHandlers {
            audit: self
                .audit_gateway
                .get()
                .map(|gateway| Arc::clone(gateway) as Arc<dyn LeasedMessageHandler>),
            usage: self.usage_publisher.get().map(|publisher| {
                Arc::new(UsageOutboxHandler::new(Arc::clone(publisher)))
                    as Arc<dyn LeasedMessageHandler>
            }),
            attachment_cleanup: self.cleanup_handler(config, db),
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-return
            thread_summary: self.summary_handler(config, db),
            chat_cleanup: self.chat_cleanup_handler(config, db),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-return
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-registration
        let started = start_pipeline_with_handlers(db.clone(), &specs, &handlers)
            .await
            .context("cannot start the outbox pipeline")?;
        enqueuer.bind(started.outbox());
        *handle = Some(started);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-outbox
        Ok(())
    }

    /// The outbox part of `stop`: stop the pipeline, or give up when the framework deadline
    /// fires. Workers are joined before this is called.
    pub async fn stop_outbox(&self, deadline: &CancellationToken) {
        let Some(handle) = self.outbox.lock().await.take() else {
            return;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-outbox
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-stop
        // the foundation's gear stop stops the outbox pipeline; a delivery it interrupts is
        // redelivered after its lease and re-runs idempotently
        tokio::select! {
            () = handle.stop() => tracing::info!("mini-chat outbox pipeline stopped"),
            () = deadline.cancelled() => {
                tracing::warn!("framework deadline fired; giving up on the outbox pipeline");
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-stop
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-outbox
    }

    /// `true` while the outbox pipeline is running (between `start` and `stop`).
    pub async fn outbox_running(&self) -> bool {
        self.outbox.lock().await.is_some()
    }

    /// The provider registry built by `init`.
    #[must_use]
    pub fn provider_registry(&self) -> Option<&Arc<ProviderRegistry>> {
        self.providers.get()
    }

    /// The metrics instruments built by `init`.
    #[must_use]
    pub fn metrics(&self) -> Option<&Arc<MiniChatMetrics>> {
        self.metrics.get()
    }

    /// The audit gateway built by `init`.
    #[must_use]
    pub fn audit_gateway(&self) -> Option<&Arc<AuditGateway>> {
        self.audit_gateway.get()
    }

    /// The background task set created by `start` (the reconcile task and the workers spawn
    /// hook); `None` before `start`.
    #[must_use]
    pub fn background_tasks(&self) -> Option<&Arc<BackgroundTasks>> {
        self.tasks.get()
    }

    /// The leader elector prepared by `start`; `None` before `start` and when no leader-only
    /// worker is enabled.
    #[must_use]
    pub fn leader_elector(&self) -> Option<&Arc<dyn LeaderElector>> {
        self.elector.get()
    }

    /// The validated configuration built by `init`.
    #[must_use]
    pub fn config(&self) -> Option<&Arc<MiniChatConfig>> {
        self.config.get()
    }

    /// The token that stops the background indexing tasks of the attachments service; `stop`
    /// cancels it. `None` before `init`.
    #[must_use]
    pub fn uploads_shutdown(&self) -> Option<&CancellationToken> {
        self.uploads_shutdown.get()
    }

    /// The platform clients resolved by `init`.
    #[must_use]
    pub fn clients(&self) -> Option<&PlatformClients> {
        self.clients.get()
    }

    /// The services built by `init`.
    #[must_use]
    pub fn services(&self) -> Option<&Arc<Services>> {
        self.services.get()
    }

    /// The outbox enqueuer built by `init` (bound by `start`).
    #[must_use]
    pub fn enqueuer(&self) -> Option<&Arc<OutboxEnqueuer>> {
        self.enqueuer.get()
    }

    /// Load the gear configuration: the section is required and `${VAR}` is expanded only in
    /// the fields that allow it.
    fn load_config(ctx: &GearCtx) -> anyhow::Result<MiniChatConfig> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-load-config
        let raw: serde_json::Value = ctx.config()?;
        let mut config = MiniChatConfig::parse(raw)?;
        config.expand_vars().map_err(|e| {
            anyhow::anyhow!("cannot expand the variables of the configuration: {e}")
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-load-config
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-validate-config
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-validation-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return-validation-error
        // a failed validation is a startup error that names the offending key or section
        config.validate()?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return-validation-error
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-validation-failed
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-validate-config
        for warning in config.deprecated_field_warnings() {
            tracing::warn!("{warning}");
        }
        Ok(config)
    }

    /// Resolve the platform clients from the `ClientHub`; a missing one is a startup error
    /// naming the dependency.
    fn resolve_clients(ctx: &GearCtx) -> anyhow::Result<PlatformClients> {
        let hub = ctx.client_hub();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-resolve-clients
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-client-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return-client-error
        let authorizer = hub
            .get::<dyn AuthZResolverApi>()
            .context("required dependency `authz_resolver` is not available")?;
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .context("required dependency `oagw` is not available")?;
        let authenticator = hub
            .get::<dyn AuthNResolverClient>()
            .context("required dependency `authn_resolver` is not available")?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return-client-error
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-client-missing
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-resolve-clients
        Ok(PlatformClients {
            authz: authorizer,
            authn: authenticator,
            oagw: gateway,
        })
    }

    fn try_init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = Arc::new(Self::load_config(ctx)?);
        let db = ctx.db_required()?.db();

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-registry
        // `upstream_alias` is filled with the host (the OAGW-derived alias) when it is not configured
        let providers = Arc::new(ProviderRegistry::build(&config.providers));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-registry
        let clients = Self::resolve_clients(ctx)?;

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-metrics-outbox
        let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
        let metrics = Arc::new(MiniChatMetrics::new(
            &opentelemetry::global::meter_with_scope(scope),
            &config.metrics.effective_prefix(),
        ));
        // the outbox pipeline is not started yet; `start` registers the queues and binds it
        let enqueuer = Arc::new(OutboxEnqueuer::new(queue_specs(
            &config.outbox,
            config.thread_summary_worker.claim_timeout_secs,
        )));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-metrics-outbox

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-create-gateways
        // the plugins are resolved lazily through types-registry, not at init
        let gateway = Arc::new(GtsModelPolicyGateway::new(
            ctx.client_hub(),
            config.vendor.clone(),
        ));
        let policy: Arc<dyn ModelPolicyGateway> =
            Arc::clone(&gateway) as Arc<dyn ModelPolicyGateway>;
        // the same gateway publishes the usage events of the usage queue handler
        let usage_publisher: Arc<dyn UsagePublisher> = gateway;
        let audit_gateway = Arc::new(AuditGateway::new(
            ctx.client_hub(),
            config.vendor.clone(),
            Arc::clone(&metrics),
        ));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-create-gateways
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-services
        let pep = Arc::new(PepService::with_pdp(Arc::clone(&clients.authz)));
        // chat management: the database provider is the chat store and the late-bound
        // enqueuer is the chat-cleanup queue
        let provider = Arc::new(DBProvider::new(db.clone()));
        let chats = ChatsService::new(
            Arc::clone(&pep),
            Arc::clone(&policy),
            Arc::clone(&provider),
            Arc::clone(&enqueuer) as Arc<dyn ChatCleanupQueue>,
        );
        // quota and billing: the policy through the gateway (no cache), the ledger over the
        // gear schema, the usage enqueue of the shared outbox, the cost metrics and the system
        // clock
        let quota = QuotaService::new(QuotaDeps {
            pep: Arc::clone(&pep),
            gateway: Arc::clone(&policy),
            db: Arc::clone(&provider),
            ledger: Arc::new(SqlQuotaLedger),
            queue: Arc::clone(&enqueuer) as Arc<dyn UsageQueue>,
            metrics: Arc::clone(&metrics) as Arc<dyn QuotaMetrics>,
            clock: Arc::new(SystemClock),
            settings: QuotaSettings::from_config(&config),
        });
        // message streaming: the messages list and turn status over the database provider, and
        // the send setup (validation, admission, preflight, reserve) with the quota service, the
        // stream finalization (the audit and thread-summary queues of the shared outbox, the
        // metrics) and the real provider task, which calls OAGW under the S2S context bound at
        // start
        let messages = MessagesService::new(Arc::clone(&pep), Arc::clone(&provider));
        let s2s = S2sContext::unbound();
        // attachments: the upload and get flows over the same database provider (chat, attachment
        // and vector store persistence), the provider registry, the OAGW storage client under the
        // S2S context bound at start, the attachment-cleanup queue of the shared outbox, the
        // upload permits (`rag.max_concurrent_uploads`) and the real timer; the background indexing
        // tasks stop with `uploads_shutdown`
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-rag
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-thumbnail
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-policy
        // the kill switches and catalog fields are read per request from the policy snapshot
        let uploads_shutdown = ctx.cancellation_token().child_token();
        let attachment_metrics: Arc<dyn AttachmentMetrics> =
            Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>;
        let attachments = AttachmentsService::new(AttachmentsDeps {
            pep: Arc::clone(&pep),
            policy: Arc::clone(&policy),
            chats: Arc::clone(&provider) as Arc<dyn ChatStore>,
            store: Arc::clone(&provider) as Arc<dyn AttachmentStore>,
            vector_stores: Arc::clone(&provider) as Arc<dyn ChatVectorStoreRepo>,
            providers: Arc::clone(&providers),
            storage: Arc::new(OagwStorageClient::new(
                Arc::clone(&clients.oagw),
                Arc::clone(&s2s),
            )),
            cleanup: Arc::clone(&enqueuer) as Arc<dyn AttachmentCleanupQueue>,
            permits: UploadPermits::new(
                config.rag.max_concurrent_uploads,
                Arc::clone(&attachment_metrics),
            ),
            clock: Arc::new(SystemClock),
            timer: Arc::new(TokioTimer),
            metrics: attachment_metrics,
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-return
            settings: AttachmentSettings {
                rag: config.rag.clone(),
                thumbnail: config.thumbnail.clone(),
            },
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-return
            shutdown: uploads_shutdown.clone(),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-policy
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-thumbnail
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-rag
        let attachments = Arc::new(attachments);
        let quota_service = Arc::new(quota.clone());
        let progress: Arc<dyn TurnProgress> = Arc::clone(&provider) as Arc<dyn TurnProgress>;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-knowledge
        // `knowledge_search.*` and `context.file_search_guard`
        let tool_source = Arc::new(AttachmentToolSource::new(
            Arc::clone(&provider) as Arc<dyn AttachmentFactsStore>,
            AttachmentToolSettings {
                file_search_guard: config.context.file_search_guard.clone(),
                knowledge: config.knowledge_search.clone(),
                providers: Arc::clone(&providers),
                retriever_configured: config.knowledge_search.enabled,
            },
        ));
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-configuration:p1:inst-cfg-knowledge
        let stream_store: Arc<dyn StreamStore> = Arc::clone(&provider) as Arc<dyn StreamStore>;
        let finalizer: Arc<dyn TurnFinalizer> = Arc::new(
            SqlTurnFinalizer::new(Arc::clone(&quota_service)).with_stream_deps(
                StreamFinalizationDeps {
                    outbox: Arc::clone(&enqueuer) as Arc<dyn TurnOutbox>,
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-summary
                    summary: SummarySettings {
                        enabled: config.thread_summary_worker.enabled,
                        compression_threshold_pct: config
                            .thread_summary_worker
                            .compression_threshold_pct,
                    },
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-summary
                    metrics: Arc::clone(&metrics),
                },
            ),
        );
        let provider_task = StreamingProviderTask::new(ProviderTaskDeps {
            gateway: Arc::clone(&clients.oagw),
            s2s: Arc::clone(&s2s),
            finalizer: Arc::clone(&finalizer),
            progress: Arc::clone(&progress),
            quota: Arc::clone(&quota_service),
            metrics: Arc::clone(&metrics),
            settings: ProviderTaskSettings::from_config(&config),
            knowledge: config.knowledge_search.enabled.then(|| KnowledgeLoopDeps {
                retriever: Arc::new(OagwKnowledgeRetriever::new(
                    Arc::clone(&clients.oagw),
                    Arc::clone(&s2s),
                    Arc::clone(&providers),
                    config.knowledge_search.clone(),
                )),
                progress: Arc::clone(&progress),
                metrics: Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>,
                settings: KnowledgeLoopSettings::from_config(&config.knowledge_search),
            }),
        });
        let streaming = Arc::new(
            MessageStreamService::new(
                Arc::clone(&pep),
                Arc::clone(&policy),
                stream_store,
                Arc::clone(&quota_service),
                Arc::clone(&providers),
                Arc::new(provider_task),
                finalizer,
                StreamSettings::from_config(&config),
            )
            .with_attachment_tools(tool_source),
        );
        // FEATURE 0006: retry, edit and delete reuse the send pipeline; reactions only need the
        // PEP, the store and the clock
        let mutations = TurnMutationService::new(MutationDeps {
            streaming: Arc::clone(&streaming),
            store: Arc::clone(&provider) as Arc<dyn MutationStore>,
            outbox: Arc::clone(&enqueuer) as Arc<dyn TurnOutbox>,
            metrics: Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
        });
        let reactions = ReactionService::new(
            Arc::clone(&pep),
            Arc::clone(&provider) as Arc<dyn ReactionStore>,
            Arc::clone(&quota_service.clock),
        );
        let services = Arc::new(
            Services::new(pep, Arc::clone(&policy))
                .with_chats(chats)
                .with_quota(quota)
                .with_messages(messages)
                .with_streaming_shared(streaming)
                .with_mutations(mutations)
                .with_reactions(reactions)
                .with_attachments_shared(Arc::clone(&attachments)),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-build-services

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-keep-services
        // the routes are registered later, from these services, by the REST registration step
        let already = |what: &str| anyhow::anyhow!("mini-chat {what} is already initialized");
        self.config
            .set(config)
            .map_err(|_| already("configuration"))?;
        self.db.set(db).map_err(|_| already("database"))?;
        self.clients.set(clients).map_err(|_| already("clients"))?;
        self.enqueuer.set(enqueuer).map_err(|_| already("outbox"))?;
        self.services
            .set(services)
            .map_err(|_| already("services"))?;
        self.policy
            .set(policy)
            .map_err(|_| already("model-policy gateway"))?;
        self.usage_publisher
            .set(usage_publisher)
            .map_err(|_| already("usage publisher"))?;
        self.providers
            .set(providers)
            .map_err(|_| already("provider registry"))?;
        self.metrics.set(metrics).map_err(|_| already("metrics"))?;
        self.audit_gateway
            .set(audit_gateway)
            .map_err(|_| already("audit gateway"))?;
        self.s2s.set(s2s).map_err(|_| already("S2S context"))?;
        self.quota
            .set(quota_service)
            .map_err(|_| already("quota service"))?;
        self.uploads_shutdown
            .set(uploads_shutdown)
            .map_err(|_| already("upload shutdown token"))?;
        self.attachments
            .set(attachments)
            .map_err(|_| already("attachments service"))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-keep-services
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        match self.try_init(ctx) {
            Ok(()) => {
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-init-ok
                self.set_state(LifecycleState::Initialized);
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-init-ok
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return
                Ok(())
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-return
            }
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-init-failed
                self.set_state(LifecycleState::Failed);
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-init-failed
                Err(error)
            }
        }
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-declare-migrations
        // the gear migrations include the platform outbox migrations
        all_migrations()
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-init:p1:inst-init-declare-migrations
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let (Some(config), Some(services)) = (self.config.get(), self.services.get()) else {
            anyhow::bail!("routes cannot be registered before the gear is initialized");
        };
        Ok(register_routes(
            router,
            openapi,
            &config.url_prefix,
            Arc::clone(services),
        ))
    }
}

/// Log an error for a summary model that is disabled or absent from the catalog.
fn log_summary_model_status(status: SummaryModelStatus, model_id: &str) {
    match status {
        SummaryModelStatus::Enabled => {}
        SummaryModelStatus::Disabled => tracing::error!(
            model_id,
            "thread_summary_worker.summary_model_id names a disabled catalog model; \
             summary jobs will be rejected"
        ),
        SummaryModelStatus::Missing => tracing::error!(
            model_id,
            "thread_summary_worker.summary_model_id names no catalog model; \
             summary jobs will be rejected"
        ),
    }
}

impl MiniChatGear {
    /// Check that `thread_summary_worker.summary_model_id` names an enabled catalog entry. A
    /// missing or disabled model, or a catalog that cannot be read, never fails the start.
    async fn check_summary_model(&self, config: &MiniChatConfig) {
        let Some(policy) = self.policy.get() else {
            return;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-summary-model-check
        // an empty id selects the documented default summary model
        let model_id = match config.thread_summary_worker.summary_model_id.as_str() {
            "" => DEFAULT_SUMMARY_MODEL,
            configured => configured,
        };
        // the check runs without a user: the plugin serves one catalog for everybody
        match policy.current_snapshot(Uuid::nil()).await {
            Ok(snapshot) => {
                let status = summary_model_status(&snapshot, model_id);
                log_summary_model_status(status, model_id);
            }
            Err(error) => tracing::warn!(
                model_id,
                %error,
                "cannot check thread_summary_worker.summary_model_id: the model catalog is unavailable"
            ),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-summary-model-check
    }

    /// Spawn the orphan watchdog on `tasks` when `orphan_watchdog.enabled`: it runs behind the
    /// leader elector of the role (the no-op elector when none was prepared) and stops with the
    /// gear cancellation token.
    fn spawn_orphan_watchdog(&self, config: &MiniChatConfig, tasks: &BackgroundTasks) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-watchdog
        // read once at start: a change takes effect after a restart
        if !config.orphan_watchdog.enabled {
            return;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-watchdog
        let (Some(quota), Some(enqueuer), Some(metrics)) =
            (self.quota.get(), self.enqueuer.get(), self.metrics.get())
        else {
            tracing::error!("the orphan watchdog cannot start: the gear is not initialized");
            return;
        };
        let elector = self
            .elector
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(NoopLeaderElector) as Arc<dyn LeaderElector>);
        let watchdog = Arc::new(OrphanWatchdog::new(
            Arc::clone(quota),
            Arc::clone(enqueuer) as Arc<dyn TurnOutbox>,
            elector,
            Arc::clone(metrics),
            WatchdogSettings::from_config(config),
        ));
        tasks.spawn("orphan-watchdog", move |token| watchdog.run(token));
    }

    /// The `start` steps in order; any error leaves the outbox pipeline not started when it
    /// comes from the elector or the provisioning.
    async fn try_start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let (Some(config), Some(clients), Some(registry)) =
            (self.config.get(), self.clients.get(), self.providers.get())
        else {
            anyhow::bail!("the gear cannot start before it is initialized");
        };
        let tasks = Arc::new(BackgroundTasks::new(&cancel));
        self.tasks
            .set(Arc::clone(&tasks))
            .map_err(|_| anyhow::anyhow!("mini-chat is already started"))?;

        // the elector (steps `inst-start-leader-needed` and `inst-start-leader-elector`)
        if let Some(elector) = prepare_elector(config, &tasks).await? {
            self.elector
                .set(elector)
                .map_err(|_| anyhow::anyhow!("the leader elector is already prepared"))?;
        }

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-provisioning
        let first = first_pass(
            clients.authn.as_ref(),
            clients.oagw.as_ref(),
            &config.client_credentials,
            registry,
        )
        .await;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-provisioning
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-provisioning-failed
        let first = match first {
            Ok(first) => first,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-return-provisioning-error
                // the outbox pipeline is not started; the elector's renewal stops
                tasks.cancel();
                return Err(anyhow::Error::new(error).context("OAGW provisioning failed"));
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-return-provisioning-error
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-provisioning-failed

        // the provider task calls OAGW under the S2S context obtained at start: the upstreams
        // and routes are owned by its tenant; the user and tenant identity travel only in the
        // request body
        if let Some(s2s) = self.s2s.get() {
            s2s.bind(first.s2s.clone());
        }

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-deferred-pending
        if !first.deferred.is_empty() {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-spawn-reconcile
            // the outbox step follows right away, without waiting for the reconcile task
            let (oagw, s2s, deferred) =
                (Arc::clone(&clients.oagw), first.s2s.clone(), first.deferred);
            tasks.spawn("oagw-provisioning-reconcile", move |token| {
                run_reconcile(oagw, s2s, deferred, token)
            });
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-spawn-reconcile
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-deferred-pending

        if let Err(error) = self.start_outbox().await {
            tasks.cancel();
            return Err(error);
        }
        self.check_summary_model(config).await;
        self.spawn_orphan_watchdog(config, &tasks);
        self.spawn_upload_reaper(config, &tasks);
        Ok(())
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        match self.try_start(cancel).await {
            Ok(()) => {
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-start-ok
                self.set_state(LifecycleState::Started);
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-start-ok
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-return
                Ok(())
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-return
            }
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-start-failed
                self.set_state(LifecycleState::Failed);
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-start-failed
                Err(error)
            }
        }
    }

    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-stop-signal
        self.set_state(LifecycleState::Stopping);
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-stop-signal
        // the background indexing tasks stop at their next await: the rows they leave `uploaded`
        // are finished by the upload reaper
        if let Some(token) = self.uploads_shutdown.get() {
            token.cancel();
        }
        if let Some(attachments) = self.attachments.get() {
            tokio::select! {
                result = tokio::time::timeout(WORKER_JOIN_TIMEOUT, attachments.drain_background()) => {
                    if result.is_err() {
                        tracing::warn!("background indexing tasks did not stop in time");
                    }
                }
                () = deadline_token.cancelled() => {
                    tracing::warn!("framework deadline fired; giving up on the background indexing");
                }
            }
        }
        // the reconcile task and the background workers are cancelled and joined before the
        // outbox pipeline stops, because a winding-down worker may still enqueue
        if let Some(tasks) = self.tasks.get() {
            tokio::select! {
                report = tasks.shutdown(WORKER_JOIN_TIMEOUT) => {
                    if !report.timed_out.is_empty() {
                        tracing::warn!(
                            tasks = %report.timed_out.join(", "),
                            "background tasks did not stop in time and were aborted"
                        );
                    }
                }
                () = deadline_token.cancelled() => {
                    tasks.cancel();
                    tracing::warn!("framework deadline fired; giving up on the background tasks");
                }
            }
        }
        self.stop_outbox(&deadline_token).await;
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-stopped
        self.set_state(LifecycleState::Stopped);
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-gear-stopped
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-return
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;

#[cfg(test)]
#[path = "gear_start_tests.rs"]
mod gear_start_tests;

#[cfg(test)]
#[path = "gear_stream_tests.rs"]
mod gear_stream_tests;

#[cfg(test)]
#[path = "gear_startup_config_tests.rs"]
mod gear_startup_config_tests;

#[cfg(test)]
#[path = "gear_attachment_tests.rs"]
mod gear_attachment_tests;

#[cfg(test)]
#[path = "gear_attachment_ops_tests.rs"]
mod gear_attachment_ops_tests;

#[cfg(test)]
#[path = "gear_maintenance_tests.rs"]
mod gear_maintenance_tests;
