//! The plugin engine's built-in plugins and registries.
//!
//! `docs/ADR/0002-plugin-system.md` places the built-ins in `infra/plugin/`;
//! the traits they implement live in `crate::domain::plugin`. Registering an
//! external plugin means calling
//! [`PluginEngine::auth`]/[`guard`]/[`transform`] and adding to the registry
//! before the data plane starts serving.
//!
//! [`guard`]: PluginEngine::guard
use std::sync::Arc;

pub mod auth;
pub mod credstore;
pub mod guard;
pub mod registry;
pub mod transform;

pub use auth::{
    APIKEY_DEFAULT_HEADER, APIKEY_PLUGIN_TYPE, NoopAuthPlugin, OAUTH2_BASIC_PLUGIN_TYPE,
    OAUTH2_FORM_PLUGIN_TYPE, OAuth2ClientCredAuthPlugin, TokenCacheConfig,
};
pub use credstore::CredStoreSecretResolver;

/// A [`SecretResolver`] that refuses every reference.
///
/// The gear declares `credstore` as a dependency, so this is only reachable
/// when the platform started without it: the gear still boots, and a binding
/// that needs a credential fails with a precise problem document instead of
/// taking the whole gear down at `init`.
pub struct UnresolvedSecretResolver;

#[async_trait::async_trait]
impl crate::domain::plugin::SecretResolver for UnresolvedSecretResolver {
    async fn resolve(
        &self,
        reference: &str,
    ) -> Result<Option<crate::domain::plugin::ResolvedSecret>, crate::domain::plugin::SecretError>
    {
        Err(crate::domain::plugin::SecretError::Unavailable(format!(
            "the credstore client is not registered on this gateway, so '{reference}' cannot be \
             resolved"
        )))
    }
}

/// The fallback secret resolver the gear installs when `credstore` is absent.
#[must_use]
pub fn resolver_that_fails() -> Arc<dyn crate::domain::plugin::SecretResolver> {
    Arc::new(UnresolvedSecretResolver)
}
pub use guard::{REQUIRED_HEADERS_PLUGIN_TYPE, RequiredHeadersGuardPlugin};
pub use registry::{
    AuthBinding, AuthPluginRegistry, GuardPluginRegistry, PluginBinding, PluginEngine,
    TransformPluginRegistry,
};
pub use transform::{
    REQUEST_ID_ATTRIBUTE, REQUEST_ID_HEADER, REQUEST_ID_PLUGIN_TYPE, RequestIdTransformPlugin,
};
