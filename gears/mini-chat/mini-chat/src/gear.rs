//! The `mini-chat` gear: wiring, lifecycle (`init`, REST registration, `start`, `stop`).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::app::AppServices;
use crate::domain::policy::PolicyGateway;
use crate::infra::llm::LlmGateway;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxEnqueuer;

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
#[derive(Default)]
pub struct MiniChatGear {
    services: OnceLock<Arc<AppServices>>,
    outbox: Mutex<Option<OutboxHandle>>,
}

impl MiniChatGear {
    async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let svc = self
            .services
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat started before init"))?;

        // OAGW provisioning under the S2S context.
        if let Err(e) = svc.llm.exchange_s2s().await {
            tracing::warn!(error = %e, "S2S client credentials exchange failed; provisioning deferred");
        }
        let plans = crate::infra::llm::provisioning::plans(svc.llm.providers());
        let deferred = svc.llm.provision_all(&plans).await?;
        let mut tasks = Vec::new();
        if !deferred.is_empty() {
            let llm = Arc::clone(&svc.llm);
            let c = cancel.clone();
            tasks.push(tokio::spawn(async move {
                llm.reconcile_deferred(deferred, c).await;
            }));
        }

        crate::domain::thread_summary::check_summary_model(&svc).await;
        tasks.extend(crate::infra::workers::spawn(&svc, &cancel));

        ready.notify();
        cancel.cancelled().await;
        svc.shutdown.cancel();
        for t in tasks {
            // Bounded wait; a task that does not stop in time is abandoned, as before.
            tokio::time::timeout(Duration::from_secs(5), t).await.ok();
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
        let mut cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat configuration: {e}"))?;
        cfg.fill_upstream_aliases();
        cfg.warn_deprecated();
        let cfg = Arc::new(cfg);

        let raw = ctx.db_required()?;
        let raw_db = raw.db();
        let db = Arc::new(DBProvider::new(raw_db.clone()));
        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("AuthZResolverApi not available: {e}"))?;
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("ServiceGatewayClientV1 not available: {e}"))?;
        let authn_client = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("AuthNResolverClient not available: {e}"))?;

        let llm = Arc::new(LlmGateway::new(
            oagw,
            authn_client,
            cfg.client_credentials.clone(),
            cfg.providers.clone(),
        ));
        let policy = Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let metrics = Arc::new(Metrics::new(&cfg.metrics.prefix));
        let upload_permits = Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads)));
        let enforcer = PolicyEnforcer::new(authz);
        let svc = Arc::new(AppServices {
            cfg,
            db,
            raw_db: raw_db.clone(),
            enforcer,
            policy,
            outbox,
            llm,
            metrics,
            hub,
            upload_permits,
            shutdown: CancellationToken::new(),
        });
        let handle = crate::infra::outbox::handlers::start(raw_db, &svc).await?;
        svc.outbox.bind(handle.outbox());
        if let Ok(mut g) = self.outbox.lock() {
            *g = Some(handle);
        }
        self.services
            .set(svc)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut migrations = crate::infra::db::migrations::Migrator::migrations();
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        migrations
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let svc = self
            .services
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat services not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(
            router, openapi, svc,
        ))
    }
}
