//! Built-in plugin implementations.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use registry::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
