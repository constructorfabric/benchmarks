//! Infrastructure layer of the OAGW gear — concrete, dependency-carrying
//! components: in-memory persistence, the proxy data plane, built-in plugin
//! registries and their implementing plugins.

pub mod cors;
pub mod data_plane;
pub mod memory_repo;
pub mod oauth2_auth;
pub mod plugins;
pub mod secrets;
pub mod ssrf;
