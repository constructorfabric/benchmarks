//! Plugin implementations: registries and the built-in plugins.

pub mod absent_credstore;
pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;

pub use registry::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};

use crate::domain::plugin::gts_helpers;

/// Context builders shared by the plugin unit tests.
#[cfg(test)]
pub(crate) mod test_support;

/// Whether a plugin reference is catalogued but has no implementation.
///
/// These identifiers exist in the types-registry catalog only; binding them is
/// rejected.
#[must_use]
pub fn is_catalog_only(reference: &str) -> bool {
    gts_helpers::CATALOG_ONLY_AUTH.contains(&reference)
        || gts_helpers::CATALOG_ONLY_GUARD.contains(&reference)
        || gts_helpers::CATALOG_ONLY_TRANSFORM.contains(&reference)
}
