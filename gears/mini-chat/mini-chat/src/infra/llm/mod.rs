//! `llm_provider` library (ADR-0001, ADR-0005): provider-neutral types,
//! adapters, sanitization, provider resolution and the OAGW gateway.

pub mod gateway;
pub mod provider_resolver;
pub mod providers;
pub mod sanitize;
pub mod sse_parser;
pub mod storage;
pub mod types;
