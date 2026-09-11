//! Infrastructure layer: repository implementations, the proxy engine,
//! plugin registries, metrics and type provisioning.

pub mod metrics;
pub mod plugin;
pub mod proxy;
pub mod rate_limit;
pub mod storage;
pub mod tenant_dir;
pub mod type_catalog;
