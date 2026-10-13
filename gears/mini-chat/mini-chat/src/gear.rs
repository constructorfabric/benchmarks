//! Gear definition: registration, lifecycle and capability wiring.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use oagw_sdk::ServiceGatewayClientV1;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::outbox::OutboxHandle;

use crate::api::state::AppServices;
use crate::config::{MiniChatConfig, ThreadSummaryWorkerConfig};
use crate::domain::thread_summary::{resolve_summary_model, summary_model_id};
use crate::infra::gateways::audit::PluginAuditGateway;
use crate::infra::gateways::policy::{PluginPolicyGateway, PolicyGateway};
use crate::infra::llm::provisioning::{provision_all, spawn_reconcile};
use crate::infra::llm::s2s::obtain_s2s_context;
use crate::infra::workers::leader::LeaderElector;
use crate::infra::workers::{OrphanWatchdog, UploadReaper};
use crate::wiring::{ServiceDeps, build_services, default_outbox_handlers, start_outbox_pipeline};

/// How long `serve` waits for the background workers after cancellation.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// First and longest wait between policy catalog lookups of the startup summary-model check.
const SUMMARY_CHECK_BACKOFF: (Duration, Duration) =
    (Duration::from_secs(1), Duration::from_secs(30));

/// Multi-tenant AI chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
#[derive(Default)]
pub struct MiniChatGear {
    config: OnceLock<Arc<MiniChatConfig>>,
    services: OnceLock<Arc<AppServices>>,
    /// Client of the S2S client-credentials exchange (its plugin may not be ready until `serve`).
    authn: OnceLock<Arc<dyn AuthNResolverClient>>,
    /// The outbox pipeline, started in `init` and stopped by `serve` after cancellation.
    outbox: Mutex<Option<OutboxHandle>>,
}

impl MiniChatGear {
    /// Lifecycle entry: exchanges the S2S credentials, provisions OAGW, reports readiness and
    /// waits for cancellation.
    ///
    /// Before `ready`: the S2S exchange (retried while the authn plugin is unavailable; a
    /// cancellation ends `serve` cleanly) and the OAGW provisioning (a deterministic failure fails
    /// startup; providers whose secret is not readable yet are retried by a background task that
    /// stops on cancel). The orphan watchdog and the upload reaper (when enabled) start after
    /// provisioning, under the leader elector; on cancellation `serve` waits up to 10 s for them
    /// before it stops the outbox. The summary-model check runs in the background once the
    /// policy catalog is available and only logs (DESIGN "Thread Summary Update").
    ///
    /// # Errors
    /// Returns an error when invoked before `init`, when the credentials are rejected or when
    /// provisioning fails deterministically.
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let (Some(cfg), Some(services), Some(authn)) =
            (self.config.get(), self.services.get(), self.authn.get())
        else {
            anyhow::bail!("{}: serve invoked before init", Self::MODULE_NAME);
        };

        let elector = match build_elector(cfg).await {
            Ok(elector) => elector,
            Err(err) => {
                self.stop_outbox().await?;
                return Err(err);
            }
        };
        let started = self
            .start_providers(cfg, services, authn.as_ref(), &cancel)
            .await;
        let reconcile = match started {
            Ok(reconcile) => reconcile,
            Err(err) => {
                self.stop_outbox().await?;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                return Err(err);
            }
        };
        let summary_check = {
            let (summary_cfg, policy, cancel) = (
                cfg.thread_summary_worker.clone(),
                Arc::clone(&services.policy),
                cancel.clone(),
            );
            tokio::spawn(async move {
                check_summary_model(summary_cfg, policy.as_ref(), &cancel).await;
            })
        };
        let workers = start_workers(cfg, services, &elector, &cancel);
        ready.notify();
        cancel.cancelled().await;
        // Background indexing waits end with the gear; their rows are left to the upload reaper.
        services.attachments.shutdown();
        // These tasks end by cancellation; a panic in one must not fail the shutdown.
        summary_check.await.ok();
        if let Some(task) = reconcile {
            task.await.ok();
        }
        // The workers enqueue outbox messages: they end before the pipeline stops.
        join_workers(workers).await;
        self.stop_outbox().await
    }

    /// S2S exchange, provisioning and the reconcile task for deferred providers.
    async fn start_providers(
        &self,
        cfg: &MiniChatConfig,
        services: &AppServices,
        authn: &dyn AuthNResolverClient,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
        let ctx = obtain_s2s_context(authn, &cfg.client_credentials, cancel).await?;
        services.s2s.set(ctx.clone());
        let report = provision_all(services.gateway.as_ref(), &ctx, &cfg.providers).await?;
        Ok(spawn_reconcile(
            Arc::clone(&services.gateway),
            services.s2s.clone(),
            cfg.providers.clone(),
            report.deferred,
            cancel.clone(),
        ))
    }

    /// Stops the outbox pipeline started by `init`, if still running.
    async fn stop_outbox(&self) -> anyhow::Result<()> {
        // Release the lock before awaiting the stop.
        let outbox = self
            .outbox
            .lock()
            .map_err(|_| anyhow::anyhow!("outbox lock poisoned"))?
            .take();
        if let Some(handle) = outbox {
            handle.stop().await;
        }
        Ok(())
    }
}

/// Outcome of the startup summary-model check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryModelCheck {
    /// Thread summaries are disabled: nothing to check.
    Off,
    /// The summary model is enabled in the policy catalog.
    Present,
    /// The summary model is missing from the catalog or disabled (an error is logged).
    Missing,
    /// Cancelled before the catalog became available.
    Cancelled,
}

/// Resolves the summary model once the policy catalog is available (retried with backoff while
/// the lookup fails; `cancel` ends a wait or a lookup in flight) and logs an error when it is missing or disabled. Never fails:
/// a dynamic policy plugin can add the model later, and summary tasks are rejected meanwhile.
async fn check_summary_model(
    cfg: ThreadSummaryWorkerConfig,
    policy: &dyn PolicyGateway,
    cancel: &CancellationToken,
) -> SummaryModelCheck {
    if !cfg.enabled {
        return SummaryModelCheck::Off;
    }
    let (mut wait, max_wait) = SUMMARY_CHECK_BACKOFF;
    loop {
        // The lookup has no deadline of its own: cancellation also ends one in flight.
        let looked_up = tokio::select! {
            () = cancel.cancelled() => return SummaryModelCheck::Cancelled,
            outcome = lookup_summary_model(&cfg, policy) => outcome,
        };
        if let Some(outcome) = looked_up {
            return outcome;
        }
        tokio::select! {
            () = cancel.cancelled() => return SummaryModelCheck::Cancelled,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(max_wait);
    }
}

/// One lookup of [`check_summary_model`]; `None` when the catalog is not available.
async fn lookup_summary_model(
    cfg: &ThreadSummaryWorkerConfig,
    policy: &dyn PolicyGateway,
) -> Option<SummaryModelCheck> {
    let model = summary_model_id(cfg);
    match resolve_summary_model(policy, cfg).await {
        Ok(Some(_)) => Some(SummaryModelCheck::Present),
        Ok(None) => {
            report_missing_summary_model(model);
            Some(SummaryModelCheck::Missing)
        }
        Err(err) => {
            tracing::debug!(model, error = %err, "policy catalog not available yet; summary model check retried");
            None
        }
    }
}

fn report_missing_summary_model(model: &str) {
    tracing::error!(
        model,
        "thread summary model is missing from the policy catalog or disabled; \
         thread summary tasks are rejected until the policy provides it"
    );
}

/// The elector of the background workers: a Kubernetes Lease elector when built with the `k8s`
/// feature and at least one worker is enabled (needs `POD_NAMESPACE` and `POD_NAME`), the no-op
/// elector otherwise.
#[cfg(feature = "k8s")]
async fn build_elector(cfg: &MiniChatConfig) -> anyhow::Result<Arc<dyn LeaderElector>> {
    if !cfg.orphan_watchdog.enabled && !cfg.upload_reaper.enabled {
        return Ok(Arc::new(crate::infra::workers::NoopElector));
    }
    let elector = crate::infra::workers::LeaseElector::from_env().await?;
    Ok(Arc::new(elector))
}

#[cfg(not(feature = "k8s"))]
#[allow(clippy::unused_async)] // the `k8s` variant builds a client
async fn build_elector(_cfg: &MiniChatConfig) -> anyhow::Result<Arc<dyn LeaderElector>> {
    Ok(Arc::new(crate::infra::workers::NoopElector))
}

/// Spawns the enabled background workers.
fn start_workers(
    cfg: &MiniChatConfig,
    services: &AppServices,
    elector: &Arc<dyn LeaderElector>,
    cancel: &CancellationToken,
) -> JoinSet<()> {
    let mut workers = JoinSet::new();
    if cfg.orphan_watchdog.enabled {
        let watchdog = OrphanWatchdog::new(services);
        let interval = Duration::from_secs(cfg.orphan_watchdog.scan_interval_secs);
        workers.spawn(watchdog.run(Arc::clone(elector), interval, cancel.clone()));
    }
    if cfg.upload_reaper.enabled {
        let reaper = UploadReaper::new(services);
        let interval = Duration::from_secs(cfg.upload_reaper.scan_interval_secs);
        workers.spawn(reaper.run(Arc::clone(elector), interval, cancel.clone()));
    }
    workers
}

/// Waits for the workers to finish after cancellation; stragglers are aborted after
/// [`WORKER_JOIN_TIMEOUT`].
async fn join_workers(mut workers: JoinSet<()>) {
    let joined = tokio::time::timeout(WORKER_JOIN_TIMEOUT, async {
        while let Some(result) = workers.join_next().await {
            if let Err(err) = result {
                tracing::warn!(error = %err, "background worker ended abnormally");
            }
        }
    })
    .await;
    if joined.is_err() {
        tracing::warn!("background workers did not stop in time; aborting them");
        workers.abort_all();
    }
}

#[async_trait]
impl Gear for MiniChatGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut cfg: MiniChatConfig = ctx.config_expanded_or_default()?;
        cfg.normalize();
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("{} config invalid: {err}", Self::MODULE_NAME))?;
        for warning in cfg.deprecation_warnings() {
            tracing::warn!(gear = Self::MODULE_NAME, "{warning}");
        }

        if self.config.get().is_some() {
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }
        let cfg = Arc::new(cfg);

        let db = ctx.db_required()?.db();
        let outbox_db = db.clone();
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
        // Plugins are resolved lazily on first use: they may init after this gear.
        let policy = Arc::new(PluginPolicyGateway::new(
            Arc::clone(&hub),
            cfg.vendor.clone(),
        ));
        let audit = Arc::new(PluginAuditGateway::new(
            Arc::clone(&hub),
            cfg.vendor.clone(),
        ));
        let services = build_services(ServiceDeps {
            cfg: Arc::clone(&cfg),
            db,
            hub,
            authz_client,
            gateway,
            policy,
            audit,
            indexing_timings: crate::domain::attachment::IndexingTimings::default(),
            metrics: None,
        })?;

        let handlers = default_outbox_handlers(&services);
        let handle = start_outbox_pipeline(&services, outbox_db, handlers, None).await?;
        *self
            .outbox
            .lock()
            .map_err(|_| anyhow::anyhow!("outbox lock poisoned"))? = Some(handle);

        self.authn
            .set(authn)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.services
            .set(services)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.config
            .set(cfg)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        tracing::info!(gear = Self::MODULE_NAME, "initialized");
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        let mut migrations = crate::infra::db::migrations::migrations();
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
        let services = self.services.get().cloned().ok_or_else(|| {
            anyhow::anyhow!("{}: register_rest invoked before init", Self::MODULE_NAME)
        })?;
        let url_prefix = services.cfg.url_prefix.clone();
        Ok(crate::api::routes::register_routes(
            router,
            openapi,
            services,
            &url_prefix,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use authn_resolver_sdk::{AuthNResolverClient, AuthNResolverError};
    use authz_resolver_sdk::AuthZResolverApi;
    use axum::body::Body;
    use mini_chat_sdk::{
        MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, TierLimits,
    };
    use oagw_sdk::ServiceGatewayClientV1;
    use serde_json::{Value, json};
    use std::time::Duration;
    use toolkit::api::OpenApiRegistryImpl;
    use toolkit::client_hub::ClientScope;
    use toolkit::config::ConfigProvider;
    use toolkit::gts::PluginV1;
    use tower::ServiceExt;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use uuid::Uuid;

    use crate::test_support::app::{NO_KILL_SWITCHES, RecordingPolicy};
    use crate::test_support::authn::{FakeAuthn, S2S_SUBJECT};
    use crate::test_support::catalog::test_catalog;
    use crate::test_support::db::test_memory_db;
    use crate::test_support::gateway::{self, FakeGateway};
    use crate::test_support::pdp::{FakePdp, PdpMode};

    struct TestConfig(Value);

    impl ConfigProvider for TestConfig {
        fn get_gear_config(&self, gear: &str) -> Option<&Value> {
            self.0.get(gear)
        }
    }

    /// Context with a migrated database and the clients `init` resolves from the hub.
    async fn ctx_with_hub(config: Value, hub: Arc<toolkit::ClientHub>) -> GearCtx {
        let authz: Arc<dyn AuthZResolverApi> = Arc::new(FakePdp::new(PdpMode::TenantConstraint));
        hub.register::<dyn AuthZResolverApi>(authz);
        let gateway: Arc<dyn ServiceGatewayClientV1> = Arc::new(FakeGateway::new());
        hub.register::<dyn ServiceGatewayClientV1>(gateway);
        let exchange: Arc<dyn AuthNResolverClient> = Arc::new(FakeAuthn::new());
        hub.register::<dyn AuthNResolverClient>(exchange);
        GearCtx::new(
            MiniChatGear::MODULE_NAME,
            Uuid::new_v4(),
            Arc::new(TestConfig(config)),
            hub,
            CancellationToken::new(),
        )
        .with_db(toolkit_db::DBProvider::new(test_memory_db().await))
    }

    async fn ctx(config: Value) -> GearCtx {
        ctx_with_hub(config, Arc::new(toolkit::ClientHub::new())).await
    }

    fn valid_config() -> Value {
        json!({"mini-chat": {"config": {
            "client_credentials": {"client_id": "mini-chat", "client_secret": "s"}
        }}})
    }

    #[tokio::test]
    async fn init_loads_validates_and_serve_waits_for_cancel() {
        let gear = Arc::new(MiniChatGear::default());
        gear.init(&ctx(valid_config()).await).await.unwrap();
        assert!(
            gear.init(&ctx(valid_config()).await).await.is_err(),
            "second init must fail"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        rx.await.expect("serve must notify ready");
        assert!(
            !task.is_finished(),
            "serve must keep running until cancelled"
        );
        // `init` started the pipeline: the enqueuer is connected.
        let rec = crate::infra::outbox::OutboxRecord::usage(
            &crate::test_support::fixtures::usage_event(Uuid::new_v4()),
        )
        .unwrap();
        let services = Arc::clone(gear.services.get().unwrap());
        let outbox = Arc::clone(&services.outbox);
        crate::infra::db::tx::write_tx_with_wakes(&services.db, move |tx, wakes| {
            let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
            Box::pin(async move {
                wakes.add(outbox.enqueue(tx, rec).await?);
                Ok(())
            })
        })
        .await
        .expect("enqueue after init");
        assert!(gear.outbox.lock().unwrap().is_some());

        cancel.cancel();
        task.await.unwrap().unwrap();
        assert!(
            gear.outbox.lock().unwrap().is_none(),
            "serve stops the pipeline after cancel"
        );
    }

    #[tokio::test]
    async fn serve_runs_the_workers_and_joins_them_on_cancel() {
        use crate::infra::db::entity::{attachments, chats};
        use crate::infra::db::ts::db_now;
        use crate::test_support::workers::{SeedTurn, SeedUpload, seed_running_turn, seed_upload};
        use sea_orm::ActiveValue::Set;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use toolkit_db::secure::{AccessScope, SecureEntityExt as _, secure_insert};

        let (gear, _gateway, _authn) = scripted().await;
        let services = Arc::clone(gear.services.get().unwrap());
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = Uuid::new_v4();
        let now = db_now();
        let conn = services.db.conn().unwrap();
        secure_insert::<chats::Entity>(
            chats::ActiveModel {
                id: Set(chat),
                tenant_id: Set(tenant),
                user_id: Set(user),
                model: Set("gpt-premium".to_owned()),
                title: Set(None),
                is_temporary: Set(false),
                created_at: Set(now),
                updated_at: Set(now),
                deleted_at: Set(None),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
        // Rows a crashed process left behind: a stale upload and a stale running turn (a retry
        // turn without reserve fields, so no policy plugin is needed).
        let stale = now - time::Duration::minutes(30);
        let upload = SeedUpload {
            tenant,
            chat,
            uploader: user,
            status: "pending",
            provider_file_id: None,
            updated_at: stale,
            cleanup_status: None,
        };
        let db = services.db.db();
        let upload_id = seed_upload(&db, &upload).await;
        let (turn_id, _) =
            seed_running_turn(&db, &SeedTurn::unreserved(tenant, chat, user, stale)).await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        rx.await.expect("ready");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let conn = services.db.conn().unwrap();
            let upload = attachments::Entity::find()
                .filter(attachments::Column::Id.eq(upload_id))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .one(&conn)
                .await
                .unwrap()
                .unwrap();
            let turn = crate::infra::db::entity::chat_turns::Entity::find()
                .filter(crate::infra::db::entity::chat_turns::Column::Id.eq(turn_id))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .one(&conn)
                .await
                .unwrap()
                .unwrap();
            if upload.status == "failed" && turn.state == "failed" {
                assert_eq!(upload.error_code.as_deref(), Some("upload_abandoned"));
                assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "workers never ran: {upload:?} {turn:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let cancelled_at = std::time::Instant::now();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("serve joins the workers within the bound")
            .unwrap()
            .unwrap();
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(5),
            "serve took {:?} to stop after cancel",
            cancelled_at.elapsed()
        );
        assert!(
            gear.outbox.lock().unwrap().is_none(),
            "outbox stopped after the workers"
        );
    }

    /// A hub with scripted fakes, a context over it and the fakes themselves.
    async fn scripted() -> (Arc<MiniChatGear>, Arc<FakeGateway>, Arc<FakeAuthn>) {
        let hub = Arc::new(toolkit::ClientHub::new());
        let gateway = Arc::new(FakeGateway::new());
        let authn = Arc::new(FakeAuthn::new());
        let ctx = ctx_with_hub(valid_config(), Arc::clone(&hub)).await;
        // Replace the defaults `ctx_with_hub` registered.
        hub.register::<dyn ServiceGatewayClientV1>(gateway.clone());
        hub.register::<dyn AuthNResolverClient>(authn.clone());
        let gear = Arc::new(MiniChatGear::default());
        gear.init(&ctx).await.unwrap();
        (gear, gateway, authn)
    }

    #[tokio::test]
    async fn serve_exchanges_credentials_and_provisions_before_ready() {
        let (gear, gateway, authn) = scripted().await;
        let services = Arc::clone(gear.services.get().unwrap());
        assert!(services.s2s.get().is_err(), "no S2S context before serve");

        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        rx.await.expect("ready");

        assert_eq!(
            authn.exchanges(),
            [("mini-chat".to_owned(), "s".to_owned())]
        );
        assert_eq!(services.s2s.get().unwrap().subject_id(), S2S_SUBJECT);
        assert_eq!(gateway.upstreams().len(), 1, "provisioned before ready");
        assert_eq!(gateway.upstreams()[0].alias, "api.openai.com");
        assert_eq!(gateway.routes().len(), 3);

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn serve_fails_startup_on_a_fatal_provisioning_error() {
        let (gear, gateway, _authn) = scripted().await;
        gateway.script_create_upstream(vec![Err(gateway::validation_error())]);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let err = gear
            .serve(CancellationToken::new(), ReadySignal::from_sender(tx))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("openai"), "{err}");
        assert!(rx.await.is_err(), "ready must not be signalled");
    }

    #[tokio::test]
    async fn serve_fails_startup_when_the_token_exchange_fails() {
        let (gear, gateway, authn) = scripted().await;
        authn.script(vec![Err(AuthNResolverError::TokenAcquisitionFailed(
            "invalid client credentials".to_owned(),
        ))]);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let err = gear
            .serve(CancellationToken::new(), ReadySignal::from_sender(tx))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid client credentials"), "{err}");
        assert!(rx.await.is_err());
        assert!(gateway.upstreams().is_empty());
    }

    #[tokio::test]
    async fn cancel_while_waiting_for_the_authn_plugin_ends_serve_cleanly() {
        let (gear, _gateway, authn) = scripted().await;
        authn.script(
            (0..1000)
                .map(|_| Err(AuthNResolverError::NoPluginAvailable))
                .collect(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while authn.calls() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "serve never called the authn client"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !task.is_finished(),
            "serve keeps retrying while the plugin is missing"
        );
        cancel.cancel();
        task.await.unwrap().unwrap();
        assert!(rx.await.is_err(), "never became ready");
        assert!(gear.outbox.lock().unwrap().is_none(), "outbox stopped");
    }

    #[tokio::test]
    async fn serve_reconciles_a_deferred_provider_in_the_background() {
        let (gear, gateway, _authn) = scripted().await;
        gateway.script_create_upstream(vec![Err(gateway::secret_not_readable())]);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        rx.await
            .expect("a deferred provider does not block startup");
        assert!(gateway.upstreams().is_empty(), "not provisioned yet");

        // First retry runs 2 s after start (real time: the gear owns a database).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while gateway.upstreams().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "reconcile never provisioned"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(gateway.routes().len(), 3);

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn init_fails_without_client_credentials() {
        let gear = MiniChatGear::default();
        let err = gear
            .init(&ctx(json!({})).await)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("client_credentials"), "{err}");
    }

    #[tokio::test]
    async fn serve_before_init_fails() {
        let gear = Arc::new(MiniChatGear::default());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let res = gear
            .serve(CancellationToken::new(), ReadySignal::from_sender(tx))
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn register_rest_serves_models_through_the_policy_plugin() {
        let hub = Arc::new(toolkit::ClientHub::new());
        let ctx = ctx_with_hub(valid_config(), Arc::clone(&hub)).await;
        let gear = MiniChatGear::default();
        assert!(
            gear.register_rest(&ctx, axum::Router::new(), &OpenApiRegistryImpl::new())
                .is_err(),
            "register_rest before init"
        );
        gear.init(&ctx).await.unwrap();
        let router = gear
            .register_rest(&ctx, axum::Router::new(), &OpenApiRegistryImpl::new())
            .unwrap();

        // The policy plugin is resolved lazily (after init) via types-registry by vendor.
        register_policy_plugin(&hub, Arc::new(recording_policy()));

        let mut req = http::Request::get("/mini-chat/v1/models")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(crate::test_support::app::ctx(
            Uuid::new_v4(),
            Uuid::new_v4(),
        ));
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 3);
    }

    /// A policy plugin serving [`test_catalog`].
    fn recording_policy() -> RecordingPolicy {
        let limits = TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 1,
        };
        RecordingPolicy::new(test_catalog(), NO_KILL_SWITCHES, limits, limits)
    }

    /// Registers `plugin` as the `constructorfabric` policy plugin (types-registry + scoped client).
    fn register_policy_plugin(
        hub: &toolkit::ClientHub,
        plugin: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    ) {
        let (id, json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.test_policy.v1",
            "constructorfabric",
            1,
        )
        .unwrap();
        let id = id.to_string();
        let registry: Arc<dyn TypesRegistryClient> = Arc::new(
            MockTypesRegistryClient::new().with_instances([make_test_instance(&id, json)]),
        );
        hub.register::<dyn TypesRegistryClient>(registry);
        hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
            ClientScope::gts_id(&id),
            plugin,
        );
    }

    fn summary_cfg(model: &str) -> crate::config::ThreadSummaryWorkerConfig {
        crate::config::ThreadSummaryWorkerConfig {
            summary_model_id: model.to_owned(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn summary_model_check_reports_present_missing_disabled_and_off() {
        use crate::infra::gateways::policy::DirectPolicyGateway;
        let plugin = Arc::new(recording_policy());
        let policy = DirectPolicyGateway(plugin.clone());
        let cancel = CancellationToken::new();
        let check = |model: &str| check_summary_model(summary_cfg(model), &policy, &cancel);

        assert_eq!(check("gpt-standard").await, SummaryModelCheck::Present);
        assert_eq!(check("gpt-disabled").await, SummaryModelCheck::Missing);
        assert_eq!(check("no-such-model").await, SummaryModelCheck::Missing);
        // Empty id: the default summary model, absent from the test catalog.
        assert_eq!(check("").await, SummaryModelCheck::Missing);

        let calls = plugin.calls().len();
        let mut off = summary_cfg("gpt-standard");
        off.enabled = false;
        assert_eq!(
            check_summary_model(off, &policy, &cancel).await,
            SummaryModelCheck::Off
        );
        assert_eq!(
            plugin.calls().len(),
            calls,
            "no lookup when summaries are off"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn summary_model_check_waits_for_the_catalog_and_ends_on_cancel() {
        use crate::infra::gateways::policy::DirectPolicyGateway;
        let plugin = Arc::new(recording_policy());
        plugin.fail_snapshots(true);
        let policy = Arc::new(DirectPolicyGateway(plugin.clone()));

        // The catalog becomes available after a few failed lookups.
        let task = {
            let policy = Arc::clone(&policy);
            tokio::spawn(async move {
                check_summary_model(
                    summary_cfg("gpt-standard"),
                    policy.as_ref(),
                    &CancellationToken::new(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            !task.is_finished(),
            "keeps waiting while the catalog is unavailable"
        );
        let lookups = plugin.calls().len();
        assert!(lookups >= 2, "retried: {lookups} calls");
        plugin.fail_snapshots(false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(task.await.unwrap(), SummaryModelCheck::Present);

        // Cancelled while the catalog is unavailable.
        plugin.fail_snapshots(true);
        let cancel = CancellationToken::new();
        let task = {
            let (policy, cancel) = (Arc::clone(&policy), cancel.clone());
            tokio::spawn(async move {
                check_summary_model(summary_cfg("gpt-standard"), policy.as_ref(), &cancel).await
            })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), SummaryModelCheck::Cancelled);
    }

    /// A policy gateway whose lookups never complete.
    struct HangingPolicy;

    #[async_trait]
    impl PolicyGateway for HangingPolicy {
        async fn current_snapshot(
            &self,
            _user_id: Uuid,
        ) -> Result<mini_chat_sdk::PolicySnapshot, crate::domain::error::DomainError> {
            std::future::pending().await
        }
        async fn snapshot(
            &self,
            _user_id: Uuid,
            _version: i64,
        ) -> Result<mini_chat_sdk::PolicySnapshot, crate::domain::error::DomainError> {
            std::future::pending().await
        }
        async fn user_limits(
            &self,
            _user_id: Uuid,
            _version: i64,
        ) -> Result<mini_chat_sdk::UserLimits, crate::domain::error::DomainError> {
            std::future::pending().await
        }
        async fn publish_usage(
            &self,
            _ev: mini_chat_sdk::UsageEvent,
        ) -> Result<(), crate::infra::gateways::policy::PublishOutcome> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn summary_model_check_ends_on_cancel_during_a_hanging_lookup() {
        let cancel = CancellationToken::new();
        let task = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                check_summary_model(summary_cfg("gpt-standard"), &HangingPolicy, &cancel).await
            })
        };
        // Let the check enter the lookup.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!task.is_finished(), "the lookup hangs");
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the check ends promptly after cancel")
            .unwrap();
        assert_eq!(outcome, SummaryModelCheck::Cancelled);
    }

    #[tokio::test]
    async fn serve_becomes_ready_and_stops_when_the_summary_model_is_missing() {
        let hub = Arc::new(toolkit::ClientHub::new());
        let ctx = ctx_with_hub(valid_config(), Arc::clone(&hub)).await;
        let plugin = Arc::new(recording_policy());
        register_policy_plugin(&hub, plugin.clone());
        let gear = Arc::new(MiniChatGear::default());
        gear.init(&ctx).await.unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let task =
            tokio::spawn(Arc::clone(&gear).serve(cancel.clone(), ReadySignal::from_sender(tx)));
        rx.await
            .expect("a missing summary model does not block startup");
        // The default summary model (`gpt-4.1-mini`) is not in the test catalog: the check
        // looked it up (and logged an error) without failing `serve`.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !plugin
            .calls()
            .iter()
            .any(|c| matches!(c, crate::test_support::plugins::PolicyCall::Snapshot { .. }))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the summary model was never looked up"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!task.is_finished());
        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn migrations_are_gear_schema_followed_by_outbox() {
        // One gear migration, then the platform outbox schema (default prefix).
        assert_eq!(MiniChatGear::default().migrations().len(), 2);
    }
}
