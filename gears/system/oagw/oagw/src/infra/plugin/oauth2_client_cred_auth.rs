//! `cf.core.oagw.oauth2_client_cred[_basic].v1` — OAuth2 Client Credentials
//! with an internal token cache (ADR 0008).
//!
//! Registered twice, once per client authentication method. Both variants
//! share this implementation; only `auth_method` differs.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use http::{HeaderValue, header};
use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::{
    ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token,
};

use crate::domain::gts;
use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError, config_nonblank};

use super::credref::resolve_secret;

/// Never cache a token that is about to expire.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Gear-level cache settings threaded in from [`crate::OagwConfig`].
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    pub ttl: Duration,
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            capacity: 10_000,
        }
    }
}

/// Cache entry that carries its own key.
///
/// `TinyUfo` hashes keys to `u64` and does not compare them on hit, so the key
/// is re-checked here: a hash collision must degrade to a miss, never to
/// another tenant's token.
#[derive(Clone)]
struct CachedToken {
    key: String,
    token: SecretString,
}

/// OAuth2 Client Credentials auth plugin.
pub struct OAuth2ClientCredAuthPlugin {
    credstore: Arc<dyn CredStoreClientV1>,
    auth_method: ClientAuthMethod,
    cache: MemoryCache<String, CachedToken>,
    cache_ttl: Duration,
}

impl OAuth2ClientCredAuthPlugin {
    #[must_use]
    pub fn new(
        credstore: Arc<dyn CredStoreClientV1>,
        auth_method: ClientAuthMethod,
        cache: TokenCacheConfig,
    ) -> Self {
        Self {
            credstore,
            auth_method,
            cache: MemoryCache::new(cache.capacity.max(1)),
            cache_ttl: cache.ttl,
        }
    }

    fn method_tag(&self) -> &'static str {
        match self.auth_method {
            ClientAuthMethod::Basic => "basic",
            ClientAuthMethod::Form => "form",
        }
    }

    /// Identity-complete cache key: tenant, subject, client-auth method and a
    /// deterministic hash of the binding config.
    fn build_cache_key(&self, ctx: &AuthContext<'_>) -> String {
        let security = ctx.security_context();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // BTreeMap iteration is already ordered, so the hash is stable.
        let ordered: &BTreeMap<String, serde_json::Value> = ctx.config;
        for (key, value) in ordered {
            key.hash(&mut hasher);
            value.to_string().hash(&mut hasher);
        }
        format!(
            "{}:{}:{}:{:016x}",
            security.subject_tenant_id(),
            security.subject_id(),
            self.method_tag(),
            hasher.finish()
        )
    }
}

impl std::fmt::Debug for OAuth2ClientCredAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ClientCredAuthPlugin")
            .field("auth_method", &self.method_tag())
            .field("cache_ttl", &self.cache_ttl)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AuthPlugin for OAuth2ClientCredAuthPlugin {
    fn id(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => "oauth2_client_cred_basic",
            ClientAuthMethod::Form => "oauth2_client_cred",
        }
    }

    fn plugin_type(&self) -> &str {
        match self.auth_method {
            ClientAuthMethod::Basic => gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ClientAuthMethod::Form => gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        }
    }

    async fn authenticate(&self, ctx: &mut AuthContext<'_>) -> Result<(), PluginError> {
        let key = self.build_cache_key(ctx);
        if let (Some(entry), _) = self.cache.get(&key)
            && entry.key == key
        {
            inject_bearer(ctx, &entry.token)?;
            return Ok(());
        }

        let config = ctx.config;
        let token_endpoint = config_nonblank(config, "token_endpoint");
        let issuer_url = config_nonblank(config, "issuer_url");
        if token_endpoint.is_some() == issuer_url.is_some() {
            return Err(PluginError::InvalidConfig(
                "oauth2 client credentials plugin requires exactly one of 'token_endpoint' or \
                 'issuer_url'"
                    .to_owned(),
            ));
        }

        let client_id_ref = config_nonblank(config, "client_id_ref").ok_or_else(|| {
            PluginError::InvalidConfig("missing 'client_id_ref' config key".to_owned())
        })?;
        let client_secret_ref = config_nonblank(config, "client_secret_ref").ok_or_else(|| {
            PluginError::InvalidConfig("missing 'client_secret_ref' config key".to_owned())
        })?;

        let security = ctx.security_context();
        let client_id = resolve_secret(&self.credstore, security, &client_id_ref).await?;
        let client_secret = resolve_secret(&self.credstore, security, &client_secret_ref).await?;

        let mut oauth_config = OAuthClientConfig {
            client_id: client_id.expose().to_owned(),
            client_secret,
            auth_method: self.auth_method,
            scopes: config_nonblank(config, "scopes")
                .map(|raw| {
                    raw.split_whitespace()
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            ..OAuthClientConfig::default()
        };
        if let Some(endpoint) = token_endpoint {
            oauth_config.token_endpoint = Some(parse_url(&endpoint, "token_endpoint")?);
        }
        if let Some(issuer) = issuer_url {
            oauth_config.issuer_url = Some(parse_url(&issuer, "issuer_url")?);
        }

        // A failed fetch is never cached — the next request retries the IdP.
        let fetched = fetch_token(oauth_config)
            .await
            .map_err(|err| PluginError::Unauthenticated(format!("token exchange failed: {err}")))?;

        let ttl = fetched
            .expires_in
            .checked_sub(EXPIRY_SAFETY_MARGIN)
            .map(|remaining| remaining.min(self.cache_ttl));
        if let Some(ttl) = ttl.filter(|t| !t.is_zero()) {
            self.cache.put(
                &key,
                CachedToken {
                    key: key.clone(),
                    token: SecretString::new(fetched.bearer.expose().to_owned()),
                },
                Some(ttl),
            );
        }

        inject_bearer(ctx, &fetched.bearer)
    }
}

fn parse_url(raw: &str, field: &str) -> Result<url::Url, PluginError> {
    url::Url::parse(raw)
        .map_err(|err| PluginError::InvalidConfig(format!("invalid '{field}' URL: {err}")))
}

fn inject_bearer(ctx: &mut AuthContext<'_>, token: &SecretString) -> Result<(), PluginError> {
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| PluginError::Internal("access token is not a valid header value".to_owned()))?;
    value.set_sensitive(true);
    ctx.headers.insert(header::AUTHORIZATION, value);
    Ok(())
}
