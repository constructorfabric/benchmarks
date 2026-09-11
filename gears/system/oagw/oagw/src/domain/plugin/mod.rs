//! Built-in Auth/Guard/Transform plugin implementations
//! (`cpt-cf-oagw-adr-plugin-system`, `cpt-cf-oagw-dod-plugin-kinds`).
//!
//! Each submodule implements exactly one built-in plugin's behaviour; the
//! chain-execution seam (`crate::domain::proxy::plugin_seam`) is the only
//! caller. [`registry::BuiltinPluginRegistry`] is the real
//! `NamedPluginRegistry` implementation feature 4 deliberately left empty.

pub mod apikey_auth;
pub mod binding;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod token_cache;

pub use registry::BuiltinPluginRegistry;
pub use token_cache::TokenCache;
