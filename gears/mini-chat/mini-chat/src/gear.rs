//! `mini-chat` gear registration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::PolicyEnforcer;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, client_hub::ClientHub};
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxHandle, OutboxProfile, Partitions, WorkerTuning};

use crate::config::MiniChatConfig;
use crate::domain::errors::DomainError;
use crate::domain::state::AppState;
use crate::infra::handlers::{AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, ThreadSummaryHandler, UsageHandler};
use crate::infra::llm::client::{LlmClient, S2sContext};
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::outbox::OutboxDispatch;
use crate::infra::policy::{AuditGateway, PolicyGateway};
use crate::infra::provisioning;
use crate::infra::storage::StorageClient;

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    state: OnceLock<Arc<AppState>>,
    hub: OnceLock<Arc<ClientHub>>,
    s2s: OnceLock<Arc<S2sContext>>,
    outbox: Mutex<Option<OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            hub: OnceLock::new(),
            s2s: OnceLock::new(),
            outbox: Mutex::new(None),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn start_outbox(state: &Arc<AppState>) -> anyhow::Result<OutboxHandle> {
    let o = &state.cfg.outbox;
    let parts = Partitions::of(u16::try_from(o.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(state.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(state.db.db())
        .profile(OutboxProfile::low_latency())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&o.queue_name, parts)
        .leased(UsageHandler(Arc::clone(state)))
        .queue(&o.audit_queue_name, parts)
        .leased(AuditHandler(Arc::clone(state)))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(2),
        })
        .queue(&o.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler(Arc::clone(state)))
        .queue(&o.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler(Arc::clone(state)))
        .queue(&o.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler(Arc::clone(state)))
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(5),
        })
        .start()
        .await?;
    state.outbox.bind(handle.outbox());
    Ok(handle)
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for w in cfg.deprecation_warnings() {
            tracing::warn!("{w}");
        }
        let cfg = Arc::new(cfg);
        let hub = ctx.client_hub();
        let db_raw = ctx.db_required()?;
        let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));
        let s2s = Arc::new(S2sContext::new(
            Arc::clone(&hub),
            cfg.client_credentials.client_id.clone(),
            cfg.client_credentials.client_secret.clone(),
        ));
        let resolver = Arc::new(ProviderResolver::new(cfg.providers.clone()));
        let provisioner = Arc::new(provisioning::Provisioner::new(
            Arc::clone(&hub),
            Arc::clone(&s2s),
            Arc::clone(&resolver),
        ));
        let llm = Arc::new(LlmClient::new(Arc::clone(&hub), Arc::clone(&s2s), provisioner));
        let state = Arc::new(AppState {
            cfg: Arc::clone(&cfg),
            db,
            enforcer: PolicyEnforcer::from_hub(Arc::clone(&hub)),
            policy: Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone())),
            audit: Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone())),
            outbox: Arc::new(OutboxDispatch::new(cfg.outbox.clone())),
            resolver: Arc::clone(&resolver),
            storage: Arc::new(StorageClient::new(Arc::clone(&llm))),
            llm,
            upload_slots: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
        });

        let handle = start_outbox(&state).await?;
        *lock(&self.outbox) = Some(handle);

        self.hub
            .set(hub)
            .map_err(|_| anyhow::anyhow!("mini-chat already initialized"))?;
        let _ = self.s2s.set(s2s);
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("mini-chat already initialized"))?;
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
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat state not initialized"))?;
        let prefix = state.cfg.url_prefix.clone();
        Ok(crate::api::rest::routes::register_routes(router, openapi, state, &prefix))
    }
}

impl MiniChatGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(state) = self.state.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let mut tasks = Vec::new();

        // OAGW provisioning (deterministic misconfiguration fails startup).
        if let (Some(hub), Some(s2s)) = (self.hub.get().cloned(), self.s2s.get().cloned()) {
            let specs = provisioning::specs(&state.cfg.providers);
            let deferred = match provisioning::provision_all(&hub, &s2s, &state.resolver, specs.clone()).await {
                Ok(d) => d,
                Err(e) if e.to_string().contains("provisioning failed") => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "mini-chat provisioning deferred");
                    specs
                }
            };
            if !deferred.is_empty() {
                state.llm.provisioner().set_pending(deferred).await;
                tasks.push(tokio::spawn(Arc::clone(state.llm.provisioner()).reconcile(cancel.clone())));
            }
        }

        // Summary model availability check (non-fatal).
        if state.cfg.thread_summary_worker.enabled {
            let model = state.cfg.thread_summary_worker.effective_model_id().to_owned();
            match state.policy.current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID).await {
                Ok(snap) if snap.find_enabled_model(&model).is_some() => {}
                Ok(_) => tracing::error!(model = %model, "thread summary model is missing or disabled in the catalog"),
                Err(e) => tracing::warn!(error = %e, "could not verify the thread summary model"),
            }
        }

        if state.cfg.orphan_watchdog.enabled {
            let st = Arc::clone(&state);
            let c = cancel.clone();
            let every = Duration::from_secs(state.cfg.orphan_watchdog.scan_interval_secs.max(1));
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        () = c.cancelled() => break,
                        _ = tick.tick() => {
                            if let Err(e) = st.orphan_scan().await {
                                tracing::warn!(error = %e, "orphan watchdog scan failed");
                            }
                        }
                    }
                }
            }));
        }
        if state.cfg.upload_reaper.enabled {
            let st = Arc::clone(&state);
            let c = cancel.clone();
            let every = Duration::from_secs(state.cfg.upload_reaper.scan_interval_secs.max(1));
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        () = c.cancelled() => break,
                        _ = tick.tick() => {
                            if let Err(e) = st.reap_uploads().await {
                                tracing::warn!(error = %e, "upload reaper scan failed");
                            }
                        }
                    }
                }
            }));
        }

        ready.notify();
        cancel.cancelled().await;
        state.shutdown.cancel();
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
        let handle = lock(&self.outbox).take();
        if let Some(h) = handle {
            h.stop().await;
        }
        Ok(())
    }
}
