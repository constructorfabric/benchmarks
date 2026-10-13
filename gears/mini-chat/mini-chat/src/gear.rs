//! Gear declaration, lifecycle and wiring
//! (`cpt-cf-mini-chat-flow-gear-foundation-gear-init`, `-gear-start`, `-gear-stop`,
//! `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle`).
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-gear-crate:p1

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use oagw_sdk::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::app_services::AppServices;
use crate::config::{MiniChatConfig, ProviderRegistry};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::attachments::service::{
    AttachmentSettings, AttachmentsDeps, AttachmentsService, PROVIDER_DELETE_TIMEOUT,
};
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::domain::service::models::ModelsService;
use crate::domain::service::mutations::ports::MutationMetrics;
use crate::domain::service::mutations::service::{MutationDeps, MutationService};
use crate::domain::service::quota::ports::{QuotaMetrics, QuotaRepository};
use crate::domain::service::quota::service::{QuotaDeps, QuotaService, QuotaServiceConfig};
use crate::domain::service::stream::pipeline::{StreamDeps, StreamService, StreamSettings};
use crate::domain::service::stream::ports::{
    KnowledgeRetriever, ProviderClient, ProviderDirectory, StreamMetrics, TurnRepository,
};
use crate::domain::service::workers::CHAT_CLEANUP_DELIVERY_BUDGET;
use crate::domain::service::workers::chat_cleanup::{ChatCleanupDeps, ChatCleanupService};
use crate::domain::service::workers::clock::{SharedWorkerClock, TokioWorkerClock};
use crate::domain::service::workers::metrics::WorkerMetrics;
use crate::domain::service::workers::ports::{SummaryModelClient, SummaryStore};
use crate::domain::service::workers::summary::{SummaryDeps, SummaryService};
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::background_tasks::{BackgroundTasks, TASK_JOIN_TIMEOUT};
use crate::infra::db::repos::attachments::SeaAttachmentRepository;
use crate::infra::db::repos::chat_cleanup::SeaChatCleanupStore;
use crate::infra::db::repos::chats::SeaChatRepository;
use crate::infra::db::repos::messages::SeaMessageRepository;
use crate::infra::db::repos::mutations::SeaMutationRepository;
use crate::infra::db::repos::orphans::SeaOrphanStore;
use crate::infra::db::repos::quota::SeaQuotaRepository;
use crate::infra::db::repos::summaries::SeaSummaryStore;
use crate::infra::db::repos::turns::SeaTurnRepository;
use crate::infra::llm::knowledge_retriever::OagwKnowledgeRetriever;
use crate::infra::llm::oagw_client::OagwProviderClient;
use crate::infra::llm::oagw_provisioning::{
    OagwProvisioner, ProvisionTarget, ProvisioningTimeouts,
};
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::llm::reconcile::{ReconcileSchedule, TargetRetry, reconcile_providers};
use crate::infra::llm::summary_client::OagwSummaryClient;
use crate::infra::metrics::{AuditEmitMetrics, MiniChatMetrics};
use crate::infra::outbox::attachment_cleanup::{AttachmentCleanupHandler, CleanupHandlerDeps};
use crate::infra::outbox::audit::AuditHandler;
use crate::infra::outbox::chat_cleanup::ChatCleanupHandler;
use crate::infra::outbox::queues::{OutboxEnqueuer, OutboxHandlers, start_outbox};
use crate::infra::outbox::thread_summary::ThreadSummaryHandler;
use crate::infra::outbox::usage::UsageHandler;
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::infra::runtime::GearRuntime;
use crate::infra::startup_checks::check_summary_model;
use crate::infra::storage::oagw_storage::OagwStorageClient;
use crate::infra::workers::leader::{LeaderElector, start_elector};
use crate::infra::workers::orphan_watchdog::{ORPHAN_WATCHDOG_TASK, OrphanWatchdog, WatchdogDeps};
use crate::infra::workers::upload_reaper::{ReaperDeps, UPLOAD_REAPER_TASK, UploadReaper};

/// Lifecycle state of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearLifecycleState {
    /// Constructed, `init` not called.
    Created,
    /// `init` succeeded.
    Initialized,
    /// `init` failed (invalid configuration or missing client).
    InitFailed,
    /// `start` succeeded.
    Started,
    /// `start` failed (S2S exchange or misconfigured provider).
    StartFailed,
    /// `stop` completed.
    Stopped,
}

/// Everything `init` builds.
struct Wired {
    /// Services and shared handles exposed to the API layer and later features.
    services: Arc<AppServices>,
    /// Validated, alias-filled provider entries (input of the OAGW provisioning).
    providers: ProviderRegistry,
    /// S2S credential exchange client.
    authn: Arc<dyn AuthNResolverClient>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    state: Mutex<GearLifecycleState>,
    timeouts: ProvisioningTimeouts,
    wired: OnceLock<Arc<Wired>>,
    elector: Mutex<Option<Arc<dyn LeaderElector>>>,
    outbox: tokio::sync::Mutex<Option<OutboxHandle>>,
    tasks: Mutex<Option<Arc<BackgroundTasks>>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: Mutex::new(GearLifecycleState::Created),
            timeouts: ProvisioningTimeouts::default(),
            wired: OnceLock::new(),
            elector: Mutex::new(None),
            outbox: tokio::sync::Mutex::new(None),
            tasks: Mutex::new(None),
        }
    }
}

/// The knowledge retriever, wired only when `knowledge_search.enabled`
/// (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`): the OAGW
/// retriever of `knowledge_search.provider_id` and
/// `knowledge_search.vector_store_id`, both validated at load.
fn knowledge_retriever(
    config: &MiniChatConfig,
    oagw: &Arc<dyn ServiceGatewayClientV1>,
    providers: &Arc<ProviderResolver>,
) -> Option<Arc<dyn KnowledgeRetriever>> {
    let knowledge = &config.knowledge_search;
    if !knowledge.enabled {
        return None;
    }
    let (provider_id, vector_store_id) = knowledge
        .provider_id
        .as_deref()
        .zip(knowledge.vector_store_id.as_deref())?;
    Some(Arc::new(OagwKnowledgeRetriever::new(
        Arc::clone(oagw),
        Arc::clone(providers) as Arc<dyn ProviderDirectory>,
        provider_id,
        vector_store_id,
    )))
}

/// Resolves a required client from `ClientHub`, naming it when it is missing.
fn required<T: ?Sized + Send + Sync + 'static>(
    ctx: &GearCtx,
    name: &str,
) -> anyhow::Result<Arc<T>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-resolve-clients
    let client = ctx.client_hub().get::<T>();
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-resolve-clients
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-if-client-missing
    client.map_err(|error| {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-client-missing
        anyhow::anyhow!("required client `{name}` is not available: {error}")
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-client-missing
    })
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-if-client-missing
}

impl MiniChatGear {
    /// Overrides the S2S and OAGW call bounds (tests use short ones).
    #[must_use]
    pub fn with_provisioning_timeouts(mut self, timeouts: ProvisioningTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// The services and shared handles built at `init`.
    #[must_use]
    pub fn services(&self) -> Option<Arc<AppServices>> {
        self.wired.get().map(|wired| Arc::clone(&wired.services))
    }

    /// The outbox enqueuer built at `init` (its pipeline is bound at `start`).
    #[must_use]
    pub fn outbox_enqueuer(&self) -> Option<Arc<OutboxEnqueuer>> {
        self.wired
            .get()
            .map(|wired| Arc::clone(&wired.services.outbox))
    }

    /// The leader elector prepared at `start`; `None` before `start` or when no
    /// leader-only worker is enabled.
    #[must_use]
    pub fn leader_elector(&self) -> Option<Arc<dyn LeaderElector>> {
        self.elector.lock().ok().and_then(|slot| slot.clone())
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn lifecycle_state(&self) -> GearLifecycleState {
        self.state
            .lock()
            .map_or(GearLifecycleState::Created, |state| *state)
    }

    /// Names of the background tasks that are still running.
    #[must_use]
    pub fn running_background_tasks(&self) -> Vec<&'static str> {
        self.tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.as_ref().map(|tasks| tasks.running()))
            .unwrap_or_default()
    }

    fn set_state(&self, next: GearLifecycleState) {
        if let Ok(mut state) = self.state.lock() {
            *state = next;
        }
    }

    fn try_init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let raw: serde_json::Value = ctx.config_or_default()?;
        let raw = if raw.is_null() {
            serde_json::json!({})
        } else {
            raw
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-load-config
        let loaded = match MiniChatConfig::load(&raw) {
            Ok(loaded) => loaded,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-if-config-invalid
                // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-config-error
                return Err(anyhow::Error::new(error));
                // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-config-error
                // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-if-config-invalid
            }
        };
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-load-config

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-create-gateways
        let policy = Arc::new(ModelPolicyGateway::new(
            &ctx.client_hub(),
            loaded.config.vendor.clone(),
        ));
        let audit = Arc::new(AuditGateway::new(
            &ctx.client_hub(),
            loaded.config.vendor.clone(),
        ));
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-create-gateways

        let pdp: Arc<dyn AuthZResolverApi> = required(ctx, "authz_resolver")?;
        let oagw: Arc<dyn ServiceGatewayClientV1> = required(ctx, "oagw")?;
        let s2s: Arc<dyn AuthNResolverClient> = required(ctx, "authn_resolver")?;
        let db = Arc::new(DBProvider::<DomainError>::new(ctx.db_required()?.db()));

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-resolver
        let resolver = Arc::new(ProviderResolver::new(&loaded.providers));
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-resolver
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-register-metrics
        let metrics = Arc::new(MiniChatMetrics::from_global(&loaded.config.metrics.prefix));
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-register-metrics
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-enqueuer
        let enqueuer = Arc::new(OutboxEnqueuer::new(&loaded.config.outbox));
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-enqueuer
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-services
        let authz_service = Arc::new(AuthzService::new(pdp));
        let models = Arc::new(ModelsService::new(
            Arc::clone(&authz_service),
            Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
        ));
        let chats = Arc::new(ChatsService::new(
            Arc::clone(&authz_service),
            Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            Arc::new(SeaChatRepository::new(
                Arc::clone(&db),
                Arc::clone(&enqueuer),
            )),
        ));
        let messages = Arc::new(MessagesService::new(
            Arc::clone(&authz_service),
            Arc::new(SeaMessageRepository::new(Arc::clone(&db))),
        ));
        let quota_repo = Arc::new(SeaQuotaRepository::new(
            Arc::clone(&db),
            Arc::clone(&enqueuer),
            loaded.config.quota.overshoot_tolerance_factor,
        ));
        let quota = Arc::new(QuotaService::new(QuotaDeps {
            authz: Arc::clone(&authz_service),
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            repo: Arc::clone(&quota_repo) as Arc<dyn QuotaRepository>,
            metrics: Arc::clone(&metrics) as Arc<dyn QuotaMetrics>,
            config: QuotaServiceConfig::from_config(&loaded.config),
        }));
        let turn_repo: Arc<dyn TurnRepository> = Arc::new(SeaTurnRepository::new(
            Arc::clone(&db),
            quota_repo,
            Arc::clone(&enqueuer),
        ));
        let streams = Arc::new(StreamService::new(StreamDeps {
            authz: Arc::clone(&authz_service),
            chats: Arc::new(SeaChatRepository::new(
                Arc::clone(&db),
                Arc::clone(&enqueuer),
            )),
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota),
            turns: Arc::clone(&turn_repo),
            providers: Arc::clone(&resolver) as Arc<dyn ProviderDirectory>,
            client: Arc::new(OagwProviderClient::new(Arc::clone(&oagw))) as Arc<dyn ProviderClient>,
            retriever: knowledge_retriever(&loaded.config, &oagw, &resolver),
            metrics: Arc::clone(&metrics) as Arc<dyn StreamMetrics>,
            settings: StreamSettings::from_config(&loaded.config),
            clock: Arc::new(crate::domain::clock::now_micros),
        }));
        let mutations = Arc::new(MutationService::new(MutationDeps {
            authz: Arc::clone(&authz_service),
            chats: Arc::new(SeaChatRepository::new(
                Arc::clone(&db),
                Arc::clone(&enqueuer),
            )),
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota),
            turns: turn_repo,
            mutations: Arc::new(SeaMutationRepository::new(
                Arc::clone(&db),
                Arc::clone(&enqueuer),
            )),
            stream: Arc::clone(&streams),
            metrics: Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
            clock: Arc::new(crate::domain::clock::now_micros),
        }));
        let attachment_repo: Arc<dyn AttachmentRepository> = Arc::new(
            SeaAttachmentRepository::new(Arc::clone(&db), Arc::clone(&enqueuer)),
        );
        let storage: Arc<dyn StorageClient> = Arc::new(OagwStorageClient::new(Arc::clone(&oagw)));
        let runtime = Arc::new(GearRuntime::new());
        let attachments = Arc::new(AttachmentsService::new(AttachmentsDeps {
            authz: Arc::clone(&authz_service),
            chats: Arc::new(SeaChatRepository::new(
                Arc::clone(&db),
                Arc::clone(&enqueuer),
            )),
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            directory: Arc::clone(&resolver) as Arc<dyn StorageDirectory>,
            repo: Arc::clone(&attachment_repo),
            storage: Arc::clone(&storage),
            metrics: Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>,
            runtime: Arc::clone(&runtime) as Arc<dyn BackgroundRuntime>,
            settings: AttachmentSettings::from_config(&loaded.config),
            clock: Arc::new(crate::domain::clock::now_micros),
        }));
        let services = Arc::new(AppServices {
            db,
            authz: authz_service,
            policy,
            audit,
            providers: resolver,
            outbox: enqueuer,
            metrics,
            config: Arc::new(loaded.config),
            oagw,
            models,
            chats,
            messages,
            quota,
            streams,
            mutations,
            attachments,
            runtime,
            attachment_repo,
            storage,
        });
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-build-services

        self.wired
            .set(Arc::new(Wired {
                services,
                providers: loaded.providers,
                authn: s2s,
            }))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))
    }

    async fn try_start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let wired = self
            .wired
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} gear started before init", Self::MODULE_NAME))?;
        let config = &wired.services.config;

        let tasks = Arc::new(BackgroundTasks::new(&cancel));
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-prepare-elector
        let elector = start_elector(config, &tasks)
            .await
            .map_err(anyhow::Error::new)?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-prepare-elector
        if let Ok(mut slot) = self.elector.lock() {
            slot.clone_from(&elector);
        }
        let result = self.start_services(wired, &tasks, elector).await;
        if result.is_err() {
            tasks.cancel_and_join(TASK_JOIN_TIMEOUT).await;
        }
        result
    }

    async fn start_services(
        &self,
        wired: &Arc<Wired>,
        tasks: &Arc<BackgroundTasks>,
        elector: Option<Arc<dyn LeaderElector>>,
    ) -> anyhow::Result<()> {
        let services = &wired.services;
        let config = &services.config;

        let provisioner = Arc::new(
            OagwProvisioner::new(
                Arc::clone(&services.oagw),
                Arc::clone(&wired.authn),
                config.client_credentials.clone(),
            )
            .with_timeouts(self.timeouts),
        );
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-provision-oagw
        let provision_result = provisioner.provision(&wired.providers).await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-provision-oagw
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-provision-failed
        let outcome = match provision_result {
            Ok(outcome) => outcome,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-return-provision-error
                return Err(anyhow::Error::new(error));
                // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-return-provision-error
            }
        };
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-provision-failed

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-start-outbox
        // The runtime (task registry and S2S context) is bound before the
        // pipeline can deliver: every delivery spends an outbox attempt, so a
        // handler that met an unbound runtime would burn the attempt budget of
        // the first messages (B7B-001).
        services.runtime.bind(tasks, outcome.s2s_context.clone());
        let timer: SharedWorkerClock = Arc::new(TokioWorkerClock::new());
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-register-chat-cleanup-handler
        let chat_cleanup = Arc::new(ChatCleanupHandler::new(ChatCleanupService::new(
            ChatCleanupDeps {
                store: Arc::new(SeaChatCleanupStore::new(Arc::clone(&services.db))),
                storage: Arc::clone(&services.storage),
                directory: Arc::clone(&services.providers) as Arc<dyn StorageDirectory>,
                policy: Arc::clone(&services.policy) as Arc<dyn PolicyCatalog>,
                providers: Arc::clone(&services.providers) as Arc<dyn ProviderDirectory>,
                metrics: Arc::clone(&services.metrics) as Arc<dyn WorkerMetrics>,
                runtime: Arc::clone(&services.runtime) as Arc<dyn BackgroundRuntime>,
                clock: Arc::new(crate::domain::clock::now_micros),
                timer: Arc::clone(&timer),
                max_attempts: config.cleanup_worker.max_attempts,
                delete_timeout: PROVIDER_DELETE_TIMEOUT,
                delivery_budget: CHAT_CLEANUP_DELIVERY_BUDGET,
            },
        )));
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-register-chat-cleanup-handler
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-register-summary-handler
        let thread_summary = Arc::new(ThreadSummaryHandler::new(SummaryService::new(
            SummaryDeps {
                store: Arc::new(SeaSummaryStore::new(
                    Arc::clone(&services.db),
                    Arc::clone(&services.outbox),
                )) as Arc<dyn SummaryStore>,
                policy: Arc::clone(&services.policy) as Arc<dyn PolicyCatalog>,
                providers: Arc::clone(&services.providers) as Arc<dyn ProviderDirectory>,
                client: Arc::new(OagwSummaryClient::new(Arc::clone(&services.oagw)))
                    as Arc<dyn SummaryModelClient>,
                metrics: Arc::clone(&services.metrics) as Arc<dyn WorkerMetrics>,
                runtime: Arc::clone(&services.runtime) as Arc<dyn BackgroundRuntime>,
                clock: Arc::new(crate::domain::clock::now_micros),
                timer,
                config: config.thread_summary_worker.clone(),
            },
        )));
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-register-summary-handler
        let handlers = OutboxHandlers {
            usage: Arc::new(UsageHandler::new(Arc::clone(&services.policy))),
            audit: Arc::new(AuditHandler::new(
                Arc::clone(&services.audit),
                Arc::clone(&services.metrics) as Arc<dyn AuditEmitMetrics>,
            )),
            attachment_cleanup: Some(Arc::new(AttachmentCleanupHandler::new(
                CleanupHandlerDeps {
                    repo: Arc::clone(&services.attachment_repo),
                    storage: Arc::clone(&services.storage),
                    directory: Arc::clone(&services.providers) as Arc<dyn StorageDirectory>,
                    metrics: Arc::clone(&services.metrics) as Arc<dyn AttachmentMetrics>,
                    runtime: Arc::clone(&services.runtime) as Arc<dyn BackgroundRuntime>,
                    max_attempts: config.cleanup_worker.max_attempts,
                    clock: Arc::new(crate::domain::clock::now_micros),
                    delete_timeout: PROVIDER_DELETE_TIMEOUT,
                },
            ))),
            chat_cleanup: Some(chat_cleanup),
            thread_summary: Some(thread_summary),
        };
        let handle = start_outbox(
            &services.db.db(),
            &config.outbox,
            &config.thread_summary_worker,
            handlers,
        )
        .await?;
        services.outbox.bind(handle.outbox());
        *self.outbox.lock().await = Some(handle);
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-start-outbox

        check_summary_model(
            &services.policy,
            outcome.s2s_context.subject_id(),
            &config.thread_summary_worker.summary_model_id,
        )
        .await;

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-spawn-workers
        spawn_leader_workers(tasks, elector.as_ref(), services);
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-spawn-workers
        let deferred: Vec<ProvisionTarget> = outcome.deferred().into_iter().cloned().collect();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-if-deferred
        if !deferred.is_empty() {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-spawn-reconcile
            let token = tasks.token();
            let retry: Arc<dyn TargetRetry> = provisioner;
            let s2s = outcome.s2s_context.clone();
            tasks.spawn("provider-reconcile", async move {
                reconcile_providers(retry, s2s, deferred, token, ReconcileSchedule::default())
                    .await;
            });
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-spawn-reconcile
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-if-deferred
        if let Ok(mut slot) = self.tasks.lock() {
            *slot = Some(Arc::clone(tasks));
        }
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-return-started
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-return-started
    }
}

/// Spawns the leader-only workers after the outbox pipeline: the upload reaper
/// and the orphan watchdog (each runs only while `elector` reports the
/// leadership of its role).
fn spawn_leader_workers(
    tasks: &BackgroundTasks,
    elector: Option<&Arc<dyn LeaderElector>>,
    services: &AppServices,
) {
    let Some(elector) = elector else {
        return;
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-spawn-reaper
    if services.config.upload_reaper.enabled {
        let reaper = Arc::new(UploadReaper::new(ReaperDeps {
            repo: Arc::clone(&services.attachment_repo),
            elector: Arc::clone(elector),
            metrics: Arc::clone(&services.metrics) as Arc<dyn AttachmentMetrics>,
            config: services.config.upload_reaper.clone(),
            clock: Arc::new(crate::domain::clock::now_micros),
        }));
        let stop = tasks.token();
        tasks.spawn(UPLOAD_REAPER_TASK, async move { reaper.run(stop).await });
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-spawn-reaper
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-spawn-watchdog
    if services.config.orphan_watchdog.enabled {
        let quota_repo = Arc::new(SeaQuotaRepository::new(
            Arc::clone(&services.db),
            Arc::clone(&services.outbox),
            services.config.quota.overshoot_tolerance_factor,
        ));
        let watchdog = Arc::new(OrphanWatchdog::new(WatchdogDeps {
            store: Arc::new(SeaOrphanStore::new(
                Arc::clone(&services.db),
                quota_repo,
                Arc::clone(&services.outbox),
            )),
            quota: Arc::clone(&services.quota),
            elector: Arc::clone(elector),
            metrics: Arc::clone(&services.metrics) as Arc<dyn WorkerMetrics>,
            config: services.config.orphan_watchdog.clone(),
            clock: Arc::new(crate::domain::clock::now_micros),
        }));
        let stop = tasks.token();
        tasks.spawn(
            ORPHAN_WATCHDOG_TASK,
            async move { watchdog.run(stop).await },
        );
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-spawn-watchdog
}

#[async_trait]
impl Gear for MiniChatGear {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-call-init
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let result = self.try_init(ctx);
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-initialized
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-init-failed
        self.set_state(if result.is_ok() {
            GearLifecycleState::Initialized
        } else {
            GearLifecycleState::InitFailed
        });
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-init-failed
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-initialized
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-initialized
        result
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-return-initialized
    }
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-call-init
}

impl toolkit::contracts::DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-apply-migrations
        crate::infra::db::migrations::Migrator::migrations()
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-apply-migrations
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let wired = self
            .wired
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} gear is not initialized", Self::MODULE_NAME))?;
        let url_prefix = &wired.services.config.url_prefix;
        let router = crate::api::rest::routes::register_routes(
            router,
            openapi,
            Arc::clone(&wired.services.models),
            url_prefix,
        );
        let router = crate::api::rest::routes::register_chat_routes(
            router,
            openapi,
            Arc::clone(&wired.services.chats),
            Arc::clone(&wired.services.messages),
            url_prefix,
        );
        let router = crate::api::rest::routes::register_quota_routes(
            router,
            openapi,
            Arc::clone(&wired.services.quota),
            url_prefix,
        );
        let router = crate::api::rest::routes::register_stream_routes(
            router,
            openapi,
            Arc::clone(&wired.services.streams),
            url_prefix,
        );
        let router = crate::api::rest::routes::register_turn_mutation_routes(
            router,
            openapi,
            Arc::clone(&wired.services.mutations),
            url_prefix,
        );
        Ok(crate::api::rest::routes::register_attachment_routes(
            router,
            openapi,
            Arc::clone(&wired.services.attachments),
            url_prefix,
        ))
    }
}

#[async_trait]
impl toolkit::contracts::RunnableCapability for MiniChatGear {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-call-start
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let result = self.try_start(cancel).await;
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-started
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-start-failed
        self.set_state(if result.is_ok() {
            GearLifecycleState::Started
        } else {
            GearLifecycleState::StartFailed
        });
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-start-failed
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-started
        result
    }
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-call-start

    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-call-stop
    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        let tasks = self.tasks.lock().ok().and_then(|mut slot| slot.take());
        if let Some(tasks) = tasks {
            tokio::select! {
                biased;
                () = deadline.cancelled() => tracing::warn!("stop deadline fired before the background tasks ended"),
                _ = tasks.cancel_and_join(TASK_JOIN_TIMEOUT) => {}
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-stop-outbox
        let outbox = self.outbox.lock().await.take();
        if let Some(handle) = outbox {
            tokio::select! {
                biased;
                () = deadline.cancelled() => tracing::warn!("stop deadline fired before the outbox pipeline stopped"),
                () = handle.stop() => {}
            }
        }
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-stop-outbox
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-stopped
        if self.lifecycle_state() == GearLifecycleState::Started {
            self.set_state(GearLifecycleState::Stopped);
        }
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle:p1:inst-to-stopped
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-return-stopped
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-return-stopped
    }
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-call-stop
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "gear_chats_tests.rs"]
mod chats_tests;

#[cfg(test)]
#[path = "gear_attachments_tests.rs"]
mod attachments_tests;

#[cfg(test)]
#[path = "gear_mutations_tests.rs"]
mod mutations_tests;

#[cfg(test)]
#[path = "gear_workers_tests.rs"]
mod workers_tests;
