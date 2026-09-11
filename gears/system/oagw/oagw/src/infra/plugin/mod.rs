//! Built-in plugin implementations and the in-process registries that
//! resolve them.

pub mod apikey_auth;
pub mod credentials;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use apikey_auth::ApiKeyAuthPlugin;
pub use noop_auth::NoopAuthPlugin;
pub use oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
pub use request_id_transform::RequestIdTransformPlugin;
pub use required_headers_guard::RequiredHeadersGuardPlugin;
