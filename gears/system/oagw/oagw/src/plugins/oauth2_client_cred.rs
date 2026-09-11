//! `OAuth2ClientCredAuthPlugin` — client credentials flow with an internal
//! token cache.
//!
//! See ADR-0008: the plugin owns a `pingora-memory-cache` keyed by
//! `(tenant, subject, auth_method, config_hash)`; a miss resolves both
//! `cred://` references and performs a single token exchange. TTL is
//! `min(config_ttl, expires_in − 30s)`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use pingora_memory_cache::{CacheStatus, MemoryCache};
use serde_json::Value;
use toolkit_auth::oauth2::SecretString;
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, fetch_token};

use super::apikey_auth::resolve_secret;
use super::{AuthPlugin, PluginError, RequestContext, hash_config};

/// Cached access token plus the key it was stored under.
///
/// `TinyUfo` hashes keys to `u64` and never compares them, so a hit is verified
/// against the original key before the token is used: a hash collision degrades
/// to a miss instead of leaking another tenant's token.
#[derive(Clone)]
pub struct CachedToken {
    /// Original cache key.
    pub key: String,
    /// Bearer token.
    pub token: SecretString,
}

/// Which client authentication method the plugin transmits credentials with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMethodTag {
    /// Credentials in the request body.
    Form,
    /// Credentials in the `Authorization` header.
    Basic,
}

impl ClientAuthMethodTag {
    fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Basic => "basic",
        }
    }
}

/// Parsed plugin configuration.
#[derive(Debug, Clone)]
pub struct OAuth2PluginConfig {
    /// Direct token endpoint URL.
    pub token_endpoint: Option<String>,
    /// OIDC issuer URL for discovery.
    pub issuer_url: Option<String>,
    /// `cred://` reference for the client id.
    pub client_id_ref: String,
    /// `cred://` reference for the client secret.
    pub client_secret_ref: String,
    /// Space-separated scopes.
    pub scopes: Option<String>,
}

impl OAuth2PluginConfig {
    /// Parses the plugin configuration from the binding.
    ///
    /// # Errors
    ///
    /// Returns an internal error for missing or mutually exclusive keys.
    pub fn from_config(config: &serde_json::Map<String, Value>) -> Result<Self, PluginError> {
        let get = |k: &str| {
            config
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let token_endpoint = get("token_endpoint");
        let issuer_url = get("issuer_url");
        if token_endpoint.is_some() && issuer_url.is_some() {
            return Err(PluginError::Internal(
                "oauth2 plugin config has both token_endpoint and issuer_url".into(),
            ));
        }
        let client_id_ref = get("client_id_ref")
            .ok_or_else(|| PluginError::Internal("oauth2 plugin requires client_id_ref".into()))?;
        let client_secret_ref = get("client_secret_ref").ok_or_else(|| {
            PluginError::Internal("oauth2 plugin requires client_secret_ref".into())
        })?;
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes: get("scopes"),
        })
    }

    /// Deterministic hash of the resolved configuration.
    #[must_use]
    pub fn config_hash(&self) -> u64 {
        let mut map = serde_json::Map::new();
        map.insert(
            "token_endpoint".into(),
            Value::from(self.token_endpoint.clone().unwrap_or_default()),
        );
        map.insert(
            "issuer_url".into(),
            Value::from(self.issuer_url.clone().unwrap_or_default()),
        );
        map.insert(
            "scopes".into(),
            Value::from(self.scopes.clone().unwrap_or_default()),
        );
        hash_config(&map)
    }
}

/// OAuth2 client credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethodTag,
    http_config: Option<toolkit_http::HttpClientConfig>,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    /// Creates the plugin.
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethodTag,
        cache_ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            http_config: None,
            cache: MemoryCache::new(cache_capacity),
            cache_ttl,
        }
    }

    /// Overrides the HTTP client configuration used for token fetches.
    #[must_use]
    pub fn with_http_config(mut self, config: toolkit_http::HttpClientConfig) -> Self {
        self.http_config = Some(config);
        self
    }

    /// Builds the cache key for a request.
    #[must_use]
    pub fn build_cache_key(&self, ctx: &RequestContext, config: &OAuth2PluginConfig) -> String {
        format!(
            "{}:{}:{}:{}",
            ctx.security_context.subject_tenant_id(),
            ctx.security_context.subject_id(),
            self.auth_method.as_str(),
            config.config_hash()
        )
    }

    /// Cache TTL for a token with the given `expires_in`.
    #[must_use]
    pub fn ttl_for(&self, expires_in: Duration) -> Option<Duration> {
        let effective = expires_in.checked_sub(safety_margin())?;
        Some(self.cache_ttl.min(effective))
    }
}

fn safety_margin() -> Duration {
    Duration::from_secs(30)
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethodTag::Form => "oauth2_client_cred",
            ClientAuthMethodTag::Basic => "oauth2_client_cred_basic",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethodTag::Form => crate::gts::auth_plugin::OAUTH2_CLIENT_CRED,
            ClientAuthMethodTag::Basic => crate::gts::auth_plugin::OAUTH2_CLIENT_CRED_BASIC,
        }
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError> {
        let config = OAuth2PluginConfig::from_config(&ctx.config)?;
        let key = self.build_cache_key(ctx, &config);

        let (cached, status) = self.cache.get(&key);
        if status == CacheStatus::Hit
            && let Some(entry) = cached
            && entry.key == key
        {
            inject(&mut ctx.headers, entry.token.expose());
            return Ok(());
        }

        let client_id = resolve_secret(
            &self.credstore,
            &ctx.security_context,
            &config.client_id_ref,
        )
        .await?;
        let client_secret = resolve_secret(
            &self.credstore,
            &ctx.security_context,
            &config.client_secret_ref,
        )
        .await?;

        let scopes = config
            .scopes
            .clone()
            .map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();

        let fetch = fetch_token(OAuthClientConfig {
            token_endpoint: config
                .token_endpoint
                .clone()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| PluginError::Internal(format!("invalid token_endpoint: {e}")))?,
            issuer_url: config
                .issuer_url
                .clone()
                .map(|s| {
                    url::Url::parse(&s)
                        .map_err(|e| PluginError::Internal(format!("invalid issuer_url: {e}")))
                })
                .transpose()?,
            client_id,
            client_secret: SecretString::new(client_secret),
            scopes,
            auth_method: if self.auth_method == ClientAuthMethodTag::Basic {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            extra_headers: Vec::new(),
            refresh_offset: Duration::from_secs(0),
            jitter_max: Duration::from_secs(0),
            min_refresh_period: Duration::from_secs(0),
            default_ttl: self.cache_ttl,
            http_config: self.http_config.clone(),
        })
        .await
        .map_err(|e| PluginError::Internal(format!("oauth2 token fetch failed: {e}")))?;

        let bearer = fetch.bearer.clone();
        if let Some(ttl) = self.ttl_for(fetch.expires_in) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: bearer.clone(),
                },
                Some(ttl),
            );
        }

        inject(&mut ctx.headers, bearer.expose());
        Ok(())
    }
}

fn inject(headers: &mut http::HeaderMap, token: &str) {
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(http::header::AUTHORIZATION, value);
    }
}
