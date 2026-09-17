//! Infrastructure layer: in-memory stores, the proxy engine, the built-in
//! plugins and type provisioning.

/// The proxy data plane.
pub mod proxy;
/// Built-in plugins.
pub mod plugin;
/// In-memory control-plane store.
pub mod storage;
/// GTS type registration.
pub mod type_provisioning;

pub use plugin::{CredentialSource, builtin_registry, builtin_registry_with_credstore};
pub use proxy::{ProxyEngine, ProxyRequest};
pub use storage::InMemoryStore;
