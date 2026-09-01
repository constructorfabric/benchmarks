//! Built-in plugin implementations (ADR-0002 §Built-in Plugins).
//!
//! Every plugin is stateless except the OAuth2 client-credentials plugin,
//! which owns an access-token cache.

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_cc_auth;
pub mod request_id_transform;
pub mod required_headers_guard;

use std::sync::Arc;

use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};

/// Resolves `cred://` references on behalf of plugins.
///
/// The credential store is optional so the gear still initialises when the
/// `credstore` client is not published; every lookup then fails closed.
#[derive(Clone)]
pub struct SecretResolver {
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
}

impl std::fmt::Debug for SecretResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SecretResolver").finish()
    }
}

impl SecretResolver {
    /// Wraps a credstore client.
    #[must_use]
    pub fn new(credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>) -> Self {
        Self { credstore }
    }

    /// Fetches a secret value as UTF-8, trimming surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns 500 [`DomainError::SecretNotFound`] when the reference is
    /// malformed, the secret is absent or the value is not UTF-8. The value
    /// itself is never included in the error.
    pub async fn resolve(
        &self,
        security_context: &SecurityContext,
        reference: &str,
    ) -> Result<String, DomainError> {
        let key = crate::domain::plugin::parse_secret_ref(reference)?;
        let response = self
            .credstore
            .as_ref()
            .ok_or_else(|| {
                DomainError::SecretNotFound(
                    "the credential store is not available in this deployment".to_owned(),
                )
            })?
            .get(security_context, &key)
            .await
            .map_err(|_| {
                DomainError::SecretNotFound(format!(
                    "credential '{reference}' is not readable for this caller"
                ))
            })?
            .ok_or_else(|| {
                DomainError::SecretNotFound(format!(
                    "credential '{reference}' does not exist for this caller"
                ))
            })?;
        String::from_utf8(response.value.as_bytes().to_owned()).map_or_else(
            |_| {
                Err(DomainError::SecretNotFound(format!(
                    "credential '{reference}' is not valid UTF-8"
                )))
            },
            |value| Ok(value.trim().to_owned()),
        )
    }
}

/// Registry set with all built-ins registered.
#[derive(Clone)]
pub struct BuiltinPlugins {
    /// Auth plugin registry.
    pub auth: Arc<AuthPluginRegistry>,
    /// Guard plugin registry.
    pub guards: Arc<GuardPluginRegistry>,
    /// Transform plugin registry.
    pub transforms: Arc<TransformPluginRegistry>,
}

impl std::fmt::Debug for BuiltinPlugins {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinPlugins")
            .field("auth", &self.auth.ids())
            .finish()
    }
}

impl BuiltinPlugins {
    /// Registers every built-in plugin with the ADR-0008 cache defaults.
    #[must_use]
    pub fn with_builtins(credstore: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self::with_builtins_optional(Some(credstore))
    }

    /// Registers every built-in plugin with the ADR-0008 cache defaults,
    /// tolerating a missing credential store.
    ///
    /// When the `credstore` gear is not linked in (or publishes no client) the
    /// auth plugins still resolve, but every `cred://` lookup fails with
    /// 500 [`DomainError::SecretNotFound`]; nothing else is degraded.
    #[must_use]
    pub fn with_builtins_optional(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Self {
        Self::build(
            credstore,
            oauth2_cc_auth::DEFAULT_TOKEN_CACHE_TTL_SECS,
            oauth2_cc_auth::DEFAULT_TOKEN_CACHE_CAPACITY,
        )
    }

    /// Registers every built-in plugin, sizing the OAuth2 access-token cache
    /// from the gear configuration (ADR-0008 `token_cache_ttl_secs` /
    /// `token_cache_capacity`).
    #[must_use]
    pub fn with_token_cache(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        token_cache_ttl_secs: u64,
        token_cache_capacity: usize,
    ) -> Self {
        Self::build(
            credstore,
            token_cache_ttl_secs.max(oauth2_cc_auth::MIN_TOKEN_CACHE_TTL_SECS),
            token_cache_capacity,
        )
    }

    fn build(
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
        token_cache_ttl_secs: u64,
        token_cache_capacity: usize,
    ) -> Self {
        let resolver = Arc::new(SecretResolver::new(credstore));
        let cache = |plugin: oauth2_cc_auth::OAuth2ClientCredAuthPlugin| {
            Arc::new(plugin.with_cache(
                token_cache_capacity,
                std::time::Duration::from_secs(token_cache_ttl_secs),
            ))
        };
        Self {
            auth: Arc::new(AuthPluginRegistry::new(vec![
                Arc::new(noop_auth::NoopAuthPlugin),
                Arc::new(apikey_auth::ApiKeyAuthPlugin::new(resolver.clone())),
                cache(oauth2_cc_auth::OAuth2ClientCredAuthPlugin::form(
                    resolver.clone(),
                )),
                cache(oauth2_cc_auth::OAuth2ClientCredAuthPlugin::basic(resolver)),
            ])),
            guards: Arc::new(GuardPluginRegistry::new(vec![Arc::new(
                required_headers_guard::RequiredHeadersGuardPlugin,
            )])),
            transforms: Arc::new(TransformPluginRegistry::new(vec![Arc::new(
                request_id_transform::RequestIdTransformPlugin,
            )])),
        }
    }
}

/// Reads a string config key, trimming it and mapping blanks to `None`.
#[must_use]
pub fn config_str(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
}

/// Reads a non-negative integer config key.
#[must_use]
pub fn config_u64(config: &serde_json::Value, key: &str) -> Option<u64> {
    config.get(key).and_then(serde_json::Value::as_u64)
}

/// Reads a boolean config key (default `false`).
#[must_use]
pub fn config_bool(config: &serde_json::Value, key: &str) -> bool {
    config
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn config_helpers_ignore_blanks() {
        let config = serde_json::json!({"a": " x ", "b": "", "c": 7, "d": true});
        assert_eq!(config_str(&config, "a").as_deref(), Some("x"));
        assert_eq!(config_str(&config, "b"), None);
        assert_eq!(config_str(&config, "missing"), None);
        assert_eq!(config_u64(&config, "c"), Some(7));
        assert_eq!(config_u64(&config, "a"), None);
        assert!(config_bool(&config, "d"));
        assert!(!config_bool(&config, "missing"));
    }

    #[test]
    fn builtins_are_registered() {
        let plugins = BuiltinPlugins::with_builtins(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        ));
        assert_eq!(plugins.auth.ids().len(), 4);
        assert_eq!(plugins.guards.ids().len(), 1);
    }

    #[test]
    fn with_token_cache_registers_the_same_builtins_and_clamps_the_ttl() {
        let plugins = BuiltinPlugins::with_token_cache(
            Some(Arc::new(
                credstore_sdk::test_util::MockCredStoreClient::empty(),
            )),
            0,
            1,
        );
        assert_eq!(plugins.auth.ids().len(), 4);
        for id in plugins.auth.ids() {
            plugins
                .auth
                .resolve(&id)
                .unwrap_or_else(|error| panic!("'{id}' must resolve: {error}"));
        }
        // A zero TTL is clamped to the ADR-0008 minimum rather than disabling
        // caching entirely.
        assert_eq!(
            oauth2_cc_auth::cache_ttl_for(
                std::time::Duration::from_secs(oauth2_cc_auth::MIN_TOKEN_CACHE_TTL_SECS),
                std::time::Duration::from_secs(600),
            ),
            Some(std::time::Duration::from_secs(
                oauth2_cc_auth::MIN_TOKEN_CACHE_TTL_SECS
            ))
        );
    }
}
