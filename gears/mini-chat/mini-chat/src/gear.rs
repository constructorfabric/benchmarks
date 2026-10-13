//! Gear declaration and lifecycle (DESIGN section 3.2 "Gear lifecycle").
//!
//! `init` loads the configuration, expands `${VAR}` placeholders, fills the
//! upstream aliases, validates every section (an invalid configuration fails
//! startup) and logs deprecated keys. It then builds the model-policy and
//! audit gateways (plugins are resolved lazily through types-registry), the
//! PEP over the `authz_resolver` client (resolved from the `ClientHub` at call
//! time), the S2S security context source (`authn_resolver` client
//! credentials), the OAGW client, the provider resolver and the LLM
//! adapters, the outbox enqueuer (not started yet) with the usage and audit
//! handlers, the repositories and the domain services (quota, stream, turn
//! mutations, attachments over the provider Files / vector store storage
//! and the WebP thumbnailer, thread summaries, orphan finalization),
//! registers the real handler of every outbox queue (usage, audit,
//! attachment cleanup, chat cleanup, thread summary) and keeps the gear's
//! database provider. The REST routes are registered in the separate REST
//! registration step from the services built in `init`. The gear migrations
//! include the shared outbox migrations.
//!
//! The lifecycle entry is the `start` phase: it prepares the leader elector
//! of every enabled leader-only worker (orphan watchdog, upload reaper),
//! registers the OAGW upstreams and routes of every provider (a
//! deterministic misconfiguration fails startup; transient failures are
//! retried by a background reconcile task), starts the outbox pipeline with
//! the five mini-chat queues and attaches it to the enqueuer, checks the
//! thread-summary model (logged, never fatal), spawns the orphan watchdog
//! and the upload reaper (after the outbox: the reaper enqueues attachment
//! cleanup events) and reports ready. On cancellation (`stop`) it cancels
//! the attachment shutdown token (background indexing tasks stop), cancels
//! the workers and joins them within [`WORKER_JOIN_TIMEOUT`], then stops
//! the reconcile task and the pipeline; the runtime's stop timeout bounds
//! the drain.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use oagw_sdk::api::ServiceGatewayClientV1;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::{DBProvider, DbError};
use tracing::{info, warn};

use crate::api::rest::{self, RestState};
use crate::config::MiniChatConfig;
use crate::domain::attachment_service::{AttachmentPorts, AttachmentService, AttachmentSettings};
use crate::domain::authz::AuthzService;
use crate::domain::chat_service::ChatService;
use crate::domain::knowledge::KnowledgeRetriever;
use crate::domain::message_service::MessageService;
use crate::domain::models_service::ModelsService;
use crate::domain::orphan::OrphanFinalizer;
use crate::domain::ports::{OrphanTurnRepository, QuotaRepository, ThreadSummaryStore, TurnOutbox};
use crate::domain::quota_service::{QuotaService, QuotaSettings};
use crate::domain::stream::{StreamService, StreamSettings};
use crate::domain::summary_service::{SummarySettings, ThreadSummaryService, check_summary_model};
use crate::domain::turn_service::TurnService;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::chat_cleanup_handler::ChatCleanupHandler;
use crate::infra::db::repo::DbRepository;
use crate::infra::llm::LlmAdapters;
use crate::infra::llm::anthropic_files::AnthropicFiles;
use crate::infra::llm::knowledge::AzureKnowledgeRetriever;
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::llm::storage::ProviderStorage;
use crate::infra::oagw_provisioning::{self, AuthnSystemContext, SystemContextSource};
use crate::infra::outbox::{MiniChatQueue, OutboxEnqueuer, OutboxQueues};
use crate::infra::outbox_handlers::AttachmentCleanupHandler;
use crate::infra::outbox_pipeline::{self, OutboxPipeline, QueueHandlers};
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::infra::summary_handler::ThreadSummaryHandler;
use crate::infra::thumbnail::{ThumbnailLimits, WebpThumbnailer};
use crate::infra::workers::{
    LeaderElector, NoopElector, OrphanWatchdog, UploadReaper, WorkerRole, elector_for,
};

/// Upper bound of the background workers' join on stop; a worker still
/// running afterwards is aborted.
pub const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The spawned leader-only workers and the token that stops them (and their
/// electors).
struct BackgroundWorkers {
    cancel: CancellationToken,
    handles: Vec<(WorkerRole, JoinHandle<()>)>,
}

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    services: OnceLock<Arc<MiniChatServices>>,
    /// Outbox pipeline started by the lifecycle entry; taken on stop.
    outbox: Mutex<Option<OutboxPipeline>>,
    /// Background OAGW reconcile task (pending providers); taken on stop.
    reconcile: Mutex<Option<JoinHandle<()>>>,
    /// Orphan watchdog and upload reaper spawned by `start`; taken on stop.
    workers: Mutex<Option<BackgroundWorkers>>,
}

/// Everything `init` builds: the validated configuration, the gateways, the
/// PEP, the domain services and the database provider.
pub struct MiniChatServices {
    /// Validated configuration.
    pub config: Arc<MiniChatConfig>,
    /// Database provider of the gear (`gears.mini-chat.database`).
    pub db: Arc<DBProvider<DbError>>,
    /// Model-policy plugin gateway of the configured vendor.
    pub policy: Arc<ModelPolicyGateway>,
    /// Audit plugin gateway of the configured vendor.
    pub audit: Arc<AuditGateway>,
    /// Policy Enforcement Point.
    pub authz: Arc<AuthzService>,
    /// Models API service.
    pub models: Arc<ModelsService>,
    /// Outbox producer; attached to the pipeline by the lifecycle entry.
    pub outbox: Arc<OutboxEnqueuer>,
    /// Leased handler of every outbox queue.
    pub queue_handlers: QueueHandlers,
    /// Chat CRUD service.
    pub chats: Arc<ChatService>,
    /// Message history, reaction and turn status service.
    pub messages: Arc<MessageService>,
    /// Quota service.
    pub quota: Arc<QuotaService>,
    /// Streaming turn service.
    pub stream: Arc<StreamService>,
    /// Turn mutation service (retry, edit, delete).
    pub turns: Arc<TurnService>,
    /// Attachment upload, read and delete service.
    pub attachments: Arc<AttachmentService>,
    /// Repositories (and their outbox producer) of the upload reaper.
    pub repo: Arc<DbRepository>,
    /// Orphan turn finalizer run by the orphan watchdog.
    pub orphans: Arc<OrphanFinalizer>,
    /// Stops the background work of the services (attachment indexing);
    /// cancelled by the lifecycle entry on stop.
    pub shutdown: CancellationToken,
    /// OAGW client (provisioning and proxying).
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    /// S2S security context source (OAGW provisioning and proxying).
    pub system: Arc<dyn SystemContextSource>,
}

impl std::fmt::Debug for MiniChatServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiniChatServices")
            .field("vendor", &self.config.vendor)
            .field("url_prefix", &self.config.url_prefix)
            .field("models", &self.models)
            .field("outbox", &self.outbox)
            .finish_non_exhaustive()
    }
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            services: OnceLock::new(),
            outbox: Mutex::new(None),
            reconcile: Mutex::new(None),
            workers: Mutex::new(None),
        }
    }
}

impl MiniChatGear {
    /// Validated configuration; `None` before `init`.
    #[must_use]
    pub fn config(&self) -> Option<&Arc<MiniChatConfig>> {
        self.services.get().map(|services| &services.config)
    }

    /// Services built by `init`; `None` before `init`.
    #[must_use]
    pub fn services(&self) -> Option<&Arc<MiniChatServices>> {
        self.services.get()
    }

    /// Lifecycle entry (`start` / `stop`): prepares the leader electors,
    /// registers the OAGW upstreams, starts the outbox pipeline, spawns the
    /// background workers, reports ready, runs until the runtime cancels it,
    /// then stops the workers, the reconcile task and the pipeline.
    #[allow(
        clippy::redundant_pub_crate,
        reason = "module-private lifecycle entry invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        self.start(&cancel).await?;
        ready.notify();
        info!(target: "mini_chat.lifecycle", "mini-chat started");

        cancel.cancelled().await;

        self.stop().await;
        info!(target: "mini_chat.lifecycle", "mini-chat stopped");
        Ok(())
    }

    /// `stop`: cancels the attachment background work, stops the workers
    /// (bounded join), the reconcile task, then the outbox pipeline.
    async fn stop(&self) {
        if let Some(services) = self.services() {
            services.shutdown.cancel();
        }
        self.stop_workers().await;
        self.stop_reconcile().await;
        self.stop_outbox().await;
    }

    /// `start`: leader electors, OAGW provisioning, the outbox pipeline, the
    /// summary model check, then the background workers. A failure stops
    /// what was already started.
    async fn start(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let services = Arc::clone(self.initialized()?);
        let workers_cancel = cancel.child_token();
        let electors = match Electors::prepare(&services.config, &workers_cancel).await {
            Ok(electors) => electors,
            Err(err) => {
                workers_cancel.cancel();
                return Err(err);
            }
        };
        if let Err(err) = self.provision_oagw(cancel).await {
            workers_cancel.cancel();
            return Err(err);
        }
        if let Err(err) = self.start_outbox().await {
            workers_cancel.cancel();
            self.stop_reconcile().await;
            return Err(err);
        }
        // Logged only: a dynamic policy plugin can enable the model later.
        check_summary_model(
            services.policy.as_ref(),
            &services.config.thread_summary_worker,
        )
        .await;
        self.spawn_workers(&services, electors, workers_cancel);
        Ok(())
    }

    /// `start`: spawns the orphan watchdog and the upload reaper; both stop
    /// with `cancel` (a disabled worker returns at once).
    fn spawn_workers(
        &self,
        services: &MiniChatServices,
        electors: Electors,
        cancel: CancellationToken,
    ) {
        let prefix = services.config.metrics.effective_prefix();
        let watchdog = OrphanWatchdog::new(
            Arc::clone(&services.orphans),
            electors.orphan,
            &services.config.orphan_watchdog,
            prefix,
        );
        let reaper = UploadReaper::new(
            Arc::clone(&services.repo),
            electors.reaper,
            &services.config.upload_reaper,
            prefix,
        );
        let watchdog_cancel = cancel.child_token();
        let reaper_cancel = cancel.child_token();
        let handles = vec![
            (
                WorkerRole::OrphanWatchdog,
                tokio::spawn(async move { watchdog.run(&watchdog_cancel).await }),
            ),
            (
                WorkerRole::UploadReaper,
                tokio::spawn(async move { reaper.run(&reaper_cancel).await }),
            ),
        ];
        *self.workers.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(BackgroundWorkers { cancel, handles });
    }

    /// `stop`: cancels the background workers (and their electors) and joins
    /// them within [`WORKER_JOIN_TIMEOUT`]; a worker still running then is
    /// aborted.
    async fn stop_workers(&self) {
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(workers) = workers else {
            return;
        };
        workers.cancel.cancel();
        let deadline = tokio::time::Instant::now() + WORKER_JOIN_TIMEOUT;
        for (role, mut handle) in workers.handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                warn!(
                    target: "mini_chat.lifecycle",
                    role = role.as_str(),
                    "background worker did not stop in time; aborted"
                );
                handle.abort();
            }
        }
        info!(target: "mini_chat.lifecycle", "mini-chat background workers stopped");
    }

    fn initialized(&self) -> anyhow::Result<&Arc<MiniChatServices>> {
        self.services
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: serve invoked before init"))
    }

    /// `start`: registers the OAGW upstream and routes of every provider
    /// entry; pending ones are left to the background reconcile task, which
    /// stops with `cancel`.
    ///
    /// # Errors
    ///
    /// A deterministic misconfiguration (invalid `api_path`, OAGW validation
    /// failure, rejected S2S credentials).
    async fn provision_oagw(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let services = self.initialized()?;
        let started = tokio::time::Instant::now();
        let plans = oagw_provisioning::plan_upstreams(&services.config.providers)?;
        let report = oagw_provisioning::provision(
            services.gateway.as_ref(),
            services.system.as_ref(),
            &plans,
        )
        .await?;
        info!(
            target: "mini_chat.lifecycle",
            registered = report.registered.len(),
            pending = report.pending.len(),
            "mini-chat OAGW provisioning done"
        );
        let handle = oagw_provisioning::spawn_reconcile(
            Arc::clone(&services.gateway),
            Arc::clone(&services.system),
            report.pending_plans(),
            started,
            cancel.child_token(),
        );
        *self
            .reconcile
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = handle;
        Ok(())
    }

    /// `stop`: stops the background reconcile task, if any.
    async fn stop_reconcile(&self) {
        let handle = self
            .reconcile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle.abort();
            // A cancelled (aborted) task is the expected outcome.
            drop(handle.await);
        }
    }

    /// `start`: runs the outbox pipeline with every mini-chat queue and
    /// attaches it to the enqueuer.
    async fn start_outbox(&self) -> anyhow::Result<()> {
        let services = self.initialized()?;
        let pipeline = outbox_pipeline::start(
            services.db.db(),
            &services.config,
            &services.queue_handlers,
            &services.outbox,
        )
        .await
        .map_err(|err| anyhow::anyhow!("mini-chat: outbox pipeline failed to start: {err}"))?;
        *self.outbox.lock().unwrap_or_else(PoisonError::into_inner) = Some(pipeline);
        info!(
            target: "mini_chat.lifecycle",
            queues = ?services.outbox.queues(),
            "mini-chat outbox pipeline started"
        );
        Ok(())
    }

    /// `stop`: stops the outbox pipeline; the runtime's stop timeout bounds
    /// the drain.
    async fn stop_outbox(&self) {
        // Release the lock before awaiting the drain.
        let pipeline = self
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(pipeline) = pipeline {
            pipeline.stop().await;
            info!(target: "mini_chat.lifecycle", "mini-chat outbox pipeline stopped");
        }
    }
}

/// The leader electors of the background workers.
struct Electors {
    orphan: Arc<dyn LeaderElector>,
    reaper: Arc<dyn LeaderElector>,
}

impl Electors {
    /// The elector of every enabled leader-only worker (renewal stops with
    /// `cancel`); a disabled worker gets the [`NoopElector`] (it never
    /// scans).
    ///
    /// # Errors
    ///
    /// The elector of an enabled worker cannot be prepared (`k8s` build:
    /// missing pod identity or Kubernetes client configuration).
    async fn prepare(config: &MiniChatConfig, cancel: &CancellationToken) -> anyhow::Result<Self> {
        Ok(Self {
            orphan: Self::elector(
                WorkerRole::OrphanWatchdog,
                config.orphan_watchdog.enabled,
                cancel,
            )
            .await?,
            reaper: Self::elector(
                WorkerRole::UploadReaper,
                config.upload_reaper.enabled,
                cancel,
            )
            .await?,
        })
    }

    async fn elector(
        role: WorkerRole,
        enabled: bool,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Arc<dyn LeaderElector>> {
        if !enabled {
            return Ok(Arc::new(NoopElector));
        }
        elector_for(role, cancel).await.map_err(|err| {
            anyhow::anyhow!(
                "mini-chat: leader elector of `{}` failed: {err}",
                role.as_str()
            )
        })
    }
}

/// Load and prepare the configuration: `${VAR}` expansion, upstream alias
/// fill, validation (DESIGN Appendix B.1).
fn load_config(ctx: &GearCtx) -> anyhow::Result<MiniChatConfig> {
    let mut cfg: MiniChatConfig = ctx
        .config_or_default()
        .map_err(|err| anyhow::anyhow!("mini-chat config could not be loaded: {err}"))?;
    // Both steps return messages prefixed with `mini-chat config:` and the key.
    cfg.expand_env().map_err(anyhow::Error::msg)?;
    cfg.fill_upstream_aliases();
    cfg.validate().map_err(anyhow::Error::msg)?;
    Ok(cfg)
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(module = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = load_config(ctx)?;
        for warning in cfg.deprecation_warnings() {
            warn!(target: "mini_chat.config", "{warning}");
        }
        info!(
            vendor = %cfg.vendor,
            providers = cfg.providers.len(),
            url_prefix = %cfg.url_prefix,
            "mini-chat configuration loaded"
        );

        let db = Arc::new(ctx.db_required()?);
        let hub = ctx.client_hub();

        // Plugins are looked up lazily on first use: they may register after
        // this gear's `init`.
        let policy = Arc::new(ModelPolicyGateway::new(
            Arc::clone(&hub),
            cfg.vendor.clone(),
        ));
        let audit = Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));

        // The OAGW and authn clients are registered by gears this gear
        // depends on, so they are in the hub by now.
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|err| anyhow::anyhow!("mini-chat: OAGW client is not available: {err}"))?;
        let authn = hub.get::<dyn AuthNResolverClient>().map_err(|err| {
            anyhow::anyhow!("mini-chat: authn-resolver client is not available: {err}")
        })?;
        let system: Arc<dyn SystemContextSource> =
            Arc::new(AuthnSystemContext::new(authn, &cfg.client_credentials));

        // The `AuthZResolverApi` client is resolved from the hub at call time.
        let authz = Arc::new(AuthzService::new(PolicyEnforcer::from_hub(hub)));

        let policy_client: Arc<dyn MiniChatModelPolicyPluginClientV1> = policy.clone();
        let models = Arc::new(ModelsService::new(
            Arc::clone(&policy_client),
            Arc::clone(&authz),
        ));

        // The pipeline is started by the lifecycle entry; producers enqueue
        // once it is attached.
        let queues = OutboxQueues::from_config(&cfg.outbox);
        let queue_handlers_base = QueueHandlers::for_gear(
            &queues,
            policy.clone(),
            Arc::clone(&audit),
            cfg.metrics.effective_prefix(),
        );
        let outbox = Arc::new(OutboxEnqueuer::new(queues));
        let repo = Arc::new(DbRepository::new(Arc::clone(&db), Arc::clone(&outbox)));
        let repos = repo.repositories();

        // Provider Files API / vector stores through OAGW, shared by uploads
        // and the attachment cleanup handler.
        let storage = Arc::new(ProviderStorage::new(
            Arc::clone(&gateway),
            Arc::clone(&system),
            &cfg.providers,
        ));
        // Anthropic Files API client (secondary image copies), only when an
        // entry uses `anthropic_messages`.
        let anthropic_files = AnthropicFiles::from_providers(
            Arc::clone(&gateway),
            Arc::clone(&system),
            &cfg.providers,
        )
        .map(Arc::new);
        let resolver = Arc::new(ProviderResolver::new(&cfg.providers));
        let adapters = Arc::new(LlmAdapters::new(Arc::clone(&gateway), Arc::clone(&system)));
        let summary = Arc::new(ThreadSummaryService::new(
            Arc::clone(&repo) as Arc<dyn ThreadSummaryStore>,
            Arc::clone(&policy_client),
            resolver.clone(),
            adapters.clone(),
            SummarySettings::from_config(&cfg.thread_summary_worker),
        ));
        let metrics_prefix = cfg.metrics.effective_prefix();
        let mut attachment_cleanup = AttachmentCleanupHandler::new(
            Arc::clone(&repo),
            storage.clone(),
            cfg.cleanup_worker.max_attempts,
        );
        let mut chat_cleanup = ChatCleanupHandler::new(
            Arc::clone(&repo),
            storage.clone(),
            storage.clone(),
            cfg.cleanup_worker.max_attempts,
            metrics_prefix,
        );
        if let Some(files) = &anthropic_files {
            attachment_cleanup = attachment_cleanup.with_secondary(files.clone());
            chat_cleanup = chat_cleanup.with_secondary(files.clone(), files.clone());
        }
        let queue_handlers = queue_handlers_base
            .with_attachment_cleanup(Arc::new(attachment_cleanup))
            .with(MiniChatQueue::ChatCleanup, Arc::new(chat_cleanup))
            .with(
                MiniChatQueue::ThreadSummary,
                Arc::new(ThreadSummaryHandler::new(summary, metrics_prefix)),
            );
        let chats = Arc::new(ChatService::new(
            repos.chats.clone(),
            Arc::clone(&models),
            Arc::clone(&authz),
        ));
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&repo) as Arc<dyn QuotaRepository>,
            Arc::clone(&policy_client),
            Arc::clone(&authz),
            QuotaSettings::from_config(&cfg),
        ));
        let orphans = Arc::new(OrphanFinalizer::new(
            db.db(),
            Arc::clone(&repo) as Arc<dyn OrphanTurnRepository>,
            Arc::clone(&repo) as Arc<dyn TurnOutbox>,
            Arc::clone(&quota),
            cfg.orphan_watchdog.timeout_secs,
        ));
        let stream_service = StreamService::new(
            db.db(),
            repos.clone(),
            Arc::clone(&quota),
            Arc::clone(&authz),
            resolver,
            adapters,
            StreamSettings::from_config(&cfg),
        );
        // The knowledge retriever exists only when knowledge search is on.
        let stream = Arc::new(
            match AzureKnowledgeRetriever::from_config(
                Arc::clone(&gateway),
                Arc::clone(&system),
                &cfg.knowledge_search,
                &cfg.providers,
                metrics_prefix,
            ) {
                Some(retriever) => {
                    if let Err(reason) = retriever.check(uuid::Uuid::nil()) {
                        warn!(
                            target: "mini_chat.config",
                            reason = %reason,
                            "knowledge search is enabled but its provider cannot serve it; search_knowledge will not be offered"
                        );
                    }
                    stream_service.with_knowledge_retriever(Arc::new(retriever))
                }
                None => stream_service,
            },
        );
        let turns = Arc::new(TurnService::new(
            db.db(),
            repos.clone(),
            Arc::clone(&authz),
            Arc::clone(&stream),
        ));
        let shutdown = CancellationToken::new();
        let attachment_service = AttachmentService::new(
            AttachmentPorts {
                chats: repos.chats.clone(),
                attachments: Arc::clone(&repo) as _,
                vector_stores: Arc::clone(&repo) as _,
                files: storage.clone(),
                stores: storage,
                thumbnails: Arc::new(WebpThumbnailer::new(ThumbnailLimits::from_config(
                    &cfg.thumbnail,
                ))),
            },
            Arc::clone(&models),
            Arc::clone(&authz),
            AttachmentSettings::from_config(&cfg),
            shutdown.clone(),
        );
        let attachments = Arc::new(match anthropic_files {
            Some(files) => attachment_service.with_secondary(files, cfg.thumbnail.max_decode_bytes),
            None => attachment_service,
        });
        let messages = Arc::new(MessageService::new(repos, Arc::clone(&authz)));

        let services = MiniChatServices {
            config: Arc::new(cfg),
            db,
            policy,
            audit,
            authz,
            models,
            outbox,
            queue_handlers,
            chats,
            messages,
            quota,
            stream,
            turns,
            attachments,
            repo,
            orphans,
            shutdown,
            gateway,
            system,
        };
        self.services
            .set(Arc::new(services))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        info!("mini-chat gear initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        // Gear tables followed by the shared outbox migrations.
        crate::infra::db::migrations()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let services = self
            .services
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: register_rest invoked before init"))?;
        let state = RestState {
            models: Arc::clone(&services.models),
            chats: Arc::clone(&services.chats),
            messages: Arc::clone(&services.messages),
            stream: Arc::clone(&services.stream),
            turns: Arc::clone(&services.turns),
            quota: Arc::clone(&services.quota),
            attachments: Arc::clone(&services.attachments),
            sse_ping_interval: Duration::from_secs(u64::from(
                services.config.streaming.sse_ping_interval_seconds,
            )),
        };
        let router = rest::register_routes(router, openapi, state, &services.config.url_prefix);
        info!(url_prefix = %services.config.url_prefix, "mini-chat REST routes registered");
        Ok(router)
    }
}
