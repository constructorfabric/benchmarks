//! Infrastructure layer: storage, outbox, plugin resolution and the bundled plugins.

pub mod attachment_cleanup;
pub mod audit_gateway;
pub mod chat_cleanup_handler;
pub mod clock;
pub mod error_conv;
pub mod leader;
pub mod metrics;
pub mod oagw;
pub mod orphan_watchdog;
pub mod outbox;
pub mod plugins;
pub mod policy_gateway;
pub mod provider_task;
pub mod storage;
pub mod tasks;
pub mod thread_summary_handler;
pub mod turn_finalizer;
pub mod upload_reaper;
pub mod usage_handler;
