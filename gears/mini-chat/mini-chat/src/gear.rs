//! The `mini-chat` gear: wiring of config, clients, services, REST routes,
//! outbox pipeline and background workers.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::api::ServiceGatewayClientV1;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::domain::services::{MiniChatService, ServiceDeps};
use crate::infra::audit::AuditGateway;
use crate::infra::llm::provisioning::{Provisioner, upstream_specs};
use crate::infra::llm::storage::StorageClient;
use crate::infra::llm::{LlmClient, ProviderResolver, S2sContext};
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::outbox::handlers::start_pipeline;
use crate::infra::policy::PolicyGateway;
use crate::infra::workers;

struct Running {
    cancel: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

/// Mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    service: OnceLock<Arc<MiniChatService>>,
    provisioner: OnceLock<Arc<Provisioner>>,
    outbox: Mutex<Option<OutboxHandle>>,
    running: Mutex<Option<Running>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            provisioner: OnceLock::new(),
            outbox: Mutex::new(None),
            running: Mutex::new(None),
        }
    }
}

impl MiniChatGear {
    /// The service (after init).
    #[must_use]
    pub fn service(&self) -> Option<Arc<MiniChatService>> {
        self.service.get().cloned()
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    #[allow(clippy::similar_names)] // `authn` / `authz` are the canonical names
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for key in cfg.deprecated_field_warnings() {
            tracing::warn!(key, "deprecated mini-chat config key is set to a non-default value and is ignored");
        }
        let cfg = Arc::new(cfg);
        let hub = ctx.client_hub();

        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let authz = Authz::new(PolicyEnforcer::new(authz_client));
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = hub.try_get::<dyn AuthNResolverClient>();
        if authn.is_none() {
            tracing::warn!("AuthN resolver client not available; provider calls use the caller's context");
        }

        let policy = Arc::new(PolicyGateway::from_hub(Arc::clone(&hub), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::from_hub(Arc::clone(&hub), cfg.vendor.clone()));

        let db_raw = ctx.db_required()?;
        let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));

        let s2s = Arc::new(S2sContext::default());
        let resolver = Arc::new(ProviderResolver::new(cfg.providers.clone()));
        let llm = Arc::new(LlmClient::new(Arc::clone(&gateway), Arc::clone(&resolver), Arc::clone(&s2s)));
        let storage = Arc::new(
            StorageClient::new(Arc::clone(&gateway), Arc::clone(&s2s)).with_resolver(Arc::clone(&resolver)),
        );
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let metrics = Arc::new(Metrics::new(&cfg.metrics.prefix));

        let svc = Arc::new(MiniChatService::new(ServiceDeps {
            db,
            cfg: Arc::clone(&cfg),
            authz,
            policy,
            audit,
            llm,
            storage,
            outbox: Arc::clone(&outbox),
            metrics,
        }));

        // Queues must be registered before the first enqueue: start the
        // pipeline here (migrations already ran).
        let handle = start_pipeline(&svc, false)
            .await
            .map_err(|e| anyhow::anyhow!("failed to start the mini-chat outbox pipeline: {e}"))?;
        outbox.bind(Arc::clone(handle.outbox()));
        *self.outbox.lock() = Some(handle);

        let provisioner = Arc::new(Provisioner::new(
            gateway,
            authn,
            cfg.client_credentials.clone(),
            Arc::clone(&resolver),
            s2s,
        ));
        resolver.set_hook(Arc::clone(&provisioner) as Arc<dyn crate::infra::llm::ProvisionHook>);
        self.provisioner.set(provisioner).ok();
        self.service
            .set(svc)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
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
        let svc = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat service not initialized"))?;
        Ok(crate::api::rest::register_routes(router, openapi, svc))
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let svc = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: start before init"))?;
        let token = cancel.child_token();
        let mut handles = Vec::new();

        if let Some(p) = self.provisioner.get().cloned() {
            if let Err(e) = p.ensure_s2s().await {
                tracing::warn!(error = %e, "S2S client-credentials exchange failed; will retry during provisioning");
            }
            let specs = upstream_specs(&svc.config().providers);
            if p.start(&specs).await {
                handles.push(tokio::spawn(Arc::clone(&p).reconcile(token.clone())));
            }
        }
        if let Some(h) = workers::spawn_orphan_watchdog(Arc::clone(&svc), token.clone()) {
            handles.push(h);
        }
        if let Some(h) = workers::spawn_upload_reaper(Arc::clone(&svc), token.clone()) {
            handles.push(h);
        }
        *self.running.lock() = Some(Running { cancel: token, handles });
        tracing::info!("mini-chat gear started");
        Ok(())
    }

    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()> {
        if let Some(svc) = self.service.get() {
            svc.shutdown_token().cancel();
        }
        let running = self.running.lock().take();
        if let Some(r) = running {
            r.cancel.cancel();
            let join = futures::future::join_all(r.handles);
            tokio::select! {
                _ = join => {}
                () = deadline_token.cancelled() => {
                    tracing::warn!("mini-chat workers did not stop before the deadline");
                }
                () = tokio::time::sleep(Duration::from_secs(10)) => {
                    tracing::warn!("mini-chat workers did not stop within 10 s");
                }
            }
        }
        let outbox = self.outbox.lock().take();
        if let Some(h) = outbox {
            tokio::select! {
                () = h.stop() => {}
                () = deadline_token.cancelled() => {
                    tracing::warn!("mini-chat outbox did not stop before the deadline");
                }
            }
        }
        Ok(())
    }
}
