//! Infrastructure layer of the OAGW gear: the in-memory registry store and
//! its read-optimised L1 caches, the plugin system the data plane runs and the
//! outbound proxy engine behind the proxy routes.

pub mod plugin;
pub mod proxy;
pub mod storage;

pub use proxy::{ProxyEngine, ProxyRequest, ProxyResponse};
pub use storage::{CacheLimits, RegistryStore};
