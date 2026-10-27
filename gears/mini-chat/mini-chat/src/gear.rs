//! The mini-chat gear: registration, lifecycle, wiring.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;

use crate::config::MiniChatConfig;
use crate::domain::attachment_service::AttachmentService;
use crate::domain::authz::Pep;
use crate::domain::chat_service::ChatService;
use crate::domain::model_service::ModelService;
use crate::domain::services::{AppServices, Repos, ServiceContext};
use crate::domain::stream_service::StreamService;
use crate::domain::turn_service::TurnService;
use crate::infra::gateways::{AuditGateway, ModelPolicyGateway};
use crate::infra::llm::knowledge::KnowledgeSearchRuntime;
use crate::infra::llm::providers::ProviderAdapter;
use crate::infra::llm::resolver::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;
use crate::infra::metrics::Metrics;
use crate::infra::oagw_provision::OagwProvisioner;

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    state: OnceLock<Arc<GearState>>,
    outbox: parking_lot::Mutex<Option<toolkit_db::outbox::OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            outbox: parking_lot::Mutex::new(None),
        }
    }
}

/// The initialized gear state.
pub struct GearState {
    /// The application services handed to the REST layer.
    pub services: Arc<AppServices>,
    /// The shared service context.
    pub ctx: Arc<ServiceContext>,
    /// OAGW provisioner.
    pub provisioner: Arc<OagwProvisioner>,
    /// Provider registry.
    pub registry: Arc<ProviderRegistry>,
    /// Storage client.
    pub storage: Arc<StorageClient>,
    /// Provider adapter facade.
    pub adapter: Arc<ProviderAdapter>,
    /// The S2S security context (set during provisioning).
    pub s2s_ctx: Arc<OnceLock<SecurityContext>>,
}

impl GearState {
    /// The S2S context used by background handlers.
    #[must_use]
    pub fn s2s_ctx(&self) -> Option<SecurityContext> {
        self.s2s_ctx.get().cloned()
    }
}

impl MiniChatGear {
    /// The lifecycle entry: provisioning, outbox pipeline, background
    /// workers, then idle until cancellation.
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(state) = self.state.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };

        // OAGW provisioning (also exchanges the S2S credentials).
        let s2s = state.provisioner.s2s_context_pub().await?;
        let _ = state.s2s_ctx.set(s2s);
        let deferred = state.provisioner.provision_all().await?;
        Arc::new(OagwProvisioner::new(
            state.provisioner.oagw(),
            state.provisioner.authn(),
            state.provisioner.config(),
        ))
        .reconcile_deferred(deferred, cancel.clone());

        // Outbox pipeline (five queues).
        let deps = crate::infra::outbox::pipeline::PipelineDeps {
            ctx: Arc::clone(&state.ctx),
            storage: Arc::clone(&state.storage),
            registry: Arc::clone(&state.registry),
            adapter: Arc::clone(&state.adapter),
            s2s: state.s2s_ctx().unwrap_or_else(SecurityContext::anonymous),
            enqueuer: Arc::clone(&state.ctx.outbox),
            processors: None,
        };
        let handle = crate::infra::outbox::pipeline::start(&deps).await?;
        *self.outbox.lock() = Some(handle);

        // Background workers (orphan watchdog, upload reaper) under the
        // gear's leader elector.
        crate::infra::watchdog::spawn(&state, cancel.clone())
            .await
            .map_err(|err| anyhow::anyhow!("mini-chat: leader elector setup failed: {err}"))?;

        ready.notify();
        cancel.cancelled().await;

        // Stop: cancel workers, then stop the outbox pipeline.
        let handle = self.outbox.lock().take();
        if let Some(handle) = handle {
            handle.stop().await;
        }
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut config: MiniChatConfig = ctx.config_expanded_or_default()?;
        config.normalize();
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        config.warn_deprecated();
        let config = Arc::new(config);

        let db = ctx.db_required()?;
        let db: crate::infra::storage::repo::Db = toolkit_db::DBProvider::new(db.db());

        let hub = ctx.client_hub();
        let authz_resolver_client = hub
            .get::<dyn authz_resolver_sdk::AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("authz-resolver client missing: {e}"))?;
        let authn_resolver_client = hub
            .get::<dyn authn_resolver_sdk::AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("authn-resolver client missing: {e}"))?;
        let oagw = hub
            .get::<dyn oagw_sdk::ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("oagw client missing: {e}"))?;
        let registry_client = hub
            .get::<dyn types_registry_sdk::TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("types-registry client missing: {e}"))?;

        let metrics = Metrics::new(&config.metrics.prefix);
        let repos = Repos::new(&db);
        let pep = Arc::new(Pep::new(authz_resolver_client));
        let policy: Arc<dyn crate::infra::gateways::PolicyPort> = Arc::new(ModelPolicyGateway::new(
            hub.clone(),
            registry_client.clone(),
            config.vendor.clone(),
        ));
        let audit: Arc<dyn crate::infra::gateways::AuditPort> =
            Arc::new(AuditGateway::new(hub, registry_client, config.vendor.clone()));
        let registry = Arc::new(ProviderRegistry::new(&config));
        let adapter = Arc::new(ProviderAdapter::new(oagw.clone()));
        let storage = Arc::new(StorageClient::new(oagw.clone()));
        let knowledge = if config.knowledge_search.enabled {
            let ks = &config.knowledge_search;
            match (&ks.provider_id, &ks.vector_store_id) {
                (Some(provider_id), Some(vector_store_id)) => Some(KnowledgeSearchRuntime::new(
                    oagw.clone(),
                    Arc::clone(&registry),
                    provider_id.clone(),
                    vector_store_id.clone(),
                    ks.top_k,
                    ks.max_chunk_chars,
                )),
                _ => None,
            }
        } else {
            None
        };

        let service_ctx = Arc::new(ServiceContext {
            config: Arc::clone(&config),
            db,
            repos,
            pep,
            policy,
            audit,
            knowledge,
            metrics: metrics.clone(),
            outbox: Arc::new(crate::infra::outbox::SharedEnqueuer::new()),
        });

        let chats = Arc::new(ChatService::new(Arc::clone(&service_ctx)));
        let models = Arc::new(ModelService::new(Arc::clone(&service_ctx)));
        let stream = Arc::new(StreamService::new(
            Arc::clone(&service_ctx),
            adapter.clone(),
            registry.clone(),
            metrics.clone(),
        ));
        let turns = Arc::new(TurnService::new(Arc::clone(&service_ctx), stream.clone()));
        let attachments = Arc::new(AttachmentService::new(
            Arc::clone(&service_ctx),
            registry.clone(),
            storage.clone(),
        ));
        let services = Arc::new(AppServices {
            chats,
            models,
            stream,
            turns,
            attachments,
            config: Arc::clone(&config),
        });

        self.state
            .set(Arc::new(GearState {
                services,
                ctx: service_ctx,
                provisioner: Arc::new(OagwProvisioner::new(oagw, authn_resolver_client, config)),
                registry,
                storage,
                adapter,
                s2s_ctx: Arc::new(OnceLock::new()),
            }))
            .map_err(|_| anyhow::anyhow!("mini-chat module already initialized"))?;
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut migrations = crate::infra::storage::Migrator::migrations();
        let mut outbox = toolkit_db::outbox::outbox_migrations();
        migrations.append(&mut outbox);
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
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat services not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(
            router,
            openapi,
            state.services.clone(),
        ))
    }
}
