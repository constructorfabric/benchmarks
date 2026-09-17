//! Built-in plugins and their registries (ADR-0002, ADR-0008, ADR-0009).

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_cc_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod token_cache;
