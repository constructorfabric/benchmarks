//! `mini-chat` gear declaration, wiring and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;
use tracing::{debug, error, info, warn};

use crate::config::MiniChatConfig;
use crate::domain::service::{Core, CoreDeps, SYSTEM_SUBJECT_ID};
use crate::infra::llm::oagw_transport::OagwTransport;
use crate::infra::oagw_provisioning::{provision_all, targets};
use crate::infra::outbox_handlers::start_outbox;
use crate::infra::plugin_gateway::{AuditGateway, PolicyGateway};

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    core: OnceLock<Arc<Core>>,
    outbox: Mutex<Option<OutboxHandle>>,
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    gateway: OnceLock<Arc<dyn ServiceGatewayClientV1>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            core: OnceLock::new(),
            outbox: Mutex::new(None),
            authn: OnceLock::new(),
            gateway: OnceLock::new(),
        }
    }
}

impl MiniChatGear {
    #[allow(
        clippy::redundant_pub_crate,
        reason = "serve entry point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(core) = self.core.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        // S2S identity for OAGW provisioning and background provider calls.
        if let (Some(authn), Some(gw)) = (self.authn.get().cloned(), self.gateway.get().cloned()) {
            let creds = core.cfg.client_credentials.clone();
            let req = ClientCredentialsRequest {
                client_id: creds.client_id.clone(),
                client_secret: creds
                    .client_secret
                    .clone()
                    .unwrap_or_else(|| secrecy::SecretString::from(String::new())),
                scopes: Vec::new(),
            };
            match authn.exchange_client_credentials(&req).await {
                Ok(res) => {
                    core.system_ctx.set(res.security_context.clone());
                    provision_all(
                        gw,
                        res.security_context,
                        targets(&core.cfg.providers),
                        core.shutdown.clone(),
                    )
                    .await;
                }
                Err(e) => {
                    error!(error = %e, "mini-chat: S2S client credential exchange failed; providers not provisioned");
                }
            }
        }
        self.check_summary_model(&core).await;
        ready.notify();

        let mut workers = Vec::new();
        if core.cfg.orphan_watchdog.enabled {
            let c = Arc::clone(&core);
            let every = Duration::from_secs(core.cfg.orphan_watchdog.scan_interval_secs.max(1));
            workers.push(tokio::spawn(async move {
                let mut iv = tokio::time::interval(every);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        () = c.shutdown.cancelled() => break,
                        _ = iv.tick() => {
                            let n = c.scan_orphans().await;
                            if n > 0 { info!(finalized = n, "mini-chat: orphan watchdog finalized turns"); }
                        }
                    }
                }
            }));
        }
        if core.cfg.upload_reaper.enabled {
            let c = Arc::clone(&core);
            let every = Duration::from_secs(core.cfg.upload_reaper.scan_interval_secs.max(1));
            workers.push(tokio::spawn(async move {
                let mut iv = tokio::time::interval(every);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        () = c.shutdown.cancelled() => break,
                        _ = iv.tick() => {
                            let n = c.reap_abandoned_uploads().await;
                            if n > 0 { info!(failed = n, "mini-chat: upload reaper failed abandoned uploads"); }
                        }
                    }
                }
            }));
        }

        cancel.cancelled().await;
        core.shutdown.cancel();
        for w in workers {
            join_worker(w).await;
        }
        let handle = self.outbox.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            h.stop().await;
        }
        Ok(())
    }

    async fn check_summary_model(&self, core: &Arc<Core>) {
        if !core.cfg.thread_summary_worker.enabled {
            return;
        }
        let id = core
            .cfg
            .thread_summary_worker
            .effective_model_id()
            .to_owned();
        match core.policy.current_snapshot(SYSTEM_SUBJECT_ID).await {
            Ok(s) if s.find_enabled_model(&id).is_some() => {}
            Ok(_) => {
                error!(model = %id, "mini-chat: thread summary model is missing or disabled in the catalog");
            }
            Err(e) => warn!(error = %e, "mini-chat: could not check the thread summary model"),
        }
    }
}

/// Waits (bounded) for a background worker to stop after shutdown was signalled.
async fn join_worker(w: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(Duration::from_secs(5), w).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!(error = %e, "mini-chat: background worker ended abnormally"),
        Err(_) => debug!("mini-chat: background worker did not stop within 5s"),
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(gear = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        cfg.warn_deprecated();
        let db_raw = ctx.db_required()?;
        let db = Arc::new(DBProvider::new(db_raw.db()));
        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("AuthZResolverApi unavailable: {e}"))?;
        let gw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("ServiceGatewayClientV1 unavailable: {e}"))?;
        if let Ok(authn) = hub.get::<dyn AuthNResolverClient>() {
            if self.authn.set(authn).is_err() {
                debug!("mini-chat: AuthN client already set; keeping the first one");
            }
        } else {
            warn!("mini-chat: AuthNResolverClient unavailable; OAGW provisioning is skipped");
        }
        if self.gateway.set(Arc::clone(&gw)).is_err() {
            debug!("mini-chat: OAGW client already set; keeping the first one");
        }
        let vendor = cfg.vendor.clone();
        let core = Core::new(CoreDeps {
            cfg,
            db,
            enforcer: PolicyEnforcer::new(authz),
            policy: Arc::new(PolicyGateway::from_hub(Arc::clone(&hub), vendor.clone())),
            audit: Arc::new(AuditGateway::from_hub(hub, vendor)),
            transport: Arc::new(OagwTransport::new(gw)),
        });
        let handle = start_outbox(db_raw.db(), &core).await?;
        if let Ok(mut g) = self.outbox.lock() {
            *g = Some(handle);
        }
        self.core
            .set(core)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        info!("mini-chat gear initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        all_migrations()
    }
}

/// Gear migrations followed by the shared outbox migrations.
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    use sea_orm_migration::MigratorTrait;
    let mut m = crate::infra::db::migrations::Migrator::migrations();
    m.extend(toolkit_db::outbox::outbox_migrations());
    m
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let core = self
            .core
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        Ok(crate::api::routes::register_routes(router, openapi, core))
    }
}
