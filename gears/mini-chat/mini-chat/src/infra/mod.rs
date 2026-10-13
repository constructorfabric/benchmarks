//! Infrastructure layer: persistence, outbox, plugin gateways, provider adapters, OAGW
//! provisioning, metrics and bundled plugins.

pub mod audit_gateway;
pub mod leader;
pub mod llm;
pub mod metrics;
pub mod model_policy_gateway;
pub mod oagw_provisioning;
pub mod outbox;
pub mod plugin_select;
pub mod plugins;
pub mod provider_storage;
pub mod storage;
pub mod trace;
