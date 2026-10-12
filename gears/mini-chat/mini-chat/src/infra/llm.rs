//! `llm_provider` (ADR-0001, ADR-0005): the provider resolver, the
//! `openai_responses` adapter and the OAGW provisioning, all inside the gear.

pub mod openai_responses;
mod providers;
pub mod provisioning;
pub mod sanitize;
pub mod storage;

pub use providers::LlmProviders;
pub use storage::ProviderStorage;
