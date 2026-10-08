//! `mini-chat` gear registration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::pep::PolicyEnforcer;
use oagw_sdk::ServiceGatewayClientV1;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::app::App;
use crate::domain::error::DomainError;
use crate::infra::gateways::{AuditGateway, PolicyGateway};
use crate::infra::llm::transport::OagwTransport;
use crate::infra::oagw_provisioning;

struct Runtime {
    outbox: Option<OutboxHandle>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    app: OnceLock<Arc<App>>,
    transport: OnceLock<Arc<OagwTransport>>,
    gateway: OnceLock<Arc<dyn ServiceGatewayClientV1>>,
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    runtime: Mutex<Option<Runtime>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            app: OnceLock::new(),
            transport: OnceLock::new(),
            gateway: OnceLock::new(),
            authn: OnceLock::new(),
            runtime: Mutex::new(None),
        }
    }
}

fn expand(s: &str) -> anyhow::Result<String> {
    toolkit::var_expand::expand_env_vars(s).map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))
}

/// Expands `${VAR}` in provider host / auth config and client credentials.
fn expand_config(cfg: &mut MiniChatConfig) -> anyhow::Result<()> {
    cfg.client_credentials.client_id = expand(&cfg.client_credentials.client_id)?;
    cfg.client_credentials.client_secret = expand(&cfg.client_credentials.client_secret)?;
    for p in cfg.providers.values_mut() {
        p.host = expand(&p.host)?;
        for v in p.auth_config.values_mut() {
            *v = expand(v)?;
        }
        for o in p.tenant_overrides.values_mut() {
            if let Some(h) = &o.host {
                o.host = Some(expand(h)?);
            }
            if let Some(ac) = o.auth_config.as_mut() {
                for v in ac.values_mut() {
                    *v = expand(v)?;
                }
            }
        }
        if p.upstream_alias.as_deref().is_none_or(str::is_empty) {
            p.upstream_alias = Some(p.host.clone());
        }
    }
    Ok(())
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_or_default()?;
        expand_config(&mut cfg)?;
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for w in cfg.deprecation_warnings() {
            tracing::warn!("{w}");
        }
        let db_raw = ctx.db_required()?;
        let db = DBProvider::<DomainError>::new(db_raw.db());
        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;
        let transport = Arc::new(OagwTransport::new(Arc::clone(&gateway)));
        let vendor = cfg.vendor.clone();
        let app = Arc::new(App::new(
            cfg,
            db,
            PolicyEnforcer::new(authz),
            PolicyGateway::from_hub(Arc::clone(&hub), vendor.clone()),
            AuditGateway::from_hub(hub, vendor),
            Arc::clone(&transport) as Arc<dyn crate::infra::llm::ProviderTransport>,
        ));
        let _ = self.transport.set(transport);
        let _ = self.gateway.set(gateway);
        let _ = self.authn.set(authn);
        self.app
            .set(app)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
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
    fn register_rest(&self, _ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router> {
        let app = self
            .app
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let prefix = app.cfg.url_prefix.clone();
        Ok(crate::api::rest::register_routes(router, openapi, app, &prefix))
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, _cancel: CancellationToken) -> anyhow::Result<()> {
        let app = self
            .app
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat start before init"))?;
        let mut tasks = Vec::new();

        // S2S context for OAGW provisioning and provider traffic.
        if let (Some(authn), Some(transport), Some(gateway)) = (self.authn.get(), self.transport.get(), self.gateway.get()) {
            let req = ClientCredentialsRequest {
                client_id: app.cfg.client_credentials.client_id.clone(),
                client_secret: SecretString::from(app.cfg.client_credentials.client_secret.clone()),
                scopes: Vec::new(),
            };
            match authn.exchange_client_credentials(&req).await {
                Ok(res) => {
                    let ctx = res.security_context;
                    transport.set_context(ctx.clone());
                    let plans = oagw_provisioning::plan(&app.cfg.providers);
                    if let Some(t) =
                        oagw_provisioning::provision_all(Arc::clone(gateway), ctx, plans, app.shutdown.clone()).await
                    {
                        tasks.push(t);
                    }
                }
                Err(e) => tracing::error!(error = %e, "mini-chat: client credentials exchange failed; OAGW provisioning skipped"),
            }
        }

        let outbox = crate::infra::outbox::handlers::start_outbox(&app)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat outbox start failed: {e}"))?;
        if app.cfg.thread_summary_worker.enabled {
            match app.policy.current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID).await {
                Ok(s) if s.find_enabled(app.cfg.thread_summary_worker.summary_model()).is_none() => {
                    tracing::error!(model = %app.cfg.thread_summary_worker.summary_model(), "thread summary model is missing or disabled");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "policy catalog not available at start"),
            }
        }
        tasks.extend(app.spawn_workers());
        if let Ok(mut rt) = self.runtime.lock() {
            *rt = Some(Runtime { outbox: Some(outbox), tasks });
        }
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(app) = self.app.get() {
            app.shutdown.cancel();
        }
        let rt = self.runtime.lock().ok().and_then(|mut g| g.take());
        if let Some(mut rt) = rt {
            for t in rt.tasks.drain(..) {
                let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
            }
            if let Some(outbox) = rt.outbox.take() {
                tokio::select! {
                    () = outbox.stop() => {}
                    () = deadline.cancelled() => {}
                }
            }
        }
        Ok(())
    }
}
