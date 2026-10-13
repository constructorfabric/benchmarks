//! `mini-chat` gear declaration and lifecycle.
//!
//! - `init`: parse + validate `gears.mini-chat.config`, register the two plugin
//!   GTS schemas in types-registry, build the policy and audit gateways (lazy
//!   plugin resolution) and the REST service container.
//! - `register_rest`: register the REST routes (`{url_prefix}/v1/...`).
//! - `start` (stateful): exchange `client_credentials` for the S2S security
//!   context and provision the OAGW upstreams and routes. A deterministic
//!   misconfiguration (rejected credentials, an entry OAGW rejects) fails
//!   startup; entries that cannot be provisioned yet (credstore secret not
//!   readable, authn plugin not ready) are retried by a background loop.
//!   Then start the outbox pipeline (five leased queues with the real
//!   handlers) and install it on the services' enqueuer, check that the
//!   thread-summary model is in the catalog (error log only), then spawn the
//!   orphan watchdog and the upload reaper (when enabled; leader elector of
//!   the build: no-op without the `k8s` feature).
//! - `stop`: cancel the background work and join it with a bounded timeout,
//!   then stop the outbox pipeline (or give up at the framework deadline).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};
use oagw_sdk::ServiceGatewayClientV1;
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
use crate::domain::clock::SystemClock;
use crate::domain::ports::{AuditPort, PolicyPort};
use crate::domain::services::{AppDeps, AppServices, IndexingTimings};
use crate::infra::gateways::audit_gateway::AuditGateway;
use crate::infra::gateways::policy_gateway::PolicyGateway;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::oagw_provisioning::{self, UpstreamSpec};
use crate::infra::outbox::enqueuer::OutboxEnqueuer;
use crate::infra::outbox::{HandlerDeps, start_pipeline};
use crate::infra::s2s::{S2sContextProvider, exchange_error_is_fatal};
use crate::infra::workers::leader::default_elector;
use crate::infra::workers::orphan_watchdog::OrphanWatchdog;
use crate::infra::workers::upload_reaper::UploadReaper;

/// Background work started in `start`: its cancel token and tasks.
type Background = (CancellationToken, Vec<JoinHandle<()>>);

/// Bound on joining the background tasks at stop.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Mini Chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
#[derive(Default)]
pub struct MiniChatGear {
    config: OnceLock<Arc<MiniChatConfig>>,
    policy: OnceLock<Arc<dyn PolicyPort>>,
    audit: OnceLock<Arc<dyn AuditPort>>,
    services: OnceLock<Arc<AppServices>>,
    /// Outbox enqueuer of the services (installed on the pipeline at start).
    enqueuer: OnceLock<Arc<OutboxEnqueuer>>,
    oagw: OnceLock<Arc<dyn ServiceGatewayClientV1>>,
    s2s: OnceLock<Arc<S2sContextProvider>>,
    /// Background work started in `start` (cancel token + tasks).
    background: Mutex<Option<Background>>,
    /// Outbox pipeline started in `start`, stopped in `stop`.
    outbox: Mutex<Option<OutboxHandle>>,
    /// Shutdown token handed to the services: request-spawned background
    /// tasks (upload indexing) run on child tokens. Cancelled at stop.
    shutdown: CancellationToken,
}

impl MiniChatGear {
    /// Validated configuration (after `init`).
    #[must_use]
    pub fn config(&self) -> Option<&Arc<MiniChatConfig>> {
        self.config.get()
    }

    /// Policy gateway (after `init`).
    #[must_use]
    pub fn policy(&self) -> Option<&Arc<dyn PolicyPort>> {
        self.policy.get()
    }

    /// Audit gateway (after `init`).
    #[must_use]
    pub fn audit(&self) -> Option<&Arc<dyn AuditPort>> {
        self.audit.get()
    }

    fn not_initialized() -> anyhow::Error {
        anyhow::anyhow!("{} gear not initialized", Self::MODULE_NAME)
    }

    /// S2S exchange + OAGW provisioning; returns the specs still pending.
    async fn provision_providers(
        config: &MiniChatConfig,
        oagw: &dyn ServiceGatewayClientV1,
        s2s: &S2sContextProvider,
    ) -> anyhow::Result<Vec<UpstreamSpec>> {
        let specs = oagw_provisioning::plan(&config.providers);
        let ctx = match s2s.exchange().await {
            Ok(ctx) => ctx,
            Err(e) if exchange_error_is_fatal(&e) => {
                anyhow::bail!("mini-chat S2S client credentials were rejected: {e}")
            }
            Err(e) => {
                warn!(error = %e, "S2S credentials exchange failed; OAGW provisioning deferred");
                return Ok(specs);
            }
        };
        let report = oagw_provisioning::provision(oagw, &ctx, &specs)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat OAGW provisioning failed: {e:#}"))?;
        info!(provisioned = ?report.ok, deferred = ?report.deferred, "OAGW provisioning done");
        Ok(specs
            .into_iter()
            .filter(|s| report.deferred.contains(&s.label))
            .collect())
    }

    fn already_initialized() -> anyhow::Error {
        anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME)
    }

    /// Start the outbox pipeline with the real handlers and install it on
    /// the services' enqueuer.
    async fn start_outbox(&self) -> anyhow::Result<()> {
        let (Some(config), Some(services), Some(policy), Some(audit), Some(enqueuer)) = (
            self.config.get(),
            self.services.get(),
            self.policy.get(),
            self.audit.get(),
            self.enqueuer.get(),
        ) else {
            return Err(Self::not_initialized());
        };
        let deps = HandlerDeps {
            policy: Arc::clone(policy),
            audit: Arc::clone(audit),
            cleanup: Arc::clone(&services.cleanup),
            thread_summary: services.summaries.clone(),
            metrics: Arc::clone(&services.metrics),
        };
        let handle = start_pipeline(services.db.db(), config, deps).await?;
        if let Err(e) = enqueuer.set_outbox(Arc::clone(handle.outbox())) {
            handle.stop().await;
            return Err(anyhow::anyhow!("mini-chat outbox: {e}"));
        }
        *self.outbox.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        info!("mini-chat outbox pipeline started");
        Ok(())
    }
}

impl MiniChatGear {
    /// Spawn the enabled periodic workers (orphan watchdog, upload reaper)
    /// under the build's leader elector; they stop when `token` fires.
    fn spawn_workers(services: &AppServices, token: &CancellationToken) -> Vec<JoinHandle<()>> {
        let elector = default_elector();
        let mut tasks = Vec::new();
        if services.config.orphan_watchdog.enabled {
            let watchdog = Arc::new(OrphanWatchdog::new(services));
            tasks.push(watchdog.spawn(Arc::clone(&elector), token.clone()));
        } else {
            info!("mini-chat orphan watchdog disabled");
        }
        if services.config.upload_reaper.enabled {
            let reaper = Arc::new(UploadReaper::new(services));
            tasks.push(reaper.spawn(elector, token.clone()));
        } else {
            info!("mini-chat upload reaper disabled");
        }
        tasks
    }
}

/// Register the plugin GTS type schemas (idempotent: identical content is
/// accepted again; the toolkit-gts inventory seeds the same documents).
async fn register_plugin_schemas(registry: &dyn TypesRegistryClient) -> anyhow::Result<()> {
    let schemas = [
        MiniChatModelPolicyPluginSpecV1::gts_schema_with_refs_as_string(),
        MiniChatAuditPluginSpecV1::gts_schema_with_refs_as_string(),
    ]
    .iter()
    .map(|s| serde_json::from_str::<serde_json::Value>(s))
    .collect::<Result<Vec<_>, _>>()?;
    let results = registry.register(schemas).await?;
    RegisterResult::ensure_all_ok(&results)?;
    Ok(())
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx
            .config()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e:#}"))?;

        let hub = ctx.client_hub();
        let registry = hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;
        register_plugin_schemas(registry.as_ref()).await?;

        let policy: Arc<dyn PolicyPort> =
            Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit: Arc<dyn AuditPort> =
            Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));

        let providers: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
        info!(
            vendor = %cfg.vendor,
            url_prefix = %cfg.url_prefix,
            providers = ?providers,
            "mini-chat initialized"
        );

        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;
        let s2s = Arc::new(S2sContextProvider::new(
            authn,
            cfg.client_credentials.clone(),
        ));

        let config = Arc::new(cfg);
        let db = Arc::new(DBProvider::new(ctx.db_required()?.db()));
        let enqueuer = Arc::new(OutboxEnqueuer::new(config.outbox.clone()));
        let services = Arc::new(AppServices::new(AppDeps {
            config: Arc::clone(&config),
            db,
            clock: Arc::new(SystemClock),
            policy: Arc::clone(&policy),
            // Lazy: the authz-resolver client is resolved from the hub per call.
            enforcer: PolicyEnforcer::from_hub(hub),
            // Not ready until the outbox pipeline is started.
            outbox: enqueuer.clone(),
            oagw: Arc::clone(&oagw),
            s2s: Arc::clone(&s2s),
            indexing: IndexingTimings::default(),
            shutdown: self.shutdown.clone(),
            metrics: Arc::new(MiniChatMetrics::from_global(
                config.metrics.effective_prefix(),
            )),
        }));

        self.config
            .set(config)
            .map_err(|_| Self::already_initialized())?;
        self.policy
            .set(policy)
            .map_err(|_| Self::already_initialized())?;
        self.services
            .set(services)
            .map_err(|_| Self::already_initialized())?;
        self.audit
            .set(audit)
            .map_err(|_| Self::already_initialized())?;
        self.enqueuer
            .set(enqueuer)
            .map_err(|_| Self::already_initialized())?;
        self.oagw
            .set(oagw)
            .map_err(|_| Self::already_initialized())?;
        self.s2s.set(s2s).map_err(|_| Self::already_initialized())?;
        Ok(())
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let (Some(config), Some(oagw), Some(s2s)) =
            (self.config.get(), self.oagw.get(), self.s2s.get())
        else {
            return Err(Self::not_initialized());
        };
        let pending = Self::provision_providers(config, oagw.as_ref(), s2s).await?;
        self.start_outbox().await?;
        if let Some(services) = self.services.get() {
            // Logs an error when the summary model is unusable; startup continues.
            services.summaries.check_summary_model().await;
        }

        let token = cancel.child_token();
        let mut tasks = Vec::new();
        // Stopping the background work also stops request-spawned tasks.
        let (shutdown, stopped) = (self.shutdown.clone(), token.clone());
        tasks.push(tokio::spawn(async move {
            stopped.cancelled().await;
            shutdown.cancel();
        }));
        if let Some(services) = self.services.get() {
            tasks.extend(Self::spawn_workers(services, &token));
        }
        if !pending.is_empty() {
            let (oagw, s2s, token) = (Arc::clone(oagw), Arc::clone(s2s), token.clone());
            tasks.push(tokio::spawn(async move {
                oagw_provisioning::reconcile_deferred(oagw.as_ref(), &s2s, pending, token).await;
            }));
        }
        *self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((token, tasks));
        info!("mini-chat started");
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        let background = self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.shutdown.cancel();
        if let Some((token, tasks)) = background {
            token.cancel();
            let join = futures::future::join_all(tasks);
            tokio::select! {
                _ = join => {}
                () = deadline.cancelled() => warn!("mini-chat stop deadline reached; background tasks abandoned"),
                () = tokio::time::sleep(STOP_JOIN_TIMEOUT) => warn!("mini-chat background tasks did not stop in time"),
            }
        }
        let outbox = self
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = outbox {
            tokio::select! {
                () = handle.stop() => info!("mini-chat outbox pipeline stopped"),
                () = deadline.cancelled() => warn!("mini-chat stop deadline reached; outbox pipeline abandoned"),
            }
        }
        info!("mini-chat stopped");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    /// Mini-chat schema followed by the shared outbox tables (default
    /// `toolkit_outbox_*` prefix).
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut all = crate::infra::db::migrations::Migrator::migrations();
        all.extend(toolkit_db::outbox::outbox_migrations());
        all
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let services = self.services.get().ok_or_else(Self::not_initialized)?;
        Ok(crate::api::rest::routes::register_routes(
            router,
            openapi,
            Arc::clone(services),
        ))
    }
}
