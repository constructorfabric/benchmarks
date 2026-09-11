//! Built-in plugin implementations and the registries that resolve them.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod secret;

pub use apikey_auth::ApiKeyAuthPlugin;
pub use noop_auth::NoopAuthPlugin;
pub use oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
pub use registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginRegistries, TransformPluginRegistry,
};
pub use request_id_transform::RequestIdTransformPlugin;
pub use required_headers_guard::RequiredHeadersGuardPlugin;
