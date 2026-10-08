//! Infrastructure layer: persistence, provider adapters, outbox, workers,
//! plugin gateways and bundled plugins.

pub mod audit;
pub mod db;
pub mod llm;
pub mod metrics;
pub mod outbox;
pub mod plugins;
pub mod policy;
pub mod thumbnail;
pub mod workers;
