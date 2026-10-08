//! `mini-chat` gear: configuration, wiring, REST registration and lifecycle.
//!
//! `init` builds the services through [`build_services`] (the same function the
//! test harness uses). `serve` (DESIGN §3.2 "Gear lifecycle") exchanges the
//! `client_credentials` for the S2S context, provisions the OAGW upstreams and
//! routes (deferred providers are retried in the background), starts the outbox
//! pipeline, checks the summary model, then spawns the orphan watchdog and the
//! upload reaper (the reaper enqueues cleanup messages). On cancellation it
//! cancels the gear-stop token (background indexing, workers, reconcile), joins
//! the tasks with a bounded timeout and stops the outbox pipeline.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use tracing::{info, warn};

use crate::config::MiniChatConfig;
use crate::domain::authz::ChatAuthz;
use crate::domain::error::DomainError;
use crate::domain::services::{ServiceDeps, Services, UploadTimings, build_services};
use crate::infra::gateways::audit::{AuditGateway, PluginAuditGateway};
use crate::infra::gateways::model_policy::{ModelPolicyGateway, PluginModelPolicyGateway};
use crate::infra::llm::{LlmClient, ProviderResolver, RagClient};
use crate::infra::oagw::provisioning::Provisioner;
use crate::infra::oagw::s2s::{S2sBootstrap, S2sContext};
use crate::infra::outbox::{OutboxEnqueuer, OutboxHandlers, start_outbox};
use crate::infra::workers::leader::{LeaderElector, NoopElector, default_elector};

/// Bound on joining the background tasks at stop.
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// State built by `init`.
struct Wiring {
    config: Arc<MiniChatConfig>,
    authn: Arc<dyn AuthNResolverClient>,
    provisioner: Arc<Provisioner>,
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
    policy: Arc<dyn ModelPolicyGateway>,
    audit: Arc<dyn AuditGateway>,
    services: Services,
    /// Root "gear stop" token of the background tasks (background indexing,
    /// orphan watchdog, upload reaper, OAGW reconcile).
    stop: CancellationToken,
}

/// The `mini-chat` gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct MiniChatGear {
    wiring: OnceLock<Wiring>,
}

impl Default for MiniChatGear {
    fn default() -> Self {
        Self {
            wiring: OnceLock::new(),
        }
    }
}

impl MiniChatGear {
    fn wiring(&self) -> anyhow::Result<&Wiring> {
        self.wiring
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} not initialized", Self::MODULE_NAME))
    }

    /// Start the gear (outbox pipeline → S2S context → OAGW provisioning →
    /// summary model check → workers), report ready, and stop on cancellation.
    ///
    /// The outbox needs only the database, so it starts first: a request that
    /// arrives as soon as the routes are live can enqueue. Its provider-calling
    /// queues are held until the S2S context and the OAGW upstreams exist.
    #[allow(
        clippy::redundant_pub_crate,
        reason = "serve entry point invoked by the toolkit runtime"
    )]
    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let wiring = self.wiring()?;
        let (providers_ready, providers_ready_rx) = watch::channel(false);
        let handle = start_outbox(
            wiring.db.db(),
            &wiring.config,
            OutboxHandlers::with_gateways(Arc::clone(&wiring.policy), Arc::clone(&wiring.audit))
                .with_thread_summary(Arc::clone(&wiring.services.thread_summary))
                .with_cleanup(Arc::clone(&wiring.services.cleanup))
                .gate_provider_queues(&providers_ready_rx),
        )
        .await?;
        wiring.outbox.set_outbox(Arc::clone(handle.outbox()));
        info!("mini-chat outbox pipeline started");

        let start_providers = async {
            let s2s = S2sBootstrap::exchange(
                Arc::clone(&wiring.authn),
                &wiring.config.client_credentials,
            )
            .await?;
            wiring.services.s2s.set(s2s.clone());
            provision(wiring, s2s).await
        };
        let started = tokio::select! {
            () = cancel.cancelled() => Ok(None),
            r = start_providers => r.map(Some),
        };
        let mut tasks = match started {
            Ok(Some(tasks)) => tasks,
            other => {
                // Release the held handlers, then stop the outbox.
                drop(providers_ready);
                wiring.stop.cancel();
                handle.stop().await;
                return other.map(drop);
            }
        };
        providers_ready.send_replace(true);
        if wiring.config.thread_summary_worker.enabled {
            // Logged only; startup does not wait for the policy plugin.
            let summaries = Arc::clone(&wiring.services.thread_summary);
            let stop = wiring.stop.clone();
            tasks.push(tokio::spawn(async move {
                tokio::select! {
                    () = stop.cancelled() => {}
                    _ = summaries.check_summary_model() => {}
                }
            }));
        }
        tasks.extend(spawn_workers(wiring));

        ready.notify();
        cancel.cancelled().await;

        info!("mini-chat stopping background tasks and the outbox pipeline");
        wiring.stop.cancel();
        join_tasks(tasks).await;
        handle.stop().await;
        Ok(())
    }
}

/// Register the OAGW upstreams and routes; deferred providers get a background
/// reconcile task (returned) that ends with the gear-stop token.
async fn provision(
    wiring: &Wiring,
    s2s: SecurityContext,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let deferred = wiring.provisioner.provision_all(s2s).await?;
    info!("mini-chat OAGW provisioning done");
    if deferred.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![
        Arc::clone(&wiring.provisioner).spawn_reconcile(deferred, wiring.stop.clone()),
    ])
}

/// Spawn the enabled leader-only workers; they end with the gear-stop token.
fn spawn_workers(wiring: &Wiring) -> Vec<tokio::task::JoinHandle<()>> {
    let mut workers = Vec::new();
    if wiring.config.orphan_watchdog.enabled {
        workers.push(Arc::clone(&wiring.services.orphan_watchdog).spawn(wiring.stop.clone()));
    }
    if wiring.config.upload_reaper.enabled {
        workers.push(Arc::clone(&wiring.services.upload_reaper).spawn(wiring.stop.clone()));
    }
    workers
}

/// Wait (bounded) for the background tasks to end (their stop token is cancelled).
async fn join_tasks(tasks: Vec<tokio::task::JoinHandle<()>>) {
    let join = async {
        for task in tasks {
            if let Err(err) = task.await {
                warn!(%err, "mini-chat background task ended abnormally");
            }
        }
    };
    if tokio::time::timeout(TASK_JOIN_TIMEOUT, join).await.is_err() {
        warn!("mini-chat background tasks did not stop in time");
    }
}

/// Whether a worker that runs under the leader elector is enabled.
fn cfg_leader_only_workers(config: &MiniChatConfig) -> bool {
    config.orphan_watchdog.enabled || config.upload_reaper.enabled
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(gear = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.apply_defaults();
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for warning in cfg.deprecation_warnings() {
            warn!("{warning}");
        }

        let db_raw = ctx.db_required()?;
        let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));

        let authz_client = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let authz = Arc::new(ChatAuthz::new(PolicyEnforcer::new(authz_client)));

        let policy: Arc<dyn ModelPolicyGateway> = Arc::new(PluginModelPolicyGateway::new(
            ctx.client_hub(),
            cfg.vendor.clone(),
        ));
        let audit: Arc<dyn AuditGateway> = Arc::new(PluginAuditGateway::new(
            ctx.client_hub(),
            cfg.vendor.clone(),
        ));
        let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
        let config = Arc::new(cfg);

        let gateway = ctx
            .client_hub()
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = ctx
            .client_hub()
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;
        // Filled with the `client_credentials` S2S context when the gear starts.
        let s2s = Arc::new(S2sContext::new());
        // Registers the upstreams/routes at start, routes the resolver by the
        // aliases OAGW assigns and provisions deferred providers on demand.
        let providers = Arc::new(ProviderResolver::new(&config));
        let provisioner = Arc::new(Provisioner::new(
            Arc::clone(&gateway),
            &config,
            Arc::clone(&providers),
            Arc::clone(&s2s),
        ));
        let llm = Arc::new(
            LlmClient::new(Arc::clone(&gateway), Arc::clone(&s2s))
                .with_provisioner(Arc::clone(&provisioner)),
        );
        let rag = Arc::new(
            RagClient::new(gateway, Arc::clone(&s2s)).with_provisioner(Arc::clone(&provisioner)),
        );
        let stop = CancellationToken::new();
        // Leader-only workers: a Lease elector with the `k8s` feature, else no-op.
        let elector: Arc<dyn LeaderElector> = if cfg_leader_only_workers(&config) {
            default_elector().await?
        } else {
            Arc::new(NoopElector)
        };

        let services = build_services(ServiceDeps {
            config: Arc::clone(&config),
            db: Arc::clone(&db),
            authz,
            policy: Arc::clone(&policy),
            outbox: Arc::clone(&outbox),
            llm,
            s2s,
            rag,
            providers,
            upload_timings: UploadTimings::default(),
            stop: stop.clone(),
            elector,
        });

        self.wiring
            .set(Wiring {
                config,
                authn,
                provisioner,
                db,
                outbox,
                policy,
                audit,
                services,
                stop,
            })
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        info!("mini-chat initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        crate::infra::db::all_migrations()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let wiring = self.wiring()?;
        Ok(crate::api::rest::routes::register_routes(
            router,
            openapi,
            wiring.services.clone(),
            &wiring.config,
        ))
    }
}
