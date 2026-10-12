//! `ToolKit` gear registration, dependency wiring and lifecycle.
//!
//! * `init` loads and validates the configuration, builds the plugin
//!   gateways (plugins are resolved lazily through the types-registry),
//!   resolves the `authz_resolver`, `oagw` and `authn_resolver` clients from
//!   the `ClientHub` and builds the domain services.
//! * REST routes are registered in the separate REST registration step from
//!   the services built in `init`.
//! * `init` starts the outbox pipeline with the queues the implemented slices
//!   write to: usage (delivered to the model-policy plugin), audit (delivered
//!   to the audit plugin, lease 60 s), chat-cleanup and attachment-cleanup
//!   (both delete provider resources through the provider storage) and
//!   thread-summary (the summary model call, leased for
//!   `thread_summary_worker.claim_timeout_secs`). The thread-summary handler
//!   needs the provider layer and the policy gateway, so it is built before
//!   the pipeline starts, and its usage port is bound right after.
//! * The stateful `serve` entry is the lifecycle anchor: it exchanges the S2S
//!   `client_credentials` and provisions the OAGW upstreams and routes (a
//!   deterministic misconfiguration fails startup, a transient failure is
//!   retried by a background reconcile task), reports readiness and keeps
//!   running until the runtime cancels it, then stops the background task and
//!   the outbox. The orphan watchdog runs as a background task of `serve`
//!   (it enqueues usage and audit messages, so it starts after the outbox
//!   pipeline and stops before it); the upload reaper runs the same way (it
//!   enqueues attachment-cleanup messages), and the attachment service's
//!   background indexing tasks are cancelled before the outbox stops.
//!
//! Migrations are the domain migrations plus the platform outbox migrations.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use oagw_sdk::ServiceGatewayClientV1;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::DatabaseCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxHandle, Partitions};
use toolkit_security::constants::DEFAULT_SUBJECT_ID;

use crate::api::rest::{
    register_attachment_routes, register_chat_routes, register_models_routes,
    register_quota_routes, register_stream_routes, register_turn_mutation_routes,
};
use crate::config::MiniChatConfig;
use crate::domain::attachments::AttachmentService;
use crate::domain::chats::ChatsService;
use crate::domain::error::DomainError;
use crate::domain::messages::MessagesService;
use crate::domain::models::ModelsService;
use crate::domain::quota_status::QuotaStatusService;
use crate::domain::reactions::ReactionsService;
use crate::domain::streaming::StreamService;
use crate::domain::thread_summary::summary_model_id;
use crate::domain::turn_mutations::TurnMutationService;
use crate::domain::turns::TurnsService;
use crate::domain::upload_reaper::UploadReaper;
use crate::domain::watchdog::OrphanWatchdog;
use crate::infra::audit::AuditGateway;
use crate::infra::db::migrations::gear_migrations;
use crate::infra::llm::provisioning::{
    ProvisionReport, provision_with_credentials, reconcile_pending,
};
use crate::infra::llm::{LlmProviders, ProviderStorage};
use crate::infra::model_policy::ModelPolicyGateway;
use crate::infra::outbox::{
    AttachmentCleanupHandler, AttachmentCleanupOutbox, AuditOutbox, AuditOutboxHandler,
    ChatCleanupHandler, ChatCleanupOutbox, ThreadSummaryHandler, ThreadSummaryOutbox, UsageOutbox,
    UsageOutboxHandler, thread_summary_lease,
};

/// Lease of the audit queue (DESIGN 3.2 "Audit plugin and audit outbox"): the
/// plugin call is bounded by 30 s.
const AUDIT_LEASE: LeaseConfig = LeaseConfig {
    duration: std::time::Duration::from_secs(60),
    headroom: std::time::Duration::from_secs(2),
};

/// Everything `init` builds; shared by the REST registration and the
/// lifecycle entry (and extended by the slices that add repositories,
/// services and workers).
pub struct MiniChatRuntime {
    /// Validated gear configuration.
    pub config: Arc<MiniChatConfig>,
    /// Database handle of the gear (Secure ORM entities live in `infra::db`).
    pub db: Arc<DBProvider<DomainError>>,
    /// Policy enforcement point (`authz_resolver` client).
    pub enforcer: PolicyEnforcer,
    /// Gateway to the model-policy plugin.
    pub policy: Arc<ModelPolicyGateway>,
    /// Gateway to the audit plugin.
    pub audit: Arc<AuditGateway>,
    /// OAGW client used by the provider adapters.
    pub oagw: Arc<dyn ServiceGatewayClientV1>,
    /// `authn_resolver` client used for the S2S token exchange.
    pub authn: Arc<dyn AuthNResolverClient>,
    /// Models API service.
    pub models: Arc<ModelsService>,
    /// Chat CRUD, list and delete service.
    pub chats: Arc<ChatsService>,
    /// Message list service.
    pub messages: Arc<MessagesService>,
    /// Message reactions service.
    pub reactions: Arc<ReactionsService>,
    /// Streaming core (`POST .../messages:stream`).
    pub stream: Arc<StreamService>,
    /// Turn Status API service.
    pub turns: Arc<TurnsService>,
    /// Retry, edit and delete of the latest turn.
    pub mutations: Arc<TurnMutationService>,
    /// Orphan turn watchdog (run by `serve`).
    pub watchdog: Arc<OrphanWatchdog>,
    /// Quota status service (`GET /v1/quota/status`).
    pub quota: Arc<QuotaStatusService>,
    /// Attachment upload, read and delete, and the background indexing.
    pub attachments: Arc<AttachmentService>,
    /// Upload reaper (run by `serve`).
    pub reaper: Arc<UploadReaper>,
    /// Running outbox pipeline; taken and stopped when `serve` is cancelled.
    pub outbox: tokio::sync::Mutex<Option<OutboxHandle>>,
}

/// The mini-chat gear.
#[toolkit::gear(
    name = "mini-chat",
    deps = [types_registry, authn_resolver, authz_resolver, oagw],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
#[derive(Default)]
pub struct MiniChatGear {
    runtime: OnceLock<Arc<MiniChatRuntime>>,
}

impl MiniChatGear {
    #[allow(
        clippy::redundant_pub_crate,
        reason = "module-private serve entry-point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(runtime) = self.runtime.get() else {
            anyhow::bail!("mini-chat: serve invoked before init");
        };
        let report = provision_with_credentials(
            runtime.authn.as_ref(),
            runtime.oagw.as_ref(),
            &runtime.config,
        )
        .await
        .map_err(|error| anyhow::anyhow!("mini-chat: OAGW provisioning failed: {error}"))?;
        log_started(&runtime.config, &report);
        let reconcile = spawn_reconcile(runtime, report.pending, &cancel);
        // The outbox pipeline has been running since `init`.
        let watchdog = tokio::spawn(Arc::clone(&runtime.watchdog).run(cancel.child_token()));
        let reaper = tokio::spawn(Arc::clone(&runtime.reaper).run(cancel.child_token()));
        ready.notify();
        // Best effort, after readiness: a policy plugin that is slow to answer
        // does not delay startup, and shutdown interrupts the check.
        tokio::select! {
            () = check_summary_model(runtime) => {}
            () = cancel.cancelled() => {}
        }
        cancel.cancelled().await;
        shutdown(runtime, reconcile, watchdog, reaper).await;
        Ok(())
    }
}

fn log_started(config: &MiniChatConfig, report: &ProvisionReport) {
    tracing::info!(
        url_prefix = %config.url_prefix,
        vendor = %config.vendor,
        provisioned = ?report.provisioned,
        pending = ?report.pending,
        "mini-chat started"
    );
}

/// Logs an error when thread summaries are enabled and the summary model is
/// missing from the policy catalog or disabled (DESIGN 3.6 "Thread Summary
/// Update"). Startup continues: a dynamic policy plugin can add the model
/// later, and the handler resolves it again on every delivery.
async fn check_summary_model(runtime: &MiniChatRuntime) {
    let worker = &runtime.config.thread_summary_worker;
    if !worker.enabled {
        return;
    }
    let model_id = summary_model_id(worker);
    match runtime.policy.current_snapshot(DEFAULT_SUBJECT_ID).await {
        Ok(snapshot) => {
            if !snapshot
                .model_catalog
                .iter()
                .any(|entry| entry.id == model_id && entry.enabled)
            {
                tracing::error!(
                    model_id,
                    "the thread summary model is missing from the policy catalog or disabled; \
                     summary work items are rejected until it is available"
                );
            }
        }
        Err(error) => {
            tracing::warn!(%error, model_id, "the thread summary model could not be checked at startup");
        }
    }
}

/// Stops the reconcile task, the orphan watchdog, the upload reaper and the
/// background indexing tasks, then the outbox pipeline (they all enqueue into
/// it). The thread-summary handler runs inside the pipeline: it is the last
/// thing to stop, so a summary delivery that is cut short is redelivered after
/// the next start (its commit is one transaction and its provider call has no
/// side effect).
async fn shutdown(
    runtime: &MiniChatRuntime,
    reconcile: Option<tokio::task::JoinHandle<()>>,
    watchdog: tokio::task::JoinHandle<()>,
    reaper: tokio::task::JoinHandle<()>,
) {
    if let Some(task) = reconcile {
        join_task("OAGW reconcile", task).await;
    }
    join_task("orphan watchdog", watchdog).await;
    join_task("upload reaper", reaper).await;
    runtime.attachments.stop().await;
    let outbox = runtime.outbox.lock().await.take();
    if let Some(outbox) = outbox {
        outbox.stop().await;
    }
    tracing::info!("mini-chat stopped");
}

/// Waits for a background task; an abnormal end is only logged.
async fn join_task(name: &str, task: tokio::task::JoinHandle<()>) {
    if let Err(error) = task.await {
        tracing::warn!(%error, "{name} task ended abnormally");
    }
}

/// Retries the providers whose provisioning was deferred, until they are all
/// provisioned or the gear stops.
fn spawn_reconcile(
    runtime: &Arc<MiniChatRuntime>,
    pending: Vec<String>,
    cancel: &CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if pending.is_empty() {
        return None;
    }
    let runtime = Arc::clone(runtime);
    let cancel = cancel.child_token();
    Some(tokio::spawn(async move {
        reconcile_pending(
            runtime.authn.as_ref(),
            runtime.oagw.as_ref(),
            &runtime.config,
            pending,
            cancel,
        )
        .await;
    }))
}

#[async_trait]
impl Gear for MiniChatGear {
    #[tracing::instrument(skip_all, fields(gear = "mini-chat"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: MiniChatConfig = ctx.config_expanded_or_default()?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("mini-chat config invalid: {e}"))?;
        for key in config.deprecated_estimation_budget_keys() {
            tracing::warn!(
                key,
                "estimation_budgets.{key} is deprecated and ignored; the model catalog entry carries the budgets"
            );
        }

        let db = Arc::new(DBProvider::<DomainError>::new(ctx.db_required()?.db()));

        let hub = ctx.client_hub();
        let authz = hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        let enforcer = PolicyEnforcer::new(authz);
        let oagw = hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get ServiceGatewayClientV1: {e}"))?;
        let authn = hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthNResolverClient: {e}"))?;

        let config = Arc::new(config);
        let storage = Arc::new(
            ProviderStorage::new(Arc::clone(&oagw), &config)
                .with_service_identity(Arc::clone(&authn), &config.client_credentials),
        );
        let policy = Arc::new(ModelPolicyGateway::new(
            Arc::clone(&hub),
            config.vendor.clone(),
        ));
        let audit = Arc::new(AuditGateway::new(hub, config.vendor.clone()));
        let models = Arc::new(ModelsService::new(Arc::clone(&policy), enforcer.clone()));

        // The cleanup, usage and audit messages are enqueued in the transactions
        // of the requests, so the pipeline must be running before the first
        // request.
        let partitions = u16::try_from(config.outbox.num_partitions).map_err(|_| {
            anyhow::anyhow!(
                "outbox.num_partitions {} does not fit the outbox partition count",
                config.outbox.num_partitions
            )
        })?;
        let llm = Arc::new(
            LlmProviders::new(Arc::clone(&oagw), &config)
                .with_service_identity(Arc::clone(&authn), &config.client_credentials),
        );
        let summary_handler = ThreadSummaryHandler::new(
            Arc::clone(&db),
            Arc::clone(&policy),
            Arc::clone(&llm),
            Arc::clone(&config),
        );
        let outbox = Outbox::builder(db.db())
            .queue(&config.outbox.queue_name, Partitions::of(partitions))
            .leased(UsageOutboxHandler::new(Arc::clone(&policy)))
            .queue(&config.outbox.audit_queue_name, Partitions::of(partitions))
            .leased(AuditOutboxHandler::new(Arc::clone(&audit)))
            .lease(AUDIT_LEASE)
            .queue(
                &config.outbox.cleanup_queue_name,
                Partitions::of(partitions),
            )
            .leased(AttachmentCleanupHandler::new(
                Arc::clone(&db),
                Arc::clone(&storage),
                Arc::clone(&config),
            ))
            .queue(
                &config.outbox.chat_cleanup_queue_name,
                Partitions::of(partitions),
            )
            .leased(ChatCleanupHandler::new(
                Arc::clone(&db),
                Arc::clone(&storage),
                Arc::clone(&config),
            ))
            .queue(
                &config.outbox.thread_summary_queue_name,
                Partitions::of(partitions),
            )
            .leased(summary_handler.clone())
            .lease(thread_summary_lease(&config))
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start the mini-chat outbox: {e}"))?;
        let cleanup = Arc::new(ChatCleanupOutbox::new(
            Arc::clone(outbox.outbox()),
            config.outbox.chat_cleanup_queue_name.clone(),
            partitions,
        ));
        let chats = Arc::new(ChatsService::new(
            Arc::clone(&db),
            Arc::clone(&policy),
            cleanup,
            enforcer.clone(),
        ));
        let messages = Arc::new(MessagesService::new(Arc::clone(&db), enforcer.clone()));
        let reactions = Arc::new(ReactionsService::new(Arc::clone(&db), enforcer.clone()));
        let usage_outbox = Arc::new(UsageOutbox::new(
            Arc::clone(outbox.outbox()),
            config.outbox.queue_name.clone(),
            partitions,
        ));
        // The handler was registered before the pipeline existed: from here on
        // its commit transaction can enqueue the system usage event.
        summary_handler.bind_usage(Arc::clone(&usage_outbox));
        let thread_summary = Arc::new(ThreadSummaryOutbox::new(
            Arc::clone(outbox.outbox()),
            config.outbox.thread_summary_queue_name.clone(),
            partitions,
        ));
        let audit_outbox = Arc::new(AuditOutbox::new(
            Arc::clone(outbox.outbox()),
            config.outbox.audit_queue_name.clone(),
            partitions,
        ));
        let attachment_cleanup = Arc::new(AttachmentCleanupOutbox::new(
            Arc::clone(outbox.outbox()),
            config.outbox.cleanup_queue_name.clone(),
            partitions,
        ));
        let stream = Arc::new(
            StreamService::new(
                Arc::clone(&db),
                Arc::clone(&policy),
                llm,
                Arc::clone(&config),
                enforcer.clone(),
            )
            .with_outbox(Arc::clone(&usage_outbox), Arc::clone(&audit_outbox))
            .with_thread_summary(thread_summary),
        );
        let mutations = Arc::new(
            TurnMutationService::new(Arc::clone(&db), Arc::clone(&stream), enforcer.clone())
                .with_audit(Arc::clone(&audit_outbox)),
        );
        let watchdog = Arc::new(
            OrphanWatchdog::new(Arc::clone(&db), Arc::clone(&policy), Arc::clone(&config))
                .with_outbox(usage_outbox, audit_outbox),
        );
        let turns = Arc::new(TurnsService::new(Arc::clone(&db), enforcer.clone()));
        let quota = Arc::new(QuotaStatusService::new(
            Arc::clone(&db),
            Arc::clone(&policy),
            Arc::clone(&config),
            enforcer.clone(),
        ));

        let attachments = Arc::new(AttachmentService::new(
            Arc::clone(&db),
            Arc::clone(&policy),
            storage,
            Arc::clone(&attachment_cleanup),
            Arc::clone(&config),
            enforcer.clone(),
        ));
        let reaper = Arc::new(
            UploadReaper::new(Arc::clone(&db), Arc::clone(&config))
                .with_cleanup(attachment_cleanup),
        );

        let runtime = Arc::new(MiniChatRuntime {
            config,
            db,
            enforcer,
            policy,
            audit,
            oagw,
            authn,
            models,
            chats,
            messages,
            reactions,
            stream,
            turns,
            mutations,
            watchdog,
            quota,
            attachments,
            reaper,
            outbox: tokio::sync::Mutex::new(Some(outbox)),
        });
        self.runtime
            .set(runtime)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

impl DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        gear_migrations()
    }
}

impl RestApiCapability for MiniChatGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let Some(runtime) = self.runtime.get() else {
            anyhow::bail!("mini-chat: REST registration invoked before init");
        };
        let router = register_models_routes(
            router,
            openapi,
            Arc::clone(&runtime.models),
            &runtime.config.url_prefix,
        );
        let router = register_chat_routes(
            router,
            openapi,
            Arc::clone(&runtime.chats),
            Arc::clone(&runtime.messages),
            Arc::clone(&runtime.reactions),
            &runtime.config.url_prefix,
        );
        let router = register_stream_routes(
            router,
            openapi,
            Arc::clone(&runtime.stream),
            Arc::clone(&runtime.turns),
            &runtime.config.url_prefix,
        );
        let router = register_turn_mutation_routes(
            router,
            openapi,
            Arc::clone(&runtime.mutations),
            &runtime.config.url_prefix,
        );
        let router = register_attachment_routes(
            router,
            openapi,
            Arc::clone(&runtime.attachments),
            &runtime.config.url_prefix,
        );
        Ok(register_quota_routes(
            router,
            openapi,
            Arc::clone(&runtime.quota),
            &runtime.config.url_prefix,
        ))
    }
}
