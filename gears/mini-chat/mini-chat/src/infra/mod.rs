//! Infrastructure: persistence, LLM provider library, outbox, workers, plugins.

pub mod db;
pub mod llm;
pub mod metrics;
pub mod outbox;
pub mod plugin_gateway;
pub mod plugins;
pub mod workers;
