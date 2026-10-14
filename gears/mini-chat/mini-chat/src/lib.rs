//! `mini-chat` gear: multi-tenant AI chat.
//!
//! The crate follows the DDD-light layout of the workspace: `api` (REST), `domain`
//! (services and rules) and `infra` (storage, outbox, plugins). The module paths
//! `infra::plugins::static_audit` and `infra::plugins::static_model_policy` stay
//! public because the example server imports them to link the plugin registrations.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::MiniChatGear;
#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "traceability_tests.rs"]
mod traceability_tests;

#[cfg(test)]
#[path = "traceability_attachments_tests.rs"]
mod traceability_attachments_tests;

#[cfg(test)]
#[path = "traceability_mutations_tests.rs"]
mod traceability_mutations_tests;

#[cfg(test)]
#[path = "traceability_maintenance_tests.rs"]
mod traceability_maintenance_tests;
