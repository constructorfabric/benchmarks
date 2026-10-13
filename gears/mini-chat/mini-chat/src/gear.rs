//! The `mini-chat` gear: declaration, init / start / stop and REST registration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::AuthZResolverApi;
use mini_chat_sdk::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;
use tracing::{info, warn};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::MiniChatConfig;
use crate::domain::attachment::IndexingTimings;
use crate::domain::error::DomainError;
use crate::domain::orphan::OrphanWatchdog;
use crate::infra::leader;
use crate::infra::oagw_provisioning;
use crate::infra::outbox::{
    self, AttachmentCleanupHandler, AuditOutboxHandler, ChatCleanupHandler, OutboxEnqueuer,
    QueueHandlers, ThreadSummaryHandler, UsageOutboxHandler,
};
use crate::infra::storage::migrations;
use crate::services::Services;

/// The mini-chat gear.
///
/// - `init` loads and validates the configuration, builds the services (plugins are resolved lazily
///   through the types registry) and registers the plugin GTS schemas;
/// - `start` starts the outbox pipeline, then provisions the OAGW upstreams and routes of the
///   configured providers;
/// - `stop` waits (bounded) for the provider tasks to finalize their turns, then stops the pipeline.
///
/// Besides the outbox pipeline (usage, audit, attachment cleanup, chat cleanup and thread-summary
/// handlers) and the OAGW reconcile task, the orphan watchdog and the upload reaper are the
/// background workers.
#[derive(Default)]
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    services: OnceLock<Arc<Services>>,
    outbox: Mutex<Option<OutboxHandle>>,
    reconcile: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
    watchdog: Mutex<Option<WorkerTasks>>,
    reaper: Mutex<Option<WorkerTasks>>,
    indexing_timings: OnceLock<IndexingTimings>,
}

/// A running leader-elected worker and its elector task.
struct WorkerTasks {
    token: CancellationToken,
    worker: JoinHandle<()>,
    elector: Option<JoinHandle<()>>,
}

impl MiniChatGear {
    /// The outbox enqueuer, once the gear is initialized (test observation hook).
    #[doc(hidden)]
    #[must_use]
    pub fn outbox_enqueuer(&self) -> Option<Arc<OutboxEnqueuer>> {
        self.services
            .get()
            .map(|services| Arc::clone(&services.outbox))
    }

    /// The orphan watchdog, once the gear is initialized (test drive hook).
    #[doc(hidden)]
    #[must_use]
    pub fn orphan_watchdog(&self) -> Option<Arc<OrphanWatchdog>> {
        self.services
            .get()
            .map(|services| Arc::clone(&services.watchdog))
    }

    /// Overrides the indexing wait timings of uploads; effective only when called before `init`
    /// (test hook: production uses the DESIGN constants).
    #[doc(hidden)]
    pub fn use_indexing_timings(&self, timings: IndexingTimings) {
        if self.indexing_timings.set(timings).is_err() {
            warn!("the indexing timings are already set; keeping the first ones");
        }
    }

    /// The services, once the gear is initialized (test hook for race scenarios that need the
    /// repositories behind them).
    #[doc(hidden)]
    #[must_use]
    pub fn services_handle(&self) -> Option<Arc<Services>> {
        self.services.get().map(Arc::clone)
    }

    fn services(&self) -> anyhow::Result<&Arc<Services>> {
        self.services
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: the gear is not initialized"))
    }
}

/// Registers the plugin GTS schemas. Types-registry also seeds them from the link-time inventory,
/// so a per-item refusal (for example "already registered") is logged, not fatal.
async fn register_plugin_schemas(registry: &dyn TypesRegistryClient) -> anyhow::Result<()> {
    let schemas = [
        MiniChatModelPolicyPluginSpecV1::gts_schema_with_refs_as_string(),
        MiniChatAuditPluginSpecV1::gts_schema_with_refs_as_string(),
    ];
    let mut entities = Vec::with_capacity(schemas.len());
    for schema in schemas {
        entities.push(serde_json::from_str(&schema)?);
    }
    for result in registry.register(entities).await? {
        if let RegisterResult::Err { gts_id, error } = result {
            warn!(
                gts_id = gts_id.as_deref().unwrap_or("<unknown>"),
                error = %error,
                "plugin schema registration refused"
            );
        }
    }
    Ok(())
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(module = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: MiniChatConfig = ctx.config_expanded()?;
        config.validate()?;
        for warning in config.deprecation_warnings() {
            warn!("{warning}");
        }
        info!(vendor = %config.vendor, url_prefix = %config.url_prefix, "initializing mini-chat");

        let db = Arc::new(DBProvider::<DomainError>::new(ctx.db_required()?.db()));
        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|err| anyhow::anyhow!("failed to get AuthZResolverApi: {err}"))?;
        let registry = hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|err| anyhow::anyhow!("failed to get TypesRegistryClient: {err}"))?;
        register_plugin_schemas(registry.as_ref()).await?;

        let timings = self.indexing_timings.get().copied().unwrap_or_default();
        let services = Arc::new(Services::build(config, db, authz, &hub, timings)?);
        self.services
            .set(services)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        info!("mini-chat initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        migrations::all_migrations()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let services = self.services()?;
        Ok(crate::api::rest::register_routes(
            router,
            openapi,
            &services.config.url_prefix,
            crate::api::rest::ApiServices {
                chats: Arc::clone(&services.chats),
                models: Arc::clone(&services.models),
                messages: Arc::clone(&services.messages),
                turns: Arc::clone(&services.turns),
                stream: Arc::clone(&services.stream),
                mutations: Arc::clone(&services.mutations),
                quota: Arc::clone(&services.quota_status),
                attachments: Arc::clone(&services.attachments),
            },
        ))
    }
}

impl MiniChatGear {
    /// Prepares the leader elector and spawns the orphan watchdog.
    async fn spawn_watchdog(
        &self,
        services: &Services,
        cancel: &CancellationToken,
    ) -> anyhow::Result<WorkerTasks> {
        let token = cancel.child_token();
        let election = leader::prepare("orphan-watchdog", token.clone()).await?;
        let watchdog = Arc::clone(&services.watchdog);
        let elector = election.elector;
        let worker_token = token.clone();
        let worker = tokio::spawn(async move { watchdog.run(elector, worker_token).await });
        Ok(WorkerTasks {
            token,
            worker,
            elector: election.task,
        })
    }

    /// Prepares the leader elector (its own Lease, `mini-chat-upload-reaper`) and spawns the upload
    /// reaper.
    async fn spawn_reaper(
        &self,
        services: &Services,
        cancel: &CancellationToken,
    ) -> anyhow::Result<WorkerTasks> {
        let token = cancel.child_token();
        let election = leader::prepare("upload-reaper", token.clone()).await?;
        let reaper = Arc::clone(&services.upload_reaper);
        let elector = election.elector;
        let worker_token = token.clone();
        let worker = tokio::spawn(async move { reaper.run(elector, worker_token).await });
        Ok(WorkerTasks {
            token,
            worker,
            elector: election.task,
        })
    }

    /// Exchanges the client credentials for an S2S context and provisions the OAGW upstreams and
    /// routes. Deterministic misconfiguration is an error; deferred targets are retried in the
    /// background.
    async fn provision_oagw(
        &self,
        services: &Services,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let authn = services
            .hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|err| anyhow::anyhow!("failed to get AuthNResolverClient: {err}"))?;
        let credentials = &services.config.client_credentials;
        let s2s = authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: credentials.client_id.clone(),
                client_secret: credentials.client_secret.clone(),
                scopes: Vec::new(),
            })
            .await
            .map_err(|err| anyhow::anyhow!("S2S authentication failed: {err}"))?
            .security_context;
        let targets = oagw_provisioning::targets_of(&services.config.providers);
        let pending =
            oagw_provisioning::provision(services.gateway.as_ref(), &s2s, targets).await?;
        if !pending.is_empty() {
            let token = cancel.child_token();
            let handle = oagw_provisioning::spawn_reconcile(
                Arc::clone(&services.gateway),
                s2s,
                pending,
                token.clone(),
            );
            *self.reconcile.lock() = Some((token, handle));
        }
        Ok(())
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let services = self.services()?;
        // The outbox is bound before anything can finalize a turn: OAGW provisioning may take a
        // while and a request must never finalize without an enqueuer.
        services.attachments.bind_shutdown(cancel.child_token());
        let handlers = QueueHandlers {
            chat_cleanup: ChatCleanupHandler::new(Arc::clone(&services.chat_cleanup)),
            attachment_cleanup: AttachmentCleanupHandler::new(Arc::clone(
                &services.attachment_cleanup,
            )),
            usage: UsageOutboxHandler::new(Arc::clone(&services.policy)),
            audit: AuditOutboxHandler::new(
                Arc::clone(&services.audit),
                Arc::clone(&services.metrics),
            ),
            thread_summary: ThreadSummaryHandler::new(
                Arc::clone(&services.summary),
                services.config.thread_summary_worker.max_attempts,
            ),
        };
        let handle = outbox::start(
            services.db.db(),
            &services.config,
            &services.outbox,
            handlers,
        )
        .await?;
        info!("mini-chat outbox pipeline started");
        services.summary.check_model().await;
        if let Err(err) = self.provision_oagw(services, &cancel).await {
            handle.stop().await;
            return Err(err);
        }
        // The watchdog and the reaper finalize through the outbox, so they start after the
        // pipeline.
        if services.config.orphan_watchdog.enabled {
            match self.spawn_watchdog(services, &cancel).await {
                Ok(tasks) => *self.watchdog.lock() = Some(tasks),
                Err(err) => {
                    handle.stop().await;
                    return Err(err);
                }
            }
        }
        if services.config.upload_reaper.enabled {
            match self.spawn_reaper(services, &cancel).await {
                Ok(tasks) => *self.reaper.lock() = Some(tasks),
                Err(err) => {
                    handle.stop().await;
                    return Err(err);
                }
            }
        }
        *self.outbox.lock() = Some(handle);
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        let reconcile = self.reconcile.lock().take();
        if let Some((token, handle)) = reconcile {
            token.cancel();
            if let Err(err) = handle.await {
                warn!(error = %err, "the OAGW reconcile task ended abnormally");
            }
        }
        let watchdog = self.watchdog.lock().take();
        stop_worker(watchdog, "orphan watchdog").await;
        let reaper = self.reaper.lock().take();
        stop_worker(reaper, "upload reaper").await;
        // The provider tasks finalize their turns through the outbox: wait for them (bounded by the
        // stop deadline) before the outbox stops.
        if let Some(services) = self.services.get() {
            tokio::select! {
                () = services.stream.wait_for_turns() => info!("mini-chat provider tasks finished"),
                () = deadline.cancelled() => warn!("mini-chat stop deadline reached; provider tasks are still running"),
            }
        }
        // Background indexing tasks enqueue their failures: they finish before the outbox stops.
        if let Some(services) = self.services.get() {
            tokio::select! {
                () = services.attachments.shutdown() => info!("mini-chat attachment tasks finished"),
                () = deadline.cancelled() => warn!("mini-chat stop deadline reached; attachment tasks are still running"),
            }
        }
        let handle = self.outbox.lock().take();
        if let Some(handle) = handle {
            tokio::select! {
                () = handle.stop() => info!("mini-chat outbox pipeline stopped"),
                () = deadline.cancelled() => warn!("mini-chat stop deadline reached; abandoning the outbox pipeline"),
            }
        }
        Ok(())
    }
}

/// Cancels a leader-elected worker and waits for it and its elector.
async fn stop_worker(tasks: Option<WorkerTasks>, name: &str) {
    let Some(tasks) = tasks else {
        return;
    };
    tasks.token.cancel();
    if let Err(err) = tasks.worker.await {
        warn!(error = %err, worker = name, "the worker ended abnormally");
    }
    if let Some(elector) = tasks.elector
        && let Err(err) = elector.await
    {
        warn!(error = %err, worker = name, "the leader elector ended abnormally");
    }
}
