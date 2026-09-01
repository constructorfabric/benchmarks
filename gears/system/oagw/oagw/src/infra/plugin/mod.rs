//! Built-in plugin implementations and the auth-plugin registry.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod required_headers_guard;

pub use registry::{AuthPluginRegistry, GuardPluginRegistry, TokenCacheConfig};
