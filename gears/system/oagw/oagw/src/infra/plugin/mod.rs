//! Built-in plugins and the registry that resolves plugin bindings.

pub mod api_key_auth;
pub mod noop_auth;
pub mod oauth2_client_credentials;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod security_context;

pub use oauth2_client_credentials::{ClientCredentialsAuth, Variant};
pub use registry::{PluginRegistry, PluginResolution};

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod oauth2_client_cred_tests;
#[cfg(test)]
mod registry_tests;
