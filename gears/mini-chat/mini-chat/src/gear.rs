//! Gear declaration: configuration, wiring, REST registration and the
//! `serve` lifecycle (OAGW provisioning, outbox pipeline, background workers).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::pep::PolicyEnforcer;
use oagw_sdk::ServiceGatewayClientV1;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::{
    LeaseConfig, Outbox, OutboxHandle, OutboxProfile, Partitions, WorkerTuning,
};
use tracing::{info, warn};

use crate::config::MiniChatConfig;
use crate::domain::service::Services;
use crate::domain::workers::{
    AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, ThreadSummaryHandler, UsageHandler,
    periodic,
};
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::provisioning;
use crate::infra::llm::registry::ProviderRegistry;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::{OutboxBridge, Queue};
use crate::infra::plugins_gateway::{AuditGateway, PolicyGateway};

/// Audit queue lease.
const AUDIT_LEASE: Duration = Duration::from_secs(60);
/// Bound on joining background workers at stop.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Mini Chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    services: OnceLock<Arc<Services>>,
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    outbox: Mutex<Option<OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            services: OnceLock::new(),
            authn: OnceLock::new(),
            outbox: Mutex::new(None),
        }
    }
}

/// Load, expand and validate the gear configuration.
///
/// # Errors
/// Unknown keys, failed `${VAR}` expansion or an invalid value.
pub fn load_config(ctx: &GearCtx) -> anyhow::Result<MiniChatConfig> {
    let mut cfg: MiniChatConfig = ctx
        .config_or_default()
        .map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))?;
    cfg.expand_vars()
        .map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))?;
    cfg.fill_default_aliases();
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))?;
    for w in cfg.deprecation_warnings() {
        warn!("mini-chat config: {w}");
    }
    Ok(cfg)
}

impl MiniChatGear {
    fn svc(&self) -> anyhow::Result<Arc<Services>> {
        self.services
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat gear not initialized"))
    }

    async fn s2s_context(
        &self,
        svc: &Services,
    ) -> anyhow::Result<toolkit_security::SecurityContext> {
        let authn = self
            .authn
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat gear not initialized"))?;
        let creds = &svc.cfg.client_credentials;
        let req = ClientCredentialsRequest {
            client_id: creds.client_id.clone(),
            client_secret: creds.client_secret.clone().into(),
            scopes: Vec::new(),
        };
        let res = authn
            .exchange_client_credentials(&req)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat: client_credentials exchange failed: {e}"))?;
        Ok(res.security_context)
    }

    async fn start_outbox(&self, svc: &Arc<Services>) -> anyhow::Result<()> {
        let cfg = &svc.cfg;
        let parts = u16::try_from(cfg.outbox.num_partitions)
            .map_err(|_| anyhow::anyhow!("outbox.num_partitions out of range"))?;
        let parts = Partitions::of(parts);
        let bridge = &svc.outbox;
        let summary_lease =
            Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs.max(3));
        let handle = Outbox::builder(svc.db.db())
            .profile(OutboxProfile::low_latency())
            .processor_tuning(
                WorkerTuning::processor_low_latency()
                    .batch_size(1)
                    .retry_max(Duration::from_secs(30)),
            )
            .processors(6)
            .queue(bridge.queue_name(Queue::Usage), parts)
            .leased(UsageHandler(Arc::clone(svc)))
            .queue(bridge.queue_name(Queue::AttachmentCleanup), parts)
            .leased(AttachmentCleanupHandler(Arc::clone(svc)))
            .queue(bridge.queue_name(Queue::ChatCleanup), parts)
            .leased(ChatCleanupHandler(Arc::clone(svc)))
            .queue(bridge.queue_name(Queue::ThreadSummary), parts)
            .leased(ThreadSummaryHandler(Arc::clone(svc)))
            .lease(LeaseConfig {
                duration: summary_lease,
                headroom: Duration::from_secs(2),
            })
            .queue(bridge.queue_name(Queue::Audit), parts)
            .leased(AuditHandler(Arc::clone(svc)))
            .lease(LeaseConfig {
                duration: AUDIT_LEASE,
                headroom: Duration::from_secs(2),
            })
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat: outbox start failed: {e}"))?;
        bridge.bind(Arc::clone(handle.outbox()));
        *self.outbox.lock().await = Some(handle);
        Ok(())
    }

    /// Lifecycle entry: provision, start the pipeline and workers, then wait
    /// for cancellation and drain.
    #[allow(clippy::cognitive_complexity)]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let svc = self.svc()?;
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();

        // S2S context and OAGW provisioning.
        let s2s = self.s2s_context(&svc).await?;
        svc.llm.set_s2s(s2s.clone());
        let specs = provisioning::specs(svc.providers.entries());
        let deferred = provisioning::provision_all(&svc.llm, &svc.providers, &s2s, specs).await;
        if !deferred.is_empty() {
            tasks.push(tokio::spawn(provisioning::reconcile_loop(
                Arc::clone(&svc.llm),
                Arc::clone(&svc.providers),
                s2s,
                deferred,
                svc.shutdown.clone(),
            )));
        }

        self.start_outbox(&svc).await?;

        let wd = &svc.cfg.orphan_watchdog;
        if wd.enabled {
            let s = Arc::clone(&svc);
            tasks.push(tokio::spawn(periodic(
                Duration::from_secs(wd.scan_interval_secs.max(1)),
                svc.shutdown.clone(),
                move || {
                    let s = Arc::clone(&s);
                    async move { s.orphan_scan().await }
                },
            )));
        }
        let reaper = &svc.cfg.upload_reaper;
        if reaper.enabled {
            let s = Arc::clone(&svc);
            tasks.push(tokio::spawn(periodic(
                Duration::from_secs(reaper.scan_interval_secs.max(1)),
                svc.shutdown.clone(),
                move || {
                    let s = Arc::clone(&s);
                    async move { s.reap_uploads().await }
                },
            )));
        }

        info!(url_prefix = %svc.cfg.url_prefix, "mini-chat started");
        ready.notify();
        cancel.cancelled().await;

        info!("mini-chat stopping");
        svc.shutdown.cancel();
        let join = futures::future::join_all(tasks);
        if tokio::time::timeout(WORKER_JOIN_TIMEOUT, join)
            .await
            .is_err()
        {
            warn!("mini-chat background workers did not stop in time");
        }
        let handle = self.outbox.lock().await.take();
        if let Some(h) = handle {
            h.stop().await;
        }
        info!("mini-chat stopped");
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = Arc::new(load_config(ctx)?);
        let hub = ctx.client_hub();

        let db = ctx.db_required()?;
        let db: Arc<crate::infra::db::Db> = Arc::new(DBProvider::new(db.db()));

        let authz_api = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("mini-chat: authz_resolver client missing: {e}"))?;
        let authn_client = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("mini-chat: authn_resolver client missing: {e}"))?;
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("mini-chat: oagw client missing: {e}"))?;

        let providers = Arc::new(ProviderRegistry::new(cfg.providers.clone()));
        let summary_model = cfg.thread_summary_worker.effective_summary_model_id();
        if cfg.thread_summary_worker.enabled && summary_model.is_empty() {
            warn!("mini-chat: thread summary model is empty");
        }

        let policy = Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let outbox = Arc::new(OutboxBridge::new(cfg.outbox.clone()));
        let metrics = Arc::new(Metrics::new(cfg.metrics.effective_prefix()));
        let upload_slots = Arc::new(Semaphore::new(usize::from(
            cfg.rag.max_concurrent_uploads.max(1),
        )));
        let services = Arc::new(Services {
            cfg,
            db,
            enforcer: PolicyEnforcer::new(authz_api),
            policy,
            audit,
            llm: Arc::new(LlmClient::new(oagw)),
            providers,
            outbox,
            metrics,
            upload_slots,
            shutdown: CancellationToken::new(),
        });
        self.services
            .set(services)
            .map_err(|_| anyhow::anyhow!("mini-chat gear already initialized"))?;
        self.authn
            .set(authn_client)
            .map_err(|_| anyhow::anyhow!("mini-chat gear already initialized"))?;
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut m = crate::infra::db::migrations::Migrator::migrations();
        m.extend(toolkit_db::outbox::outbox_migrations());
        m
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let svc = self.svc()?;
        Ok(crate::api::rest::routes::register_routes(
            router, openapi, svc,
        ))
    }
}
