//! The `llm_provider` library (ADR-0001, ADR-0005): provider adapters, the OAGW-backed provider
//! and the provider resolver.

mod adapter;
mod alias;
mod anthropic;
mod chat_completions;
mod common;
mod openai_responses;
mod provider;
mod resolver;
mod vllm_responses;

pub use alias::{
    is_ip_host, normalized, normalized_host, upstream_alias, upstream_alias_for_tenant,
};
pub use resolver::ConfiguredProviders;

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod adapter_tests;
