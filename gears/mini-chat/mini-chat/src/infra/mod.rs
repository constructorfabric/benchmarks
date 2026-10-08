//! Infrastructure layer: persistence, provider adapters, OAGW, outbox, plugins.

pub mod plugins;
pub mod audit_gateway;
pub mod db;
pub mod llm;
pub mod oagw_provision;
pub mod outbox;
pub mod policy_gateway;
