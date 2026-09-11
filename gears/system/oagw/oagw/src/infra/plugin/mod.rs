// Updated: 2026-09-01 by Constructor Tech
//! Builtin plugin implementations.
//!
//! [`registry::PluginRegistry`] is the lookup table; the files beside it are
//! the implementations. Each one is a small, stateless (or cache-only) unit
//! that implements exactly one of the three ADR-0002 traits.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

#[cfg(test)]
pub mod test_support;
