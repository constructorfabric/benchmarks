//! `mini-chat` gear registration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::service::{Deps, Services, cleanup, orphan_watchdog, summary, upload_reaper, usage_audit};
use crate::infra::llm::{KnowledgeRetriever, ProviderResolver};
use crate::infra::llm::client::OagwProviderClient;
use crate::infra::outbox::{self, MiniChatOutbox, OutboxHandlers};
use crate::infra::plugin_gateways::{AuditGateway, PolicyGateway};
use crate::infra::s2s::S2sContextProvider;

/// Runtime state built in `init`.
struct State {
    services: Arc<Services>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    state: OnceLock<State>,
    outbox_handle: Mutex<Option<OutboxHandle>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
            outbox_handle: Mutex::new(None),
        }
    }
}

/// Loads, expands and validates the gear configuration.
///
/// # Errors
/// Invalid configuration.
pub fn load_config(ctx: &GearCtx) -> anyhow::Result<MiniChatConfig> {
    let mut cfg: MiniChatConfig = ctx.config_or_default()?;
    cfg.expand()
        .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
    cfg.fill_aliases();
    for w in cfg.deprecation_warnings() {
        tracing::warn!("{w}");
    }
    Ok(cfg)
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = Arc::new(load_config(ctx)?);
        let db_raw = ctx.db_required()?;
        let db = Arc::new(DBProvider::<DomainError>::new(db_raw.db()));

        let hub = ctx.client_hub();
        let authz = hub.get::<dyn AuthZResolverApi>()?;
        let enforcer = PolicyEnforcer::new(authz);
        let oagw = hub.get::<dyn ServiceGatewayClientV1>()?;
        let authn = hub.get::<dyn AuthNResolverClient>()?;

        let policy = Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let providers = Arc::new(ProviderResolver::new(&cfg));
        let s2s = Arc::new(S2sContextProvider::new(
            authn,
            cfg.client_credentials.client_id.clone(),
            cfg.client_credentials.client_secret.clone(),
        ));
        let provider_client = Arc::new(OagwProviderClient::new(
            Arc::clone(&providers),
            Arc::clone(&oagw),
            Arc::clone(&s2s),
        ));
        let outbox_facade = Arc::new(MiniChatOutbox::new(cfg.outbox.clone()));
        let knowledge: Option<Arc<dyn KnowledgeRetriever>> = cfg
            .knowledge_search
            .enabled
            .then(|| Arc::clone(&provider_client) as Arc<dyn KnowledgeRetriever>);

        let deps = Arc::new(Deps {
            cfg: Arc::clone(&cfg),
            db: Arc::clone(&db),
            enforcer,
            policy,
            audit,
            outbox: Arc::clone(&outbox_facade),
            llm: provider_client.clone(),
            storage: provider_client,
            knowledge,
            providers,
            upload_slots: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        });
        let services = Arc::new(Services::new(Arc::clone(&deps)));

        // Outbox pipeline (handlers resolve plugins lazily).
        let handle = outbox::start(
            db.db(),
            &cfg.outbox,
            cfg.thread_summary_worker.claim_timeout_secs,
            OutboxHandlers {
                usage: usage_audit::UsageHandler::new(Arc::clone(&deps)),
                audit: usage_audit::AuditHandler::new(Arc::clone(&deps)),
                attachment_cleanup: cleanup::AttachmentCleanupHandler::new(Arc::clone(&deps)),
                chat_cleanup: cleanup::ChatCleanupHandler::new(Arc::clone(&deps)),
                thread_summary: summary::ThreadSummaryHandler::new(Arc::clone(&deps)),
            },
        )
        .await?;
        outbox_facade.bind(Arc::clone(handle.outbox()));
        *self.outbox_handle.lock() = Some(handle);

        self.state
            .set(State {
                services,
                oagw,
                s2s,
            })
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

impl MiniChatGear {
    async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(state) = self.state.get() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let deps = Arc::clone(&state.services.deps);

        // OAGW provisioning (misconfiguration fails startup; deferred entries retried in background).
        crate::infra::provisioning::provision(
            Arc::clone(&deps.cfg),
            Arc::clone(&state.oagw),
            Arc::clone(&state.s2s),
            deps.shutdown.child_token(),
        )
        .await?;

        // Summary model check (plugins are resolvable once every gear finished init).
        let check = tokio::time::timeout(Duration::from_secs(10), summary::check_summary_model(&deps)).await;
        if check.is_err() {
            tracing::warn!("thread summary model check timed out");
        }

        if deps.cfg.orphan_watchdog.enabled {
            deps.tasks.spawn(orphan_watchdog::run(Arc::clone(&deps), deps.shutdown.child_token()));
        }
        if deps.cfg.upload_reaper.enabled {
            deps.tasks.spawn(upload_reaper::run(Arc::clone(&deps), deps.shutdown.child_token()));
        }

        ready.notify();
        cancel.cancelled().await;

        deps.shutdown.cancel();
        deps.tasks.close();
        if tokio::time::timeout(Duration::from_secs(10), deps.tasks.wait()).await.is_err() {
            tracing::warn!("mini-chat background tasks did not stop in time");
        }
        let handle = self.outbox_handle.lock().take();
        if let Some(h) = handle {
            h.stop().await;
        }
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
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let prefix = state.services.deps.cfg.url_prefix.clone();
        Ok(crate::api::rest::routes::register_routes(
            router,
            openapi,
            Arc::clone(&state.services),
            &prefix,
        ))
    }
}
