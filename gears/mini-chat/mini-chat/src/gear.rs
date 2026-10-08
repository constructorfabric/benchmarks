//! Gear declaration and lifecycle (DESIGN §3.2 "Gear lifecycle").

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, RunnableCapability};
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::OutboxHandle;

use crate::config::MiniChatConfig;
use crate::domain::service::MiniChatService;
use crate::infra::llm::{LlmGateway, ProviderResolver, fallback_context};
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::policy::{AuditGateway, PolicyGateway};
use crate::infra::workers::{LeaderElector, NoopElector, spawn_reaper, spawn_watchdog};

struct Runtime {
    svc: Arc<MiniChatService>,
    db: toolkit_db::Db,
    cfg: Arc<MiniChatConfig>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    authn: Option<Arc<dyn AuthNResolverClient>>,
}

/// The `mini-chat` gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful]
)]
pub struct MiniChatGear {
    runtime: OnceLock<Runtime>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    outbox: Mutex<Option<OutboxHandle>>,
    cancel: Mutex<Option<CancellationToken>>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            runtime: OnceLock::new(),
            workers: Mutex::new(Vec::new()),
            outbox: Mutex::new(None),
            cancel: Mutex::new(None),
        }
    }
}

#[allow(clippy::cognitive_complexity)] // flat list of independent deprecation checks
fn warn_deprecated(cfg: &MiniChatConfig) {
    let d = crate::config::CleanupWorkerConfig::default();
    let c = &cfg.cleanup_worker;
    if !c.enabled {
        tracing::warn!("cleanup_worker.enabled has no effect");
    }
    for (name, set, def) in [
        ("cleanup_worker.poll_interval_secs", c.poll_interval_secs, d.poll_interval_secs),
        ("cleanup_worker.reconcile_interval_secs", c.reconcile_interval_secs, d.reconcile_interval_secs),
        ("cleanup_worker.stale_in_progress_timeout_secs", c.stale_in_progress_timeout_secs, d.stale_in_progress_timeout_secs),
        ("cleanup_worker.batch_size", u64::from(c.batch_size), u64::from(d.batch_size)),
        ("thread_summary_worker.reconcile_interval_secs", cfg.thread_summary_worker.reconcile_interval_secs, 60),
    ] {
        if set != def {
            tracing::warn!(field = name, "deprecated configuration field has no effect");
        }
    }
    let e = &cfg.estimation_budgets;
    let de = crate::config::GearEstimationBudgets::default();
    if (e.bytes_per_token_conservative, e.fixed_overhead_tokens, e.safety_margin_pct, e.image_token_budget)
        != (de.bytes_per_token_conservative, de.fixed_overhead_tokens, de.safety_margin_pct, de.image_token_budget)
        || (e.tool_surcharge_tokens, e.web_search_surcharge_tokens, e.code_interpreter_surcharge_tokens)
            != (de.tool_surcharge_tokens, de.web_search_surcharge_tokens, de.code_interpreter_surcharge_tokens)
    {
        tracing::warn!("estimation_budgets fields other than minimal_generation_floor are deprecated and ignored");
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    #[allow(clippy::similar_names)] // `authz`/`authn` are the established client names
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        cfg.fill_aliases();
        if cfg.client_credentials.is_none() {
            tracing::warn!("mini-chat: client_credentials not configured; using the platform default identity for OAGW");
        }
        warn_deprecated(&cfg);
        let cfg = Arc::new(cfg);
        let db_raw = ctx.db_required()?;
        let raw = db_raw.db();
        let db = Arc::new(DBProvider::<crate::domain::error::DomainError>::new(raw.clone()));
        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("mini-chat: AuthZResolverApi unavailable: {e}"))?;
        let oagw = ctx
            .client_hub()
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("mini-chat: OAGW client unavailable: {e}"))?;
        let authn = ctx.client_hub().get::<dyn AuthNResolverClient>().ok();
        let resolver = Arc::new(ProviderResolver::new(&cfg));
        let llm = Arc::new(LlmGateway::new(Arc::clone(&oagw), resolver));
        let policy = Arc::new(PolicyGateway::from_hub(ctx.client_hub(), cfg.vendor.clone()));
        let audit = Arc::new(AuditGateway::from_hub(ctx.client_hub(), cfg.vendor.clone()));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let svc = MiniChatService::new(db, PolicyEnforcer::new(authz), policy, audit, llm, outbox, Arc::clone(&cfg));
        self.runtime
            .set(Runtime { svc, db: raw, cfg, oagw, authn })
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
        let rt = self.runtime.get().ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        Ok(crate::api::rest::routes::register_routes(router, openapi, Arc::clone(&rt.svc), &rt.cfg.url_prefix))
    }
}

async fn s2s_context(rt: &Runtime) -> toolkit_security::SecurityContext {
    if let (Some(cc), Some(authn)) = (&rt.cfg.client_credentials, &rt.authn) {
        let req = ClientCredentialsRequest {
            client_id: cc.client_id.clone(),
            client_secret: cc.client_secret.clone(),
            scopes: Vec::new(),
        };
        match authn.exchange_client_credentials(&req).await {
            Ok(res) => return res.security_context,
            Err(e) => tracing::warn!(error = %e, "mini-chat: client credentials exchange failed; using the default identity"),
        }
    }
    fallback_context()
}

#[cfg_attr(not(feature = "k8s"), allow(clippy::unused_async))] // awaits only with the `k8s` feature
async fn elector() -> Arc<dyn LeaderElector> {
    #[cfg(feature = "k8s")]
    {
        match crate::infra::workers::k8s::LeaseElector::from_env().await {
            Ok(e) => return Arc::new(e),
            Err(err) => tracing::warn!(error = %err, "mini-chat: k8s lease elector unavailable; running as leader"),
        }
    }
    Arc::new(NoopElector)
}

#[async_trait]
impl RunnableCapability for MiniChatGear {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let rt = self.runtime.get().ok_or_else(|| anyhow::anyhow!("mini-chat not initialized"))?;
        let token = cancel.child_token();
        *self.cancel.lock() = Some(token.clone());

        // S2S identity + OAGW provisioning.
        let ctx = s2s_context(rt).await;
        *rt.svc.llm.s2s_slot().write() = Some(ctx.clone());
        let targets = crate::infra::oagw::targets(&rt.cfg);
        let pending = crate::infra::oagw::provision_all(rt.oagw.as_ref(), &ctx, &rt.svc.llm.resolver, &targets).await;
        let mut workers = Vec::new();
        if !pending.is_empty() {
            workers.push(crate::infra::oagw::spawn_reconcile(
                Arc::clone(&rt.oagw),
                ctx,
                Arc::clone(&rt.svc.llm.resolver),
                pending,
                token.clone(),
            ));
        }

        // Outbox pipeline (five queues).
        let handle = crate::infra::outbox_handlers::start(rt.db.clone(), &rt.svc)
            .await
            .map_err(|e| anyhow::anyhow!("mini-chat outbox start failed: {e}"))?;
        *self.outbox.lock() = Some(handle);

        // Summary model check (startup continues on error).
        if rt.cfg.thread_summary_worker.enabled {
            let model = rt.cfg.thread_summary_worker.summary_model().to_owned();
            let policy = Arc::clone(&rt.svc.policy);
            workers.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                match policy.current_snapshot(toolkit_security::constants::DEFAULT_SUBJECT_ID).await {
                    Ok(s) if s.find_enabled(&model).is_none() => {
                        tracing::error!(model = %model, "thread summary model is missing or disabled in the catalog");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "thread summary model check skipped"),
                }
            }));
        }

        // Leader-only workers.
        let el = elector().await;
        if rt.cfg.orphan_watchdog.enabled {
            workers.push(spawn_watchdog(Arc::clone(&rt.svc), Arc::clone(&el), token.clone()));
        }
        if rt.cfg.upload_reaper.enabled {
            workers.push(spawn_reaper(Arc::clone(&rt.svc), el, token.clone()));
        }
        self.workers.lock().extend(workers);
        tracing::info!("mini-chat started");
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(t) = self.cancel.lock().take() {
            t.cancel();
        }
        if let Some(rt) = self.runtime.get() {
            rt.svc.shutdown.cancel();
        }
        let workers: Vec<JoinHandle<()>> = std::mem::take(&mut *self.workers.lock());
        let join = async {
            for w in workers {
                drop(tokio::time::timeout(Duration::from_secs(5), w).await);
            }
            let handle = self.outbox.lock().take();
            if let Some(h) = handle {
                h.stop().await;
            }
        };
        tokio::select! {
            () = join => {}
            () = deadline.cancelled() => tracing::warn!("mini-chat stop deadline reached"),
        }
        Ok(())
    }
}
