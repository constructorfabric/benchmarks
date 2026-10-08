//! `mini-chat` gear: configuration, wiring and lifecycle (DESIGN "Gear lifecycle").

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use std::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxHandle, OutboxProfile, Partitions, WorkerTuning};

use crate::config::MiniChatConfig;
use crate::domain::audit::AuditGateway;
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::domain::policy::PolicyGateway;
use crate::domain::service::Svc;
use crate::domain::workers::{LeaderElector, NoopElector, spawn_periodic};
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::gateway::Gateway;
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::metrics::Metrics;
use crate::infra::oagw_provisioning::Provisioner;
use crate::infra::outbox::attachment_cleanup::AttachmentCleanupHandler;
use crate::infra::outbox::audit::AuditHandler;
use crate::infra::outbox::chat_cleanup::ChatCleanupHandler;
use crate::infra::outbox::thread_summary::{SYSTEM_USER_ID, ThreadSummaryHandler};
use crate::infra::outbox::usage::UsageHandler;
use crate::infra::outbox::Enqueuer;

/// Lease of the audit queue (hardcoded).
const AUDIT_LEASE: Duration = Duration::from_secs(60);
/// Bounded join timeout of background workers on stop.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The `mini-chat` gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
#[derive(Default)]
pub struct MiniChatGear {
    svc: OnceLock<Arc<Svc>>,
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    outbox: Mutex<Option<OutboxHandle>>,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl MiniChatGear {
    fn svc(&self) -> anyhow::Result<Arc<Svc>> {
        self.svc.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat services are not initialized"))
    }

    /// Services (tests / embedding).
    #[must_use]
    pub fn services(&self) -> Option<Arc<Svc>> {
        self.svc.get().cloned()
    }
}

/// Builds the domain services from their dependencies.
#[must_use]
pub fn build_services(
    cfg: MiniChatConfig,
    db: Arc<DBProvider<DomainError>>,
    enforcer: PolicyEnforcer,
    policy: Arc<PolicyGateway>,
    audit: Arc<AuditGateway>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
) -> Arc<Svc> {
    let resolver = Arc::new(ProviderResolver::new(cfg.providers.clone()));
    let gateway = Arc::new(Gateway::new(oagw));
    let llm = Arc::new(LlmClient::new(gateway, resolver));
    let metrics = Arc::new(Metrics::new(cfg.metrics.effective_prefix()));
    let outbox = Arc::new(Enqueuer::new(cfg.outbox.clone()));
    let upload_sem = Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads)));
    Arc::new(Svc {
        cfg: Arc::new(cfg),
        db,
        authz: Authz::new(enforcer),
        policy,
        audit,
        llm,
        outbox,
        upload_sem,
        metrics,
        shutdown: CancellationToken::new(),
    })
}

/// Starts the outbox pipeline with the five Mini Chat queues and binds the enqueuer.
///
/// # Errors
/// Outbox start failures.
pub async fn start_outbox(svc: &Arc<Svc>, db: toolkit_db::Db) -> anyhow::Result<OutboxHandle> {
    let o = &svc.cfg.outbox;
    let partitions = Partitions::of(u16::try_from(o.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(svc.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&o.queue_name, partitions)
        .leased(UsageHandler { svc: svc.clone() })
        .queue(&o.cleanup_queue_name, partitions)
        .leased(AttachmentCleanupHandler { svc: svc.clone() })
        .queue(&o.chat_cleanup_queue_name, partitions)
        .leased(ChatCleanupHandler { svc: svc.clone() })
        .queue(&o.thread_summary_queue_name, partitions)
        .leased(ThreadSummaryHandler { svc: svc.clone() })
        .lease(LeaseConfig { duration: summary_lease, headroom: Duration::from_secs(5) })
        .queue(&o.audit_queue_name, partitions)
        .leased(AuditHandler { svc: svc.clone() })
        .lease(LeaseConfig { duration: AUDIT_LEASE, headroom: Duration::from_secs(2) })
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("mini-chat outbox start failed: {e}"))?;
    svc.outbox.bind(handle.outbox());
    Ok(handle)
}

/// Leader elector of the leader-only workers: a Kubernetes Lease elector when built with `k8s`
/// (falls back to single-process mode when the pod environment is unavailable), otherwise no-op.
#[cfg(feature = "k8s")]
async fn leader_elector(svc: &Arc<Svc>) -> Arc<dyn LeaderElector> {
    if svc.cfg.orphan_watchdog.enabled || svc.cfg.upload_reaper.enabled {
        use crate::infra::leader::{LeaseElector, ROLE_ORPHAN_WATCHDOG, ROLE_UPLOAD_REAPER};
        match LeaseElector::start(&[ROLE_ORPHAN_WATCHDOG, ROLE_UPLOAD_REAPER], svc.shutdown.clone()).await {
            Ok(e) => return e,
            Err(e) => tracing::error!(error = %e, "Kubernetes lease elector unavailable; running leader-only workers in single-process mode"),
        }
    }
    Arc::new(NoopElector)
}

/// Leader elector of the leader-only workers: no-op (single-process mode) without `k8s`.
#[cfg(not(feature = "k8s"))]
fn leader_elector() -> Arc<dyn LeaderElector> {
    Arc::new(NoopElector)
}

/// Starts the outbox, retrying transient database lock errors.
async fn start_outbox_with_retry(svc: &Arc<Svc>, db: toolkit_db::Db) -> anyhow::Result<OutboxHandle> {
    const ATTEMPTS: u32 = 10;
    let mut delay = Duration::from_millis(100);
    let mut attempt = 1;
    loop {
        match start_outbox(svc, db.clone()).await {
            Err(e) if attempt < ATTEMPTS && e.to_string().contains("locked") => {
                tracing::warn!(attempt, error = %e, "mini-chat outbox start hit a database lock; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
                attempt += 1;
            }
            res => return res,
        }
    }
}

async fn exchange_s2s(svc: &Svc, authn: &Arc<dyn AuthNResolverClient>) -> anyhow::Result<()> {
    let cc = &svc.cfg.client_credentials;
    let res = authn
        .exchange_client_credentials(&ClientCredentialsRequest {
            client_id: cc.client_id.clone(),
            client_secret: secrecy::SecretString::from(cc.client_secret.clone()),
            scopes: Vec::new(),
        })
        .await
        .map_err(|e| anyhow::anyhow!("S2S client_credentials exchange failed: {e}"))?;
    svc.llm.gateway.set_context(res.security_context);
    Ok(())
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for field in cfg.deprecated_fields_in_use() {
            tracing::warn!(field, "mini-chat config key is deprecated and has no effect");
        }
        let raw = ctx.db_required()?;
        let db = Arc::new(DBProvider::<DomainError>::new(raw.db()));
        let hub = ctx.client_hub();
        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("authz resolver client missing: {e}"))?;
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("OAGW client missing: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("authn resolver client missing: {e}"))?;
        let policy = Arc::new(PolicyGateway::new(hub.clone(), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(hub.clone(), cfg.vendor.clone()));
        let svc = build_services(cfg, db, PolicyEnforcer::new(authz_client), policy, audit, oagw);
        // The pipeline starts before any route can be served: requests that commit outbox
        // messages (turn finalization, deletions) must never see an unbound enqueuer, and the
        // queue registration must not race request transactions on SQLite.
        let handle = start_outbox_with_retry(&svc, raw.db()).await?;
        *lock(&self.outbox) = Some(handle);
        if self.authn.set(authn).is_err() {
            tracing::debug!("mini-chat authn client already set");
        }
        self.svc.set(svc).map_err(|_| anyhow::anyhow!("mini-chat already initialized"))?;
        tracing::info!("mini-chat gear initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        crate::infra::db::migrations::all_migrations()
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
        Ok(crate::api::rest::register_routes(router, openapi, svc))
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, _cancel: CancellationToken) -> anyhow::Result<()> {
        let svc = self.svc()?;
        let authn = self.authn.get().cloned().ok_or_else(|| anyhow::anyhow!("authn client missing"))?;
        #[cfg(feature = "k8s")]
        let elector = leader_elector(&svc).await;
        #[cfg(not(feature = "k8s"))]
        let elector = leader_elector();

        // S2S identity + OAGW provisioning (deferred entries are reconciled in the background).
        let provisioner = Arc::new(Provisioner::new(svc.llm.gateway.clone(), svc.llm.resolver.clone()));
        match exchange_s2s(&svc, &authn).await {
            Ok(()) => {
                let pending = provisioner.provision_all().await;
                provisioner.clone().spawn_reconcile(pending, svc.shutdown.clone());
            }
            Err(e) => {
                tracing::warn!(error = %e, "S2S exchange failed; retrying in the background");
                let svc2 = svc.clone();
                let cancel = svc.shutdown.clone();
                tokio::spawn(async move {
                    let mut delay = Duration::from_secs(1);
                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => return,
                            () = tokio::time::sleep(delay) => {}
                        }
                        if exchange_s2s(&svc2, &authn).await.is_ok() {
                            let pending = provisioner.provision_all().await;
                            provisioner.spawn_reconcile(pending, cancel);
                            return;
                        }
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                });
            }
        }

        // Leader-only workers.
        let mut workers = Vec::new();
        if svc.cfg.orphan_watchdog.enabled {
            let s = svc.clone();
            workers.push(spawn_periodic(
                crate::infra::leader::ROLE_ORPHAN_WATCHDOG,
                Duration::from_secs(svc.cfg.orphan_watchdog.scan_interval_secs),
                elector.clone(),
                svc.shutdown.clone(),
                move || {
                    let s = s.clone();
                    async move {
                        if let Err(e) = s.orphan_scan().await {
                            tracing::warn!(error = %e, "orphan watchdog scan failed");
                        }
                    }
                },
            ));
        }
        if svc.cfg.upload_reaper.enabled {
            let s = svc.clone();
            workers.push(spawn_periodic(
                crate::infra::leader::ROLE_UPLOAD_REAPER,
                Duration::from_secs(svc.cfg.upload_reaper.scan_interval_secs),
                elector,
                svc.shutdown.clone(),
                move || {
                    let s = s.clone();
                    async move {
                        if let Err(e) = s.upload_reaper_scan().await {
                            tracing::warn!(error = %e, "upload reaper scan failed");
                        }
                    }
                },
            ));
        }
        *lock(&self.workers) = workers;

        // Summary model check (startup continues on failure).
        if svc.cfg.thread_summary_worker.enabled {
            let s = svc.clone();
            tokio::spawn(async move {
                let id = s.cfg.thread_summary_worker.effective_model_id().to_owned();
                match s.policy.current_snapshot(SYSTEM_USER_ID).await {
                    Ok(snap) if snap.enabled_model(&id).is_some() => {}
                    Ok(_) => tracing::error!(model = %id, "thread summary model is missing or disabled in the catalog"),
                    Err(e) => tracing::warn!(error = %e, "thread summary model check skipped: policy unavailable"),
                }
            });
        }
        tracing::info!("mini-chat gear started");
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(svc) = self.svc.get() {
            svc.shutdown.cancel();
        }
        let workers: Vec<_> = std::mem::take(&mut *lock(&self.workers));
        for w in workers {
            tokio::select! {
                _ = tokio::time::timeout(WORKER_JOIN_TIMEOUT, w) => {}
                () = deadline.cancelled() => break,
            }
        }
        let handle = lock(&self.outbox).take();
        if let Some(h) = handle {
            tokio::select! {
                () = h.stop() => {}
                () = deadline.cancelled() => {}
            }
        }
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
