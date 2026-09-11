//! Built-in plugin implementations and the registries that resolve them.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod secret;

pub use oauth2_client_cred_auth::TokenCacheConfig;
pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
