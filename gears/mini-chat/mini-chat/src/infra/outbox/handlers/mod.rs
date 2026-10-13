//! Leased outbox handlers of the five mini-chat queues.

pub mod attachment_cleanup;
pub mod audit;
pub mod chat_cleanup;
pub mod thread_summary;
pub mod usage;

use std::sync::Arc;
use std::time::Duration;

use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxError, OutboxHandle, Partitions, WorkerTuning};

use crate::domain::services::AppServices;

/// Builds and starts the outbox pipeline with the five queues and binds the enqueuer.
///
/// # Errors
/// Returns the outbox error when the pipeline cannot start.
pub async fn start_pipeline(app: &Arc<AppServices>) -> Result<OutboxHandle, OutboxError> {
    let cfg = &app.cfg.outbox;
    let parts = u16::try_from(cfg.num_partitions).unwrap_or(4);
    let p = || Partitions::of(parts);
    let summary_lease = Duration::from_secs(app.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(app.db.db())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&cfg.queue_name, p())
        .leased(usage::UsageHandler::new(Arc::clone(app)))
        .queue(&cfg.cleanup_queue_name, p())
        .leased(attachment_cleanup::AttachmentCleanupHandler::new(Arc::clone(app)))
        .queue(&cfg.chat_cleanup_queue_name, p())
        .leased(chat_cleanup::ChatCleanupHandler::new(Arc::clone(app)))
        .queue(&cfg.thread_summary_queue_name, p())
        .leased(thread_summary::ThreadSummaryHandler::new(Arc::clone(app)))
        .lease(LeaseConfig { duration: summary_lease, headroom: Duration::from_secs(2) })
        .queue(&cfg.audit_queue_name, p())
        .leased(audit::AuditHandler::new(Arc::clone(app)))
        .lease(LeaseConfig { duration: Duration::from_secs(60), headroom: Duration::from_secs(2) })
        .start()
        .await?;
    app.outbox.bind(Arc::clone(handle.outbox()));
    Ok(handle)
}
