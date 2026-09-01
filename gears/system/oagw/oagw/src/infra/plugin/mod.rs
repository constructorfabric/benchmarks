//! The built-in plugin implementations and their registries.
//!
//! Every plugin of `DESIGN` §3.2 lives here: the auth family (`ADR`-0008), the
//! guard family (`ADR`-0009) and the transform family. [`registry`] turns a
//! stored plugin reference into a live instance; the proxy engine is the only
//! caller.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod secrets;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod noop_apikey_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod oauth2_client_cred_auth_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod registry_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod request_id_transform_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod required_headers_guard_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod secrets_tests;
