//! Infrastructure layer of the mini-chat gear.

pub mod audit_gateway;
pub mod background_tasks;
pub mod db;
pub mod llm;
pub mod metrics;
pub mod outbox;
pub mod plugin_registry;
pub mod plugin_resolution;
pub mod plugins;
pub mod policy_gateway;
pub mod runtime;
pub mod startup_checks;
pub mod storage;
pub mod workers;

#[cfg(test)]
pub(crate) mod test_support;
