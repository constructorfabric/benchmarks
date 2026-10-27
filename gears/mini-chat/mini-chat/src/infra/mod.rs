//! Infrastructure layer: persistence, LLM provider access, OAGW
//! provisioning, outbox, plugins, background workers, metrics.

pub mod election;
pub mod llm;
pub mod metrics;
pub mod oagw_provision;
pub mod gateways;
pub mod outbox;
pub mod plugins;
pub mod sanitize;
pub mod storage;
pub mod watchdog;
