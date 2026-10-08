//! `mini-chat` gear: wiring, migrations, REST registration and the managed
//! lifecycle (S2S identity, OAGW provisioning, outbox pipeline, workers).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{info, warn};

use crate::config::MiniChatConfig;
use crate::infra::llm::{LlmGateway, ProviderRegistry, provision};
use crate::infra::policy::{AuditGateway, PolicyGateway};
use crate::service::{AppState, OutboxSlot, outbox, workers};

#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s")
)]
pub struct MiniChatGear {
    state: OnceLock<Arc<AppState>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl MiniChatGear {
    async fn exchange_s2s(state: &AppState, authn: &Arc<dyn AuthNResolverClient>) {
        let Some(cc) = state.cfg.client_credentials.clone() else {
            return;
        };
        let req = ClientCredentialsRequest {
            client_id: cc.client_id,
            client_secret: cc.client_secret.into(),
            scopes: vec![],
        };
        match authn.exchange_client_credentials(&req).await {
            Ok(r) => {
                state.llm.set_s2s(r.security_context).await;
                info!("mini-chat S2S identity obtained");
            }
            Err(e) => {
                warn!(error = %e, "mini-chat S2S credential exchange failed; using caller identities");
            }
        }
    }

    /// Provision every upstream (retrying in the background until it works).
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    async fn provision(state: Arc<AppState>, cancel: CancellationToken) {
        let targets = provision::targets(&state.llm);
        let mut pending = targets;
        let mut delay = Duration::from_millis(500);
        loop {
            let ctx = match state.llm.s2s().await {
                Some(c) => c,
                None => match crate::service::default_system_ctx() {
                    Some(c) => c,
                    None => return,
                },
            };
            let mut failed = Vec::new();
            for t in pending {
                if let Err(e) = provision::provision_target(&state.llm, &ctx, &t).await {
                    warn!(alias = %t.alias, error = %e, "upstream provisioning failed; will retry");
                    failed.push(t);
                } else {
                    info!(alias = %t.alias, provider = %t.provider_id, "upstream provisioned");
                }
            }
            if failed.is_empty() {
                return;
            }
            pending = failed;
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken) -> anyhow::Result<()> {
        let Some(state) = self.state.get().cloned() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        if let Ok(authn) = state.hub.get::<dyn AuthNResolverClient>() {
            Self::exchange_s2s(&state, &authn).await;
        } else {
            warn!("AuthN resolver client not available; S2S identity disabled");
        }
        tokio::spawn(Self::provision(Arc::clone(&state), state.cancel.clone()));

        let handle = outbox::start_outbox(&state)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat outbox start failed: {e}"))?;
        state.outbox.set(Some(Arc::clone(handle.outbox())));
        workers::spawn_workers(&state);
        if state.cfg.thread_summary_worker.enabled {
            let st = Arc::clone(&state);
            tokio::spawn(async move { st.check_summary_model().await });
        }
        info!("mini-chat started");

        cancel.cancelled().await;
        state.cancel.cancel();
        state.outbox.set(None);
        handle.stop().await;
        info!("mini-chat stopped");
        Ok(())
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_or_default()?;
        cfg.expand_vars()
            .map_err(|e| anyhow::anyhow!("mini-chat config: {e}"))?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        cfg.warn_deprecated();
        let cfg = Arc::new(cfg);

        let db = ctx.db_required()?.db();
        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let enforcer = PolicyEnforcer::new(authz);

        let llm = Arc::new(LlmGateway::new(ProviderRegistry::new(
            cfg.providers.clone(),
        )));
        match hub.get::<dyn ServiceGatewayClientV1>() {
            Ok(gw) => llm.set_gateway(gw),
            Err(e) => warn!(error = %e, "OAGW client not available; provider calls will fail"),
        }

        let policy = Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let upload_sem = Arc::new(Semaphore::new(usize::from(
            cfg.rag.max_concurrent_uploads.max(1),
        )));
        let state = Arc::new(AppState {
            cfg,
            db,
            enforcer,
            policy,
            audit,
            hub,
            llm,
            outbox: OutboxSlot::default(),
            cancel: CancellationToken::new(),
            upload_sem,
        });
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("mini-chat already initialized"))?;
        info!("mini-chat initialized");
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
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(
            router, openapi, state,
        ))
    }
}
