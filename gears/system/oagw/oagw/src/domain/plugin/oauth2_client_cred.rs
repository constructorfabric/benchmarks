//! `OAuth2` client-credentials auth plugins (ADR 0008).
//!
//! Two variants share one implementation and differ only in how the client
//! authenticates to the token endpoint: `Form` body credentials or an HTTP
//! `Basic` `Authorization` header. Access tokens are cached in
//! `pingora-memory-cache` for `min(config_ttl, expires_in − 30s)`.

use super::{
    AuthDecision, AuthPlugin, Credential, CredentialResolver, PluginRequestContext, config_string,
    config_u64,
};
use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use crate::gts_helpers;
use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use std::sync::Arc;
use std::time::Duration;

/// Configuration keys understood by the plugin.
pub mod keys {
    /// Direct token endpoint URL.
    pub const TOKEN_ENDPOINT: &str = "token_endpoint";
    /// OIDC issuer URL, resolved to a token endpoint by discovery.
    pub const ISSUER_URL: &str = "issuer_url";
    /// `cred://` reference holding the client id.
    pub const CLIENT_ID_REF: &str = "client_id_ref";
    /// `cred://` reference holding the client secret.
    pub const CLIENT_SECRET_REF: &str = "client_secret_ref";
    /// Space-separated `OAuth2` scopes.
    pub const SCOPES: &str = "scopes";
}

/// Safety margin subtracted from the `IdP`'s `expires_in`.
pub const EXPIRY_MARGIN_SECS: u64 = 30;

/// How the client authenticates to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethod {
    /// Credentials in the form body.
    Form,
    /// Credentials in an HTTP `Basic` header.
    Basic,
}

/// A cached access token, carrying its key for collision defence.
#[derive(Clone, PartialEq, Eq)]
struct CachedToken {
    key: String,
    bearer: String,
}

/// Fetches and caches access tokens for one client-auth method.
pub struct OAuth2ClientCredAuthPlugin {
    token_client: Arc<dyn TokenFetcher>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

/// Performs the token-endpoint round trip, so tests can substitute it.
#[async_trait]
pub trait TokenFetcher: Send + Sync {
    /// Exchanges client credentials for an access token.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::AuthenticationFailed`] when the `IdP` rejects the
    /// credentials or the response is unusable.
    async fn fetch(
        &self,
        endpoint: &str,
        method: ClientAuthMethod,
        client_id: &Credential,
        client_secret: &Credential,
        scopes: Option<&str>,
    ) -> Result<FetchedToken, OagwError>;
}

/// A token returned by the `IdP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedToken {
    /// The bearer token.
    pub bearer: String,
    /// IdP-reported lifetime in seconds.
    pub expires_in: u64,
}

impl OAuth2ClientCredAuthPlugin {
    /// Builds a plugin over the given token fetcher.
    #[must_use]
    pub fn new(
        token_client: Arc<dyn TokenFetcher>,
        auth_method: ClientAuthMethod,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            token_client,
            auth_method,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// The cache key naming all identity components (ADR 0008).
    #[must_use]
    pub fn build_cache_key(
        tenant_id: uuid::Uuid,
        config: &AuthConfig,
        auth_method: ClientAuthMethod,
    ) -> String {
        format!(
            "{}:{}:{}:{}",
            tenant_id,
            auth_method_label(auth_method),
            config.plugin_type,
            serde_json::to_string(&config_value(config)).unwrap_or_default()
        )
    }
}

fn auth_method_label(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Form => "form",
        ClientAuthMethod::Basic => "basic",
    }
}

fn config_value(config: &AuthConfig) -> serde_json::Value {
    serde_json::Value::Object(
        config
            .config
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

/// The lifetime a cached token gets: the configured ceiling capped by the
/// IdP-reported lifetime minus the safety margin.
#[must_use]
pub(crate) fn ttl_for(config_ttl: Duration, expires_in: u64) -> Duration {
    let safe = expires_in.saturating_sub(EXPIRY_MARGIN_SECS);
    Duration::from_secs(safe.min(config_ttl.as_secs()))
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Form => gts_helpers::AUTH_OAUTH2_CLIENT_CRED,
            ClientAuthMethod::Basic => gts_helpers::AUTH_OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    async fn authenticate(
        &self,
        context: &mut PluginRequestContext,
        config: &AuthConfig,
        credentials: &dyn CredentialResolver,
    ) -> Result<AuthDecision, OagwError> {
        let value = config_value(config);
        let endpoint = config_string(&value, keys::TOKEN_ENDPOINT)
            .or_else(|| config_string(&value, keys::ISSUER_URL))
            .ok_or_else(|| {
                OagwError::ValidationError(
                    "auth.config requires token_endpoint or issuer_url".to_owned(),
                )
            })?;
        let client_id_ref = config_string(&value, keys::CLIENT_ID_REF).ok_or_else(|| {
            OagwError::ValidationError("auth.config.client_id_ref is required".to_owned())
        })?;
        let client_secret_ref =
            config_string(&value, keys::CLIENT_SECRET_REF).ok_or_else(|| {
                OagwError::ValidationError("auth.config.client_secret_ref is required".to_owned())
            })?;
        let scopes = config_string(&value, keys::SCOPES);

        let key = Self::build_cache_key(context.tenant_id, config, self.auth_method);
        let (hit, _status) = self.cache.get(&key);
        if let Some(hit) = hit.filter(|entry| entry.key == key) {
            inject(context, &hit.bearer)?;
            return Ok(AuthDecision::Injected);
        }

        let client_id = credentials
            .resolve(context.tenant_id, &client_id_ref)
            .await?;
        let client_secret = credentials
            .resolve(context.tenant_id, &client_secret_ref)
            .await?;
        let fetched = self
            .token_client
            .fetch(
                &endpoint,
                self.auth_method,
                &client_id,
                &client_secret,
                scopes.as_deref(),
            )
            .await?;

        let ttl = ttl_for(self.cache_ttl, fetched.expires_in);
        self.cache.put(
            &key,
            CachedToken {
                key: key.clone(),
                bearer: fetched.bearer.clone(),
            },
            Some(ttl),
        );
        inject(context, &fetched.bearer)?;
        Ok(AuthDecision::Injected)
    }
}

fn inject(context: &mut PluginRequestContext, bearer: &str) -> Result<(), OagwError> {
    let value = format!("Bearer {bearer}");
    let parsed = http::HeaderValue::try_from(value).map_err(|_| {
        OagwError::AuthenticationFailed("access token is not a valid header value".to_owned())
    })?;
    context.headers.insert(http::header::AUTHORIZATION, parsed);
    Ok(())
}

/// [`config_u64`] is exposed for callers reading the cache ceiling.
#[must_use]
pub fn cache_ttl_from(config: &serde_json::Value) -> Duration {
    Duration::from_secs(config_u64(config, "token_cache_ttl_secs").unwrap_or(300))
}

/// The default token-cache capacity.
#[must_use]
pub const fn default_cache_capacity() -> usize {
    10_000
}

#[cfg(test)]
#[path = "oauth2_client_cred_tests.rs"]
mod tests;
