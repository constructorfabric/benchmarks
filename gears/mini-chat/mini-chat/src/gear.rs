//! `mini-chat` gear declaration, wiring and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
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
use tracing::{error, info, warn};

use crate::config::MiniChatConfig;
use crate::domain::authz::PdpAuthorizer;
use crate::domain::service::MiniChat;
use crate::domain::workers::cleanup::{AttachmentCleanupHandler, ChatCleanupHandler};
use crate::domain::workers::thread_summary::ThreadSummaryHandler;
use crate::domain::workers::{AuditHandler, ServiceSlot, UsageHandler, periodic};
use crate::infra::audit_gateway::GtsAuditResolver;
use crate::infra::llm::client::{LlmClient, OagwProxy, S2sContext};
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::llm::storage::StorageClient;
use crate::infra::outbox::{OutboxEnqueuer, OutboxHandlers, start_pipeline};
use crate::infra::policy_gateway::GtsPolicyProvider;

struct Runtime {
    svc: Arc<MiniChat>,
    s2s: Arc<S2sContext>,
    gateway: Arc<dyn ServiceGatewayClientV1>,
    authn: Arc<dyn AuthNResolverClient>,
    outbox: Mutex<Option<OutboxHandle>>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    runtime: OnceLock<Runtime>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            runtime: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = Arc::new(MiniChatConfig::from_value(Some(ctx.raw_config()))?);
        for field in cfg.deprecated_field_warnings() {
            warn!(field, "deprecated mini-chat configuration field has no effect");
        }
        let db_raw = ctx.db_required()?;
        let raw_db = db_raw.db();
        let db = Arc::new(DBProvider::new(raw_db.clone()));
        let hub = ctx.client_hub();
        let authz_client = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let gateway = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;
        let s2s = Arc::new(S2sContext::default());
        let proxy = Arc::new(OagwProxy::new(Arc::clone(&gateway), Arc::clone(&s2s)));
        let resolver = ProviderResolver::new(cfg.providers.clone());
        let slot: ServiceSlot = Arc::new(OnceLock::new());
        let handlers = OutboxHandlers {
            usage: Box::new(UsageHandler { slot: Arc::clone(&slot) }),
            attachment_cleanup: Box::new(AttachmentCleanupHandler { slot: Arc::clone(&slot) }),
            chat_cleanup: Box::new(ChatCleanupHandler { slot: Arc::clone(&slot) }),
            thread_summary: Box::new(ThreadSummaryHandler { slot: Arc::clone(&slot) }),
            audit: Box::new(AuditHandler {
                resolver: Arc::new(GtsAuditResolver::new(Arc::clone(&hub), cfg.vendor.clone())),
            }),
        };
        let handle = start_pipeline(
            raw_db,
            &cfg.outbox,
            cfg.thread_summary_worker.claim_timeout_secs,
            handlers,
        )
        .await
        .map_err(|e| anyhow::anyhow!("outbox pipeline start failed: {e}"))?;
        let outbox = OutboxEnqueuer::new(Arc::clone(handle.outbox()), cfg.outbox.clone());
        let svc = Arc::new(MiniChat {
            cfg: Arc::clone(&cfg),
            db,
            authz: Arc::new(PdpAuthorizer::new(PolicyEnforcer::new(authz_client))),
            policy: Arc::new(GtsPolicyProvider::new(Arc::clone(&hub), cfg.vendor.clone())),
            llm: LlmClient::new(proxy.clone()),
            storage: StorageClient::new(proxy),
            resolver,
            outbox,
            upload_slots: Arc::new(Semaphore::new(usize::from(cfg.rag.max_concurrent_uploads))),
            shutdown: CancellationToken::new(),
        });
        slot.set(Arc::clone(&svc)).ok();
        self.runtime
            .set(Runtime {
                svc,
                s2s,
                gateway,
                authn,
                outbox: Mutex::new(Some(handle)),
            })
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
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
    fn register_rest(&self, _ctx: &GearCtx, router: axum::Router, openapi: &dyn OpenApiRegistry) -> anyhow::Result<axum::Router> {
        let rt = self
            .runtime
            .get()
            .ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let prefix = rt.svc.cfg.url_prefix.clone();
        Ok(crate::api::rest::routes::register_routes(router, openapi, Arc::clone(&rt.svc), &prefix))
    }
}

impl MiniChatGear {
    async fn connect_providers(
        svc: Arc<MiniChat>,
        s2s: Arc<S2sContext>,
        gateway: Arc<dyn ServiceGatewayClientV1>,
        authn: Arc<dyn AuthNResolverClient>,
        cancel: CancellationToken,
    ) {
        let request = ClientCredentialsRequest {
            client_id: svc.cfg.client_credentials.client_id.clone(),
            client_secret: secrecy::SecretString::from(svc.cfg.client_credentials.client_secret.clone()),
            scopes: Vec::new(),
        };
        let mut delay = Duration::from_secs(1);
        let ctx = loop {
            match authn.exchange_client_credentials(&request).await {
                Ok(r) => break r.security_context,
                Err(e) => {
                    warn!(error = %e, "S2S client credentials exchange failed; retrying");
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        };
        s2s.set(ctx.clone());
        crate::infra::oagw_provision::provision_all(gateway, ctx, Arc::clone(&svc.resolver), cancel).await;
    }

    async fn check_summary_model(svc: &MiniChat) {
        if !svc.cfg.thread_summary_worker.enabled {
            return;
        }
        let id = svc.cfg.thread_summary_worker.summary_model().to_owned();
        let user = uuid::Uuid::parse_str(crate::domain::workers::thread_summary::SYSTEM_USER_ID).unwrap_or_default();
        match svc.policy.current_snapshot(user).await {
            Ok(s) if s.find_enabled(&id).is_some() => {}
            Ok(_) => error!(model = %id, "thread summary model is missing or disabled in the catalog"),
            Err(e) => warn!(error = %e, "could not verify the thread summary model"),
        }
    }

    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken, ready: ReadySignal) -> anyhow::Result<()> {
        let Some(rt) = self.runtime.get() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let svc = Arc::clone(&rt.svc);
        let worker_cancel = svc.shutdown.clone();
        tokio::spawn(Self::connect_providers(
            Arc::clone(&svc),
            Arc::clone(&rt.s2s),
            Arc::clone(&rt.gateway),
            Arc::clone(&rt.authn),
            worker_cancel.clone(),
        ));
        let mut tasks = Vec::new();
        if svc.cfg.orphan_watchdog.enabled {
            let s = Arc::clone(&svc);
            let c = worker_cancel.clone();
            let every = Duration::from_secs(svc.cfg.orphan_watchdog.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                periodic(every, c, || {
                    let s = Arc::clone(&s);
                    async move {
                        if let Err(e) = s.orphan_scan().await {
                            warn!(error = %e, "orphan watchdog scan failed");
                        }
                    }
                })
                .await;
            }));
        }
        if svc.cfg.upload_reaper.enabled {
            let s = Arc::clone(&svc);
            let c = worker_cancel.clone();
            let every = Duration::from_secs(svc.cfg.upload_reaper.scan_interval_secs);
            tasks.push(tokio::spawn(async move {
                periodic(every, c, || {
                    let s = Arc::clone(&s);
                    async move {
                        if let Err(e) = s.reaper_scan().await {
                            warn!(error = %e, "upload reaper scan failed");
                        }
                    }
                })
                .await;
            }));
        }
        {
            let s = Arc::clone(&svc);
            tokio::spawn(async move { Self::check_summary_model(&s).await });
        }
        ready.notify();
        cancel.cancelled().await;
        worker_cancel.cancel();
        for t in tasks {
            tokio::time::timeout(Duration::from_secs(5), t).await.ok();
        }
        let handle = rt.outbox.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            h.stop().await;
        }
        info!("mini-chat stopped");
        Ok(())
    }
}
