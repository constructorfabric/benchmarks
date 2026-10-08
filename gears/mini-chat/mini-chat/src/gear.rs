//! `mini-chat` gear registration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::client_hub::ClientHub;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::authz::EnforcerAuthz;
use crate::domain::services::AppServices;
use crate::infra::llm::transport::OagwTransport;
use crate::infra::plugin_gateway::PluginGateway;

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    app: OnceLock<Arc<AppServices>>,
    hub: OnceLock<Arc<ClientHub>>,
    transport: OnceLock<Arc<OagwTransport>>,
    outbox: Mutex<Option<OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self { app: OnceLock::new(), hub: OnceLock::new(), transport: OnceLock::new(), outbox: Mutex::new(None) }
    }
}

impl MiniChatGear {
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let app = self.app.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat serve before init"))?;
        let hub = self.hub.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat serve before init"))?;
        let transport = self.transport.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat serve before init"))?;

        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(crate::infra::llm::provisioning::run(
            Arc::clone(&app),
            hub,
            transport,
            app.shutdown.clone(),
        )));
        if app.cfg.orphan_watchdog.enabled {
            tasks.push(tokio::spawn(crate::infra::workers::orphan_watchdog::run(Arc::clone(&app), app.shutdown.clone())));
        }
        if app.cfg.upload_reaper.enabled {
            tasks.push(tokio::spawn(crate::infra::workers::upload_reaper::run(Arc::clone(&app), app.shutdown.clone())));
        }
        if app.cfg.thread_summary_worker.enabled {
            let app2 = Arc::clone(&app);
            tasks.push(tokio::spawn(async move {
                let id = app2.cfg.thread_summary_worker.effective_summary_model_id().to_owned();
                match app2.policy.current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID).await {
                    Ok(s) if s.enabled_model(&id).is_some() => {}
                    Ok(_) => tracing::error!(model = %id, "thread summary model is missing or disabled in the policy catalog"),
                    Err(e) => tracing::warn!(error = %e, "could not check the thread summary model at start"),
                }
            }));
        }
        ready.notify();

        cancel.cancelled().await;
        app.shutdown.cancel();
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
        }
        let handle = self.outbox.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(10), h.stop()).await;
        }
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("invalid mini-chat configuration: {e}"))?;
        for field in cfg.deprecated_field_warnings() {
            tracing::warn!(field = %field, "mini-chat config field is deprecated and has no effect");
        }
        let cfg = Arc::new(cfg);

        let db_raw = ctx.db_required()?;
        let db = Arc::new(DBProvider::new(db_raw.db()));

        let hub = ctx.client_hub();
        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let authz = Arc::new(EnforcerAuthz::new(PolicyEnforcer::new(authz_client)));
        let plugins = Arc::new(PluginGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let transport = Arc::new(OagwTransport::new(Arc::clone(&hub)));

        let app = Arc::new(AppServices::new(
            Arc::clone(&cfg),
            db,
            authz,
            plugins.clone(),
            plugins,
            transport.clone(),
        ));

        let handle = crate::infra::outbox::handlers::start_pipeline(&app)
            .await
            .map_err(|e| anyhow::anyhow!("failed to start mini-chat outbox pipeline: {e}"))?;
        if let Ok(mut g) = self.outbox.lock() {
            *g = Some(handle);
        }

        self.app.set(app).map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let _ = self.hub.set(hub);
        let _ = self.transport.set(transport);
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
        let app = self.app.get().cloned().ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        Ok(crate::api::routes::register(router, openapi, app))
    }
}
