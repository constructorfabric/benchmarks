//! Outbox integration: queue registration, transactional enqueue and payloads
//! (spec §13.1).

pub mod attachment_cleanup;
pub mod audit;
pub mod chat_cleanup;
pub mod enqueuer;
pub mod payloads;
pub mod pipeline;
pub mod thread_summary;
pub mod usage;

pub use attachment_cleanup::AttachmentCleanupHandler;
pub use audit::AuditHandler;
pub use chat_cleanup::ChatCleanupHandler;
pub use enqueuer::{OutboxEnqueuer, PendingWakes, QueueKind, partition_for};
pub use pipeline::{AckAllHandler, OutboxHandlers, start_outbox};
pub use thread_summary::ThreadSummaryHandler;
pub use usage::UsageHandler;

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod outbox_tests;

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod handlers_tests;
