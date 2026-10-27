//! Outbox pipeline startup: five leased queues.

use std::sync::Arc;

use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxHandle, Partitions};
use toolkit_security::SecurityContext;

use super::{DbEnqueuer, SharedEnqueuer, handlers};
use crate::domain::services::ServiceContext;
use crate::infra::llm::providers::ProviderAdapter;
use crate::infra::llm::resolver::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;

/// The pipeline inputs.
pub struct PipelineDeps {
    /// The service context.
    pub ctx: Arc<ServiceContext>,
    /// Storage client.
    pub storage: Arc<StorageClient>,
    /// Provider registry.
    pub registry: Arc<ProviderRegistry>,
    /// Provider adapter.
    pub adapter: Arc<ProviderAdapter>,
    /// S2S context for background provider calls.
    pub s2s: SecurityContext,
    /// The shared enqueuer to install into.
    pub enqueuer: Arc<SharedEnqueuer>,
    /// Optional processor budget override (tests).
    pub processors: Option<usize>,
}

/// Start the outbox pipeline with the five mini-chat queues and install the
/// enqueuer used by the domain services.
pub async fn start(deps: &PipelineDeps) -> Result<OutboxHandle, toolkit_db::outbox::OutboxError> {
    let cfg = &deps.ctx.config.outbox;
    let partitions = Partitions::of(u16::try_from(cfg.num_partitions).unwrap_or(4));
    let s2s = deps.s2s.clone();

    let builder = Outbox::builder(deps.ctx.db.db());
    let builder = if let Some(n) = deps.processors {
        builder.processors(n).maintenance(1, 1)
    } else {
        builder
    };
    let handle = builder
        .queue(&cfg.queue_name, partitions)
        .leased(handlers::UsageHandler::new(Arc::clone(&deps.ctx)))
        .queue(&cfg.cleanup_queue_name, partitions)
        .leased(handlers::AttachmentCleanupHandler::new(
            Arc::clone(&deps.ctx),
            Arc::clone(&deps.storage),
            Arc::clone(&deps.registry),
            s2s.clone(),
        ))
        .queue(&cfg.chat_cleanup_queue_name, partitions)
        .leased(handlers::ChatCleanupHandler::new(
            Arc::clone(&deps.ctx),
            Arc::clone(&deps.storage),
            Arc::clone(&deps.registry),
            s2s.clone(),
        ))
        .queue(&cfg.thread_summary_queue_name, partitions)
        .leased(handlers::ThreadSummaryHandler::new(
            Arc::clone(&deps.ctx),
            Arc::clone(&deps.adapter),
            Arc::clone(&deps.registry),
            s2s.clone(),
        ))
        .lease(LeaseConfig {
            duration: std::time::Duration::from_secs(
                deps.ctx.config.thread_summary_worker.claim_timeout_secs,
            ),
            headroom: std::time::Duration::from_secs(2),
        })
        .queue(&cfg.audit_queue_name, partitions)
        .leased(handlers::AuditHandler::new(Arc::clone(&deps.ctx)))
        .lease(LeaseConfig {
            duration: std::time::Duration::from_secs(60),
            headroom: std::time::Duration::from_secs(2),
        })
        .start()
        .await?;

    deps.enqueuer
        .install(DbEnqueuer::new(Arc::clone(handle.outbox()), cfg));
    Ok(handle)
}
