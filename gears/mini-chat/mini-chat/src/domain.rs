//! Domain layer: authorization (PEP), canonical error type and services.

pub mod attachments;
pub mod authz;
pub mod chats;
pub mod cleanup;
pub mod clock;
pub mod context;
pub mod error;
pub mod leader;
pub mod messages;
pub mod models;
pub mod ports;
pub mod provider;
pub mod quota;
pub mod quota_status;
pub mod reactions;
pub mod sanitize;
pub mod settlement;
pub mod streaming;
pub mod thread_summary;
pub mod turn_mutations;
pub mod turns;
pub mod upload_reaper;
pub mod watchdog;
