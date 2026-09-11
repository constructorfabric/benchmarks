//! The plugin implementations the plugin-system feature delivers.
//!
//! Three members: [`credential`] is the `cred://` routine that is the only
//! thing in the gear that turns a reference into material, [`token_cache`] is
//! the OAuth2 entry cache whose stored key is verified on every hit, and
//! [`builtin`] is the six implementations the built-in catalogue backs. The
//! registries that hold them are declared beside the contracts they serve, in
//! [`crate::domain::plugin_contract`].
//!
//! Nothing here is reachable through an endpoint: material and cached tokens
//! enter and leave through the contexts the data plane builds.

pub mod builtin;
pub mod chain;
pub mod credential;
pub mod token_cache;

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_security::SecurityContext;

pub use builtin::{
    ApiKeyAuthPlugin, NoopAuthPlugin, OAuth2ClientCredAuthPlugin, RequestIdTransformPlugin,
    RequiredHeadersGuardPlugin,
};
pub use chain::{ComposedAuth, ComposedChain, ComposedStep};
pub use token_cache::{TokenCache, TokenCacheConfig};

use crate::domain::plugin_contract::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};

/// The three registries the gear serves the plugin contracts through, built
/// once at initialization and shared by every caller that resolves an
/// identifier.
#[derive(Clone)]
pub struct PluginRegistries {
    /// The auth implementations, one per upstream.
    pub auth: AuthPluginRegistry,
    /// The guard implementations, many per upstream and per route.
    pub guard: GuardPluginRegistry,
    /// The transform implementations, many per upstream and per route.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Builds the three registries with the six backed built-in implementations
    /// registered at initialization.
    ///
    /// The auth registry resolves its references through the credential store
    /// it is handed and its two Client Credentials variants with the cache
    /// ceilings it is handed; the guard and transform registries hold their
    /// stateless implementations.
    #[must_use]
    pub fn with_builtins(
        store: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(store, token_cache),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }

    /// Builds the registries a deployment serves, whose credential store may
    /// be absent.
    ///
    /// A gear the hub resolved no `cred_store` client for still mounts the six
    /// built-ins, over [`UnavailableCredStore`]: every credential resolution
    /// that store answers fails closed as `PluginFailure::Unavailable`, so a
    /// chain bound to an auth plugin is refused at execution time rather than
    /// being served with material that was never resolved. The guard and
    /// transform families are stateless and unaffected.
    #[must_use]
    pub fn for_deployment(
        store: Option<Arc<dyn CredStoreClientV1>>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        Self::with_builtins(
            store.unwrap_or_else(|| Arc::new(UnavailableCredStore)),
            token_cache,
        )
    }
}

/// The credential store a deployment without one serves.
///
/// The gear mounts the plugin registries at initialization whatever the hub
/// resolved, because the token cache they hold is shared state a later
/// resolution cannot rebuild; this store stands in for the absent client and
/// turns every resolution into the typed unavailability the routine maps, so
/// no request is ever answered with material that was not resolved.
pub struct UnavailableCredStore;

#[async_trait::async_trait]
impl CredStoreClientV1 for UnavailableCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        Err(credstore_sdk::CredStoreError::ServiceUnavailable {
            detail: String::from("no credential store is mounted in this deployment"),
            retry_after: None,
        })
    }
}
