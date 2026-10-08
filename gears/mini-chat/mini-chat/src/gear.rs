//! `mini-chat` gear declaration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::client_hub::ClientHub;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::outbox::OutboxHandle;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use tracing::{info, warn};

use crate::config::MiniChatConfig;
use crate::domain::service::policy::RegistryPolicySource;
use crate::domain::service::{AppServices, Authz, policy};
use crate::infra::llm::transport::{AliasMap, OagwTransport, S2sContext};
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::outbox::handlers::{RegistryAuditSource, start_pipeline};
use crate::infra::{provision, workers};

struct State {
    svc: Arc<AppServices>,
    hub: Arc<ClientHub>,
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    aliases: Arc<AliasMap>,
    outbox: Mutex<Option<OutboxHandle>>,
}

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    state: OnceLock<State>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl MiniChatGear {
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(st) = self.state.get() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let svc = Arc::clone(&st.svc);
        let cfg = Arc::clone(&svc.cfg);

        // S2S identity for OAGW provisioning and provider calls.
        match st.hub.get::<dyn AuthNResolverClient>() {
            Ok(authn) => {
                let req = ClientCredentialsRequest {
                    client_id: cfg.client_credentials.client_id.clone(),
                    client_secret: cfg.client_credentials.client_secret.clone().into(),
                    scopes: Vec::new(),
                };
                match authn.exchange_client_credentials(&req).await {
                    Ok(r) => st.s2s.set(r.security_context),
                    Err(e) => warn!(error = %e, "client credentials exchange failed; using default identity"),
                }
            }
            Err(e) => warn!(error = %e, "authn-resolver client unavailable"),
        }
        provision::provision_all(
            Arc::clone(&st.gateway),
            Arc::clone(&st.s2s),
            &cfg,
            Arc::clone(&st.aliases),
            cancel.clone(),
        )
        .await;

        if cfg.thread_summary_worker.enabled {
            match policy::current_snapshot(svc.policy.as_ref(), DEFAULT_SUBJECT_ID).await {
                Ok(s) if s.enabled_model(cfg.thread_summary_worker.summary_model()).is_none() => {
                    tracing::error!(
                        model = cfg.thread_summary_worker.summary_model(),
                        "thread summary model is missing or disabled in the catalog"
                    );
                }
                _ => {}
            }
        }

        let mut tasks = Vec::new();
        if cfg.orphan_watchdog.enabled {
            let s = Arc::clone(&svc);
            let c = cancel.clone();
            let every = Duration::from_secs(cfg.orphan_watchdog.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                workers::run_periodic("orphan_watchdog", every, c, || workers::orphan_scan(&s)).await;
            }));
        }
        if cfg.upload_reaper.enabled {
            let s = Arc::clone(&svc);
            let c = cancel.clone();
            let every = Duration::from_secs(cfg.upload_reaper.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                workers::run_periodic("upload_reaper", every, c, || workers::upload_reaper_scan(&s)).await;
            }));
        }

        ready.notify();
        info!("mini-chat gear started");
        cancel.cancelled().await;
        svc.shutdown.cancel();
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
        }
        let handle = st.outbox.lock().take();
        if let Some(h) = handle {
            h.stop().await;
        }
        info!("mini-chat gear stopped");
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_or_default()?;
        cfg.expand_and_normalize()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for field in cfg.deprecated_fields_in_use() {
            warn!(field, "deprecated mini-chat configuration field has no effect");
        }
        let cfg = Arc::new(cfg);
        let db = ctx.db_required()?.db();
        let hub = ctx.client_hub();
        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let s2s = Arc::new(S2sContext::default());
        let aliases = Arc::new(AliasMap::default());
        let transport = Arc::new(OagwTransport {
            gateway: Arc::clone(&gateway),
            s2s: Arc::clone(&s2s),
            aliases: Arc::clone(&aliases),
        });
        let policy = Arc::new(RegistryPolicySource::new(Arc::clone(&hub), cfg.vendor.clone()));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let svc = AppServices::new(
            Arc::clone(&cfg),
            db.clone(),
            Authz::new(PolicyEnforcer::new(authz_client)),
            policy,
            transport,
            outbox,
        );
        let audit = Arc::new(RegistryAuditSource::new(Arc::clone(&hub), cfg.vendor.clone()));
        let handle = start_pipeline(db, &svc, audit)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat outbox start failed: {e}"))?;
        self.state
            .set(State {
                svc,
                hub,
                gateway,
                s2s,
                aliases,
                outbox: Mutex::new(Some(handle)),
            })
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        info!(providers = cfg.providers.len(), "mini-chat gear initialized");
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
        let st = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let prefix = st.svc.cfg.url_prefix.trim_end_matches('/').to_owned();
        Ok(crate::api::rest::routes::register_routes(
            router,
            openapi,
            Arc::clone(&st.svc),
            &prefix,
        ))
    }
}
