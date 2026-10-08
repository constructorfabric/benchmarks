//! The `mini-chat` gear: init, REST registration, migrations, start / stop.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::api::ServiceGatewayClientV1;
use parking_lot::Mutex;
use secrecy::SecretString;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit::{Healthcheck, HealthcheckResult};
use toolkit_db::outbox::OutboxHandle;

use crate::api::rest::handlers::AppState;
use crate::api::rest::routes::register_routes;
use crate::config::MiniChatConfig;
use crate::domain::authz::Authorizer;
use crate::domain::ports::PolicyProvider;
use crate::domain::service::Service;
use crate::domain::service::cleanup::{
    AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, UsageHandler,
};
use crate::domain::service::summary::ThreadSummaryHandler;
use crate::domain::service::workers::{LeaderElector, NoopElector};
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::gateway::{ProxyClient, S2sContext};
use crate::infra::llm::provider::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;
use crate::infra::oagw_provision;
use crate::infra::outbox::{self, Handlers, OutboxEnqueuer};
use crate::infra::plugins::audit_gateway::AuditGateway;
use crate::infra::plugins::policy_gateway::PolicyGateway;
use crate::infra::storage::migrations::all_migrations;

/// Worker join timeout on stop.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    service: OnceLock<Arc<Service>>,
    s2s: Arc<S2sContext>,
    gateway: OnceLock<Arc<dyn ServiceGatewayClientV1>>,
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    outbox_handle: Mutex<Option<OutboxHandle>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_cancel: Mutex<Option<CancellationToken>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            s2s: Arc::new(S2sContext::default()),
            gateway: OnceLock::new(),
            authn: OnceLock::new(),
            outbox_handle: Mutex::new(None),
            workers: Mutex::new(Vec::new()),
            worker_cancel: Mutex::new(None),
        }
    }
}

impl MiniChatGear {
    fn service(&self) -> anyhow::Result<Arc<Service>> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat is not initialized"))
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat: invalid configuration: {e}"))?;
        for w in cfg.deprecation_warnings() {
            tracing::warn!("mini-chat: {w}");
        }
        let cfg = Arc::new(cfg);
        let db = ctx.db_required()?.db();
        let hub = ctx.client_hub();

        let authz_client = hub.get::<dyn AuthZResolverApi>()?;
        let gateway = hub.get::<dyn ServiceGatewayClientV1>()?;
        let authn = hub.get::<dyn AuthNResolverClient>()?;
        let policy: Arc<dyn PolicyProvider> =
            Arc::new(PolicyGateway::new(Arc::clone(&hub), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::new(Arc::clone(&hub), cfg.vendor.clone()));

        let providers = Arc::new(ProviderRegistry::new(&cfg.providers));
        let proxy = ProxyClient::new(Arc::clone(&gateway), Arc::clone(&self.s2s));
        let service = Arc::new(Service {
            cfg: Arc::clone(&cfg),
            db,
            authz: Authorizer::new(PolicyEnforcer::new(authz_client)),
            policy,
            audit: Some(audit),
            providers,
            llm: LlmClient::new(proxy.clone()),
            storage: StorageClient::new(proxy),
            outbox: Arc::new(OutboxEnqueuer::new(&cfg.outbox)),
            upload_slots: Arc::new(Semaphore::new(usize::from(
                cfg.rag.max_concurrent_uploads.max(1),
            ))),
            shutdown: CancellationToken::new(),
            metrics: Arc::new(crate::infra::metrics::Metrics::global(cfg.metrics_prefix())),
        });
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("mini-chat already initialized"))?;
        if self.gateway.set(gateway).is_err() || self.authn.set(authn).is_err() {
            anyhow::bail!("mini-chat already initialized");
        }
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        all_migrations()
    }
}

/// Readiness: the outbox pipeline runs and the S2S context is available.
struct Readiness {
    service: Arc<Service>,
    s2s: Arc<S2sContext>,
}

#[async_trait]
impl Healthcheck for Readiness {
    fn name(&self) -> &'static str {
        "mini-chat"
    }

    async fn check(&self) -> HealthcheckResult {
        if !self.service.outbox.is_ready() {
            return HealthcheckResult::unhealthy("outbox pipeline not started");
        }
        if !self.s2s.is_set() {
            return HealthcheckResult::degraded("S2S security context not available");
        }
        HealthcheckResult::healthy()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let service = self.service()?;
        Ok(register_routes(router, openapi, AppState { service }))
    }

    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> {
        let service = self.service.get()?.clone();
        Some(Arc::new(Readiness {
            service,
            s2s: Arc::clone(&self.s2s),
        }))
    }
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let service = self.service()?;
        let cfg = Arc::clone(&service.cfg);
        let worker_cancel = cancel.child_token();
        *self.worker_cancel.lock() = Some(worker_cancel.clone());

        // Outbox pipeline first: request paths wait for it.
        let handle = outbox::start(
            service.db.clone(),
            &service.outbox,
            Handlers {
                usage: Arc::new(UsageHandler(Arc::clone(&service))),
                attachment_cleanup: Arc::new(AttachmentCleanupHandler(Arc::clone(&service))),
                chat_cleanup: Arc::new(ChatCleanupHandler(Arc::clone(&service))),
                thread_summary: Arc::new(ThreadSummaryHandler(Arc::clone(&service))),
                audit: Arc::new(AuditHandler(Arc::clone(&service))),
            },
            Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs),
        )
        .await
        .map_err(|e| anyhow::anyhow!("mini-chat: outbox start failed: {e}"))?;
        *self.outbox_handle.lock() = Some(handle);

        // S2S context and OAGW provisioning.
        let authn = self
            .authn
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: authn client missing"))?;
        let result = authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: cfg.client_credentials.client_id.clone(),
                client_secret: SecretString::from(cfg.client_credentials.client_secret.clone()),
                scopes: Vec::new(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat: client credentials exchange failed: {e}"))?;
        let s2s_ctx = result.security_context;
        self.s2s.set(s2s_ctx.clone());

        let gateway = self
            .gateway
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("mini-chat: oagw client missing"))?;
        let plans = oagw_provision::plan(&service.providers);
        let deferred = oagw_provision::register_all(&gateway, &s2s_ctx, &plans)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat: {e}"))?;
        let mut workers = Vec::new();
        if !deferred.is_empty() {
            workers.push(tokio::spawn(oagw_provision::reconcile(
                Arc::clone(&gateway),
                s2s_ctx,
                deferred,
                worker_cancel.clone(),
            )));
        }

        // Summary model check (startup continues).
        if cfg.thread_summary_worker.enabled {
            match service
                .policy
                .current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID)
                .await
            {
                Ok(s)
                    if s.find_enabled_model(cfg.thread_summary_worker.summary_model().as_ref())
                        .is_none() =>
                {
                    tracing::error!(
                        model = %cfg.thread_summary_worker.summary_model(),
                        "mini-chat: thread summary model is missing or disabled"
                    );
                }
                _ => {}
            }
        }

        let leader_workers = cfg.orphan_watchdog.enabled || cfg.upload_reaper.enabled;
        #[cfg(feature = "k8s")]
        let elector: Arc<dyn LeaderElector> = if leader_workers {
            Arc::new(
                crate::infra::leader::LeaseElector::from_env(worker_cancel.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("mini-chat: leader elector: {e}"))?,
            )
        } else {
            Arc::new(NoopElector)
        };
        #[cfg(not(feature = "k8s"))]
        let elector: Arc<dyn LeaderElector> = {
            let _ = leader_workers;
            Arc::new(NoopElector)
        };
        if cfg.orphan_watchdog.enabled {
            let svc = Arc::clone(&service);
            workers.push(tokio::spawn(svc.run_periodic(
                "orphan-watchdog",
                Duration::from_secs(cfg.orphan_watchdog.scan_interval_secs.max(1)),
                Arc::clone(&elector),
                worker_cancel.clone(),
                |s| async move {
                    if let Err(e) = s.orphan_scan().await {
                        tracing::error!(error = %e, "mini-chat: orphan scan failed");
                    }
                },
            )));
        }
        if cfg.upload_reaper.enabled {
            let svc = Arc::clone(&service);
            workers.push(tokio::spawn(svc.run_periodic(
                "upload-reaper",
                Duration::from_secs(cfg.upload_reaper.scan_interval_secs.max(1)),
                Arc::clone(&elector),
                worker_cancel.clone(),
                |s| async move {
                    if let Err(e) = s.reaper_scan().await {
                        tracing::error!(error = %e, "mini-chat: upload reaper scan failed");
                    }
                },
            )));
        }
        *self.workers.lock() = workers;
        tracing::info!("mini-chat started");
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(c) = self.worker_cancel.lock().take() {
            c.cancel();
        }
        if let Some(s) = self.service.get() {
            s.shutdown.cancel();
        }
        let workers: Vec<JoinHandle<()>> = std::mem::take(&mut *self.workers.lock());
        for w in workers {
            tokio::select! {
                _ = tokio::time::timeout(WORKER_JOIN_TIMEOUT, w) => {}
                () = deadline.cancelled() => return Ok(()),
            }
        }
        let handle = self.outbox_handle.lock().take();
        if let Some(h) = handle {
            tokio::select! {
                () = h.stop() => {}
                () = deadline.cancelled() => {}
            }
        }
        Ok(())
    }
}
