//! `llm_provider`: in-process provider adapters (ADR-0001, ADR-0005).

pub mod anthropic;
pub mod client;
pub mod gateway;
pub mod openai_chat;
pub mod openai_responses;
pub mod provider;
pub mod sanitize;
pub mod storage;
pub mod types;

#[cfg(test)]
#[path = "adapters_tests.rs"]
mod adapters_tests;
