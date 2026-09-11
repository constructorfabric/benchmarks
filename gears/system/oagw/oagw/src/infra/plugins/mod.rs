//! Built-in plugin implementations (`ADR/0002`, `ADR/0008`, `ADR/0009`).
//!
//! Six of the twelve catalog identifiers have a backing implementation; the
//! other six are catalog-only and resolve to [`DomainError::PluginNotFound`]
//! at proxy time.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use registry::PluginRegistry;
