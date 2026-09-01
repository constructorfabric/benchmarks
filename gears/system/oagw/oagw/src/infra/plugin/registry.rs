//! Plugin resolution: a GTS id plus a binding config becomes a live plugin.
//!
//! The control plane stores references; the data plane turns each reference
//! into an [`Arc<dyn AuthPlugin>`], [`Arc<dyn GuardPlugin>`] or
//! [`Arc<dyn TransformPlugin>`] at request-build time. An id this gear cannot
//! resolve — a custom row id, or a catalog-only built-in — is
//! [`DomainError::PluginNotFound`], which maps to `503` (`DESIGN` §3.3): the
//! gateway is missing a capability, not the caller making a mistake.

use std::sync::Arc;

use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    API_KEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::model::AuthConfig;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin, uuid_tail};
use crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::{
    CachedToken, OAuth2ClientCredAuthPlugin, TokenCacheConfig,
};
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;
use crate::infra::plugin::secrets::SecretResolver;

/// Shared token cache of the `oauth2_client_cred` plugin family (`ADR`-0008).
pub type SharedTokenCache = Arc<pingora_memory_cache::MemoryCache<String, CachedToken>>;

/// A token cache sized by [`TokenCacheConfig`].
///
/// # Panics
/// Never: `MemoryCache::new` is total.
#[must_use]
pub fn token_cache(config: TokenCacheConfig) -> SharedTokenCache {
    Arc::new(pingora_memory_cache::MemoryCache::new(config.capacity))
}

/// Resolves `auth.type` plus its configuration into an [`AuthPlugin`].
#[derive(Clone)]
pub struct AuthPluginRegistry {
    secrets: Arc<dyn SecretResolver>,
    token_http_config: Option<toolkit_http::HttpClientConfig>,
    token_cache: SharedTokenCache,
    token_cache_config: TokenCacheConfig,
}

impl AuthPluginRegistry {
    /// A registry over `secrets`, whose `OAuth2` bindings share `token_cache`.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        token_http_config: Option<toolkit_http::HttpClientConfig>,
        token_cache: SharedTokenCache,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            secrets,
            token_http_config,
            token_cache,
            token_cache_config,
        }
    }

    /// The registry of a deployment that uses the platform credential store
    /// (`ADR`-0008): one token cache shared by every binding.
    #[must_use]
    pub fn with_builtins(
        secrets: Arc<dyn SecretResolver>,
        token_http_config: Option<toolkit_http::HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        let cache = token_cache(token_cache_config);
        Self::new(secrets, token_http_config, cache, token_cache_config)
    }

    /// The token cache every `OAuth2` binding of this registry shares.
    #[must_use]
    pub fn token_cache(&self) -> SharedTokenCache {
        Arc::clone(&self.token_cache)
    }

    /// The plugin `auth` names, bound to `identity` (tenant, subject).
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when `auth` names no plugin at all
    /// and [`DomainError::PluginNotFound`] when it names one this gear cannot
    /// resolve.
    pub fn resolve(
        &self,
        auth: Option<&AuthConfig>,
        identity: (uuid::Uuid, uuid::Uuid),
    ) -> Result<Option<Arc<dyn AuthPlugin>>, DomainError> {
        let Some(auth) = auth else {
            return Ok(None);
        };
        let id = auth
            .plugin_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                DomainError::validation("upstream auth requires a 'type' plugin identity")
            })?;
        Ok(Some(self.resolve_id(id, auth.config.as_ref(), identity)?))
    }

    /// The plugin `id` names, bound to `config` and `identity`.
    ///
    /// # Errors
    /// As [`Self::resolve`].
    pub fn resolve_id(
        &self,
        id: &str,
        config: Option<&serde_json::Value>,
        identity: (uuid::Uuid, uuid::Uuid),
    ) -> Result<Arc<dyn AuthPlugin>, DomainError> {
        if uuid_tail(id).is_some() {
            // Custom auth rows are catalog-only today (`DESIGN` §3.2).
            return Err(DomainError::plugin_not_found(id));
        }
        let plugin: Arc<dyn AuthPlugin> = if id == NOOP_AUTH_PLUGIN_ID {
            Arc::new(NoopAuthPlugin::new())
        } else if id == API_KEY_AUTH_PLUGIN_ID {
            Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&self.secrets), config)?)
        } else if id == OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID {
            Arc::new(self.oauth2(ClientAuthMethod::Form, identity, config)?)
        } else if id == OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID {
            Arc::new(self.oauth2(ClientAuthMethod::Basic, identity, config)?)
        } else {
            return Err(DomainError::plugin_not_found(id));
        };
        Ok(plugin)
    }

    fn oauth2(
        &self,
        auth_method: ClientAuthMethod,
        identity: (uuid::Uuid, uuid::Uuid),
        config: Option<&serde_json::Value>,
    ) -> Result<OAuth2ClientCredAuthPlugin, DomainError> {
        OAuth2ClientCredAuthPlugin::new(
            Arc::clone(&self.secrets),
            auth_method,
            self.token_http_config.clone(),
            Arc::clone(&self.token_cache),
            self.token_cache_config,
            identity,
            config,
        )
    }
}

/// Resolves the guard plugins of an upstream or route chain.
#[derive(Debug, Default, Clone, Copy)]
pub struct GuardPluginRegistry;

impl GuardPluginRegistry {
    /// The registry of a deployment: `required_headers` is the only resolvable
    /// guard (`DESIGN` §3.2).
    #[must_use]
    pub const fn with_builtins() -> Self {
        Self
    }

    /// The plugin `id` names, bound to `config`.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginNotFound`] when `id` is not a resolvable
    /// built-in guard.
    pub fn resolve(
        id: &str,
        config: Option<&serde_json::Value>,
    ) -> Result<Arc<dyn GuardPlugin>, DomainError> {
        if uuid_tail(id).is_some() || id != REQUIRED_HEADERS_GUARD_PLUGIN_ID {
            return Err(DomainError::plugin_not_found(id));
        }
        Ok(Arc::new(RequiredHeadersGuardPlugin::new(config)))
    }
}

/// Resolves the transform plugins of an upstream or route chain.
#[derive(Debug, Default, Clone, Copy)]
pub struct TransformPluginRegistry;

impl TransformPluginRegistry {
    /// The registry of a deployment: `request_id` is the only resolvable
    /// transform (`DESIGN` §3.2).
    #[must_use]
    pub const fn with_builtins() -> Self {
        Self
    }

    /// The plugin `id` names, bound to `config`.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginNotFound`] when `id` is not a resolvable
    /// built-in transform.
    pub fn resolve(
        id: &str,
        config: Option<&serde_json::Value>,
    ) -> Result<Arc<dyn TransformPlugin>, DomainError> {
        if uuid_tail(id).is_some() || id != REQUEST_ID_TRANSFORM_PLUGIN_ID {
            return Err(DomainError::plugin_not_found(id));
        }
        Ok(Arc::new(RequestIdTransformPlugin::new(config)?))
    }
}
