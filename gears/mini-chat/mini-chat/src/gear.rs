//! Gear wiring: configuration, DB, outbox pipeline, background workers,
//! OAGW provisioning and REST registration.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use secrecy::SecretString;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::audit::{AuditGateway, AuditHandler, UsageHandler};
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::domain::policy::PolicyGateway;
use crate::domain::service::MiniChat;
use crate::domain::workers::{AttachmentCleanupHandler, ChatCleanupHandler, ThreadSummaryHandler, run_periodic};
use crate::infra::llm::LlmGateway;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::{OutboxHandlers, OutboxSlot, start_pipeline};
use crate::infra::provisioning;

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    service: OnceLock<Arc<MiniChat>>,
    outbox: Mutex<Option<OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self { service: OnceLock::new(), outbox: Mutex::new(None) }
    }
}

impl MiniChatGear {
    async fn s2s_context(svc: &MiniChat, hub: &toolkit::ClientHub) -> Option<toolkit_security::SecurityContext> {
        let creds = svc.cfg.client_credentials.clone()?;
        let authn = match hub.get::<dyn AuthNResolverClient>() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "authn resolver client unavailable; S2S context not obtained");
                return None;
            }
        };
        let req = ClientCredentialsRequest {
            client_id: creds.client_id.clone(),
            client_secret: SecretString::from(creds.client_secret.clone()),
            scopes: Vec::new(),
        };
        let mut delay = Duration::from_millis(250);
        for attempt in 0..6 {
            match authn.exchange_client_credentials(&req).await {
                Ok(r) => return Some(r.security_context),
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "S2S client-credentials exchange failed");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        None
    }

    #[allow(
        clippy::redundant_pub_crate,
        clippy::cognitive_complexity,
        reason = "entry point invoked by the toolkit runtime; sequential startup steps"
    )]
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(svc) = self.service.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let hub = svc.hub.clone();
        let s2s = Self::s2s_context(&svc, &hub).await.unwrap_or_else(|| {
            tracing::error!("mini-chat: S2S context unavailable; provisioning with a synthesized system context");
            svc.system_ctx_for(toolkit_security::constants::DEFAULT_TENANT_ID)
        });
        if svc.system_ctx.set(s2s.clone()).is_err() {
            tracing::debug!("mini-chat: system context already set");
        }
        svc.llm.set_fallback_ctx(s2s.clone());
        let gw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("OAGW client unavailable: {e}"))?;
        provisioning::provision(gw, &s2s, &svc.cfg.providers, svc.shutdown.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        if svc.cfg.thread_summary_worker.enabled {
            let id = svc.cfg.thread_summary_worker.effective_summary_model_id().to_owned();
            match svc.policy.current(uuid::Uuid::nil()).await {
                Ok(p) if p.find_enabled(&id).is_some() => {}
                Ok(_) => tracing::error!(model = %id, "thread summary model is missing or disabled in the catalog"),
                Err(e) => tracing::warn!(error = %e, "could not verify the thread summary model at start"),
            }
        }

        let mut tasks = Vec::new();
        if svc.cfg.orphan_watchdog.enabled {
            let s = svc.clone();
            let c = svc.shutdown.clone();
            let every = Duration::from_secs(svc.cfg.orphan_watchdog.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                run_periodic("orphan_watchdog", every, c, || s.orphan_scan()).await;
            }));
        }
        if svc.cfg.upload_reaper.enabled {
            let s = svc.clone();
            let c = svc.shutdown.clone();
            let every = Duration::from_secs(svc.cfg.upload_reaper.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                run_periodic("upload_reaper", every, c, || s.upload_reaper_scan()).await;
            }));
        }
        ready.notify();
        cancel.cancelled().await;
        svc.shutdown.cancel();
        for t in tasks {
            if let Err(e) = t.await {
                tracing::debug!(error = %e, "mini-chat: background task join failed");
            }
        }
        let handle = self.outbox.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            h.stop().await;
        }
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_or_default()?;
        cfg.expand_and_normalize();
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))?;
        cfg.warn_deprecated();
        let cfg = Arc::new(cfg);

        let raw = ctx.db_required()?;
        let db = DBProvider::<DomainError>::new(raw.db());
        let hub = ctx.client_hub();
        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let metrics = Arc::new(Metrics::new(&cfg.metrics.prefix));
        let policy = Arc::new(PolicyGateway::new(hub.clone(), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(hub.clone(), cfg.vendor.clone()));
        let outbox_slot = Arc::new(OutboxSlot::new(cfg.outbox.clone()));
        let svc = Arc::new(MiniChat {
            cfg: cfg.clone(),
            db,
            raw_db: raw.db(),
            authz: Authz::new(PolicyEnforcer::new(authz_client)),
            policy: policy.clone(),
            audit: audit.clone(),
            outbox: outbox_slot.clone(),
            llm: Arc::new(LlmGateway::new(cfg.providers.clone(), oagw)),
            metrics: metrics.clone(),
            upload_permits: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
            system_ctx: OnceLock::new(),
            hub: hub.clone(),
        });
        let handlers = OutboxHandlers {
            usage: Box::new(UsageHandler { policy }),
            audit: Box::new(AuditHandler { gateway: audit, metrics }),
            attachment_cleanup: Box::new(AttachmentCleanupHandler(svc.clone())),
            chat_cleanup: Box::new(ChatCleanupHandler(svc.clone())),
            thread_summary: Box::new(ThreadSummaryHandler(svc.clone())),
        };
        let handle = start_pipeline(raw.db(), &cfg.outbox, cfg.thread_summary_worker.claim_timeout_secs, handlers)
            .await
            .map_err(|e| anyhow::anyhow!("outbox pipeline start failed: {e}"))?;
        outbox_slot.install(handle.outbox().clone());
        if let Ok(mut g) = self.outbox.lock() {
            *g = Some(handle);
        }
        self.service
            .set(svc)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut m = crate::infra::storage::migrations::Migrator::migrations();
        m.extend(toolkit_db::outbox::outbox_migrations());
        m
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(&self, _ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router> {
        let svc = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat service not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(router, openapi, svc))
    }
}
