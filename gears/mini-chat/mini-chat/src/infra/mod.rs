//! Infrastructure layer: persistence, LLM providers, outbox, workers and the
//! bundled plugins.

pub(crate) mod db;
pub(crate) mod llm;
pub(crate) mod metrics;
pub(crate) mod outbox;
pub mod plugins;
pub(crate) mod plugins_gateway;
pub(crate) mod thumbnail;
